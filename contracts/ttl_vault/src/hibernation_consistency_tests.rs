//! State-consistency tests for the hibernation feature (docs/hibernation.md).
//!
//! These tests verify that TTL-dependent fields and pending state transitions
//! are correctly preserved and resumed across a hibernate -> advance clock ->
//! wake cycle, as opposed to the existing lifecycle test which only checks
//! the final expiry outcome.

#![cfg(test)]

extern crate alloc;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token::{self, StellarAssetClient},
    Address, Env,
};

fn setup() -> (
    Env,
    Address,
    Address,
    Address,
    Address,
    TtlVaultContractClient<'static>,
) {
    let env = Env::default();
    env.mock_all_auths();

    let owner = Address::generate(&env);
    let beneficiary = Address::generate(&env);
    let admin = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token_address = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    StellarAssetClient::new(&env, &token_address).mint(&owner, &10_000_000);

    let contract_address = env.register_contract(None, TtlVaultContract);
    let client = TtlVaultContractClient::new(&env, &contract_address);
    client.initialize(&token_address, &admin);

    (env, owner, beneficiary, admin, token_address, client)
}

/// Hibernate a vault, advance the ledger clock past the hibernation window,
/// wake (via natural expiry of the window) and assert the TTL-dependent
/// fields (`is_expired`, `get_hibernation`) behave correctly on both sides
/// of the hibernation boundary.
#[test]
fn test_hibernation_state_survives_wake_cycle() {
    let (env, owner, beneficiary, _admin, _token, client) = setup();
    let interval = 1_000u64;
    let hibernation_duration = 10_000u64;

    let vault_id = client.create_vault(&owner, &beneficiary, &interval, &None);
    client.deposit(&vault_id, &owner, &100_000i128);

    // Enter hibernation partway through the normal interval.
    env.ledger().with_mut(|l| l.timestamp = 100);
    client.enter_hibernation(&vault_id, &owner, &hibernation_duration);

    let entry = client.get_hibernation(&vault_id).expect("entry recorded");
    assert_eq!(entry.started_at, 100);
    assert_eq!(entry.duration_seconds, hibernation_duration);

    // While the window is open (well past what the normal interval alone
    // would have allowed), the vault must remain unexpired.
    env.ledger().with_mut(|l| l.timestamp = 100 + interval * 5);
    assert!(
        !client.is_expired(&vault_id),
        "TTL must be frozen while hibernating, regardless of elapsed check-in interval"
    );
    // The hibernation entry itself must be unchanged mid-window (no implicit wake).
    let mid_entry = client.get_hibernation(&vault_id).expect("still hibernating");
    assert_eq!(mid_entry.started_at, entry.started_at);
    assert_eq!(mid_entry.duration_seconds, entry.duration_seconds);

    // Advance the ledger clock past the hibernation window boundary — this
    // is the "wake" transition. hibernated_seconds now becomes duration_seconds
    // and is credited onto the effective deadline.
    let wake_time = 100 + hibernation_duration + 1;
    env.ledger().with_mut(|l| l.timestamp = wake_time);

    // Immediately after waking, the vault must not be expired: the owner is
    // owed a fresh `interval` worth of time from the moment hibernation closed.
    assert!(
        !client.is_expired(&vault_id),
        "vault must not expire the instant hibernation closes"
    );

    // Advance past the post-wake interval and confirm normal TTL expiry resumes.
    env.ledger()
        .with_mut(|l| l.timestamp = wake_time + interval + 1);
    assert!(
        client.is_expired(&vault_id),
        "normal TTL expiry must resume correctly after the hibernation boundary"
    );
}

/// Exiting hibernation early must credit the elapsed hibernation time onto
/// the effective check-in baseline, preserving the remaining TTL budget
/// exactly as documented in docs/hibernation.md.
#[test]
fn test_early_exit_preserves_remaining_ttl_budget() {
    let (env, owner, beneficiary, _admin, _token, client) = setup();
    let interval = 2_000u64;
    let hibernation_duration = 20_000u64;

    let vault_id = client.create_vault(&owner, &beneficiary, &interval, &None);
    client.deposit(&vault_id, &owner, &50_000i128);

    env.ledger().with_mut(|l| l.timestamp = 0);
    client.enter_hibernation(&vault_id, &owner, &hibernation_duration);

    // Exit early, after only half the hibernation window has elapsed.
    let elapsed = hibernation_duration / 2;
    env.ledger().with_mut(|l| l.timestamp = elapsed);
    client.exit_hibernation(&vault_id, &owner);

    // The hibernation entry must be cleared on exit.
    assert!(client.get_hibernation(&vault_id).is_none());

    // Immediately after early exit the vault must not be expired.
    assert!(!client.is_expired(&vault_id));

    // A full interval measured from the exit point must still be honored.
    env.ledger()
        .with_mut(|l| l.timestamp = elapsed + interval - 1);
    assert!(
        !client.is_expired(&vault_id),
        "remaining TTL budget must be preserved across early hibernation exit"
    );

    env.ledger().with_mut(|l| l.timestamp = elapsed + interval + 1);
    assert!(client.is_expired(&vault_id));
}

/// A pending admin transition (propose/accept with a 24h timelock) must be
/// unaffected by a concurrent vault hibernation cycle: hibernation only
/// pauses per-vault TTL expiry, it must not interact with or reset unrelated
/// contract-level pending state.
#[test]
fn test_pending_admin_transition_spans_hibernation_boundary() {
    let (env, owner, beneficiary, admin, _token, client) = setup();
    let interval = 1_000u64;
    let hibernation_duration = 100_000u64;

    let vault_id = client.create_vault(&owner, &beneficiary, &interval, &None);
    client.deposit(&vault_id, &owner, &10_000i128);

    env.ledger().with_mut(|l| l.timestamp = 0);

    // Propose a new admin (24h timelock) just before the vault hibernates.
    let new_admin = Address::generate(&env);
    client.propose_new_admin(&new_admin);
    assert_eq!(client.get_pending_admin(), Some(new_admin.clone()));

    // Vault owner hibernates, spanning the admin timelock window.
    client.enter_hibernation(&vault_id, &owner, &hibernation_duration);

    // Advance the ledger clock across both the hibernation window and the
    // 24h admin timelock (86_400s).
    env.ledger().with_mut(|l| l.timestamp = 90_000);

    // Pending admin transition must still be intact and unaffected by the
    // unrelated vault's hibernation state.
    assert_eq!(
        client.get_pending_admin(),
        Some(new_admin.clone()),
        "pending admin transition must survive a concurrent hibernation cycle"
    );

    // Accept the admin transfer now that the timelock has elapsed.
    client.accept_admin();
    assert_eq!(client.get_pending_admin(), None);

    // The hibernating vault's TTL state must be independently unaffected by
    // the admin transition completing.
    let entry = client
        .get_hibernation(&vault_id)
        .expect("hibernation entry must be untouched by admin transition");
    assert_eq!(entry.duration_seconds, hibernation_duration);
    assert!(!client.is_expired(&vault_id));

    // Sanity: original admin is gone as caller for admin-only ops (implicit
    // via successful accept_admin above); nothing else to assert here.
    let _ = admin;
}
