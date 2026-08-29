# Code Changes Summary - Graceful Degradation State Fix

## File: backend/src/degradation.rs

### Change 1: Module Documentation Updated

**From:**
```rust
//! - [`DegradationState`] — registry of capability -> status, defaulting to
//!   `Full` for anything not explicitly registered
```

**To:**
```rust
//! Capability statuses are persisted in the SQL database and shared across all
//! instances in a load-balanced deployment. When an operator marks a capability
//! degraded via `POST /admin/capabilities`, all instances immediately observe
//! the change on subsequent reads.
//! 
//! - [`DegradationState`] — registry of capability -> status backed by SQL,
//!   defaulting to `Full` for anything not explicitly registered
```

### Change 2: Import Statements Updated

**From:**
```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
```

**To:**
```rust
use std::sync::Arc;
```

### Change 3: DegradationState Struct Redefined

**From:**
```rust
pub struct DegradationState {
    registry: Mutex<HashMap<String, CapabilityStatus>>,
}

impl DegradationState {
    pub fn new() -> Self {
        Self {
            registry: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for DegradationState {
    fn default() -> Self {
        Self::new()
    }
}
```

**To:**
```rust
/// Database-backed registry of capability statuses.
/// All instances in a load-balanced deployment share the same storage,
/// ensuring consistent degradation state across the fleet.
pub struct DegradationState {
    db: Arc<crate::db::Db>,
}

impl DegradationState {
    pub fn new(db: Arc<crate::db::Db>) -> Self {
        Self { db }
    }
}
```

### Change 4: Methods Return Results

**From:**
```rust
pub fn set_status(
    &self,
    name: &str,
    level: DegradationLevel,
    reason: Option<String>,
    fallback_available: bool,
) -> CapabilityStatus {
    let status = CapabilityStatus { ... };
    self.registry.lock().unwrap().insert(name.to_string(), status.clone());
    status
}

pub fn check(&self, name: &str) -> CapabilityStatus {
    self.registry.lock().unwrap().get(name).cloned().unwrap_or_else(...)
}

pub fn list(&self) -> Vec<CapabilityStatus> {
    self.registry.lock().unwrap().values().cloned().collect()
}

pub fn negotiate(&self, requested: &[String]) -> NegotiationResult {
    // ...
}
```

**To:**
```rust
pub fn set_status(
    &self,
    name: &str,
    level: DegradationLevel,
    reason: Option<String>,
    fallback_available: bool,
) -> Result<CapabilityStatus, String> {
    let status = CapabilityStatus { ... };
    self.db.set_capability_status(&status)
        .map_err(|e| format!("failed to set capability status: {}", e))?;
    Ok(status)
}

pub fn check(&self, name: &str) -> Result<CapabilityStatus, String> {
    self.db.get_capability_status(name)
        .map_err(|e| format!("failed to get capability status: {}", e))
}

pub fn list(&self) -> Result<Vec<CapabilityStatus>, String> {
    self.db.list_capability_statuses()
        .map_err(|e| format!("failed to list capability statuses: {}", e))
}

pub fn negotiate(&self, requested: &[String]) -> Result<NegotiationResult, String> {
    let capabilities: Result<Vec<NegotiatedCapability>, String> = requested
        .iter()
        .map(|name| {
            let status = self.check(name)?;
            Ok(NegotiatedCapability { ... })
        })
        .collect();
    
    let capabilities = capabilities?;
    // ...
    Ok(NegotiationResult { ... })
}
```

### Change 5: HTTP Handlers Updated for Result Types

**From:**
```rust
pub async fn set_capability(
    State(state): State<Arc<DegradationState>>,
    Json(body): Json<SetCapabilityRequest>,
) -> Result<Json<CapabilityStatus>, (StatusCode, Json<serde_json::Value>)> {
    if body.name.trim().is_empty() { ... }
    Ok(Json(state.set_status(...)))
}

pub async fn list_capabilities(
    State(state): State<Arc<DegradationState>>,
) -> Json<Vec<CapabilityStatus>> {
    Json(state.list())
}

pub async fn negotiate_capabilities(
    State(state): State<Arc<DegradationState>>,
    Json(body): Json<NegotiateRequest>,
) -> Json<NegotiationResult> {
    Json(state.negotiate(&body.requested))
}

pub async fn capability_fallback(...) -> Result<..., StatusCode> {
    let status = state.check(&name);
    if status.level == DegradationLevel::Full { ... }
    // ...
}
```

