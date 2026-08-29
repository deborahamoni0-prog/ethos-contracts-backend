# Bulk Operation Queuing (#74)

The job queue system enables asynchronous execution of bulk operations, preventing long-running tasks from blocking API responses.

## Architecture

```
Client Request (POST /jobs)
      │
      ▼
  Create Job (status: queued)
      │
      ├──► Spawn background task
      │
      └──► Return 202 Accepted + job_id
                  │
                  ▼
            Background Task:
              • Mark status: running
              • Process items in batches
              • Update progress
              • Mark status: completed/failed
```

Jobs are stored in an in-memory `JobStore` (`Arc<Mutex<HashMap<String, BulkJob>>>`). For production, replace with a persistent queue (PostgreSQL, Redis, or RabbitMQ).

## Endpoints

### `POST /jobs` — submit a bulk operation

```json
{
  "operation": "update_ttl",
  "items": ["vault_id_1", "vault_id_2", "vault_id_3"],
  "label": "Monthly TTL refresh"
}
```

**Response** (202 Accepted):

```json
{
  "job_id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "queued",
  "message": "Job queued for processing"
}
```

### `GET /jobs/:job_id` — get job status and progress

**Response**:

```json
{
  "job": {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "operation": "update_ttl",
    "status": "running",
    "progress": 65,
    "total_items": 100,
    "processed_items": 65,
    "failed_items": 2,
    "created_at": "2026-07-27T08:00:00Z",
    "started_at": "2026-07-27T08:00:01Z",
    "completed_at": null,
    "result": null,
    "error": null,
    "label": "Monthly TTL refresh"
  },
  "estimated_seconds_remaining": 18
}
```

### `GET /jobs` — list all jobs (with optional filter)

Query parameters:
- `status` — filter by job status (`queued`, `running`, `completed`, `failed`, `cancelled`)

**Response**:

```json
[
  {
    "id": "550e8400-e29b-41d4-a716-446655440000",
    "operation": "send_reminders",
    "status": "completed",
    "progress": 100,
    "total_items": 50,
    "processed_items": 50,
    "failed_items": 0,
    ...
  }
]
```

### `DELETE /jobs/:job_id` — cancel a queued or running job

**Response**: 204 No Content (if cancelled), or 404 Not Found.

## Supported operation types

| Operation | Description | Items format |
|---|---|---|
| `update_ttl` | Batch-update TTL for multiple vaults | Array of vault IDs |
| `send_reminders` | Send check-in reminders to vault owners | Array of owner addresses |
| `export_vaults` | Export vault data to JSON | Array of vault IDs |
| `retention_sweep` | Apply retention policies to time-series data | Empty (applies to all series) |
| `custom` | Arbitrary operation defined in payload | Caller-defined structure |

## Job lifecycle states

```
Queued → Running → Completed
                 → Failed
                 → Cancelled
```

- **Queued**: Job submitted, waiting for background task to pick it up
- **Running**: Background task is actively processing items
- **Completed**: All items processed successfully (100% progress)
- **Failed**: Processing encountered an unrecoverable error
- **Cancelled**: User cancelled the job before/during execution

## Progress tracking

- `progress`: Percentage (0–100) of completion
- `processed_items`: Number of items successfully processed so far
- `failed_items`: Number of items that resulted in errors (still counts toward progress)
- `estimated_seconds_remaining`: Linear extrapolation based on current processing rate (only when status is `running`)

## Example: bulk check-in for 100 vaults

```bash
# 1. Submit the job
curl -X POST http://localhost:3000/jobs \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "update_ttl",
    "items": ["vault_1", "vault_2", ..., "vault_100"],
    "label": "Quarterly renewal"
  }'

# Response: { "job_id": "abc123", "status": "queued", ... }

# 2. Poll for progress
curl http://localhost:3000/jobs/abc123

# 3. Cancel if needed
curl -X DELETE http://localhost:3000/jobs/abc123
```

## Integrating job processing

To add support for a new operation type, implement the processing logic in `jobs::process_job`:

```rust
match operation {
    BulkOperationType::UpdateTtl => {
        for vault_id in items {
            // Call vault TTL update logic
            update_vault_ttl(&vault_id)?;
            job.advance(1, 0);
        }
    },
    // ... other operations
}
```

## Notes

- **Non-blocking**: The `POST /jobs` handler returns immediately; processing happens in a spawned `tokio::task::spawn_blocking` task
- **Error handling**: Individual item failures increment `failed_items` but do not halt the job; the job completes with a partial success result
- **Concurrency**: Each job runs in its own task. Multiple jobs can run concurrently (up to the Tokio thread pool limit)
- **Idempotency**: Submitting the same operation multiple times creates separate jobs. Implement deduplication in the handler if needed (e.g., by checking existing queued jobs for the same `label`)
