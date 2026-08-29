# Query Cache

## Overview

The `QueryCache` (implemented in `backend/src/query_cache.rs`) is a thread-safe,
in-memory cache for arbitrary `serde_json::Value` query results.  It gives
route handlers a cheap first-level lookup before touching the SQLite database,
which matters most for frequently read, slowly changing data such as vault
summaries and subscription preferences.

## Cache Strategy

The cache uses a **read-through** strategy:

1. The handler checks the cache with a unique key (e.g. `"vault:42:summary"`).
2. On a **hit** the cached value is returned immediately; the hit counter is
   incremented atomically.
3. On a **miss** (key absent or entry expired) the handler queries the database,
   stores the result, and returns it; the miss counter is incremented.

Writes that mutate data must call the appropriate invalidation method so that
subsequent reads see fresh values.

## Time to Live (TTL)

Each entry has an independent TTL stored alongside it.  The default TTL is
**60 seconds**, configurable at construction time via `QueryCache::with_ttl`.

Expired entries are **lazily evicted**: they are removed from the map the next
time the same key is accessed, not by a background sweeper.  The `stats()`
method reports how many entries currently exist in the expired-but-not-yet-
evicted state.

## Invalidation

Four granularities of invalidation are available:

| Method | Scope |
|---|---|
| `invalidate(key)` | Remove exactly one entry |
| `invalidate_prefix(prefix)` | Remove all keys starting with `prefix` |
| `invalidate_all()` | Remove every entry |
| *(TTL expiry)* | Automatic on next access of a stale key |

### Recommended key conventions

```
vault:<vault_id>:summary        # VaultSummary
vault:<vault_id>:ttl            # TTL remaining
vault:<vault_id>:prefs          # ReminderPreferences
subscription:<vault_id>         # Subscription
```

To invalidate all data for a single vault after a check-in:

```rust
state.query_cache.invalidate_prefix(&format!("vault:{}:", vault_id));
```

## Integration with AppState

`AppState` (in `db.rs`) carries:

```rust
pub query_cache: Arc<QueryCache>,
```

Initialised in `main.rs` as:

```rust
query_cache: Arc::new(QueryCache::new()),
```

## Statistics Endpoint

`GET /admin/query-cache/stats`

Returns a JSON object:

```json
{
  "total_entries": 42,
  "hit_count": 1234,
  "miss_count": 56,
  "expired_entries": 3
}
```

| Field | Description |
|---|---|
| `total_entries` | Entries currently stored (including expired ones not yet evicted) |
| `hit_count` | Successful lookups since startup |
| `miss_count` | Failed lookups (key missing or expired) since startup |
| `expired_entries` | Entries past their TTL waiting for lazy eviction |

Hit/miss counters are maintained with `AtomicU64` so they do not require
holding the inner `Mutex`, keeping contention minimal even under high load.

## Concurrency Model

The map of entries is protected by a `std::sync::Mutex`.  The lock is held for
the minimum time needed:

- **get**: lock, inspect, optionally remove, unlock, then increment atomic
  counter.
- **set**: lock, insert, unlock.
- **invalidate_prefix**: lock, `retain`, unlock.

The hit/miss counters are outside the `Mutex` to avoid serialising counter
updates.
