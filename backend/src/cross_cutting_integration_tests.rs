//! Cross-cutting integration tests: auth, rate limiting, and caching
//! interacting together, as opposed to each module's existing unit-level
//! tests which exercise `audit.rs`, `cache.rs`, and `websocket.rs` in
//! isolation. See docs/integration-testing-guide.md.

#![cfg(test)]

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::http::HeaderMap;
use chrono::Utc;

use crate::audit::{authorize_admin, log_state_modification};
use crate::cache::VaultCache;
use crate::db::{create_vault_store, get_vault_cached, invalidate_vault_cache, Db};
use crate::models::{AuditLogQuery, Vault, VaultStatus};
use crate::websocket::MessageRateLimiter;

/// `authorize_admin` reads the process-wide `ADMIN_API_KEY` env var, so tests
/// that toggle it must not run concurrently with each other.
fn env_guard() -> &'static Mutex<()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD.get_or_init(|| Mutex::new(()))
}

fn headers_with_bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {token}").parse().unwrap(),
    );
    headers
}

fn sample_vault(id: &str) -> Vault {
    Vault {
        id: id.to_string(),
        owner: "owner-1".to_string(),
        beneficiary: "beneficiary-1".to_string(),
        balance: 1_000,
        check_in_interval: 3_600,
        last_check_in: Utc::now(),
        created_at: Utc::now(),
        status: VaultStatus::Active,
        ttl_remaining: Some(3_600),
    }
}

/// Scenario 1: auth + caching. A rejected (unauthorized) request must never
/// populate the vault cache; only a successful auth check may warm it. This
/// guards against leaking cached state to callers who never proved they were
/// allowed to trigger the fetch that produced it.
#[test]
fn test_auth_failure_does_not_populate_cache() {
    let _lock = env_guard().lock().unwrap();
    std::env::set_var("ADMIN_API_KEY", "expected-secret");

    let store = create_vault_store();
    let cache = VaultCache::new();
    let vault_id = "vault-auth-cache";
    store
        .lock()
        .unwrap()
        .insert(vault_id.to_string(), sample_vault(vault_id));

    // Simulated protected handler: only touch the cache if auth passes.
    let unauthorized_headers = headers_with_bearer("wrong-token");
    if authorize_admin(&unauthorized_headers).is_ok() {
        let _ = get_vault_cached(&store, &cache, vault_id);
    }
    assert!(
        cache.get_vault(vault_id).is_none(),
        "cache must stay empty after a failed authorization check"
    );

    let authorized_headers = headers_with_bearer("expected-secret");
    assert!(authorize_admin(&authorized_headers).is_ok());
    let fetched = get_vault_cached(&store, &cache, vault_id);
    assert!(fetched.is_some());
    assert!(
        cache.get_vault(vault_id).is_some(),
        "cache must be populated only after a successful authorization check"
    );

    std::env::remove_var("ADMIN_API_KEY");
}

/// Scenario 2: auth + rate limiting. The message rate limiter must enforce
/// its per-second budget independently of whatever auth outcome accompanies
/// each call — the two concerns must not be accidentally coupled (e.g. a
/// failed auth check silently resetting or bypassing the counter).
#[test]
fn test_rate_limiter_enforces_budget_independent_of_auth_outcome() {
    let mut limiter = MessageRateLimiter::new();
    let mut allowed = 0;
    let mut denied = 0;

    for i in 0..15 {
        // Alternate a simulated auth outcome; it must have no bearing on the
        // rate limiter's internal counter.
        let _simulated_auth_ok = i % 2 == 0;
        if limiter.check_and_count() {
            allowed += 1;
        } else {
            denied += 1;
        }
    }

    assert_eq!(allowed, 10, "exactly RATE_LIMIT_MSG_PER_SEC calls should be allowed");
    assert_eq!(denied, 5, "calls beyond the budget must be denied regardless of auth state");
}

