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

### Prometheus Metrics Export (#364)

`BulkheadRegistry::render_prometheus()` exports per-bulkhead metrics with endpoint labels into the application Prometheus exposition feed:

- `bulkhead_active_permits{endpoint="..."}` (gauge): Current in-flight permits.
- `bulkhead_queue_depth{endpoint="..."}` (gauge): Requests currently waiting in queue.
- `bulkhead_rejected_total{endpoint="..."}` (counter): Requests rejected due to full queue.
- `bulkhead_completed_total{endpoint="..."}` (counter): Requests successfully completed.
- `bulkhead_max_concurrent{endpoint="..."}` (gauge): Concurrency limit.
- `bulkhead_max_queue_size{endpoint="..."}` (gauge): Queue capacity.

### Grafana Panel Reference (#364)

Recommended Grafana dashboard panels for bulkhead isolation monitoring:

| Panel Title | Query (PromQL) | Visualization | Description |
|---|---|---|---|
| **Active Permits vs Concurrency Limit** | `bulkhead_active_permits` vs `bulkhead_max_concurrent` | Time series graph | Tracks in-flight load against pool capacity per endpoint. |
| **Bulkhead Queue Depth** | `bulkhead_queue_depth` | Time series graph / Gauge | Identifies pending request buildup per endpoint. |
| **Bulkhead Rejection Rate** | `sum by (endpoint) (rate(bulkhead_rejected_total[1m]))` | Bar chart / Graph | Detects endpoints experiencing saturation rejections. |
| **Throughput & Completions** | `sum by (endpoint) (rate(bulkhead_completed_total[1m]))` | Time series graph | Measures requests completed per second across bulkheads. |

## Testing isolation

`backend/src/bulkhead.rs` includes unit tests
(`isolated_endpoints_do_not_share_capacity`, `queue_overflow_is_rejected`,
`acquire_respects_concurrency_limit`, `prometheus_metrics_render_with_per_bulkhead_labels`)
that saturate one endpoint's bulkhead and assert a different endpoint's bulkhead
is unaffected, verify full queue rejection, and assert Prometheus metrics correctness.
