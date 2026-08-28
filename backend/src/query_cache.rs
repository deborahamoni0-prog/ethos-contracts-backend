/// Generic query-result cache with per-entry TTL support and domain invalidation helpers.
///
/// `QueryCache` stores arbitrary `serde_json::Value` results keyed by a
/// caller-supplied string. Entries expire after the configured TTL and are
/// lazily evicted on the next access. Hit/miss counters use `AtomicU64` so
/// they can be incremented without holding the inner `Mutex`.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Default TTL ───────────────────────────────────────────────────────────────

/// Default cache time-to-live: 60 seconds.
pub const DEFAULT_TTL_SECS: u64 = 60;

// ── QueryCacheKey ─────────────────────────────────────────────────────────────

/// Standard cache key formats used across handlers for consistent caching and invalidation.
pub struct QueryCacheKey;

impl QueryCacheKey {
    /// Cache key for vault summary: `vault:{vault_id}:summary`.
    pub fn vault_summary(vault_id: &str) -> String {
        format!("vault:{vault_id}:summary")
    }

    /// Cache key for vault TTL remaining: `vault:{vault_id}:ttl`.
    pub fn vault_ttl(vault_id: &str) -> String {
        format!("vault:{vault_id}:ttl")
    }

    /// Cache key for vault reminder preferences: `vault:{vault_id}:prefs`.
    pub fn vault_prefs(vault_id: &str) -> String {
        format!("vault:{vault_id}:prefs")
    }

    /// Cache key for notification preferences: `vault:{vault_id}:notification_preferences`.
    pub fn vault_notif_prefs(vault_id: &str) -> String {
        format!("vault:{vault_id}:notification_preferences")
    }

    /// Cache key for vault shares: `vault:{vault_id}:shares`.
    pub fn vault_shares(vault_id: &str) -> String {
        format!("vault:{vault_id}:shares")
    }

    /// Cache key for vault share tokens: `vault:{vault_id}:tokens`.
    pub fn vault_tokens(vault_id: &str) -> String {
        format!("vault:{vault_id}:tokens")
    }

    /// Cache key for vault detail analytics: `vault:{vault_id}:analytics`.
    pub fn vault_analytics(vault_id: &str) -> String {
        format!("vault:{vault_id}:analytics")
    }

    /// Prefix for all query cache entries of a specific vault: `vault:{vault_id}:`.
    pub fn vault_prefix(vault_id: &str) -> String {
        format!("vault:{vault_id}:")
    }

    /// Cache key for vault notification subscription: `subscription:{vault_id}`.
    pub fn subscription(vault_id: &str) -> String {
        format!("subscription:{vault_id}")
    }

    /// Prefix for tenant query cache entries: `tenant:{tenant_id}:`.
    pub fn tenant_prefix(tenant_id: &str) -> String {
        format!("tenant:{tenant_id}:")
    }

    /// Cache key for a share token: `token:{token}`.
    pub fn token(token: &str) -> String {
        format!("token:{token}")
    }
}

// ── CachedResult ──────────────────────────────────────────────────────────────

/// A single entry held inside the cache.
pub struct CachedResult {
    /// The cached query result.
    pub value: Value,
    /// When this entry was inserted.
    pub inserted_at: Instant,
    /// How long this entry lives before being treated as expired.
    pub ttl: Duration,
    /// The key under which this entry is stored (mirrors the map key for
    /// convenience).
    pub query_key: String,
}

impl CachedResult {
    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() >= self.ttl
    }
}

// ── CacheStats ────────────────────────────────────────────────────────────────

/// Snapshot of cache performance metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    /// Total number of entries currently stored (including expired ones that
    /// have not yet been evicted).
    pub total_entries: usize,
    /// Number of successful cache lookups since the cache was created.
    pub hit_count: u64,
    /// Number of failed cache lookups (key missing or entry expired) since the
    /// cache was created.
    pub miss_count: u64,
    /// Number of entries that are present but have already exceeded their TTL.
    pub expired_entries: usize,
}

// ── QueryCache ────────────────────────────────────────────────────────────────

/// Thread-safe in-memory query result cache with TTL support and domain invalidation.
///
/// # Example
/// ```rust,ignore
/// let cache = QueryCache::new();
/// cache.set("vault:42:summary", serde_json::json!({"id": 42}));
/// if let Some(v) = cache.get("vault:42:summary") {
///     println!("cache hit: {v}");
/// }
/// ```
pub struct QueryCache {
    inner: Mutex<HashMap<String, CachedResult>>,
    default_ttl: Duration,
    hit_count: AtomicU64,
    miss_count: AtomicU64,
}

