#[cfg(test)]
use crate::models::VaultStatus;
use crate::models::{
    AuditEntry, AuditLogEntry, AuditLogQuery, Channel, Frequency, ReminderPreferences, SearchQuery,
    SearchResult, ShareToken, Subscription, SubscriptionChannel, SubscriptionFrequency,
    TwoFactorConfig, TwoFactorMethod, Vault, VaultBackup, VaultEvent, VaultNotificationPreferences,
    VaultShare,
};

use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub type VaultStore = Arc<Mutex<HashMap<String, Vault>>>;
pub type EventStore = Arc<Mutex<Vec<VaultEvent>>>;
pub type AuditStore = Arc<Mutex<Vec<AuditEntry>>>;
pub type BackupStore = Arc<Mutex<HashMap<String, VaultBackup>>>;
pub type ShareStore = Arc<Mutex<Vec<VaultShare>>>;
pub type ShareTokenStore = Arc<Mutex<HashMap<String, ShareToken>>>;
pub type NotificationStore = Arc<Mutex<HashMap<String, VaultNotificationPreferences>>>;

pub fn create_vault_store() -> VaultStore {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn create_event_store() -> EventStore {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn create_audit_store() -> AuditStore {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn create_backup_store() -> BackupStore {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn create_share_store() -> ShareStore {
    Arc::new(Mutex::new(Vec::new()))
}

pub fn create_share_token_store() -> ShareTokenStore {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn create_notification_store() -> NotificationStore {
    Arc::new(Mutex::new(HashMap::new()))
}

// ── Shared application state for axum routes ─────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub vault_store: VaultStore,
    pub event_store: EventStore,
    pub audit_store: AuditStore,
    pub share_store: ShareStore,
    pub share_token_store: ShareTokenStore,
    pub consensus: Arc<crate::consensus::NodeCache>,
}

impl axum::extract::FromRef<AppState> for Arc<Db> {
    fn from_ref(state: &AppState) -> Arc<Db> {
        Arc::clone(&state.db)
    }
}

impl axum::extract::FromRef<AppState> for Arc<AppState> {
    fn from_ref(state: &AppState) -> Arc<AppState> {
        Arc::new(state.clone())
    }
}

pub fn search_vaults(store: &VaultStore, query: &SearchQuery) -> SearchResult {
    let vaults = store.lock().unwrap();
    let page = query.page.unwrap_or(1);
    let limit = query.limit.unwrap_or(10);
    let offset = ((page - 1) * limit) as usize;

    let filtered: Vec<Vault> = vaults
        .values()
        .filter(|v| {
            if let Some(ref owner) = query.owner {
                if v.owner != *owner {
                    return false;
                }
            }
            if let Some(ref beneficiary) = query.beneficiary {
                if v.beneficiary != *beneficiary {
                    return false;
                }
            }
            if let Some(ref status) = query.status {
                if v.status != *status {
                    return false;
                }
            }
            if let Some(after) = query.created_after {
                if v.created_at < after {
                    return false;
                }
            }
            if let Some(before) = query.created_before {
                if v.created_at > before {
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    let total = filtered.len() as u32;
    let paginated: Vec<Vault> = filtered
        .into_iter()
        .skip(offset)
        .take(limit as usize)
        .collect();

    SearchResult {
        vaults: paginated,
        total,
        page,
        limit,
    }
}

pub fn get_vault_history(event_store: &EventStore, vault_id: &str) -> Vec<VaultEvent> {
    event_store
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.vault_id == vault_id)
        .cloned()
        .collect()
}

pub fn get_vault_audit_log(audit_store: &AuditStore, vault_id: &str) -> Vec<AuditEntry> {
    audit_store
        .lock()
        .unwrap()
        .iter()
        .filter(|a| {
            a.details
                .get("vault_id")
                .is_some_and(|v| v.as_str() == Some(vault_id))
        })
        .cloned()
        .collect()
}

// ── Task 1: Analytics ────────────────────────────────────────────────────────

pub fn compute_vault_analytics(store: &VaultStore) -> crate::models::VaultAnalytics {
    use crate::models::{TimeSeriesPoint, VaultAnalytics, VaultStatus};
    use std::collections::BTreeMap;

    let vaults = store.lock().unwrap();
    let total_vaults = vaults.len() as u64;
    let active_vaults = vaults
        .values()
        .filter(|v| v.status == VaultStatus::Active)
        .count() as u64;
    let released_vaults = vaults
        .values()
        .filter(|v| v.status == VaultStatus::Released)
        .count() as u64;

    let avg_ttl = if total_vaults > 0 {
        vaults
            .values()
            .map(|v| v.check_in_interval as f64)
            .sum::<f64>()
            / total_vaults as f64
    } else {
        0.0
    };

    let release_rate = if total_vaults > 0 {
        released_vaults as f64 / total_vaults as f64
    } else {
        0.0
    };

    // Build daily time-series bucketed by creation date
    let mut created_by_day: BTreeMap<String, u64> = BTreeMap::new();
    let mut released_by_day: BTreeMap<String, u64> = BTreeMap::new();
    for v in vaults.values() {
        let day = v.created_at.format("%Y-%m-%d").to_string();
        *created_by_day.entry(day.clone()).or_insert(0) += 1;
        if v.status == VaultStatus::Released {
            *released_by_day.entry(day).or_insert(0) += 1;
        }
    }

    let all_days: std::collections::BTreeSet<String> = created_by_day
        .keys()
        .chain(released_by_day.keys())
        .cloned()
        .collect();

    let time_series = all_days
        .into_iter()
        .map(|date| TimeSeriesPoint {
            vaults_created: *created_by_day.get(&date).unwrap_or(&0),
            vaults_released: *released_by_day.get(&date).unwrap_or(&0),
            date,
        })
        .collect();

    VaultAnalytics {
        total_vaults,
        active_vaults,
        average_ttl_seconds: avg_ttl,
        release_rate,
        time_series,
    }
}

// ── Task 2: Backup & Recovery ─────────────────────────────────────────────────

pub fn store_backup(backup_store: &BackupStore, backup: crate::models::VaultBackup) {
    backup_store
        .lock()
        .unwrap()
        .insert(backup.backup_id.clone(), backup);
}

pub fn get_backup(
    backup_store: &BackupStore,
    backup_id: &str,
) -> Option<crate::models::VaultBackup> {
    backup_store.lock().unwrap().get(backup_id).cloned()
}

// ── Task 3: Sharing ───────────────────────────────────────────────────────────

pub fn add_vault_share(share_store: &ShareStore, share: crate::models::VaultShare) {
    share_store.lock().unwrap().push(share);
}

pub fn get_vault_shares(
    share_store: &ShareStore,
    vault_id: &str,
) -> Vec<crate::models::VaultShare> {
    share_store
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.vault_id == vault_id)
        .cloned()
        .collect()
}

// ── Share token persistence ──────────────────────────────────────────────────

pub fn add_share_token(store: &ShareTokenStore, token: ShareToken) {
    store.lock().unwrap().insert(token.token.clone(), token);
}

pub fn get_share_token(store: &ShareTokenStore, token: &str) -> Option<ShareToken> {
    store.lock().unwrap().get(token).cloned()
}

pub fn get_vault_share_tokens(store: &ShareTokenStore, vault_id: &str) -> Vec<ShareToken> {
    store
        .lock()
        .unwrap()
        .values()
        .filter(|t| t.vault_id == vault_id)
        .cloned()
        .collect()
}

pub fn revoke_share_token(store: &ShareTokenStore, token: &str) -> Option<ShareToken> {
    let mut lock = store.lock().unwrap();
    if let Some(t) = lock.get_mut(token) {
        t.revoked = true;
        Some(t.clone())
    } else {
        None
    }
}

// ── Audit helper ─────────────────────────────────────────────────────────────

pub fn append_audit_entry(
    audit_store: &AuditStore,
    action: &str,
    actor: &str,
    details: serde_json::Value,
) {
    audit_store.lock().unwrap().push(AuditEntry {
        timestamp: Utc::now(),
        action: action.to_string(),
        actor: actor.to_string(),
        details,
    });
}

// ── Task 4: Notification Preferences ─────────────────────────────────────────

pub fn set_notification_preferences(
    notif_store: &NotificationStore,
    prefs: crate::models::VaultNotificationPreferences,
) {
    notif_store
        .lock()
        .unwrap()
        .insert(prefs.owner.clone(), prefs);
}

pub fn get_notification_preferences(
    notif_store: &NotificationStore,
    owner: &str,
) -> Option<crate::models::VaultNotificationPreferences> {
    notif_store.lock().unwrap().get(owner).cloned()
}

// ── TTL Insurance persistence (Postgres) ─────────────────────────────────────

use crate::models::TtlInsurancePolicy;

impl Db {
    pub async fn upsert_insurance_policy(&self, policy: &TtlInsurancePolicy) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.execute(
            r"
            INSERT INTO ttl_insurance_policies (
                vault_id,
                extension_seconds,
                inactivity_threshold_seconds,
                enabled,
                purchased_at,
                last_extended_at
            ) VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (vault_id) DO UPDATE SET
                extension_seconds = EXCLUDED.extension_seconds,
                inactivity_threshold_seconds = EXCLUDED.inactivity_threshold_seconds,
                enabled = EXCLUDED.enabled,
                purchased_at = EXCLUDED.purchased_at,
                last_extended_at = EXCLUDED.last_extended_at
            ",
            &[
                &policy.vault_id.cast_signed(),
                &policy.extension_seconds.cast_signed(),
                &policy.inactivity_threshold_seconds.cast_signed(),
                &policy.enabled,
                &policy.purchased_at,
                &policy.last_extended_at,
            ],
        )
        .await?;

        Ok(())
    }

    pub async fn get_insurance_policy(
        &self,
        vault_id: u64,
    ) -> Result<Option<TtlInsurancePolicy>, DbError> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                r"
            SELECT vault_id, extension_seconds, inactivity_threshold_seconds, enabled, purchased_at, last_extended_at
            FROM ttl_insurance_policies
            WHERE vault_id = $1
            ",
                &[&vault_id.cast_signed()],
            )
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(TtlInsurancePolicy {
            vault_id: row.try_get::<_, i64>(0)? as u64,
            extension_seconds: row.try_get::<_, i64>(1)? as u64,
            inactivity_threshold_seconds: row.try_get::<_, i64>(2)? as u64,
            enabled: row.try_get(3)?,
            purchased_at: row.try_get(4)?,
            last_extended_at: row.try_get(5)?,
        }))
    }

    pub async fn upsert_owner_activity(
        &self,
        owner_id: u64,
        last_active_at: chrono::DateTime<Utc>,
    ) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.execute(
            r"
            INSERT INTO owner_activity (owner_id, last_active_at)
            VALUES ($1, $2)
            ON CONFLICT (owner_id) DO UPDATE SET
                last_active_at = EXCLUDED.last_active_at
            ",
            &[&owner_id.cast_signed(), &last_active_at],
        )
        .await?;
        Ok(())
    }

    pub async fn get_owner_last_active_at(
        &self,
        owner_id: u64,
    ) -> Result<Option<chrono::DateTime<Utc>>, DbError> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                r"
            SELECT last_active_at
            FROM owner_activity
            WHERE owner_id = $1
            ",
                &[&owner_id.cast_signed()],
            )
            .await?;

        match row {
            Some(row) => Ok(Some(row.try_get(0)?)),
            None => Ok(None),
        }
    }

    pub async fn all_enabled_insurance_policies(&self) -> Result<Vec<TtlInsurancePolicy>, DbError> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                r"
            SELECT vault_id, extension_seconds, inactivity_threshold_seconds, enabled, purchased_at, last_extended_at
            FROM ttl_insurance_policies
            WHERE enabled = TRUE
            ",
                &[],
            )
            .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(TtlInsurancePolicy {
                vault_id: row.try_get::<_, i64>(0)? as u64,
                extension_seconds: row.try_get::<_, i64>(1)? as u64,
                inactivity_threshold_seconds: row.try_get::<_, i64>(2)? as u64,
                enabled: row.try_get(3)?,
                purchased_at: row.try_get(4)?,
                last_extended_at: row.try_get(5)?,
            });
        }
        Ok(out)
    }
}

