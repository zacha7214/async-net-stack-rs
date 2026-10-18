use async_net_stack_rs::trace::{
    bundle::{self, Bundle},
    experiment, Capture, Limits,
};

use serde_json::json;
use std::{
    fs::File,
    io::{self, Write},
    path::PathBuf,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("net-experiment: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let mut source = None;
    let mut root = bundle::default_root();
    let mut output = None;
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("Usage: net-experiment [BUNDLE_DIR|CAPTURE_FILE] [--root DIR] [--report FILE]\n\nWithout a path, replay the newest completed bundle in {}.\nASYNC_NET_TRACE_DIR overrides the default store. JSON report goes to stdout\nor --report. Environment/counters are ingested as context, not applied as faults.",root.display());
            return Ok(());
        }

        if arg == "--root" {
            root = PathBuf::from(args.next().ok_or("--root requires a path")?);
        } else if arg == "--report" {
            output = Some(PathBuf::from(
                args.next().ok_or("--report requires a path")?,
            ));
        } else if arg.to_string_lossy().starts_with('-') {
            return Err(format!("unknown option: {}", arg.to_string_lossy()).into());
        } else if source.replace(PathBuf::from(arg)).is_some() {
            return Err("only one source path is allowed".into());
        }
    }

    let path = match source {
        Some(path) => path,
        None => bundle::latest(&root)?,
    };

    let bundle_path = if path.is_dir() {
        Some(path.clone())
    } else if path.file_name().is_some_and(|n| n == "manifest.json") {
        Some(path.parent().unwrap().to_path_buf())
    } else {
        None
    };

    let loaded = bundle_path.as_deref().map(Bundle::load).transpose()?;
    let capture_path = loaded
        .as_ref()
        .map_or_else(|| path.clone(), |b| b.capture_path());

    eprintln!("Replaying {}", capture_path.display());
    let capture = Capture::read(File::open(&capture_path)?, Limits::default())?;

    let mut result = experiment::replay(capture)?;
    result["source"] = json!(capture_path);

    if let Some(bundle) = loaded {
        let mut statuses = std::collections::BTreeMap::<String, usize>::new();
        for phase in ["before", "after"] {
            if let Some(commands) = bundle.environment[phase]["commands"].as_array() {
                for command in commands {
                    let status = command["status"].as_str().unwrap_or("unknown");
                    *statuses.entry(status.into()).or_default() += 1;
                }
            }
        }

        for sample in &bundle.samples {
            if let Some(commands) = sample["commands"].as_array() {
                for command in commands {
                    let status = command["status"].as_str().unwrap_or("unknown");
                    *statuses.entry(status.into()).or_default() += 1;
                }
            }
        }

        result["collection"] = json!({
            "manifest":bundle.manifest,
            "counter_samples":bundle.samples.len(),
            "counter_deltas":bundle.counter_deltas(),
            "command_status_counts":statuses,
            "configuration_present":true,
            "environment_applied":false
        });
    }

    if let Some(output) = output {
        // A report must never overwrite its capture or collection metadata.
        let absolute = if output.is_absolute() {
            output.clone()
        } else {
            std::env::current_dir()?.join(&output)
        };

        let source = std::fs::canonicalize(&capture_path)?;
        if absolute == source || output.exists() {
            return Err("report path already exists or is an input; choose a new file".into());
        }

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?;
        serde_json::to_writer_pretty(&mut file, &result)?;

        file.write_all(b"\n")?;
        eprintln!("Report: {}", output.display());
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        serde_json::to_writer_pretty(&mut out, &result)?;

        out.write_all(b"\n")?;
    }

    Ok(())
}
