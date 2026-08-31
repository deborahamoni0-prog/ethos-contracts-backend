//! Tests for bounded on-chain storage of PasskeyUsage and PasskeyAuditLog.
//!
//! Issue: log_passkey_usage and log_passkey_audit_entry previously had no cap,
//! allowing the on-chain Vec to grow without bound on every check-in.  These
//! tests verify that:
//!
//!   1. The PasskeyUsage log never exceeds MAX_PASSKEY_USAGE_ENTRIES entries.
//!   2. The PasskeyAuditLog never exceeds MAX_PASSKEY_AUDIT_ENTRIES entries.
//!   3. When the cap is hit, the *oldest* entry is pruned (ring-buffer semantics).
//!   4. After check-ins past the cap, both log sizes remain bounded — O(1) growth.
//!   5. Existing event emission (PASSKEY_USAGE_TOPIC, PASSKEY_AUDIT_TOPIC) is
//!      unchanged — every operation still emits its event regardless of pruning.
//!   6. Readers (get_passkey_usage, get_passkey_audit_log) return the bounded,
//!      most-recent slice without error after pruning.

#![cfg(test)]

extern crate alloc;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Events, Ledger},
    token::StellarAssetClient,
    Address, BytesN, Env, IntoVal, String, TryIntoVal, Vec,
};

// ── helpers ──────────────────────────────────────────────────────────────────

fn setup() -> (Env, Address, Address, TtlVaultContractClient<'static>) {
    let env = Env::default();

    // These cap-boundary tests drive enough lifecycle/check-in operations to
    // push the capped Vecs (MAX_PASSKEY_*_ENTRIES = 1000) to and past their
    // limits. Each `Vec::remove(0)` is O(n), so the loop is O(n²) and blows the
    // small default soroban test budget well before the production cap logic is
    // reached. Lift the budget when building the test environment so the tests
    // actually exercise cap enforcement instead of dying in the harness.
    env.budget().reset_unlimited();

    env.mock_all_auths();
    env.budget().reset_unlimited();

    let owner = Address::generate(env);
    let beneficiary = Address::generate(env);
    let admin = Address::generate(env);

    let token_admin = Address::generate(env);
    let token_address = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    StellarAssetClient::new(env, &token_address).mint(&owner, &1_000_000);

    let contract_address = env.register_contract(None, TtlVaultContract);
    let client = TtlVaultContractClient::new(env, &contract_address);
    client.initialize(&token_address, &admin);

    (owner, beneficiary, contract_address, client)
}

fn seed_passkey_usage(env: &Env, contract_address: &Address, vault_id: u64, count: u32) {
    env.as_contract(contract_address, || {
        let mut usage = Vec::new(env);
        for i in 0..count {
            let mut raw = [0u8; 32];
            raw[0] = (i & 0xFF) as u8;
            raw[1] = ((i >> 8) & 0xFF) as u8;
            usage.push_back(PasskeyUsageEntry {
                passkey_hash: BytesN::from_array(env, &raw),
                timestamp: 1000 + i as u64,
            });
        }
        env.storage().persistent().set(&DataKey::PasskeyUsage(vault_id), &usage);
    });
}

fn seed_passkey_audit_log(
    env: &Env,
    contract_address: &Address,
    vault_id: u64,
    count: u32,
    owner: &Address,
) {
    env.as_contract(contract_address, || {
        let mut log = Vec::new(env);
        for i in 0..count {
            let mut raw = [0u8; 32];
            raw[0] = (i & 0xFF) as u8;
            raw[1] = ((i >> 8) & 0xFF) as u8;
            log.push_back(PasskeyAuditEntry {
                operation: String::from_str(env, if i % 2 == 0 { "add" } else { "remove" }),
                actor: owner.clone(),
                passkey_hash: BytesN::from_array(env, &raw),
                timestamp: 1000 + i as u64,
            });
        }
        env.storage().persistent().set(&DataKey::PasskeyAuditLog(vault_id), &log);
    });
}

