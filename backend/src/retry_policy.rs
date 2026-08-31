//! Retry policy framework (Issue: "Retry logic is scattered").
//!
//! Provides a small policy language for describing retry behaviour
//! (max attempts, exponential backoff, jitter) that can be registered
//! per-endpoint via the `POST /admin/retry-policies` endpoint instead of
//! being hardcoded at each call site.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use rand::Rng;
use serde::{Deserialize, Serialize};

/// Jitter strategy applied on top of the computed exponential backoff delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JitterMode {
    /// No jitter, use the computed delay as-is.
    None,
    /// Uniform random delay in `[0, computed_delay]`.
    Full,
    /// `computed_delay / 2 + uniform(0, computed_delay / 2)`.
    Equal,
}

impl Default for JitterMode {
    fn default() -> Self {
        JitterMode::Equal
    }
}

/// A retry policy: the "language" describing how a class of requests
/// (matched by `endpoint_pattern`) should be retried.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub id: String,
    pub name: String,
    /// Simple prefix/glob-ish pattern matched against the request path,
    /// e.g. `/api/vaults/*` or `/webhooks`.
    pub endpoint_pattern: String,
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default)]
    pub jitter: JitterMode,
    /// HTTP status codes that should trigger a retry.
    #[serde(default = "default_retry_statuses")]
    pub retry_on_status: Vec<u16>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn default_multiplier() -> f64 {
    2.0
}

fn default_retry_statuses() -> Vec<u16> {
    vec![429, 500, 502, 503, 504]
}

#[derive(Debug, Deserialize)]
pub struct CreateRetryPolicyRequest {
    pub name: String,
    pub endpoint_pattern: String,
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default)]
    pub jitter: JitterMode,
    #[serde(default = "default_retry_statuses")]
    pub retry_on_status: Vec<u16>,
}

impl RetryPolicy {
    /// Validates that this policy's backoff/jitter configuration is sane
    /// before it's stored and used to compute real delays.
    ///
    /// `jitter` itself (`JitterMode`) is a closed enum with no numeric
    /// fields, so it can't independently be "negative" or "out of bounds" —
    /// but the parameters that feed the jittered delay computation
    /// (`compute_backoff_delay`) can be, and a bad value there is exactly
    /// what would produce a negative, zero-forever, or larger-than-interval
    /// jitter window at runtime. This checks those.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_attempts == 0 {
            return Err("max_attempts must be > 0".to_string());
        }
        if self.base_delay_ms > self.max_delay_ms {
            return Err("base_delay_ms must be <= max_delay_ms".to_string());
        }
        if !self.multiplier.is_finite() || self.multiplier <= 0.0 {
            return Err(format!(
                "multiplier must be a finite positive number, got {}",
                self.multiplier
            ));
        }
        Ok(())
    }
}

/// Computes the delay before attempt number `attempt` (1-indexed) using
/// exponential backoff capped at `max_delay_ms`, with jitter applied.
pub fn compute_backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1) as i32;
    let raw_ms = (policy.base_delay_ms as f64) * policy.multiplier.powi(exp);
    let capped_ms = raw_ms.min(policy.max_delay_ms as f64).max(0.0);

    let jittered_ms = match policy.jitter {
        JitterMode::None => capped_ms,
        JitterMode::Full => {
            let mut rng = rand::thread_rng();
            rng.gen_range(0.0..=capped_ms.max(1.0))
        }
        JitterMode::Equal => {
            let half = capped_ms / 2.0;
            let mut rng = rand::thread_rng();
            half + rng.gen_range(0.0..=half.max(1.0))
        }
    };

    Duration::from_millis(jittered_ms.round() as u64)
}

/// Whether `status` should trigger a retry under `policy`.
pub fn should_retry_status(policy: &RetryPolicy, status: u16) -> bool {
    policy.retry_on_status.contains(&status)
}

