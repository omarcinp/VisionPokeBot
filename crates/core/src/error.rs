use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid image: {0}")]
    InvalidImage(String),
    #[error("video source reached end of stream")]
    EndOfStream,
    #[error("device disconnected: {0}")]
    Disconnected(String),
    #[error("device error: {0}")]
    Device(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("invalid data: {0}")]
    InvalidData(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
