# Disaster Recovery Runbook Automation

## Overview

`backend/src/dr_automation.rs` wraps two error-prone manual steps from
`docs/disaster-recovery-runbook.md` as scriptable, audited API endpoints:
triggering/resolving failover (runbook §1) and validating a backup before
trusting it for a restore (runbook §4). All endpoints require an admin API
key (`Authorization: Bearer <ADMIN_API_KEY>`, enforced by
`audit::authorize_admin`).

## Confirmation Tokens

Triggering or resolving failover is destructive enough that admin auth
alone isn't considered sufficient — both require a short-lived, single-use
confirmation token minted by a separate call first:

```
POST /admin/dr/confirmations
Content-Type: application/json

{ "action": "failover_trigger" }
```

Response:

```json
{
  "confirmation_token": "5b1f...",
  "action": "failover_trigger",
  "expires_at": "2026-08-29T12:05:00Z"
}
```

Tokens expire after **5 minutes** and are **single-use** — consuming one
(successfully or not) removes it, so a retried request needs a fresh token.
A token is only accepted by the endpoint whose action it was minted for;
`action` must be exactly `"failover_trigger"` or `"failover_resolve"`.

## Failover

```
POST /admin/dr/failover/trigger
Content-Type: application/json

{ "confirmation_token": "5b1f...", "actor": "alice", "reason": "suspected exploit in vault contract" }
```

Marks the backend as being in failover mode and opens a `Sev1` incident
via `incidents.rs` describing who triggered it and why. Returns
`{ "failover_active": true, "last_changed_at": "..." }`.

```
POST /admin/dr/failover/resolve
```

Same request/response shape, requires a token minted for
`"failover_resolve"`. Clears failover mode once the root cause is fixed.

```
GET /admin/dr/failover/status
```

Read-only; no confirmation token required. Returns the current
`{ failover_active, last_changed_at }`.

## Backup Restore Validation

```
POST /admin/dr/backup-restore/validate
Content-Type: application/json

{ "backup_id": "backup-2026-07-26", "data_base64": "<base64-encoded SQLite file bytes>" }
```

Read-only — no confirmation token required. Runs the same checksum +
integrity + restore-simulation pipeline described in
`docs/backup-validation.md` (`POST /admin/validate-backup`), and opens a
`Sev2` incident on checksum mismatch, since discovering a bad backup
mid-incident is itself worth surfacing. Returns a `BackupValidationResult`.

## Action History

```
GET /admin/dr/history
```

Returns every DR automation action attempted (oldest first): action name,
actor, reason, outcome, and timestamp. Intended for the post-incident
review checklist in runbook §8.