use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::NoTls;

/// Errors surfaced by [`Db`]. Wraps both connection-pool acquisition failures
/// and Postgres query errors behind a single type so callers (and
/// `AppError::Db`) don't need to depend on `tokio-postgres`/`bb8` directly.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("failed to acquire a database connection: {0}")]
    Pool(#[from] bb8::RunError<tokio_postgres::Error>),
    #[error("database query failed: {0}")]
    Query(#[from] tokio_postgres::Error),
    #[error("invalid data stored in database: {0}")]
    Conversion(String),
    #[error("no matching row found")]
    NotFound,
}

pub struct PoolConfig {
    pub min: u32,
    pub max: u32,
    pub timeout_secs: u32,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            min: 2,
            max: 10,
            timeout_secs: 30,
        }
    }
}

impl PoolConfig {
    pub fn from_env() -> Self {
        Self {
            min: std::env::var("DB_POOL_MIN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2),
            max: std::env::var("DB_POOL_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            timeout_secs: std::env::var("DB_POOL_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
        }
    }
}

pub struct Db {
    pool: bb8::Pool<PostgresConnectionManager<NoTls>>,
    pool_config: PoolConfig,
    /// In-memory vault store shared across the application.
    pub vault_store: VaultStore,
}

impl Db {
    /// Open a connection pool against `database_url` (a Postgres connection
    /// string, e.g. `postgres://user:pass@host:5432/dbname`) using default
    /// pool settings.
    pub async fn open(database_url: &str) -> Result<Self, DbError> {
        Self::open_with_pool_config(database_url, &PoolConfig::default()).await
    }

    /// Open a connection pool against `database_url` using the given pool
    /// sizing/timeout configuration (see [`PoolConfig::from_env`]).
    pub async fn open_with_pool_config(
        database_url: &str,
        config: &PoolConfig,
    ) -> Result<Self, DbError> {
        let pg_config: tokio_postgres::Config = database_url.parse()?;
        Self::build(pg_config, config).await
    }

    /// Test-only helper: opens a pool scoped to a freshly created, uniquely
    /// named Postgres schema so that parallel tests (which all talk to the
    /// same physical test database) don't collide with each other's rows.
    /// The schema is created via a short-lived bootstrap connection and then
    /// wired into the pool's connection options as `search_path`.
    #[doc(hidden)]
    pub async fn open_isolated(database_url: &str) -> Result<Self, DbError> {
        let schema = format!("test_{}", uuid::Uuid::new_v4().simple());
        let base_config: tokio_postgres::Config = database_url.parse()?;

        // Bootstrap connection: create the schema before the pool starts
        // issuing queries against it. tokio-postgres requires the
        // connection's background I/O future to be polled independently, so
        // spawn it; it exits on its own once `client` is dropped.
        let (client, connection) = base_config.connect(NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "bootstrap postgres connection closed with error");
            }
        });
        client
            .batch_execute(&format!(r#"CREATE SCHEMA IF NOT EXISTS "{schema}""#))
            .await?;
        drop(client);

        let mut pg_config = base_config;
        pg_config.options(format!("-c search_path={schema}"));

        Self::build(pg_config, &PoolConfig::default()).await
    }

    async fn build(pg_config: tokio_postgres::Config, config: &PoolConfig) -> Result<Self, DbError> {
        let manager = PostgresConnectionManager::new(pg_config, NoTls);
        let max = config.max.max(config.min).max(1);
        let pool = bb8::Pool::builder()
            .min_idle(Some(config.min))
            .max_size(max)
            .connection_timeout(std::time::Duration::from_secs(u64::from(config.timeout_secs)))
            .build(manager)
            .await?;
        Ok(Self {
            pool,
            pool_config: PoolConfig {
                min: config.min,
                max: config.max,
                timeout_secs: config.timeout_secs,
            },
            vault_store: create_vault_store(),
        })
    }

    /// Pool sizing/timeout configuration this `Db` was opened with.
    pub fn pool_config(&self) -> &PoolConfig {
        &self.pool_config
    }

    /// Insert or replace a vault in the in-memory store.
    pub fn insert_vault(&self, vault: crate::models::Vault) {
        self.vault_store
            .lock()
            .unwrap()
            .insert(vault.id.clone(), vault);
    }

    /// Retrieve a vault from the in-memory store by string ID.
    pub fn get_vault(&self, vault_id: &str) -> Option<crate::models::Vault> {
        self.vault_store.lock().unwrap().get(vault_id).cloned()
    }

    pub async fn check_connectivity(&self) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.batch_execute("SELECT 1").await?;
        Ok(())
    }

