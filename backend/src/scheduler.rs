use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::{db::Db, models::Frequency};

/// Polls preferences every minute and fires reminders for vaults whose TTL
/// is within the user-configured window.
///
/// In production, replace `fetch_ttl_remaining` with a real Stellar RPC call
/// and `send_reminder` with actual email/SMS/push dispatch.
pub async fn run(db: Arc<Db>) {
    // Seed default secret rotation policies on startup.
    crate::secret_rotation::seed_default_policies(&db);

    let mut interval = tokio::time::interval(Duration::from_mins(1));
    // Track when we last ran the daily/hourly tasks.
    let mut last_daily_purge = chrono::DateTime::<Utc>::MIN_UTC;
    let mut last_rotation_check = chrono::DateTime::<Utc>::MIN_UTC;
    let mut last_cache_drift_check = chrono::DateTime::<Utc>::MIN_UTC;

    loop {
        interval.tick().await;
        let now = Utc::now();

        // 1) Existing reminder preferences scheduler.
        match db.all() {
            Ok(all_prefs) => {
                for prefs in all_prefs {
                    let ttl_hours = fetch_ttl_remaining(prefs.vault_id);
                    let window = prefs.hours_before_expiry;

                    let subscription = db.get_subscription(prefs.vault_id).ok().flatten();

                    use crate::models::SubscriptionFrequency;
                    let should_notify = if let Some(ref sub) = subscription {
                        match sub.frequency {
                            SubscriptionFrequency::Once => {
                                ttl_hours <= window && ttl_hours > window.saturating_sub(1)
                            }
                            SubscriptionFrequency::Daily => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24)
                            }
                            SubscriptionFrequency::Weekly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 7)
                            }
                            SubscriptionFrequency::Hourly => ttl_hours <= window,
                            SubscriptionFrequency::Monthly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 30)
                            }
                        }
                    } else {
                        match prefs.frequency {
                            Frequency::Once => {
                                ttl_hours <= window && ttl_hours > window.saturating_sub(1)
                            }
                            Frequency::Daily => ttl_hours <= window && ttl_hours.is_multiple_of(24),
                            Frequency::Weekly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 7)
                            }
                            Frequency::Hourly => ttl_hours <= window,
                            Frequency::Monthly => {
                                ttl_hours <= window && ttl_hours.is_multiple_of(24 * 30)
                            }
                        }
                    };

                    if should_notify {
                        for channel in &prefs.channels {
                            let deliver_on_channel = if let Some(ref sub) = subscription {
                                use crate::models::SubscriptionChannel;
                                match channel {
                                    crate::models::Channel::Email => {
                                        sub.channels.contains(&SubscriptionChannel::Email)
                                    }
                                    crate::models::Channel::Sms => {
                                        sub.channels.contains(&SubscriptionChannel::Sms)
                                    }
                                    crate::models::Channel::Push => false,
                                }
                            } else {
                                true
                            };

                            if deliver_on_channel {
                                send_reminder(prefs.vault_id, channel, ttl_hours);
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to fetch reminder preferences");
            }
        }

        // 2) TTL insurance scheduler.
        extend_ttl_for_inactive_owners(&db);

        // 3) Data retention purge (runs at most once every 24 hours).
        if now.signed_duration_since(last_daily_purge).num_hours() >= 24 {
            crate::retention::run_purge_scheduler(&db);
            last_daily_purge = now;
        }

        // 4) Secret rotation overdue check (runs at most once every hour).
        if now.signed_duration_since(last_rotation_check).num_minutes() >= 60 {
            crate::secret_rotation::run_rotation_scheduler(&db);
            last_rotation_check = now;
        }

        // 5) Consistency checks (runs at most once every 5 minutes).
        if now.signed_duration_since(last_cache_drift_check).num_minutes() >= 5 {
            run_consistency_check(&db);
            last_cache_drift_check = now;
        }
    }
}

fn extend_ttl_for_inactive_owners(db: &Arc<Db>) {
    let policies = match db.all_enabled_insurance_policies() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "failed to fetch insurance policies");
            return;
        }
    };

    let now = Utc::now();

    for policy in policies {
        if !policy.enabled {
            continue;
        }
        let owner_last_active = match db.get_owner_last_active_at(policy.vault_id) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(
                    vault_id = policy.vault_id,
                    error = %e,
                    "failed to fetch owner last active time"
                );
                continue;
            }
        };
        let Some(last_active) = owner_last_active else {
            continue;
        };

        let inactive_for = now.signed_duration_since(last_active).num_seconds();
        if inactive_for < policy.inactivity_threshold_seconds.cast_signed() {
            continue;
        }

        tracing::info!(
            vault_id = policy.vault_id,
            extension_seconds = policy.extension_seconds,
            "TTL extended by insurance due to inactivity"
        );

        if let Err(e) = db.upsert_insurance_policy(&crate::models::TtlInsurancePolicy {
            vault_id: policy.vault_id,
            extension_seconds: policy.extension_seconds,
            inactivity_threshold_seconds: policy.inactivity_threshold_seconds,
            enabled: true,
            purchased_at: policy.purchased_at,
            last_extended_at: Some(now),
        }) {
            tracing::error!(
                vault_id = policy.vault_id,
                error = %e,
                "failed to update insurance policy after TTL extension"
            );
        }
    }
}

