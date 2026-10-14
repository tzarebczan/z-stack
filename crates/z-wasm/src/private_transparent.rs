//! The worker owns the live recovery future and private PIR query contexts.
use crate::{js_err, WasmWallet};
use wasm_bindgen::prelude::*;
use z_transparent_pir_v2::{Anchor, Checkpoint, Limits, Pass, Reply, Scope, Step};

pub(crate) struct PrivateTransparentPass {
    pass: Pass,
    staged: Option<String>,
}
#[wasm_bindgen]
impl WasmWallet {
    /// Closed worker operations. Only public selectors and ciphertext cross the host boundary.
    #[wasm_bindgen(js_name = privateTransparent)]
    pub fn private_transparent(
        &mut self,
        operation: &str,
        bytes: &[u8],
    ) -> Result<Vec<u8>, JsValue> {
        if operation == "cancel" {
            if let Some(mut state) = self.private_transparent.take() {
                state.pass.cancel();
            }
            return Ok(vec![]);
        }
        if operation == "status" && bytes.is_empty() {
            return serde_json::to_vec(&self.inner.transparent_pir_status()).map_err(js_err);
        }
        if operation == "begin" {
            self.private_transparent.take();
            #[derive(serde::Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Begin {
                source_id: String,
            }
            let input: Begin = serde_json::from_slice(bytes)
                .map_err(|_| js_err("transparent_pir_input_invalid"))?;
            let scope_json = self
                .inner
                .begin_transparent_pir(&input.source_id)
                .map_err(js_err)?;
            let scope: Scope = serde_json::from_str(&scope_json).map_err(js_err)?;
            let checkpoint = self
                .inner
                .transparent_pir_checkpoint()
                .map(|value| serde_json::from_str::<Checkpoint>(&value))
                .transpose()
                .map_err(js_err)?;
            let anchor = scope.anchor.clone();
            let pass = Pass::new(scope, checkpoint, Limits::default()).map_err(js_err)?;
            let result = serde_json::to_vec(&anchor).map_err(js_err)?;
            self.private_transparent = Some(PrivateTransparentPass { pass, staged: None });
            return Ok(result);
        }
        let state = self
            .private_transparent
            .as_mut()
            .ok_or_else(|| js_err("transparent_pir_context_consumed"))?;
        // Recheck the wallet's source/scope and accepted anchor on each state mutation in z-engine.
        match operation {
            "advance" if bytes.is_empty() => match state.pass.advance().map_err(js_err)? {
                Step::Checkpoint { checkpoint } => {
                    let id = checkpoint.checkpoint_id.clone();
                    self.inner
                        .apply_transparent_pir_checkpoint(
                            &serde_json::to_string(&checkpoint).map_err(js_err)?,
                        )
                        .map_err(js_err)?;
                    state.staged = Some(id.clone());
                    serde_json::to_vec(&serde_json::json!({"kind":"checkpoint","checkpoint_id":id}))
                        .map_err(js_err)
                }
                step => serde_json::to_vec(&step).map_err(js_err),
            },
            "accept" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Accept {
                    request_id: u64,
                    reply: Reply,
                }
                let input: Accept = serde_json::from_slice(bytes)
                    .map_err(|_| js_err("transparent_pir_input_invalid"))?;
                state
                    .pass
                    .accept(input.request_id, input.reply)
                    .map_err(js_err)?;
                Ok(vec![])
            }
            "chain" => {
                let point: Anchor = serde_json::from_slice(bytes)
                    .map_err(|_| js_err("transparent_pir_input_invalid"))?;
                state
                    .pass
                    .accept_chain(point.height, point.hash)
                    .map_err(js_err)?;
                Ok(vec![])
            }
            "ack" => {
                let id = std::str::from_utf8(bytes)
                    .map_err(|_| js_err("transparent_pir_input_invalid"))?;
                if state.staged.as_deref() != Some(id) {
                    return Err(js_err("transparent_pir_checkpoint_mismatch"));
                }
                self.inner
                    .acknowledge_transparent_pir_checkpoint(id)
                    .map_err(js_err)?;
                state.pass.acknowledge(id).map_err(js_err)?;
                state.staged = None;
                Ok(vec![])
            }
            _ => Err(js_err("transparent_pir_operation_invalid")),
        }
    }
}
