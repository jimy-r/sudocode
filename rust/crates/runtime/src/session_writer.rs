//! An exact transcript and its completion journal have one writer lifetime.
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::fs_backend::{FsBackend, FsLock};

pub const JOURNAL_VERSION: u32 = 2;

pub struct SessionWriter {
    lock: Arc<dyn FsLock>,
    deleted: AtomicBool,
    writes: Mutex<()>,
}

impl SessionWriter {
    pub fn acquire(fs: &dyn FsBackend, transcript: &Path) -> Result<Arc<Self>, String> {
        if !fs.fenced_locks() {
            return Err("completion_storage_exclusion_unavailable".into());
        }
        // A v1 writer never took this lock. Acquiring it cannot authorize a
        // live conversion. This unreleased development format is preserved and rejected;
        // the first released completion journal uses the shared writer protocol.
        let journal = transcript.with_extension("completions.json");
        let check = || -> Result<(), String> {
            if fs
                .exists(&journal.to_string_lossy())
                .map_err(|e| e.to_string())?
            {
                let value: serde_json::Value = serde_json::from_slice(
                    &fs.read(&journal.to_string_lossy())
                        .map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())?;
                if value["deleted"].is_string() {
                    return Err("session_deleted".into());
                }
                if value["version"].as_u64() != Some(u64::from(JOURNAL_VERSION)) {
                    return Err("legacy_writer_unconfirmed".into());
                }
                crate::subagent_completion::validate_journal(value)?;
            }
            Ok(())
        };
        check()?;
        let lock = fs
            .try_lock(
                &transcript.with_extension("writer.lock").to_string_lossy(),
                30_000,
            )
            .map_err(|e| e.to_string())?
            .ok_or("session_writer_busy")?;
        check()?;
        Ok(Arc::new(Self {
            lock,
            deleted: AtomicBool::new(false),
            writes: Mutex::new(()),
        }))
    }

    pub fn check(&self) -> Result<(), String> {
        if self.deleted.load(Ordering::Acquire) {
            return Err("session_deleted".into());
        }
        if !self.lock.renew().map_err(|e| e.to_string())? {
            return Err("session_writer_lost".into());
        }
        Ok(())
    }

    /// Serialize transcript writes with journal deletion for this lifetime.
    /// Checking the tombstone before taking this mutex leaves a deletion race.
    pub fn write(&self) -> Result<MutexGuard<'_, ()>, String> {
        let guard = self.writes.lock().map_err(|_| "session writer poisoned")?;
        self.check()?;
        Ok(guard)
    }

    pub fn retire(&self) {
        self.deleted.store(true, Ordering::Release);
    }
}
