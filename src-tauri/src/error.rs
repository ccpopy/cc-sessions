use serde::{Serialize, Serializer};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("path: {0}")]
    Path(String),
    #[allow(dead_code)]
    #[error("invalid codex dir: {0}")]
    InvalidCodexDir(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::Other(e.to_string())
    }
}

impl Serialize for AppError {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;

pub fn ensure_not_cancelled(cancel: Option<&AtomicBool>) -> AppResult<()> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err(AppError::Cancelled)
    } else {
        Ok(())
    }
}
