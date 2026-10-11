use crate::error::{EngineError, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

/// Cancellation and canonical commit share a linearization gate. After cancel
/// returns, a recovery transaction cannot start or finish a later commit.
pub struct RecoveryCancellation {
    cancelled: AtomicBool,
    commit: Mutex<()>,
}
impl Default for RecoveryCancellation {
    fn default() -> Self {
        Self::new()
    }
}
impl RecoveryCancellation {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            commit: Mutex::new(()),
        }
    }
    pub fn cancel(&self) {
        // Even a poisoned operation gate must never restore access.
        let _gate = self
            .commit
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    /// Existing discovery transports only read this flag. To cancel canonical
    /// work callers MUST use cancel(), which also excludes the commit boundary.
    pub fn flag(&self) -> &AtomicBool {
        &self.cancelled
    }
    pub(super) fn transaction<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let _gate = self
            .commit
            .lock()
            .map_err(|_| EngineError::Message("native_recovery_cancelled".into()))?;
        if self.is_cancelled() {
            return Err(EngineError::Message("native_recovery_cancelled".into()));
        }
        operation()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_work_never_enters_commit_operation() {
        let token = RecoveryCancellation::new();
        token.cancel();
        let mut entered = false;
        assert!(token
            .transaction(|| {
                entered = true;
                Ok(())
            })
            .is_err());
        assert!(!entered);
    }
    #[test]
    fn cancellation_waits_for_commit_that_already_won() {
        let token = std::sync::Arc::new(RecoveryCancellation::new());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let commit = token.clone();
        let worker = std::thread::spawn(move || {
            commit
                .transaction(|| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .unwrap()
        });
        entered_rx.recv().unwrap();
        let cancelling = token.clone();
        let canceller = std::thread::spawn(move || {
            cancelling.cancel();
            cancelled_tx.send(()).unwrap();
        });
        assert!(cancelled_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        canceller.join().unwrap();
        cancelled_rx.recv().unwrap();
        assert!(token.is_cancelled());
        assert!(token.transaction(|| Ok(())).is_err());
    }
}
