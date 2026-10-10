//! Linux-only capture collection; this executable also builds on macOS and
//! explains the platform requirement without starting subprocesses there.
mod support;
use async_net_stack_rs::trace::bundle;
use std::{path::PathBuf, time::Duration};
fn main() {
    if let Err(error) = run() {
        eprintln!("net-capture: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut interface = None;
    let mut duration = 10.0;
    let mut interval = 1.0;
    let mut root = bundle::default_root();
    let mut filter = None;
    let mut max_packets = 100_000;

    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("Usage: net-capture --interface IFACE [--duration SECONDS] [--interval SECONDS]\n                   [--root DIR] [--filter BPF_EXPRESSION] [--max-packets N]\n\nLinux; requires tcpdump and capture privileges. ip, ethtool, tc, and ss are optional.\nDefaults: duration 10s, minimum sample gap 1s, max-packets 100000.\nDefault store: {}\nASYNC_NET_TRACE_DIR overrides the default store.\nDuration begins when tcpdump reports readiness; snapshots also take time.",root.display());
            return Ok(());
        }

        let value = args.next().ok_or("option requires a value (use --help)")?;
        match arg.as_str() {
            "--interface" => interface = Some(value),
            "--duration" => duration = value.parse::<f64>()?,
            "--interval" => interval = value.parse::<f64>()?,
            "--root" => root = PathBuf::from(value),
            "--filter" => filter = Some(value),
            "--max-packets" => max_packets = value.parse::<u32>()?,
            _ => return Err(format!("unknown option: {arg}").into()),
        }
    }

    if !duration.is_finite()
        || !(0.1..=3600.0).contains(&duration)
        || !interval.is_finite()
        || !(0.1..=3600.0).contains(&interval)
        || !(1..=1_000_000).contains(&max_packets)
    {
        return Err(
            "duration/interval must be 0.1..3600 seconds; max-packets must be 1..1000000".into(),
        );
    }

    let directory = support::collect(support::Config {
        interface: interface.ok_or("--interface is required (use --help)")?,
        duration: Duration::from_secs_f64(duration),
        interval: Duration::from_secs_f64(interval),
        root,
        filter,
        max_packets,
    })?;

    println!(
        "Completed bundle: {}\nReplay with: net-experiment {}",
        directory.display(),
        directory.display()
    );

    Ok(())
}
