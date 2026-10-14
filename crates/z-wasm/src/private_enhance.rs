//! One opaque enhancement pass belongs to its wallet; no positions or note data cross the host API.
use crate::{js_err, WasmWallet};
use wasm_bindgen::prelude::*;
use z_engine::{enhancement::EnhanceBatch, Network};
fn now(time: f64) -> Result<u64, JsValue> {
    if !time.is_finite() || time < 0.0 || time.fract() != 0.0 || time > 9_007_199_254_740_991.0 {
        return Err(js_err("enhance_time_invalid"));
    }
    Ok(time as u64)
}
#[wasm_bindgen]
impl WasmWallet {
    /// Closed operations: begin/material/prepare/finish/anchor/cancel. All payloads are public metadata or PIR ciphertext.
    #[wasm_bindgen(js_name = privateEnhance)]
    pub fn private_enhance(
        &mut self,
        operation: &str,
        bytes: &[u8],
        time: f64,
    ) -> Result<Vec<u8>, JsValue> {
        if operation == "cancel" {
            if let Some(mut batch) = self.private_enhance.take() {
                batch.cancel();
            }
            return Ok(vec![]);
        }
        if operation == "finish" && now(time).is_err() {
            self.private_enhance.take();
        }
        let now = now(time)?;
        if self.inner.network() != Network::Mainnet {
            return Err(js_err("enhance_network_unsupported"));
        }
        if operation == "pending" && bytes.is_empty() {
            return Ok(vec![u8::from(
                !self
                    .inner
                    .private_enhance_work()
                    .map_err(js_err)?
                    .is_empty(),
            )]);
        }
        if operation == "begin" {
            self.private_enhance.take();
            let anchor = z_enhance_pir::manifest_anchor(bytes).map_err(js_err)?;
            self.inner
                .private_enhance_anchor(
                    anchor.height as u32,
                    &hex_hash(anchor.hash),
                    anchor.ironwood_tree_size,
                )
                .map_err(js_err)?;
            let work = self
                .inner
                .private_enhance_work()
                .map_err(js_err)?
                .into_iter()
                .map(|r| {
                    let p = r.position();
                    (r, p)
                });
            let batch = EnhanceBatch::open(bytes, &anchor, work, now).map_err(js_err)?;
            let result = serde_json::to_vec(&batch.session_ids()).map_err(js_err)?;
            self.private_enhance = Some(batch);
            return Ok(result);
        }
        let batch = self
            .private_enhance
            .as_mut()
            .ok_or_else(|| js_err("enhance_context_consumed"))?;
        let anchor = batch.anchor();
        // A malformed, stale, or late response consumes its cryptographic context before any apply.
        if operation == "finish" {
            let (request, record) = batch.finish(bytes, now).map_err(js_err)?;
            self.inner
                .private_enhance_anchor(
                    anchor.height as u32,
                    &hex_hash(anchor.hash),
                    anchor.ironwood_tree_size,
                )
                .map_err(js_err)?;
            let result = self
                .inner
                .apply_private_enhance_record(request, &record)
                .map_err(js_err)?;
            return Ok(format!("{result:?}").into_bytes());
        }
        self.inner
            .private_enhance_anchor(
                anchor.height as u32,
                &hex_hash(anchor.hash),
                anchor.ironwood_tree_size,
            )
            .map_err(js_err)?;
        match operation {
            "material" => {
                batch.add_session(bytes, now).map_err(js_err)?;
                Ok(vec![])
            }
            "prepare" if bytes.is_empty() => batch.prepare(now).map_err(js_err),
            "anchor" if bytes.is_empty() => serde_json::to_vec(
                &serde_json::json!({"height":anchor.height,"hash":hex_hash(anchor.hash)}),
            )
            .map_err(js_err),
            _ => Err(js_err("enhance_operation_invalid")),
        }
    }
}
fn hex_hash(hash: [u8; 32]) -> String {
    z_engine::web::to_hex(&hash)
}
