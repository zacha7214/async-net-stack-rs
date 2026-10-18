//! Portable observed-packet experiment with a machine-readable result.
use super::{Capture, ReplayConfig, Severity, Trace, TraceDevice};
use crate::coordinator::ProtocolPort;
use crate::{
    coordinator::{Coordinator, PortConfig},
    device::Device,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
};

pub fn replay(capture: Capture) -> io::Result<Value> {
    let trace = Trace::analyze(capture);
    let diagnostics: Vec<_> = trace
        .diagnostics()
        .iter()
        .map(|d| {
            json!({
                "record":d.record,
                "severity": format!("{:?}",d.severity).to_lowercase(),
                "message":d.message})
        })
        .collect();

    let conversations: Vec<_> = trace
        .conversations()
        .iter()
        .enumerate()
        .map(|(id, c)| {
            json!({
                "id":id,
                "interface":c.key.interface,
                "vlan_tags":c.key.vlan_tags,
                "protocol": c.key.protocol,
                "endpoints":c.key.endpoints
                .iter()
                .map(|e|
                    json!({
                        "address":e.address.to_string(),
                        "port":e.port})).collect::<Vec<_>>(),
                "instance":c.instance,
                "first_record":c.first_record,
                "last_record":c.last_record,
                "packets_by_direction":c.packets,
                "syn_seen":c.syn_seen,
                "fin_seen":c.fin_seen,
                "reset_seen":c.reset_seen
            })
        })
        .collect();

    let interfaces: Vec<_> = trace
        .capture()
        .interfaces
        .iter()
        .map(|i| {
            json!({
                "id":i.id,
                "link_type":i.link_type,
                "name":i.name,
                "snaplen":i.snaplen
            })
        })
        .collect();

    let mut result = json!({
        "schema":"async-net.experiment.v1",
        "mode":"observed_packet_playback",
        "environment_applied":false,
        "capture_records":trace.capture().records.len(),
        "capture_notes":trace.capture().notes,
        "interfaces":interfaces,
        "conversations":conversations,
        "diagnostics":diagnostics}
    );

    let skipped = trace
        .diagnostics()
        .iter()
        .filter(|d| d.severity == Severity::Skipped)
        .count();

    let mut expected: BTreeMap<u8, VecDeque<usize>> = BTreeMap::new();
    for (i, p) in trace.packets().iter().enumerate() {
        expected.entry(p.protocol).or_default().push_back(i);
    }

    let target = trace.packets().len();
    let mut coordinator = Coordinator::new(TraceDevice::new(trace, ReplayConfig::default())?);
    let mut ports: Vec<(u8, ProtocolPort<TraceDevice>)> = Vec::new();
    for &protocol in expected.keys() {
        let protocol_port_entry = (protocol, coordinator.port(protocol, PortConfig::default())?);
        ports.push(protocol_port_entry);
    }

    let mut received = 0;
    let mut late = 0;
    let mut deliveries = Vec::new();
    let mut rx = Vec::new();
    while let Some(at) = coordinator.next_deadline([]) {
        coordinator.poll(at, 64)?;

        for d in coordinator.with_device(|d| d.take_deliveries()) {
            late += usize::from(d.actual != d.scheduled);
            deliveries.push(json!({
                "record":d.record,
                "scheduled_ns":d.scheduled.as_nanos().to_string(),
                "actual_ns":d.actual.as_nanos().to_string(),
                "lateness_ns":d.actual.saturating_sub(d.scheduled).as_nanos().to_string()}
            ));
        }

        for (protocol, port) in &mut ports {
            port.recv(64, &mut rx)?;

            for frame in rx.drain(..) {
                let index = expected
                    .get_mut(protocol)
                    .and_then(|q| q.pop_front())
                    .ok_or_else(|| io::Error::other("unexpected replay packet"))?;

                let matches = coordinator.with_device(|d| {
                    d.trace()
                        .packet_bytes(
                            &d.trace()
                                .packets()[index]
                        ) == Some(frame.as_slice()
                    )
                });

                if !matches {
                    return Err(io::Error::other("replay packet bytes changed"));
                }

                received += 1;
            }
        }
    }

    let has_dropped_pkt = ports.iter().any(|(_, p)| p.stats().queue_drops != 0);
    if received != target
        || coordinator.stats().malformed != 0
        || coordinator.stats().unclaimed != 0
        || has_dropped_pkt
    {
        return Err(io::Error::other("replay lost a packet during dispatch"));
    }

    result["deliveries"] = json!(deliveries);
    result["summary"] = json!({
        "delivered":received,
        "skipped":skipped,
        "late":late,
        "coordinator_drops":0,
        "bytes_verified":true,
        "coverage":if skipped==0 {"complete"} else {"partial"}
    });

    Ok(result)
}