**To:**
```rust
pub async fn set_capability(
    State(state): State<Arc<DegradationState>>,
    Json(body): Json<SetCapabilityRequest>,
) -> Result<Json<CapabilityStatus>, (StatusCode, Json<serde_json::Value>)> {
    if body.name.trim().is_empty() { ... }
    state.set_status(...)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, 
                      Json(serde_json::json!({ "error": e }))))
}

pub async fn list_capabilities(
    State(state): State<Arc<DegradationState>>,
) -> Result<Json<Vec<CapabilityStatus>>, (StatusCode, Json<serde_json::Value>)> {
    state.list()
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, 
                      Json(serde_json::json!({ "error": e }))))
}

pub async fn negotiate_capabilities(
    State(state): State<Arc<DegradationState>>,
    Json(body): Json<NegotiateRequest>,
) -> Result<Json<NegotiationResult>, (StatusCode, Json<serde_json::Value>)> {
    state.negotiate(&body.requested)
        .map(Json)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, 
                      Json(serde_json::json!({ "error": e }))))
}

pub async fn capability_fallback(...) -> Result<..., StatusCode> {
    let status = state.check(&name)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if status.level == DegradationLevel::Full { ... }
    // ...
}
```

### Change 6: Tests Completely Rewritten

**From:** In-memory mock tests with no database

**To:** Database-backed tests with helper function:
```rust
fn create_test_db() -> Arc<crate::db::Db> {
    let db = Arc::new(crate::db::Db::open(":memory:")
        .expect("failed to open in-memory db"));
    db.migrate().expect("migration failed");
    db
}
```

With 8 comprehensive tests including:
- Regression tests (existing behavior preserved)
- **`test_two_handles_share_same_store`** (tests shared store)
- **`test_degradation_state_persists_across_instances`** (tests persistence)

---

## File: backend/src/db.rs

### Change 1: Database Migration #12 Added

**Location:** In the `MIGRATIONS` const array, after migration #11

```rust
(
    "12",
    r"
    -- Graceful degradation: capability status registry
    -- Shared across all instances in a load-balanced deployment.
    CREATE TABLE IF NOT EXISTS capability_statuses (
        name                 TEXT PRIMARY KEY,
        level                TEXT NOT NULL,
        reason               TEXT,
        fallback_available   INTEGER NOT NULL DEFAULT 0,
        updated_at           TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_capability_statuses_updated_at
        ON capability_statuses(updated_at);
    ",
),
```

### Change 2: AppState Struct Updated

**From:**
```rust
pub struct AppState {
    pub db: Arc<Db>,
    pub vault_store: VaultStore,
    // ... other fields ...
    pub message_queue: Arc<crate::message_queue::MessageQueueState>,
}
```

**To:**
```rust
pub struct AppState {
    pub db: Arc<Db>,
    pub vault_store: VaultStore,
    // ... other fields ...
    pub message_queue: Arc<crate::message_queue::MessageQueueState>,
    /// Graceful degradation: shared capability status registry across instances.
    pub degradation_state: Arc<crate::degradation::DegradationState>,
}
```

### Change 3: FromRef Implementation Updated

**Added:**
```rust
impl axum::extract::FromRef<AppState> for Arc<crate::degradation::DegradationState> {
    fn from_ref(state: &AppState) -> Arc<crate::degradation::DegradationState> {
        Arc::clone(&state.degradation_state)
    }
}
```

### Change 4: Database Methods Added

**New impl block for capability status management:**

```rust
// ── Graceful degradation: capability status management ───────────────────────

impl Db {
    pub fn set_capability_status(
        &self,
        status: &crate::degradation::CapabilityStatus,
    ) -> Result<(), rusqlite::Error> {
        // INSERT ... ON CONFLICT ... DO UPDATE for atomic upsert
        // Serializes DegradationLevel as JSON
    }

    pub fn get_capability_status(
        &self,
        name: &str,
    ) -> Result<crate::degradation::CapabilityStatus, rusqlite::Error> {
        // SELECT from capability_statuses
        // Deserializes JSON back to DegradationLevel
        // Returns Full (default) if not found
    }

    pub fn list_capability_statuses(
        &self,
    ) -> Result<Vec<crate::degradation::CapabilityStatus>, rusqlite::Error> {
        // SELECT all from capability_statuses
        // Ordered by updated_at DESC
    }
}
```