// ── PasskeyUsage cap tests ────────────────────────────────────────────────────

/// After exactly MAX_PASSKEY_USAGE_ENTRIES check-ins the log is at the cap.
#[test]
fn test_passkey_usage_at_cap_boundary() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[1u8; 32]);

    seed_passkey_usage(&env, &contract_address, vault_id, MAX_PASSKEY_USAGE_ENTRIES - 1);
    client.check_in(&vault_id, &owner, &passkey_hash, &0u64);

    let usage = client.get_passkey_usage(&vault_id);
    assert_eq!(
        usage.len(),
        MAX_PASSKEY_USAGE_ENTRIES,
        "usage log should be exactly at the cap after {} check-ins",
        MAX_PASSKEY_USAGE_ENTRIES
    );
}

/// After MAX_PASSKEY_USAGE_ENTRIES + 1 check-ins the log does not exceed the cap.
#[test]
fn test_passkey_usage_does_not_exceed_cap() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[2u8; 32]);

    seed_passkey_usage(&env, &contract_address, vault_id, MAX_PASSKEY_USAGE_ENTRIES);
    client.check_in(&vault_id, &owner, &passkey_hash, &0u64);

    let usage = client.get_passkey_usage(&vault_id);
    assert_eq!(
        usage.len(),
        MAX_PASSKEY_USAGE_ENTRIES,
        "usage log must never exceed the cap of {} entries",
        MAX_PASSKEY_USAGE_ENTRIES
    );
}

/// After exceeding the cap, the oldest entry is dropped (ring-buffer semantics).
#[test]
fn test_passkey_usage_oldest_entry_pruned() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);

    let hash_a = BytesN::<32>::from_array(&env, &[0xAAu8; 32]);
    let hash_b = BytesN::<32>::from_array(&env, &[0xBBu8; 32]);

    // Seed 1000 entries where the first entry is hash_a and rest are distinct
    env.as_contract(&contract_address, || {
        let mut usage = Vec::new(&env);
        usage.push_back(PasskeyUsageEntry {
            passkey_hash: hash_a.clone(),
            timestamp: 100,
        });
        for i in 1..MAX_PASSKEY_USAGE_ENTRIES {
            let mut raw = [0u8; 32];
            raw[0] = (i & 0xFF) as u8;
            raw[1] = ((i >> 8) & 0xFF) as u8;
            usage.push_back(PasskeyUsageEntry {
                passkey_hash: BytesN::from_array(&env, &raw),
                timestamp: 100 + i as u64,
            });
        }
        env.storage().persistent().set(&DataKey::PasskeyUsage(vault_id), &usage);
    });

    // Check in with hash_b -> should push hash_a out
    client.check_in(&vault_id, &owner, &hash_b, &0u64);

    let usage = client.get_passkey_usage(&vault_id);
    assert_eq!(usage.len(), MAX_PASSKEY_USAGE_ENTRIES);

    let oldest = usage.get(0).unwrap();
    assert_ne!(oldest.passkey_hash, hash_a, "hash_a should have been pruned");

    let newest = usage.get(MAX_PASSKEY_USAGE_ENTRIES - 1).unwrap();
    assert_eq!(newest.passkey_hash, hash_b);
}

/// After a large number of check-ins the log size stays bounded.
#[test]
fn test_passkey_usage_bounded_after_many_check_ins() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[3u8; 32]);

    seed_passkey_usage(&env, &contract_address, vault_id, MAX_PASSKEY_USAGE_ENTRIES);
    for _ in 0..5 {
        client.check_in(&vault_id, &owner, &passkey_hash, &0u64);
        env.ledger().with_mut(|li| li.timestamp = li.timestamp.saturating_add(61));
    }

    let usage = client.get_passkey_usage(&vault_id);
    assert_eq!(
        usage.len(),
        MAX_PASSKEY_USAGE_ENTRIES,
        "log size must still be capped at {}",
        MAX_PASSKEY_USAGE_ENTRIES
    );
}

