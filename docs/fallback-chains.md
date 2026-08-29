# Fallback Chains

## Overview

Several backend subsystems (webhook delivery, RPC calls, notification
providers) depend on a single downstream target. If that target goes down,
the whole operation fails even when a secondary target could have served the
request. Fallback chains let operators register an ordered list of targets
for a resource so requests cascade to the next target instead of failing
outright.

## Registering a Chain

```
POST /admin/fallback-chains
Content-Type: application/json

{
  "name": "vault-created-webhooks",
  "resource": "webhook:vault-created",
  "targets": [
    { "name": "primary", "endpoint": "https://hooks.example.com/primary", "priority": 0 },
    { "name": "secondary", "endpoint": "https://hooks.example.com/secondary", "priority": 1 },
    { "name": "tertiary", "endpoint": "https://hooks.example.com/tertiary", "priority": 2 }
  ]
}
```

Returns `201 Created` with the stored chain (targets sorted by `priority`,
lowest first). At least one target is required.

## Listing / Fetching Chains

```
GET /admin/fallback-chains
GET /admin/fallback-chains/:id
```

## Cascading Fallback Semantics

`fallback::cascade()` walks a chain's targets in priority order and invokes
a caller-supplied attempt function for each. The first target that succeeds
resolves the cascade; if it wasn't the highest-priority target, the result is
flagged `degraded: true` so operators can see the system served the request
in a degraded state rather than silently.

If every target fails, `resolved_target` is `null` and every attempt is
recorded with its failure reason.

## Testing a Chain

Operators can dry-run a chain without touching real infrastructure:

```
POST /admin/fallback-chains/:id/test
Content-Type: application/json

{ "simulate_failures": ["primary", "secondary"] }
```

This simulates the named targets failing and returns the resulting
`FallbackExecutionResult`, showing which target the cascade would have
resolved to (`tertiary` in the example above) and whether the result would
have been degraded.

## Response Shape

```json
{
  "chain_id": "…",
  "attempts": [
    { "target": "primary", "priority": 0, "succeeded": false, "error": "simulated failure" },
    { "target": "secondary", "priority": 1, "succeeded": false, "error": "simulated failure" },
    { "target": "tertiary", "priority": 2, "succeeded": true, "error": null }
  ],
  "resolved_target": "https://hooks.example.com/tertiary",
  "degraded": true
}
```

## Storage

Chains are held in an in-memory store (`fallback::FallbackStore`) scoped to
the process, mirroring the existing webhook registry pattern in
`webhook.rs`. Restarting the service clears registered chains.