    pub async fn migrate(&self) -> Result<(), DbError> {
        // Bootstrap the migration tracking table before anything else.
        {
            let conn = self.pool.get().await?;
            conn.batch_execute(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version    TEXT PRIMARY KEY,
                    applied_at TIMESTAMPTZ NOT NULL
                );",
            )
            .await?;
        }

        const MIGRATIONS: &[(&str, &str)] = &[
            (
                "1",
                r"
                CREATE TABLE IF NOT EXISTS reminder_preferences (
                    vault_id             BIGINT PRIMARY KEY,
                    channels             TEXT NOT NULL,
                    hours_before_expiry  BIGINT NOT NULL,
                    frequency            TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS ttl_insurance_policies (
                    vault_id                      BIGINT PRIMARY KEY,
                    extension_seconds             BIGINT NOT NULL,
                    inactivity_threshold_seconds  BIGINT NOT NULL,
                    enabled                        BOOLEAN NOT NULL,
                    purchased_at                   TIMESTAMPTZ NOT NULL,
                    last_extended_at               TIMESTAMPTZ
                );
                CREATE TABLE IF NOT EXISTS owner_activity (
                    owner_id       BIGINT PRIMARY KEY,
                    last_active_at TIMESTAMPTZ NOT NULL
                );
                CREATE TABLE IF NOT EXISTS idempotency_keys (
                    key           TEXT PRIMARY KEY,
                    status_code   BIGINT NOT NULL,
                    response_body TEXT NOT NULL,
                    created_at    TIMESTAMPTZ NOT NULL
                );
                CREATE TABLE IF NOT EXISTS unsubscribe_tokens (
                    token      TEXT PRIMARY KEY,
                    owner      TEXT NOT NULL,
                    created_at TIMESTAMPTZ NOT NULL
                );
                CREATE TABLE IF NOT EXISTS unsubscribed_users (
                    owner TEXT PRIMARY KEY
                );
                ",
            ),
            (
                "2",
                "ALTER TABLE reminder_preferences ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;",
            ),
            (
                "3",
                r"
                CREATE TABLE IF NOT EXISTS audit_logs (
                    id         BIGSERIAL PRIMARY KEY,
                    timestamp  TIMESTAMPTZ NOT NULL,
                    user_id    TEXT NOT NULL DEFAULT '',
                    action     TEXT NOT NULL,
                    resource   TEXT NOT NULL DEFAULT '',
                    result     TEXT NOT NULL DEFAULT 'success',
                    ip_address TEXT NOT NULL DEFAULT '',
                    details    JSONB
                );
                CREATE INDEX IF NOT EXISTS idx_audit_logs_timestamp ON audit_logs(timestamp);
                CREATE INDEX IF NOT EXISTS idx_audit_logs_user_id   ON audit_logs(user_id);
                CREATE INDEX IF NOT EXISTS idx_audit_logs_action    ON audit_logs(action);
                ",
            ),
            (
                "4",
                r"
                CREATE TABLE IF NOT EXISTS two_factor_config (
                    vault_id     TEXT PRIMARY KEY,
                    method       TEXT NOT NULL,
                    enabled      BOOLEAN NOT NULL DEFAULT FALSE,
                    secret       TEXT,
                    phone        TEXT,
                    email        TEXT,
                    created_at   TIMESTAMPTZ NOT NULL,
                    verified_at  TIMESTAMPTZ
                );
                ",
            ),
            (
                "5",
                r"
                CREATE TABLE IF NOT EXISTS vault_subscriptions (
                    vault_id   BIGINT PRIMARY KEY,
                    owner      TEXT NOT NULL,
                    channels   TEXT NOT NULL,
                    frequency  TEXT NOT NULL
                );
                ",
            ),
        ];

        for &(version, sql) in MIGRATIONS {
            let already_applied: bool = {
                let conn = self.pool.get().await?;
                conn.query_opt(
                    "SELECT 1 FROM schema_migrations WHERE version = $1",
                    &[&version],
                )
                .await?
                .is_some()
            };

            if already_applied {
                tracing::debug!(version = version, "migration already applied, skipping");
            } else {
                tracing::info!(version = version, "applying migration");
                let conn = self.pool.get().await?;
                conn.batch_execute(sql).await?;
                conn.execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES ($1, $2)",
                    &[&version, &Utc::now()],
                )
                .await?;
                tracing::info!(version = version, "migration applied successfully");
            }
        }

