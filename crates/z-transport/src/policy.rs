//! Unified privacy policy contract shared across `z-engine`, native apps, and SDK surfaces.

use serde::{Deserialize, Serialize};

/// Canonical mainnet/testnet birthday rounding window (10,000 blocks ≈ 9 days).
pub const COARSE_BIRTHDAY_BUCKET: u32 = 10_000;

/// How full transaction / memo enhancement behaves after compact block scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum MemoPolicy {
    /// Default for Private Cloud / PIR-enabled wallets:
    /// Pure-Ironwood memos use Enhance PIR automatically; non-Ironwood / out-of-scope txs
    /// stay pending with zero exact-height or exact-txid requests unless the user triggers
    /// an explicit one-off privacy-preserving cover-bucket fetch.
    #[default]
    PirOnly,
    /// Enhance PIR for pure-Ironwood plus unconditional tip-aligned shared memo buckets
    /// (independent of wallet matches) for ongoing near-tip syncing.
    PirPlusSharedTip,
    /// Wallet-independent sequential shared memo range scanning from birthday to tip.
    SharedSequential,
    /// Direct per-txid `GetTransaction` lookups (opt-in only on self-hosted / explicit hosted servers).
    DirectTxidOptIn,
}

/// How transparent (`t1`/`t3`) receives, spends, and UTXOs are discovered and verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TransparentPolicy {
    /// Default on Private Cloud:
    /// - All-pool compact blocks (`pool_types = [1, 2, 3, 4]`) verify post-birthday UTXOs locally
    ///   with zero extra bytes and zero address disclosure (immediately shieldable!).
    /// - Transparent PIR v12 recovers confirmed history from the wallet's coarse birthday floor
    ///   (or from genesis when restoring an earlier birthday) without revealing transparent
    ///   addresses; pre-birthday unspent outputs verify against one aligned compact bucket before shielding.
    #[default]
    CompactPlusPir,
    /// Local all-pool compact block scanning only (v0.5.0+ lightwalletd/Zaino, zero addresses sent).
    CompactOnly,
    /// Transparent PIR v12 only (read-only history unless outputs are verified via compact bucket).
    PirOnly,
    /// Legacy direct `GetAddressUtxos` lookup (explicit opt-in only for pre-v0.5 servers).
    LegacyUtxoOptIn,
}

/// Whether sync may automatically resubmit saved pending transaction bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SubmissionPolicy {
    /// Default: never auto-rebroadcast during sync; unresolved sends require explicit user retry.
    #[default]
    Explicit,
    /// Opt-in legacy mode that resubmits identical saved signed bytes during sync.
    SavedBytesAuto,
}

/// Unified, fail-closed privacy configuration for a wallet session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletPrivacyPolicy {
    pub memo: MemoPolicy,
    pub transparent: TransparentPolicy,
    pub submission: SubmissionPolicy,
    /// When true (default), a hop or PIR failure never silently falls back to direct clearnet,
    /// `GetTransaction(txid)`, or `GetAddressUtxos`.
    pub fail_closed: bool,
    /// Birthday rounding bucket in blocks (default 10,000 on mainnet/testnet; 1 on regtest).
    pub birthday_rounding_blocks: u32,
    /// Minimum cover-bucket size (in blocks) for explicit one-off out-of-PIR tx fetches.
    pub cover_bucket_blocks: u32,
}

impl Default for WalletPrivacyPolicy {
    fn default() -> Self {
        Self {
            memo: MemoPolicy::PirOnly,
            transparent: TransparentPolicy::CompactPlusPir,
            submission: SubmissionPolicy::Explicit,
            fail_closed: true,
            birthday_rounding_blocks: COARSE_BIRTHDAY_BUCKET,
            cover_bucket_blocks: 10,
        }
    }
}

impl WalletPrivacyPolicy {
    /// Aligned inclusive `[start, end]` cover bucket for a single mined block height.
    /// Guarantees `start >= 1`, `end <= tip`, and ALWAYS returns a full `width`-block
    /// cover window whenever `tip >= width` (shifting backwards to `[tip - width + 1, tip]`
    /// near the chain tip so it NEVER shrinks to a single block `(h, h)`).
    pub fn aligned_cover_bucket(&self, height: u32, tip: u32) -> Option<(u32, u32)> {
        if height == 0 || tip == 0 || height > tip {
            return None;
        }
        let width = self.cover_bucket_blocks.clamp(10, 100);
        if tip <= width {
            return Some((1, tip));
        }
        let start = ((height - 1) / width) * width + 1;
        let end = start.saturating_add(width - 1);
        if end <= tip {
            Some((start, end))
        } else {
            Some((tip.saturating_sub(width - 1).max(1), tip))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_cover_bucket_never_returns_single_block_when_chain_has_blocks() {
        let p = WalletPrivacyPolicy::default();
        assert_eq!(p.aligned_cover_bucket(1, 100), Some((1, 10)));
        assert_eq!(p.aligned_cover_bucket(10, 100), Some((1, 10)));
        assert_eq!(p.aligned_cover_bucket(11, 100), Some((11, 20)));
        // Near tip (including exact bucket start == tip) shifts backwards to full 10-block width!
        assert_eq!(p.aligned_cover_bucket(95, 98), Some((89, 98)));
        assert_eq!(p.aligned_cover_bucket(91, 91), Some((82, 91)));
        assert_eq!(p.aligned_cover_bucket(101, 100), None);
    }
}