/// Executes `op` (an async closure producing `Result<T, E>`) according to
/// `policy`, sleeping between attempts using exponential backoff + jitter.
/// `is_retryable` decides whether a given error should trigger another
/// attempt.
pub async fn execute_with_retry<F, Fut, T, E>(
    policy: &RetryPolicy,
    mut is_retryable: impl FnMut(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= policy.max_attempts || !is_retryable(&e) {
                    return Err(e);
                }
                let delay = compute_backoff_delay(policy, attempt);
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// In-memory registry of retry policies, keyed by id.
#[derive(Debug, Default)]
pub struct RetryPolicyStore {
    policies: Mutex<HashMap<String, RetryPolicy>>,
}

impl RetryPolicyStore {
    pub fn new() -> Self {
        Self {
            policies: Mutex::new(HashMap::new()),
        }
    }

    pub fn upsert(&self, policy: RetryPolicy) -> RetryPolicy {
        let mut guard = self.policies.lock().unwrap();
        guard.insert(policy.id.clone(), policy.clone());
        policy
    }

    pub fn get(&self, id: &str) -> Option<RetryPolicy> {
        self.policies.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<RetryPolicy> {
        self.policies.lock().unwrap().values().cloned().collect()
    }

    /// Finds the first policy whose `endpoint_pattern` is a prefix of
    /// `path`, preferring the most specific (longest) match.
    pub fn find_for_path(&self, path: &str) -> Option<RetryPolicy> {
        self.policies
            .lock()
            .unwrap()
            .values()
            .filter(|p| {
                let pattern = p.endpoint_pattern.trim_end_matches('*');
                path.starts_with(pattern)
            })
            .max_by_key(|p| p.endpoint_pattern.len())
            .cloned()
    }
}

#[derive(Clone)]
pub struct RetryPolicyState {
    pub store: Arc<RetryPolicyStore>,
}

impl RetryPolicyState {
    pub fn new() -> Self {
        Self {
            store: Arc::new(RetryPolicyStore::new()),
        }
    }
}

impl Default for RetryPolicyState {
    fn default() -> Self {
        Self::new()
    }
}

async fn create_retry_policy(
    State(state): State<RetryPolicyState>,
    Json(body): Json<CreateRetryPolicyRequest>,
) -> Result<(StatusCode, Json<RetryPolicy>), (StatusCode, String)> {
    let policy = RetryPolicy {
        id: uuid::Uuid::new_v4().to_string(),
        name: body.name,
        endpoint_pattern: body.endpoint_pattern,
        max_attempts: body.max_attempts,
        base_delay_ms: body.base_delay_ms,
        max_delay_ms: body.max_delay_ms,
        multiplier: body.multiplier,
        jitter: body.jitter,
        retry_on_status: body.retry_on_status,
        created_at: chrono::Utc::now(),
    };

    policy
        .validate()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;

    let saved = state.store.upsert(policy);
    Ok((StatusCode::CREATED, Json(saved)))
}

async fn list_retry_policies(State(state): State<RetryPolicyState>) -> Json<Vec<RetryPolicy>> {
    Json(state.store.list())
}

async fn get_retry_policy(
    State(state): State<RetryPolicyState>,
    Path(id): Path<String>,
) -> Result<Json<RetryPolicy>, StatusCode> {
    state.store.get(&id).map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// Builds the `/admin/retry-policies` router with its own state; merge it
/// into the main application router.
pub fn router(state: RetryPolicyState) -> Router {
    Router::new()
        .route(
            "/admin/retry-policies",
            post(create_retry_policy).get(list_retry_policies),
        )
        .route("/admin/retry-policies/:id", get(get_retry_policy))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy(jitter: JitterMode) -> RetryPolicy {
        RetryPolicy {
            id: "test".into(),
            name: "test".into(),
            endpoint_pattern: "/api".into(),
            max_attempts: 5,
            base_delay_ms: 100,
            max_delay_ms: 2000,
            multiplier: 2.0,
            jitter,
            retry_on_status: default_retry_statuses(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn backoff_grows_exponentially_without_jitter() {
        let policy = test_policy(JitterMode::None);
        assert_eq!(compute_backoff_delay(&policy, 1).as_millis(), 100);
        assert_eq!(compute_backoff_delay(&policy, 2).as_millis(), 200);
        assert_eq!(compute_backoff_delay(&policy, 3).as_millis(), 400);
    }

    #[test]
    fn backoff_is_capped_at_max_delay() {
        let policy = test_policy(JitterMode::None);
        assert_eq!(compute_backoff_delay(&policy, 10).as_millis(), 2000);
    }

    #[test]
    fn full_jitter_never_exceeds_capped_delay() {
        let policy = test_policy(JitterMode::Full);
        for attempt in 1..=6 {
            let delay = compute_backoff_delay(&policy, attempt);
            assert!(delay.as_millis() <= 2000);
        }
    }

    #[test]
    fn equal_jitter_stays_within_half_to_full_capped_delay() {
        // Unlike full jitter (which can land anywhere in [0, capped]), equal
        // jitter should never fall below half the capped delay, and never
        // exceed it. Sampled repeatedly since jitter is randomized.
        let policy = test_policy(JitterMode::Equal);
        for attempt in 1..=6 {
            let capped_ms = (policy.base_delay_ms as f64 * policy.multiplier.powi(attempt as i32 - 1))
                .min(policy.max_delay_ms as f64);
            for _ in 0..50 {
                let delay_ms = compute_backoff_delay(&policy, attempt).as_millis() as f64;
                assert!(
                    delay_ms >= (capped_ms / 2.0).floor() && delay_ms <= capped_ms.ceil(),
                    "attempt {attempt}: delay {delay_ms}ms outside expected [{}, {}] window",
                    capped_ms / 2.0,
                    capped_ms
                );
            }
        }
    }

    #[test]
    fn no_jitter_mode_never_produces_negative_or_out_of_bounds_delay() {
        let policy = test_policy(JitterMode::None);
        for attempt in 1..=10 {
            let delay = compute_backoff_delay(&policy, attempt);
            assert!(delay.as_millis() <= policy.max_delay_ms as u128);
        }
    }

    #[test]
    fn validate_accepts_sane_boundary_configurations() {
        // multiplier == 1.0 (no growth, but still a sane, finite, positive
        // value) and base_delay_ms == max_delay_ms (zero-width interval) are
        // both edge cases that should be accepted, not rejected.
        let mut policy = test_policy(JitterMode::None);
        policy.multiplier = 1.0;
        assert!(policy.validate().is_ok());

        policy.base_delay_ms = policy.max_delay_ms;
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_max_attempts() {
        let mut policy = test_policy(JitterMode::None);
        policy.max_attempts = 0;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn validate_rejects_base_delay_greater_than_max_delay() {
        let mut policy = test_policy(JitterMode::None);
        policy.base_delay_ms = policy.max_delay_ms + 1;
        assert!(policy.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_positive_multiplier() {
        for bad in [0.0, -1.0, -0.5] {
            let mut policy = test_policy(JitterMode::None);
            policy.multiplier = bad;
            assert!(
                policy.validate().is_err(),
                "multiplier {bad} should be rejected"
            );
        }
    }

    #[test]
    fn validate_rejects_non_finite_multiplier() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut policy = test_policy(JitterMode::None);
            policy.multiplier = bad;
            assert!(
                policy.validate().is_err(),
                "multiplier {bad} should be rejected"
            );
        }
    }

    #[test]
    fn find_for_path_prefers_most_specific_match() {
        let store = RetryPolicyStore::new();
        store.upsert(RetryPolicy {
            endpoint_pattern: "/api".into(),
            ..test_policy(JitterMode::None)
        });
        store.upsert(RetryPolicy {
            id: "specific".into(),
            endpoint_pattern: "/api/vaults".into(),
            ..test_policy(JitterMode::None)
        });
        let found = store.find_for_path("/api/vaults/123").unwrap();
        assert_eq!(found.id, "specific");
    }
}
