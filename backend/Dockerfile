# syntax=docker/dockerfile:1
#
# Build context for this Dockerfile MUST be the repository root (not backend/),
# because backend/Cargo.toml uses `[lints] workspace = true`, which requires the
# workspace root Cargo.toml (and the other workspace members' manifests) to be
# present during resolution. docker-compose.yml / docker-compose.override.yml
# are configured accordingly (context: ., dockerfile: backend/Dockerfile).
#
# Runtime environment variables (see .env.example):
#   DATABASE_URL, ALLOWED_ORIGINS, MIN_CONTRACT_VERSION, DB_POOL_MIN, DB_POOL_MAX,
#   DB_POOL_TIMEOUT_SECS, NOTIFICATION_SCHEDULER_INTERVAL_SECS,
#   FCM_SERVER_KEY, FCM_PROJECT_ID, REMINDER_EMAIL_API_KEY, REMINDER_SMS_API_KEY,
#   REMINDER_ENCRYPTION_SECRET
# Additional vars read by the backend but not yet documented in .env.example:
#   NODE_ID, CONSENSUS_STRATEGY, REDIS_URL, ADMIN_API_KEY
# NOTE: the backend connects to Postgres via DATABASE_URL (backend/src/main.rs,
# backend/src/db.rs). DATABASE_URL is required at startup -- the process exits
# immediately if it is unset, rather than silently falling back to an
# ephemeral database. docker-compose.yml's `db` service supplies it, and
# reminder preferences / subscriptions / audit trails persist across backend
# container restarts as long as the `postgres_data` volume persists.

ARG RUST_VERSION=1.96.1

# ── Base: shared toolchain + system deps for building ───────────────────────
FROM rust:${RUST_VERSION}-slim-bookworm AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
        gcc \
        pkg-config \
      && rm -rf /var/lib/apt/lists/*
WORKDIR /app

# ── Development: live-reload against the bind-mounted backend/src ───────────
FROM base AS development
RUN cargo install cargo-watch --locked
COPY Cargo.toml Cargo.lock ./
COPY contracts ./contracts
COPY backend ./backend
EXPOSE 3000
CMD ["cargo", "watch", "--workdir", "/app", "-w", "backend/src", "-x", "run --package ethos-protocol-backend"]

# ── Builder: compile the release binary ──────────────────────────────────────
FROM base AS builder
COPY Cargo.toml Cargo.lock ./
COPY contracts ./contracts
COPY backend ./backend
RUN cargo build --release --package ethos-protocol-backend --locked

# ── Runtime: minimal production image ────────────────────────────────────────
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
      && rm -rf /var/lib/apt/lists/* \
      && useradd --system --create-home --shell /usr/sbin/nologin ethos

WORKDIR /app
COPY --from=builder /app/target/release/ethos-protocol-backend /usr/local/bin/ethos-protocol-backend

USER ethos
EXPOSE 3000
ENV RUST_LOG=info

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -f http://localhost:3000/health || exit 1

ENTRYPOINT ["/usr/local/bin/ethos-protocol-backend"]
