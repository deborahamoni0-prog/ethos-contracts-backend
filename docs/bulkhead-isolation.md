# Bulkhead Isolation

Previously a single slow or overloaded endpoint could exhaust shared
resources and degrade every other endpoint. `backend/src/bulkhead.rs`
gives each endpoint its own bounded concurrency pool ("thread pool") and
bounded wait queue, isolating failures.

## How it works

- Requests are grouped into a bulkhead by the first two path segments
  (e.g. `/api/vaults/42` and `/api/vaults/7` share the `/api/vaults`
  bulkhead; `/webhooks` gets its own).
- Each bulkhead wraps a `tokio::sync::Semaphore` sized to
  `max_concurrent` — this is the per-endpoint "thread pool".
- Requests that can't immediately acquire a slot are queued (counted, not
  literally buffered) up to `max_queue_size`. Once the queue is full,
  further requests are rejected immediately with `503 Service Unavailable`
  instead of piling up indefinitely.
- Metrics (`active`, `queued`, `rejected_total`, `completed_total`) are
  tracked per endpoint with atomics.

Default configuration is 10 concurrent requests / 20 queued per endpoint
group (`BulkheadConfig::default()`); override per endpoint via
`BulkheadRegistry::configure`.

## Middleware

`bulkhead_middleware` is layered globally in `main.rs::build_router` via
`axum::middleware::from_fn_with_state`, so isolation applies to every
route without per-handler changes.

## Metrics endpoint

### `GET /admin/bulkheads/metrics`

```json
[
  {
    "endpoint": "/api/vaults",
    "max_concurrent": 10,
    "max_queue_size": 20,
    "active": 3,
    "queued": 0,
    "rejected_total": 0,
    "completed_total": 128
  }
]
```

## Testing isolation

`backend/src/bulkhead.rs` includes unit tests
(`isolated_endpoints_do_not_share_capacity`, `queue_overflow_is_rejected`,
`acquire_respects_concurrency_limit`) that saturate one endpoint's
bulkhead and assert a different endpoint's bulkhead is unaffected, and
that a full queue is rejected rather than blocking forever.
