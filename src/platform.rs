//! Host services used by the shared protocol sources.
//! The portable build supplies its own implementation of this small boundary.
pub use std::{io, net};

fn initial_sequence() -> io::Result<u32> {
    use io::Read;
    let mut bytes = [0; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(u32::from_ne_bytes(bytes))
}

pub(crate) fn default_sequence_source() -> io::Result<fn() -> io::Result<u32>> {
    Ok(initial_sequence)
}
