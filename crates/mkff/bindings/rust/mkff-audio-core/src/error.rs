use std::fmt;

/// Errors from the audio codecs and containers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A feature of the stream or a parameter combination that is not implemented.
    Unsupported(String),
    /// The data is damaged or is not what the codec / container expects.
    InvalidData(String),
    /// Not enough data yet (a truncated packet or stream).
    EndOfData,
    /// A caller argument is out of range.
    InvalidArgument(String),
    /// Reading or writing failed.
    Io(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidData(msg.into())
    }
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Error::Unsupported(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::InvalidData(s) => write!(f, "invalid data: {s}"),
            Error::EndOfData => write!(f, "unexpected end of data"),
            Error::InvalidArgument(s) => write!(f, "invalid argument: {s}"),
            Error::Io(s) => write!(f, "I/O error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
