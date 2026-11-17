use std::fmt;

/// Media Kit errors (BeOS `status_t` values with a message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No add-on / accelerant offers what was asked (`B_MEDIA_BAD_FORMAT`, `B_NOT_SUPPORTED`).
    Unsupported(String),
    /// The two sides of a connection could not agree on a format (`B_MEDIA_BAD_FORMAT`).
    BadFormat(String),
    /// Unknown or released node / connection (`B_MEDIA_BAD_NODE`, `B_MEDIA_BAD_SOURCE`, …).
    BadNode(String),
    /// A caller argument is out of range (`B_BAD_VALUE`).
    InvalidArgument(String),
    /// Waiting for a buffer, an engine or a node reply timed out (`B_TIMED_OUT`).
    TimedOut,
    /// A codec failed.
    Codec(String),
    /// An operating-system or driver call failed.
    Os(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "not supported: {s}"),
            Error::BadFormat(s) => write!(f, "bad media format: {s}"),
            Error::BadNode(s) => write!(f, "bad node: {s}"),
            Error::InvalidArgument(s) => write!(f, "invalid argument: {s}"),
            Error::TimedOut => write!(f, "timed out"),
            Error::Codec(s) => write!(f, "codec error: {s}"),
            Error::Os(s) => write!(f, "system error: {s}"),
        }
    }
}

impl std::error::Error for Error {}
