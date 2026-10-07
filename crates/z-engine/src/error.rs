use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),

    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid network: {0}")]
    InvalidNetwork(String),

    #[error("wallet already exists at {0}")]
    AlreadyExists(String),

    #[error("wallet not found at {0}")]
    NotFound(String),

    #[error("no account in wallet database")]
    NoAccount,

    #[error("sync required before this operation")]
    SyncRequired,

    #[error("insufficient funds")]
    InsufficientFunds,

    #[error("proving parameters missing — run `z-wallet params` first")]
    MissingParams,

    #[error("seed unlock required (passphrase, OS keychain / Hello, or paste the words)")]
    SeedLocked,

    #[error("this wallet is view-only — paste the seed to send")]
    ViewOnly,

    #[error("seed decrypt failed (wrong passphrase?)")]
    SeedDecryptFailed,

    #[error("chain mismatch: wallet={wallet}, server={server}")]
    ChainMismatch { wallet: String, server: String },

    #[error("birthday {birthday} is too far below tip {tip} for default sync (max gap {max_gap}); raise birthday or set Z_STACK_ALLOW_DEEP_SYNC=1")]
    DeepSyncRejected {
        birthday: u32,
        tip: u32,
        max_gap: u32,
    },

    #[error("birthday {birthday} is above chain tip {tip}")]
    BirthdayAboveTip { birthday: u32, tip: u32 },

    #[error("broadcast rejected ({code}): {message}")]
    BroadcastRejected { code: i32, message: String },

    #[error("broadcast failed after persist; saved transaction retained; sync to reconcile submission: {0}")]
    BroadcastFailed(String),

    #[error("reorg at height {height}; rescan from {next}")]
    Reorg { height: u32, next: u32 },

    #[error("gRPC/transport: {0}")]
    Transport(String),

    #[error("wallet db: {0}")]
    WalletDb(String),

    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, EngineError>;

/// Stable codes — keep lockstep with `@z-stack/core` `WalletErrorCode`.
impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::AlreadyExists(_) => "already_exists",
            Self::NoAccount => "no_account",
            Self::SyncRequired => "sync_required",
            Self::InsufficientFunds => "insufficient_funds",
            Self::MissingParams => "missing_params",
            Self::SeedLocked => "seed_locked",
            Self::ViewOnly => "view_only",
            Self::SeedDecryptFailed => "seed_decrypt_failed",
            Self::ChainMismatch { .. } => "chain_mismatch",
            Self::DeepSyncRejected { .. } => "deep_sync_rejected",
            Self::BirthdayAboveTip { .. } => "birthday_above_tip",
            Self::BroadcastRejected { .. } => "broadcast_rejected",
            Self::BroadcastFailed(_) => "broadcast_failed",
            Self::Reorg { .. } => "reorg",
            Self::Transport(_) => "transport",
            Self::WalletDb(_) => "wallet_db",
            Self::InvalidNetwork(_) => "invalid_network",
            Self::Io(_) => "transport",
            Self::Message(m) => classify_wallet_error_code(m),
            Self::Anyhow(e) => classify_wallet_error_code(&e.to_string()),
        }
    }

    /// User-facing string. Known codes hide raw `wallet db: …` dumps.
    pub fn user_message(&self) -> String {
        wallet_error_user_message(self.code(), &self.to_string())
    }
}

