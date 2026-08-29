# Migration Testing

## Overview

`backend/src/db.rs` owns a small hand-rolled migration runner: `Db::migrate()`
applies an ordered list of `(version, sql)` pairs, tracking which versions
have run in the `schema_migrations` table so re-invoking `migrate()` is
idempotent. Historically only the "up" (apply) path was tested — nothing
verified that a migration's rollback path actually restores the prior
schema and data, only that applying migrations succeeded.

## Rollback Support

`Db::rollback(version: &str)` reverses a single migration:

1. Runs the reverse SQL registered for that version in the `DOWN_MIGRATIONS`
   table inside `rollback()`.
2. Deletes the version's row from `schema_migrations`, so a subsequent
   `migrate()` call re-applies it.

Every entry in `MIGRATIONS` (the up path) must have a matching entry in
`DOWN_MIGRATIONS` (the down path). When adding a new migration:

- If it creates tables/indexes, the down migration must `DROP` exactly what
  the up migration created.
- If it alters an existing table (e.g. `ADD COLUMN`), the down migration
  must reverse that exact alteration (e.g. `DROP COLUMN`, supported by the
  bundled SQLite version used via `rusqlite`'s `bundled` feature, 3.35+).
- If it transforms existing data (a backfill `UPDATE`, not just a schema
  change), the down migration only needs to undo the schema change — data
  transformations applied to pre-existing rows are not required to be
  perfectly invertible, but re-applying the migration after a rollback
  **must** correctly re-derive the transformed values for rows that existed
  before the rollback. See migration `"10"` (`normalized_frequency`) for an
  example: it adds a column and backfills it from `frequency`; its down
  migration just drops the column, and the rollback test suite verifies the
  backfill is correctly redone on re-apply for a row that existed prior to
  the rollback.

Because later migrations can depend on earlier ones' tables/columns,
rollbacks must only be performed from the top of the applied version list
downward (i.e., to roll back version `N`, versions `> N` must already be
rolled back). `Db::rollback()` does not enforce this — callers (tests, or
any future rollback tooling) are responsible for rolling back in the
correct order.

## Test Coverage

`backend/src/migration_rollback_tests.rs` covers:

1. **`test_apply_rollback_reapply_restores_full_schema`** — applies every
   migration, rolls back every migration (newest to oldest), asserts only
   the `schema_migrations` tracking table remains, then re-applies
   everything and asserts the full schema (table and index names) matches
   the original exactly.
2. **`test_each_migration_rollback_removes_and_reapply_restores_its_own_objects`** —
   for each migration from the newest down to version `"3"`, rolls it back
   in isolation, asserts its table (or, for the column-level migration
   `"10"`, its column) is gone, then re-applies and asserts the exact same
   column set is restored.
3. **`test_data_transformation_migration_rollback_preserves_seed_data`** —
   seeds a `reminder_preferences` row, rolls back the data-transformation
   migration `"10"`, asserts the row's other columns are untouched, then
   re-applies the migration and asserts `normalized_frequency` is correctly
   backfilled for that pre-existing row (not just for newly inserted rows).

Run locally:

```bash
cargo test --package ethos-protocol-backend migration_rollback
```

## CI

`.github/workflows/ci.yml` runs a dedicated "Run migration rollback tests
(apply, rollback, re-apply)" step on every push/PR, in addition to the
broader "Run backend cross-cutting integration tests" step (which also
exercises this file, since it runs the full backend test suite). Both are
required checks — a PR that breaks a migration's rollback path fails CI
before it can merge.

## Adding a New Migration: Checklist

- [ ] Add the up migration to `MIGRATIONS` in `Db::migrate()`.
- [ ] Add the matching down migration to `DOWN_MIGRATIONS` in `Db::rollback()`.
- [ ] If the migration transforms existing data, add or extend a test in
      `migration_rollback_tests.rs` seeding a pre-existing row and asserting
      the transformation is correctly redone after a rollback + re-apply
      cycle.
- [ ] Run `cargo test --package ethos-protocol-backend migration_rollback`
      locally before opening a PR.
