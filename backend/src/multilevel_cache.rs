/// Multi-level caching strategy (L1: in-memory, L2: simulated persistent store).
///
/// Implements a two-level cache hierarchy:
/// - L1 (in-memory): Fast, small capacity, short TTL.
/// - L2 (persistent/Redis-compatible interface): Slower, large capacity, longer TTL.
///
/// Cache coherence between levels is maintained on write (write-through),
/// on miss (read-through with promotion), and via a scheduled consistency
/// verification job with automatic drift healing (#360).
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::models::{Vault, VaultSummary};

// ── Configuration ─────────────────────────────────────────────────────────────

/// L1 cache TTL: 1 minute (fast, in-process cache).
pub const L1_TTL_SECS: u64 = 60;

/// L2 cache TTL: 30 minutes (slower, larger capacity).
pub const L2_TTL_SECS: u64 = 1800;

/// L1 maximum entry count — evict LRU when exceeded.
pub const L1_MAX_ENTRIES: usize = 500;

// ── Consistency Drift Types (#360) ───────────────────────────────────────────

/// Nature of detected drift between L1 and L2 cache levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriftKind {
    /// Entry is present in L1 but missing or expired in L2.
    L1Only,
    /// Entry is present in L2 but missing or expired in L1.
    L2Only,
    /// Entry is present in both levels but the cached values disagree.
    ValueMismatch,
}

/// Information about a single drifted cache key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheDriftDetail {
    pub key: String,
    pub kind: DriftKind,
    pub description: String,
    pub healed: bool,
}

/// Report produced by multi-level cache consistency verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheDriftReport {
    pub checked_at: DateTime<Utc>,
    pub checked_keys_count: usize,
    pub drift_count: usize,
    pub healed_count: usize,
    pub details: Vec<CacheDriftDetail>,
}

/// Cumulative metrics for multi-level cache consistency checks and healing.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DriftMetrics {
    /// Total number of consistency verification runs executed.
    pub total_verifications: u64,
    /// Total number of drifted keys detected across all runs.
    pub total_drifts_detected: u64,
    /// Total number of drifted keys successfully healed from source of truth.
    pub total_drifts_healed: u64,
    /// Timestamp of the most recent consistency verification run.
    pub last_verification_at: Option<DateTime<Utc>>,
}

// ── Generic Cache Entry ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Entry<T> {
    value: T,
    inserted_at: Instant,
    ttl: Duration,
    access_count: u64,
    last_accessed: Instant,
}

impl<T: Clone> Entry<T> {
    fn new(value: T, ttl: Duration) -> Self {
        let now = Instant::now();
        Self {
            value,
            inserted_at: now,
            ttl,
            access_count: 0,
            last_accessed: now,
        }
    }

    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() >= self.ttl
    }

    fn access(&mut self) -> T {
        self.access_count += 1;
        self.last_accessed = Instant::now();
        self.value.clone()
    }
}

// ── L1 In-Memory Cache ────────────────────────────────────────────────────────

struct L1Cache {
    vaults: HashMap<String, Entry<Vault>>,
    ttl_remaining: HashMap<String, Entry<Option<u64>>>,
    summaries: HashMap<String, Entry<VaultSummary>>,
    max_entries: usize,
    stats: L1Stats,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct L1Stats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub insertions: u64,
}

impl L1Cache {
    fn new(max_entries: usize) -> Self {
        Self {
            vaults: HashMap::new(),
            ttl_remaining: HashMap::new(),
            summaries: HashMap::new(),
            max_entries,
            stats: L1Stats::default(),
        }
    }

    fn get_vault(&mut self, vault_id: &str) -> Option<Vault> {
        if let Some(entry) = self.vaults.get_mut(vault_id) {
            if !entry.is_expired() {
                self.stats.hits += 1;
                return Some(entry.access());
            }
            self.vaults.remove(vault_id);
        }
        self.stats.misses += 1;
        None
    }