/// Map a known engine / bridge / WASM string to a stable code. Unknown → `unknown`.
pub fn classify_wallet_error_code(message: &str) -> &'static str {
    let lower = message.trim().to_ascii_lowercase();
    if lower.contains("bridge token")
        || lower.contains("401 ")
        || lower.contains("unauthorized")
        || lower.contains("not a zaino password")
        || lower.contains("origin not allowed")
    {
        return "auth";
    }
    if lower.contains("insufficient") || lower.contains("no spendable") {
        return "insufficient_funds";
    }
    if lower.contains("sync required") || lower.contains("still behind tip") {
        return "sync_required";
    }
    if lower.contains("seed unlock")
        || lower.contains("seedlocked")
        || lower.contains("re-enter spending seed")
    {
        return "seed_locked";
    }
    if lower.contains("view-only") || lower.contains("view only") {
        return "view_only";
    }
    if lower.contains("seed decrypt") || lower.contains("wrong passphrase") {
        return "seed_decrypt_failed";
    }
    if lower.contains("already exists") {
        return "already_exists";
    }
    if lower.contains("wallet not found") || lower.contains("no wasm wallet") {
        return "not_found";
    }
    if lower.contains("no account") {
        return "no_account";
    }
    if lower.contains("proving parameters missing") || lower.contains("missing params") {
        return "missing_params";
    }
    if lower.contains("chain mismatch") {
        return "chain_mismatch";
    }
    if lower.contains("too far below tip") || lower.contains("deep sync") {
        return "deep_sync_rejected";
    }
    if lower.contains("above chain tip") || lower.contains("above tip") {
        return "birthday_above_tip";
    }
    if lower.contains("broadcast rejected") {
        return "broadcast_rejected";
    }
    if lower.contains("broadcast failed") {
        return "broadcast_failed";
    }
    if lower.starts_with("reorg ") || lower.contains("reorg at height") {
        return "reorg";
    }
    if lower.contains("grpc/transport")
        || lower.contains("i/o:")
        || lower.contains("engine not reachable")
        || lower.contains("failed to fetch")
        || lower.contains("networkerror")
    {
        return "transport";
    }
    if lower.contains("wallet db") {
        return "wallet_db";
    }
    if lower.contains("invalid network") || lower.contains("unknown network") {
        return "invalid_network";
    }
    if lower.contains("invalid address")
        || lower.contains("not a zcash:")
        || lower.contains("not uri-safe")
        || lower.contains("empty address")
        || lower.contains("no address")
    {
        return "invalid_address";
    }
    if lower.contains("empty amount")
        || lower.contains("decimal zec")
        || lower.contains("more than 8 decimal")
        || lower.contains("amount must be greater")
    {
        return "invalid_amount";
    }
    if lower.contains("memo longer") || lower.contains("memo:") {
        return "invalid_memo";
    }
    if lower.contains("transparent send is not supported") {
        return "unsupported_transparent";
    }
    if lower.contains("orchard-only")
        || lower.contains("no shielded receiver")
        || lower.contains("sapling destinations")
    {
        return "unsupported_destination";
    }
    "unknown"
}

pub fn wallet_error_user_message(code: &str, fallback: &str) -> String {
    match code {
        "not_found" => "No wallet on this device.".into(),
        "already_exists" => "A wallet already exists here.".into(),
        "no_account" => "This wallet has no account yet.".into(),
        "sync_required" => "Sync the wallet before sending.".into(),
        "insufficient_funds" => {
            "Not enough shielded funds for this send (including the fee).".into()
        }
        "missing_params" => "Proving parameters are missing. Run `z-wallet params` first.".into(),
        "seed_locked" => "Unlock the seed to send or shield.".into(),
        "view_only" => "This wallet is view-only. Paste the seed to send.".into(),
        "seed_decrypt_failed" => "Could not decrypt the seed. Check the passphrase.".into(),
        "chain_mismatch" => "This wallet does not match the light server chain.".into(),
        "deep_sync_rejected" => "Birthday is too far below tip for a default sync.".into(),
        "birthday_above_tip" => "Birthday is above the current chain tip.".into(),
        "broadcast_rejected" => "The network rejected this transaction.".into(),
        "broadcast_failed" => {
            "The transaction is saved locally. Submission was not confirmed; sync before retrying."
                .into()
        }
        "reorg" => "The chain reorganized. Rescan to continue.".into(),
        "transport" => "Could not reach the light server.".into(),
        "wallet_db" => {
            "The wallet database hit an error. Try wipe scan & resync if notes look wrong.".into()
        }
        "invalid_network" => "Unknown network.".into(),
        "invalid_address" => "That address is not a valid Zcash destination.".into(),
        "invalid_amount" => "Enter a valid ZEC amount.".into(),
        "invalid_memo" => "Memo is too long.".into(),
        "auth" => "This action needs the bridge token from `z-wallet serve`.".into(),
        "unsupported_transparent" => "Transparent send is not supported. Shield first.".into(),
        "unsupported_destination" => {
            "Need a shielded receiver. Transparent-only destinations are not supported.".into()
        }
        _ => {
            let f = fallback.trim();
            if f.is_empty() {
                "Something went wrong.".into()
            } else {
                f.to_string()
            }
        }
    }
}

/// Prefer a typed {@link EngineError} in the chain; else classify the display string.
pub fn display_anyhow(err: &anyhow::Error) -> String {
    for cause in err.chain() {
        if let Some(e) = cause.downcast_ref::<EngineError>() {
            return e.user_message();
        }
    }
    let raw = format!("{err:#}");
    wallet_error_user_message(classify_wallet_error_code(&raw), &raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_hide_wallet_db_dump() {
        let e = EngineError::WalletDb("sqlite explode at page 9".into());
        assert_eq!(e.code(), "wallet_db");
        assert!(!e.user_message().contains("sqlite explode"));
        assert!(e.user_message().contains("wipe scan"));
        assert_eq!(
            classify_wallet_error_code("insufficient funds"),
            "insufficient_funds"
        );
        assert_eq!(
            EngineError::Message("amount must be greater than 0".into()).code(),
            "invalid_amount"
        );
    }
}