---

## File: backend/src/main.rs

### Change 1: DegradationState Initialization

**Added in main() function:**

```rust
let degradation_state = Arc::new(DegradationState::new(Arc::clone(&db)));
```

### Change 2: AppState Construction Updated

**From:**
```rust
let state = AppState {
    db,
    vault_store,
    event_store,
    // ... other fields ...
    message_queue: Arc::new(MessageQueueState::new()?),
};
```

**To:**
```rust
let degradation_state = Arc::new(DegradationState::new(Arc::clone(&db)));

let state = AppState {
    db,
    vault_store,
    event_store,
    // ... other fields ...
    message_queue: Arc::new(MessageQueueState::new()?),
    degradation_state,
};
```

### Change 2: Router Routes Added

**Added in build_router() function:**

```rust
// ── Graceful degradation routes ─────────────────────────────────────────
.route("/admin/capabilities", post(set_capability).get(list_capabilities))
.route("/capabilities/negotiate", post(negotiate_capabilities))
.route("/capabilities/:name/fallback", get(capability_fallback))
```

**Placed after health routes, before legacy reminder routes.**

---

## File: docs/graceful-degradation.md

### Change 1: Added "Shared Degradation State" Section

**New section at top after intro:**

```markdown
## Shared Degradation State

Capability statuses are persisted in the SQL database and shared across all
instances in a load-balanced deployment. When an operator marks a capability
degraded via `POST /admin/capabilities`, all instances immediately observe
the change on subsequent reads. This ensures consistent client guidance during
live incidents, regardless of which instance handles the request.

**Example**: A downstream payment processing service becomes unavailable.
An operator calls `POST /admin/capabilities` on any instance to mark the
"payments" capability as degraded. All instances, load-balanced behind a
single endpoint, will now report the same degradation status to clients
checking `POST /capabilities/negotiate`. This allows clients to gracefully
fall back to reduced functionality instead of receiving contradictory
guidance depending on which instance they connect to.
```

### Change 2: Updated Feature Availability Checks Section

**Changed code example from:**
```rust
let status = state.degradation_state.check("search");
```

**To:**
```rust
let status = state.degradation_state.check("search")
    .expect("failed to check capability status");
```

### Change 3: Added Database Persistence Section

**New section at end:**

```markdown
## Database persistence & instance synchronization

Capability statuses are stored in the `capability_statuses` table in SQLite
(or your configured database). This ensures:

- **Persistence**: Status survives process restarts. A capability marked
  degraded before a restart remains degraded after.
- **Synchronization**: All instances in a load-balanced deployment read from
  the same table, ensuring consistent guidance during incidents.
- **Immediate visibility**: Changes propagate immediately — there is no
  in-process cache or eventual consistency delay.
```

---

## File Summary

| File | Changes | Impact |
|------|---------|--------|
| `backend/src/degradation.rs` | Complete refactor: HashMap → Database | Core fix |
| `backend/src/db.rs` | Add migration #12 + 3 methods | Data layer |
| `backend/src/main.rs` | Initialize DegradationState + register routes | Server setup |
| `docs/graceful-degradation.md` | Add shared state explanation + persistence notes | Documentation |

## Lines of Code Changed

- **degradation.rs**: ~50 lines (struct + methods refactored)
- **db.rs**: ~120 lines (migration + 3 methods added)
- **main.rs**: ~10 lines (initialization + routes)
- **graceful-degradation.md**: ~40 lines (documentation added)

**Total**: ~220 lines of meaningful changes

## Backward Compatibility

✅ **Public API unchanged**: HTTP request/response formats identical
✅ **Error handling improved**: Returns 500 on database errors instead of panicking
✅ **Single-instance behavior**: Functionally identical from caller's perspective
✅ **Regression tests**: All 6 regression tests pass

## Forward Compatibility

✅ **JSON serialization**: Allows new DegradationLevel variants without schema changes
✅ **Indexed queries**: Database performance scales with capability count
✅ **Migration system**: Easy to add new fields/tables in future versions
