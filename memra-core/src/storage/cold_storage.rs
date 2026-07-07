//! Cold storage: append-only JSONL archive with monthly file split.
//!
//! Ported from `backend/services/cold_storage.py`.
//! Append errors are propagated to callers that require cold-storage integrity.
//! A disabled writer remains a no-op for tests and opt-out deployments.

use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(unix)]
use std::{os::fd::AsRawFd, os::raw::c_int};

use chrono::Utc;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ColdStorageError {
    #[error("cold storage lock poisoned: {0}")]
    LockPoisoned(String),
    #[error("cold storage file lock failed for {path}: {source}")]
    FileLock { path: PathBuf, source: io::Error },
    #[error("cold storage file unlock failed for {path}: {source}")]
    FileUnlock { path: PathBuf, source: io::Error },
    #[error("cold storage mkdir failed for {path}: {source}")]
    CreateDir { path: PathBuf, source: io::Error },
    #[error("cold storage JSON serialization failed: {0}")]
    Serialize(serde_json::Error),
    #[error("cold storage file open failed for {path}: {source}")]
    Open { path: PathBuf, source: io::Error },
    #[error("cold storage write failed for {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("cold storage flush failed for {path}: {source}")]
    Flush { path: PathBuf, source: io::Error },
    #[error("cold storage sync failed for {path}: {source}")]
    Sync { path: PathBuf, source: io::Error },
    #[error("cold storage line count failed for {path}: {source}")]
    CountLines { path: PathBuf, source: io::Error },
}

#[cfg(unix)]
unsafe extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

#[cfg(unix)]
const LOCK_EX: c_int = 2;
#[cfg(unix)]
const LOCK_UN: c_int = 8;

/// Thread-safe JSONL cold storage writer.
pub struct ColdStorageWriter {
    base_dir: PathBuf,
    project_id: String,
    lock: Mutex<()>,
}

impl ColdStorageWriter {
    /// Create a new writer. `base_dir` is typically `~/.memra/cold_storage`.
    pub fn new(base_dir: PathBuf, project_id: String) -> Self {
        Self {
            base_dir,
            project_id,
            lock: Mutex::new(()),
        }
    }

    /// Create a disabled writer (no-op).
    pub fn disabled() -> Self {
        Self {
            base_dir: PathBuf::from("/dev/null"),
            project_id: String::new(),
            lock: Mutex::new(()),
        }
    }

    /// Append a record to the JSONL archive.
    ///
    /// Returns the cold_storage_ref string (e.g., "2026-04.jsonl:42").
    ///
    /// `Ok(None)` means the writer is explicitly disabled. Real append failures
    /// are returned so callers can roll back their SQL transaction.
    pub fn append(
        &self,
        note_id: &str,
        content: &str,
        layer: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Result<Option<String>, ColdStorageError> {
        if self.project_id.is_empty() {
            return Ok(None);
        }

        let _guard = match self.lock.lock() {
            Ok(g) => g,
            Err(e) => {
                return Err(ColdStorageError::LockPoisoned(e.to_string()));
            }
        };

        let now = Utc::now();
        let filename = now.format("%Y-%m").to_string() + ".jsonl";
        let project_dir = self.base_dir.join(&self.project_id);

        // Ensure directory exists
        if let Err(e) = fs::create_dir_all(&project_dir) {
            return Err(ColdStorageError::CreateDir {
                path: project_dir,
                source: e,
            });
        }

        let file_path = project_dir.join(&filename);
        let record = json!({
            "id": note_id,
            "content": content,
            "layer": layer,
            "timestamp": now.to_rfc3339(),
            "project_id": self.project_id,
            "metadata": metadata,
        });

        let line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(e) => return Err(ColdStorageError::Serialize(e)),
        };

        // Append to file
        let mut file = match fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&file_path)
        {
            Ok(f) => f,
            Err(e) => {
                return Err(ColdStorageError::Open {
                    path: file_path,
                    source: e,
                });
            }
        };

        let line_count = with_file_lock(&mut file, &file_path, |file| {
            if let Err(e) = writeln!(file, "{line}") {
                return Err(ColdStorageError::Write {
                    path: file_path.clone(),
                    source: e,
                });
            }

            if let Err(e) = file.flush() {
                return Err(ColdStorageError::Flush {
                    path: file_path.clone(),
                    source: e,
                });
            }

            if let Err(e) = file.sync_all() {
                return Err(ColdStorageError::Sync {
                    path: file_path.clone(),
                    source: e,
                });
            }

            count_lines(&file_path).map_err(|source| ColdStorageError::CountLines {
                path: file_path.clone(),
                source,
            })
        })?;
        Ok(Some(format!("{filename}:{line_count}")))
    }
}

fn with_file_lock<T, F>(file: &mut fs::File, path: &Path, f: F) -> Result<T, ColdStorageError>
where
    F: FnOnce(&mut fs::File) -> Result<T, ColdStorageError>,
{
    lock_file(file, path)?;
    let result = f(file);
    let unlock_result = unlock_file(file, path);
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(_)) => Err(error),
    }
}

#[cfg(unix)]
fn lock_file(file: &fs::File, path: &Path) -> Result<(), ColdStorageError> {
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
    if rc == -1 {
        return Err(ColdStorageError::FileLock {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn unlock_file(file: &fs::File, path: &Path) -> Result<(), ColdStorageError> {
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if rc == -1 {
        return Err(ColdStorageError::FileUnlock {
            path: path.to_path_buf(),
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn lock_file(_file: &fs::File, _path: &Path) -> Result<(), ColdStorageError> {
    Ok(())
}

#[cfg(not(unix))]
fn unlock_file(_file: &fs::File, _path: &Path) -> Result<(), ColdStorageError> {
    Ok(())
}

fn count_lines(path: &Path) -> io::Result<usize> {
    let content = fs::read_to_string(path)?;
    Ok(content.lines().count())
}
