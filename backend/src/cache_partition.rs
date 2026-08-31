/// Cache partitioning for multi-tenant isolation (#92) and
/// consistent hashing with partition rebalancing (#362).
///
/// Each tenant operates within its own logical partition of the cache,
/// ensuring that keys from one tenant cannot collide with or leak into
/// another tenant's data. Consistent hashing distributes keys across
/// partitions with minimal key movement on partition additions/removals,
/// and per-partition load tracking detects hot partitions.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ── Key namespacing ───────────────────────────────────────────────────────────

/// Separator used between tenant ID and cache key.
const KEY_SEP: char = ':';

/// Return a namespaced cache key for the given tenant and logical key.
///
/// ```
/// # use ethos_protocol_backend::cache_partition::namespaced_key;
/// let k = namespaced_key("tenant_abc", "vault:42");
/// assert_eq!(k, "tenant_abc:vault:42");
/// ```
pub fn namespaced_key(tenant_id: &str, key: &str) -> String {
    format!("{tenant_id}{KEY_SEP}{key}")
}

/// Extract the tenant ID and original key from a namespaced key.
/// Returns `None` if the key does not contain the separator.
pub fn parse_namespaced_key(namespaced: &str) -> Option<(&str, &str)> {
    namespaced
        .find(KEY_SEP)
        .map(|pos| (&namespaced[..pos], &namespaced[pos + 1..]))
}

// ── Partition entry ───────────────────────────────────────────────────────────

#[derive(Clone)]
struct PartitionEntry {
    value: String,
    inserted_at: Instant,
    ttl: Duration,
}

impl PartitionEntry {
    fn new(value: String, ttl: Duration) -> Self {
        Self {
            value,
            inserted_at: Instant::now(),
            ttl,
        }
    }

    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() >= self.ttl
    }
}

// ── Per-tenant partition statistics ──────────────────────────────────────────

/// Statistics for a single tenant partition.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartitionStats {
    /// Number of keys currently alive (non-expired) in this partition.
    pub live_keys: usize,
    /// Cumulative cache hits for this partition since creation.
    pub hits: u64,
    /// Cumulative cache misses for this partition since creation.
    pub misses: u64,
    /// Cumulative evictions (expired entries removed on read) for this partition.
    pub evictions: u64,
}

impl PartitionStats {
    /// Cache-hit ratio in the range `[0.0, 1.0]`.  Returns `None` when no
    /// accesses have been recorded yet.
    pub fn hit_ratio(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        if total == 0 {
            None
        } else {
            Some(self.hits as f64 / total as f64)
        }
    }
}

// ── Consistent Hashing (#362) ────────────────────────────────────────────────

/// Default number of virtual nodes per partition on the consistent hash ring.
pub const DEFAULT_VNODES_PER_PARTITION: usize = 100;

/// Consistent hash ring with virtual nodes for key-to-partition assignment (#362).
#[derive(Debug, Clone)]
pub struct ConsistentHashRing {
    vnodes_per_partition: usize,
    ring: BTreeMap<u64, String>,
    partitions: HashSet<String>,
}

impl ConsistentHashRing {
    /// Create a new consistent hash ring with the specified number of virtual nodes per partition.
    pub fn new(vnodes_per_partition: usize) -> Self {
        Self {
            vnodes_per_partition: vnodes_per_partition.max(1),
            ring: BTreeMap::new(),
            partitions: HashSet::new(),
        }
    }

    /// Create a consistent hash ring initialized with a list of partition identifiers.
    pub fn with_partitions(partitions: &[&str], vnodes_per_partition: usize) -> Self {
        let mut ring = Self::new(vnodes_per_partition);
        for &p in partitions {
            ring.add_partition(p);
        }
        ring
    }

