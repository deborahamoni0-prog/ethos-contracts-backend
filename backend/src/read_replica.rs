//! # Task #76 & #358 — Database Read Replicas Support & Staleness-Aware Routing
//!
//! Adds support for routing read queries to one or more read replicas while
//! directing writes to the primary database. Factoring replication lag into
//! routing decisions ensures read-after-write consistency and automatic fallback
//! to the primary database when replicas become stale.
//!
//! ## Configuration (environment variables)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `PRIMARY_DB_URL` / `DATABASE_URL` | _(empty)_ | Primary SQLite database path |
//! | `READ_REPLICA_URLS` | _(empty)_ | Comma-separated list of replica SQLite paths |
//! | `READ_REPLICA_STRATEGY` | `round_robin` | `round_robin` or `least_lag` |
//! | `REPLICATION_LAG_THRESHOLD_MS` | `500` | Max acceptable lag before marking a replica unhealthy |
//!
//! ## Architecture
//!
//! ```text
//!  ┌────────────┐       writes       ┌─────────────┐
//!  │   Client   │──────────────────▶ │   Primary   │
//!  │            │                    └──────┬──────┘
//!  │            │       reads               │ (fallback / RAW)
//!  │            │──────────────────▶ ┌──────▼──────┐
//!  │            │ (staleness-aware)  │ ReadReplica │──▶ replica-0 (lag: 10ms)
//!  └────────────┘                    │   Router    │──▶ replica-1 (lag: 120ms)
//!                                    └─────────────┘
//! ```

use rusqlite::{Connection, OptionalExtension};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ── Public types ──────────────────────────────────────────────────────────────

/// Strategy used by [`ReadReplicaRouter`] to select among healthy replicas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaRoutingStrategy {
    /// Distribute reads evenly across all healthy replicas (default).
    RoundRobin,
    /// Route reads to the replica with the smallest known replication lag.
    LeastLag,
}

impl Default for ReplicaRoutingStrategy {
    fn default() -> Self {
        Self::RoundRobin
    }
}

/// Read consistency preference for staleness-aware routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadPreference {
    /// Standard read query: routes to healthy replicas within the staleness threshold.
    #[default]
    Eventual,
    /// Read-after-write query: routes directly to primary (or freshest replica if primary is unavailable).
    ReadAfterWrite,
}

/// Options controlling routing and staleness constraints for a read query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadOptions {
    /// Whether this is a read-after-write request requiring maximum freshness.
    pub read_after_write: bool,
    /// Per-query maximum acceptable replication lag in milliseconds.
    /// If specified, replicas with lag greater than this threshold will not be selected.
    pub max_acceptable_lag_ms: Option<u64>,
}

impl ReadOptions {
    /// Default eventual consistency read options.
    pub fn eventual() -> Self {
        Self {
            read_after_write: false,
            max_acceptable_lag_ms: None,
        }
    }

    /// Read-after-write consistency: routes to primary or freshest replica.
    pub fn read_after_write() -> Self {
        Self {
            read_after_write: true,
            max_acceptable_lag_ms: None,
        }
    }

    /// Read with an explicit maximum staleness tolerance.
    pub fn with_max_lag(max_lag_ms: u64) -> Self {
        Self {
            read_after_write: false,
            max_acceptable_lag_ms: Some(max_lag_ms),
        }
    }
}

/// Health state of an individual replica connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaHealth {
    /// Replica is reachable and within the acceptable lag threshold.
    Healthy,
    /// Replica did not respond to the last health check.
    Unreachable,
    /// Replica responded but its replication lag exceeded the configured
    /// threshold.
    LagExceeded,
}

/// Runtime metrics for a single replica (or primary).
#[derive(Debug, Clone)]
pub struct ReplicaMetrics {
    /// Replica identifier (path / URL).
    pub id: String,
    /// Latest replication lag estimate in milliseconds (0 for primary).
    pub lag_ms: u64,
    /// Current health status.
    pub health: ReplicaHealth,
    /// Timestamp of the most recent successful health check.
    pub last_checked_at: Option<Instant>,
    /// Total number of read queries routed to this target.
    pub total_reads: u64,
}

/// A single managed database connection (replica or primary).
pub struct ReplicaConn {
    pub id: String,
    pub conn: Mutex<Connection>,
    pub metrics: Mutex<ReplicaMetrics>,
}

