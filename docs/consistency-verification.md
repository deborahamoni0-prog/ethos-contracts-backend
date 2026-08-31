# Data Consistency Verification

## Overview

The `ConsistencyChecker` (implemented in `backend/src/consistency.rs`) runs a
set of cross-table data-integrity checks against the live SQLite database and
produces a `ConsistencyReport` describing every anomaly found.

Checks are designed to be **read-only** and **idempotent**: they never mutate
data and can be run at any time without side effects.

## Severity Levels

| Level | Meaning |
|---|---|
| `warning` | Minor anomaly; application behaviour is not incorrect but data is unexpected. |
| `error` | Data integrity problem that may cause incorrect behaviour. |
| `critical` | Serious problem that could lead to data loss or propagation of corrupt state. |

## Available Checks

### 1. `check_foreign_keys` – Foreign Key Integrity

Runs SQLite's built-in `PRAGMA foreign_key_check`.

This PRAGMA returns one row per referential-integrity violation — for example,
a row in a child table that references a non-existent parent row.  Because
SQLite does not enforce foreign keys by default (they must be enabled per
connection), this check catches violations that accumulated while the pragma
was disabled.

**Severity**: `critical` — orphaned rows indicate data corruption.

### 2. `check_reminder_consistency` – Reminder Preferences

Performs two sub-checks on the `reminder_preferences` table:

#### 2a. `reminder_hours_before_expiry`

Counts rows where `hours_before_expiry = 0` and `deleted_at IS NULL`.  A
reminder set to fire zero hours before expiry would trigger at the exact moment
the vault expires, which is operationally useless.

**Severity**: `warning`

#### 2b. `reminder_orphaned_vault_ids`

Counts active `reminder_preferences` rows whose `vault_id` has no matching row
in `vault_subscriptions`.  Such rows will never result in a delivered
notification because the subscription channel list is missing.

**Severity**: `warning`

### 3. `check_derived_fields` – Tenant Vault References

Counts rows in `tenant_vaults` that reference a `tenant_id` with no
corresponding row in the `tenants` table.

**Severity**: `error` — the vault is assigned to a non-existent tenant, making
tenant-scoped queries unreliable.

## ConsistencyReport Structure

```json
{
  "checked_at": "2026-07-26T18:00:00Z",
  "issues": [
    {
      "check_name": "reminder_hours_before_expiry",
      "severity": "warning",
      "description": "2 reminder_preferences row(s) have hours_before_expiry = 0",
      "affected_rows": 2
    }
  ],
  "total_checks": 3,
  "passed_checks": 2,
  "failed_checks": 1
}
```

## API Endpoint

`POST /admin/verify-consistency`

No request body required.  Runs all checks synchronously and returns a
`ConsistencyReport` JSON object.

## Scheduled Job

The scheduler runs consistency checks approximately **every 6 hours** (every
360 ticks of the one-minute scheduler loop).

Log output:

- `tracing::error!` for `Critical` and `Error` severity issues.
- `tracing::warn!` for `Warning` severity issues.
- `tracing::info!` for the job start and completion summary.

## Adding New Checks

1. Add a `check_*` function to `ConsistencyChecker` in `consistency.rs`.
2. Register it in the `check_fns` slice inside `run_all_checks`.
3. Document it in this file with its severity level and the query it runs.

## Distributed Cache Consensus Reconciliation

Separate from the SQLite-focused checks above, `backend/src/consensus.rs`
implements `ConsensusReport` / `ConflictDetail` for comparing a node's local
cache against the shared `InMemoryBackend` / `RedisBackend` used for
multi-node distributed-cache consensus (`NodeCache::check_and_resolve`).

### Scheduled Job

The scheduler (`scheduler.rs`) runs a consensus reconciliation job at most
**every 5 minutes**. Unlike the SQLite consistency checks above, cache
divergence between nodes can compound quickly (each node keeps serving
stale reads until reconciled), so this job runs on a tighter cadence than
the 6-hour SQLite checks or the hourly backup validation job.

Each run:

1. Calls `NodeCache::check_and_resolve()`, which diffs the local cache
   against the distributed backend, resolves any conflicts per the
   configured `ConflictStrategy` (`last_write_wins` or `voting`), and
   returns a `ConsensusReport`.
2. Publishes the result as Prometheus metrics on `/metrics`:
   - `ethos_protocol_consensus_checks_total` (counter)
   - `ethos_protocol_consensus_conflicts_total` (counter)
   - `ethos_protocol_consensus_consistent` (gauge; 1 = consistent, 0 = conflicts found)
3. When conflicts are found, opens an incident via `incidents.rs`
   (`POST /incidents`-equivalent, severity `Sev3`) describing the affected
   keys and how many conflicts were auto-resolved, so operators are
   notified even if nobody is actively watching `/health/consensus` or
   `/metrics`.

### On-Demand Endpoint

`GET /health/consensus` runs the same check synchronously and returns the
current consistency status; it does not itself open an incident or update
the scheduled-job metrics above (those are only touched by the periodic
job).
