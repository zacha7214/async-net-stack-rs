//! Synthetic RX generator for QEMU's vhost-user-net backend, on macOS or Linux.
//! This is an experimental trusted-VM lab, not a general-purpose virtual switch.
#[path = "support/vhost_user/mod.rs"]
mod backend;
#[path = "support/vm_packet.rs"]
mod vm_packet;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    backend::main()
}