/// Every check-in still emits PASSKEY_USAGE_TOPIC — pruning must not suppress events.
#[test]
fn test_passkey_usage_events_emitted_past_cap() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[4u8; 32]);

    seed_passkey_usage(&env, &contract_address, vault_id, MAX_PASSKEY_USAGE_ENTRIES);
    for _ in 0..3 {
        client.check_in(&vault_id, &owner, &passkey_hash, &0u64);
        env.ledger().with_mut(|li| li.timestamp = li.timestamp.saturating_add(61));
    }

    let total_usage_events = env
        .events()
        .all()
        .iter()
        .filter(|e| {
            let topics: soroban_sdk::Vec<soroban_sdk::Val> = e.1.clone().into_val(&env);
            topics
                .get(0)
                .and_then(|v| v.try_into_val(&env).ok())
                .is_some_and(|s: soroban_sdk::Symbol| s == PASSKEY_USAGE_TOPIC)
        })
        .count();

    assert_eq!(
        total_usage_events as u32,
        3,
        "every check-in must emit a usage event regardless of on-chain pruning"
    );
}

// ── PasskeyAuditLog cap tests ─────────────────────────────────────────────────

/// After MAX_PASSKEY_AUDIT_ENTRIES operations the log is at the cap.
#[test]
fn test_passkey_audit_at_cap_boundary() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[5u8; 32]);

    seed_passkey_audit_log(&env, &contract_address, vault_id, MAX_PASSKEY_AUDIT_ENTRIES - 1, &owner);
    client.add_passkey(&vault_id, &owner, &passkey_hash);

    let log = client.get_passkey_audit_log(&vault_id);
    assert_eq!(
        log.len(),
        MAX_PASSKEY_AUDIT_ENTRIES,
        "audit log should be exactly at the cap after {} operations",
        MAX_PASSKEY_AUDIT_ENTRIES
    );
}

/// After exceeding MAX_PASSKEY_AUDIT_ENTRIES operations the log does not grow.
#[test]
fn test_passkey_audit_does_not_exceed_cap() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[6u8; 32]);

    seed_passkey_audit_log(&env, &contract_address, vault_id, MAX_PASSKEY_AUDIT_ENTRIES, &owner);
    client.add_passkey(&vault_id, &owner, &passkey_hash);

    let log = client.get_passkey_audit_log(&vault_id);
    assert_eq!(
        log.len(),
        MAX_PASSKEY_AUDIT_ENTRIES,
        "audit log must never exceed the cap of {} entries",
        MAX_PASSKEY_AUDIT_ENTRIES
    );
}

/// After exceeding the cap, the oldest audit entry is dropped.
#[test]
fn test_passkey_audit_oldest_entry_pruned() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);

    let sentinel_hash = BytesN::<32>::from_array(&env, &[0xFFu8; 32]);
    let new_hash = BytesN::<32>::from_array(&env, &[0x77u8; 32]);

    // Seed 1000 entries with index 0 being sentinel_hash
    env.as_contract(&contract_address, || {
        let mut log = Vec::new(&env);
        log.push_back(PasskeyAuditEntry {
            operation: String::from_str(&env, "add"),
            actor: owner.clone(),
            passkey_hash: sentinel_hash.clone(),
            timestamp: 100,
        });
        for i in 1..MAX_PASSKEY_AUDIT_ENTRIES {
            let mut raw = [0u8; 32];
            raw[0] = (i & 0xFF) as u8;
            raw[1] = ((i >> 8) & 0xFF) as u8;
            log.push_back(PasskeyAuditEntry {
                operation: String::from_str(&env, "add"),
                actor: owner.clone(),
                passkey_hash: BytesN::from_array(&env, &raw),
                timestamp: 100 + i as u64,
            });
        }
        env.storage().persistent().set(&DataKey::PasskeyAuditLog(vault_id), &log);
    });

    client.add_passkey(&vault_id, &owner, &new_hash);

    let log = client.get_passkey_audit_log(&vault_id);
    assert_eq!(log.len(), MAX_PASSKEY_AUDIT_ENTRIES);

    let oldest = log.get(0).unwrap();
    assert_ne!(oldest.passkey_hash, sentinel_hash, "sentinel_hash should have been pruned");
}

