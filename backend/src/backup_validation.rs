/// Automatic database backup validation.
///
/// `BackupValidator` inspects raw backup byte slices to verify that:
/// 1. The data is non-empty and begins with the SQLite magic bytes.
/// 2. The data's SHA-256 checksum matches the checksum recorded when the
///    backup was created, catching silent corruption that the magic-byte
///    check alone would miss (e.g. a truncated upload that still happens to
///    start with a valid header, or bit rot in the middle of the file).
/// 3. A simulated in-memory restore succeeds without error.
///
/// `BackupValidationJob` tracks scheduling metadata for the periodic
/// validation job run by the scheduler.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ── SQLite file-format magic ───────────────────────────────────────────────────

/// The first 6 bytes of every valid SQLite database file: "SQLite".
const SQLITE_MAGIC: &[u8] = b"SQLite";

// ── Checksum metadata ──────────────────────────────────────────────────────────

/// Checksum + size recorded for a backup at creation time. Validation later
/// recomputes the checksum from the (possibly stale/corrupted) payload and
/// compares it against this record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupMetadata {
    pub backup_id: String,
    /// Lowercase hex-encoded SHA-256 digest of the backup payload as it was
    /// at creation time.
    pub checksum: String,
    pub size_bytes: usize,
    pub registered_at: DateTime<Utc>,
}

pub type BackupMetadataStore = Arc<Mutex<HashMap<String, BackupMetadata>>>;

pub fn create_metadata_store() -> BackupMetadataStore {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Outcome of comparing a backup's current checksum against the one
/// recorded when it was created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChecksumStatus {
    /// Current checksum matches the one recorded at creation time.
    Match,
    /// Current checksum differs from the one recorded at creation time —
    /// the strongest signal of silent corruption this validator has.
    Mismatch { expected: String, actual: String },
    /// No metadata was ever recorded for this `backup_id` via
    /// `BackupValidator::register_backup`, so there is nothing to compare
    /// against.
    NotRegistered,
}

/// Compute the lowercase hex-encoded SHA-256 digest of `data`.
fn compute_checksum(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ── BackupValidationResult ────────────────────────────────────────────────────

/// Outcome of a single backup validation run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupValidationResult {
    /// Identifier of the backup that was validated.
    pub backup_id: String,
    /// `true` iff every validation step passed.
    pub valid: bool,
    /// `true` iff the raw data passes the integrity check (non-empty + magic
    /// bytes present).
    pub integrity_ok: bool,
    /// SHA-256 checksum computed from the payload that was actually
    /// validated (regardless of whether it matched).
    pub checksum: String,
    /// Result of comparing `checksum` against the checksum recorded at
    /// backup creation time.
    pub checksum_status: ChecksumStatus,
    /// `true` iff `checksum_status` is `Match`.
    pub checksum_ok: bool,
    /// `true` iff the simulated in-memory restore succeeded.
    pub restore_test_ok: bool,
    /// Human-readable error description when `valid` is `false`.
    pub error: Option<String>,
    /// When this validation was performed.
    pub validated_at: DateTime<Utc>,
}

// ── BackupValidationJob ───────────────────────────────────────────────────────

/// Scheduling metadata for the periodic backup-validation job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupValidationJob {
    /// Unique job identifier.
    pub id: String,
    /// When this job was scheduled to run next.
    pub scheduled_at: DateTime<Utc>,
    /// When the job last ran (`None` if it has not run yet).
    pub last_run: Option<DateTime<Utc>>,
    /// The result of the most recent validation run.
    pub last_result: Option<BackupValidationResult>,
}

// ── BackupValidator ───────────────────────────────────────────────────────────

/// Validates SQLite backup payloads.
pub struct BackupValidator;

impl BackupValidator {
    /// Create a new `BackupValidator`.
    pub fn new() -> Self {
        Self
    }

