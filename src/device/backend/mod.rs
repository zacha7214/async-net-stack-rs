pub mod error;
pub use error::Error;

#[cfg(all(feature = "tun", any(target_os = "linux", target_os = "macos")))]
mod tun;
#[cfg(all(feature = "tun", any(target_os = "linux", target_os = "macos")))]
pub use tun::DefaultDevice;
