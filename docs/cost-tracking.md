# Operational Cost Tracking

Operational costs weren't attributed to the operations or teams that
incurred them, which made cost optimization guesswork. This module records
tagged cost entries per operation and produces aggregate reports and
proportional allocation.

## Recording a cost entry

`POST /admin/cost/entries`

```json
{
  "operation": "db.query",
  "tags": { "team": "vaults", "region": "us-east-1" },
  "amount": 0.0042,
  "currency": "USD"
}
```

Call this at the point where a billable operation completes (e.g. after a
DB query, an external API call, or a compute-heavy job) with whatever tags
are relevant for later attribution — team, vault ID, region, environment,
etc. Tags are free-form key/value pairs, so new attribution dimensions
don't require code changes.

## Cost reports

`GET /admin/cost/report` returns:

```json
{
  "total_amount": 145.32,
  "currency": "USD",
  "entry_count": 5000,
  "by_operation": { "db.query": 90.10, "api.call": 55.22 },
  "by_tag": {
    "team": { "vaults": 100.00, "billing": 45.32 },
    "region": { "us-east-1": 145.32 }
  }
}
```

`by_operation` sums cost per operation name; `by_tag` sums cost per tag key,
broken down by each value seen under that key.

## Cost allocation

`POST /admin/cost/allocate` splits a shared cost (e.g. a monthly cloud
invoice line item that isn't itemized) across the values of a chosen tag,
proportional to that tag's historical share of recorded cost:

```json
{ "total_amount": 1000.0, "tag_key": "team" }
```

```json
{
  "tag_key": "team",
  "total_amount": 1000.0,
  "allocations": { "vaults": 750.0, "billing": 250.0 }
}
```

If no historical entries carry the given `tag_key`, `allocations` is empty
— record cost entries with that tag before relying on allocation for it.

## Usage pattern

1. Instrument billable code paths to call `POST /admin/cost/entries` (or use
   `CostState::record` directly from Rust call sites) with team/vault/region
   tags.
2. Pull `GET /admin/cost/report` into a dashboard or scheduled export for
   ongoing visibility.
3. When an un-itemized shared cost arrives (e.g. a platform-wide invoice
   line), use `POST /admin/cost/allocate` to split it fairly across teams
   based on their actual historical usage.
