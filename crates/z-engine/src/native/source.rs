//! Application-authenticated native sources and durable wallet capability policy.

use super::lwd::{self, LwdClient};
use crate::{
    error::{EngineError, Result},
    Network,
};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};
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

/// A fresh wallet's application-authenticated scan and explicit submission source.
/// The binding identifies the application's accepted publisher/profile, not a URL.
/// The application must verify chain data and transport responses before serving
/// this channel. The SDK never creates a replacement channel or a direct fallback.
#[derive(Clone)]
pub struct NativeWalletSource {
    channel: Channel,
    network: Network,
    binding: [u8; 32],
}

impl NativeWalletSource {
    pub fn new(channel: Channel, network: Network, binding: [u8; 32]) -> Self {
        Self {
            channel,
            network,
            binding,
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn binding(&self) -> [u8; 32] {
        self.binding
    }

    pub(super) fn client(&self) -> LwdClient {
        lwd::read_only_client(self.channel.clone())
    }

    pub(super) fn submission_client(&self) -> LwdClient {
        lwd::wallet_source_client(self.channel.clone())
    }
}

const WALLET_POLICY_FILE: &str = "native-wallet-source.json";
const WALLET_SETUP_FILE: &str = "native-wallet-source-setup.json";

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct WalletBinding {
    version: u8,
    network: String,
    binding: [u8; 32],
}

fn wallet_invalid() -> EngineError {
    EngineError::Message("native_wallet_source_policy_invalid".into())
}

fn sync_wallet_directory(root: &Path) -> Result<()> {
    #[cfg(unix)]
    std::fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| wallet_invalid())?;
    Ok(())
}

fn read_binding_file(root: &Path) -> Result<Option<WalletBinding>> {
    let path = root.join(WALLET_POLICY_FILE);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 => {}
        _ => return Err(wallet_invalid()),
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| wallet_invalid())?
        .take(1025)
        .read_to_end(&mut bytes)
        .map_err(|_| wallet_invalid())?;
    if bytes.len() > 1024 {
        return Err(wallet_invalid());
    }
    let policy: WalletBinding = serde_json::from_slice(&bytes).map_err(|_| wallet_invalid())?;
    if policy.version != 1
        || Network::parse(&policy.network).is_none_or(|n| n.as_str() != policy.network)
    {
        return Err(wallet_invalid());
    }
    Ok(Some(policy))
}

/// Both independent copies prevent a cold open or endpoint edit from dropping
/// the supplied-source requirement. Conflicting or malformed copies fail closed.
pub(super) fn wallet_binding(root: &Path) -> Result<Option<WalletBinding>> {
    let file = read_binding_file(root)?;
    let db = root.join("data.sqlite");
    let stored = if db.try_exists().map_err(|_| wallet_invalid())? {
        let conn =
            rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|_| wallet_invalid())?;
        let objects: (u64,u64) = conn.query_row("SELECT COUNT(*),COALESCE(SUM(type='table'),0) FROM sqlite_master WHERE name='ext_native_wallet_source_v1'", [], |r|Ok((r.get(0)?,r.get(1)?))).map_err(|_| wallet_invalid())?;
        match objects {
            (0, 0) => None,
            (1, 1) => {
                let mut rows = conn
                    .prepare("SELECT id,version,network,binding FROM ext_native_wallet_source_v1")
                    .map_err(|_| wallet_invalid())?;
                let mut rows = rows.query([]).map_err(|_| wallet_invalid())?;
                let row = rows
                    .next()
                    .map_err(|_| wallet_invalid())?
                    .ok_or_else(wallet_invalid)?;
                let id: u64 = row.get(0).map_err(|_| wallet_invalid())?;
                let version: u8 = row.get(1).map_err(|_| wallet_invalid())?;
                let network: String = row.get(2).map_err(|_| wallet_invalid())?;
                let bytes: Vec<u8> = row.get(3).map_err(|_| wallet_invalid())?;
                let binding = bytes.try_into().map_err(|_| wallet_invalid())?;
                if id != 1
                    || version != 1
                    || Network::parse(&network).is_none_or(|n| n.as_str() != network)
                    || rows.next().map_err(|_| wallet_invalid())?.is_some()
                {
                    return Err(wallet_invalid());
                }
                Some(WalletBinding {
                    version,
                    network,
                    binding,
                })
            }
            _ => return Err(wallet_invalid()),
        }
    } else {
        None
    };
    match (file, stored) {
        (Some(file), Some(stored)) if file != stored => Err(wallet_invalid()),
        (Some(policy), _) | (_, Some(policy)) => Ok(Some(policy)),
        (None, None)
            if root
                .join(WALLET_SETUP_FILE)
                .try_exists()
                .map_err(|_| wallet_invalid())? =>
        {
            Err(wallet_invalid())
        }
        (None, None) => Ok(None),
    }
}

