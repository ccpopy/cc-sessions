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
    /// An atomic writer rejected the operation before replacing the destination because another
    /// writer won the compare-and-swap/create race. Callers may retry from a fresh snapshot.
    #[error("{0}")]
    AtomicWriteConflict(String),
    /// An atomic writer failed before replacing the destination for a non-retryable reason.
    /// Compensation layers must not claim any concurrently changed destination bytes as theirs.
    #[error("{0}")]
    AtomicWriteNotCommitted(String),
    /// The destination was replaced successfully, but a durability/cleanup step failed later.
    /// Compensation layers must treat the new destination bytes as belonging to this operation.
    #[error("{0}")]
    AtomicWriteCommitted(String),
    #[error("{0}")]
    Other(String),
}

impl AppError {
    pub(crate) fn atomic_write_not_committed(&self) -> bool {
        matches!(
            self,
            Self::AtomicWriteConflict(_) | Self::AtomicWriteNotCommitted(_)
        )
    }

    pub(crate) fn retryable_atomic_write_conflict(&self) -> bool {
        matches!(self, Self::AtomicWriteConflict(_))
    }

    pub(crate) fn atomic_write_committed(&self) -> bool {
        matches!(self, Self::AtomicWriteCommitted(_))
    }
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