    fn peek_vault(&self, vault_id: &str) -> Option<Vault> {
        self.vaults
            .get(vault_id)
            .filter(|e| !e.is_expired())
            .map(|e| e.value.clone())
    }

    fn set_vault(&mut self, vault_id: &str, vault: Vault, ttl: Duration) {
        self.maybe_evict(&mut self.vaults.len().clone());
        self.vaults
            .insert(vault_id.to_string(), Entry::new(vault, ttl));
        self.stats.insertions += 1;
    }

    fn get_ttl_remaining(&mut self, vault_id: &str) -> Option<Option<u64>> {
        if let Some(entry) = self.ttl_remaining.get_mut(vault_id) {
            if !entry.is_expired() {
                self.stats.hits += 1;
                return Some(entry.access());
            }
            self.ttl_remaining.remove(vault_id);
        }
        self.stats.misses += 1;
        None
    }

    fn peek_ttl_remaining(&self, vault_id: &str) -> Option<Option<u64>> {
        self.ttl_remaining
            .get(vault_id)
            .filter(|e| !e.is_expired())
            .map(|e| e.value)
    }

    fn set_ttl_remaining(&mut self, vault_id: &str, value: Option<u64>, ttl: Duration) {
        self.ttl_remaining
            .insert(vault_id.to_string(), Entry::new(value, ttl));
        self.stats.insertions += 1;
    }

    fn get_summary(&mut self, vault_id: &str) -> Option<VaultSummary> {
        if let Some(entry) = self.summaries.get_mut(vault_id) {
            if !entry.is_expired() {
                self.stats.hits += 1;
                return Some(entry.access());
            }
            self.summaries.remove(vault_id);
        }
        self.stats.misses += 1;
        None
    }

    fn peek_summary(&self, vault_id: &str) -> Option<VaultSummary> {
        self.summaries
            .get(vault_id)
            .filter(|e| !e.is_expired())
            .map(|e| e.value.clone())
    }

    fn set_summary(&mut self, vault_id: &str, summary: VaultSummary, ttl: Duration) {
        self.summaries
            .insert(vault_id.to_string(), Entry::new(summary, ttl));
        self.stats.insertions += 1;
    }

    fn invalidate(&mut self, vault_id: &str) {
        self.vaults.remove(vault_id);
        self.ttl_remaining.remove(vault_id);
        self.summaries.remove(vault_id);
    }

    fn invalidate_all(&mut self) {
        self.vaults.clear();
        self.ttl_remaining.clear();
        self.summaries.clear();
    }

    fn live_entry_count(&self) -> usize {
        let vault_count = self.vaults.values().filter(|e| !e.is_expired()).count();
        let ttl_count = self
            .ttl_remaining
            .values()
            .filter(|e| !e.is_expired())
            .count();
        let summary_count = self.summaries.values().filter(|e| !e.is_expired()).count();
        vault_count.max(ttl_count).max(summary_count)
    }

    fn all_vault_keys(&self) -> Vec<String> {
        self.vaults
            .iter()
            .filter(|(_, e)| !e.is_expired())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// LRU eviction: remove the least recently accessed entry when at capacity.
    fn maybe_evict(&mut self, current_len: &usize) {
        if *current_len >= self.max_entries {
            if let Some(oldest_key) = self
                .vaults
                .iter()
                .min_by_key(|(_, e)| e.last_accessed)
                .map(|(k, _)| k.clone())
            {
                self.vaults.remove(&oldest_key);
                self.stats.evictions += 1;
            }
        }
    }
}

// ── L2 Cache (Redis-compatible interface) ─────────────────────────────────────

struct L2Cache {
    vaults: HashMap<String, Entry<Vault>>,
    ttl_remaining: HashMap<String, Entry<Option<u64>>>,
    summaries: HashMap<String, Entry<VaultSummary>>,
    stats: L2Stats,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct L2Stats {
    pub hits: u64,
    pub misses: u64,
    pub promotions: u64, // Entries promoted from L2 → L1.
    pub insertions: u64,
}

impl L2Cache {
    fn new() -> Self {
        Self {
            vaults: HashMap::new(),
            ttl_remaining: HashMap::new(),
            summaries: HashMap::new(),
            stats: L2Stats::default(),
        }
    }