pub(super) fn require_wallet(root: &Path, source: &NativeWalletSource) -> Result<()> {
    if read_only(root)? {
        return Err(refused());
    }
    match wallet_binding(root)? {
        Some(policy)
            if policy.network == source.network.as_str() && policy.binding == source.binding =>
        {
            Ok(())
        }
        Some(_) => Err(EngineError::Message("native_wallet_source_mismatch".into())),
        None => Err(EngineError::Message("native_wallet_source_required".into())),
    }
}

pub(super) fn enroll_wallet(root: &Path, source: &NativeWalletSource) -> Result<()> {
    if read_only(root)? {
        return Err(refused());
    }
    let expected = WalletBinding {
        version: 1,
        network: source.network.as_str().into(),
        binding: source.binding,
    };
    let prior = wallet_binding(root)?;
    if prior.as_ref().is_some_and(|p| p != &expected) {
        return Err(wallet_invalid());
    }
    let db = root.join("data.sqlite");
    // The reservation must precede creation of any wallet database. This API
    // cannot enroll an ordinary or historical read-only wallet into spending.
    if prior.is_none() && db.try_exists().map_err(|_| wallet_invalid())? {
        return Err(wallet_invalid());
    }
    super::replace_file(
        &root.join(WALLET_POLICY_FILE),
        &serde_json::to_vec(&expected).map_err(|_| wallet_invalid())?,
        |_| Ok(()),
    )?;
    sync_wallet_directory(root)?;
    if db.try_exists().map_err(|_| wallet_invalid())? {
        let mut conn = rusqlite::Connection::open(db).map_err(|_| wallet_invalid())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|_| wallet_invalid())?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|_| wallet_invalid())?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|_| wallet_invalid())?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS ext_native_wallet_source_v1 (id INTEGER PRIMARY KEY CHECK(id=1), version INTEGER NOT NULL CHECK(version=1), network TEXT NOT NULL, binding BLOB NOT NULL CHECK(length(binding)=32));").map_err(|_| wallet_invalid())?;
        tx.execute(
            "INSERT OR IGNORE INTO ext_native_wallet_source_v1 VALUES(1,1,?1,?2)",
            rusqlite::params![expected.network, expected.binding.as_slice()],
        )
        .map_err(|_| wallet_invalid())?;
        tx.commit().map_err(|_| wallet_invalid())?;
        let (busy, frames, checkpointed): (u32, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(|_| wallet_invalid())?;
        if busy != 0 || frames != checkpointed {
            return Err(wallet_invalid());
        }
    }
    require_wallet(root, source)
}

const SETUP_FILE: &str = "native-source-setup.json";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Setup {
    version: u8,
    network: String,
    pub birthday_height: u32,
    account_index: u32,
}

pub(super) fn setup(root: &Path) -> Result<Option<Setup>> {
    setup_file(root, SETUP_FILE)
}

fn setup_file(root: &Path, filename: &str) -> Result<Option<Setup>> {
    let path = root.join(filename);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= 1024 => {}
        _ => return Err(policy_invalid()),
    };
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| policy_invalid())?
        .take(1025)
        .read_to_end(&mut bytes)
        .map_err(|_| policy_invalid())?;
    if bytes.len() > 1024 {
        return Err(policy_invalid());
    }
    let setup: Setup = serde_json::from_slice(&bytes).map_err(|_| policy_invalid())?;
    if setup.version != 1
        || Network::parse(&setup.network).is_none_or(|n| n.as_str() != setup.network)
        || setup.account_index >= 0x8000_0000
    {
        return Err(policy_invalid());
    }
    Ok(Some(setup))
}

