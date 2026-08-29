# Dead-Letter Queue (DLQ)

## Overview

Previously, a webhook delivery that exhausted its retry budget
(`webhook::attempt_delivery`) was logged and dropped — the event payload was
lost with no way to recover it. The dead-letter queue captures those
failures instead so operators can inspect what was lost and replay it once
the downstream endpoint recovers.

## Automatic Routing on Failure

`webhook::attempt_delivery` now calls `dlq::route_to_dlq` when the delivery
retry loop (`MAX_RETRIES = 4`, exponential back-off) is exhausted. Each
dead-lettered entry records:

| Field | Description |
|---|---|
| `id` | Generated UUID for the DLQ entry |
| `source` | Origin of the failure, e.g. `webhook:<registration_id>` |
| `target` | The URL the payload was destined for (used for replay) |
| `payload` | The original event payload, as JSON |
| `error` | Human-readable failure reason |
| `attempts` | Number of delivery attempts made before dead-lettering |
| `status` | `pending`, `replayed`, or `replay_failed` |
| `created_at` / `last_attempt_at` | Timestamps |

## Inspecting the Queue

```
GET /admin/dlq
GET /admin/dlq?status=pending
GET /admin/dlq?source=webhook&limit=50
```

Results are sorted newest-first and support filtering by `status` (substring
match on `source`) and an optional `limit`.

## Replaying Entries

```
POST /admin/dlq/replay
Content-Type: application/json

{ "id": "3fae2b1c-…" }
```

or replay everything currently pending:

```
POST /admin/dlq/replay
Content-Type: application/json

{ "replay_all": true }
```

Replay re-POSTs the original `payload` to the recorded `target`. Each
attempt updates the entry's `status` (`replayed` on a 2xx response,
`replay_failed` otherwise) and increments `attempts`. The endpoint returns a
per-entry breakdown:

```json
{
  "attempted": 3,
  "succeeded": 2,
  "failed": 1,
  "results": [
    { "id": "…", "success": true, "detail": "replay accepted with status 200 OK" },
    { "id": "…", "success": true, "detail": "replay accepted with status 200 OK" },
    { "id": "…", "success": false, "detail": "replay rejected with status 503 Service Unavailable" }
  ]
}
```

Entries without a recorded `target` cannot be replayed and are marked
`replay_failed` with an explanatory detail message.

## Storage

Entries are held in an in-memory store (`dlq::DlqStore`) scoped to the
process. A persistent-store implementation would be a natural follow-up if
dead-lettered payloads need to survive a restart.