/// Routes read queries across a pool of replica connections with staleness awareness
/// and fallback to the primary database.
///
/// # Example
///
/// ```rust,ignore
/// let router = ReadReplicaRouter::from_env();
/// if router.has_healthy_replicas() {
///     let vault = router.query_one("SELECT ...", vec![], |row| ...);
/// }
/// ```
pub struct ReadReplicaRouter {
    primary: Option<Arc<ReplicaConn>>,
    replicas: Vec<Arc<ReplicaConn>>,
    strategy: ReplicaRoutingStrategy,
    /// Maximum acceptable replication lag before a replica is considered unhealthy.
    lag_threshold_ms: u64,
    /// Round-robin cursor (only used by [`ReplicaRoutingStrategy::RoundRobin`]).
    rr_cursor: Mutex<usize>,
}

impl ReadReplicaRouter {
    /// Build a router from environment variables.
    ///
    /// Reads `PRIMARY_DB_URL` (or `DATABASE_URL`), `READ_REPLICA_URLS`,
    /// `READ_REPLICA_STRATEGY`, and `REPLICATION_LAG_THRESHOLD_MS`.
    pub fn from_env() -> Self {
        let primary_url = std::env::var("PRIMARY_DB_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
            .filter(|s| !s.trim().is_empty());

        let primary = primary_url.and_then(|url| {
            Connection::open(&url).ok().map(|conn| {
                Arc::new(ReplicaConn {
                    id: url.clone(),
                    conn: Mutex::new(conn),
                    metrics: Mutex::new(ReplicaMetrics {
                        id: url,
                        lag_ms: 0,
                        health: ReplicaHealth::Healthy,
                        last_checked_at: Some(Instant::now()),
                        total_reads: 0,
                    }),
                })
            })
        });

        let urls = std::env::var("READ_REPLICA_URLS").unwrap_or_default();
        let strategy = match std::env::var("READ_REPLICA_STRATEGY")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "least_lag" => ReplicaRoutingStrategy::LeastLag,
            _ => ReplicaRoutingStrategy::RoundRobin,
        };
        let lag_threshold_ms = std::env::var("REPLICATION_LAG_THRESHOLD_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);

        let replicas = urls
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .filter_map(|url| {
                Connection::open(url).ok().map(|conn| {
                    Arc::new(ReplicaConn {
                        id: url.to_string(),
                        conn: Mutex::new(conn),
                        metrics: Mutex::new(ReplicaMetrics {
                            id: url.to_string(),
                            lag_ms: 0,
                            health: ReplicaHealth::Healthy,
                            last_checked_at: None,
                            total_reads: 0,
                        }),
                    })
                })
            })
            .collect();

        Self {
            primary,
            replicas,
            strategy,
            lag_threshold_ms,
            rr_cursor: Mutex::new(0),
        }
    }

    /// Construct a router from explicit replica paths (useful in tests).
    pub fn new(paths: &[&str], strategy: ReplicaRoutingStrategy, lag_threshold_ms: u64) -> Self {
        Self::new_with_primary(None, paths, strategy, lag_threshold_ms)
    }

    /// Construct a router with an optional primary path and replica paths.
    pub fn new_with_primary(
        primary_path: Option<&str>,
        replica_paths: &[&str],
        strategy: ReplicaRoutingStrategy,
        lag_threshold_ms: u64,
    ) -> Self {
        let primary = primary_path.and_then(|path| {
            Connection::open(path).ok().map(|conn| {
                Arc::new(ReplicaConn {
                    id: path.to_string(),
                    conn: Mutex::new(conn),
                    metrics: Mutex::new(ReplicaMetrics {
                        id: path.to_string(),
                        lag_ms: 0,
                        health: ReplicaHealth::Healthy,
                        last_checked_at: Some(Instant::now()),
                        total_reads: 0,
                    }),
                })
            })
        });

        let replicas = replica_paths
            .iter()
            .filter_map(|&path| {
                Connection::open(path).ok().map(|conn| {
                    Arc::new(ReplicaConn {
                        id: path.to_string(),
                        conn: Mutex::new(conn),
                        metrics: Mutex::new(ReplicaMetrics {
                            id: path.to_string(),
                            lag_ms: 0,
                            health: ReplicaHealth::Healthy,
                            last_checked_at: None,
                            total_reads: 0,
                        }),
                    })
                })
            })
            .collect();

        Self {
            primary,
            replicas,
            strategy,
            lag_threshold_ms,
            rr_cursor: Mutex::new(0),
        }
    }

    /// Builder helper to set or replace the primary database connection.
    pub fn with_primary_connection(mut self, id: &str, conn: Connection) -> Self {
        self.primary = Some(Arc::new(ReplicaConn {
            id: id.to_string(),
            conn: Mutex::new(conn),
            metrics: Mutex::new(ReplicaMetrics {
                id: id.to_string(),
                lag_ms: 0,
                health: ReplicaHealth::Healthy,
                last_checked_at: Some(Instant::now()),
                total_reads: 0,
            }),
        }));
        self
    }

    /// Builder helper to add an initialized replica connection.
    pub fn with_replica_connection(mut self, id: &str, conn: Connection) -> Self {
        self.replicas.push(Arc::new(ReplicaConn {
            id: id.to_string(),
            conn: Mutex::new(conn),
            metrics: Mutex::new(ReplicaMetrics {
                id: id.to_string(),
                lag_ms: 0,
                health: ReplicaHealth::Healthy,
                last_checked_at: None,
                total_reads: 0,
            }),
        }));
        self
    }

    // ── Lag tracking & freshness ──────────────────────────────────────────────

    /// Explicitly record/update the replication lag (in milliseconds) for a replica.
    ///
    /// Automatically updates the replica's health status to [`ReplicaHealth::LagExceeded`]
    /// if `lag_ms > lag_threshold_ms`, or restores it to [`ReplicaHealth::Healthy`] if reachable.
    pub fn record_lag(&self, replica_id: &str, lag_ms: u64) {
        for r in &self.replicas {
            if r.id == replica_id {
                let mut m = r.metrics.lock().unwrap();
                m.lag_ms = lag_ms;
                m.last_checked_at = Some(Instant::now());
                if m.health != ReplicaHealth::Unreachable {
                    m.health = if lag_ms > self.lag_threshold_ms {
                        ReplicaHealth::LagExceeded
                    } else {
                        ReplicaHealth::Healthy
                    };
                }
            }
        }
    }

    /// Retrieve the current known replication lag for a replica.
    pub fn get_replica_lag(&self, replica_id: &str) -> Option<u64> {
        self.replicas
            .iter()
            .find(|r| r.id == replica_id)
            .map(|r| r.metrics.lock().unwrap().lag_ms)
    }

    /// Find the freshest healthy replica (the one with the lowest replication lag).
    pub fn freshest_replica(&self) -> Option<Arc<ReplicaConn>> {
        self.replicas
            .iter()
            .filter(|r| r.metrics.lock().unwrap().health == ReplicaHealth::Healthy)
            .min_by_key(|r| r.metrics.lock().unwrap().lag_ms)
            .cloned()
    }

    /// Check if a primary database is configured.
    pub fn has_primary(&self) -> bool {
        self.primary.is_some()
    }

    /// Snapshot metrics for the primary database connection, if configured.
    pub fn primary_metrics(&self) -> Option<ReplicaMetrics> {
        self.primary
            .as_ref()
            .map(|p| p.metrics.lock().unwrap().clone())
    }

    // ── Routing ───────────────────────────────────────────────────────────────

    /// Returns `true` if at least one replica is [`ReplicaHealth::Healthy`].
    pub fn has_healthy_replicas(&self) -> bool {
        self.replicas.iter().any(|r| {
            r.metrics.lock().unwrap().health == ReplicaHealth::Healthy
        })
    }

    /// Select a healthy replica using the configured routing strategy.
    ///
    /// Returns `None` when all replicas are unhealthy or the pool is empty.
    pub fn select_replica(&self) -> Option<Arc<ReplicaConn>> {
        self.select_target_for_read(&ReadOptions::eventual()).ok()
    }

    /// Select a target (replica or primary) for a read query based on staleness constraints
    /// and caller read preferences.
    ///
    /// - For **read-after-write** flows (`options.read_after_write = true`):
    ///   Routes directly to the primary database if configured; otherwise routes to the
    ///   freshest healthy replica within the staleness threshold.
    /// - For **eventual consistency** reads:
    ///   Filters healthy replicas whose lag does not exceed `max_acceptable_lag_ms` (or `lag_threshold_ms`).
    ///   If eligible replicas exist, selects one via the configured strategy (`RoundRobin` or `LeastLag`).
    ///   If **all replicas exceed the staleness threshold** (or are unreachable), falls back to the primary.
    pub fn select_target_for_read(
        &self,
        options: &ReadOptions,
    ) -> Result<Arc<ReplicaConn>, ReplicaError> {
        let max_lag = options
            .max_acceptable_lag_ms
            .unwrap_or(self.lag_threshold_ms);

        if options.read_after_write {
            // Read-after-write: prefer primary for guaranteed freshness.
            if let Some(ref primary) = self.primary {
                return Ok(Arc::clone(primary));
            }

            // If no primary is configured in the router, route to the freshest replica.
            if let Some(freshest) = self.freshest_replica() {
                if freshest.metrics.lock().unwrap().lag_ms <= max_lag {
                    return Ok(freshest);
                }
            }

            return Err(ReplicaError::NoHealthyReplica);
        }

        // Standard read: filter healthy replicas within the staleness threshold.
        let eligible: Vec<Arc<ReplicaConn>> = self
            .replicas
            .iter()
            .filter(|r| {
                let m = r.metrics.lock().unwrap();
                m.health == ReplicaHealth::Healthy && m.lag_ms <= max_lag
            })
            .cloned()
            .collect();

        if !eligible.is_empty() {
            match self.strategy {
                ReplicaRoutingStrategy::RoundRobin => {
                    let mut cursor = self.rr_cursor.lock().unwrap();
                    let idx = *cursor % eligible.len();
                    *cursor = cursor.wrapping_add(1);
                    return Ok(Arc::clone(&eligible[idx]));
                }
                ReplicaRoutingStrategy::LeastLag => {
                    let best = eligible
                        .into_iter()
                        .min_by_key(|r| r.metrics.lock().unwrap().lag_ms)
                        .unwrap();
                    return Ok(best);
                }
            }
        }

        // Staleness fallback: all replicas are stale or unhealthy, fall back to primary.
        if let Some(ref primary) = self.primary {
            return Ok(Arc::clone(primary));
        }

        if !self.replicas.is_empty() {
            Err(ReplicaError::AllReplicasStale)
        } else {
            Err(ReplicaError::NoHealthyReplica)
        }
    }

    // ── Read query helpers ────────────────────────────────────────────────────

    /// Execute a simple `SELECT 1` connectivity probe on the selected replica.
    ///
    /// Returns `Ok(true)` on success, `Ok(false)` when no healthy target is
    /// available, or an `Err` if the query fails.
    pub fn ping_replica(&self) -> Result<bool, ReplicaError> {
        self.ping_target(&ReadOptions::eventual())
    }

    /// Execute a simple connectivity probe using specific [`ReadOptions`].
    pub fn ping_target(&self, options: &ReadOptions) -> Result<bool, ReplicaError> {
        let target = match self.select_target_for_read(options) {
            Ok(r) => r,
            Err(ReplicaError::NoHealthyReplica | ReplicaError::AllReplicasStale) => return Ok(false),
            Err(e) => return Err(e),
        };

        let conn = target.conn.lock().unwrap();
        conn.execute_batch("SELECT 1")
            .map_err(|e| ReplicaError::Query(e.to_string()))?;
        target.metrics.lock().unwrap().total_reads += 1;
        Ok(true)
    }

    /// Read a single row by key from any healthy replica within the default staleness threshold.
    pub fn query_one<T, F>(
        &self,
        sql: &str,
        params: Vec<Box<dyn rusqlite::types::ToSql>>,
        map: F,
    ) -> Result<Option<T>, ReplicaError>
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        self.query_one_with_options(sql, params, &ReadOptions::eventual(), map)
    }

