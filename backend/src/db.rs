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
    /// Webhook registry and HTTP delivery client (#65).
    pub webhook_state: Arc<crate::webhook::WebhookState>,
    /// GraphQL schema for the /graphql endpoint (#66).
    pub graphql_schema: crate::graphql::EthosSchema,
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

impl axum::extract::FromRef<AppState> for Arc<crate::webhook::WebhookState> {
    fn from_ref(state: &AppState) -> Arc<crate::webhook::WebhookState> {
        Arc::clone(&state.webhook_state)
    }
}

impl axum::extract::FromRef<AppState> for crate::graphql::EthosSchema {
    fn from_ref(state: &AppState) -> crate::graphql::EthosSchema {
        state.graphql_schema.clone()
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

// ── TTL Insurance persistence (SQLite) ───────────────────────────────────────

use crate::models::TtlInsurancePolicy;

impl Db {
    pub fn upsert_insurance_policy(
        &self,
        policy: &TtlInsurancePolicy,
    ) -> Result<(), rusqlite::Error> {
        // Store DateTimes as RFC3339 strings.
        let purchased_at = policy.purchased_at.to_rfc3339();
        let last_extended_at = policy.last_extended_at.map(|d| d.to_rfc3339());

        let enabled_i = i64::from(policy.enabled);

        self.conn.lock().unwrap().execute(
            r"
            INSERT INTO ttl_insurance_policies (
                vault_id,
                extension_seconds,
                inactivity_threshold_seconds,
                enabled,
                purchased_at,
                last_extended_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(vault_id) DO UPDATE SET
                extension_seconds = excluded.extension_seconds,
                inactivity_threshold_seconds = excluded.inactivity_threshold_seconds,
                enabled = excluded.enabled,
                purchased_at = excluded.purchased_at,
                last_extended_at = excluded.last_extended_at
            ",
            params![
                policy.vault_id.cast_signed(),
                policy.extension_seconds.cast_signed(),
                policy.inactivity_threshold_seconds.cast_signed(),
                enabled_i,
                purchased_at,
                last_extended_at,
            ],
        )?;

        Ok(())
    }

    pub fn get_insurance_policy(
        &self,
        vault_id: u64,
    ) -> Result<Option<TtlInsurancePolicy>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"
            SELECT vault_id, extension_seconds, inactivity_threshold_seconds, enabled, purchased_at, last_extended_at
            FROM ttl_insurance_policies
            WHERE vault_id = ?1
            ",
        )?;

        let row_res = stmt.query_row(params![vault_id.cast_signed()], |r| {
            let purchased_at_str: String = r.get(4)?;
            let purchased_at = chrono::DateTime::parse_from_rfc3339(&purchased_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

            let last_extended_at: Option<String> = r.get(5)?;
            let last_extended_at_dt = match last_extended_at {
                Some(s) => {
                    let dt = chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    Some(dt)
                }
                None => None,
            };

            let enabled_i: i64 = r.get(3)?;

            Ok(TtlInsurancePolicy {
                vault_id: r.get::<_, i64>(0)? as u64,
                extension_seconds: r.get::<_, i64>(1)? as u64,
                inactivity_threshold_seconds: r.get::<_, i64>(2)? as u64,
                enabled: enabled_i != 0,
                purchased_at,
                last_extended_at: last_extended_at_dt,
            })
        });

        match row_res {
            Ok(p) => Ok(Some(p)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn upsert_owner_activity(
        &self,
        owner_id: u64,
        last_active_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"
            INSERT INTO owner_activity (owner_id, last_active_at)
            VALUES (?1, ?2)
            ON CONFLICT(owner_id) DO UPDATE SET
                last_active_at = excluded.last_active_at
            ",
            params![owner_id.cast_signed(), last_active_at.to_rfc3339(),],
        )?;
        Ok(())
    }

    pub fn get_owner_last_active_at(
        &self,
        owner_id: u64,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"
            SELECT last_active_at
            FROM owner_activity
            WHERE owner_id = ?1
            ",
        )?;

        let row_res: Result<String, rusqlite::Error> =
            stmt.query_row(params![owner_id.cast_signed()], |r| r.get(0));

        match row_res {
            Ok(s) => {
                let dt = chrono::DateTime::parse_from_rfc3339(&s)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                Ok(Some(dt))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn all_enabled_insurance_policies(
        &self,
    ) -> Result<Vec<TtlInsurancePolicy>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"
            SELECT vault_id, extension_seconds, inactivity_threshold_seconds, enabled, purchased_at, last_extended_at
            FROM ttl_insurance_policies
            WHERE enabled = 1
            ",
        )?;

        let iter = stmt.query_map([], |r| {
            let purchased_at_str: String = r.get(4)?;
            let purchased_at = chrono::DateTime::parse_from_rfc3339(&purchased_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;

            let last_extended_at: Option<String> = r.get(5)?;
            let last_extended_at_dt = match last_extended_at {
                Some(s) => {
                    let dt = chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(e),
                            )
                        })?;
                    Some(dt)
                }
                None => None,
            };

            let enabled_i: i64 = r.get(3)?;

            Ok(TtlInsurancePolicy {
                vault_id: r.get::<_, i64>(0)? as u64,
                extension_seconds: r.get::<_, i64>(1)? as u64,
                inactivity_threshold_seconds: r.get::<_, i64>(2)? as u64,
                enabled: enabled_i != 0,
                purchased_at,
                last_extended_at: last_extended_at_dt,
            })
        })?;

        let mut out = Vec::new();
        for item in iter {
            out.push(item?);
        }
        Ok(out)
    }
}

use rusqlite::{params, Connection};

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
    conn: std::sync::Mutex<Connection>,
    // DB_POOL_MIN/DB_POOL_MAX are accepted for forward compatibility but unused:
    // `conn` is a single mutex-guarded connection, not a real pool. Only
    // `timeout_secs` (DB_POOL_TIMEOUT_SECS) is currently applied, via busy_timeout.
    #[allow(dead_code)]
    pool_config: PoolConfig,
    /// In-memory vault store shared across the application.
    pub vault_store: VaultStore,
}

impl Db {
    pub fn open(path: &str) -> Result<Self, rusqlite::Error> {
        Self::open_with_pool_config(path, &PoolConfig::default())
    }

