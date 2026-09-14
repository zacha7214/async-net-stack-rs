//! Two-process, shared-RAM RX descriptor/completion experiment; not a VM backend.
//! See docs/mac-shared-memory.md for the memory model and QEMU integration design.
#[path = "support/shm_nic.rs"]
mod lab;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    lab::main()
}
