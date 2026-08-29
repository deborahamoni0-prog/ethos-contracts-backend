# Retry Policies

Retry logic used to be scattered ad hoc across call sites. The retry policy
framework (`backend/src/retry_policy.rs`) centralizes retry configuration
into a small policy language that can be registered per endpoint.

## Concepts

A `RetryPolicy` describes:

| Field | Description |
|---|---|
| `endpoint_pattern` | Path prefix the policy applies to, e.g. `/api/vaults` |
| `max_attempts` | Total attempts (including the first) before giving up |
| `base_delay_ms` | Delay before the first retry |
| `max_delay_ms` | Upper bound the exponential backoff is capped at |
| `multiplier` | Exponential growth factor applied per attempt (default `2.0`) |
| `jitter` | `none`, `full`, or `equal` (default `equal`) |
| `retry_on_status` | HTTP status codes that should trigger a retry (default `[429, 500, 502, 503, 504]`) |

## Exponential backoff + jitter

Delay for attempt `n` is `min(base_delay_ms * multiplier^(n-1), max_delay_ms)`,
then jitter is applied:

- `none`: use the computed delay as-is.
- `full`: uniform random value in `[0, delay]`.
- `equal`: `delay/2 + uniform(0, delay/2)` — keeps a floor while still
  spreading retries out, recommended default to avoid thundering herds.

See `compute_backoff_delay` and `execute_with_retry` for the implementation,
which callers can use directly:

```rust
let result = execute_with_retry(
    &policy,
    |err: &MyError| err.is_transient(),
    || do_the_request(),
).await;
```

## API

### `POST /admin/retry-policies`

```json
{
  "name": "vault-reads",
  "endpoint_pattern": "/api/vaults",
  "max_attempts": 4,
  "base_delay_ms": 100,
  "max_delay_ms": 5000,
  "multiplier": 2.0,
  "jitter": "equal",
  "retry_on_status": [429, 503]
}
```

Returns `201 Created` with the stored policy (including its generated `id`).

### `GET /admin/retry-policies`

Lists all registered policies.

### `GET /admin/retry-policies/:id`

Fetches a single policy by id, `404` if not found.

## Policy resolution

`RetryPolicyStore::find_for_path` matches the most specific
(longest-prefix) `endpoint_pattern` for a given request path, falling back
to no retry behavior if nothing matches.