    /// Read a single row with read-after-write freshness guarantees (routes to primary or freshest replica).
    pub fn query_one_read_after_write<T, F>(
        &self,
        sql: &str,
        params: Vec<Box<dyn rusqlite::types::ToSql>>,
        map: F,
    ) -> Result<Option<T>, ReplicaError>
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        self.query_one_with_options(sql, params, &ReadOptions::read_after_write(), map)
    }

    /// Read a single row by key using explicit [`ReadOptions`].
    ///
    /// The function automatically increments the `total_reads` counter on the selected target.
    pub fn query_one_with_options<T, F>(
        &self,
        sql: &str,
        params: Vec<Box<dyn rusqlite::types::ToSql>>,
        options: &ReadOptions,
        map: F,
    ) -> Result<Option<T>, ReplicaError>
    where
        F: Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        let target = self.select_target_for_read(options)?;

        let conn = target.conn.lock().unwrap();
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| ReplicaError::Query(e.to_string()))?;

        let result = stmt
            .query_row(param_refs.as_slice(), map)
            .optional()
            .map_err(|e| ReplicaError::Query(e.to_string()))?;

        target.metrics.lock().unwrap().total_reads += 1;
        Ok(result)
    }

    // ── Health checking ───────────────────────────────────────────────────────

    /// Run a health check against every replica.
    ///
    /// Each check:
    /// 1. Attempts a `SELECT 1` and measures round-trip duration.
    /// 2. Reads `lag_ms` from a `replication_lag_ms` table if it exists.
    /// 3. Updates [`ReplicaMetrics`] and marks the replica healthy or degraded.
    pub fn check_all_replicas(&self) {
        for replica in &self.replicas {
            self.check_replica(Arc::clone(replica));
        }
    }

    fn check_replica(&self, replica: Arc<ReplicaConn>) {
        let start = Instant::now();
        let conn = replica.conn.lock().unwrap();

        // Basic connectivity check.
        if conn.execute_batch("SELECT 1").is_err() {
            let mut m = replica.metrics.lock().unwrap();
            m.health = ReplicaHealth::Unreachable;
            m.last_checked_at = Some(Instant::now());
            return;
        }

        let rtt_ms = start.elapsed().as_millis() as u64;

        // Optional: read lag from a replication_lag_ms table (may not exist).
        let lag_ms: u64 = conn
            .query_row(
                "SELECT lag_ms FROM replication_lag_ms LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .unwrap_or(rtt_ms); // fall back to RTT as lag proxy

        let mut m = replica.metrics.lock().unwrap();
        m.lag_ms = lag_ms;
        m.last_checked_at = Some(Instant::now());
        m.health = if lag_ms > self.lag_threshold_ms {
            ReplicaHealth::LagExceeded
        } else {
            ReplicaHealth::Healthy
        };
    }

    /// Force-mark a replica healthy (used in tests and manual operator overrides).
    pub fn mark_healthy(&self, replica_id: &str) {
        for r in &self.replicas {
            if r.id == replica_id {
                let mut m = r.metrics.lock().unwrap();
                m.health = ReplicaHealth::Healthy;
                m.lag_ms = 0;
            }
        }
    }

    /// Force-mark a replica unreachable (used in tests).
    pub fn mark_unreachable(&self, replica_id: &str) {
        for r in &self.replicas {
            if r.id == replica_id {
                r.metrics.lock().unwrap().health = ReplicaHealth::Unreachable;
            }
        }
    }

    // ── Metrics export ────────────────────────────────────────────────────────

    /// Snapshot current metrics for all replicas (and primary if present).
    pub fn all_metrics(&self) -> Vec<ReplicaMetrics> {
        let mut list: Vec<ReplicaMetrics> = Vec::new();
        if let Some(ref p) = self.primary {
            list.push(p.metrics.lock().unwrap().clone());
        }
        for r in &self.replicas {
            list.push(r.metrics.lock().unwrap().clone());
        }
        list
    }

    /// Number of configured replicas regardless of health.
    pub fn replica_count(&self) -> usize {
        self.replicas.len()
    }

    /// Number of currently healthy replicas.
    pub fn healthy_count(&self) -> usize {
        self.replicas
            .iter()
            .filter(|r| r.metrics.lock().unwrap().health == ReplicaHealth::Healthy)
            .count()
    }

    /// Number of replicas currently exceeding staleness lag threshold.
    pub fn staleness_exceeded_count(&self) -> usize {
        self.replicas
            .iter()
            .filter(|r| r.metrics.lock().unwrap().health == ReplicaHealth::LagExceeded)
            .count()
    }
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by [`ReadReplicaRouter`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReplicaError {
    /// No healthy replica or primary is currently available.
    NoHealthyReplica,
    /// All replicas exceed the acceptable staleness threshold, and no primary fallback is available.
    AllReplicasStale,
    /// The underlying SQLite query failed.
    Query(String),
}

impl std::fmt::Display for ReplicaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoHealthyReplica => write!(f, "no healthy replica available"),
            Self::AllReplicasStale => write!(f, "all replicas exceed staleness threshold"),
            Self::Query(msg) => write!(f, "replica query error: {msg}"),
        }
    }
}

