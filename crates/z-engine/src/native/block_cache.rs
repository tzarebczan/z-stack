//! Compact-block caches for `zcash_client_backend::sync::run`.
//!
//! [`MemBlockCache`] is for tests. Native sync uses [`FsBlockCache`] under
//! `wallet-data/blocks/` as packed range files (not one file per height).

use crate::web::{decode_delimited, encode_delimited};
use async_trait::async_trait;
use prost::Message;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use zcash_client_backend::{
    data_api::{
        chain::{error::Error as ChainError, BlockCache, BlockSource},
        scanning::ScanRange,
    },
    proto::compact_formats::CompactBlock,
};
use zcash_protocol::consensus::BlockHeight;

#[cfg(test)]
#[derive(Debug, Error)]
pub enum MemCacheError {
    #[error("block cache lock poisoned")]
    Poisoned,
    #[error("no cached blocks for requested range starting at {0}")]
    EmptyRange(BlockHeight),
    #[error("compact block height {0} does not fit in u32")]
    HeightOverflow(u64),
}

#[cfg(test)]
#[derive(Default)]
pub struct MemBlockCache {
    blocks: Mutex<BTreeMap<BlockHeight, CompactBlock>>,
}

#[cfg(test)]
impl MemBlockCache {
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
impl BlockSource for MemBlockCache {
    type Error = MemCacheError;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), ChainError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<WalletErrT, Self::Error>>,
    {
        let guard = self
            .blocks
            .lock()
            .map_err(|_| ChainError::BlockSource(MemCacheError::Poisoned))?;
        let start = from_height.unwrap_or(BlockHeight::from(0));
        let take = limit.unwrap_or(usize::MAX);
        for (_, block) in guard.range(start..).take(take) {
            with_block(block.clone())?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[async_trait]
impl BlockCache for MemBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let guard = self.blocks.lock().map_err(|_| MemCacheError::Poisoned)?;
        let tip = match range {
            Some(r) => guard
                .range(r.block_range().clone())
                .next_back()
                .map(|(h, _)| *h),
            None => guard.keys().next_back().copied(),
        };
        Ok(tip)
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        // Trait allows short reads: return the longest contiguous prefix starting
        // at range.start. Empty / missing start => error.
        let guard = self.blocks.lock().map_err(|_| MemCacheError::Poisoned)?;
        let start = range.block_range().start;
        let end = range.block_range().end;
        if !guard.contains_key(&start) {
            return Err(MemCacheError::EmptyRange(start));
        }
        let mut out = Vec::new();
        let mut expected = start;
        for (height, block) in guard.range(start..end) {
            if *height != expected {
                break; // short read — contiguous prefix only
            }
            out.push(block.clone());
            expected = expected + 1;
        }
        Ok(out)
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut guard = self.blocks.lock().map_err(|_| MemCacheError::Poisoned)?;
        for block in compact_blocks {
            let height_u32: u32 = block
                .height
                .try_into()
                .map_err(|_| MemCacheError::HeightOverflow(block.height))?;
            guard.insert(BlockHeight::from_u32(height_u32), block);
        }
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        let mut guard = self.blocks.lock().map_err(|_| MemCacheError::Poisoned)?;
        let keys: Vec<_> = guard
            .range(range.block_range().clone())
            .map(|(h, _)| *h)
            .collect();
        for h in keys {
            guard.remove(&h);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum FsCacheError {
    #[error("block cache lock poisoned")]
    Poisoned,
    #[error("no cached blocks for requested range starting at {0}")]
    EmptyRange(BlockHeight),
    #[error("compact block height {0} does not fit in u32")]
    HeightOverflow(u64),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode {path}: {source}")]
    Decode {
        path: String,
        #[source]
        source: prost::DecodeError,
    },
    #[error("{0}")]
    Message(String),
}

/// Packed compact-block cache: `{start:010}-{end:010}.pack` (length-delimited).
/// Legacy `{height:010}.pb` files are still read on open.
pub struct FsBlockCache {
    dir: PathBuf,
    /// Inclusive spans `start -> end`, non-overlapping.
    packs: Mutex<BTreeMap<u32, u32>>,
    downloaded: Option<Arc<AtomicU32>>,
    scanned: Option<Arc<AtomicU32>>,
}

impl FsBlockCache {
    /// Delete cached compact-block files (`.pack` and legacy `.pb`). Wallet sqlite is not touched.
    pub fn wipe(dir: impl AsRef<Path>) -> Result<(), FsCacheError> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Ok(());
        }
        for ent in std::fs::read_dir(dir)? {
            let path = ent?.path();
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "pb" | "pack" | "tmp") {
                if let Err(e) = std::fs::remove_file(&path) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(())
    }

    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, FsCacheError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let mut packs = BTreeMap::new();
        for ent in std::fs::read_dir(&dir)? {
            let ent = ent?;
            let name = ent.file_name();
            let name = name.to_string_lossy();
            if let Some(stem) = name.strip_suffix(".pack") {
                if let Some((a, b)) = stem.split_once('-') {
                    if let (Ok(start), Ok(end)) = (a.parse::<u32>(), b.parse::<u32>()) {
                        if start <= end {
                            packs.insert(start, end);
                        }
                    }
                }
                continue;
            }
            if let Some(stem) = name.strip_suffix(".pb") {
                if let Ok(h) = stem.parse::<u32>() {
                    packs.entry(h).or_insert(h);
                }
            }
        }
        Ok(Self {
            dir,
            packs: Mutex::new(packs),
            downloaded: None,
            scanned: None,
        })
    }

    pub fn with_progress(mut self, downloaded: Arc<AtomicU32>) -> Self {
        if let Some(h) = self
            .packs
            .lock()
            .ok()
            .and_then(|g| g.values().next_back().copied())
        {
            downloaded.fetch_max(h, Ordering::Relaxed);
        }
        self.downloaded = Some(downloaded);
        self
    }

    pub fn with_scan_progress(mut self, scanned: Arc<AtomicU32>) -> Self {
        self.scanned = Some(scanned);
        self
    }

    fn mark_scanned(&self, height: BlockHeight) {
        if let Some(p) = &self.scanned {
            p.fetch_max(u32::from(height), Ordering::Relaxed);
        }
    }

    fn pack_path(&self, start: u32, end: u32) -> PathBuf {
        self.dir.join(format!("{start:010}-{end:010}.pack"))
    }

    fn legacy_path(&self, height: u32) -> PathBuf {
        self.dir.join(format!("{height:010}.pb"))
    }

    fn load_span(&self, start: u32, end: u32) -> Result<Vec<CompactBlock>, FsCacheError> {
        let pack = self.pack_path(start, end);
        if pack.exists() {
            let bytes = std::fs::read(&pack)?;
            return decode_delimited(&bytes).map_err(|e| FsCacheError::Message(e.to_string()));
        }
        if start == end {
            let legacy = self.legacy_path(start);
            if legacy.exists() {
                let bytes = std::fs::read(&legacy)?;
                let block = CompactBlock::decode(bytes.as_slice()).map_err(|source| {
                    FsCacheError::Decode {
                        path: legacy.display().to_string(),
                        source,
                    }
                })?;
                return Ok(vec![block]);
            }
        }
        Err(FsCacheError::EmptyRange(BlockHeight::from_u32(start)))
    }

    fn remove_span_files(&self, start: u32, end: u32) {
        let _ = std::fs::remove_file(self.pack_path(start, end));
        if start == end {
            let _ = std::fs::remove_file(self.legacy_path(start));
        }
    }

    fn overlapping(packs: &BTreeMap<u32, u32>, lo: u32, hi: u32) -> Vec<(u32, u32)> {
        packs
            .iter()
            .filter(|(&s, &e)| s <= hi && e >= lo)
            .map(|(&s, &e)| (s, e))
            .collect()
    }

    fn write_span(&self, blocks: &[CompactBlock]) -> Result<(u32, u32), FsCacheError> {
        let start = u32::try_from(blocks[0].height)
            .map_err(|_| FsCacheError::HeightOverflow(blocks[0].height))?;
        let end = u32::try_from(blocks[blocks.len() - 1].height)
            .map_err(|_| FsCacheError::HeightOverflow(blocks[blocks.len() - 1].height))?;
        let path = self.pack_path(start, end);
        let tmp = path.with_extension("pack.tmp");
        std::fs::write(&tmp, encode_delimited(blocks.iter().cloned()))?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&tmp, &path)?;
        Ok((start, end))
    }

    fn split_contiguous(mut blocks: Vec<CompactBlock>) -> Vec<Vec<CompactBlock>> {
        if blocks.is_empty() {
            return Vec::new();
        }
        blocks.sort_by_key(|b| b.height);
        let mut runs: Vec<Vec<CompactBlock>> = Vec::new();
        for block in blocks {
            match runs.last_mut() {
                Some(run) => {
                    let prev = run.last().map(|b: &CompactBlock| b.height).unwrap_or(0);
                    if block.height == prev.saturating_add(1) {
                        run.push(block);
                    } else {
                        runs.push(vec![block]);
                    }
                }
                None => runs.push(vec![block]),
            }
        }
        runs
    }
}

impl BlockSource for FsBlockCache {
    type Error = FsCacheError;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        mut with_block: F,
    ) -> Result<(), ChainError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<WalletErrT, Self::Error>>,
    {
        let start = u32::from(from_height.unwrap_or(BlockHeight::from(0)));
        let take = limit.unwrap_or(usize::MAX);
        let spans: Vec<(u32, u32)> = {
            let packs = self
                .packs
                .lock()
                .map_err(|_| ChainError::BlockSource(FsCacheError::Poisoned))?;
            packs
                .iter()
                .filter(|(_, &e)| e >= start)
                .map(|(&s, &e)| (s, e))
                .collect()
        };
        let mut remaining = take;
        let mut last = None;
        for (ps, pe) in spans {
            if remaining == 0 {
                break;
            }
            let blocks = self.load_span(ps, pe).map_err(ChainError::BlockSource)?;
            for block in blocks {
                if remaining == 0 {
                    break;
                }
                let h = u32::try_from(block.height).unwrap_or(0);
                if h < start {
                    continue;
                }
                with_block(block)?;
                last = Some(h);
                remaining -= 1;
            }
        }
        if let Some(h) = last {
            self.mark_scanned(BlockHeight::from_u32(h));
        }
        Ok(())
    }
}

