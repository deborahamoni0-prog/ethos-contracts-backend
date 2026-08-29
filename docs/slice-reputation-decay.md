# Issue #41 — Slice Reputation Decay

## Overview

Attestor reputation is now dynamic rather than static. The reputation decay mechanism reflects degraded performance over time, allowing vault owners to penalize unreliable attestors and reward performance improvements.

## Architecture

### Core Concepts

**Reputation Factor** — A value between 0-10,000 (BPS) that represents an attestor's current standing on a slice.
- **10,000** = Full reputation (newly created attestor default)
- **5,000** = 50% reputation (moderate degradation)
- **0** = No reputation (complete failure)

**Decay Rate** — A BPS value (0-10,000) that controls how aggressively reputation degrades or recovers.
- **decay_rate = 0**: Complete decay (reputation → 0)
- **decay_rate = 5,000**: 50% decay (new_rep = current_rep × 0.5)
- **decay_rate = 10,000**: No decay (reputation preserved)

### Storage Model

Reputation decay data is persisted in three storage layers:

1. **Current Reputation Factor** — Latest reputation score for an attestor on a slice
   - Key: `ReputationDecay(slice_id, attestor)`
   - Value: `u32` (0-10,000)
   - Default: 10,000 (full reputation if no decay history)

2. **Decay History** — Chronological log of reputation changes
   - Key: `DecayHistory(slice_id, attestor, entry_index)`
   - Value: `DecayHistoryEntry` with timestamp, decay_rate, before/after values, and reason
   - Supports pagination and limit-based retrieval

3. **History Count** — Number of decay history entries (for pagination)
   - Key: `DecayHistoryCount(slice_id, attestor)`
   - Value: `u64`

All entries are persisted with extended TTL to remain available for the vault's lifetime.

## API Functions

### Decay Operations

#### `apply_reputation_decay(vault_id, caller, slice_id, attestor, decay_rate_bps, reason) -> u32`

Apply reputation decay to an attestor on a slice.

**Parameters:**
- `vault_id` — Vault ID (owner verification)
- `caller` — Address of the vault owner
- `slice_id` — Slice identifier
- `attestor` — Address of the attestor
- `decay_rate_bps` — Decay rate in BPS (0-10,000)
- `reason` — Descriptive reason for the decay (e.g., "low_success_rate", "high_latency", "manual_penalty")

**Returns:** New reputation factor (0-10,000)

**Formula:**
```
new_reputation = current_reputation × decay_rate_bps / 10_000
```

**Examples:**
- Decay by 50%: `decay_rate_bps = 5,000` → `new_rep = current_rep / 2`
- Complete decay: `decay_rate_bps = 0` → `new_rep = 0`
- No decay: `decay_rate_bps = 10,000` → `new_rep = current_rep`

**Validation:**
- Only vault owner may apply decay
- `decay_rate_bps` must be in range [0, 10,000]

**Events:**
- Emits `ReputationDecayAppliedEvent` with decay details
- Records entry in decay history

---

#### `apply_reputation_recovery(vault_id, caller, slice_id, attestor, improvement_rate_bps) -> u32`

Recover reputation for an attestor when performance improves.

**Parameters:**
- `vault_id` — Vault ID (owner verification)
- `caller` — Address of the vault owner
- `slice_id` — Slice identifier
- `attestor` — Address of the attestor
- `improvement_rate_bps` — Recovery rate in BPS (0-10,000)

**Returns:** New reputation factor (0-10,000)

**Formula:**
```
recovery_amount = (10_000 - current_reputation) × improvement_rate_bps / 10_000
new_reputation = min(current_reputation + recovery_amount, 10_000)
```

**Examples:**
- Full recovery: `improvement_rate = 10,000` → `new_rep = 10,000` (clamped to max)
- 50% recovery: `improvement_rate = 5,000` → `new_rep = current + (10_000 - current) / 2`
- No recovery: `improvement_rate = 0` → `new_rep = current_rep`

**Behavior:**
- If reputation is already at 10,000 (maximum), recovery is no-op
- Recovery asymptotically approaches 10,000 but never exceeds it
- Diminishing returns prevent rapid reputation swings

**Validation:**
- Only vault owner may apply recovery
- `improvement_rate_bps` must be in range [0, 10,000]

**Events:**
- Emits `ReputationRecoveredEvent` with recovery details
- Records entry in decay history

---

### Query Operations

#### `get_reputation_factor(slice_id, attestor) -> u32`

Retrieve the current reputation factor for an attestor on a slice.

**Returns:** Reputation factor (0-10,000), defaults to 10,000 if no decay history exists

---

#### `get_decay_history(slice_id, attestor, limit) -> Vec<DecayHistoryEntry>`

Retrieve decay history for an attestor on a slice.

**Parameters:**
- `slice_id` — Slice identifier
- `attestor` — Address of the attestor
- `limit` — Maximum number of entries to return

**Returns:** Vector of `DecayHistoryEntry` objects, most recent first (reverse chronological)

**Entry Structure:**
```rust
pub struct DecayHistoryEntry {
    pub applied_at: u64,           // Ledger timestamp
    pub decay_rate_bps: u32,       // Rate applied (for both decay and recovery)
    pub reputation_before: u32,    // Reputation before change
    pub reputation_after: u32,     // Reputation after change
    pub reason: String,            // Reason/description
}
```