impl std::error::Error for ReplicaError {}

// ── Replica setup helper ──────────────────────────────────────────────────────

/// Initialise a fresh in-memory SQLite database that mirrors the primary schema
/// (useful in tests and for the `:memory:` replica path).
pub fn bootstrap_replica(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version    TEXT PRIMARY KEY,
            applied_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS replication_lag_ms (
            lag_ms INTEGER NOT NULL
        );
        INSERT OR REPLACE INTO replication_lag_ms (lag_ms) VALUES (0);
        ",
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_router() -> ReadReplicaRouter {
        let conn = Connection::open_in_memory().expect("in-memory db");
        bootstrap_replica(&conn).expect("bootstrap");

        let replica = Arc::new(ReplicaConn {
            id: ":memory:".to_string(),
            conn: Mutex::new(conn),
            metrics: Mutex::new(ReplicaMetrics {
                id: ":memory:".to_string(),
                lag_ms: 0,
                health: ReplicaHealth::Healthy,
                last_checked_at: None,
                total_reads: 0,
            }),
        });

        ReadReplicaRouter {
            primary: None,
            replicas: vec![replica],
            strategy: ReplicaRoutingStrategy::RoundRobin,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        }
    }

    fn make_test_replica(id: &str, lag_ms: u64) -> Arc<ReplicaConn> {
        let conn = Connection::open_in_memory().expect("in-memory db");
        bootstrap_replica(&conn).expect("bootstrap");
        Arc::new(ReplicaConn {
            id: id.to_string(),
            conn: Mutex::new(conn),
            metrics: Mutex::new(ReplicaMetrics {
                id: id.to_string(),
                lag_ms,
                health: ReplicaHealth::Healthy,
                last_checked_at: None,
                total_reads: 0,
            }),
        })
    }

    #[test]
    fn test_empty_router_has_no_healthy_replicas() {
        let router = ReadReplicaRouter::new(&[], ReplicaRoutingStrategy::RoundRobin, 500);
        assert!(!router.has_healthy_replicas());
        assert_eq!(router.replica_count(), 0);
        assert_eq!(router.healthy_count(), 0);
    }

    #[test]
    fn test_ping_returns_false_with_no_replicas() {
        let router = ReadReplicaRouter::new(&[], ReplicaRoutingStrategy::RoundRobin, 500);
        assert_eq!(router.ping_replica().unwrap(), false);
    }

    #[test]
    fn test_healthy_replica_ping() {
        let router = in_memory_router();
        assert!(router.has_healthy_replicas());
        assert_eq!(router.ping_replica().unwrap(), true);
    }

    #[test]
    fn test_mark_unreachable_excludes_from_routing() {
        let router = in_memory_router();
        router.mark_unreachable(":memory:");
        assert!(!router.has_healthy_replicas());
        assert_eq!(router.healthy_count(), 0);
    }

    #[test]
    fn test_mark_healthy_restores_routing() {
        let router = in_memory_router();
        router.mark_unreachable(":memory:");
        router.mark_healthy(":memory:");
        assert!(router.has_healthy_replicas());
        assert_eq!(router.healthy_count(), 1);
    }

    #[test]
    fn test_metrics_total_reads_increments() {
        let router = in_memory_router();
        router.ping_replica().unwrap();
        router.ping_replica().unwrap();
        let metrics = router.all_metrics();
        assert_eq!(metrics[0].total_reads, 2);
    }

    #[test]
    fn test_round_robin_distributes_across_replicas() {
        let router = ReadReplicaRouter {
            primary: None,
            replicas: vec![make_test_replica("r0", 0), make_test_replica("r1", 0)],
            strategy: ReplicaRoutingStrategy::RoundRobin,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        router.ping_replica().unwrap();
        router.ping_replica().unwrap();
        router.ping_replica().unwrap();

        let metrics = router.all_metrics();
        let total: u64 = metrics.iter().map(|m| m.total_reads).sum();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_health_check_marks_replica_healthy() {
        let router = in_memory_router();
        router.mark_unreachable(":memory:");
        assert!(!router.has_healthy_replicas());

        router.mark_healthy(":memory:");
        router.check_all_replicas();
        assert!(router.has_healthy_replicas());
    }

    #[test]
    fn test_lag_threshold_marks_lag_exceeded() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS replication_lag_ms (lag_ms INTEGER NOT NULL);
             INSERT INTO replication_lag_ms (lag_ms) VALUES (9999);",
        )
        .unwrap();

        let replica = Arc::new(ReplicaConn {
            id: "lag-replica".to_string(),
            conn: Mutex::new(conn),
            metrics: Mutex::new(ReplicaMetrics {
                id: "lag-replica".to_string(),
                lag_ms: 0,
                health: ReplicaHealth::Healthy,
                last_checked_at: None,
                total_reads: 0,
            }),
        });

        let router = ReadReplicaRouter {
            primary: None,
            replicas: vec![replica],
            strategy: ReplicaRoutingStrategy::RoundRobin,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        router.check_all_replicas();
        let metrics = router.all_metrics();
        assert_eq!(metrics[0].health, ReplicaHealth::LagExceeded);
        assert_eq!(router.staleness_exceeded_count(), 1);
    }

    #[test]
    fn test_from_env_empty_produces_zero_replicas() {
        std::env::remove_var("READ_REPLICA_URLS");
        std::env::remove_var("PRIMARY_DB_URL");
        std::env::remove_var("DATABASE_URL");
        let router = ReadReplicaRouter::from_env();
        assert_eq!(router.replica_count(), 0);
        assert!(!router.has_primary());
    }

    #[test]
    fn test_least_lag_strategy_selects_lowest_lag() {
        let router = ReadReplicaRouter {
            primary: None,
            replicas: vec![make_test_replica("high-lag", 400), make_test_replica("low-lag", 50)],
            strategy: ReplicaRoutingStrategy::LeastLag,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        for _ in 0..3 {
            router.ping_replica().unwrap();
        }

        let metrics = router.all_metrics();
        let low = metrics.iter().find(|m| m.id == "low-lag").unwrap();
        let high = metrics.iter().find(|m| m.id == "high-lag").unwrap();
        assert_eq!(low.total_reads, 3);
        assert_eq!(high.total_reads, 0);
    }

    // ── Task #358 Specific Tests ──────────────────────────────────────────────

    #[test]
    fn test_track_replication_lag_per_replica() {
        let router = ReadReplicaRouter {
            primary: None,
            replicas: vec![make_test_replica("rep-1", 10), make_test_replica("rep-2", 20)],
            strategy: ReplicaRoutingStrategy::LeastLag,
            lag_threshold_ms: 200,
            rr_cursor: Mutex::new(0),
        };

        assert_eq!(router.get_replica_lag("rep-1"), Some(10));
        assert_eq!(router.get_replica_lag("rep-2"), Some(20));

        // Update lag dynamically
        router.record_lag("rep-1", 250);
        assert_eq!(router.get_replica_lag("rep-1"), Some(250));

        // Since 250 > lag_threshold_ms (200), rep-1 should now be marked LagExceeded
        let metrics = router.all_metrics();
        let r1 = metrics.iter().find(|m| m.id == "rep-1").unwrap();
        assert_eq!(r1.health, ReplicaHealth::LagExceeded);

        // rep-2 is still healthy
        let r2 = metrics.iter().find(|m| m.id == "rep-2").unwrap();
        assert_eq!(r2.health, ReplicaHealth::Healthy);

        // Freshest replica should now be rep-2
        assert_eq!(router.freshest_replica().unwrap().id, "rep-2");
    }

    #[test]
    fn test_route_read_after_write_to_primary() {
        let primary_conn = Connection::open_in_memory().expect("primary db");
        bootstrap_replica(&primary_conn).expect("bootstrap");

        let router = ReadReplicaRouter {
            primary: Some(Arc::new(ReplicaConn {
                id: "primary-db".to_string(),
                conn: Mutex::new(primary_conn),
                metrics: Mutex::new(ReplicaMetrics {
                    id: "primary-db".to_string(),
                    lag_ms: 0,
                    health: ReplicaHealth::Healthy,
                    last_checked_at: Some(Instant::now()),
                    total_reads: 0,
                }),
            })),
            replicas: vec![make_test_replica("rep-1", 5)],
            strategy: ReplicaRoutingStrategy::RoundRobin,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        // Standard read routes to replica
        let target_eventual = router.select_target_for_read(&ReadOptions::eventual()).unwrap();
        assert_eq!(target_eventual.id, "rep-1");

        // Read-after-write request routes directly to primary
        let target_raw = router.select_target_for_read(&ReadOptions::read_after_write()).unwrap();
        assert_eq!(target_raw.id, "primary-db");
    }

    #[test]
    fn test_route_read_after_write_to_freshest_replica_when_no_primary() {
        let router = ReadReplicaRouter {
            primary: None,
            replicas: vec![
                make_test_replica("rep-stale", 150),
                make_test_replica("rep-fresh", 20),
            ],
            strategy: ReplicaRoutingStrategy::RoundRobin,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        // Without primary configured, RAW request routes to freshest healthy replica
        let target = router.select_target_for_read(&ReadOptions::read_after_write()).unwrap();
        assert_eq!(target.id, "rep-fresh");
    }

    #[test]
    fn test_staleness_fallback_to_primary_when_all_replicas_exceed_threshold() {
        let primary_conn = Connection::open_in_memory().expect("primary db");
        bootstrap_replica(&primary_conn).expect("bootstrap");

        let router = ReadReplicaRouter {
            primary: Some(Arc::new(ReplicaConn {
                id: "primary-fallback".to_string(),
                conn: Mutex::new(primary_conn),
                metrics: Mutex::new(ReplicaMetrics {
                    id: "primary-fallback".to_string(),
                    lag_ms: 0,
                    health: ReplicaHealth::Healthy,
                    last_checked_at: Some(Instant::now()),
                    total_reads: 0,
                }),
            })),
            replicas: vec![
                make_test_replica("rep-1", 1000), // Exceeds 500ms
                make_test_replica("rep-2", 800),  // Exceeds 500ms
            ],
            strategy: ReplicaRoutingStrategy::LeastLag,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        // All replicas exceed 500ms threshold -> router falls back to primary
        let target = router.select_target_for_read(&ReadOptions::eventual()).unwrap();
        assert_eq!(target.id, "primary-fallback");

        // Custom query-level max_acceptable_lag_ms: if query accepts max 50ms lag and reps have 60ms
        let rep_mild = make_test_replica("rep-mild", 60);
        let router_custom = ReadReplicaRouter {
            primary: Some(Arc::new(ReplicaConn {
                id: "primary-custom".to_string(),
                conn: Mutex::new(Connection::open_in_memory().unwrap()),
                metrics: Mutex::new(ReplicaMetrics {
                    id: "primary-custom".to_string(),
                    lag_ms: 0,
                    health: ReplicaHealth::Healthy,
                    last_checked_at: None,
                    total_reads: 0,
                }),
            })),
            replicas: vec![rep_mild],
            strategy: ReplicaRoutingStrategy::LeastLag,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        let target_custom = router_custom
            .select_target_for_read(&ReadOptions::with_max_lag(50))
            .unwrap();
        assert_eq!(target_custom.id, "primary-custom");
    }

    #[test]
    fn test_query_one_with_options_and_read_after_write() {
        let primary_conn = Connection::open_in_memory().expect("primary db");
        primary_conn
            .execute_batch("CREATE TABLE items (id TEXT PRIMARY KEY, val TEXT); INSERT INTO items VALUES ('k1', 'primary_val');")
            .unwrap();

        let replica_conn = Connection::open_in_memory().expect("replica db");
        replica_conn
            .execute_batch("CREATE TABLE items (id TEXT PRIMARY KEY, val TEXT); INSERT INTO items VALUES ('k1', 'replica_val');")
            .unwrap();

        let router = ReadReplicaRouter {
            primary: Some(Arc::new(ReplicaConn {
                id: "primary".to_string(),
                conn: Mutex::new(primary_conn),
                metrics: Mutex::new(ReplicaMetrics {
                    id: "primary".to_string(),
                    lag_ms: 0,
                    health: ReplicaHealth::Healthy,
                    last_checked_at: None,
                    total_reads: 0,
                }),
            })),
            replicas: vec![Arc::new(ReplicaConn {
                id: "replica".to_string(),
                conn: Mutex::new(replica_conn),
                metrics: Mutex::new(ReplicaMetrics {
                    id: "replica".to_string(),
                    lag_ms: 10,
                    health: ReplicaHealth::Healthy,
                    last_checked_at: None,
                    total_reads: 0,
                }),
            })],
            strategy: ReplicaRoutingStrategy::LeastLag,
            lag_threshold_ms: 500,
            rr_cursor: Mutex::new(0),
        };

        // Standard read returns replica value
        let res_eventual: Option<String> = router
            .query_one("SELECT val FROM items WHERE id = ?1", vec![Box::new("k1".to_string())], |r| r.get(0))
            .unwrap();
        assert_eq!(res_eventual, Some("replica_val".to_string()));

        // Read-after-write query returns primary value
        let res_raw: Option<String> = router
            .query_one_read_after_write("SELECT val FROM items WHERE id = ?1", vec![Box::new("k1".to_string())], |r| r.get(0))
            .unwrap();
        assert_eq!(res_raw, Some("primary_val".to_string()));
    }
}
