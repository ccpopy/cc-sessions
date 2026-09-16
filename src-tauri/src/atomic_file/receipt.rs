//! Thread-bound receipt collection for synchronous journal callbacks. Only the atomic writer
//! publishes evidence; a later read of the destination can never become a write receipt.
use std::cell::RefCell;
use std::path::{Path, PathBuf};

use super::FileFingerprint;
use crate::error::{AppError, AppResult};

struct Entry {
    path: PathBuf,
    baseline: Option<FileFingerprint>,
    written: Option<FileFingerprint>,
}

thread_local! {
    static ACTIVE: RefCell<Vec<Entry>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct ReceiptScope {
    path: PathBuf,
    // Scopes must be finished/dropped on the thread that runs the synchronous writer.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl ReceiptScope {
    pub(crate) fn begin(path: &Path, baseline: Option<FileFingerprint>) -> AppResult<Self> {
        let path = crate::path_safety::coordination_path(path)?;
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active.iter().any(|entry| entry.path == path) {
                return Err(AppError::Other("同一文件不能嵌套登记补偿写入".into()));
            }
            active.push(Entry {
                path: path.clone(),
                baseline,
                written: None,
            });
            Ok(Self {
                path,
                _thread_bound: std::marker::PhantomData,
            })
        })
    }

    pub(crate) fn written(&self) -> Option<FileFingerprint> {
        ACTIVE.with(|active| {
            active
                .borrow()
                .iter()
                .find(|entry| entry.path == self.path)
                .and_then(|entry| entry.written.clone())
        })
    }
}

impl Drop for ReceiptScope {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.borrow_mut().retain(|entry| entry.path != self.path));
    }
}

// Outer Option distinguishes an unjournaled write from a journaled create (baseline None).
pub(super) fn baseline(path: &Path) -> AppResult<Option<Option<FileFingerprint>>> {
    if ACTIVE.with(|active| active.borrow().is_empty()) {
        return Ok(None);
    }
    let path = crate::path_safety::coordination_path(path)?;
    Ok(ACTIVE.with(|active| {
        active
            .borrow()
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.written.clone().or_else(|| entry.baseline.clone()))
    }))
}

pub(super) fn publish(path: &Path, written: FileFingerprint) {
    // The identity was resolved before committing. Use that same key, even if an external
    // process replaces the path or parent immediately after publication.
    ACTIVE.with(|active| {
        if let Some(entry) = active
            .borrow_mut()
            .iter_mut()
            .find(|entry| entry.path == path)
        {
            entry.written = Some(written);
        }
    });
}

pub(super) fn key(path: &Path) -> AppResult<Option<PathBuf>> {
    if ACTIVE.with(|active| active.borrow().is_empty()) {
        return Ok(None);
    }
    Ok(Some(crate::path_safety::coordination_path(path)?))
}