    fn get_vault(&mut self, vault_id: &str) -> Option<Vault> {
        if let Some(entry) = self.vaults.get_mut(vault_id) {
            if !entry.is_expired() {
                self.stats.hits += 1;
                return Some(entry.access());
            }
            self.vaults.remove(vault_id);
        }
        self.stats.misses += 1;
        None
    }

    fn peek_vault(&self, vault_id: &str) -> Option<Vault> {
        self.vaults
            .get(vault_id)
            .filter(|e| !e.is_expired())
            .map(|e| e.value.clone())
    }

    fn set_vault(&mut self, vault_id: &str, vault: Vault, ttl: Duration) {
        self.vaults
            .insert(vault_id.to_string(), Entry::new(vault, ttl));
        self.stats.insertions += 1;
    }

    fn get_ttl_remaining(&mut self, vault_id: &str) -> Option<Option<u64>> {
        if let Some(entry) = self.ttl_remaining.get_mut(vault_id) {
            if !entry.is_expired() {
                self.stats.hits += 1;
                return Some(entry.access());
            }
            self.ttl_remaining.remove(vault_id);
        }
        self.stats.misses += 1;
        None
    }

    fn peek_ttl_remaining(&self, vault_id: &str) -> Option<Option<u64>> {
        self.ttl_remaining
            .get(vault_id)
            .filter(|e| !e.is_expired())
            .map(|e| e.value)
    }

    fn set_ttl_remaining(&mut self, vault_id: &str, value: Option<u64>, ttl: Duration) {
        self.ttl_remaining
            .insert(vault_id.to_string(), Entry::new(value, ttl));
        self.stats.insertions += 1;
    }

    fn get_summary(&mut self, vault_id: &str) -> Option<VaultSummary> {
        if let Some(entry) = self.summaries.get_mut(vault_id) {
            if !entry.is_expired() {
                self.stats.hits += 1;
                return Some(entry.access());
            }
            self.summaries.remove(vault_id);
        }
        self.stats.misses += 1;
        None
    }

    fn peek_summary(&self, vault_id: &str) -> Option<VaultSummary> {
        self.summaries
            .get(vault_id)
            .filter(|e| !e.is_expired())
            .map(|e| e.value.clone())
    }

    fn set_summary(&mut self, vault_id: &str, summary: VaultSummary, ttl: Duration) {
        self.summaries
            .insert(vault_id.to_string(), Entry::new(summary, ttl));
        self.stats.insertions += 1;
    }

    fn invalidate(&mut self, vault_id: &str) {
        self.vaults.remove(vault_id);
        self.ttl_remaining.remove(vault_id);
        self.summaries.remove(vault_id);
    }

    fn invalidate_all(&mut self) {
        self.vaults.clear();
        self.ttl_remaining.clear();
        self.summaries.clear();
    }

    fn live_entry_count(&self) -> usize {
        let vault_count = self.vaults.values().filter(|e| !e.is_expired()).count();
        let ttl_count = self
            .ttl_remaining
            .values()
            .filter(|e| !e.is_expired())
            .count();
        let summary_count = self.summaries.values().filter(|e| !e.is_expired()).count();
        vault_count.max(ttl_count).max(summary_count)
    }

    fn all_vault_keys(&self) -> Vec<String> {
        self.vaults
            .iter()
            .filter(|(_, e)| !e.is_expired())
            .map(|(k, _)| k.clone())
            .collect()
    }
}

// ── Multi-Level Cache ─────────────────────────────────────────────────────────

/// Two-level cache with automatic read-through, write-through coherence, and
/// periodic consistency verification and drift healing (#360).
pub struct MultiLevelCache {
    l1: Mutex<L1Cache>,
    l2: Mutex<L2Cache>,
    l1_ttl: Duration,
    l2_ttl: Duration,
    drift_metrics: Mutex<DriftMetrics>,
}

impl MultiLevelCache {
    pub fn new() -> Self {
        Self {
            l1: Mutex::new(L1Cache::new(L1_MAX_ENTRIES)),
            l2: Mutex::new(L2Cache::new()),
            l1_ttl: Duration::from_secs(L1_TTL_SECS),
            l2_ttl: Duration::from_secs(L2_TTL_SECS),
            drift_metrics: Mutex::new(DriftMetrics::default()),
        }
    }

