use thiserror::Error as ThisError;

#[derive(ThisError, Debug)]
pub enum CheckError {
    #[error("Repository not found")]
    NotFound,

    #[error("Repository is locked. Unlock with `restic unlock`.")]
    Locked,

    #[error("Bad password.")]
    BadPassword,

    #[error("Repository error {0}.")]
    Error(String),
}

#[derive(ThisError, Debug)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Repository check error: {0}")]
    Check(CheckError),

    #[error("Restic initialization failed.")]
    Init,

    #[error("Failed to backup volume: {0}. Error: {1}")]
    Backup(String, String),

    #[error("Failed to unlock respository: {0}")]
    Unlock(String),

    #[error("Docker error: {0}")]
    Docker(#[from] bollard::errors::Error),
}

impl From<i32> for CheckError {
    fn from(code: i32) -> Self {
        match code {
            10 => CheckError::NotFound,
            11 => CheckError::Locked,
            12 => CheckError::BadPassword,
            _ => CheckError::Error(format!("Unknown error code: {code}")),
        }
    }
}
