//! Versioned capture bundles shared by the Linux collector and portable runner.
use serde_json::Value;
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCHEMA: &str = "async-net.capture.v1";
pub const ENV_SCHEMA: &str = "async-net.environment.v1";
pub const COUNTER_SCHEMA: &str = "async-net.counters.v1";
pub const PACKETS: &str = "capture.pcap";

pub fn unix_ns() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

pub fn default_root() -> PathBuf {
    if let Some(path) = std::env::var_os("ASYNC_NET_TRACE_DIR") {
        return PathBuf::from(path);
    }

    // Keep binaries launched from a workspace subdirectory on the same store.
    if let Ok(cwd) = std::env::current_dir() {
        for path in cwd.ancestors() {
            if fs::read_to_string(path.join("Cargo.toml"))
                .is_ok_and(|s| s.contains("name = \"async-net-stack-rs\""))
            {
                return path.join("results/traces");
            }
        }
    }

    let built = Path::new(env!("CARGO_MANIFEST_DIR"));
    if built.join("Cargo.toml").exists() {
        built.join("results/traces")
    } else {
        PathBuf::from("results/traces")
    }
}

pub fn create_directory(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;

    for suffix in 0..1000 {
        let path = root.join(format!(
            "trace-{}-{}-{suffix}",
            unix_ns(),
            std::process::id()
        ));

        match fs::create_dir(&path) {
            Ok(()) => {
                fs::create_dir(path.join("raw"))?;
                return fs::canonicalize(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "cannot create unique trace directory",
    ))
}

/// Replace metadata only after the complete JSON has reached disk.
pub fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    let temp = path.with_extension("json.tmp");
    let mut out = File::create(&temp)?;
    serde_json::to_writer_pretty(&mut out, value)?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    fs::rename(temp, path)
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn read_json(path: &Path, limit: u64) -> io::Result<Value> {
    if fs::metadata(path)?.len() > limit {
        return Err(invalid(format!(
            "{} exceeds metadata limit",
            path.display()
        )));
    }

    serde_json::from_reader(File::open(path)?)
        .map_err(|e| invalid(format!("{}: {e}", path.display())))
}

fn check_schema(v: &Value, schema: &str) -> io::Result<()> {
    if v["schema"].as_str() != Some(schema) {
        return Err(invalid(format!("unsupported schema: expected {schema}")));
    }
    Ok(())
}

/// Fixed artifact names avoid trusting arbitrary paths in an imported manifest.
pub struct Bundle {
    pub directory: PathBuf,
    pub manifest: Value,
    pub environment: Value,
    pub samples: Vec<Value>,
}

impl Bundle {
    pub fn load(path: &Path) -> io::Result<Self> {
        let directory = fs::canonicalize(path)?;
        let manifest = read_json(&directory.join("manifest.json"), 1024 * 1024)?;
        Self::from_metadata(directory, manifest)
    }

    /// Validate staged artifacts against a proposed complete manifest before
    /// publishing it, so incomplete bundles never become the default experiment.
    pub fn validate_completed(path: &Path, manifest: &Value) -> io::Result<()> {
        Self::from_metadata(fs::canonicalize(path)?, manifest.clone()).map(|_| ())
    }

    fn from_metadata(directory: PathBuf, manifest: Value) -> io::Result<Self> {
        check_schema(&manifest, SCHEMA)?;
        if manifest["status"] != "complete" {
            return Err(invalid(
                "capture bundle is not complete; inspect manifest.json and capture.stderr.log",
            ));
        }

        let expected = manifest["capture_bytes"]
            .as_u64()
            .ok_or_else(|| invalid("missing capture_bytes"))?;
        if fs::metadata(directory.join(PACKETS))?.len() != expected {
            return Err(invalid("capture size differs from completed manifest"));
        }

        let environment = read_json(&directory.join("environment.json"), 32 * 1024 * 1024)?;
        check_schema(&environment, ENV_SCHEMA)?;
        let samples_path = directory.join("counters.jsonl");
        if fs::metadata(&samples_path)?.len() > 64 * 1024 * 1024 {
            return Err(invalid("counter log exceeds 64 MiB ingestion limit"));
        }

        let mut samples = Vec::new();
        let mut previous = 0u128;
        for line in BufReader::new(File::open(samples_path)?).lines() {
            let sample: Value = serde_json::from_str(&line?).map_err(|e| invalid(e.to_string()))?;
            check_schema(&sample, COUNTER_SCHEMA)?;
            let at = sample["monotonic_ns"]
                .as_str()
                .and_then(|s| s.parse::<u128>().ok())
                .ok_or_else(|| invalid("invalid counter sample timestamp"))?;

            if at < previous {
                return Err(invalid("counter sample timestamps went backwards"));
            }

            previous = at;
            if !sample["interface_counters"].is_object() {
                return Err(invalid("missing interface counter object"));
            }

            if samples.len() == 100_000 {
                return Err(invalid("too many counter samples"));
            }

            samples.push(sample);
        }

        if samples.is_empty() {
            return Err(invalid("bundle contains no counter samples"));
        }

        Ok(Self {
            directory,
            manifest,
            environment,
            samples,
        })
    }

    pub fn capture_path(&self) -> PathBuf {
        self.directory.join(PACKETS)
    }

    /// A decrease indicates reset or wrap
    /// and deliberately has no inferred delta.
    pub fn counter_deltas(&self) -> Value {
        let mut deltas = serde_json::Map::new();
        let first = &self.samples[0]["interface_counters"];
        let last = &self.samples[self.samples.len() - 1]["interface_counters"];

        if let Some(fields) = first.as_object() {
            for (key, start) in fields {
                let end = last[key].as_u64();
                if let (Some(start), Some(end)) = (start.as_u64(), end) {
                    let values: Vec<_> = self
                        .samples
                        .iter()
                        .filter_map(|s| s["interface_counters"][key].as_u64())
                        .collect();
                    let reset = values.windows(2).any(|w| w[1] < w[0]);
                    let delta = if reset || self.samples.len() < 2 {
                        None
                    } else {
                        end.checked_sub(start)
                    };

                    deltas.insert(
                        key.clone(),
                        serde_json::json!({
                               "start":start,
                               "end":end,
                               "delta":delta,
                               "reset_or_wrap":reset,
                               "samples_with_value":values.len()
                        }),
                    );
                }
            }
        }

        Value::Object(deltas)
    }
}

pub fn latest(root: &Path) -> io::Result<PathBuf> {
    let mut candidates = Vec::new();
    if !root.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no traces in {}; run net-capture first or pass a bundle path",
                root.display()
            ),
        ));
    }

    for entry in fs::read_dir(fs::canonicalize(root)?)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        if let Ok(v) = read_json(&entry.path().join("manifest.json"), 1024 * 1024) {
            if v["schema"] == SCHEMA && v["status"] == "complete" {
                if let Some(at) = v["finished_unix_ns"]
                    .as_str()
                    .and_then(|s| s.parse::<u128>().ok())
                {
                    candidates.push((at, entry.path()));
                }
            }
        }
    }

    candidates.sort();
    candidates.pop().map(|(_, p)| p).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no completed traces in {}", root.display()),
        )
    })
}
