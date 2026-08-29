# Feature Flags

Feature flags let engineers merge incomplete work directly into `main`
(trunk-based development) instead of maintaining long-lived feature
branches. Code paths are gated behind a flag and only exposed to traffic
once they're ready, via gradual rollout.

## Storage & evaluation

Flags are stored durably in SQLite via the shared [`Db`](backend/src/db.rs)
(`backend/src/feature_flags.rs`), keyed by flag `key`. Storage is **shared
across all instances** of the backend: every read and evaluation goes through
the database, so an update made through one instance is immediately visible to
every other instance sharing the same database file — including under a
load-balanced deployment. State also **survives process restarts**.

Each flag tracks:

- `enabled` — the master on/off switch
- `rollout_percentage` (0-100) — what percentage of subjects see the flag as
  enabled when `enabled` is true
- `version` — incremented on every update
- `history` — snapshots of prior versions (loaded from the
  `feature_flag_history` SQL table on every read) for auditing/rollback

Evaluation hashes `(flag_key, subject_id)` into a stable bucket in `[0, 100)`
using an FNV-1a style hash, so the same subject always gets a consistent
result and raising the rollout percentage only ever adds subjects.

## API

### `POST /admin/flags`

Create or update a flag.

```json
{
  "key": "new-checkout",
  "description": "New checkout flow",
  "enabled": true,
  "rollout_percentage": 25,
  "updated_by": "alice"
}
```

Returns the resulting `FeatureFlag`, including its incremented `version`.

### `GET /admin/flags`

List all flags.

### `GET /admin/flags/:key`

Fetch a single flag by key.

### `POST /admin/flags/:key/evaluate`

Evaluate a flag for a specific subject:

```json
{ "subject_id": "user-123" }
```

Response:

```json
{
  "key": "new-checkout",
  "subject_id": "user-123",
  "enabled": true,
  "reason": "gradual rollout at 25%",
  "flag_version": 3
}
```

## Gradual rollout example

1. Ship code behind `if evaluate_flag(&flag, &user_id).enabled { ... }`.
2. `POST /admin/flags` with `rollout_percentage: 5` to expose to 5% of users.
3. Monitor error rates / metrics.
4. Increase `rollout_percentage` incrementally (25, 50, 100) via repeated
   `POST /admin/flags` calls — each call bumps `version` and records the
   previous state in the `feature_flag_history` table.

## Versioning & rollback

Every `POST /admin/flags` call writes a `FlagVersionSnapshot` (the flag's
state *before* the update) to the `feature_flag_history` SQL table, so the
audit trail is durable and shared across instances. To roll back, `POST` the
values from the desired historical snapshot — this creates a new version
rather than mutating history in place, keeping a full audit trail.

## Consistency guarantees

- **Cross-instance:** flag reads, evaluations, and updates all go through the
  shared SQL store (`FlagState` wraps an `Arc<Db>`), so two instances always
  observe the same flag state after either receives an update.
- **Durability:** flag state and version history survive process restarts.
- **Stable bucketing:** evaluation hashes `(flag_key, subject_id)` into a
  stable bucket in `[0, 100)` using FNV-1a, unchanged from when storage was
  in-memory — the same subject always gets the same result for a given
  rollout percentage, and increasing the percentage only ever adds subjects.
