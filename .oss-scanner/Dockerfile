# OSS Scanner build for z-stack. Built with the repo as context; network is cut after this build,
# so everything tests need is fetched here.
FROM rust:1.91-bookworm
ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_TERM_COLOR=never \
    RUSTFLAGS="-C force-frame-pointers=yes" \
    CARGO_PROFILE_DEV_DEBUG=true
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates clang pkg-config git python3 xz-utils curl libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Node 22 + pnpm (matches CI and packageManager in package.json)
ARG NODE_VERSION=22.23.3
RUN curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
      | tar -xJ -C /usr/local --strip-components=1 \
    && corepack enable

COPY . /src
WORKDIR /src
RUN rustup show active-toolchain || rustup toolchain install 1.91 --profile minimal --target wasm32-unknown-unknown

# Rust: engine (native + PIR features), transparent PIR, enhance PIR, wasm bindings.
# z-desktop (GPUI app) is skipped; it is a UI shell over z-engine.
RUN cargo fetch --locked
RUN cargo build --locked -p z-engine --features native,cli,native-pir,private-enhance,transparent-pir --all-targets \
    && cargo build --locked -p z-transparent-pir-v2 -p z-enhance-pir --all-targets \
    && cargo check --locked -p z-wasm --target wasm32-unknown-unknown

# Both production WASM variants: single-threaded (stable) and multicore (nightly + rust-src, atomics).
ARG WASM_PACK_VERSION=0.15.0
ARG WASM_MT_TOOLCHAIN=nightly-2026-09-22
RUN curl -fsSL "https://github.com/wasm-bindgen/wasm-pack/releases/download/v${WASM_PACK_VERSION}/wasm-pack-v${WASM_PACK_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 "wasm-pack-v${WASM_PACK_VERSION}-x86_64-unknown-linux-musl/wasm-pack" \
    && rustup toolchain install "${WASM_MT_TOOLCHAIN}" --profile minimal --component rust-src --target wasm32-unknown-unknown

# TypeScript packages (SDK, core, passkey, base, gift-cards), then build both WASM engines.
RUN corepack prepare --activate && pnpm install --frozen-lockfile
RUN pnpm build:wasm && pnpm build:wasm:mt

# Cheap tests so we know the image works; failures don't abort the build.
RUN (cargo test --locked -p z-engine --features native,cli,native-pir,private-enhance,transparent-pir --lib \
      || echo "WARNING: z-engine tests reported failures") \
    && (cargo test --locked -p z-transparent-pir-v2 --lib -- --test-threads=1 \
      || echo "WARNING: z-transparent-pir-v2 tests reported failures") \
    && (cargo test --locked -p z-enhance-pir --lib || echo "WARNING: z-enhance-pir tests reported failures") \
    && (node --conditions=@z-stack/source --import tsx --test packages/core/test/core.test.ts packages/passkey/test/vault.test.ts packages/sdk/test/grpc-web.test.ts packages/sdk/test/integrity.test.ts \
      || echo "WARNING: TS tests reported failures")
