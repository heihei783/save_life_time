# syntax=docker/dockerfile:1
FROM rust:1-bookworm AS builder
WORKDIR /src
COPY rust/Cargo.toml rust/Cargo.lock ./rust/
COPY rust/src ./rust/src
COPY web ./web
WORKDIR /src/rust
RUN cargo build --release --bin winsignin-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --create-home --uid 10001 app
WORKDIR /app
COPY --from=builder /src/rust/target/release/winsignin-server /app/winsignin-server
RUN mkdir /data && chown -R app:app /app /data
USER app
ENV BIND=0.0.0.0:1112 CAS_PROXY_BIND=0.0.0.0:1113 DATA_DIR=/data
EXPOSE 1112 1113
VOLUME ["/data"]
CMD ["/app/winsignin-server"]
