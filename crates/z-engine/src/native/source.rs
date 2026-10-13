//! Caller-provided, read-only native public-data transport.

use super::lwd::{self, LwdClient};
use crate::{
    error::{EngineError, Result},
    Network,
};
use std::path::Path;
use tonic::transport::Channel;

/// An application-authenticated channel. The application owns transport trust,
/// publication freshness and accepted-chain persistence before supplying it.
/// The SDK never constructs an endpoint from this source or falls back to one.
#[derive(Clone)]
pub struct NativeReadOnlySource {
    channel: Channel,
    network: Network,
}

impl NativeReadOnlySource {
    pub fn new(channel: Channel, network: Network) -> Self {
        Self { channel, network }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub(super) fn client(&self) -> LwdClient {
        lwd::read_only_client(self.channel.clone())
    }
}

const POLICY_FILE: &str = "native-source-policy";
const POLICY: &[u8] = b"z-stack-native-read-only-source-v1\n";

fn policy_invalid() -> EngineError {
    EngineError::Message("native_source_policy_invalid".into())
}

pub(super) fn read_only(root: &Path) -> Result<bool> {
    let path = root.join(POLICY_FILE);
    let sidecar = match std::fs::symlink_metadata(&path) {
        Ok(meta)
            if meta.is_file()
                && !meta.file_type().is_symlink()
                && meta.len() == POLICY.len() as u64 =>
        {
            if std::fs::read(path).map_err(|_| policy_invalid())? != POLICY {
                return Err(policy_invalid());
            }
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        _ => return Err(policy_invalid()),
    };
    let db = root.join("data.sqlite");
    if !db.try_exists().map_err(|_| policy_invalid())? {
        return Ok(sidecar);
    }
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| policy_invalid())?;
    let objects: (u64,u64) = conn.query_row("SELECT COUNT(*),COALESCE(SUM(type='table'),0) FROM sqlite_master WHERE name='ext_native_source_policy_v1'", [], |r|Ok((r.get(0)?,r.get(1)?))).map_err(|_| policy_invalid())?;
    match objects {
        (0, 0) => Ok(sidecar),
        (1, 1) => {
            let state: (u64,Option<u64>,Option<u64>,Option<u64>) = conn.query_row("SELECT COUNT(*),MIN(id),MIN(version),MIN(read_only) FROM ext_native_source_policy_v1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_|policy_invalid())?;
            if state != (1, Some(1), Some(1), Some(1)) {
                return Err(policy_invalid());
            }
            Ok(true)
        }
        _ => Err(policy_invalid()),
    }
}

pub(super) fn enroll(root: &Path) -> Result<()> {
    read_only(root)?;
    // First protect interrupted setup before a database or wallet.json exists.
    super::replace_file(&root.join(POLICY_FILE), POLICY, |_| Ok(()))?;
    let db = root.join("data.sqlite");
    if db.try_exists().map_err(|_| policy_invalid())? {
        let mut conn = rusqlite::Connection::open(db).map_err(|_| policy_invalid())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| policy_invalid())?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| policy_invalid())?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| policy_invalid())?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS ext_native_source_policy_v1 (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL CHECK(version=1), read_only INTEGER NOT NULL CHECK(read_only=1)); INSERT OR IGNORE INTO ext_native_source_policy_v1 VALUES(1,1,1);").map_err(|_|policy_invalid())?;
        tx.commit().map_err(|_| policy_invalid())?;
    }
    if !read_only(root)? {
        return Err(policy_invalid());
    }
    Ok(())
}

pub(super) fn refused() -> EngineError {
    EngineError::Message("native_read_only_source".into())
}
