// Reproducible standalone protocol qualification packages; production uses z-wasm.
import { spawnSync } from 'node:child_process';
import { resolve } from 'node:path';
const mode=process.argv[2];
if(!['st','mt'].includes(mode))throw new Error('expected st or mt');
const root=resolve(import.meta.dirname,'..');
const output=resolve(root,process.argv[3]??'test-output/enhance-v7-final-'+mode);
const flags=mode==='mt'?[
 '--cfg','getrandom_backend="wasm_js"','-C','target-feature=+atomics,+bulk-memory,+mutable-globals,+simd128',
 '-C','no-vectorize-slp','-C','link-arg=--shared-memory','-C','link-arg=--max-memory=4294967296',
 '-C','link-arg=--import-memory',...['__wasm_init_tls','__tls_size','__tls_align','__tls_base'].flatMap(name=>['-C','link-arg=--export='+name]),
]:['-C','target-feature=+bulk-memory,+mutable-globals,+simd128','-C','no-vectorize-slp'];
const toolchain=mode==='mt'?'nightly-2026-09-22':'1.91.0';
const target=process.env.CARGO_TARGET_DIR??resolve(root,'target','enhance-v7-'+mode);
const result=spawnSync('wasm-pack',['build','crates/z-enhance-pir','--release','--target','web','--out-dir',output,'--','--features',mode==='mt'?'protocol-bindings,multicore':'protocol-bindings',...(mode==='mt'?['-Z','build-std=std,panic_abort']:[])],{
 cwd:root,stdio:'inherit',env:{...process.env,RUSTUP_TOOLCHAIN:toolchain,CARGO_BUILD_JOBS:'1',CARGO_TARGET_DIR:target,RUSTFLAGS:flags.join(' ')},
});
process.exitCode=result.status??1;
