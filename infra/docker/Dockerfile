# MatrixMedia -- Multi-stage Rust build
#
# Build:  docker build -f infra/docker/Dockerfile -t matrixmedia:local ../..
# Run:    docker run --rm -p 6167:6167 matrixmedia:local serve

# ---------------------------------------------------------------------------
# Stage 1: Build the Rust binary
# ---------------------------------------------------------------------------
FROM rust:1-slim-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy ONLY the inputs of the Rust build. With `COPY . .` this layer — and so the
# release compile after it — depended on every file in the context, and a PR that
# touched nothing but .github/workflows/ paid for a full rebuild (11–22 min on the
# shared buildkitd, against ~20 s cached). Dockerfile.fakestripe opens with the same
# COPYs, so the two images share these layers.
# mm-core/data/ is here because crates/mm-api include_str!s reserved_usernames.json.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY mm-core/data ./mm-core/data

# `s3` feature flag gates the aws-sdk-s3 dependency in mm-core.
# Without it MM_STORAGE_BACKEND=s3 errors at startup with
# "S3 storage backend requested but not compiled in". It is spelled
# `mm-server/s3` because cargo rejects a bare `--features` across several `-p`.
# Build only mm-server, mm-fleet-runner and mm-media-worker (the fleet runner and the ad
# media worker ship in this same image and run as their own compose services; the worker's
# image is this file's `media-worker` target, which adds ffmpeg on top of the runtime) to keep
# the image lean.
#
# Cache mounts live on the buildkitd's disk, never in the image:
#   - cargo registry + git: downloaded crates. cargo locks these itself, so the
#     default `shared` mode is safe and mm-core/mm-fakestripe can run side by side.
#   - target/: compiled dependencies survive between builds, so a Rust change
#     recompiles the workspace crates instead of every dependency.
# The target cache needs two guards, because cargo judges a workspace crate fresh by
# MTIME (sources older than the last build => reuse it), not by content:
#   1. `touch` every source, so a checkout whose files are older than the cached
#      artifacts (a run that started first but reached this step second) recompiles
#      instead of shipping the other run's binary;
#   2. `sharing=locked`, so no other build can write newer artifacts between our
#      touch and our cargo run.
# The ids are namespaced because the buildkitd is shared with other projects, and a
# mount without an id is keyed by its target path alone.
RUN --mount=type=cache,id=matrixmedia-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=matrixmedia-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=matrixmedia-target-mm-core,target=/build/target,sharing=locked \
    find crates mm-core -type f -exec touch {} + && \
    cargo build --release -p mm-server -p mm-fleet-runner -p mm-media-worker --features mm-server/s3 && \
    cp target/release/mm-server /usr/local/bin/mm-server && \
    cp target/release/mm-fleet-runner /usr/local/bin/mm-fleet-runner && \
    cp target/release/mm-media-worker /usr/local/bin/mm-media-worker

# ---------------------------------------------------------------------------
# Stage 2: Minimal runtime image (shared by the mm-core and media-worker targets)
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/local/bin/mm-server /usr/local/bin/matrixmedia
COPY --from=builder /usr/local/bin/mm-fleet-runner /usr/local/bin/mm-fleet-runner
COPY --from=builder /usr/local/bin/mm-media-worker /usr/local/bin/mm-media-worker

# The ids are pinned (they are what Debian bookworm-slim hands out today) because
# deploy/install.sh chowns the ad-media directories to 100:101 on the host. One source of truth.
RUN addgroup --system --gid 101 matrixmedia && \
    adduser --system --uid 100 --ingroup matrixmedia --no-create-home matrixmedia && \
    mkdir -p /data /etc/matrixmedia /var/lib/mm-fleet && \
    chown matrixmedia:matrixmedia /data /etc/matrixmedia /var/lib/mm-fleet

# The git commit this image is built from: /_mm/admin/v1/health reports it and the
# dashboard sidebar shows it. It is declared in this runtime stage, not the builder, so a
# new commit changes only this metadata layer and the cached compile is reused.
#   docker build --build-arg MM_BUILD_COMMIT="$(git rev-parse HEAD)" ...
ARG MM_BUILD_COMMIT=
ENV MM_BUILD_COMMIT=${MM_BUILD_COMMIT}

USER matrixmedia

VOLUME ["/data", "/etc/matrixmedia"]

EXPOSE 6167 6168 9090

ENTRYPOINT ["matrixmedia"]
CMD ["serve"]

# ---------------------------------------------------------------------------
# Stage 3: mm-media-worker (internal spec 2026-10-11 §5.2), the runtime plus ffmpeg.
# Same base, same `matrixmedia` user (uid 100, gid 101) and the same binaries as mm-core, so
# the Rust build is shared and never runs twice:
#   docker build -f infra/docker/Dockerfile --target media-worker \
#     --build-arg MM_BUILD_COMMIT="$(git rev-parse HEAD)" -t <registry>/matrixmedia-mm-media-worker:<tag> .
# ---------------------------------------------------------------------------
FROM runtime AS media-worker
USER root
RUN apt-get update && \
    apt-get install -y --no-install-recommends ffmpeg && \
    rm -rf /var/lib/apt/lists/*
USER matrixmedia
ENTRYPOINT ["mm-media-worker"]
CMD ["run"]

# ---------------------------------------------------------------------------
# mm-core: the runtime unchanged. It must stay the LAST stage, because a plain
# `docker build -f infra/docker/Dockerfile .` (no --target) builds the last stage.
# ---------------------------------------------------------------------------
FROM runtime AS mm-core