---

## Events

### `ReputationDecayAppliedEvent`

Emitted when `apply_reputation_decay()` is executed.

```rust
pub struct ReputationDecayAppliedEvent {
    pub slice_id: u64,
    pub attestor: Address,
    pub decay_rate_bps: u32,
    pub new_reputation_factor: u32,
    pub reason: String,
}
```

**Topic:** `reputation_decay` (symbol_short: "rep_dec")

---

### `ReputationRecoveredEvent`

Emitted when `apply_reputation_recovery()` is executed.

```rust
pub struct ReputationRecoveredEvent {
    pub slice_id: u64,
    pub attestor: Address,
    pub improvement_factor: u32,
    pub new_reputation_factor: u32,
}
```

**Topic:** `reputation_recovered` (symbol_short: "rep_rec")

---

## Workflow Examples

### Scenario 1: Progressive Degradation

An attestor experiences increasing failures, triggering cascading decay:

```
Timestep 1: reputation = 10,000 (fresh attestor)
  → apply_reputation_decay(decay_rate=8_000, reason="1_failure")
  → reputation = 8,000

Timestep 2: reputation = 8,000
  → apply_reputation_decay(decay_rate=7_500, reason="2_failures")
  → reputation = 8,000 × 7_500 / 10_000 = 6,000

Timestep 3: reputation = 6,000
  → apply_reputation_decay(decay_rate=5_000, reason="high_latency")
  → reputation = 6,000 × 5_000 / 10_000 = 3,000
```

History log (most recent first):
```
Entry 1: {before: 6,000, after: 3,000, reason: "high_latency"}
Entry 2: {before: 8,000, after: 6,000, reason: "2_failures"}
Entry 3: {before: 10,000, after: 8,000, reason: "1_failure"}
```

### Scenario 2: Recovery After Improvement

Reputation is restored when performance improves:

```
Current reputation: 3,000

apply_reputation_recovery(improvement_rate=5_000)
  → recovery_amount = (10_000 - 3_000) × 5_000 / 10_000 = 3,500
  → new_reputation = 3,000 + 3,500 = 6,500

apply_reputation_recovery(improvement_rate=10_000)  [full recovery]
  → recovery_amount = (10_000 - 6,500) × 10_000 / 10_000 = 3,500
  → new_reputation = 6,500 + 3,500 = 10,000 [clamped to max]
```

---

## Integration with Slice Performance Weighting

Reputation decay **does not directly modify BPS weights**. Instead, it provides a separate confidence metric for vault owners to consider when reweighting attestors.

**Recommended Workflow:**
1. Reputation decays over time as performance degrades
2. Vault owner queries `get_reputation_factor()` and `get_decay_history()`
3. If reputation is critically low (e.g., < 2,000), owner may:
   - Manually apply further decay via `apply_reputation_decay()`
   - Call `reweight_slice()` to reduce BPS weight
   - Replace the attestor entirely

---

## Error Handling

| Error | Cause | Mitigation |
|-------|-------|-----------|
| `NotOwner` | Caller is not vault owner | Only vault owner can apply decay/recovery |
| `InvalidBps` | `decay_rate_bps` or `improvement_rate_bps` > 10,000 | Use BPS values in range [0, 10,000] |
| `VaultNotFound` | Vault ID does not exist | Verify vault_id is correct |

---

## Testing

Comprehensive test suite covers:

✅ **Decay Operations:**
- Single decay application
- Cascading decay (multiple decays in sequence)
- Full decay (reputation → 0)
- No decay (reputation preserved)

✅ **Recovery Operations:**
- 50% recovery
- Full recovery (reputation → max)
- No recovery (reputation unchanged)
- Already-at-max handling

✅ **History Tracking:**
- History entries record before/after values
- Most recent entries returned first
- Pagination respects limit parameter
- Empty history when no changes

✅ **Authorization:**
- Non-owners rejected
- Invalid BPS values rejected
- Owner-only operations enforced

All 16 new tests pass, plus 130 existing tests remain unaffected (146 total).

---

## Performance Considerations

### Storage Efficiency

- **Current Reputation:** One entry per attestor per slice (minimal overhead)
- **Decay History:** Linear growth with each decay/recovery operation
  - Typical usage: 10-50 entries per attestor lifetime
  - Query complexity: O(limit) with reverse iteration

### TTL Management

All decay entries extend their TTL when accessed/modified, ensuring persistence for the vault's lifetime.

---

## Future Enhancements

1. **Automatic Decay Triggers** — Automatically apply decay based on performance metrics
2. **Reputation Thresholds** — Define critical reputation levels with automatic alerts
3. **Reputation-Based Weight Adjustment** — Directly incorporate reputation into BPS weight calculations
4. **Governance** — Allow beneficiaries or multi-sig to approve reputation changes
5. **Time-Based Decay** — Gradually decay reputation if no new performance observations are recorded

---

## Related Issues

- **Issue #36** — Slice Performance-Based Weighting (parent feature)
- **Issue #44** — Slice Composition Validation Rules Engine (complimentary feature)
