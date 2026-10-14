//! A bounded Rust-owned enhancement pass. Hosts see only public identifiers and ciphertext.
use std::collections::{BTreeMap, VecDeque};
use z_enhance_pir::{ChainAnchor, ManifestPlan, Query, Session};

pub const MAX_ENHANCE_QUERIES: usize = 24;

pub struct EnhanceBatch<Request> {
    manifest: ManifestPlan,
    work: VecDeque<(Request, u64, String)>,
    sessions: BTreeMap<String, Session>,
    active: Option<(Request, Query)>,
    failed: bool,
}
impl<Request> EnhanceBatch<Request> {
    pub fn open(
        manifest: &[u8],
        anchor: &ChainAnchor,
        work: impl IntoIterator<Item = (Request, u64)>,
        now: u64,
    ) -> Result<Self, &'static str> {
        let manifest = ManifestPlan::open(manifest, anchor, now)?;
        let mut pending = VecDeque::new();
        for (request, position) in work
            .into_iter()
            .filter(|(_, p)| *p < anchor.ironwood_tree_size)
            .take(MAX_ENHANCE_QUERIES)
        {
            let id = manifest.session_id(position)?;
            pending.push_back((request, position, id));
        }
        Ok(Self {
            manifest,
            work: pending,
            sessions: BTreeMap::new(),
            active: None,
            failed: false,
        })
    }
    pub fn anchor(&self) -> ChainAnchor {
        self.manifest.anchor()
    }
    pub fn session_ids(&self) -> Vec<String> {
        self.work
            .iter()
            .map(|(_, _, id)| id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    pub fn add_session(&mut self, bytes: &[u8], now: u64) -> Result<(), &'static str> {
        if self.failed || self.active.is_some() {
            return Err("enhance_context_consumed");
        }
        let session = self.manifest.session(bytes, now)?;
        let id = session.session_id();
        if !self.work.iter().any(|(_, _, needed)| *needed == id) || self.sessions.contains_key(&id)
        {
            return Err("enhance_material_unrequested");
        }
        self.sessions.insert(id, session);
        Ok(())
    }
    pub fn prepare(&mut self, now: u64) -> Result<Vec<u8>, &'static str> {
        if self.failed || self.active.is_some() {
            return Err("enhance_context_consumed");
        }
        let Some((_, position, id)) = self.work.front() else {
            return Ok(vec![]);
        };
        let session = self.sessions.get(id).ok_or("enhance_material_missing")?;
        let query = session.prepare(*position, now)?;
        let body = query.body()?;
        let (request, _, _) = self.work.pop_front().expect("pending request");
        self.active = Some((request, query));
        Ok(body)
    }
    pub fn finish(&mut self, bytes: &[u8], now: u64) -> Result<(Request, [u8; 653]), &'static str> {
        let (request, mut query) = self.active.take().ok_or("enhance_context_consumed")?;
        self.failed = true;
        let record = query.finish(bytes, now)?;
        self.failed = false;
        Ok((request, *record.as_bytes()))
    }
    pub fn cancel(&mut self) {
        self.active.take();
        self.work.clear();
        self.sessions.clear();
        self.failed = true;
    }
}
