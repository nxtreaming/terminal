# ── build stage ──────────────────────────────────────────────────────────────
FROM rust:slim AS builder

WORKDIR /app

# Install OpenSSL dev headers needed by sqlx/rustls
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cache dependency compilation separately from source
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main(){}' > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Now copy the real source and build
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ── runtime stage ─────────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/feedback-ingest /app/feedback-ingest

EXPOSE 8080

CMD ["/app/feedback-ingest"]
