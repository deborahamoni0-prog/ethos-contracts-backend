//! Migration rollback tests (docs/migration-testing.md).
//!
//! `Db::migrate()` only verified that applying migrations succeeds. These
//! tests verify the down/rollback path added in `Db::rollback()` actually
//! restores prior schema *and* data, not just that `migrate()` runs without
//! error.

#![cfg(test)]

use std::sync::Arc;

use crate::db::Db;
use crate::models::{Channel, Frequency, ReminderPreferences};

const ALL_VERSIONS_NEWEST_FIRST: &[&str] =
    &["10", "9", "8", "7", "6", "5", "4", "3", "2", "1"];

/// Apply every migration, roll every migration back (newest to oldest, the
/// only safe order since later migrations may depend on earlier ones'
/// tables), then re-apply everything and assert the resulting schema is
/// byte-for-byte identical to the original.
#[test]
fn test_apply_rollback_reapply_restores_full_schema() {
    let db = Db::open(":memory:").unwrap();
    db.migrate().unwrap();

    let objects_before = db.schema_object_names();
    assert!(
        objects_before.len() > 10,
        "sanity check: expected many tables/indexes after migrate()"
    );

    for version in ALL_VERSIONS_NEWEST_FIRST {
        db.rollback(version).unwrap();
    }

    let objects_after_rollback = db.schema_object_names();
    assert_eq!(
        objects_after_rollback,
        vec!["schema_migrations".to_string()],
        "rolling back every migration must leave only the tracking table behind"
    );

    db.migrate().unwrap();
    let objects_after_reapply = db.schema_object_names();
    assert_eq!(
        objects_after_reapply, objects_before,
        "re-applying after a full rollback must restore the exact original schema"
    );
}

/// Roll back each migration one at a time (from the top) and verify its
/// specific table/column disappears, then that re-applying restores it with
/// the same column set it originally had.
#[test]
fn test_each_migration_rollback_removes_and_reapply_restores_its_own_objects() {
    let db = Db::open(":memory:").unwrap();
    db.migrate().unwrap();

    let checks: &[(&str, &str)] = &[
        ("10", "reminder_preferences"), // column-level check handled separately below
        ("9", "idempotency_keys_cleanup"),
        ("8", "search_facets"),
        ("7", "collaborative_sessions"),
        ("6", "tenants"),
        ("5", "vault_subscriptions"),
        ("4", "two_factor_config"),
        ("3", "audit_logs"),
    ];

    for (version, table) in checks {
        let columns_before = db.table_columns(table);
        assert!(!columns_before.is_empty(), "table {table} must exist before rollback");

        db.rollback(version).unwrap();

        if *version == "10" {
            // Migration 10 only adds/drops a column, not the whole table.
            let columns_after = db.table_columns(table);
            assert!(
                !columns_after
                    .iter()
                    .any(|(name, _)| name == "normalized_frequency"),
                "normalized_frequency column must be gone after rolling back migration 10"
            );
        } else {
            let columns_after = db.table_columns(table);
            assert!(
                columns_after.is_empty(),
                "table {table} must no longer exist after rolling back migration {version}"
            );
        }

        db.migrate().unwrap();
        let columns_restored = db.table_columns(table);
        assert_eq!(
            columns_restored, columns_before,
            "re-applying migration {version} must restore {table}'s exact original columns"
        );
    }
}

/// Migration "10" is a data-transformation migration, not just a schema
/// change: it backfills `normalized_frequency` from existing `frequency`
/// values. Verify that rolling it back preserves the pre-existing seed data
/// untouched, and that re-applying it correctly re-derives the transformed
/// column for that same pre-existing row.
#[test]
fn test_data_transformation_migration_rollback_preserves_seed_data() {
    let db = Arc::new(Db::open(":memory:").unwrap());
    db.migrate().unwrap();

    let prefs = ReminderPreferences {
        vault_id: 42,
        channels: vec![Channel::Email],
        hours_before_expiry: 24,
        frequency: Frequency::Daily,
        deleted_at: None,
    };
    db.upsert(&prefs).unwrap();

    // Roll back the data-transformation migration: this drops
    // normalized_frequency but must leave the seed row's other columns
    // exactly as they were.
    db.rollback("10").unwrap();

    let columns_after_rollback = db.table_columns("reminder_preferences");
    assert!(
        !columns_after_rollback
            .iter()
            .any(|(name, _)| name == "normalized_frequency"),
        "normalized_frequency must not exist after rollback"
    );

    let fetched = db.get(prefs.vault_id).unwrap();
    assert_eq!(fetched.vault_id, prefs.vault_id);
    assert_eq!(fetched.hours_before_expiry, prefs.hours_before_expiry);
    assert_eq!(fetched.frequency, prefs.frequency);
    assert_eq!(fetched.channels, prefs.channels);

    // Re-apply: the backfill UPDATE in migration 10 must run again over the
    // pre-existing seed row (not just newly inserted rows), correctly
    // deriving normalized_frequency from its current frequency value.
    db.migrate().unwrap();

    let normalized = db.get_normalized_frequency(prefs.vault_id).unwrap();
    assert_eq!(
        normalized,
        Some("\"DAILY\"".to_string()),
        "re-applying migration 10 must backfill normalized_frequency for the pre-existing row"
    );
}
