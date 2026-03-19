#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("syncing client")]
    SyncingClient(#[from] crate::syncing::ClientError),
    #[error("Io error")]
    Io(#[from] std::io::Error),
    // more error from each module
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn not_found(&self) -> bool {
        match self {
            Error::Io(error) => error.kind() == std::io::ErrorKind::NotFound,
            _ => false,
        }
    }
}
