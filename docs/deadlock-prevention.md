# Deadlock Detection and Prevention

## Overview

The `DeadlockDetector` (implemented in `backend/src/deadlock.rs`) provides
three complementary mechanisms for avoiding and recovering from database
deadlocks:

1. **Lock ordering** – a canonical order of resource acquisition that prevents
   lock-order inversions.
2. **Cycle detection** – runtime detection of wait-for cycles before they block.
3. **Retry with exponential back-off** – automatic recovery when a transient
   deadlock is detected.

## Lock Ordering

The most reliable way to prevent deadlocks is to always acquire locks in the
same global order.  The `LOCK_ORDER` constant defines that order:

```rust
pub const LOCK_ORDER: &[&str] = &[
    "vaults",
    "subscriptions",
    "audit_logs",
    "tenants",
];
```

**Rule**: if a request needs to hold multiple resource locks simultaneously, it
must acquire them in the order they appear in `LOCK_ORDER`.  Acquiring
`"tenants"` before `"vaults"` would be a `LockOrderViolation`.

## Cycle Detection

When `acquire_lock(resource, holder)` is called and the resource is already
held:

1. `holder` is appended to the resource's `waiters` list.
2. `detect_cycle_inner` performs a single-level DFS over the wait-for graph.
   - If `holder` currently holds any resource that the existing holder is
     waiting on, a cycle is declared.
3. On cycle detection:
   - `DeadlockError::Deadlock { resources }` is returned.
   - `deadlock_count` is incremented atomically.

For the typical 2–4 resource lock graphs in this application a single-level
DFS is sufficient; a full multi-level BFS would be needed for deeper graphs.

## Retry Logic

`DeadlockDetector::with_retry` wraps any fallible operation with automatic
retry on `DeadlockError::Deadlock`:

```rust
let config = RetryConfig {
    max_retries: 3,
    backoff_ms: 50,    // doubles each attempt: 50 → 100 → 200
    timeout_ms: 5_000,
};

let result = DeadlockDetector::with_retry(&config, || {
    // ... operation that may deadlock
    Ok(42)
})?;
```

Retry behaviour:

| Attempt | Behaviour |
|---|---|
| 1 | Execute `f`. On `Deadlock`, sleep `backoff_ms` and retry. |
| 2 | Sleep `backoff_ms × 2`. |
| … | Double backoff each time. |
| `max_retries + 1` | Return the last `Deadlock` error. |
| Any | Return immediately on `Timeout` or `LockOrderViolation`. |
| Any | Return immediately if `timeout_ms` wall-clock elapsed. |

## Query Timeout Enforcement

`enforce_query_timeout` measures wall-clock elapsed time around a synchronous
closure and returns `DeadlockError::Timeout` if the closure took longer than
`timeout_ms`:

```rust
let result = DeadlockDetector::enforce_query_timeout(500, || {
    db.expensive_query()
})?;
```

This provides a last-resort guard against runaway queries holding the SQLite
`Mutex<Connection>` indefinitely.

## Statistics

`GET /admin/deadlock/stats`

```json
{
  "deadlocks_detected": 3,
  "retries_performed": 7,
  "active_locks": 1
}
```

| Field | Description |
|---|---|
| `deadlocks_detected` | Cumulative cycles detected since startup |
| `retries_performed` | Cumulative retries performed by `with_retry` |
| `active_locks` | Resources currently held by any holder |

## Integration with AppState

`AppState` carries:

```rust
pub deadlock_detector: Arc<DeadlockDetector>,
```

Initialised in `main.rs`:

```rust
deadlock_detector: Arc::new(DeadlockDetector::new()),
```

## DeadlockError Reference

| Variant | Meaning |
|---|---|
| `Deadlock { resources }` | A wait-for cycle was detected across the listed resources. |
| `Timeout { resource, waited_ms }` | Lock acquisition or query exceeded the time budget. |
| `LockOrderViolation { expected, got }` | Locks were acquired in the wrong order. |