#[async_trait]
impl BlockCache for FsBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let packs = self.packs.lock().map_err(|_| FsCacheError::Poisoned)?;
        let tip = match range {
            Some(r) => {
                let lo = u32::from(r.block_range().start);
                let hi = u32::from(r.block_range().end).saturating_sub(1);
                Self::overlapping(&packs, lo, hi)
                    .into_iter()
                    .map(|(_, e)| e.min(hi))
                    .max()
                    .map(BlockHeight::from_u32)
            }
            None => packs
                .values()
                .next_back()
                .copied()
                .map(BlockHeight::from_u32),
        };
        Ok(tip)
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        let start = u32::from(range.block_range().start);
        let end_excl = u32::from(range.block_range().end);
        let spans = {
            let packs = self.packs.lock().map_err(|_| FsCacheError::Poisoned)?;
            if Self::overlapping(&packs, start, start).is_empty() {
                return Err(FsCacheError::EmptyRange(range.block_range().start));
            }
            Self::overlapping(&packs, start, end_excl.saturating_sub(1))
        };
        let mut out = Vec::new();
        let mut expected = start;
        for (ps, pe) in spans {
            if ps > expected {
                break;
            }
            let blocks = self.load_span(ps, pe)?;
            for block in blocks {
                let h = u32::try_from(block.height)
                    .map_err(|_| FsCacheError::HeightOverflow(block.height))?;
                if h < expected {
                    continue;
                }
                if h >= end_excl {
                    break;
                }
                if h != expected {
                    return Ok(out);
                }
                out.push(block);
                expected = expected.saturating_add(1);
            }
        }
        if let Some(last) = out.last() {
            if let Ok(h) = u32::try_from(last.height) {
                self.mark_scanned(BlockHeight::from_u32(h));
            }
        }
        Ok(out)
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        if compact_blocks.is_empty() {
            return Ok(());
        }
        let runs = Self::split_contiguous(compact_blocks);
        let mut packs = self.packs.lock().map_err(|_| FsCacheError::Poisoned)?;
        for run in runs {
            let lo = u32::try_from(run[0].height)
                .map_err(|_| FsCacheError::HeightOverflow(run[0].height))?;
            let hi = u32::try_from(run[run.len() - 1].height)
                .map_err(|_| FsCacheError::HeightOverflow(run[run.len() - 1].height))?;
            let mut merged = run;
            for (s, e) in Self::overlapping(&packs, lo, hi) {
                let existing = self.load_span(s, e)?;
                self.remove_span_files(s, e);
                packs.remove(&s);
                merged.extend(existing.into_iter().filter(|b| {
                    u32::try_from(b.height)
                        .ok()
                        .is_some_and(|h| h < lo || h > hi)
                }));
            }
            for group in Self::split_contiguous(merged) {
                let (start, end) = self.write_span(&group)?;
                packs.insert(start, end);
                if let Some(p) = &self.downloaded {
                    p.fetch_max(end, Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        let lo = u32::from(range.block_range().start);
        let hi = u32::from(range.block_range().end).saturating_sub(1);
        if hi < lo {
            return Ok(());
        }
        let mut packs = self.packs.lock().map_err(|_| FsCacheError::Poisoned)?;
        let overlap = Self::overlapping(&packs, lo, hi);
        for (s, e) in overlap {
            packs.remove(&s);
            if s >= lo && e <= hi {
                self.remove_span_files(s, e);
                continue;
            }
            let blocks = self.load_span(s, e)?;
            self.remove_span_files(s, e);
            let keep: Vec<_> = blocks
                .into_iter()
                .filter(|b| {
                    u32::try_from(b.height)
                        .ok()
                        .is_some_and(|h| h < lo || h > hi)
                })
                .collect();
            for group in Self::split_contiguous(keep) {
                let (start, end) = self.write_span(&group)?;
                packs.insert(start, end);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_client_backend::data_api::scanning::ScanPriority;

    #[tokio::test]
    async fn short_read_returns_prefix() {
        let cache = MemBlockCache::new();
        let mut b0 = CompactBlock::default();
        b0.height = 10;
        let mut b1 = CompactBlock::default();
        b1.height = 11;
        cache.insert(vec![b0, b1]).await.unwrap();
        // Gap at 12 — request 10..14 should return only 10..12
        let range = ScanRange::from_parts(10.into()..14.into(), ScanPriority::Historic);
        let got = cache.read(&range).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].height, 10);
    }

    #[tokio::test]
    async fn missing_start_errors() {
        let cache = MemBlockCache::new();
        let range = ScanRange::from_parts(5.into()..8.into(), ScanPriority::Historic);
        assert!(cache.read(&range).await.is_err());
    }

    #[tokio::test]
    async fn fs_cache_roundtrip_and_gap() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsBlockCache::open(dir.path()).unwrap();
        let mut b0 = CompactBlock::default();
        b0.height = 20;
        let mut b1 = CompactBlock::default();
        b1.height = 21;
        cache.insert(vec![b0, b1]).await.unwrap();
        let packs = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("pack"))
            .count();
        assert_eq!(packs, 1, "contiguous insert writes one range pack");
        assert_eq!(cache.get_tip_height(None).unwrap(), Some(21.into()));
        let range = ScanRange::from_parts(20.into()..24.into(), ScanPriority::Historic);
        let got = cache.read(&range).await.unwrap();
        assert_eq!(got.len(), 2);
        cache.delete(range.clone()).await.unwrap();
        assert!(cache.read(&range).await.is_err());
        // Re-open from disk after a fresh insert.
        let mut b2 = CompactBlock::default();
        b2.height = 30;
        cache.insert(vec![b2]).await.unwrap();
        let reopened = FsBlockCache::open(dir.path()).unwrap();
        assert_eq!(reopened.get_tip_height(None).unwrap(), Some(30.into()));
        FsBlockCache::wipe(dir.path()).unwrap();
        let empty = FsBlockCache::open(dir.path()).unwrap();
        assert_eq!(empty.get_tip_height(None).unwrap(), None);
    }

    #[tokio::test]
    async fn fs_cache_marks_scan_progress_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let scanned = Arc::new(AtomicU32::new(0));
        let cache = FsBlockCache::open(dir.path())
            .unwrap()
            .with_scan_progress(Arc::clone(&scanned));
        let mut b0 = CompactBlock::default();
        b0.height = 40;
        let mut b1 = CompactBlock::default();
        b1.height = 41;
        cache.insert(vec![b0, b1]).await.unwrap();
        let range = ScanRange::from_parts(40.into()..42.into(), ScanPriority::Historic);
        let _ = cache.read(&range).await.unwrap();
        assert_eq!(scanned.load(Ordering::Relaxed), 41);
    }
}
