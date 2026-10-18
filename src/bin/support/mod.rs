//! Collector implementation shared with its unprivileged fixture tests.
use async_net_stack_rs::trace::{
    bundle::{self, COUNTER_SCHEMA, ENV_SCHEMA, PACKETS, SCHEMA},
    Capture, Limits,
};
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

static CANCEL: AtomicBool = AtomicBool::new(false);
extern "C" fn cancel(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

struct Signals {
    interrupt: libc::sighandler_t,
    terminate: libc::sighandler_t,
}

impl Signals {
    fn install() -> Self {
        CANCEL.store(false, Ordering::Relaxed);

        // Handler only stores a lock-free flag; file/process work stays outside it.
        unsafe {
            Self {
                interrupt: libc::signal(libc::SIGINT, cancel as *const () as libc::sighandler_t),
                terminate: libc::signal(libc::SIGTERM, cancel as *const () as libc::sighandler_t),
            }
        }
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.interrupt);
            libc::signal(libc::SIGTERM, self.terminate);
        }
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub struct Config {
    pub interface: String,
    pub duration: Duration,
    pub interval: Duration,
    pub root: PathBuf,
    pub filter: Option<String>,
    pub max_packets: u32,
}

struct Collector<'a> {
    tool_root: Option<&'a Path>,
    directory: &'a Path,
    epoch: Instant,
    sequence: usize,
}