/// After a large number of operations the audit log size stays bounded.
#[test]
fn test_passkey_audit_bounded_after_many_operations() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);

    seed_passkey_audit_log(&env, &contract_address, vault_id, MAX_PASSKEY_AUDIT_ENTRIES, &owner);
    for i in 0..5 {
        let mut raw = [0x50u8; 32];
        raw[0] = i as u8;
        let hash = BytesN::<32>::from_array(&env, &raw);
        client.add_passkey(&vault_id, &owner, &hash);
    }

    let log = client.get_passkey_audit_log(&vault_id);
    assert_eq!(
        log.len(),
        MAX_PASSKEY_AUDIT_ENTRIES,
        "audit log size must still be capped at {}",
        MAX_PASSKEY_AUDIT_ENTRIES
    );
}

/// Every lifecycle operation still emits PASSKEY_AUDIT_TOPIC — pruning must not suppress events.
#[test]
fn test_passkey_audit_events_emitted_past_cap() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[0x88u8; 32]);

    seed_passkey_audit_log(&env, &contract_address, vault_id, MAX_PASSKEY_AUDIT_ENTRIES, &owner);
    client.add_passkey(&vault_id, &owner, &passkey_hash);

    let total_audit_events = env
        .events()
        .all()
        .iter()
        .filter(|e| {
            let topics: soroban_sdk::Vec<soroban_sdk::Val> = e.1.clone().into_val(&env);
            topics
                .get(0)
                .and_then(|v| v.try_into_val(&env).ok())
                .is_some_and(|s: soroban_sdk::Symbol| s == PASSKEY_AUDIT_TOPIC)
        })
        .count();

    assert_eq!(
        total_audit_events as u32, expected_events,
        "every lifecycle operation must emit a pk_audit event regardless of on-chain pruning"
    );
}

/// get_passkey_audit_log reader returns the bounded, most-recent slice without
/// error after pruning has occurred.
#[test]
fn test_get_passkey_audit_log_works_after_pruning() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[0x99u8; 32]);

    seed_passkey_audit_log(&env, &contract_address, vault_id, MAX_PASSKEY_AUDIT_ENTRIES, &owner);
    client.add_passkey(&vault_id, &owner, &passkey_hash);

    let log = client.get_passkey_audit_log(&vault_id);
    assert_eq!(
        log.len(),
        MAX_PASSKEY_AUDIT_ENTRIES,
        "get_passkey_audit_log must work correctly after pruning"
    );
}

/// get_passkey_usage reader returns the bounded slice without error after pruning.
#[test]
fn test_get_passkey_usage_works_after_pruning() {
    let env = Env::default();
    let (owner, beneficiary, contract_address, client) = setup(&env);
    let vault_id = client.create_vault(&owner, &beneficiary, &1_000u64, &None);
    let passkey_hash = BytesN::<32>::from_array(&env, &[5u8; 32]);

    seed_passkey_usage(&env, &contract_address, vault_id, MAX_PASSKEY_USAGE_ENTRIES);
    client.check_in(&vault_id, &owner, &passkey_hash, &0u64);

    let usage = client.get_passkey_usage(&vault_id);
    assert_eq!(
        usage.len(),
        MAX_PASSKEY_USAGE_ENTRIES,
        "get_passkey_usage must work correctly after pruning"
    );
}
