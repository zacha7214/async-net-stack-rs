//! Portable build of the same engines used by the host crate.
//! Requires a global allocator; no OS services or background tasks are provided.
#![no_std]
extern crate alloc;

#[path = "../../src/api/mod.rs"]
pub mod api;

#[path = "../../src/coordinator.rs"]
pub mod coordinator;
pub mod device;

#[path = "../../src/net/mod.rs"]
pub mod net;
pub mod platform;

#[path = "../../src/simulation.rs"]
pub mod simulation;

#[path = "../../src/transport/mod.rs"]
pub mod transport;
