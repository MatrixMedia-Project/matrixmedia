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
COPY . .

RUN cargo build --release

# ---------------------------------------------------------------------------
# Stage 2: Minimal runtime image
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/mm-server /usr/local/bin/matrixmedia

RUN adduser --system --no-create-home --group matrixmedia && \
    mkdir -p /data /etc/matrixmedia && \
    chown matrixmedia:matrixmedia /data /etc/matrixmedia

USER matrixmedia

VOLUME ["/data", "/etc/matrixmedia"]

EXPOSE 6167 6168 9090

ENTRYPOINT ["matrixmedia"]
CMD ["serve"]
