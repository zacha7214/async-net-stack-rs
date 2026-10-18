//! Portable error boundary. Host builds use `std::io` directly.
pub use core::net;

pub mod io {
    use core::fmt;

    pub type Result<T> = core::result::Result<T, Error>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[non_exhaustive]
    pub enum ErrorKind {
        Other,
        AddrInUse,
        AddrNotAvailable,
        ConnectionReset,
        InvalidData,
        InvalidInput,
        NotConnected,
        NotFound,
        PermissionDenied,
        TimedOut,
        Unsupported,
        WouldBlock,
    }

    /// Allocation-free protocol/device error; no OS errno or boxed source.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Error {
        kind: ErrorKind,
        message: &'static str,
    }

    impl Error {
        pub fn other(message: &'static str) -> Self {
            Self::new(ErrorKind::Other, message)
        }
        
        pub fn new(kind: ErrorKind, message: &'static str) -> Self {
            Self { kind, message }
        }
        
        pub fn kind(&self) -> ErrorKind {
            self.kind
        }
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }
    
}

pub(crate) fn default_sequence_source() -> io::Result<fn() -> io::Result<u32>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "provide TCP sequence entropy with TcpPool::with_sequence_source",
    ))
}