    pub fn open_with_pool_config(path: &str, config: &PoolConfig) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(config.timeout_secs as u64))?;
        Ok(Self {
            conn: std::sync::Mutex::new(conn),
            pool_config: PoolConfig {
                min: config.min,
                max: config.max,
                timeout_secs: config.timeout_secs,
            },
            vault_store: create_vault_store(),
        })
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

    pub fn check_connectivity(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("SELECT 1")?;
        Ok(())
    }

    pub fn migrate(&self) -> Result<(), rusqlite::Error> {
        // Bootstrap the migration tracking table before anything else.
        self.conn.lock().unwrap().execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                version    TEXT PRIMARY KEY,
                applied_at TEXT NOT NULL
            );",
        )?;

        const MIGRATIONS: &[(&str, &str)] = &[
            (
                "1",
                r"
                CREATE TABLE IF NOT EXISTS reminder_preferences (
                    vault_id             INTEGER PRIMARY KEY,
                    channels             TEXT NOT NULL,
                    hours_before_expiry  INTEGER NOT NULL,
                    frequency            TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS ttl_insurance_policies (
                    vault_id                      INTEGER PRIMARY KEY,
                    extension_seconds             INTEGER NOT NULL,
                    inactivity_threshold_seconds  INTEGER NOT NULL,
                    enabled                        INTEGER NOT NULL,
                    purchased_at                   TEXT NOT NULL,
                    last_extended_at               TEXT
                );
                CREATE TABLE IF NOT EXISTS owner_activity (
                    owner_id       INTEGER PRIMARY KEY,
                    last_active_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS idempotency_keys (
                    key           TEXT PRIMARY KEY,
                    status_code   INTEGER NOT NULL,
                    response_body TEXT NOT NULL,
                    created_at    TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS unsubscribe_tokens (
                    token      TEXT PRIMARY KEY,
                    owner      TEXT NOT NULL,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS unsubscribed_users (
                    owner TEXT PRIMARY KEY
                );
                ",
            ),
            (
                "2",
                "ALTER TABLE reminder_preferences ADD COLUMN deleted_at TEXT;",
            ),
            (
                "3",
                r"
                CREATE TABLE IF NOT EXISTS audit_logs (
                    id         INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp  TEXT NOT NULL,
                    user_id    TEXT NOT NULL DEFAULT '',
                    action     TEXT NOT NULL,
                    resource   TEXT NOT NULL DEFAULT '',
                    result     TEXT NOT NULL DEFAULT 'success',
                    ip_address TEXT NOT NULL DEFAULT '',
                    details    TEXT
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
                    enabled      INTEGER NOT NULL DEFAULT 0,
                    secret       TEXT,
                    phone        TEXT,
                    email        TEXT,
                    created_at   TEXT NOT NULL,
                    verified_at  TEXT
                );
                ",
            ),
            (
                "5",
                r"
                CREATE TABLE IF NOT EXISTS vault_subscriptions (
                    vault_id   INTEGER PRIMARY KEY,
                    owner      TEXT NOT NULL,
                    channels   TEXT NOT NULL,
                    frequency  TEXT NOT NULL
                );
                ",
            ),
            (
                "6",
                r"
                CREATE TABLE IF NOT EXISTS tenants (
                    id          TEXT PRIMARY KEY,
                    name        TEXT NOT NULL,
                    owner       TEXT NOT NULL,
                    created_at  TEXT NOT NULL,
                    updated_at  TEXT NOT NULL,
                    is_active   INTEGER NOT NULL DEFAULT 1
                );
                CREATE TABLE IF NOT EXISTS tenant_billing (
                    tenant_id            TEXT PRIMARY KEY,
                    monthly_charge       INTEGER NOT NULL,
                    billing_cycle_start  TEXT NOT NULL,
                    billing_cycle_end    TEXT NOT NULL,
                    total_vaults         INTEGER NOT NULL,
                    status               TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS tenant_vaults (
                    tenant_id   TEXT NOT NULL,
                    vault_id    TEXT NOT NULL,
                    PRIMARY KEY (tenant_id, vault_id)
                );
                CREATE INDEX IF NOT EXISTS idx_tenant_vaults_vault_id ON tenant_vaults(vault_id);
                ",
            ),
            (
                "7",
                r"
                CREATE TABLE IF NOT EXISTS credential_updates (
                    id              TEXT PRIMARY KEY,
                    vault_id        TEXT NOT NULL,
                    user_id         TEXT NOT NULL,
                    field           TEXT NOT NULL,
                    old_value       TEXT,
                    new_value       TEXT,
                    timestamp       TEXT NOT NULL,
                    operation_id    TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS operational_transforms (
                    id              TEXT PRIMARY KEY,
                    vault_id        TEXT NOT NULL,
                    user_id         TEXT NOT NULL,
                    operation       TEXT NOT NULL,
                    position        INTEGER NOT NULL,
                    content         TEXT NOT NULL,
                    timestamp       TEXT NOT NULL,
                    version         INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS conflict_resolutions (
                    conflict_id         TEXT PRIMARY KEY,
                    vault_id            TEXT NOT NULL,
                    update1_id          TEXT NOT NULL,
                    update2_id          TEXT NOT NULL,
                    resolution_strategy TEXT NOT NULL,
                    resolved_at         TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS user_presence (
                    user_id     TEXT NOT NULL,
                    vault_id    TEXT NOT NULL,
                    status      TEXT NOT NULL,
                    last_seen   TEXT NOT NULL,
                    session_id  TEXT NOT NULL,
                    PRIMARY KEY (user_id, vault_id)
                );
                CREATE TABLE IF NOT EXISTS collaborative_sessions (
                    session_id  TEXT PRIMARY KEY,
                    vault_id    TEXT NOT NULL,
                    created_at  TEXT NOT NULL,
                    participants TEXT NOT NULL,
                    is_active   INTEGER NOT NULL DEFAULT 1
                );
                CREATE INDEX IF NOT EXISTS idx_operational_transforms_vault_id ON operational_transforms(vault_id);
                CREATE INDEX IF NOT EXISTS idx_credential_updates_vault_id ON credential_updates(vault_id);
                CREATE INDEX IF NOT EXISTS idx_collaborative_sessions_vault_id ON collaborative_sessions(vault_id);
                ",
            ),
            (
                "8",
                r"
                CREATE TABLE IF NOT EXISTS full_text_search_index (
                    id          TEXT PRIMARY KEY,
                    vault_id    TEXT NOT NULL,
                    title       TEXT NOT NULL,
                    content     TEXT NOT NULL,
                    created_at  TEXT NOT NULL,
                    indexed_at  TEXT NOT NULL
                );
                CREATE VIRTUAL TABLE IF NOT EXISTS vault_search_fts USING fts5(
                    vault_id,
                    title,
                    content,
                    content=full_text_search_index,
                    content_rowid=rowid
                );
                CREATE TABLE IF NOT EXISTS search_facets (
                    vault_id    TEXT NOT NULL,
                    facet_name  TEXT NOT NULL,
                    value       TEXT NOT NULL,
                    count       INTEGER NOT NULL,
                    PRIMARY KEY (vault_id, facet_name, value)
                );
                ",
            ),
            (
                "9",
                r"
                CREATE TABLE IF NOT EXISTS idempotency_keys_cleanup (
                    key           TEXT PRIMARY KEY,
                    status_code   INTEGER NOT NULL,
                    response_body TEXT NOT NULL,
                    created_at    TEXT NOT NULL,
                    expires_at    TEXT NOT NULL
                );
                ",
            ),
            (
                // Data-transformation migration (not just a schema change):
                // adds a column *and* backfills it from existing row data.
                "10",
                r"
                ALTER TABLE reminder_preferences ADD COLUMN normalized_frequency TEXT;
                UPDATE reminder_preferences SET normalized_frequency = UPPER(frequency);
                ",
            ),
        ];

        for (version, sql) in MIGRATIONS {
            let already_applied: bool = {
                let conn = self.conn.lock().unwrap();
                conn.query_row(
                    "SELECT 1 FROM schema_migrations WHERE version = ?1",
                    params![version],
                    |_| Ok(true),
                )
                .unwrap_or(false)
            };

            if already_applied {
                tracing::debug!(version = version, "migration already applied, skipping");
            } else {
                tracing::info!(version = version, "applying migration");
                self.conn.lock().unwrap().execute_batch(sql)?;
                self.conn.lock().unwrap().execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, chrono::Utc::now().to_rfc3339()],
                )?;
                tracing::info!(version = version, "migration applied successfully");
            }
        }

        Ok(())
    }

    /// Roll back a single applied migration by its version, applying the
    /// reverse SQL registered in `DOWN_MIGRATIONS` and removing the version
    /// from `schema_migrations` so `migrate()` will re-apply it if called
    /// again. See docs/migration-testing.md for the rollback-testing policy
    /// this supports.
    pub fn rollback(&self, version: &str) -> Result<(), rusqlite::Error> {
        const DOWN_MIGRATIONS: &[(&str, &str)] = &[
            (
                "1",
                r"
                DROP TABLE IF EXISTS unsubscribed_users;
                DROP TABLE IF EXISTS unsubscribe_tokens;
                DROP TABLE IF EXISTS idempotency_keys;
                DROP TABLE IF EXISTS owner_activity;
                DROP TABLE IF EXISTS ttl_insurance_policies;
                DROP TABLE IF EXISTS reminder_preferences;
                ",
            ),
            ("2", "ALTER TABLE reminder_preferences DROP COLUMN deleted_at;"),
            (
                "3",
                r"
                DROP INDEX IF EXISTS idx_audit_logs_action;
                DROP INDEX IF EXISTS idx_audit_logs_user_id;
                DROP INDEX IF EXISTS idx_audit_logs_timestamp;
                DROP TABLE IF EXISTS audit_logs;
                ",
            ),
            ("4", "DROP TABLE IF EXISTS two_factor_config;"),
            ("5", "DROP TABLE IF EXISTS vault_subscriptions;"),
            (
                "6",
                r"
                DROP INDEX IF EXISTS idx_tenant_vaults_vault_id;
                DROP TABLE IF EXISTS tenant_vaults;
                DROP TABLE IF EXISTS tenant_billing;
                DROP TABLE IF EXISTS tenants;
                ",
            ),
            (
                "7",
                r"
                DROP INDEX IF EXISTS idx_collaborative_sessions_vault_id;
                DROP INDEX IF EXISTS idx_credential_updates_vault_id;
                DROP INDEX IF EXISTS idx_operational_transforms_vault_id;
                DROP TABLE IF EXISTS collaborative_sessions;
                DROP TABLE IF EXISTS user_presence;
                DROP TABLE IF EXISTS conflict_resolutions;
                DROP TABLE IF EXISTS operational_transforms;
                DROP TABLE IF EXISTS credential_updates;
                ",
            ),
            (
                "8",
                r"
                DROP TABLE IF EXISTS vault_search_fts;
                DROP TABLE IF EXISTS full_text_search_index;
                DROP TABLE IF EXISTS search_facets;
                ",
            ),
            ("9", "DROP TABLE IF EXISTS idempotency_keys_cleanup;"),
            (
                "10",
                "ALTER TABLE reminder_preferences DROP COLUMN normalized_frequency;",
            ),
        ];

        let down_sql = DOWN_MIGRATIONS
            .iter()
            .find(|(v, _)| *v == version)
            .map(|(_, sql)| *sql)
            .unwrap_or_else(|| panic!("no down migration registered for version {version}"));

        tracing::info!(version = version, "rolling back migration");
        self.conn.lock().unwrap().execute_batch(down_sql)?;
        self.conn.lock().unwrap().execute(
            "DELETE FROM schema_migrations WHERE version = ?1",
            params![version],
        )?;
        tracing::info!(version = version, "migration rolled back successfully");
        Ok(())
    }

    /// Returns the sorted list of user table/index names currently in the
    /// database (excluding sqlite-internal objects). Used by migration
    /// rollback tests to compare schema snapshots before/after a
    /// rollback + re-apply cycle.
    #[cfg(test)]
    pub(crate) fn schema_object_names(&self) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master \
                 WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Returns `(column_name, column_type)` pairs for `table`, ordered by
    /// column position. Used by migration rollback tests to compare table
    /// schemas before/after a rollback + re-apply cycle.
    #[cfg(test)]
    pub(crate) fn table_columns(&self, table: &str) -> Vec<(String, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    pub fn upsert(&self, prefs: &ReminderPreferences) -> Result<(), rusqlite::Error> {
        let channels_json = serde_json::to_string(&prefs.channels).unwrap();
        self.conn.lock().unwrap().execute(
            r"
            INSERT INTO reminder_preferences (vault_id, channels, hours_before_expiry, frequency)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(vault_id) DO UPDATE SET
              channels = excluded.channels,
              hours_before_expiry = excluded.hours_before_expiry,
              frequency = excluded.frequency,
              deleted_at = NULL
            ",
            params![
                prefs.vault_id.cast_signed(),
                channels_json,
                prefs.hours_before_expiry as i64,
                serde_json::to_string(&prefs.frequency).unwrap(),
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, vault_id: u64) -> Result<ReminderPreferences, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"SELECT vault_id, channels, hours_before_expiry, frequency, deleted_at
               FROM reminder_preferences
               WHERE vault_id = ?1 AND deleted_at IS NULL",
        )?;
        let row = stmt.query_row(params![vault_id.cast_signed()], |r| {
            let channels_str: String = r.get(1)?;
            let frequency_str: String = r.get(3)?;
            let channels: Vec<Channel> = serde_json::from_str(&channels_str).unwrap_or_default();
            let frequency: Frequency = serde_json::from_str(&frequency_str).unwrap();
            Ok(ReminderPreferences {
                vault_id: r.get::<_, i64>(0)? as u64,
                channels,
                hours_before_expiry: r.get::<_, i64>(2)? as u32,
                frequency,
                deleted_at: None,
            })
        })?;
        Ok(row)
    }

    /// Reads the raw `normalized_frequency` column for `vault_id`. Only used
    /// by migration rollback tests to verify the data-transformation
    /// migration (version "10") correctly backfills this column.
    #[cfg(test)]
    pub(crate) fn get_normalized_frequency(
        &self,
        vault_id: u64,
    ) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT normalized_frequency FROM reminder_preferences WHERE vault_id = ?1",
            params![vault_id.cast_signed()],
            |r| r.get(0),
        )
    }

    pub fn all(&self) -> Result<Vec<ReminderPreferences>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"SELECT vault_id, channels, hours_before_expiry, frequency, deleted_at
               FROM reminder_preferences
               WHERE deleted_at IS NULL",
        )?;
        let iter = stmt.query_map([], |r| {
            let channels_str: String = r.get(1)?;
            let frequency_str: String = r.get(3)?;
            let channels: Vec<Channel> = serde_json::from_str(&channels_str).unwrap_or_default();
            let frequency: Frequency = serde_json::from_str(&frequency_str).unwrap();
            Ok(ReminderPreferences {
                vault_id: r.get::<_, i64>(0)? as u64,
                channels,
                hours_before_expiry: r.get::<_, i64>(2)? as u32,
                frequency,
                deleted_at: None,
            })
        })?;

        let mut out = Vec::new();
        for item in iter {
            out.push(item?);
        }
        Ok(out)
    }

    pub fn soft_delete_reminder(&self, vault_id: u64) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            "UPDATE reminder_preferences SET deleted_at = ?1 WHERE vault_id = ?2 AND deleted_at IS NULL",
            params![chrono::Utc::now().to_rfc3339(), vault_id.cast_signed()],
        )?;
        Ok(())
    }

    pub fn all_reminders_including_deleted(
        &self,
        vault_id: u64,
    ) -> Result<Vec<ReminderPreferences>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"SELECT vault_id, channels, hours_before_expiry, frequency, deleted_at
               FROM reminder_preferences
               WHERE vault_id = ?1",
        )?;
        let iter = stmt.query_map(params![vault_id.cast_signed()], |r| {
            let channels_str: String = r.get(1)?;
            let frequency_str: String = r.get(3)?;
            let channels: Vec<Channel> = serde_json::from_str(&channels_str).unwrap_or_default();
            let frequency: Frequency = serde_json::from_str(&frequency_str).unwrap();
            let deleted_at_str: Option<String> = r.get(4)?;
            let deleted_at = deleted_at_str.and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            });
            Ok(ReminderPreferences {
                vault_id: r.get::<_, i64>(0)? as u64,
                channels,
                hours_before_expiry: r.get::<_, i64>(2)? as u32,
                frequency,
                deleted_at,
            })
        })?;

        let mut out = Vec::new();
        for item in iter {
            out.push(item?);
        }
        Ok(out)
    }

    pub fn upsert_subscription(&self, sub: &Subscription) -> Result<(), rusqlite::Error> {
        let channels_json = serde_json::to_string(&sub.channels).unwrap();
        let frequency_json = serde_json::to_string(&sub.frequency).unwrap();
        self.conn.lock().unwrap().execute(
            r"
            INSERT INTO vault_subscriptions (vault_id, owner, channels, frequency)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(vault_id) DO UPDATE SET
              owner = excluded.owner,
              channels = excluded.channels,
              frequency = excluded.frequency
            ",
            params![
                sub.vault_id.cast_signed(),
                sub.owner,
                channels_json,
                frequency_json,
            ],
        )?;
        Ok(())
    }

    pub fn delete_subscription(&self, vault_id: u64) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM vault_subscriptions WHERE vault_id = ?1",
            params![vault_id.cast_signed()],
        )?;
        Ok(())
    }

    pub fn get_subscription(&self, vault_id: u64) -> Result<Option<Subscription>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"SELECT vault_id, owner, channels, frequency
               FROM vault_subscriptions
               WHERE vault_id = ?1",
        )?;
        let row = stmt.query_row(params![vault_id.cast_signed()], |r| {
            let channels_str: String = r.get(2)?;
            let frequency_str: String = r.get(3)?;
            let channels: Vec<SubscriptionChannel> =
                serde_json::from_str(&channels_str).unwrap_or_default();
            let frequency: SubscriptionFrequency = serde_json::from_str(&frequency_str).unwrap();
            Ok(Subscription {
                vault_id: r.get::<_, i64>(0)? as u64,
                owner: r.get(1)?,
                channels,
                frequency,
            })
        });
        match row {
            Ok(sub) => Ok(Some(sub)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // ── Idempotency (#825) ──────────────────────────────────────────────────

    pub fn store_idempotency(&self, key: &str, status_code: u16, response_body: &str) {
        let _ = self.conn.lock().unwrap().execute(
            r"INSERT OR REPLACE INTO idempotency_keys (key, status_code, response_body, created_at)
               VALUES (?1, ?2, ?3, ?4)",
            params![
                key,
                status_code as i64,
                response_body,
                chrono::Utc::now().to_rfc3339()
            ],
        );
    }

    pub fn check_idempotency(&self, key: &str) -> Option<crate::models::IdempotencyRecord> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding
            .prepare("SELECT key, status_code, response_body, created_at FROM idempotency_keys WHERE key = ?1")
            .ok()?;
        stmt.query_row(params![key], |r| {
            let created_str: String = r.get(3)?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
            let age = chrono::Utc::now()
                .signed_duration_since(created_at)
                .num_seconds();
            if age > 86_400 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            Ok(crate::models::IdempotencyRecord {
                key: r.get(0)?,
                status_code: r.get::<_, i64>(1)? as u16,
                response_body: r.get(2)?,
                created_at,
            })
        })
        .ok()
    }

    // ── Unsubscribe (#828) ──────────────────────────────────────────────────

    pub fn store_unsubscribe_token(&self, token: &str, owner: &str) {
        let _ = self.conn.lock().unwrap().execute(
            r"INSERT OR REPLACE INTO unsubscribe_tokens (token, owner, created_at)
               VALUES (?1, ?2, ?3)",
            params![token, owner, chrono::Utc::now().to_rfc3339()],
        );
    }

    pub fn process_unsubscribe(&self, token: &str) -> Result<String, String> {
        let conn = self.conn.lock().unwrap();
        let owner: String = conn
            .query_row(
                "SELECT owner FROM unsubscribe_tokens WHERE token = ?1",
                params![token],
                |r| r.get(0),
            )
            .map_err(|_| "invalid or expired unsubscribe token".to_string())?;

        conn.execute(
            "INSERT OR IGNORE INTO unsubscribed_users (owner) VALUES (?1)",
            params![&owner],
        )
        .map_err(|e| e.to_string())?;

        Ok(owner)
    }

    pub fn is_unsubscribed(&self, owner: &str) -> bool {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM unsubscribed_users WHERE owner = ?1",
                params![owner],
                |_| Ok(()),
            )
            .is_ok()
    }

    pub fn generate_unsubscribe_token(&self, owner: &str) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.store_unsubscribe_token(&token, owner);
        token
    }

    // ── 2FA operations (#965) ───────────────────────────────────────────────

    pub fn upsert_2fa_config(&self, config: &TwoFactorConfig) -> Result<(), rusqlite::Error> {
        let enabled_i = i64::from(config.enabled);
        let verified_at = config.verified_at.map(|d| d.to_rfc3339());
        let method_str = serde_json::to_string(&config.method).unwrap();

        self.conn.lock().unwrap().execute(
            r"
            INSERT INTO two_factor_config (vault_id, method, enabled, secret, phone, email, created_at, verified_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(vault_id) DO UPDATE SET
                method = excluded.method,
                enabled = excluded.enabled,
                secret = excluded.secret,
                phone = excluded.phone,
                email = excluded.email,
                created_at = excluded.created_at,
                verified_at = excluded.verified_at
            ",
            params![
                config.vault_id,
                method_str,
                enabled_i,
                config.secret,
                config.phone,
                config.email,
                config.created_at.to_rfc3339(),
                verified_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_2fa_config(
        &self,
        vault_id: &str,
    ) -> Result<Option<TwoFactorConfig>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"
            SELECT vault_id, method, enabled, secret, phone, email, created_at, verified_at
            FROM two_factor_config
            WHERE vault_id = ?1
            ",
        )?;

        let row_res = stmt.query_row(params![vault_id], |r| {
            let method_str: String = r.get(1)?;
            let method: TwoFactorMethod = serde_json::from_str(&method_str).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            let enabled_i: i64 = r.get(2)?;
            let created_at_str: String = r.get(6)?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
            let verified_at_str: Option<String> = r.get(7)?;
            let verified_at = verified_at_str.and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(&s)
                    .ok()
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            });

            Ok(TwoFactorConfig {
                vault_id: r.get(0)?,
                method,
                enabled: enabled_i != 0,
                secret: r.get(3)?,
                phone: r.get(4)?,
                email: r.get(5)?,
                created_at,
                verified_at,
            })
        });

        match row_res {
            Ok(c) => Ok(Some(c)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn delete_2fa_config(&self, vault_id: &str) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM two_factor_config WHERE vault_id = ?1",
            params![vault_id],
        )?;
        Ok(())
    }

    // ── Audit Log persistence (#961) ─────────────────────────────────────────

    pub fn insert_audit_log(&self, entry: &AuditLogEntry) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"
            INSERT INTO audit_logs (timestamp, user_id, action, resource, result, ip_address, details)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
            params![
                entry.timestamp.to_rfc3339(),
                entry.user_id,
                entry.action,
                entry.resource,
                entry.result,
                entry.ip_address,
                entry.details.as_ref().map(std::string::ToString::to_string),
            ],
        )?;
        Ok(())
    }

    pub fn query_audit_logs(
        &self,
        query: &AuditLogQuery,
    ) -> Result<Vec<AuditLogEntry>, rusqlite::Error> {
        let conn = self.conn.lock().unwrap();

        let mut sql = String::from(
            "SELECT id, timestamp, user_id, action, resource, result, ip_address, details FROM audit_logs WHERE 1=1"
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref user_id) = query.user_id {
            sql.push_str(" AND user_id = ?");
            param_values.push(Box::new(user_id.clone()));
        }
        if let Some(ref action) = query.action {
            sql.push_str(" AND action = ?");
            param_values.push(Box::new(action.clone()));
        }
        if let Some(ref resource) = query.resource {
            sql.push_str(" AND resource = ?");
            param_values.push(Box::new(resource.clone()));
        }
        if let Some(ref result_val) = query.result {
            sql.push_str(" AND result = ?");
            param_values.push(Box::new(result_val.clone()));
        }
        if let Some(after) = query.after {
            sql.push_str(" AND timestamp >= ?");
            param_values.push(Box::new(after.to_rfc3339()));
        }
        if let Some(before) = query.before {
            sql.push_str(" AND timestamp <= ?");
            param_values.push(Box::new(before.to_rfc3339()));
        }

        sql.push_str(" ORDER BY timestamp DESC");

        let limit = query.limit.unwrap_or(100);
        let offset = query.offset.unwrap_or(0);
        sql.push_str(" LIMIT ? OFFSET ?");
        param_values.push(Box::new(limit));
        param_values.push(Box::new(offset));

        let params: Vec<&dyn rusqlite::types::ToSql> = param_values
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect();
        let mut stmt = conn.prepare(&sql)?;

        let rows = stmt.query_map(params.as_slice(), |r| {
            let timestamp_str: String = r.get(1)?;
            let timestamp = chrono::DateTime::parse_from_rfc3339(&timestamp_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
            let details_str: Option<String> = r.get(7)?;
            let details = details_str.and_then(|s| serde_json::from_str(&s).ok());

            Ok(AuditLogEntry {
                id: r.get(0)?,
                timestamp,
                user_id: r.get(2)?,
                action: r.get(3)?,
                resource: r.get(4)?,
                result: r.get(5)?,
                ip_address: r.get(6)?,
                details,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn purge_old_audit_logs(&self, retention_days: i64) -> Result<u64, rusqlite::Error> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days)).to_rfc3339();
        let count = self.conn.lock().unwrap().execute(
            "DELETE FROM audit_logs WHERE timestamp < ?1",
            params![cutoff],
        )?;
        Ok(count as u64)
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

impl Db {
    // ── #68: Request Deduplication Cleanup ──────────────────────────────────

    pub fn cleanup_expired_idempotency_keys(&self) -> Result<u64, rusqlite::Error> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(1);
        let count = self.conn.lock().unwrap().execute(
            "DELETE FROM idempotency_keys WHERE created_at < ?1",
            params![cutoff.to_rfc3339()],
        )?;
        Ok(count as u64)
    }

    // ── #69: Multi-Tenancy Support ──────────────────────────────────────────

    pub fn create_tenant(&self, tenant: &crate::models::Tenant) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT INTO tenants (id, name, owner, created_at, updated_at, is_active)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                tenant.id,
                tenant.name,
                tenant.owner,
                tenant.created_at.to_rfc3339(),
                tenant.updated_at.to_rfc3339(),
                if tenant.is_active { 1 } else { 0 }
            ],
        )?;
        Ok(())
    }

    pub fn get_tenant(&self, tenant_id: &str) -> Result<Option<crate::models::Tenant>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            "SELECT id, name, owner, created_at, updated_at, is_active FROM tenants WHERE id = ?1",
        )?;

        match stmt.query_row(params![tenant_id], |r| {
            let created_at_str: String = r.get(3)?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?;
            let updated_at_str: String = r.get(4)?;
            let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
            let is_active_i: i64 = r.get(5)?;
            Ok(crate::models::Tenant {
                id: r.get(0)?,
                name: r.get(1)?,
                owner: r.get(2)?,
                created_at,
                updated_at,
                is_active: is_active_i != 0,
            })
        }) {
            Ok(tenant) => Ok(Some(tenant)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn add_vault_to_tenant(&self, tenant_id: &str, vault_id: &str) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            "INSERT OR IGNORE INTO tenant_vaults (tenant_id, vault_id) VALUES (?1, ?2)",
            params![tenant_id, vault_id],
        )?;
        Ok(())
    }

    pub fn get_tenant_vaults(&self, tenant_id: &str) -> Result<Vec<String>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            "SELECT vault_id FROM tenant_vaults WHERE tenant_id = ?1",
        )?;
        let iter = stmt.query_map(params![tenant_id], |r| r.get(0))?;
        let mut vaults = Vec::new();
        for vault_result in iter {
            vaults.push(vault_result?);
        }
        Ok(vaults)
    }

    pub fn upsert_tenant_billing(&self, billing: &crate::models::TenantBilling) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT OR REPLACE INTO tenant_billing (tenant_id, monthly_charge, billing_cycle_start, billing_cycle_end, total_vaults, status)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                billing.tenant_id,
                billing.monthly_charge,
                billing.billing_cycle_start.to_rfc3339(),
                billing.billing_cycle_end.to_rfc3339(),
                billing.total_vaults as i64,
                billing.status
            ],
        )?;
        Ok(())
    }

    // ── #70: Real-Time Collaboration ────────────────────────────────────────

    pub fn store_credential_update(&self, update: &crate::models::CredentialUpdate) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT INTO credential_updates (id, vault_id, user_id, field, old_value, new_value, timestamp, operation_id)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                update.id,
                update.vault_id,
                update.user_id,
                update.field,
                update.old_value.to_string(),
                update.new_value.to_string(),
                update.timestamp.to_rfc3339(),
                update.operation_id
            ],
        )?;
        Ok(())
    }

    pub fn store_operational_transform(&self, transform: &crate::models::OperationalTransform) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT INTO operational_transforms (id, vault_id, user_id, operation, position, content, timestamp, version)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                transform.id,
                transform.vault_id,
                transform.user_id,
                transform.operation,
                transform.position as i64,
                transform.content,
                transform.timestamp.to_rfc3339(),
                transform.version as i64
            ],
        )?;
        Ok(())
    }

    pub fn store_conflict_resolution(&self, resolution: &crate::models::ConflictResolution) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT INTO conflict_resolutions (conflict_id, vault_id, update1_id, update2_id, resolution_strategy, resolved_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                resolution.conflict_id,
                resolution.vault_id,
                resolution.update1_id,
                resolution.update2_id,
                resolution.resolution_strategy,
                resolution.resolved_at.to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn upsert_user_presence(&self, presence: &crate::models::UserPresence) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT OR REPLACE INTO user_presence (user_id, vault_id, status, last_seen, session_id)
               VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                presence.user_id,
                presence.vault_id,
                presence.status,
                presence.last_seen.to_rfc3339(),
                presence.session_id
            ],
        )?;
        Ok(())
    }

    pub fn get_vault_presence(&self, vault_id: &str) -> Result<Vec<crate::models::UserPresence>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            "SELECT user_id, vault_id, status, last_seen, session_id FROM user_presence WHERE vault_id = ?1",
        )?;
        let iter = stmt.query_map(params![vault_id], |r| {
            let last_seen_str: String = r.get(3)?;
            let last_seen = chrono::DateTime::parse_from_rfc3339(&last_seen_str)
                .map(|dt| dt.with_timezone(&chrono::Utc))
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?;
            Ok(crate::models::UserPresence {
                user_id: r.get(0)?,
                vault_id: r.get(1)?,
                status: r.get(2)?,
                last_seen,
                session_id: r.get(4)?,
            })
        })?;
        let mut presence = Vec::new();
        for p in iter {
            presence.push(p?);
        }
        Ok(presence)
    }

    pub fn create_collaborative_session(&self, session: &crate::models::CollaborativeSession) -> Result<(), rusqlite::Error> {
        let participants_json = serde_json::to_string(&session.participants).unwrap_or_default();
        self.conn.lock().unwrap().execute(
            r"INSERT INTO collaborative_sessions (session_id, vault_id, created_at, participants, is_active)
               VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.session_id,
                session.vault_id,
                session.created_at.to_rfc3339(),
                participants_json,
                if session.is_active { 1 } else { 0 }
            ],
        )?;
        Ok(())
    }

    // ── #71: Full-Text Search ───────────────────────────────────────────────

    pub fn index_vault_content(&self, vault_id: &str, title: &str, content: &str) -> Result<(), rusqlite::Error> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.lock().unwrap().execute(
            r"INSERT INTO full_text_search_index (id, vault_id, title, content, created_at, indexed_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, vault_id, title, content, now, now],
        )?;
        Ok(())
    }

    pub fn search_indexed_content(&self, query: &str, limit: u32) -> Result<Vec<crate::models::FullTextSearchResult>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            r"SELECT id, vault_id, title, content FROM full_text_search_index
               WHERE title LIKE ?1 OR content LIKE ?1
               LIMIT ?2",
        )?;
        let search_pattern = format!("%{}%", query);
        let iter = stmt.query_map(params![search_pattern, limit as i64], |r| {
            Ok(crate::models::FullTextSearchResult {
                id: r.get(0)?,
                vault_id: r.get(1)?,
                title: r.get(2)?,
                snippet: {
                    let content: String = r.get(3)?;
                    if content.len() > 200 {
                        format!("{}...", &content[..200])
                    } else {
                        content
                    }
                },
                relevance_score: 0.8,
                matched_fields: vec!["title".to_string(), "content".to_string()],
            })
        })?;
        let mut results = Vec::new();
        for r in iter {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn add_search_facet(&self, vault_id: &str, facet_name: &str, value: &str) -> Result<(), rusqlite::Error> {
        self.conn.lock().unwrap().execute(
            r"INSERT INTO search_facets (vault_id, facet_name, value, count)
               VALUES (?1, ?2, ?3, 1)
               ON CONFLICT(vault_id, facet_name, value) DO UPDATE SET
                   count = count + 1",
            params![vault_id, facet_name, value],
        )?;
        Ok(())
    }

    pub fn get_search_facets(&self, vault_id: &str) -> Result<Vec<crate::models::SearchFacet>, rusqlite::Error> {
        let binding = self.conn.lock().unwrap();
        let mut stmt = binding.prepare(
            "SELECT DISTINCT facet_name FROM search_facets WHERE vault_id = ?1",
        )?;
        let facet_names: Vec<String> = stmt.query_map(params![vault_id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut facets = Vec::new();
        for facet_name in facet_names {
            let mut values_stmt = binding.prepare(
                "SELECT value, count FROM search_facets WHERE vault_id = ?1 AND facet_name = ?2",
            )?;
            let values: Vec<crate::models::FacetValue> = values_stmt.query_map(params![vault_id, &facet_name], |r| {
                Ok(crate::models::FacetValue {
                    value: r.get(0)?,
                    count: r.get::<_, i64>(1)? as u32,
                })
            })?
                .collect::<Result<Vec<_>, _>>()?;

            facets.push(crate::models::SearchFacet {
                name: facet_name,
                values,
            });
        }
        Ok(facets)
    }
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