    /// Create with custom TTLs (useful for tests).
    pub fn with_ttls(l1_ttl: Duration, l2_ttl: Duration) -> Self {
        Self {
            l1: Mutex::new(L1Cache::new(L1_MAX_ENTRIES)),
            l2: Mutex::new(L2Cache::new()),
            l1_ttl,
            l2_ttl,
            drift_metrics: Mutex::new(DriftMetrics::default()),
        }
    }

    // ── get_vault ─────────────────────────────────────────────────────────────

    /// Retrieve a vault: L1 first, then L2 with promotion, then miss.
    pub fn get_vault(&self, vault_id: &str) -> Option<Vault> {
        // Try L1.
        {
            let mut l1 = self.l1.lock().unwrap();
            if let Some(v) = l1.get_vault(vault_id) {
                return Some(v);
            }
        }

        // Try L2 with promotion to L1.
        let mut l2 = self.l2.lock().unwrap();
        if let Some(v) = l2.get_vault(vault_id) {
            l2.stats.promotions += 1;
            drop(l2);

            // Promote to L1.
            let mut l1 = self.l1.lock().unwrap();
            l1.set_vault(vault_id, v.clone(), self.l1_ttl);
            return Some(v);
        }

        None
    }

    /// Write vault to both L1 and L2 (write-through).
    pub fn set_vault(&self, vault_id: &str, vault: Vault) {
        {
            let mut l1 = self.l1.lock().unwrap();
            l1.set_vault(vault_id, vault.clone(), self.l1_ttl);
        }
        {
            let mut l2 = self.l2.lock().unwrap();
            l2.set_vault(vault_id, vault, self.l2_ttl);
        }
    }

    // ── get_ttl_remaining ─────────────────────────────────────────────────────

    #[allow(clippy::option_option)]
    pub fn get_ttl_remaining(&self, vault_id: &str) -> Option<Option<u64>> {
        {
            let mut l1 = self.l1.lock().unwrap();
            if let Some(v) = l1.get_ttl_remaining(vault_id) {
                return Some(v);
            }
        }

        let mut l2 = self.l2.lock().unwrap();
        if let Some(v) = l2.get_ttl_remaining(vault_id) {
            drop(l2);
            let mut l1 = self.l1.lock().unwrap();
            l1.set_ttl_remaining(vault_id, v, self.l1_ttl);
            return Some(v);
        }

        None
    }

    pub fn set_ttl_remaining(&self, vault_id: &str, value: Option<u64>) {
        {
            let mut l1 = self.l1.lock().unwrap();
            l1.set_ttl_remaining(vault_id, value, self.l1_ttl);
        }
        {
            let mut l2 = self.l2.lock().unwrap();
            l2.set_ttl_remaining(vault_id, value, self.l2_ttl);
        }
    }

    // ── get_summary ───────────────────────────────────────────────────────────

    pub fn get_summary(&self, vault_id: &str) -> Option<VaultSummary> {
        {
            let mut l1 = self.l1.lock().unwrap();
            if let Some(v) = l1.get_summary(vault_id) {
                return Some(v);
            }
        }

        let mut l2 = self.l2.lock().unwrap();
        if let Some(v) = l2.get_summary(vault_id) {
            drop(l2);
            let mut l1 = self.l1.lock().unwrap();
            l1.set_summary(vault_id, v.clone(), self.l1_ttl);
            return Some(v);
        }

        None
    }

