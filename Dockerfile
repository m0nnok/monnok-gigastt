# Multi-stage build for gigastt
# Build: docker build -t gigastt .
# Run:   docker run -p 9876:9876 gigastt

# --- Builder stage ---
FROM rust:1.88-bookworm AS builder

ARG ORT_VERSION=1.24.2
ARG ORT_ARCHIVE_URL=

# `prost-build` (via build.rs) requires `protoc` at compile time; without it
# the build aborts with "prost-build failed to compile proto/onnx.proto".
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Avoid ort-sys' CDN downloader during Docker builds. The official Microsoft
# ONNX Runtime release archive supplies the dynamic library that ort links to.
# GitHub Releases can be flaky from some servers, so fail stalled connections
# quickly and retry. ORT_ARCHIVE_URL allows using an internal mirror if needed.
RUN archive_url="${ORT_ARCHIVE_URL:-https://github.com/microsoft/onnxruntime/releases/download/v${ORT_VERSION}/onnxruntime-linux-x64-${ORT_VERSION}.tgz}" && \
    curl -4 -fsSL \
      --connect-timeout 30 \
      --max-time 600 \
      --retry 8 \
      --retry-delay 5 \
      --retry-max-time 1800 \
      --retry-all-errors \
      --speed-limit 1024 \
      --speed-time 60 \
      "$archive_url" \
      -o /tmp/onnxruntime.tgz && \
    mkdir -p /opt/onnxruntime && \
    tar -xzf /tmp/onnxruntime.tgz -C /opt/onnxruntime --strip-components=1 && \
    rm /tmp/onnxruntime.tgz

ENV ORT_LIB_LOCATION=/opt/onnxruntime/lib
ENV ORT_PREFER_DYNAMIC_LINK=1

# Dependency-compilation cache: copy manifests + build.rs + proto/ first and
# compile a dummy binary so `cargo build` downloads + builds every transitive
# crate. Subsequent edits to src/ only invalidate the final compilation
# layer, cutting incremental rebuild time from minutes to seconds.
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto/ proto/
COPY docs/openapi.yaml docs/openapi.yaml
RUN mkdir -p src tests && \
    echo 'fn main() {}' > src/main.rs && \
    touch src/lib.rs && \
    touch tests/benchmark.rs && \
    cargo build --release && \
    cargo clean -p gigastt --release && \
    rm -rf src

# Now bring in the actual source and build the real binary.
COPY src/ src/

RUN cargo build --release && \
    strip target/release/gigastt

# --- Model bake stage (runs only when GIGASTT_BAKE_MODEL=1) ---
FROM debian:bookworm-slim AS model-fetcher

ARG GIGASTT_BAKE_MODEL=0

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/gigastt /usr/local/bin/gigastt
COPY --from=builder /opt/onnxruntime/lib/libonnxruntime.so* /usr/local/lib/
RUN ldconfig

RUN mkdir -p /models && \
    if [ "$GIGASTT_BAKE_MODEL" = "1" ]; then \
        gigastt download --model-dir /models; \
    fi

# --- Runtime stage ---
FROM debian:bookworm-slim

ARG GIGASTT_BAKE_MODEL=0

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/gigastt /usr/local/bin/gigastt
COPY --from=builder /opt/onnxruntime/lib/libonnxruntime.so* /usr/local/lib/
RUN ldconfig

RUN groupadd -r gigastt && useradd -r -g gigastt gigastt && \
    mkdir -p /home/gigastt/.gigastt/models && chown -R gigastt:gigastt /home/gigastt

# Copy baked model files (only present when GIGASTT_BAKE_MODEL=1)
COPY --from=model-fetcher --chown=gigastt:gigastt /models/. /home/gigastt/.gigastt/models/

USER gigastt

ENV RUST_LOG=gigastt=info
ENV LD_LIBRARY_PATH=/usr/local/lib

EXPOSE 9876

HEALTHCHECK --interval=30s --timeout=5s --retries=3 \
    CMD curl -f http://localhost:9876/health || exit 1

# Download model if not present, then start server.
# `--bind-all` acknowledges that container networking requires listening on
# 0.0.0.0; outside Docker the default `127.0.0.1` bind stays in effect.
ENTRYPOINT ["gigastt"]
CMD ["serve", "--port", "9876", "--host", "0.0.0.0", "--bind-all"]
