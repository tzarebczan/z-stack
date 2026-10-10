//! Stable OS sidecar lease: keep this file across every database replacement.
//! Acquisition is bounded; a reset refuses while a PIR connection can still write.
use super::*;
use std::fs::{File, OpenOptions};

fn open(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("sqlite.ownership-lock"))
        .map_err(|_| EngineError::Message("native_pir_database_busy".into()))
}
#[cfg(feature = "native-pir")]
pub(super) fn shared(path: &Path) -> Result<File> {
    let file = open(path)?;
    file.try_lock_shared()
        .map_err(|_| EngineError::Message("native_pir_database_busy".into()))?;
    Ok(file)
}
pub(super) fn exclusive(path: &Path) -> Result<File> {
    let file = open(path)?;
    file.try_lock()
        .map_err(|_| EngineError::Message("native_pir_database_busy".into()))?;
    Ok(file)
}
