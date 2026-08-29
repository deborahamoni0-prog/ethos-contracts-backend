# Request Cost Estimation (#72)

The `POST /estimate-cost` endpoint returns a detailed Stellar/Soroban fee breakdown for any supported operation **before** submitting it on-chain. This lets clients display expected costs in your UI and avoid surprises.

## Endpoint

```
POST /estimate-cost
Content-Type: application/json
```

### Request body

| Field | Type | Required | Description |
|---|---|---|---|
| `operation` | string | ✅ | Operation type (see below) |
| `vault_id` | string | ❌ | Vault ID for vault-specific context |
| `amount_stroops` | integer | ❌ | Amount in stroops (for `deposit`/`withdraw`) |
| `bulk_count` | integer | ❌ | Number of vaults (for `bulk_check_in`) |
| `payload_bytes` | integer | ❌ | Payload size (for `custom`) |
| `include_scenarios` | boolean | ❌ | Include scenario variants in response |

### Supported operations

| Operation | Description |
|---|---|
| `create_vault` | Create a new vault (3 ledger writes + rent) |
| `check_in` | Extend vault TTL (1 write + rent) |
| `deposit` | Deposit funds to vault (2 writes) |
| `withdraw` | Withdraw funds from vault (2 writes, 2 reads) |
| `trigger_release` | Transfer balance to beneficiary on TTL expiry |
| `update_beneficiary` | Change the designated beneficiary address |
| `bulk_check_in` | Check in multiple vaults in one call (scales linearly) |
| `custom` | Arbitrary operation — cost driven by `payload_bytes` |

## Cost factors

All fees are in **stroops** (1 XLM = 10,000,000 stroops). Defaults mirror the Stellar testnet fee schedule as of 2026. Override via environment variables:

| Env var | Default | Description |
|---|---|---|
| `COST_BASE_FEE_STROOPS` | 100 | Base transaction fee |
| `COST_BYTE_FEE_STROOPS` | 10 | Per-byte instruction bandwidth fee |
| `COST_WRITE_ENTRY_FEE_STROOPS` | 2500 | Per ledger key written |
| `COST_READ_ENTRY_FEE_STROOPS` | 500 | Per ledger key read |
| `COST_RENT_FEE_PER_LEDGER_STROOPS` | 50 | State archival rent per ledger |
| `COST_DEFAULT_TTL_EXTENSION_LEDGERS` | 100 | Ledgers covered by a TTL extension |

## Response structure

```json
{
  "operation": "check_in",
  "breakdown": {
    "base_fee_stroops": 100,
    "instruction_fee_stroops": 2000,
    "write_entries_fee_stroops": 2500,
    "read_entries_fee_stroops": 500,
    "rent_fee_stroops": 5000,
    "total_stroops": 10100,
    "total_xlm": 0.00101,
    "notes": [
      "Updates TTL ledger entry",
      "Extends TTL by 100 ledgers"
    ]
  },
  "scenarios": [],
  "estimated_at": "2026-07-26T21:00:00Z",
  "docs_url": "/docs/cost-estimation"
}
```

## Scenario-based estimation

Pass `"include_scenarios": true` to get pre-built cost variants for common parameter sets:

- **Deposit**: `small_deposit` (100 XLM) and `large_deposit` (10,000 XLM)
- **Bulk check-in**: `bulk_10` (10 vaults) and `bulk_100` (100 vaults)

## Example: estimate a bulk check-in

```bash
curl -X POST http://localhost:3000/estimate-cost \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "bulk_check_in",
    "bulk_count": 50,
    "include_scenarios": true
  }'
```
