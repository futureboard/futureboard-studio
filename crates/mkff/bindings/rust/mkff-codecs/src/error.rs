use std::fmt;

/// Errors from opening, demuxing or decoding media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The container, codec or a feature of the stream is not supported.
    Unsupported(String),
    /// The data is damaged or not what the codec expects.
    Decode(String),
    /// Reading the file failed.
    Io(String),
    /// The media has no stream of the requested kind (or no stream with that index).
    NoStream(&'static str),
    /// A caller argument is out of range.
    InvalidArgument(String),
    /// Encoding or muxing failed.
    Encode(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "unsupported: {s}"),
            Error::Decode(s) => write!(f, "decode error: {s}"),
            Error::Io(s) => write!(f, "I/O error: {s}"),
            Error::NoStream(kind) => write!(f, "no {kind} stream"),
            Error::InvalidArgument(s) => write!(f, "invalid argument: {s}"),
            Error::Encode(s) => write!(f, "encode error: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<filmcraft_codecs::CodecError> for Error {
    fn from(e: filmcraft_codecs::CodecError) -> Self {
        use filmcraft_codecs::CodecError as C;
        match e {
            C::Unsupported(s) => Error::Unsupported(s),
            C::Decode(s) => Error::Decode(s),
            C::Container(s) => Error::Decode(format!("container: {s}")),
            C::Cancelled => Error::Decode("cancelled".into()),
        }
    }
}

impl From<filmcraft_media::MediaError> for Error {
    fn from(e: filmcraft_media::MediaError) -> Self {
        use filmcraft_media::MediaError as M;
        match e {
            M::Unsupported(s) => Error::Unsupported(s),
            M::Io(s) => Error::Io(s),
            M::NoStream(kind) => Error::NoStream(kind),
            other => Error::Decode(other.to_string()),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