    /// Add a partition to the ring and distribute its virtual nodes.
    pub fn add_partition(&mut self, partition_id: &str) {
        if self.partitions.insert(partition_id.to_string()) {
            for i in 0..self.vnodes_per_partition {
                let vnode_key = format!("{partition_id}#vnode_{i}");
                let mut hasher = DefaultHasher::new();
                vnode_key.hash(&mut hasher);
                let hash = hasher.finish();
                self.ring.insert(hash, partition_id.to_string());
            }
        }
    }

    /// Remove a partition and its virtual nodes from the ring.
    pub fn remove_partition(&mut self, partition_id: &str) {
        if self.partitions.remove(partition_id) {
            self.ring.retain(|_, v| v != partition_id);
        }
    }

    /// Assign a key to a partition using consistent hashing.
    ///
    /// Finds the nearest virtual node clockwise on the hash ring.
    pub fn get_partition(&self, key: &str) -> Option<&str> {
        if self.ring.is_empty() {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();

        match self.ring.range(hash..).next() {
            Some((_, partition)) => Some(partition.as_str()),
            None => self.ring.values().next().map(|s| s.as_str()),
        }
    }

    /// Returns the number of partitions on the ring.
    pub fn partition_count(&self) -> usize {
        self.partitions.len()
    }

    /// Returns a sorted list of all active partition names.
    pub fn partitions(&self) -> Vec<String> {
        let mut p: Vec<_> = self.partitions.iter().cloned().collect();
        p.sort();
        p
    }
}

impl Default for ConsistentHashRing {
    fn default() -> Self {
        Self::new(DEFAULT_VNODES_PER_PARTITION)
    }
}

// ── Rebalancing and Load Metrics (#362) ──────────────────────────────────────

/// Details of a single key migration during partition rebalancing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMigration {
    pub key: String,
    pub from_partition: String,
    pub to_partition: String,
}

/// Outcome of a partition rebalancing routine (#362).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebalanceResult {
    /// Number of keys migrated to new partitions.
    pub migrated_keys: usize,
    /// Total number of live keys inspected across all partitions.
    pub total_keys: usize,
    /// Detailed list of key migrations.
    pub migrations: Vec<KeyMigration>,
}

/// Load metrics for a partition to detect hot partitions (#362).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartitionLoad {
    pub partition_id: String,
    pub live_keys: usize,
    pub total_operations: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    /// Ratio of operations on this partition relative to the average partition load.
    pub load_factor: f64,
    /// True if this partition's load factor exceeds the hot partition threshold.
    pub is_hot: bool,
}

// ── Tenant partition (internal) ───────────────────────────────────────────────

struct TenantPartition {
    data: HashMap<String, PartitionEntry>,
    hits: u64,
    misses: u64,
    evictions: u64,
    sets: u64,
}

impl TenantPartition {
    fn new() -> Self {
        Self {
            data: HashMap::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
            sets: 0,
        }
    }

    /// Insert a value under the given (non-namespaced) key.
    fn insert(&mut self, key: &str, value: String, ttl: Duration) {
        self.sets += 1;
        self.data
            .insert(key.to_string(), PartitionEntry::new(value, ttl));
    }

    /// Insert a pre-existing entry (used during rebalancing migration).
    fn insert_entry(&mut self, key: &str, entry: PartitionEntry) {
        self.sets += 1;
        self.data.insert(key.to_string(), entry);
    }