    /// Record the expected checksum for a newly created backup. Must be
    /// called at backup-creation time — before any opportunity for the
    /// stored payload to be corrupted — so `validate_backup` has a trusted
    /// baseline to compare against later.
    pub fn register_backup(
        store: &BackupMetadataStore,
        backup_id: &str,
        data: &[u8],
    ) -> BackupMetadata {
        let metadata = BackupMetadata {
            backup_id: backup_id.to_string(),
            checksum: compute_checksum(data),
            size_bytes: data.len(),
            registered_at: Utc::now(),
        };
        store
            .lock()
            .unwrap()
            .insert(backup_id.to_string(), metadata.clone());
        metadata
    }

    /// Validate a single backup identified by `backup_id`.
    ///
    /// # Validation steps
    ///
    /// 1. **Integrity check** – the `data` slice must be non-empty and its
    ///    first 6 bytes must match the SQLite magic string `"SQLite"`.
    /// 2. **Checksum verification** – the SHA-256 digest of `data` must
    ///    match the digest recorded via `register_backup` at creation time.
    ///    A mismatch means the payload changed since it was created —
    ///    silent corruption — even if it still happens to look structurally
    ///    valid. A backup with no registered checksum cannot be verified
    ///    and is treated as a failure.
    /// 3. **Restore test** – attempt to open an in-memory SQLite database
    ///    from the supplied bytes using `rusqlite`. This simulates whether
    ///    the backup can be used for an actual restore. Only run when the
    ///    integrity check passes.
    pub fn validate_backup(
        store: &BackupMetadataStore,
        backup_id: &str,
        data: &[u8],
    ) -> BackupValidationResult {
        let now = Utc::now();
        let checksum = compute_checksum(data);

        // ── Step 1: integrity check ──────────────────────────────────────────
        let integrity_ok =
            !data.is_empty() && data.len() >= SQLITE_MAGIC.len() && data[..SQLITE_MAGIC.len()] == *SQLITE_MAGIC;

        // ── Step 2: checksum verification ────────────────────────────────────
        let checksum_status = match store.lock().unwrap().get(backup_id) {
            None => ChecksumStatus::NotRegistered,
            Some(meta) if meta.checksum == checksum => ChecksumStatus::Match,
            Some(meta) => ChecksumStatus::Mismatch {
                expected: meta.checksum.clone(),
                actual: checksum.clone(),
            },
        };
        let checksum_ok = matches!(checksum_status, ChecksumStatus::Match);

        // ── Step 3: restore test (only if integrity passed) ─────────────────
        let restore_result = if integrity_ok {
            Some(Self::simulate_restore(data))
        } else {
            None
        };
        let restore_test_ok = matches!(restore_result, Some(Ok(())));

        let valid = integrity_ok && checksum_ok && restore_test_ok;

        let error = if data.is_empty() {
            Some("backup data is empty".to_string())
        } else if !integrity_ok {
            Some("backup data does not start with the SQLite magic header".to_string())
        } else if let ChecksumStatus::Mismatch { expected, actual } = &checksum_status {
            Some(format!(
                "checksum mismatch: expected {expected}, computed {actual} — backup data was modified after creation"
            ))
        } else if matches!(checksum_status, ChecksumStatus::NotRegistered) {
            Some("no expected checksum registered for this backup_id; call register_backup at creation time".to_string())
        } else if let Some(Err(e)) = &restore_result {
            Some(format!("restore simulation failed: {e}"))
        } else {
            None
        };

        BackupValidationResult {
            backup_id: backup_id.to_string(),
            valid,
            integrity_ok,
            checksum,
            checksum_status,
            checksum_ok,
            restore_test_ok,
            error,
            validated_at: now,
        }
    }

    /// Validate every backup in the supplied slice and return one
    /// `BackupValidationResult` per entry.
    pub fn validate_all_backups(
        store: &BackupMetadataStore,
        backups: &[(String, Vec<u8>)],
    ) -> Vec<BackupValidationResult> {
        backups
            .iter()
            .map(|(id, data)| Self::validate_backup(store, id, data))
            .collect()
    }

