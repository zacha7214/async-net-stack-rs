// backend/error.rs
use std::io;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    // Default io::Error conversion for `?` ergonomics.
    // All unmapped io::Error paths fall back to this.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),

    #[error("Failed to create socket: {0}")]
    CreateSocket(#[source] io::Error),

    #[error("Bind failed: {0}")]
    Bind(String),

    #[error("Ioctl failed: {0}")]
    IoctlFailed(#[source] io::Error),

    #[error("Connect failed: {0}")]
    ConnectFailed(String),

    #[error("FCntl error: {0}")]
    FCntl(#[source] io::Error),

    #[error("setsockopt error: {0}")]
    SetSockOpt(#[source] io::Error),

    #[error("getsockopt error: {0}")]
    GetSockOpt(#[source] io::Error),

    #[error("Invalid tunnel name: {0}")]
    InvalidTunnelName(String),

    // Renamed slightly so it works for both platforms.
    // macOS calls ifconfig; Linux might call `ip` or use netlink.
    #[error("Interface configuration failed: {0}")]
    IfconfigFailed(#[source] io::Error),

    // Escape hatch for truly platform-specific errors.
    // Use sparingly — prefer adding common variants instead.
    #[error("Platform-specific error: {0}")]
    Platform(String),
}