    pub fn set_summary(&self, vault_id: &str, summary: VaultSummary) {
        {
            let mut l1 = self.l1.lock().unwrap();
            l1.set_summary(vault_id, summary.clone(), self.l1_ttl);
        }
        {
            let mut l2 = self.l2.lock().unwrap();
            l2.set_summary(vault_id, summary, self.l2_ttl);
        }
    }

    // ── Invalidation ──────────────────────────────────────────────────────────

    /// Invalidate all cached data for a vault in both levels.
    pub fn invalidate(&self, vault_id: &str) {
        self.l1.lock().unwrap().invalidate(vault_id);
        self.l2.lock().unwrap().invalidate(vault_id);
    }

    /// Flush both cache levels entirely.
    pub fn invalidate_all(&self) {
        self.l1.lock().unwrap().invalidate_all();
        self.l2.lock().unwrap().invalidate_all();
    }

    // ── #360 Consistency Verification & Drift Healing ────────────────────────

    /// Verify consistency between L1 and L2 cache entries, reporting any detected drift
    /// and auto-healing entries by refreshing from the given `source_of_truth` resolver.
    pub fn verify_and_heal_consistency<F>(&self, source_of_truth: F) -> CacheDriftReport
    where
        F: Fn(&str) -> Option<Vault>,
    {
        let now = Utc::now();
        let mut keys: HashSet<String> = HashSet::new();

        {
            let l1 = self.l1.lock().unwrap();
            for k in l1.all_vault_keys() {
                keys.insert(k);
            }
        }
        {
            let l2 = self.l2.lock().unwrap();
            for k in l2.all_vault_keys() {
                keys.insert(k);
            }
        }

        let checked_keys_count = keys.len();
        let mut details: Vec<CacheDriftDetail> = Vec::new();

        for key in &keys {
            let l1_val = self.l1.lock().unwrap().peek_vault(key);
            let l2_val = self.l2.lock().unwrap().peek_vault(key);

            let drift_opt = match (&l1_val, &l2_val) {
                (Some(_), None) => Some((
                    DriftKind::L1Only,
                    format!("Key '{key}' exists in L1 but is missing from L2"),
                )),
                (None, Some(_)) => Some((
                    DriftKind::L2Only,
                    format!("Key '{key}' exists in L2 but is missing from L1"),
                )),
                (Some(v1), Some(v2)) if v1 != v2 => Some((
                    DriftKind::ValueMismatch,
                    format!("Values for '{key}' differ between L1 and L2 (balance: {} vs {})", v1.balance, v2.balance),
                )),
                _ => None,
            };

            if let Some((kind, description)) = drift_opt {
                // Auto-heal by consulting source of truth
                let healed = match source_of_truth(key) {
                    Some(truth) => {
                        self.set_vault(key, truth);
                        true
                    }
                    None => {
                        // Record does not exist in source of truth; purge from both cache levels.
                        self.invalidate(key);
                        true
                    }
                };

                details.push(CacheDriftDetail {
                    key: key.clone(),
                    kind,
                    description,
                    healed,
                });
            }
        }

        let drift_count = details.len();
        let healed_count = details.iter().filter(|d| d.healed).count();

        // Update drift metrics
        {
            let mut m = self.drift_metrics.lock().unwrap();
            m.total_verifications += 1;
            m.total_drifts_detected += drift_count as u64;
            m.total_drifts_healed += healed_count as u64;
            m.last_verification_at = Some(now);
        }

        CacheDriftReport {
            checked_at: now,
            checked_keys_count,
            drift_count,
            healed_count,
            details,
        }
    }