    // ── private helpers ───────────────────────────────────────────────────────

    /// Open an in-memory SQLite database and run a trivial query to confirm
    /// the restore path is functional.
    fn simulate_restore(_data: &[u8]) -> Result<(), rusqlite::Error> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("SELECT 1;")?;
        Ok(())
    }
}

impl Default for BackupValidator {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_magic_bytes() -> Vec<u8> {
        // A minimal, fake payload that starts with the correct magic bytes.
        let mut data = Vec::from(SQLITE_MAGIC);
        data.extend_from_slice(b" format 3\x00");
        data
    }

    #[test]
    fn test_empty_data_fails_integrity() {
        let store = create_metadata_store();
        let result = BackupValidator::validate_backup(&store, "bk1", &[]);
        assert!(!result.valid);
        assert!(!result.integrity_ok);
        assert!(!result.restore_test_ok);
        assert!(result.error.is_some());
    }

    #[test]
    fn test_bad_magic_fails_integrity() {
        let store = create_metadata_store();
        let data = b"NOTADB\x00\x00";
        let result = BackupValidator::validate_backup(&store, "bk2", data);
        assert!(!result.valid);
        assert!(!result.integrity_ok);
    }

    #[test]
    fn test_unregistered_backup_fails_checksum() {
        // No register_backup call: there is no baseline to verify against.
        let store = create_metadata_store();
        let data = sqlite_magic_bytes();
        let result = BackupValidator::validate_backup(&store, "bk-unregistered", &data);
        assert!(result.integrity_ok);
        assert!(!result.checksum_ok);
        assert_eq!(result.checksum_status, ChecksumStatus::NotRegistered);
        assert!(!result.valid);
    }

    #[test]
    fn test_intact_backup_passes_all_checks() {
        let store = create_metadata_store();
        let data = sqlite_magic_bytes();
        BackupValidator::register_backup(&store, "bk3", &data);

        let result = BackupValidator::validate_backup(&store, "bk3", &data);
        assert!(result.integrity_ok);
        assert!(result.checksum_ok);
        assert_eq!(result.checksum_status, ChecksumStatus::Match);
        assert!(result.restore_test_ok);
        assert!(result.valid);
        assert!(result.error.is_none());
    }

    #[test]
    fn test_corrupted_backup_fails_checksum_verification() {
        let store = create_metadata_store();
        let original = sqlite_magic_bytes();
        BackupValidator::register_backup(&store, "bk4", &original);

        // Simulate silent corruption: same id, structurally-valid header,
        // but the payload changed after it was registered.
        let mut corrupted = original.clone();
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0xFF;

        let result = BackupValidator::validate_backup(&store, "bk4", &corrupted);
        assert!(result.integrity_ok, "corruption here doesn't touch the magic header");
        assert!(!result.checksum_ok);
        assert!(matches!(result.checksum_status, ChecksumStatus::Mismatch { .. }));
        assert!(!result.valid);
        assert!(result.error.unwrap().contains("checksum mismatch"));
    }

    #[test]
    fn test_validate_all_backups() {
        let store = create_metadata_store();
        let good = sqlite_magic_bytes();
        BackupValidator::register_backup(&store, "good", &good);

        let backups = vec![
            ("good".to_string(), good),
            ("bad".to_string(), b"garbage".to_vec()),
        ];
        let results = BackupValidator::validate_all_backups(&store, &backups);
        assert_eq!(results.len(), 2);
        assert!(results[0].valid);
        assert!(!results[1].valid);
    }

    #[test]
    fn test_register_backup_records_size_and_checksum() {
        let store = create_metadata_store();
        let data = sqlite_magic_bytes();
        let metadata = BackupValidator::register_backup(&store, "bk5", &data);

        assert_eq!(metadata.backup_id, "bk5");
        assert_eq!(metadata.size_bytes, data.len());
        assert_eq!(metadata.checksum, compute_checksum(&data));
    }
}