impl Collector<'_> {
    /// No shell interpolation. Output is stored in files, avoiding pipe deadlock.
    /// Optional commands cannot block capture shutdown past their deadline.
    fn command(&mut self, program: &str, args: &[&str], deadline: Instant) -> Value {
        self.sequence += 1;

        let prefix = format!("raw/{:06}-{program}", self.sequence);
        let stdout_path = format!("{prefix}.stdout.txt");
        let stderr_path = format!("{prefix}.stderr.txt");
        let start = self.epoch.elapsed().as_nanos().to_string();
        let mut report = json!({
            "program":program,"args":args,
            "started_monotonic_ns":start,
            "stdout":stdout_path,
            "stderr":stderr_path
        });

        let run = (|| -> io::Result<(String, Option<i32>)> {
            if Instant::now() >= deadline {
                return Ok(("deadline".into(), None));
            }

            let stdout = File::create(self.directory.join(&stdout_path))?;
            let stderr = File::create(self.directory.join(&stderr_path))?;
            let mut child = Process(
                Command::new(
                    self.tool_root
                        .map_or_else(|| PathBuf::from(program), |root| root.join(program)),
                )
                .args(args)
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .stdout(stdout)
                .stderr(stderr)
                .spawn()?,
            );

            loop {
                if let Some(exit) = child.0.try_wait()? {
                    return Ok((
                        if exit.success() { "ok" } else { "failed" }.into(),
                        exit.code(),
                    ));
                }

                let too_big = [&stdout_path, &stderr_path].iter().any(|p| {
                    fs::metadata(self.directory.join(p)).is_ok_and(|m| m.len() > 1024 * 1024)
                });
                let deadline_reached = Instant::now() >= deadline;
                if too_big || CANCEL.load(Ordering::Relaxed) || deadline_reached {
                    child.0.kill()?;

                    let exit = child.0.wait()?;
                    return Ok((
                        if too_big {
                            "output_limit"
                        } else if CANCEL.load(Ordering::Relaxed) {
                            "cancelled"
                        } else {
                            "timeout"
                        }
                        .into(),
                        exit.code(),
                    ));
                }

                thread::sleep(Duration::from_millis(10));
            }
        })();

        match run {
            Ok((status, code)) => {
                report["status"] = json!(status);
                report["exit_code"] = json!(code);
            }
            Err(e) => {
                report["status"] = json!(if e.kind() == io::ErrorKind::NotFound {
                    "unavailable"
                } else {
                    "error"
                });

                report["error"] = json!(e.to_string());
            }
        }

        report["finished_monotonic_ns"] = json!(self.epoch.elapsed().as_nanos().to_string());
        if let Ok(file) = File::open(self.directory.join(stdout_path)) {
            let mut bytes = Vec::new();
            let read_ok = file.take(1024 * 1024).read_to_end(&mut bytes).is_ok();

            if read_ok {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    report["data"] = v;
                } else {
                    report["text"] = json!(String::from_utf8_lossy(&bytes));
                }
            }
        }
        report
    }
    fn snapshot(&mut self, interface: &str) -> Value {
        let specs: Vec<(&str, Vec<&str>)> = vec![
            ("uname", vec!["-a"]),
            ("tcpdump", vec!["--version"]),
            (
                "ip",
                vec!["-j", "-details", "address", "show", "dev", interface],
            ),
            (
                "ip",
                vec!["-j", "-details", "link", "show", "dev", interface],
            ),
            ("ip", vec!["-j", "-4", "route", "show", "table", "all"]),
            ("ip", vec!["-j", "-6", "route", "show", "table", "all"]),
            ("ip", vec!["-j", "rule", "show"]),
            ("ip", vec!["-j", "neigh", "show", "dev", interface]),
            ("ethtool", vec!["-i", interface]),
            ("ethtool", vec!["-k", interface]),
            ("ethtool", vec!["-g", interface]),
            ("ethtool", vec!["-c", interface]),
            ("ethtool", vec!["-l", interface]),
            ("ethtool", vec!["-x", interface]),
            ("ss", vec!["-t", "-u", "-n", "-i", "-m"]),
        ];

        let started = bundle::unix_ns();
        let mut commands = Vec::new();
        for (program, args) in specs {
            if CANCEL.load(Ordering::Relaxed) {
                break;
            }

            commands.push(self.command(program, &args, Instant::now() + Duration::from_secs(2)));
        }

        let mut files = serde_json::Map::new();
        for path in [
            "/proc/sys/net/core/netdev_max_backlog",
            "/proc/sys/net/core/rmem_max",
            "/proc/sys/net/core/wmem_max",
            "/proc/sys/net/ipv4/tcp_congestion_control",
            "/proc/interrupts",
            "/proc/softirqs",
            "/proc/sys/kernel/osrelease",
        ] {
            files.insert(
                path.into(),
                match fs::read_to_string(path) {
                    Ok(v) => json!({"status":"ok","text":v}),
                    Err(e) => json!({
                        "status":"unavailable",
                        "error":e.to_string()
                    }),
                },
            );
        }

        json!({
            "started_unix_ns":started,
            "finished_unix_ns":bundle::unix_ns(),
            "commands":commands,
            "files":files
        })
    }

    fn sample(&mut self, interface: &str, deadline: Instant) -> Value {
        let mut counters = serde_json::Map::new();
        let mut errors = serde_json::Map::new();
        let started = self.epoch.elapsed().as_nanos().to_string();
        let unix = bundle::unix_ns();
        for name in [
            "rx_bytes",
            "rx_packets",
            "rx_errors",
            "rx_dropped",
            "rx_missed_errors",
            "rx_fifo_errors",
            "tx_bytes",
            "tx_packets",
            "tx_errors",
            "tx_dropped",
            "tx_fifo_errors",
        ] {
            match fs::read_to_string(format!("/sys/class/net/{interface}/statistics/{name}"))
                .and_then(|s| s.trim().parse::<u64>().map_err(io::Error::other))
            {
                Ok(v) => {
                    counters.insert(name.into(), json!(v));
                }
                Err(e) => {
                    errors.insert(name.into(), json!(e.to_string()));
                }
            }
        }

        let mut commands = Vec::new();
        for (program, args) in [
            ("ip", vec!["-j", "-s", "link", "show", "dev", interface]),
            ("tc", vec!["-j", "-s", "qdisc", "show", "dev", interface]),
            ("ethtool", vec!["-S", interface]),
        ] {
            commands.push(self.command(
                program,
                &args,
                deadline.min(Instant::now() + Duration::from_secs(1)),
            ));
        }

        let mut system = serde_json::Map::new();
        for path in [
            "/proc/net/softnet_stat",
            "/proc/net/snmp",
            "/proc/net/netstat",
            "/proc/pressure/cpu",
            "/proc/pressure/memory",
        ] {
            system.insert(
                path.to_string(),
                match fs::read_to_string(path) {
                    Ok(text) => json!({"status":"ok", "text":text}),
                    Err(error) => json!({"status":"unavailable", "error":error.to_string()}),
                },
            );
        }

        let mut driver = serde_json::Map::new();
        if let Some(text) = commands.last().and_then(|v| v["text"].as_str()) {
            for line in text.lines() {
                if let Some((key, value)) = line.trim().split_once(':') {
                    if let Ok(n) = value.trim().parse::<u64>() {
                        driver
                            .insert(
                                key.trim().into(),
                                json!(n)
                            );
                    }
                }
            }
        }

        json!({
            "schema":COUNTER_SCHEMA,
            "monotonic_ns":started,
            "unix_ns":unix,
            "finished_monotonic_ns":self.epoch.elapsed().as_nanos().to_string(),
            "interface_counters":counters,
            "counter_errors":errors,
            "driver_counters":driver,
            "system":system,
            "commands":commands
        })
    }
}

