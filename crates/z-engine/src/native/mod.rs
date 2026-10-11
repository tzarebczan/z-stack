//! Native (desktop/CLI) wallet backed by `zakura-client-sqlite` + lightwalletd/Zaino.

mod accepted_blocks;
mod block_cache;
mod bridge;
mod chain_identity;
mod lwd;
mod lwd_pipe;
mod pipeline;
mod rpc;
mod runtime;
mod seed;
mod selective_scan;
mod submission_channel;
mod transparent_funding;
mod transparent_refresh;
mod tree_state;
mod verify;
mod wallet;

pub use accepted_blocks::{
    verify_mainnet_block_window, AcceptedMainnetWindow, VerifiedMainnetBlockWindow,
    MAINNET_BLOCK_WINDOW_MAGIC, MAINNET_BLOCK_WINDOW_MAX_BYTES, MAINNET_BLOCK_WINDOW_POLICY,
    MAINNET_BLOCK_WINDOW_WIDTH,
};

pub use bridge::{origin_allowed, parse_loopback_bind, Bridge};
pub use lwd_pipe::{
    serve_lwd_pipe, LwdPipeOpts, PIPE_CHANNELS, PIPE_CHUNK, PIPE_CONCURRENCY, PIPE_MAX_BLOCKS,
};
pub use rpc::{pick_local_validator, probe_validator, ValidatorProbe};
pub use runtime::{
    default_native_threads, initialize_native_threads, NativeThreadPool, MAX_NATIVE_THREADS,
};
pub use seed::{SeedStore, SeedUnlock, UnlockPolicy};
pub use verify::{TreeReport, TreeRootCheck, WitnessCheck};
pub use wallet::{
    CreatedWallet, LightProbe, NativeWallet, PaymentReceipt, RegtestScanSchedule, SeedAuth,
    WalletPaths, BATCH_SIZE, MAX_MEM_SYNC_BLOCKS,
};

/// Replace `path` with `bytes` so a crash leaves the old file or the new one,
/// never a truncated mix: write a sibling, flush it, let `prepare` finish it
/// (permissions), then rename it over the target.
pub(crate) fn replace_file(
    path: &std::path::Path,
    bytes: &[u8],
    prepare: impl FnOnce(&std::path::Path) -> crate::error::Result<()>,
) -> crate::error::Result<()> {
    use std::io::Write;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.tmp"));
    let written = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Creation mode does not change a temp file retained by a crash.
            // Restrict its handle before truncating or writing private metadata.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.set_len(0)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        prepare(&tmp)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    #[cfg(unix)]
    if written.is_ok() {
        if let Some(dir) = path.parent().and_then(|d| std::fs::File::open(d).ok()) {
            let _ = dir.sync_all();
        }
    }
    written
}

/// Experimental Rust-only qualification adapters.
#[cfg(feature = "native-pir")]
pub use transparent_wallet::{
    FilterSource as PirFilterSource, ShardTransport as PirShardTransport,
    WorkLimits as PirWorkLimits,
};
#[cfg(feature = "native-pir")]
pub use wallet::{
    verify_regtest_recovery_blocks, PirConfirmedTransaction, PirDiscoveryReport, PirRecoveryReport,
    RecoveryCancellation, RegtestAcceptedChain, VerifiedRegtestRecoveryBlocks,
};

#[cfg(feature = "native-pir")]
pub use transparent_wallet::{
    client::Table as PirTable, refusal as pir_refusal, transport::BoxError as PirTransportError,
    Overloaded as PirOverloaded, StaleRevision as PirStaleRevision,
};

#[cfg(all(test, unix))]
mod replacement_tests {
    use super::replace_file;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn replacement_temp_is_private_before_rename_and_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallet.json");
        let tmp = dir.path().join("wallet.json.tmp");
        for retained in [false, true] {
            if retained {
                std::fs::write(&tmp, b"interrupted fixture metadata").unwrap();
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            replace_file(&path, b"fixture private metadata", |prepared| {
                assert_eq!(prepared, tmp);
                assert_eq!(
                    std::fs::metadata(prepared)?.permissions().mode() & 0o777,
                    0o600
                );
                assert_eq!(std::fs::read(prepared)?, b"fixture private metadata");
                Ok(())
            })
            .unwrap();
            assert!(!tmp.exists());
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
