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
    let mut interval = tokio::time::interval(Duration::from_mins(1));
    loop {
        interval.tick().await;

        // 1) Existing reminder preferences scheduler.
        match db.all().await {
            Ok(all_prefs) => {
                for prefs in all_prefs {
                    let ttl_hours = fetch_ttl_remaining(prefs.vault_id);
                    let window = prefs.hours_before_expiry;

                    let subscription = db.get_subscription(prefs.vault_id).await.ok().flatten();

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
        extend_ttl_for_inactive_owners(&db).await;
    }
}

async fn extend_ttl_for_inactive_owners(db: &Arc<Db>) {
    let policies = match db.all_enabled_insurance_policies().await {
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
        let owner_last_active = match db.get_owner_last_active_at(policy.vault_id).await {
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

        if let Err(e) = db
            .upsert_insurance_policy(&crate::models::TtlInsurancePolicy {
                vault_id: policy.vault_id,
                extension_seconds: policy.extension_seconds,
                inactivity_threshold_seconds: policy.inactivity_threshold_seconds,
                enabled: true,
                purchased_at: policy.purchased_at,
                last_extended_at: Some(now),
            })
            .await
        {
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