        Ok(())
    }

    pub async fn upsert(&self, prefs: &ReminderPreferences) -> Result<(), DbError> {
        let channels_json = serde_json::to_string(&prefs.channels).unwrap();
        let frequency_json = serde_json::to_string(&prefs.frequency).unwrap();
        let conn = self.pool.get().await?;
        conn.execute(
            r"
            INSERT INTO reminder_preferences (vault_id, channels, hours_before_expiry, frequency)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (vault_id) DO UPDATE SET
              channels = EXCLUDED.channels,
              hours_before_expiry = EXCLUDED.hours_before_expiry,
              frequency = EXCLUDED.frequency,
              deleted_at = NULL
            ",
            &[
                &prefs.vault_id.cast_signed(),
                &channels_json,
                &i64::from(prefs.hours_before_expiry),
                &frequency_json,
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn get(&self, vault_id: u64) -> Result<ReminderPreferences, DbError> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                r"SELECT vault_id, channels, hours_before_expiry, frequency, deleted_at
               FROM reminder_preferences
               WHERE vault_id = $1 AND deleted_at IS NULL",
                &[&vault_id.cast_signed()],
            )
            .await?;
        let row = row.ok_or(DbError::NotFound)?;

        let channels_str: String = row.try_get(1)?;
        let frequency_str: String = row.try_get(3)?;
        let channels: Vec<Channel> = serde_json::from_str(&channels_str).unwrap_or_default();
        let frequency: Frequency =
            serde_json::from_str(&frequency_str).map_err(|e| DbError::Conversion(e.to_string()))?;
        Ok(ReminderPreferences {
            vault_id: row.try_get::<_, i64>(0)? as u64,
            channels,
            hours_before_expiry: row.try_get::<_, i64>(2)? as u32,
            frequency,
            deleted_at: None,
        })
    }

    pub async fn all(&self) -> Result<Vec<ReminderPreferences>, DbError> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                r"SELECT vault_id, channels, hours_before_expiry, frequency, deleted_at
               FROM reminder_preferences
               WHERE deleted_at IS NULL",
                &[],
            )
            .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let channels_str: String = row.try_get(1)?;
            let frequency_str: String = row.try_get(3)?;
            let channels: Vec<Channel> = serde_json::from_str(&channels_str).unwrap_or_default();
            let frequency: Frequency = serde_json::from_str(&frequency_str)
                .map_err(|e| DbError::Conversion(e.to_string()))?;
            out.push(ReminderPreferences {
                vault_id: row.try_get::<_, i64>(0)? as u64,
                channels,
                hours_before_expiry: row.try_get::<_, i64>(2)? as u32,
                frequency,
                deleted_at: None,
            });
        }
        Ok(out)
    }

    pub async fn soft_delete_reminder(&self, vault_id: u64) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.execute(
            "UPDATE reminder_preferences SET deleted_at = $1 WHERE vault_id = $2 AND deleted_at IS NULL",
            &[&Utc::now(), &vault_id.cast_signed()],
        )
        .await?;
        Ok(())
    }

    pub async fn all_reminders_including_deleted(
        &self,
        vault_id: u64,
    ) -> Result<Vec<ReminderPreferences>, DbError> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                r"SELECT vault_id, channels, hours_before_expiry, frequency, deleted_at
               FROM reminder_preferences
               WHERE vault_id = $1",
                &[&vault_id.cast_signed()],
            )
            .await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let channels_str: String = row.try_get(1)?;
            let frequency_str: String = row.try_get(3)?;
            let channels: Vec<Channel> = serde_json::from_str(&channels_str).unwrap_or_default();
            let frequency: Frequency = serde_json::from_str(&frequency_str)
                .map_err(|e| DbError::Conversion(e.to_string()))?;
            let deleted_at: Option<chrono::DateTime<Utc>> = row.try_get(4)?;
            out.push(ReminderPreferences {
                vault_id: row.try_get::<_, i64>(0)? as u64,
                channels,
                hours_before_expiry: row.try_get::<_, i64>(2)? as u32,
                frequency,
                deleted_at,
            });
        }
        Ok(out)
    }

    pub async fn upsert_subscription(&self, sub: &Subscription) -> Result<(), DbError> {
        let channels_json = serde_json::to_string(&sub.channels).unwrap();
        let frequency_json = serde_json::to_string(&sub.frequency).unwrap();
        let conn = self.pool.get().await?;
        conn.execute(
            r"
            INSERT INTO vault_subscriptions (vault_id, owner, channels, frequency)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (vault_id) DO UPDATE SET
              owner = EXCLUDED.owner,
              channels = EXCLUDED.channels,
              frequency = EXCLUDED.frequency
            ",
            &[
                &sub.vault_id.cast_signed(),
                &sub.owner,
                &channels_json,
                &frequency_json,
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn delete_subscription(&self, vault_id: u64) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM vault_subscriptions WHERE vault_id = $1",
            &[&vault_id.cast_signed()],
        )
        .await?;
        Ok(())
    }

    pub async fn get_subscription(&self, vault_id: u64) -> Result<Option<Subscription>, DbError> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                r"SELECT vault_id, owner, channels, frequency
               FROM vault_subscriptions
               WHERE vault_id = $1",
                &[&vault_id.cast_signed()],
            )
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };
        let channels_str: String = row.try_get(2)?;
        let frequency_str: String = row.try_get(3)?;
        let channels: Vec<SubscriptionChannel> =
            serde_json::from_str(&channels_str).unwrap_or_default();
        let frequency: SubscriptionFrequency =
            serde_json::from_str(&frequency_str).map_err(|e| DbError::Conversion(e.to_string()))?;
        Ok(Some(Subscription {
            vault_id: row.try_get::<_, i64>(0)? as u64,
            owner: row.try_get(1)?,
            channels,
            frequency,
        }))
    }

    // ── Idempotency (#825) ──────────────────────────────────────────────────

    pub async fn store_idempotency(&self, key: &str, status_code: u16, response_body: &str) {
        let Ok(conn) = self.pool.get().await else {
            return;
        };
        let _ = conn
            .execute(
                r"
            INSERT INTO idempotency_keys (key, status_code, response_body, created_at)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (key) DO UPDATE SET
              status_code = EXCLUDED.status_code,
              response_body = EXCLUDED.response_body,
              created_at = EXCLUDED.created_at
            ",
                &[&key, &i64::from(status_code), &response_body, &Utc::now()],
            )
            .await;
    }

    pub async fn check_idempotency(&self, key: &str) -> Option<crate::models::IdempotencyRecord> {
        let conn = self.pool.get().await.ok()?;
        let row = conn
            .query_opt(
                "SELECT key, status_code, response_body, created_at FROM idempotency_keys WHERE key = $1",
                &[&key],
            )
            .await
            .ok()??;

        let created_at: chrono::DateTime<Utc> = row.try_get(3).ok()?;
        let age = Utc::now().signed_duration_since(created_at).num_seconds();
        if age > 86_400 {
            return None;
        }
        Some(crate::models::IdempotencyRecord {
            key: row.try_get(0).ok()?,
            status_code: row.try_get::<_, i64>(1).ok()? as u16,
            response_body: row.try_get(2).ok()?,
            created_at,
        })
    }

    // ── Unsubscribe (#828) ──────────────────────────────────────────────────

    pub async fn store_unsubscribe_token(&self, token: &str, owner: &str) {
        let Ok(conn) = self.pool.get().await else {
            return;
        };
        let _ = conn
            .execute(
                r"
            INSERT INTO unsubscribe_tokens (token, owner, created_at)
            VALUES ($1, $2, $3)
            ON CONFLICT (token) DO UPDATE SET
              owner = EXCLUDED.owner,
              created_at = EXCLUDED.created_at
            ",
                &[&token, &owner, &Utc::now()],
            )
            .await;
    }

    pub async fn process_unsubscribe(&self, token: &str) -> Result<String, String> {
        let conn = self.pool.get().await.map_err(|e| e.to_string())?;
        let row = conn
            .query_opt(
                "SELECT owner FROM unsubscribe_tokens WHERE token = $1",
                &[&token],
            )
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "invalid or expired unsubscribe token".to_string())?;
        let owner: String = row.try_get(0).map_err(|e| e.to_string())?;

        conn.execute(
            "INSERT INTO unsubscribed_users (owner) VALUES ($1) ON CONFLICT (owner) DO NOTHING",
            &[&owner],
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok(owner)
    }

    pub async fn is_unsubscribed(&self, owner: &str) -> bool {
        let Ok(conn) = self.pool.get().await else {
            return false;
        };
        conn.query_opt(
            "SELECT 1 FROM unsubscribed_users WHERE owner = $1",
            &[&owner],
        )
        .await
        .ok()
        .flatten()
        .is_some()
    }

    pub async fn generate_unsubscribe_token(&self, owner: &str) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.store_unsubscribe_token(&token, owner).await;
        token
    }

    // ── 2FA operations (#965) ───────────────────────────────────────────────

    pub async fn upsert_2fa_config(&self, config: &TwoFactorConfig) -> Result<(), DbError> {
        let method_str = serde_json::to_string(&config.method).unwrap();
        let conn = self.pool.get().await?;
        conn.execute(
            r"
            INSERT INTO two_factor_config (vault_id, method, enabled, secret, phone, email, created_at, verified_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (vault_id) DO UPDATE SET
                method = EXCLUDED.method,
                enabled = EXCLUDED.enabled,
                secret = EXCLUDED.secret,
                phone = EXCLUDED.phone,
                email = EXCLUDED.email,
                created_at = EXCLUDED.created_at,
                verified_at = EXCLUDED.verified_at
            ",
            &[
                &config.vault_id,
                &method_str,
                &config.enabled,
                &config.secret,
                &config.phone,
                &config.email,
                &config.created_at,
                &config.verified_at,
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn get_2fa_config(&self, vault_id: &str) -> Result<Option<TwoFactorConfig>, DbError> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                r"
            SELECT vault_id, method, enabled, secret, phone, email, created_at, verified_at
            FROM two_factor_config
            WHERE vault_id = $1
            ",
                &[&vault_id],
            )
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let method_str: String = row.try_get(1)?;
        let method: TwoFactorMethod =
            serde_json::from_str(&method_str).map_err(|e| DbError::Conversion(e.to_string()))?;

        Ok(Some(TwoFactorConfig {
            vault_id: row.try_get(0)?,
            method,
            enabled: row.try_get(2)?,
            secret: row.try_get(3)?,
            phone: row.try_get(4)?,
            email: row.try_get(5)?,
            created_at: row.try_get(6)?,
            verified_at: row.try_get(7)?,
        }))
    }

    pub async fn delete_2fa_config(&self, vault_id: &str) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.execute(
            "DELETE FROM two_factor_config WHERE vault_id = $1",
            &[&vault_id],
        )
        .await?;
        Ok(())
    }

    // ── Audit Log persistence (#961) ─────────────────────────────────────────

    pub async fn insert_audit_log(&self, entry: &AuditLogEntry) -> Result<(), DbError> {
        let conn = self.pool.get().await?;
        conn.execute(
            r"
            INSERT INTO audit_logs (timestamp, user_id, action, resource, result, ip_address, details)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ",
            &[
                &entry.timestamp,
                &entry.user_id,
                &entry.action,
                &entry.resource,
                &entry.result,
                &entry.ip_address,
                &entry.details,
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn query_audit_logs(&self, query: &AuditLogQuery) -> Result<Vec<AuditLogEntry>, DbError> {
        use std::fmt::Write as _;

        let conn = self.pool.get().await?;

        let mut sql = String::from(
            "SELECT id, timestamp, user_id, action, resource, result, ip_address, details FROM audit_logs WHERE 1=1"
        );
        let mut param_values: Vec<Box<dyn tokio_postgres::types::ToSql + Sync>> = Vec::new();
        let mut idx: usize = 1;

        if let Some(ref user_id) = query.user_id {
            let _ = write!(sql, " AND user_id = ${idx}");
            param_values.push(Box::new(user_id.clone()));
            idx += 1;
        }
        if let Some(ref action) = query.action {
            let _ = write!(sql, " AND action = ${idx}");
            param_values.push(Box::new(action.clone()));
            idx += 1;
        }
        if let Some(ref resource) = query.resource {
            let _ = write!(sql, " AND resource = ${idx}");
            param_values.push(Box::new(resource.clone()));
            idx += 1;
        }
        if let Some(ref result_val) = query.result {
            let _ = write!(sql, " AND result = ${idx}");
            param_values.push(Box::new(result_val.clone()));
            idx += 1;
        }
        if let Some(after) = query.after {
            let _ = write!(sql, " AND timestamp >= ${idx}");
            param_values.push(Box::new(after));
            idx += 1;
        }
        if let Some(before) = query.before {
            let _ = write!(sql, " AND timestamp <= ${idx}");
            param_values.push(Box::new(before));
            idx += 1;
        }

        sql.push_str(" ORDER BY timestamp DESC");

        let limit = query.limit.unwrap_or(100);
        let offset = query.offset.unwrap_or(0);
        let _ = write!(sql, " LIMIT ${idx} OFFSET ${}", idx + 1);
        param_values.push(Box::new(limit));
        param_values.push(Box::new(offset));

        let params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = param_values
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect();

        let rows = conn.query(&sql, params.as_slice()).await?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(AuditLogEntry {
                id: row.try_get(0)?,
                timestamp: row.try_get(1)?,
                user_id: row.try_get(2)?,
                action: row.try_get(3)?,
                resource: row.try_get(4)?,
                result: row.try_get(5)?,
                ip_address: row.try_get(6)?,
                details: row.try_get(7)?,
            });
        }
        Ok(out)
    }

    pub async fn purge_old_audit_logs(&self, retention_days: i64) -> Result<u64, DbError> {
        let cutoff = Utc::now() - chrono::Duration::days(retention_days);
        let conn = self.pool.get().await?;
        let count = conn
            .execute("DELETE FROM audit_logs WHERE timestamp < $1", &[&cutoff])
            .await?;
        Ok(count)
    }
}

// ── Cache-aware vault accessors ───────────────────────────────────────────────

use crate::cache::VaultCache;
use crate::models::VaultSummary;

/// Retrieve a `Vault` from the in-memory store, consulting the cache first.
///
/// On a cache miss the vault is fetched from `store`, inserted into the cache
/// and then returned. Returns `None` if the vault does not exist in the store.
pub fn get_vault_cached(
    store: &VaultStore,
    cache: &VaultCache,
    vault_id: &str,
) -> Option<crate::models::Vault> {
    if let Some(v) = cache.get_vault(vault_id) {
        return Some(v);
    }
    let vault = store.lock().unwrap().get(vault_id).cloned()?;
    cache.set_vault(vault_id, vault.clone());
    Some(vault)
}

/// Retrieve the TTL-remaining value for a vault, consulting the cache first.
///
/// Returns `None` if the vault does not exist in the store. The nested
/// `Option` mirrors `VaultCache::get_ttl_remaining` (see its doc comment).
#[allow(clippy::option_option)]
pub fn get_ttl_remaining_cached(
    store: &VaultStore,
    cache: &VaultCache,
    vault_id: &str,
) -> Option<Option<u64>> {
    if let Some(ttl) = cache.get_ttl_remaining(vault_id) {
        return Some(ttl);
    }
    let vault = store.lock().unwrap().get(vault_id).cloned()?;
    let ttl = vault.ttl_remaining;
    cache.set_ttl_remaining(vault_id, ttl);
    Some(ttl)
}

/// Retrieve a lightweight `VaultSummary` for a vault, consulting the cache
/// first.
///
/// Returns `None` if the vault does not exist in the store.
pub fn get_vault_summary_cached(
    store: &VaultStore,
    cache: &VaultCache,
    vault_id: &str,
) -> Option<VaultSummary> {
    if let Some(s) = cache.get_vault_summary(vault_id) {
        return Some(s);
    }
    let vault = store.lock().unwrap().get(vault_id).cloned()?;
    let summary = VaultSummary::from(&vault);
    cache.set_vault_summary(vault_id, summary.clone());
    Some(summary)
}

/// Invalidate all cached entries for `vault_id`.  Must be called whenever
/// a check-in or state-change event modifies vault state.
pub fn invalidate_vault_cache(cache: &VaultCache, vault_id: &str) {
    cache.invalidate(vault_id);
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_search_vaults_by_owner() {
        let store = create_vault_store();
        let vault = Vault {
            id: "v1".to_string(),
            owner: "owner1".to_string(),
            beneficiary: "ben1".to_string(),
            balance: 1000,
            check_in_interval: 86400,
            last_check_in: Utc::now(),
            created_at: Utc::now(),
            status: VaultStatus::Active,
            ttl_remaining: Some(100_000),
        };
        store.lock().unwrap().insert("v1".to_string(), vault);

        let query = SearchQuery {
            owner: Some("owner1".to_string()),
            beneficiary: None,
            status: None,
            created_after: None,
            created_before: None,
            page: None,
            limit: None,
        };

        let result = search_vaults(&store, &query);
        assert_eq!(result.vaults.len(), 1);
        assert_eq!(result.total, 1);
    }

    #[test]
    fn test_search_vaults_pagination() {
        let store = create_vault_store();
        for i in 0..25 {
            let vault = Vault {
                id: format!("v{i}"),
                owner: "owner1".to_string(),
                beneficiary: "ben1".to_string(),
                balance: 1000,
                check_in_interval: 86400,
                last_check_in: Utc::now(),
                created_at: Utc::now(),
                status: VaultStatus::Active,
                ttl_remaining: Some(100_000),
            };
            store.lock().unwrap().insert(format!("v{i}"), vault);
        }

        let query = SearchQuery {
            owner: Some("owner1".to_string()),
            beneficiary: None,
            status: None,
            created_after: None,
            created_before: None,
            page: Some(2),
            limit: Some(10),
        };

        let result = search_vaults(&store, &query);
        assert_eq!(result.vaults.len(), 10);
        assert_eq!(result.total, 25);
        assert_eq!(result.page, 2);
    }
}