impl QueryCache {
    /// Create a new cache with the default 60-second TTL.
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(DEFAULT_TTL_SECS))
    }

    /// Create a cache with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            default_ttl: ttl,
            hit_count: AtomicU64::new(0),
            miss_count: AtomicU64::new(0),
        }
    }

    /// Look up `key` in the cache.
    ///
    /// Returns `Some(value)` on a live hit, `None` on a miss or if the entry
    /// has expired. Expired entries are removed lazily on access.
    pub fn get(&self, key: &str) -> Option<Value> {
        let mut map = self.inner.lock().unwrap();
        match map.get(key) {
            Some(entry) if !entry.is_expired() => {
                let value = entry.value.clone();
                drop(map);
                self.hit_count.fetch_add(1, Ordering::Relaxed);
                Some(value)
            }
            Some(_expired) => {
                // Evict the stale entry.
                map.remove(key);
                drop(map);
                self.miss_count.fetch_add(1, Ordering::Relaxed);
                None
            }
            None => {
                drop(map);
                self.miss_count.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Store `value` under `key` using the cache's default TTL.
    pub fn set(&self, key: &str, value: Value) {
        let entry = CachedResult {
            value,
            inserted_at: Instant::now(),
            ttl: self.default_ttl,
            query_key: key.to_string(),
        };
        self.inner.lock().unwrap().insert(key.to_string(), entry);
    }

    /// Remove the entry for `key`, if present.
    pub fn invalidate(&self, key: &str) {
        self.inner.lock().unwrap().remove(key);
    }

    /// Remove all entries whose key starts with `prefix`.
    ///
    /// Useful for invalidating a logical group of related queries, e.g. all
    /// entries for a specific vault: `invalidate_prefix("vault:42:")`.
    pub fn invalidate_prefix(&self, prefix: &str) {
        let mut map = self.inner.lock().unwrap();
        map.retain(|k, _| !k.starts_with(prefix));
    }

    /// Remove every entry from the cache.
    pub fn invalidate_all(&self) {
        self.inner.lock().unwrap().clear();
    }

    // ── Domain-specific invalidation helpers ───────────────────────────────────

    /// Invalidate all query cache entries associated with a specific vault
    /// (summaries, TTL, preferences, shares, tokens, analytics, and subscriptions).
    pub fn invalidate_vault(&self, vault_id: &str) {
        self.invalidate_prefix(&QueryCacheKey::vault_prefix(vault_id));
        self.invalidate(&QueryCacheKey::subscription(vault_id));
    }

    /// Invalidate share and token query caches for a vault.
    pub fn invalidate_shares(&self, vault_id: &str) {
        self.invalidate(&QueryCacheKey::vault_shares(vault_id));
        self.invalidate(&QueryCacheKey::vault_tokens(vault_id));
        self.invalidate(&QueryCacheKey::vault_summary(vault_id));
        self.invalidate(&QueryCacheKey::vault_analytics(vault_id));
    }

    /// Invalidate a specific share token and its vault tokens list.
    pub fn invalidate_share_token(&self, vault_id: &str, token: &str) {
        self.invalidate(&QueryCacheKey::vault_tokens(vault_id));
        self.invalidate(&QueryCacheKey::token(token));
    }

    /// Invalidate notification and reminder preferences for a vault.
    pub fn invalidate_preferences(&self, vault_id: &str) {
        self.invalidate(&QueryCacheKey::vault_prefs(vault_id));
        self.invalidate(&QueryCacheKey::vault_notif_prefs(vault_id));
    }

    /// Invalidate subscription query caches for a vault.
    pub fn invalidate_subscription(&self, vault_id: &str) {
        self.invalidate(&QueryCacheKey::subscription(vault_id));
        self.invalidate(&QueryCacheKey::vault_summary(vault_id));
    }

    /// Invalidate tenant query caches.
    pub fn invalidate_tenant(&self, tenant_id: &str) {
        self.invalidate_prefix(&QueryCacheKey::tenant_prefix(tenant_id));
    }

    /// Invalidate credential update caches for a vault.
    pub fn invalidate_credential(&self, vault_id: &str) {
        self.invalidate_prefix(&QueryCacheKey::vault_prefix(vault_id));
    }

    /// Return a point-in-time snapshot of cache statistics.
    pub fn stats(&self) -> CacheStats {
        let map = self.inner.lock().unwrap();
        let total_entries = map.len();
        let expired_entries = map.values().filter(|e| e.is_expired()).count();
        let hit_count = self.hit_count.load(Ordering::Relaxed);
        let miss_count = self.miss_count.load(Ordering::Relaxed);
        CacheStats {
            total_entries,
            hit_count,
            miss_count,
            expired_entries,
        }
    }
}

impl Default for QueryCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_set_and_get_hit() {
        let cache = QueryCache::new();
        cache.set("k1", json!({"x": 1}));
        let v = cache.get("k1");
        assert!(v.is_some());
        assert_eq!(v.unwrap()["x"], 1);
        assert_eq!(cache.stats().hit_count, 1);
        assert_eq!(cache.stats().miss_count, 0);
    }

    #[test]
    fn test_get_miss_on_empty() {
        let cache = QueryCache::new();
        assert!(cache.get("missing").is_none());
        assert_eq!(cache.stats().miss_count, 1);
    }

    #[test]
    fn test_expired_entry_returns_none() {
        let cache = QueryCache::with_ttl(Duration::from_millis(1));
        cache.set("k1", json!(1));
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("k1").is_none());
        assert_eq!(cache.stats().miss_count, 1);
    }

    #[test]
    fn test_invalidate_removes_entry() {
        let cache = QueryCache::new();
        cache.set("k1", json!(1));
        cache.invalidate("k1");
        assert!(cache.get("k1").is_none());
    }

    #[test]
    fn test_invalidate_prefix() {
        let cache = QueryCache::new();
        cache.set("vault:1:summary", json!(1));
        cache.set("vault:1:ttl", json!(2));
        cache.set("vault:2:summary", json!(3));
        cache.invalidate_prefix("vault:1:");
        assert!(cache.get("vault:1:summary").is_none());
        assert!(cache.get("vault:1:ttl").is_none());
        assert!(cache.get("vault:2:summary").is_some());
    }

    #[test]
    fn test_invalidate_all() {
        let cache = QueryCache::new();
        cache.set("a", json!(1));
        cache.set("b", json!(2));
        cache.invalidate_all();
        assert_eq!(cache.stats().total_entries, 0);
    }

    #[test]
    fn test_stats_expired_entries() {
        let cache = QueryCache::with_ttl(Duration::from_millis(1));
        cache.set("k1", json!(1));
        cache.set("k2", json!(2));
        std::thread::sleep(Duration::from_millis(5));
        let stats = cache.stats();
        assert_eq!(stats.total_entries, 2);
        assert_eq!(stats.expired_entries, 2);
    }

    #[test]
    fn test_domain_invalidate_vault() {
        let cache = QueryCache::new();
        cache.set(&QueryCacheKey::vault_summary("v1"), json!({"id": "v1"}));
        cache.set(&QueryCacheKey::vault_ttl("v1"), json!(3600));
        cache.set(&QueryCacheKey::vault_prefs("v1"), json!({"freq": "Daily"}));
        cache.set(&QueryCacheKey::subscription("v1"), json!({"active": true}));
        cache.set(&QueryCacheKey::vault_summary("v2"), json!({"id": "v2"}));

        cache.invalidate_vault("v1");

        assert!(cache.get(&QueryCacheKey::vault_summary("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::vault_ttl("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::vault_prefs("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::subscription("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::vault_summary("v2")).is_some());
    }

    #[test]
    fn test_domain_invalidate_shares_and_tokens() {
        let cache = QueryCache::new();
        cache.set(&QueryCacheKey::vault_shares("v1"), json!(["user1"]));
        cache.set(&QueryCacheKey::vault_tokens("v1"), json!(["tok-1"]));
        cache.set(&QueryCacheKey::token("tok-1"), json!({"token": "tok-1"}));

        cache.invalidate_share_token("v1", "tok-1");
        assert!(cache.get(&QueryCacheKey::vault_tokens("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::token("tok-1")).is_none());
        assert!(cache.get(&QueryCacheKey::vault_shares("v1")).is_some());

        cache.invalidate_shares("v1");
        assert!(cache.get(&QueryCacheKey::vault_shares("v1")).is_none());
    }

    #[test]
    fn test_domain_invalidate_preferences_and_subscription() {
        let cache = QueryCache::new();
        cache.set(&QueryCacheKey::vault_prefs("v1"), json!({"hours": 24}));
        cache.set(&QueryCacheKey::vault_notif_prefs("v1"), json!({"email": true}));
        cache.set(&QueryCacheKey::subscription("v1"), json!({"freq": "Daily"}));

        cache.invalidate_preferences("v1");
        assert!(cache.get(&QueryCacheKey::vault_prefs("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::vault_notif_prefs("v1")).is_none());
        assert!(cache.get(&QueryCacheKey::subscription("v1")).is_some());

        cache.invalidate_subscription("v1");
        assert!(cache.get(&QueryCacheKey::subscription("v1")).is_none());
    }
}
