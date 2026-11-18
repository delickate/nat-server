# --- build ---------------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /app

# Cache dependencies separately from the source.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/nat_server* target/release/nat-server*

COPY src ./src
RUN cargo build --release --locked

# --- runtime -------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --shell /usr/sbin/nologin natserver

COPY --from=build /app/target/release/nat-server /usr/local/bin/nat-server

USER natserver
ENV BIND_ADDR=0.0.0.0:8080 \
    RUST_LOG=nat_server=info
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

CMD ["nat-server"]
