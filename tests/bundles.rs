use async_net_stack_rs::trace::{
    bundle::{self, Bundle, COUNTER_SCHEMA, ENV_SCHEMA, PACKETS, SCHEMA},
    experiment, Capture, Limits,
};

use serde_json::{json, Value};

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

static TEMP_SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "net-bundle-test-{}-{}-{}",
            std::process::id(),
            bundle::unix_ns(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));

        fs::create_dir(&p).unwrap();
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture(root: &Path, time: &str) -> PathBuf {
    let dir = bundle::create_directory(root).unwrap();
    let packet = include_bytes!("fixtures/replay.pcap");
    fs::write(dir.join(PACKETS), packet).unwrap();
    bundle::write_json(&dir.join("manifest.json"),&json!({"schema":SCHEMA,"status":"complete","finished_unix_ns":time,"capture_bytes":packet.len()})).unwrap();
    bundle::write_json(&dir.join("environment.json"),&json!({"schema":ENV_SCHEMA,"before":{"commands":[{"status":"unavailable","program":"ethtool"}]},"after":{"commands":[]}})).unwrap();
    let mut file = fs::File::create(dir.join("counters.jsonl")).unwrap();
    for (at, n) in [("0", 10), ("1000", 30)] {
        writeln!(
            file,
            "{}",
            json!({"schema":COUNTER_SCHEMA,"monotonic_ns":at,"interface_counters":{"rx_packets":n}})
        )
        .unwrap();
    }

    dir
}

#[test]
fn bundle_ingestion_and_version_checks() {
    let temp = Temp::new();
    let dir = fixture(&temp.0, "100");
    let loaded = Bundle::load(&dir).unwrap();
    assert_eq!(loaded.counter_deltas()["rx_packets"]["delta"], 20);
    let result = experiment::replay(
        Capture::read(
            fs::File::open(loaded.capture_path()).unwrap(),
            Limits::default(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["summary"]["delivered"], 4);
    assert_eq!(result["summary"]["skipped"], 1);

    bundle::write_json(
        &dir.join("manifest.json"),
        &json!({"schema":"future.v2","status":"complete"}),
    )
    .unwrap();

    assert!(Bundle::load(&dir).is_err());
}

#[test]
fn latest_uses_completion_time_and_ignores_failed_or_partial() {
    let temp = Temp::new();
    let newer = fixture(&temp.0, "200");
    let older = fixture(&temp.0, "100");
    let failed = fixture(&temp.0, "300");
    let partial = fixture(&temp.0, "400");
    for (dir, status) in [(&failed, "failed"), (&partial, "collecting")] {
        bundle::write_json(
            &dir.join("manifest.json"),
            &json!({"schema":SCHEMA,"status":status,"finished_unix_ns":"999"}),
        )
        .unwrap();
    }
    assert_eq!(bundle::latest(&temp.0).unwrap(), newer);
    fs::remove_file(newer.join(PACKETS)).unwrap();

    // A corrupt latest complete bundle is surfaced, not silently replaced by older data.
    assert!(Bundle::load(&bundle::latest(&temp.0).unwrap()).is_err());
    assert!(Bundle::load(&older).is_ok());
}

#[test]
fn malformed_counter_timeline_and_capture_changes_are_rejected() {
    let temp = Temp::new();
    let dir = fixture(&temp.0, "100");
    fs::write(
        dir.join("counters.jsonl"),
        format!(
            "{}\n{}\n",
            json!({"schema":COUNTER_SCHEMA,"monotonic_ns":"2","interface_counters":{}}),
            json!({"schema":COUNTER_SCHEMA,"monotonic_ns":"1","interface_counters":{}})
        ),
    )
    .unwrap();
    assert!(Bundle::load(&dir).is_err());

    let dir = fixture(&temp.0, "200");
    fs::write(dir.join(PACKETS), b"changed").unwrap();
    assert!(Bundle::load(&dir).is_err());
}

#[test]
fn experiment_cli_defaults_and_explicit_paths() {
    let temp = Temp::new();
    let dir = fixture(&temp.0, "100");
    for args in [vec![], vec![dir.to_string_lossy().to_string()]] {
        let output = Command::new(env!("CARGO_BIN_EXE_net-experiment"))
            .args(args)
            .env("ASYNC_NET_TRACE_DIR", &temp.0)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["summary"]["delivered"], 4);
        assert_eq!(
            value["collection"]["counter_deltas"]["rx_packets"]["delta"],
            20
        );
        assert_eq!(
            value["collection"]["command_status_counts"]["unavailable"],
            1
        );
        assert_eq!(value["environment_applied"], false);
    }

    let output = Command::new(env!("CARGO_BIN_EXE_net-experiment"))
        .arg(dir.join(PACKETS))
        .env("ASYNC_NET_TRACE_DIR", temp.0.join("missing"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "explicit files must not depend on latest lookup"
    );

    let before = fs::read(dir.join(PACKETS)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_net-experiment"))
        .arg(&dir)
        .arg("--report")
        .arg(dir.join(PACKETS))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(fs::read(dir.join(PACKETS)).unwrap(), before);
}

#[test]
fn counter_reset_mid_capture_is_not_reported_as_a_valid_delta() {
    let temp = Temp::new();
    let dir = fixture(&temp.0, "100");
    let mut file = fs::File::create(dir.join("counters.jsonl")).unwrap();
    for (at, n) in [("0", 10), ("1", 2), ("2", 30)] {
        writeln!(
            file,
            "{}",
            json!({"schema":COUNTER_SCHEMA,"monotonic_ns":at,"interface_counters":{"rx_packets":n}})
        )
        .unwrap();
    }

    let loaded = Bundle::load(&dir).unwrap();
    let delta = loaded.counter_deltas();
    assert_eq!(delta["rx_packets"]["reset_or_wrap"], true);
    assert!(delta["rx_packets"]["delta"].is_null());
}
