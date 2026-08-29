# Backup Validation

## Overview

The backup validation subsystem (implemented in
`backend/src/backup_validation.rs`) automatically inspects raw database backup
payloads to confirm they are structurally sound and can be used for an actual
restore.  This catches silent corruption — such as a truncated S3 upload or a
failed copy — before the backup is ever needed in a disaster scenario.

## Validation Steps

Each backup is subjected to three sequential checks:

### 1. Integrity Check

The raw bytes are inspected for:

- **Non-empty data**: an empty file can never be a valid SQLite backup.
- **SQLite magic bytes**: every valid SQLite 3 database file begins with the
  6-byte sequence `SQLite` (`\x53\x51\x4c\x69\x74\x65`).  If either condition
  fails the validation stops with `integrity_ok: false`.

### 2. Checksum Verification

`BackupValidator::register_backup` must be called at backup-creation time,
before the payload can be corrupted, to record its SHA-256 checksum in a
`BackupMetadataStore`. Validation recomputes the checksum from the payload
being validated and compares it against that recorded baseline:

- **`Match`** — the payload is byte-for-byte what it was at creation time.
- **`Mismatch { expected, actual }`** — the payload changed after creation.
  This is the strongest signal of silent corruption the validator has: a
  truncated upload or a bit-flip can still pass the integrity check above
  (structurally-valid header) while failing this comparison.
- **`NotRegistered`** — no checksum was ever recorded for this `backup_id`,
  so there's nothing to verify against; treated as a failure.

A checksum `Mismatch` opens an incident via `incidents.rs` (severity
`Sev2`) in addition to failing validation, since silent corruption is
worth surfacing to operators even outside an active incident review.

### 3. Restore Test

An in-memory SQLite connection is opened via `rusqlite::Connection::open_in_memory`
and a trivial `SELECT 1` is executed. This confirms that:

- The `rusqlite` library is functional in the current environment.
- The restore pipeline (opening a connection, running a query) does not panic
  or error.

Only run when the integrity check passes.

In a future enhancement the backup bytes would be written to a temporary file
and opened directly for a more faithful restore simulation.

## BackupValidationResult

```json
{
  "backup_id": "backup-2026-07-26",
  "valid": true,
  "integrity_ok": true,
  "checksum": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
  "checksum_status": { "status": "match" },
  "checksum_ok": true,
  "restore_test_ok": true,
  "error": null,
  "validated_at": "2026-07-26T23:00:00Z"
}
```

| Field | Description |
|---|---|
| `backup_id` | Caller-supplied identifier |
| `valid` | `true` only when all three checks pass |
| `integrity_ok` | Magic-bytes check result |
| `checksum` | SHA-256 digest computed from the validated payload |
| `checksum_status` | `match`, `mismatch` (with `expected`/`actual`), or `not_registered` |
| `checksum_ok` | `true` iff `checksum_status` is `match` |
| `restore_test_ok` | In-memory restore simulation result |
| `error` | Human-readable reason for failure (null on success) |
| `validated_at` | UTC timestamp of the validation run |

## API Endpoints

`POST /admin/backups/register` — record a backup's expected checksum at
creation time.

Request body:

```json
{
  "backup_id": "backup-2026-07-26",
  "data_base64": "<base64-encoded SQLite file bytes>"
}
```

Response: `BackupMetadata` — `{ backup_id, checksum, size_bytes, registered_at }`.

`POST /admin/validate-backup` — validate a backup payload against its
registered checksum.

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

In the current implementation the job logs a scheduled-run event and validates
any backup payloads provided by the storage integration layer, alerting via
`incidents.rs` on checksum mismatch the same way the endpoint does. Once a
real backup storage adapter (S3, GCS, local filesystem) is wired up, the job
will retrieve the most recent backup snapshot and validate it automatically
instead of iterating an empty placeholder list.

## Adding New Validation Checks

To add a new check, extend the `validate_backup` method in
`backup_validation.rs`.  Follow the existing pattern:

1. Perform the check.
2. Return early with `valid: false` and an informative `error` string if the
   check fails.
3. Otherwise set the corresponding `*_ok` field to `true`.