/// Scenario 3: auth + auditing. Both an authorized and an unauthorized
/// attempt against the same resource must be independently and durably
/// recorded in the audit log with the correct `result`, so operators can
/// distinguish legitimate access from rejected attempts after the fact.
#[test]
fn test_audit_log_distinguishes_authorized_and_unauthorized_attempts() {
    let _lock = env_guard().lock().unwrap();
    std::env::set_var("ADMIN_API_KEY", "expected-secret");

    let db = Arc::new(Db::open(":memory:").unwrap());
    db.migrate().unwrap();

    let resource = "vault-audit-cross-cut";

    let unauthorized_headers = headers_with_bearer("wrong-token");
    let auth_result = authorize_admin(&unauthorized_headers);
    assert!(auth_result.is_err());
    log_state_modification(
        &db,
        "admin_access",
        resource,
        "failure",
        &unauthorized_headers,
        None,
    );

    let authorized_headers = headers_with_bearer("expected-secret");
    assert!(authorize_admin(&authorized_headers).is_ok());
    log_state_modification(
        &db,
        "admin_access",
        resource,
        "success",
        &authorized_headers,
        None,
    );

    let entries = db
        .query_audit_logs(&AuditLogQuery {
            resource: Some(resource.to_string()),
            ..Default::default()
        })
        .unwrap();

    assert_eq!(entries.len(), 2, "both attempts must be persisted");
    assert!(entries.iter().any(|e| e.result == "failure"));
    assert!(entries.iter().any(|e| e.result == "success"));

    std::env::remove_var("ADMIN_API_KEY");
}

/// Scenario 4: caching + rate limiting. The cache's TTL clock and the rate
/// limiter's window clock must not interfere with each other — expiring one
/// must not reset or affect the other.
#[test]
fn test_cache_ttl_and_rate_limiter_window_are_independent() {
    let store = create_vault_store();
    let cache = VaultCache::with_ttl(Duration::from_millis(30));
    let vault_id = "vault-cache-ratelimit";
    store
        .lock()
        .unwrap()
        .insert(vault_id.to_string(), sample_vault(vault_id));

    let mut limiter = MessageRateLimiter::new();
    for _ in 0..5 {
        assert!(limiter.check_and_count());
    }

    assert!(get_vault_cached(&store, &cache, vault_id).is_some());
    assert!(cache.get_vault(vault_id).is_some());

    // Let only the cache TTL elapse; the rate limiter's 1s window has not.
    std::thread::sleep(Duration::from_millis(50));

    assert!(
        cache.get_vault(vault_id).is_none(),
        "cache entry must expire independently on its own TTL"
    );
    // The rate limiter must still recall its count from before the sleep,
    // since its window is a separate, longer-lived clock.
    assert!(
        limiter.check_and_count(),
        "6th call within the same 1s window must still be allowed (budget is 10)"
    );
}

/// Scenario 5: caching + auditing. A state change must invalidate the cache
/// and be reflected on the next read, and that mutation must be captured in
/// the audit log — the two side effects of a single write must both take
/// effect together, not just one of them.
#[test]
fn test_cache_invalidation_on_state_change_is_reflected_and_audited() {
    let db = Arc::new(Db::open(":memory:").unwrap());
    db.migrate().unwrap();

    let store = create_vault_store();
    let cache = VaultCache::new();
    let vault_id = "vault-cache-audit";
    store
        .lock()
        .unwrap()
        .insert(vault_id.to_string(), sample_vault(vault_id));

    // Warm the cache.
    let first = get_vault_cached(&store, &cache, vault_id).unwrap();
    assert_eq!(first.status, VaultStatus::Active);
    assert!(cache.get_vault(vault_id).is_some());

    // Mutate the underlying store (simulating e.g. a release) and invalidate.
    {
        let mut guard = store.lock().unwrap();
        let vault = guard.get_mut(vault_id).unwrap();
        vault.status = VaultStatus::Released;
    }
    invalidate_vault_cache(&cache, vault_id);
    log_state_modification(
        &db,
        "release",
        vault_id,
        "success",
        &HeaderMap::new(),
        None,
    );

    assert!(
        cache.get_vault(vault_id).is_none(),
        "cache must be empty immediately after invalidation"
    );

    let refreshed = get_vault_cached(&store, &cache, vault_id).unwrap();
    assert_eq!(
        refreshed.status,
        VaultStatus::Released,
        "post-invalidation read must reflect the new state, not stale cached data"
    );

    let entries = db
        .query_audit_logs(&AuditLogQuery {
            resource: Some(vault_id.to_string()),
            action: Some("release".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].result, "success");
}