pub(super) fn setup_birthday(
    root: &Path,
    network: Network,
    birthday: Option<u32>,
    account_index: u32,
    retained_seed: bool,
) -> Result<Option<u32>> {
    setup_birthday_file(
        root,
        SETUP_FILE,
        network,
        birthday,
        account_index,
        retained_seed,
    )
}

fn setup_birthday_file(
    root: &Path,
    filename: &str,
    network: Network,
    birthday: Option<u32>,
    account_index: u32,
    retained_seed: bool,
) -> Result<Option<u32>> {
    if account_index >= 0x8000_0000 {
        return Err(EngineError::Message("native_source_setup_mismatch".into()));
    }
    match setup_file(root, filename)? {
        Some(setup)
            if setup.network == network.as_str()
                && setup.account_index == account_index
                && birthday.is_none_or(|b| b == setup.birthday_height) =>
        {
            Ok(Some(setup.birthday_height))
        }
        Some(_) => Err(EngineError::Message("native_source_setup_mismatch".into())),
        None if retained_seed => Err(EngineError::Message(
            "native_source_setup_binding_required".into(),
        )),
        None => Ok(birthday),
    }
}

pub(super) fn bind_setup(
    root: &Path,
    network: Network,
    birthday_height: u32,
    account_index: u32,
) -> Result<()> {
    bind_setup_file(root, SETUP_FILE, network, birthday_height, account_index)
}

fn bind_setup_file(
    root: &Path,
    filename: &str,
    network: Network,
    birthday_height: u32,
    account_index: u32,
) -> Result<()> {
    setup_birthday_file(
        root,
        filename,
        network,
        Some(birthday_height),
        account_index,
        false,
    )?;
    let bytes = serde_json::to_vec(&Setup {
        version: 1,
        network: network.as_str().into(),
        birthday_height,
        account_index,
    })
    .map_err(|_| policy_invalid())?;
    super::replace_file(&root.join(filename), &bytes, |_| Ok(()))
}

pub(super) fn wallet_setup(root: &Path) -> Result<Option<Setup>> {
    setup_file(root, WALLET_SETUP_FILE)
}
pub(super) fn wallet_setup_birthday(
    root: &Path,
    network: Network,
    birthday: Option<u32>,
    account_index: u32,
    retained_seed: bool,
) -> Result<Option<u32>> {
    setup_birthday_file(
        root,
        WALLET_SETUP_FILE,
        network,
        birthday,
        account_index,
        retained_seed,
    )
}
pub(super) fn bind_wallet_setup(
    root: &Path,
    network: Network,
    birthday_height: u32,
    account_index: u32,
) -> Result<()> {
    bind_setup_file(
        root,
        WALLET_SETUP_FILE,
        network,
        birthday_height,
        account_index,
    )?;
    sync_wallet_directory(root)
}

const POLICY_FILE: &str = "native-source-policy";
const POLICY: &[u8] = b"z-stack-native-read-only-source-v1\n";

fn policy_invalid() -> EngineError {
    EngineError::Message("native_source_policy_invalid".into())
}

pub(super) fn read_only(root: &Path) -> Result<bool> {
    let setup = setup(root)?.is_some();
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
        return Ok(sidecar || setup);
    }
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| policy_invalid())?;
    let objects: (u64,u64) = conn.query_row("SELECT COUNT(*),COALESCE(SUM(type='table'),0) FROM sqlite_master WHERE name='ext_native_source_policy_v1'", [], |r|Ok((r.get(0)?,r.get(1)?))).map_err(|_| policy_invalid())?;
    match objects {
        (0, 0) => Ok(sidecar || setup),
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
    if wallet_binding(root)?.is_some() {
        return Err(EngineError::Message("native_wallet_source_required".into()));
    }
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
        // Successful enrollment must survive a copy of the main database
        // without its WAL or policy sidecar. A retained reader can prevent
        // automatic checkpointing when this connection closes.
        let (busy, frames, checkpointed): (u32, i64, i64) = conn
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .map_err(|_| policy_invalid())?;
        if busy != 0 || frames != checkpointed {
            return Err(EngineError::Message(
                "native_source_checkpoint_incomplete".into(),
            ));
        }
    }
    if !read_only(root)? {
        return Err(policy_invalid());
    }
    Ok(())
}

pub(super) fn refused() -> EngineError {
    EngineError::Message("native_read_only_source".into())
}