    /// Read-only consistency check between L1 and L2 without altering cache contents.
    pub fn verify_consistency(&self) -> CacheDriftReport {
        let now = Utc::now();
        let mut keys: HashSet<String> = HashSet::new();

        {
            let l1 = self.l1.lock().unwrap();
            for k in l1.all_vault_keys() {
                keys.insert(k);
            }
        }
        {
            let l2 = self.l2.lock().unwrap();
            for k in l2.all_vault_keys() {
                keys.insert(k);
            }
        }

        let checked_keys_count = keys.len();
        let mut details: Vec<CacheDriftDetail> = Vec::new();

        for key in &keys {
            let l1_val = self.l1.lock().unwrap().peek_vault(key);
            let l2_val = self.l2.lock().unwrap().peek_vault(key);

            let drift_opt = match (&l1_val, &l2_val) {
                (Some(_), None) => Some((
                    DriftKind::L1Only,
                    format!("Key '{key}' exists in L1 but is missing from L2"),
                )),
                (None, Some(_)) => Some((
                    DriftKind::L2Only,
                    format!("Key '{key}' exists in L2 but is missing from L1"),
                )),
                (Some(v1), Some(v2)) if v1 != v2 => Some((
                    DriftKind::ValueMismatch,
                    format!("Values for '{key}' differ between L1 and L2"),
                )),
                _ => None,
            };

            if let Some((kind, description)) = drift_opt {
                details.push(CacheDriftDetail {
                    key: key.clone(),
                    kind,
                    description,
                    healed: false,
                });
            }
        }

        let drift_count = details.len();

        {
            let mut m = self.drift_metrics.lock().unwrap();
            m.total_verifications += 1;
            m.total_drifts_detected += drift_count as u64;
            m.last_verification_at = Some(now);
        }

        CacheDriftReport {
            checked_at: now,
            checked_keys_count,
            drift_count,
            healed_count: 0,
            details,
        }
    }

    /// Snapshot current drift metrics.
    pub fn drift_metrics(&self) -> DriftMetrics {
        self.drift_metrics.lock().unwrap().clone()
    }

    /// Reset drift metrics (useful in tests).
    pub fn reset_drift_metrics(&self) {
        let mut m = self.drift_metrics.lock().unwrap();
        *m = DriftMetrics::default();
    }

    // ── Statistics ────────────────────────────────────────────────────────────

    /// Get per-level cache statistics and drift metrics.
    pub fn get_stats(&self) -> CacheStats {
        let l1_stats = self.l1.lock().unwrap().stats.clone();
        let l2_stats = self.l2.lock().unwrap().stats.clone();
        let l1_entries = self.l1.lock().unwrap().live_entry_count();
        let l2_entries = self.l2.lock().unwrap().live_entry_count();
        let drift = self.drift_metrics();

        CacheStats {
            l1: l1_stats,
            l2: l2_stats,
            l1_live_entries: l1_entries,
            l2_live_entries: l2_entries,
            drift,
        }
    }
}

impl Default for MultiLevelCache {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheStats {
    pub l1: L1Stats,
    pub l2: L2Stats,
    pub l1_live_entries: usize,
    pub l2_live_entries: usize,
    pub drift: DriftMetrics,
}

// ── Unit Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Vault, VaultStatus, VaultSummary};
    use chrono::Utc;

    fn make_vault(id: &str) -> Vault {
        Vault {
            id: id.to_string(),
            owner: "owner1".to_string(),
            beneficiary: "ben1".to_string(),
            balance: 1000,
            check_in_interval: 86400,
            last_check_in: Utc::now(),
            created_at: Utc::now(),
            status: VaultStatus::Active,
            ttl_remaining: Some(86400),
        }
    }

    fn make_summary(vault_id: &str) -> VaultSummary {
        VaultSummary {
            vault_id: vault_id.to_string(),
            owner: "owner1".to_string(),
            status: VaultStatus::Active,
            ttl_remaining: Some(86400),
            balance: 1000,
        }
    }

    #[test]
    fn test_set_and_get_vault_l1_hit() {
        let cache = MultiLevelCache::new();
        cache.set_vault("v1", make_vault("v1"));

        let result = cache.get_vault("v1");
        assert!(result.is_some());
        assert_eq!(result.unwrap().id, "v1");

        let stats = cache.get_stats();
        assert_eq!(stats.l1.hits, 1);
    }

