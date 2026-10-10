//! Native (desktop/CLI) wallet backed by `zakura-client-sqlite` + lightwalletd/Zaino.

mod block_cache;
mod bridge;
mod lwd;
mod lwd_pipe;
mod pipeline;
mod rpc;
mod seed;
mod selective_scan;
mod transparent_funding;
mod transparent_refresh;
mod verify;
mod wallet;

pub use bridge::{origin_allowed, parse_loopback_bind, Bridge};
pub use lwd_pipe::{
    serve_lwd_pipe, LwdPipeOpts, PIPE_CHANNELS, PIPE_CHUNK, PIPE_CONCURRENCY, PIPE_MAX_BLOCKS,
};
pub use rpc::{pick_local_validator, probe_validator, ValidatorProbe};
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
        let mut file = std::fs::File::create(&tmp)?;
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
