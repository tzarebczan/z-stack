//! Opaque bindings for protocol qualification. Wallet mutation is not exposed here.
use super::{ChainAnchor, Query, Session};
use wasm_bindgen::prelude::*;
fn err(code: &'static str) -> JsValue {
    JsValue::from_str(code)
}
#[wasm_bindgen]
pub struct EnhanceV7Session(Session);
#[wasm_bindgen]
impl EnhanceV7Session {
    pub fn open(
        manifest: &[u8],
        material: &[u8],
        height: u64,
        hash: &str,
        records: u64,
        time: u64,
    ) -> Result<Self, JsValue> {
        let hash = hex::decode(hash)
            .map_err(|_| err("enhance_anchor_mismatch"))?
            .try_into()
            .map_err(|_| err("enhance_anchor_mismatch"))?;
        Ok(Self(
            Session::open(
                manifest,
                material,
                &ChainAnchor {
                    height,
                    hash,
                    ironwood_tree_size: records,
                },
                time,
            )
            .map_err(err)?,
        ))
    }
    pub fn prepare(&self, position: u64, time: u64) -> Result<EnhanceV7Query, JsValue> {
        Ok(EnhanceV7Query(self.0.prepare(position, time).map_err(err)?))
    }
}
#[wasm_bindgen]
pub struct EnhanceV7Query(Query);
#[wasm_bindgen]
impl EnhanceV7Query {
    pub fn body(&self) -> Result<Vec<u8>, JsValue> {
        self.0.body().map_err(err)
    }
    pub fn finish(&mut self, bytes: &[u8], time: u64) -> Result<Vec<u8>, JsValue> {
        Ok(self.0.finish(bytes, time).map_err(err)?.as_bytes().to_vec())
    }
    pub fn cancel(&mut self) {
        self.0.cancel();
    }
}

#[wasm_bindgen]
pub fn thread_count() -> usize {
    rayon::current_num_threads()
}
