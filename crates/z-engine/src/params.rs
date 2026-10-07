//! Sapling proving-parameter download / discovery.

use crate::error::{EngineError, Result};
use std::path::PathBuf;

/// Default folder used by `zakura-proofs` / zcashd (`ZcashParams`).
pub fn default_params_dir() -> PathBuf {
    zcash_proofs::default_params_folder().unwrap_or_else(|| PathBuf::from("ZcashParams"))
}

/// Ensure Sapling spend/output params exist (download + hash-check if needed).
///
/// Note: Orchard/Ironwood proving keys are generated in-process by halo2 and do
/// **not** use these Sapling parameter files.
pub fn ensure_sapling_params() -> Result<(PathBuf, PathBuf)> {
    remove_truncated_params();
    let paths = catch_param_panic(|| zcash_proofs::download_sapling_parameters(None))?
        .map_err(|e| EngineError::Message(format!("failed to download Sapling params: {e}")))?;
    Ok((paths.spend, paths.output))
}

static PROVER: std::sync::Mutex<Option<std::sync::Arc<zcash_proofs::prover::LocalTxProver>>> =
    std::sync::Mutex::new(None);

/// Process-wide [`LocalTxProver`], downloading Sapling params if absent.
///
/// Loading reads and hashes about 51 MB, so it happens once rather than on
/// every send or shield. A failed load is not cached, so a later call retries.
pub fn local_tx_prover() -> Result<std::sync::Arc<zcash_proofs::prover::LocalTxProver>> {
    let mut cached = PROVER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prover) = cached.as_ref() {
        return Ok(std::sync::Arc::clone(prover));
    }
    remove_truncated_params();
    let dir = default_params_dir();
    let spend = dir.join(zcash_proofs::SAPLING_SPEND_NAME);
    let output = dir.join(zcash_proofs::SAPLING_OUTPUT_NAME);
    if !(spend.is_file() && output.is_file()) {
        catch_param_panic(|| zcash_proofs::download_sapling_parameters(None))?
            .map_err(|_| EngineError::MissingParams)?;
    }
    let prover = std::sync::Arc::new(catch_param_panic(|| {
        zcash_proofs::prover::LocalTxProver::new(&spend, &output)
    })?);
    *cached = Some(std::sync::Arc::clone(&prover));
    Ok(prover)
}

/// Upstream panics on a parameter file with the wrong size or hash.
fn catch_param_panic<T>(load: impl FnOnce() -> T + std::panic::UnwindSafe) -> Result<T> {
    std::panic::catch_unwind(load).map_err(|_| {
        EngineError::Message(format!(
            "Sapling parameter files in {} are damaged; delete sapling-spend.params and \
             sapling-output.params there and retry",
            default_params_dir().display()
        ))
    })
}

/// Upstream writes a download straight to its final name, so an interrupted
/// download leaves a short file that it would otherwise reject with a panic.
fn remove_truncated_params() {
    const EXPECTED: [(&str, u64); 2] = [
        (zcash_proofs::SAPLING_SPEND_NAME, 47_958_396),
        (zcash_proofs::SAPLING_OUTPUT_NAME, 3_592_860),
    ];
    let dir = default_params_dir();
    for (name, bytes) in EXPECTED {
        let path = dir.join(name);
        if std::fs::metadata(&path).is_ok_and(|m| m.is_file() && m.len() < bytes) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_is_nonempty() {
        assert!(!default_params_dir().as_os_str().is_empty());
    }

    #[test]
    fn parameter_panics_become_errors() {
        let error = catch_param_panic(|| -> () { panic!("bad params") }).unwrap_err();
        assert!(error.to_string().contains("damaged"));
        assert_eq!(catch_param_panic(|| 7).unwrap(), 7);
    }
}