    #[test]
    fn test_l2_fallback_on_l1_miss() {
        // Use a very short L1 TTL and longer L2 TTL.
        let cache = MultiLevelCache::with_ttls(Duration::from_millis(1), Duration::from_secs(60));
        cache.set_vault("v1", make_vault("v1"));

        // Wait for L1 to expire.
        std::thread::sleep(Duration::from_millis(5));

        // L1 miss → should fall back to L2.
        let result = cache.get_vault("v1");
        assert!(result.is_some());

        let stats = cache.get_stats();
        assert_eq!(stats.l1.misses, 1);
        assert_eq!(stats.l2.hits, 1);
    }

    #[test]
    fn test_l2_promotion_to_l1() {
        let cache = MultiLevelCache::with_ttls(Duration::from_millis(1), Duration::from_secs(60));
        cache.set_vault("v1", make_vault("v1"));
        std::thread::sleep(Duration::from_millis(5));

        // First access: L1 miss → L2 hit → promote to L1.
        cache.get_vault("v1");

        let stats = cache.get_stats();
        assert_eq!(stats.l2.promotions, 1);
    }

    #[test]
    fn test_invalidate_clears_both_levels() {
        let cache = MultiLevelCache::new();
        cache.set_vault("v1", make_vault("v1"));

        cache.invalidate("v1");

        assert!(cache.get_vault("v1").is_none());

        let stats = cache.get_stats();
        assert_eq!(stats.l1.misses, 1);
        assert_eq!(stats.l2.misses, 1);
    }

    #[test]
    fn test_invalidate_all_clears_both_levels() {
        let cache = MultiLevelCache::new();
        cache.set_vault("v1", make_vault("v1"));
        cache.set_vault("v2", make_vault("v2"));

        cache.invalidate_all();

        assert!(cache.get_vault("v1").is_none());
        assert!(cache.get_vault("v2").is_none());
    }

    #[test]
    fn test_cache_miss_returns_none() {
        let cache = MultiLevelCache::new();
        assert!(cache.get_vault("nonexistent").is_none());
    }

    #[test]
    fn test_set_and_get_ttl_remaining() {
        let cache = MultiLevelCache::new();
        cache.set_ttl_remaining("v1", Some(3600));

        let result = cache.get_ttl_remaining("v1");
        assert_eq!(result, Some(Some(3600)));
    }

    #[test]
    fn test_set_and_get_summary() {
        let cache = MultiLevelCache::new();
        cache.set_summary("v1", make_summary("v1"));

        let result = cache.get_summary("v1");
        assert!(result.is_some());
        assert_eq!(result.unwrap().vault_id, "v1");
    }

    #[test]
    fn test_stats_track_hits_and_misses() {
        let cache = MultiLevelCache::new();
        cache.set_vault("v1", make_vault("v1"));

        cache.get_vault("v1"); // Hit in L1.
        cache.get_vault("missing"); // Miss in both.

        let stats = cache.get_stats();
        assert_eq!(stats.l1.hits, 1);
        assert_eq!(stats.l1.misses, 1);
    }

    #[test]
    fn test_stats_track_live_entries() {
        let cache = MultiLevelCache::new();
        cache.set_vault("v1", make_vault("v1"));
        cache.set_vault("v2", make_vault("v2"));

        let stats = cache.get_stats();
        assert_eq!(stats.l1_live_entries, 2);
        assert_eq!(stats.l2_live_entries, 2);
    }

    // ── #360 Tests: Drift Detection & Healing ─────────────────────────────────

    #[test]
    fn test_drift_detection_value_mismatch() {
        let cache = MultiLevelCache::new();
        let mut vault_l1 = make_vault("v1");
        vault_l1.balance = 1000;
        let mut vault_l2 = make_vault("v1");
        vault_l2.balance = 2000;

        // Manually inject mismatched values into L1 and L2
        cache.l1.lock().unwrap().set_vault("v1", vault_l1, Duration::from_secs(60));
        cache.l2.lock().unwrap().set_vault("v1", vault_l2, Duration::from_secs(60));

        let report = cache.verify_consistency();
        assert_eq!(report.drift_count, 1);
        assert_eq!(report.details[0].key, "v1");
        assert_eq!(report.details[0].kind, DriftKind::ValueMismatch);

        let metrics = cache.drift_metrics();
        assert_eq!(metrics.total_drifts_detected, 1);
    }

