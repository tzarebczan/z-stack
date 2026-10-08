# This unencrypted Linux amd64 image is a local regtest fixture.
FROM docker.io/library/rust:1.98.0-bookworm@sha256:4e4a7e7939c17991ab35f2b8c2e67593980f771d28f6b1254b1850f860fd0c7f AS build
SHELL ["/bin/bash", "-euo", "pipefail", "-c"]
RUN apt-get update && apt-get install -y --no-install-recommends \
    clang cmake pkg-config protobuf-compiler python3 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git init . && git remote add origin https://github.com/zingolabs/zaino.git && \
    git fetch --depth=1 origin 3244a74bb09fa6a09a4b2deeb6be53bab0890747 && \
    git checkout --detach FETCH_HEAD && \
    test "$(git rev-parse HEAD)" = 3244a74bb09fa6a09a4b2deeb6be53bab0890747
COPY zaino-0.10.1-zakura.patch /patches/zaino.patch
RUN git apply --check /patches/zaino.patch && git apply /patches/zaino.patch && \
    echo 'ef7a70409530bd0e74a4d901439c2ad2d6ca5d4dce554dc97dac593ebb5b813b  Cargo.lock' | sha256sum --check
ENV RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 CARGO_PROFILE_RELEASE_DEBUG=0
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p zainod --features no_tls_use_unencrypted_traffic && \
    mkdir -p /out && cp target/release/zainod /out/zainod
COPY license-inventory.py /license-inventory.py
COPY licenses /supplemental
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    python3 /license-inventory.py
RUN cp Cargo.lock /out/Cargo.lock && cp /patches/zaino.patch /out/zaino.patch && \
    cd /out && sha256sum zainod Cargo.lock zaino.patch THIRD_PARTY_LICENSES.txt dependencies.json > SHA256SUMS

FROM docker.io/library/debian:bookworm-slim@sha256:a4672c0cb26fbdde88e38fa2dfb6c681942306680e41e4378b28770b6e79ee91
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 libstdc++6 && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --gid 1000 zaino && useradd --uid 1000 --gid 1000 --create-home zaino && \
    mkdir -p /app/data && chown 1000:1000 /app/data
COPY --from=build /out/ /usr/share/z-stack-zaino/
RUN ln -s /usr/share/z-stack-zaino/zainod /usr/local/bin/zainod
COPY ZAINO-LICENSE /usr/share/z-stack-zaino/ZAINO-LICENSE
ARG SOURCE_REVISION=local
ARG IMAGE_VERSION=local
LABEL org.opencontainers.image.source="https://github.com/tzarebczan/z-stack" \
    org.opencontainers.image.description="Patched NU7 Zaino for isolated local regtest; unencrypted traffic; Linux amd64" \
    org.opencontainers.image.licenses="Apache-2.0" \
    org.opencontainers.image.revision="${SOURCE_REVISION}" \
    org.opencontainers.image.version="${IMAGE_VERSION}" \
    io.z-stack.zaino.upstream="3244a74bb09fa6a09a4b2deeb6be53bab0890747"
USER 1000:1000
WORKDIR /app/data
ENTRYPOINT ["/usr/local/bin/zainod"]
CMD ["start", "--config", "/app/config/zaino.toml"]