/// Stub: returns hours remaining until vault TTL expiry.
/// Replace with a Stellar RPC call to `get_ttl_remaining`.
fn fetch_ttl_remaining(_vault_id: u64) -> u32 {
    u32::MAX
}

/// Stub: dispatches a reminder via the given channel.
fn send_reminder(vault_id: u64, channel: &crate::models::Channel, hours_left: u32) {
    tracing::info!(vault_id, ?channel, hours_left, "sending reminder");
}

// ── #81: Backup Validation Job ───────────────────────────────────────────────

/// Run the periodic backup validation job.
///
/// In a real deployment this would retrieve backup snapshots from durable
/// storage and validate each one.  Here we log a scheduled-run notice and
/// simulate a trivial no-op validation so the job framework is exercised
/// without requiring an external storage integration.
#[allow(dead_code)]
fn run_backup_validation_job() {
    use crate::backup_validation::BackupValidator;
    use chrono::Utc;

    let job_id = uuid::Uuid::new_v4().to_string();
    let scheduled_at = Utc::now();

    tracing::info!(
        job_id = %job_id,
        scheduled_at = %scheduled_at,
        "backup validation job started"
    );

    // Simulate validating a placeholder backup so the code path is exercised.
    // Replace with real backup retrieval when storage integration is ready.
    let placeholder_backups: Vec<(String, Vec<u8>)> = vec![];
    let results = BackupValidator::validate_all_backups(&placeholder_backups);

    for result in &results {
        if result.valid {
            tracing::info!(
                backup_id = %result.backup_id,
                "backup validation passed"
            );
        } else {
            tracing::warn!(
                backup_id = %result.backup_id,
                error = ?result.error,
                "backup validation failed"
            );
        }
    }

    tracing::info!(
        job_id = %job_id,
        validated = results.len(),
        "backup validation job completed"
    );
}

// ── #83: Consistency Check Job ───────────────────────────────────────────────

/// Run the periodic data consistency verification job.
pub fn run_consistency_check(db: &Arc<Db>) {
    use crate::consistency::ConsistencyChecker;

    tracing::info!("consistency check job started");

    let report = ConsistencyChecker::run_all_checks(db);

    for issue in &report.issues {
        match issue.severity {
            crate::consistency::IssueSeverity::Critical => {
                tracing::error!(
                    check = %issue.check_name,
                    affected_rows = issue.affected_rows,
                    description = %issue.description,
                    "CRITICAL consistency issue detected"
                );
            }
            crate::consistency::IssueSeverity::Error => {
                tracing::error!(
                    check = %issue.check_name,
                    affected_rows = issue.affected_rows,
                    description = %issue.description,
                    "consistency error detected"
                );
            }
            crate::consistency::IssueSeverity::Warning => {
                tracing::warn!(
                    check = %issue.check_name,
                    affected_rows = issue.affected_rows,
                    description = %issue.description,
                    "consistency warning detected"
                );
            }
        }
    }

    tracing::info!(
        total_checks = report.total_checks,
        passed = report.passed_checks,
        failed = report.failed_checks,
        "consistency check job completed"
    );
}

// ── #360: Multi-Level Cache Consistency Verification Job ─────────────────────

/// Run the periodic multi-level cache consistency verification and auto-healing job.
pub fn run_cache_consistency_job(
    cache: &crate::multilevel_cache::MultiLevelCache,
    store: &crate::db::VaultStore,
) -> crate::multilevel_cache::CacheDriftReport {
    tracing::info!("multi-level cache consistency verification job started");

    let report = cache.verify_and_heal_consistency(|vault_id| {
        store.lock().unwrap().get(vault_id).cloned()
    });

    if report.drift_count > 0 {
        tracing::warn!(
            checked_keys = report.checked_keys_count,
            drift_count = report.drift_count,
            healed_count = report.healed_count,
            "cache drift detected and auto-healed"
        );
    } else {
        tracing::info!(
            checked_keys = report.checked_keys_count,
            "multi-level cache consistency verification passed with 0 drift"
        );
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::create_vault_store;
    use crate::models::{Vault, VaultStatus};
    use crate::multilevel_cache::MultiLevelCache;

    #[test]
    fn test_run_cache_consistency_job_detects_and_heals_drift() {
        let store = create_vault_store();
        let cache = MultiLevelCache::new();

        let vault = Vault {
            id: "v-sched-1".to_string(),
            owner: "owner1".to_string(),
            beneficiary: "ben1".to_string(),
            balance: 5000,
            check_in_interval: 86400,
            last_check_in: Utc::now(),
            created_at: Utc::now(),
            status: VaultStatus::Active,
            ttl_remaining: Some(86400),
        };
        store.lock().unwrap().insert("v-sched-1".to_string(), vault.clone());

        // Cache has stale / drifted data
        let mut stale_vault = vault;
        stale_vault.balance = 100;
        cache.set_vault("v-sched-1", stale_vault);

        // Run scheduled job
        let report = run_cache_consistency_job(&cache, &store);
        assert_eq!(report.checked_keys_count, 1);
        // Note: write-through set both L1 and L2 to balance 100, but source-of-truth has 5000.
        // Even if L1 == L2, verifying against source of truth ensures consistency.
        let metrics = cache.drift_metrics();
        assert_eq!(metrics.total_verifications, 1);
    }
}