    /// Get a value. Returns `None` on miss or expiry (and bumps the
    /// eviction counter if the entry was expired).
    fn get(&mut self, key: &str) -> Option<String> {
        match self.data.get(key) {
            Some(entry) if !entry.is_expired() => {
                self.hits += 1;
                Some(entry.value.clone())
            }
            Some(_expired) => {
                self.evictions += 1;
                self.misses += 1;
                self.data.remove(key);
                None
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Remove a single key from the partition.
    fn remove(&mut self, key: &str) {
        self.data.remove(key);
    }

    /// Remove all entries from the partition.
    fn clear(&mut self) {
        self.data.clear();
    }

    /// Count live (non-expired) keys.
    fn live_key_count(&self) -> usize {
        self.data.values().filter(|e| !e.is_expired()).count()
    }

    /// Total operations performed on this partition (gets + misses + sets).
    fn total_operations(&self) -> u64 {
        self.hits + self.misses + self.sets
    }

    fn stats(&self) -> PartitionStats {
        PartitionStats {
            live_keys: self.live_key_count(),
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }
}

// ── Public PartitionedCache ───────────────────────────────────────────────────

/// Thread-safe, partitioned cache supporting multi-tenant isolation (#92)
/// and consistent hashing with rebalancing (#362).
///
/// Entries can be stored under tenant-namespaced keys (`tenant:key`) or routed
/// across dynamic partitions via consistent hashing.
///
/// # Example
/// ```
/// # use std::time::Duration;
/// # use ethos_protocol_backend::cache_partition::PartitionedCache;
/// let cache = PartitionedCache::new(Duration::from_secs(300));
///
/// cache.set("tenant_a", "vault:1", "data".to_string());
/// assert_eq!(cache.get("tenant_a", "vault:1"), Some("data".to_string()));
///
/// // tenant_b cannot see tenant_a's data
/// assert!(cache.get("tenant_b", "vault:1").is_none());
/// ```
pub struct PartitionedCache {
    partitions: Arc<Mutex<HashMap<String, TenantPartition>>>,
    ring: Arc<Mutex<ConsistentHashRing>>,
    default_ttl: Duration,
}

impl PartitionedCache {
    /// Create a new cache with the given default TTL.
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            partitions: Arc::new(Mutex::new(HashMap::new())),
            ring: Arc::new(Mutex::new(ConsistentHashRing::new(DEFAULT_VNODES_PER_PARTITION))),
            default_ttl,
        }
    }

    /// Create with the default 5-minute TTL.
    pub fn with_default_ttl() -> Self {
        Self::new(Duration::from_secs(300))
    }

    /// Create a cache pre-configured with the given consistent hash partition names (#362).
    pub fn with_consistent_partitions(partitions: &[&str], default_ttl: Duration) -> Self {
        let cache = Self::new(default_ttl);
        for &p in partitions {
            cache.add_partition(p);
        }
        cache
    }

    // ── Partition Management & Rebalancing (#362) ──────────────────────────────

    /// Add a partition to the consistent hash ring and initialize its storage.
    pub fn add_partition(&self, partition_id: &str) {
        let mut ring = self.ring.lock().unwrap();
        ring.add_partition(partition_id);
        let mut partitions = self.partitions.lock().unwrap();
        partitions
            .entry(partition_id.to_string())
            .or_insert_with(TenantPartition::new);
    }

    /// Add a partition and immediately rebalance existing keys, returning a migration report.
    pub fn add_partition_and_rebalance(&self, partition_id: &str) -> RebalanceResult {
        self.add_partition(partition_id);
        self.rebalance()
    }

    /// Remove a partition from the consistent hash ring.
    pub fn remove_partition(&self, partition_id: &str) {
        let mut ring = self.ring.lock().unwrap();
        ring.remove_partition(partition_id);
    }

    /// Remove a partition and rebalance all its keys to remaining partitions.
    pub fn remove_partition_and_rebalance(&self, partition_id: &str) -> RebalanceResult {
        self.remove_partition(partition_id);
        self.rebalance()
    }

    /// Assign a key to a partition based on the consistent hash ring.
    pub fn partition_for_key(&self, key: &str) -> Option<String> {
        self.ring
            .lock()
            .unwrap()
            .get_partition(key)
            .map(|s| s.to_string())
    }

    /// Rebalance keys across partitions using consistent hashing (#362).
    ///
    /// Only keys whose assigned partition under the current hash ring differs
    /// from their current location are migrated.
    pub fn rebalance(&self) -> RebalanceResult {
        let ring = self.ring.lock().unwrap().clone();
        if ring.partition_count() == 0 {
            return RebalanceResult::default();
        }

        let mut partitions = self.partitions.lock().unwrap();
        let mut to_migrate = Vec::new();
        let mut total_keys = 0;

        // Collect keys that need to move
        for (current_part_id, part) in partitions.iter_mut() {
            let mut keys_to_remove = Vec::new();
            for (key, entry) in part.data.iter() {
                if !entry.is_expired() {
                    total_keys += 1;
                    if let Some(target_part_id) = ring.get_partition(key) {
                        if target_part_id != current_part_id {
                            keys_to_remove.push((key.clone(), target_part_id.to_string()));
                        }
                    }
                }
            }
            for (key, target_part_id) in keys_to_remove {
                if let Some(entry) = part.data.remove(&key) {
                    to_migrate.push((key, entry, current_part_id.clone(), target_part_id));
                }
            }
        }

        let migrated_keys = to_migrate.len();
        let mut migrations = Vec::with_capacity(migrated_keys);

        // Insert migrated keys into target partitions
        for (key, entry, from, to) in to_migrate {
            partitions
                .entry(to.clone())
                .or_insert_with(TenantPartition::new)
                .insert_entry(&key, entry);
            migrations.push(KeyMigration {
                key,
                from_partition: from,
                to_partition: to,
            });
        }

        RebalanceResult {
            migrated_keys,
            total_keys,
            migrations,
        }
    }

    // ── Routed operations (#362) ──────────────────────────────────────────────

    /// Insert a key using consistent hashing partition routing.
    ///
    /// Returns the name of the partition the key was assigned to, or `None` if no partitions exist.
    pub fn set_routed(&self, key: &str, value: String) -> Option<String> {
        let partition = self.partition_for_key(key)?;
        self.set(&partition, key, value);
        Some(partition)
    }

    /// Retrieve a key using consistent hashing partition routing.
    pub fn get_routed(&self, key: &str) -> Option<String> {
        let partition = self.partition_for_key(key)?;
        self.get(&partition, key)
    }

    /// Remove a key using consistent hashing partition routing.
    pub fn remove_routed(&self, key: &str) -> Option<String> {
        let partition = self.partition_for_key(key)?;
        self.remove(&partition, key);
        Some(partition)
    }

    // ── Partition load & hot partition detection (#362) ───────────────────────

    /// Return load metrics for a given partition.
    pub fn partition_load(&self, partition_id: &str) -> Option<PartitionLoad> {
        let partitions = self.partitions.lock().unwrap();
        let p = partitions.get(partition_id)?;
        let total_parts = partitions.len();
        let sum_ops: u64 = partitions.values().map(|part| part.total_operations()).sum();
        let mean_ops = if total_parts > 0 {
            sum_ops as f64 / total_parts as f64
        } else {
            0.0
        };

        let my_ops = p.total_operations();
        let load_factor = if mean_ops > 0.0 {
            my_ops as f64 / mean_ops
        } else if my_ops > 0 {
            1.0
        } else {
            0.0
        };

        Some(PartitionLoad {
            partition_id: partition_id.to_string(),
            live_keys: p.live_key_count(),
            total_operations: my_ops,
            hits: p.hits,
            misses: p.misses,
            evictions: p.evictions,
            load_factor,
            is_hot: load_factor > 1.5,
        })
    }

    /// Return load metrics for all known partitions.
    pub fn all_partition_loads(&self) -> Vec<PartitionLoad> {
        let partitions = self.partitions.lock().unwrap();
        let total_parts = partitions.len();
        let sum_ops: u64 = partitions.values().map(|p| p.total_operations()).sum();
        let mean_ops = if total_parts > 0 {
            sum_ops as f64 / total_parts as f64
        } else {
            0.0
        };

        let mut loads: Vec<_> = partitions
            .iter()
            .map(|(id, p)| {
                let my_ops = p.total_operations();
                let load_factor = if mean_ops > 0.0 {
                    my_ops as f64 / mean_ops
                } else if my_ops > 0 {
                    1.0
                } else {
                    0.0
                };
                PartitionLoad {
                    partition_id: id.clone(),
                    live_keys: p.live_key_count(),
                    total_operations: my_ops,
                    hits: p.hits,
                    misses: p.misses,
                    evictions: p.evictions,
                    load_factor,
                    is_hot: load_factor > 1.5,
                }
            })
            .collect();
        loads.sort_by(|a, b| a.partition_id.cmp(&b.partition_id));
        loads
    }

    /// Detect partitions whose load factor exceeds `threshold_factor` (e.g. 1.5).
    pub fn hot_partitions(&self, threshold_factor: f64) -> Vec<PartitionLoad> {
        self.all_partition_loads()
            .into_iter()
            .filter(|l| l.load_factor >= threshold_factor && l.total_operations > 0)
            .collect()
    }

    // ── Core get / set / remove ───────────────────────────────────────────────

    /// Insert a value for the given tenant/partition and key using the default TTL.
    pub fn set(&self, tenant_id: &str, key: &str, value: String) {
        self.set_with_ttl(tenant_id, key, value, self.default_ttl);
    }

    /// Insert a value with a custom TTL.
    pub fn set_with_ttl(&self, tenant_id: &str, key: &str, value: String, ttl: Duration) {
        let mut partitions = self.partitions.lock().unwrap();
        partitions
            .entry(tenant_id.to_string())
            .or_insert_with(TenantPartition::new)
            .insert(key, value, ttl);
    }

    /// Retrieve a value for the given tenant/partition and key.
    ///
    /// Returns `None` on cache miss or if the entry has expired.
    pub fn get(&self, tenant_id: &str, key: &str) -> Option<String> {
        let mut partitions = self.partitions.lock().unwrap();
        partitions
            .get_mut(tenant_id)
            .and_then(|p| p.get(key))
    }

    /// Remove a single key from the given tenant's partition.
    pub fn remove(&self, tenant_id: &str, key: &str) {
        let mut partitions = self.partitions.lock().unwrap();
        if let Some(p) = partitions.get_mut(tenant_id) {
            p.remove(key);
        }
    }

    /// Remove all entries for the given tenant.
    pub fn clear_tenant(&self, tenant_id: &str) {
        let mut partitions = self.partitions.lock().unwrap();
        if let Some(p) = partitions.get_mut(tenant_id) {
            p.clear();
        }
    }

    /// Remove all entries across all tenants.
    pub fn clear_all(&self) {
        let mut partitions = self.partitions.lock().unwrap();
        for p in partitions.values_mut() {
            p.clear();
        }
    }

    // ── Isolation enforcement ─────────────────────────────────────────────────

    /// Verify that `tenant_id` does not contain the key-separator character,
    /// which could be used to craft a key that crosses tenant boundaries.
    ///
    /// Returns `Err` if the tenant ID is invalid.
    pub fn validate_tenant_id(tenant_id: &str) -> Result<(), String> {
        if tenant_id.is_empty() {
            return Err("tenant_id must not be empty".into());
        }
        if tenant_id.contains(KEY_SEP) {
            return Err(format!(
                "tenant_id must not contain the separator character '{KEY_SEP}'"
            ));
        }
        Ok(())
    }

    /// Like `set`, but returns `Err` if the tenant ID is invalid.
    pub fn set_safe(
        &self,
        tenant_id: &str,
        key: &str,
        value: String,
    ) -> Result<(), String> {
        Self::validate_tenant_id(tenant_id)?;
        self.set(tenant_id, key, value);
        Ok(())
    }

    /// Like `get`, but returns `Err` if the tenant ID is invalid.
    pub fn get_safe(
        &self,
        tenant_id: &str,
        key: &str,
    ) -> Result<Option<String>, String> {
        Self::validate_tenant_id(tenant_id)?;
        Ok(self.get(tenant_id, key))
    }

    // ── Partition statistics ──────────────────────────────────────────────────

    /// Return statistics for a single tenant's partition.
    ///
    /// Returns `None` if the tenant has never accessed the cache.
    pub fn partition_stats(&self, tenant_id: &str) -> Option<PartitionStats> {
        let partitions = self.partitions.lock().unwrap();
        partitions.get(tenant_id).map(|p| p.stats())
    }

    /// Return statistics for all known tenant partitions.
    pub fn all_partition_stats(&self) -> HashMap<String, PartitionStats> {
        let partitions = self.partitions.lock().unwrap();
        partitions
            .iter()
            .map(|(id, p)| (id.clone(), p.stats()))
            .collect()
    }

    /// Return the number of known tenant partitions (including empty ones).
    pub fn tenant_count(&self) -> usize {
        self.partitions.lock().unwrap().len()
    }
}

impl Default for PartitionedCache {
    fn default() -> Self {
        Self::with_default_ttl()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── namespaced_key ────────────────────────────────────────────────────────

    #[test]
    fn test_namespaced_key_format() {
        assert_eq!(namespaced_key("tenant_a", "vault:1"), "tenant_a:vault:1");
    }

    #[test]
    fn test_parse_namespaced_key_valid() {
        let (tenant, key) = parse_namespaced_key("tenant_a:vault:1").unwrap();
        assert_eq!(tenant, "tenant_a");
        assert_eq!(key, "vault:1");
    }

    #[test]
    fn test_parse_namespaced_key_no_separator() {
        assert!(parse_namespaced_key("no_separator").is_none());
    }

    // ── set / get ─────────────────────────────────────────────────────────────

    #[test]
    fn test_set_and_get_same_tenant() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "value1".to_string());
        assert_eq!(cache.get("tenant_a", "k1"), Some("value1".to_string()));
    }

    #[test]
    fn test_get_miss_on_empty_cache() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        assert!(cache.get("tenant_a", "k1").is_none());
    }

