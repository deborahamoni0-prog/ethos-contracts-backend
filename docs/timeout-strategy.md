# Timeout Strategy

Timeouts were previously hardcoded, making them impossible to tune per
endpoint without a code change and redeploy. `backend/src/timeout_policy.rs`
makes timeouts configurable at runtime.

## Resolution order

For a given request path, the effective timeout is resolved as:

1. The most specific (longest-prefix) registered `TimeoutPolicy` for that
   path, if any.
2. Otherwise the global default (`30_000ms`).
3. If the caller sends `X-Timeout-Override-Ms`, that value wins, clamped
   to `max_override_ms` (`120_000ms`) so a client can't request an
   unbounded timeout.

## API

### `POST /admin/timeout-policies`

```json
{
  "endpoint_pattern": "/api/vaults/simulate-release",
  "timeout_ms": 10000
}
```

Returns `201 Created` with the stored policy.

### `GET /admin/timeout-policies`

Lists all registered policies.

### `GET /admin/timeout-policies/:id`

Fetches a single policy, `404` if not found.

### `GET /admin/timeout-policies/violations`

Returns a running count of requests that were aborted for exceeding their
timeout budget: `{"total_violations": 3}`.

## Per-request override

Send `X-Timeout-Override-Ms: 5000` on any request to request a tighter or
looser timeout than the resolved policy, useful for latency-sensitive
callers or long-running admin/batch operations.

## Violation alerts

`timeout_middleware` wraps every request with `tokio::time::timeout`. On
expiry it:

- Increments an atomic violation counter (exposed via
  `/admin/timeout-policies/violations`).
- Emits a `tracing::warn!` structured log line with `path`, `timeout_ms`,
  and the running `total_violations`, suitable for hooking into an
  alerting pipeline (see `docs/monitoring-guide.md`).
- Returns `504 Gateway Timeout` with a JSON body describing which budget
  was exceeded.

## Middleware wiring

`timeout_middleware` is layered globally in `main.rs::build_router` via
`axum::middleware::from_fn_with_state`, so every route is covered without
per-handler changes.