fn stop(child: &mut Process) -> io::Result<ExitStatus> {
    if let Some(status) = child.0.try_wait()? {
        return Ok(status);
    }

    // SIGINT asks tcpdump to flush its pcap and print packet/drop statistics.
    unsafe {
        libc::kill(child.0.id() as libc::pid_t, libc::SIGINT);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }

        if Instant::now() >= deadline {
            child.0.kill()?;
            let _ = child.0.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tcpdump did not exit cleanly; bundle retained as failed",
            ));
        }

        thread::sleep(Duration::from_millis(10));
    }
}

pub fn collect(config: Config) -> io::Result<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "net-capture requires Linux; net-experiment replays bundles on macOS too",
        ));
    }

    if config.interface.is_empty()
        || config.interface.len() > 15
        || config.interface.starts_with('-')
        || !config
            .interface
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
        || [".", ".."].contains(&config.interface.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Linux interface name",
        ));
    }

    if !Path::new(&format!("/sys/class/net/{}", config.interface)).exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "interface not found in current network namespace",
        ));
    }

    collect_inner(config, None)
}

fn collect_inner(config: Config, tool_root: Option<&Path>) -> io::Result<PathBuf> {
    let _signals = Signals::install();
    let directory = bundle::create_directory(&config.root)?;
    println!("Collecting into {}", directory.display());

    let epoch = Instant::now();
    let mut manifest = json!({"schema":SCHEMA,"status":"collecting","created_unix_ns":bundle::unix_ns(),"interface":config.interface,"duration_requested_ms":config.duration.as_millis() as u64,"sample_interval_ms":config.interval.as_millis() as u64,"max_packets":config.max_packets,"filter":config.filter,"collector_version":env!("CARGO_PKG_VERSION"),"capture_file":PACKETS,"timestamp_clock":"packet timestamps from tcpdump; event monotonic_ns relative to collector start; unix_ns anchors are sampled, not hardware synchronization"});
    bundle::write_json(&directory.join("manifest.json"), &manifest)?;

    let result = (|| -> io::Result<()> {
        let mut collector = Collector {
            tool_root,
            directory: &directory,
            epoch,
            sequence: 0,
        };

        let before = collector.snapshot(&config.interface);
        bundle::write_json(
            &directory.join("environment.json"),
            &json!({"schema":ENV_SCHEMA,"before":before,"after":null}),
        )?;

        if CANCEL.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled before capture",
            ));
        }

        let mut samples = File::create(directory.join("counters.jsonl"))?;
        let mut baseline =
            collector.sample(&config.interface, Instant::now() + Duration::from_secs(3));

        baseline["phase"] = json!("before");
        serde_json::to_writer(&mut samples, &baseline)?;

        samples.write_all(b"\n")?;
        samples.flush()?;
        if CANCEL.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled before capture",
            ));
        }

        // Opening the output in the parent avoids tcpdump's privilege drop needing
        // directory write permission. tcpdump -w - writes pcap to this descriptor.
        let mut command = Command::new(
            tool_root
                .map_or_else(
                    || 
                        PathBuf::from("tcpdump"), 
                    |root| 
                        root.join("tcpdump")
                ),
        );

        command
            .args([
                "-i",
                &config.interface,
                "-n",
                "-p",
                "-U",
                "-s",
                "65535",
                "-c",
                &config.max_packets.to_string(),
                "-w",
                "-",
            ])
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(File::create(directory.join(PACKETS))?)
            .stderr(File::create(directory.join("capture.stderr.log"))?);

        if let Some(filter) = &config.filter {
            command.arg("--").arg(filter);
        }

        manifest["capture_command"] = json!(std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|s| s.to_string_lossy().into_owned())
            .collect::<Vec<_>>());

        let mut capture = Process(command.spawn().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot start tcpdump (install it and run with capture privileges): {e}"),
            )
        })?);

        let startup = Instant::now() + Duration::from_secs(5);
        loop {
            let log = fs::read_to_string(directory.join("capture.stderr.log"))?;
            if log.contains("listening on ") {
                break;
            }

            if let Some(status) = capture.0.try_wait()? {
                return Err(io::Error::other(format!(
                    "tcpdump exited during startup ({status}); see capture.stderr.log"
                )));
            }

            if CANCEL.load(Ordering::Relaxed) || Instant::now() >= startup {
                let _ = stop(&mut capture);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "tcpdump did not report capture readiness; see capture.stderr.log",
                ));
            }

            thread::sleep(Duration::from_millis(10));
        }

        let start = Instant::now();
        let deadline = start + config.duration;
        manifest["capture_started_unix_ns"] = json!(bundle::unix_ns());
        manifest["capture_started_monotonic_ns"] = json!(epoch.elapsed().as_nanos().to_string());
        let mut next_sample = start;
        let reason = loop {
            if CANCEL.load(Ordering::Relaxed) {
                break "interrupted";
            }

            if let Some(exit) = capture.0.try_wait()? {
                if !exit.success() {
                    return Err(io::Error::other(format!(
                        "tcpdump failed ({exit}); see capture.stderr.log"
                    )));
                }
                break "tcpdump_exit";
            }

            if CANCEL.load(Ordering::Relaxed) {
                break "interrupted";
            }

            if Instant::now() >= deadline {
                break "duration";
            }

            if fs::metadata(directory.join(PACKETS))?.len() >= 128 * 1024 * 1024 {
                break "capture_size_limit";
            }

            if Instant::now() >= next_sample {
                let mut sample = collector.sample(&config.interface, deadline);
                sample["phase"] = json!("during");
                serde_json::to_writer(&mut samples, &sample)?;
                samples.write_all(b"\n")?;
                samples.flush()?;
                next_sample = Instant::now() + config.interval;
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        };

        let exit = stop(&mut capture)?;
        if !exit.success() {
            return Err(io::Error::other(format!(
                "tcpdump did not finish successfully ({exit}); see capture.stderr.log"
            )));
        }

        manifest["stop_reason"] = json!(reason);
        manifest["capture_elapsed_ms"] = json!(start.elapsed().as_millis() as u64);
        manifest["capture_stopped_unix_ns"] = json!(bundle::unix_ns());
        manifest["capture_stopped_monotonic_ns"] = json!(epoch.elapsed().as_nanos().to_string());

        // Always include a final sample, even for a zero-packet capture.
        let mut sample =
            collector.sample(&config.interface, Instant::now() + Duration::from_secs(3));

        sample["phase"] = json!("after");
        serde_json::to_writer(&mut samples, &sample)?;
        samples.write_all(b"\n")?;

        samples.sync_all()?;
        let after = collector.snapshot(&config.interface);
        bundle::write_json(
            &directory.join("environment.json"),
            &json!({"schema":ENV_SCHEMA,"before":before,"after":after}),
        )?;

        let bytes = fs::metadata(directory.join(PACKETS))?.len();
        let parsed = Capture::read(File::open(directory.join(PACKETS))?, Limits::default())?;
        manifest["capture_bytes"] = json!(bytes);
        manifest["capture_records"] = json!(parsed.records.len());
        if reason == "tcpdump_exit" && parsed.records.len() >= config.max_packets as usize {
            manifest["stop_reason"] = json!("packet_limit");
        }

        manifest["capture_exit_code"] = json!(exit.code());
        manifest["status"] = json!("complete");
        manifest["finished_unix_ns"] = json!(bundle::unix_ns());
        bundle::Bundle::validate_completed(&directory, &manifest)?;

        Ok(())
    })();

    if let Err(e) = result {
        manifest["status"] = json!("failed");
        manifest["error"] = json!(e.to_string());
        manifest["finished_unix_ns"] = json!(bundle::unix_ns());
        bundle::write_json(&directory.join("manifest.json"), &manifest)?;

        return Err(io::Error::new(
            e.kind(),
            format!("{e}; retained bundle: {}", directory.display()),
        ));
    }

    bundle::write_json(&directory.join("manifest.json"), &manifest)?;

    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    
    #[test]
    fn fixture_collector_finalizes_and_failed_capture_never_becomes_latest() {
        let root = std::env::temp_dir().join(format!(
            "net-collector-test-{}-{}",
            std::process::id(),
            bundle::unix_ns()
        ));

        fs::create_dir_all(root.join("tools")).unwrap();
        let packet = root.join("fixture.pcap");
        fs::write(
            &packet,
            include_bytes!("../../../tests/fixtures/replay.pcap"),
        )
        .unwrap();

        let tools = root.join("tools");
        let tcpdump = tools.join("tcpdump");

        // This fixture only writes a supplied file; it never opens an interface.
        fs::write(&tcpdump,
         format!( "#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-tcpdump; exit 0; fi\ntrap 'exit 0' INT TERM\ncat '{}'\necho 'listening on fixture' >&2\nwhile :; do sleep 0.01; done\n",
                 packet.display())
        ).unwrap();
        fs::set_permissions(&tcpdump, fs::Permissions::from_mode(0o755)).unwrap();

        let config = || Config {
            interface: "fixture0".into(),
            duration: Duration::from_millis(100),
            interval: Duration::from_millis(20),
            root: root.join("traces"),
            filter: None,
            max_packets: 100,
        };

        let directory = collect_inner(config(), Some(&tools)).unwrap();
        let loaded = bundle::Bundle::load(&directory).unwrap();

        assert_eq!(loaded.manifest["stop_reason"], "duration");
        assert!(loaded.samples.len() >= 2);
        assert_eq!(loaded.samples[0]["phase"], "before");
        assert_eq!(loaded.samples.last().unwrap()["phase"], "after");
        assert_eq!(
            loaded.environment["before"]["commands"][0]["status"],
            "unavailable"
        );

        assert_eq!(loaded.manifest["capture_records"], 5);
        fs::write(
            &tcpdump,
            "#!/bin/sh\necho 'permission denied' >&2\nexit 1\n",
        )
        .unwrap();

        assert!(collect_inner(config(), Some(&tools)).is_err());
        assert_eq!(bundle::latest(&root.join("traces")).unwrap(), directory);

        fs::remove_dir_all(root).unwrap();
    }
}