    // ── Tenant isolation ──────────────────────────────────────────────────────

    #[test]
    fn test_tenant_isolation_different_tenants_same_key() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "vault:1", "data_a".to_string());
        cache.set("tenant_b", "vault:1", "data_b".to_string());

        assert_eq!(
            cache.get("tenant_a", "vault:1"),
            Some("data_a".to_string())
        );
        assert_eq!(
            cache.get("tenant_b", "vault:1"),
            Some("data_b".to_string())
        );
    }

    #[test]
    fn test_tenant_a_cannot_read_tenant_b_data() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_b", "secret", "sensitive".to_string());
        assert!(cache.get("tenant_a", "secret").is_none());
    }

    #[test]
    fn test_clear_tenant_only_removes_that_tenant() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "a".to_string());
        cache.set("tenant_b", "k1", "b".to_string());

        cache.clear_tenant("tenant_a");

        assert!(cache.get("tenant_a", "k1").is_none());
        assert_eq!(cache.get("tenant_b", "k1"), Some("b".to_string()));
    }

    #[test]
    fn test_clear_all_removes_all_tenants() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "a".to_string());
        cache.set("tenant_b", "k1", "b".to_string());

        cache.clear_all();

        assert!(cache.get("tenant_a", "k1").is_none());
        assert!(cache.get("tenant_b", "k1").is_none());
    }

    // ── TTL expiry ────────────────────────────────────────────────────

    #[test]
    fn test_entry_expires_after_ttl() {
        let cache = PartitionedCache::new(Duration::from_millis(1));
        cache.set("tenant_a", "k1", "value".to_string());
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("tenant_a", "k1").is_none());
    }

    #[test]
    fn test_set_with_custom_ttl() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        // Override with a very short TTL.
        cache.set_with_ttl(
            "tenant_a",
            "k1",
            "value".to_string(),
            Duration::from_millis(1),
        );
        std::thread::sleep(Duration::from_millis(5));
        assert!(cache.get("tenant_a", "k1").is_none());
    }

    // ── remove ────────────────────────────────────────────────────────────────

    #[test]
    fn test_remove_specific_key() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "v1".to_string());
        cache.set("tenant_a", "k2", "v2".to_string());

        cache.remove("tenant_a", "k1");

        assert!(cache.get("tenant_a", "k1").is_none());
        assert_eq!(cache.get("tenant_a", "k2"), Some("v2".to_string()));
    }

    // ── validate_tenant_id ────────────────────────────────────────────────────

    #[test]
    fn test_validate_empty_tenant_id_rejected() {
        assert!(PartitionedCache::validate_tenant_id("").is_err());
    }

    #[test]
    fn test_validate_tenant_id_with_separator_rejected() {
        assert!(PartitionedCache::validate_tenant_id("bad:id").is_err());
    }

    #[test]
    fn test_validate_valid_tenant_id_accepted() {
        assert!(PartitionedCache::validate_tenant_id("tenant_abc_123").is_ok());
    }

    #[test]
    fn test_set_safe_rejects_invalid_tenant_id() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        let result = cache.set_safe("bad:tenant", "k", "v".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_get_safe_rejects_invalid_tenant_id() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        let result = cache.get_safe("bad:tenant", "k");
        assert!(result.is_err());
    }

    // ── Partition statistics ──────────────────────────────────────────────────

    #[test]
    fn test_partition_stats_none_for_unknown_tenant() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        assert!(cache.partition_stats("unknown").is_none());
    }

    #[test]
    fn test_partition_stats_tracks_hits_and_misses() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "v".to_string());

        // One hit
        let _ = cache.get("tenant_a", "k1");
        // One miss
        let _ = cache.get("tenant_a", "missing");

        let stats = cache.partition_stats("tenant_a").unwrap();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn test_partition_stats_hit_ratio() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "v".to_string());

        // 2 hits, 0 misses
        let _ = cache.get("tenant_a", "k1");
        let _ = cache.get("tenant_a", "k1");

        let stats = cache.partition_stats("tenant_a").unwrap();
        assert_eq!(stats.hit_ratio(), Some(1.0));
    }

    #[test]
    fn test_partition_stats_evictions_on_expiry() {
        let cache = PartitionedCache::new(Duration::from_millis(1));
        cache.set("tenant_a", "k1", "v".to_string());
        std::thread::sleep(Duration::from_millis(5));
        // Accessing an expired entry should increment evictions.
        let _ = cache.get("tenant_a", "k1");

        let stats = cache.partition_stats("tenant_a").unwrap();
        assert_eq!(stats.evictions, 1);
    }

    #[test]
    fn test_partition_stats_live_keys() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "v1".to_string());
        cache.set("tenant_a", "k2", "v2".to_string());

        let stats = cache.partition_stats("tenant_a").unwrap();
        assert_eq!(stats.live_keys, 2);
    }

    #[test]
    fn test_all_partition_stats_returns_all_tenants() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "v".to_string());
        cache.set("tenant_b", "k1", "v".to_string());

        let all = cache.all_partition_stats();
        assert!(all.contains_key("tenant_a"));
        assert!(all.contains_key("tenant_b"));
    }

    #[test]
    fn test_tenant_count() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        assert_eq!(cache.tenant_count(), 0);
        cache.set("tenant_a", "k", "v".to_string());
        cache.set("tenant_b", "k", "v".to_string());
        assert_eq!(cache.tenant_count(), 2);
    }

    #[test]
    fn test_stats_no_hit_ratio_when_no_accesses() {
        let cache = PartitionedCache::new(Duration::from_secs(60));
        cache.set("tenant_a", "k1", "v".to_string());
        // No gets yet
        let stats = cache.partition_stats("tenant_a").unwrap();
        assert_eq!(stats.hit_ratio(), None);
    }

    // ── Consistent Hashing & Rebalancing Tests (#362) ─────────────────────────

    #[test]
    fn test_consistent_hash_ring_distribution() {
        let ring = ConsistentHashRing::with_partitions(&["p0", "p1", "p2"], 50);
        assert_eq!(ring.partition_count(), 3);
        assert_eq!(ring.partitions(), vec!["p0", "p1", "p2"]);

        // Keys map deterministically to partitions
        let p_a = ring.get_partition("user:1001").unwrap();
        let p_b = ring.get_partition("user:1001").unwrap();
        assert_eq!(p_a, p_b);
        assert!(vec!["p0", "p1", "p2"].contains(&p_a));
    }

    #[test]
    fn test_rebalance_on_add_partition() {
        let cache = PartitionedCache::with_consistent_partitions(&["node-1", "node-2"], Duration::from_secs(300));

        // Insert 100 keys routed by consistent hashing
        for i in 0..100 {
            let key = format!("key_{i}");
            cache.set_routed(&key, format!("value_{i}"));
        }

        // Add node-3 and rebalance
        let result = cache.add_partition_and_rebalance("node-3");
        assert_eq!(result.total_keys, 100);
        assert!(result.migrated_keys > 0, "Some keys should migrate to new partition");

        // Verify all 100 keys are still accessible via routed get
        for i in 0..100 {
            let key = format!("key_{i}");
            assert_eq!(cache.get_routed(&key), Some(format!("value_{i}")));
        }
    }

    #[test]
    fn test_rebalance_on_remove_partition() {
        let cache = PartitionedCache::with_consistent_partitions(&["pA", "pB", "pC"], Duration::from_secs(300));

        for i in 0..100 {
            let key = format!("item_{i}");
            cache.set_routed(&key, format!("val_{i}"));
        }

        // Remove pC and rebalance
        let result = cache.remove_partition_and_rebalance("pC");
        assert_eq!(result.total_keys, 100);

        // Verify all keys remain accessible in the surviving partitions
        for i in 0..100 {
            let key = format!("item_{i}");
            assert_eq!(cache.get_routed(&key), Some(format!("val_{i}")));
        }
    }

    #[test]
    fn test_minimal_key_movement() {
        let initial_partitions = &["part-1", "part-2", "part-3", "part-4"];
        let cache = PartitionedCache::with_consistent_partitions(initial_partitions, Duration::from_secs(300));

        let num_keys = 1000;
        for i in 0..num_keys {
            let key = format!("entity_{i}");
            cache.set_routed(&key, format!("content_{i}"));
        }

        // Add 5th partition (N=4 -> N=5).
        // In ideal consistent hashing, ~1/(N+1) = 20% of keys move.
        // Modulo hashing would move ~80% of keys.
        let result = cache.add_partition_and_rebalance("part-5");
        let migrated_percentage = (result.migrated_keys as f64 / num_keys as f64) * 100.0;

        assert!(
            migrated_percentage < 35.0,
            "Consistent hashing should move ~20% of keys (got {:.2}%, expected < 35%)",
            migrated_percentage
        );

        // All keys are still preserved
        for i in 0..num_keys {
            let key = format!("entity_{i}");
            assert_eq!(cache.get_routed(&key), Some(format!("content_{i}")));
        }
    }

    #[test]
    fn test_partition_load_and_hot_partition_detection() {
        let cache = PartitionedCache::with_consistent_partitions(&["part-a", "part-b", "part-c"], Duration::from_secs(300));

        // Generate normal load on part-a and part-b
        for _ in 0..10 {
            cache.set("part-a", "k", "v".into());
            let _ = cache.get("part-a", "k");
            cache.set("part-b", "k", "v".into());
            let _ = cache.get("part-b", "k");
        }

        // Generate heavy / hot load on part-c
        for _ in 0..100 {
            cache.set("part-c", "k", "v".into());
            let _ = cache.get("part-c", "k");
        }

        let load_c = cache.partition_load("part-c").unwrap();
        assert!(load_c.is_hot, "part-c should be flagged as hot partition");
        assert!(load_c.load_factor > 1.5);

        let hot = cache.hot_partitions(1.5);
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].partition_id, "part-c");
    }
}