    #[test]
    fn test_drift_detection_l1_only_and_l2_only() {
        let cache = MultiLevelCache::new();
        let vault1 = make_vault("v1");
        let vault2 = make_vault("v2");

        // v1 in L1 only
        cache.l1.lock().unwrap().set_vault("v1", vault1, Duration::from_secs(60));
        // v2 in L2 only
        cache.l2.lock().unwrap().set_vault("v2", vault2, Duration::from_secs(60));

        let report = cache.verify_consistency();
        assert_eq!(report.drift_count, 2);

        let v1_detail = report.details.iter().find(|d| d.key == "v1").unwrap();
        let v2_detail = report.details.iter().find(|d| d.key == "v2").unwrap();

        assert_eq!(v1_detail.kind, DriftKind::L1Only);
        assert_eq!(v2_detail.kind, DriftKind::L2Only);
    }

    #[test]
    fn test_auto_heal_from_source_of_truth() {
        let cache = MultiLevelCache::new();
        let mut vault_stale = make_vault("v1");
        vault_stale.balance = 500;
        cache.l1.lock().unwrap().set_vault("v1", vault_stale, Duration::from_secs(60));

        // Source of truth has the authoritative state (balance 5000)
        let true_vault = {
            let mut v = make_vault("v1");
            v.balance = 5000;
            v
        };

        let report = cache.verify_and_heal_consistency(|key| {
            if key == "v1" {
                Some(true_vault.clone())
            } else {
                None
            }
        });

        assert_eq!(report.drift_count, 1);
        assert_eq!(report.healed_count, 1);
        assert!(report.details[0].healed);

        // Verify both L1 and L2 now reflect authoritative source of truth
        let l1_val = cache.l1.lock().unwrap().peek_vault("v1").unwrap();
        let l2_val = cache.l2.lock().unwrap().peek_vault("v1").unwrap();
        assert_eq!(l1_val.balance, 5000);
        assert_eq!(l2_val.balance, 5000);

        // Verification after healing reports 0 drift
        let fresh_report = cache.verify_consistency();
        assert_eq!(fresh_report.drift_count, 0);

        let metrics = cache.drift_metrics();
        assert_eq!(metrics.total_drifts_healed, 1);
    }

    #[test]
    fn test_auto_heal_clears_deleted_vault_from_cache() {
        let cache = MultiLevelCache::new();
        cache.l1.lock().unwrap().set_vault("v-deleted", make_vault("v-deleted"), Duration::from_secs(60));

        // Source of truth returns None (record was deleted in DB)
        let report = cache.verify_and_heal_consistency(|_| None);
        assert_eq!(report.drift_count, 1);
        assert_eq!(report.healed_count, 1);

        // Both cache levels should be cleared
        assert!(cache.get_vault("v-deleted").is_none());
    }

    #[test]
    fn test_drift_metrics_reporting() {
        let cache = MultiLevelCache::new();
        cache.reset_drift_metrics();

        let metrics_initial = cache.drift_metrics();
        assert_eq!(metrics_initial.total_verifications, 0);
        assert_eq!(metrics_initial.total_drifts_detected, 0);
        assert_eq!(metrics_initial.total_drifts_healed, 0);

        // Inject drift and run healing
        cache.l1.lock().unwrap().set_vault("v1", make_vault("v1"), Duration::from_secs(60));
        let true_v = make_vault("v1");
        cache.verify_and_heal_consistency(|_| Some(true_v.clone()));

        let metrics_after = cache.drift_metrics();
        assert_eq!(metrics_after.total_verifications, 1);
        assert_eq!(metrics_after.total_drifts_detected, 1);
        assert_eq!(metrics_after.total_drifts_healed, 1);
        assert!(metrics_after.last_verification_at.is_some());
    }
}
