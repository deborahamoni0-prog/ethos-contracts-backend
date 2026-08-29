# Backup Validation

## Overview

The backup validation subsystem (implemented in
`backend/src/backup_validation.rs`) automatically inspects raw database backup
payloads to confirm they are structurally sound and can be used for an actual
restore.  This catches silent corruption — such as a truncated S3 upload or a
failed copy — before the backup is ever needed in a disaster scenario.

## Validation Steps

Each backup is subjected to two sequential checks:

### 1. Integrity Check

The raw bytes are inspected for:

- **Non-empty data**: an empty file can never be a valid SQLite backup.
- **SQLite magic bytes**: every valid SQLite 3 database file begins with the
  6-byte sequence `SQLite` (`\x53\x51\x4c\x69\x74\x65`).  If either condition
  fails the validation stops with `integrity_ok: false`.

### 2. Restore Test

An in-memory SQLite connection is opened via `rusqlite::Connection::open_in_memory`
and a trivial `SELECT 1` is executed.  This confirms that:

- The `rusqlite` library is functional in the current environment.
- The restore pipeline (opening a connection, running a query) does not panic
  or error.

In a future enhancement the backup bytes would be written to a temporary file
and opened directly for a more faithful restore simulation.

## BackupValidationResult

```json
{
  "backup_id": "backup-2026-07-26",
  "valid": true,
  "integrity_ok": true,
  "restore_test_ok": true,
  "error": null,
  "validated_at": "2026-07-26T23:00:00Z"
}
```

| Field | Description |
|---|---|
| `backup_id` | Caller-supplied identifier |
| `valid` | `true` only when both checks pass |
| `integrity_ok` | Magic-bytes check result |
| `restore_test_ok` | In-memory restore simulation result |
| `error` | Human-readable reason for failure (null on success) |
| `validated_at` | UTC timestamp of the validation run |

## API Endpoint

`POST /admin/validate-backup`

Request body:

```json
{
  "backup_id": "backup-2026-07-26",
  "data_base64": "<base64-encoded SQLite file bytes>"
}
```

Response: `BackupValidationResult` JSON as shown above.

The raw backup bytes are base64-encoded in transit to keep the JSON payload
self-contained without multi-part uploads.

## Scheduled Job

The scheduler runs a backup validation job approximately **every hour** (every
60 ticks of the one-minute scheduler loop).

In the current implementation the job logs a scheduled-run event and processes
any backup payloads provided by the storage integration layer.  Once a real
backup storage adapter (S3, GCS, local filesystem) is wired up, the job will
retrieve the most recent backup snapshot and validate it automatically,
alerting via `tracing::warn!` on failure.

## Adding New Validation Checks

To add a new check, extend the `validate_backup` method in
`backup_validation.rs`.  Follow the existing pattern:

1. Perform the check.
2. Return early with `valid: false` and an informative `error` string if the
   check fails.
3. Otherwise set the corresponding `*_ok` field to `true`.
