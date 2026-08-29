# Formatting Fixes for PR #276

## Issue

`cargo fmt --check` was failing due to formatting inconsistencies in:
- `backend/src/degradation.rs` 
- `backend/src/db.rs`
- `backend/src/main.rs`

## Fixes Applied

### 1. degradation.rs - negotiate_capabilities Handler (line 214)

**Before**:
```rust
state
    .negotiate(&body.requested)
    .map(Json)
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })
```

**After**:
```rust
state.negotiate(&body.requested).map(Json).map_err(|e| {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e })),
    )
})
```

**Reason**: Line was unnecessarily split; formatter prefers inline short chain on single line.

### 2. degradation.rs - capability_fallback Handler (line 231)

**Before**:
```rust
let status = state.check(&name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
```

**After**:
```rust
let status = state
    .check(&name)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
```

**Reason**: Line exceeded rustfmt's line length limit; need to split before method calls.

### 3. degradation.rs - Test Assertion (line 412)

**Before**:
```rust
assert!(!list.iter().any(|s| s.name == "analytics" && s.level == DegradationLevel::Full));
```

**After**:
```rust
assert!(!list
    .iter()
    .any(|s| s.name == "analytics" && s.level == DegradationLevel::Full));
```

**Reason**: Line exceeded length limit; need to split macro arguments.

### 4. degradation.rs - Test set_status Call (line 401)

**Before**:
```rust
state
    .set_status("recommendations", DegradationLevel::Unavailable, None, false)
    .expect("set_status failed");
```

**After**:
```rust
state
    .set_status(
        "recommendations",
        DegradationLevel::Unavailable,
        None,
        false,
    )
    .expect("set_status failed");
```

**Reason**: Function arguments exceeded line length; split each argument onto own line.

### 5. db.rs - get_capability_status Query (line 2516)

**Before**:
```rust
let level: crate::degradation::DegradationLevel =
    serde_json::from_str(&level_str).unwrap_or(crate::degradation::DegradationLevel::Full);
```

**After**:
```rust
let level: crate::degradation::DegradationLevel = serde_json::from_str(&level_str)
    .unwrap_or(crate::degradation::DegradationLevel::Full);
```

**Reason**: Method call chains should align continuation lines; move method to new line.

### 6. db.rs - list_capability_statuses Query (line 2562)

Same fix as #5, applied to list operation.

### 7. main.rs - Route Registration (line 153)

**Before**:
```rust
.route("/admin/capabilities", post(set_capability).get(list_capabilities))
```

**After**:
```rust
.route(
    "/admin/capabilities",
    post(set_capability).get(list_capabilities),
)
```

**Reason**: Long argument lines split for readability; consistent formatting with other routes.

## Verification

All changes verified with:
```bash
cargo fmt --all -- --check
```

Output now clean with no formatting diffs.

## Commit

Commit: `e204b47` - "style: fix rustfmt formatting issues"

## Related

- Issue: #275 (graceful degradation)
- PR: #276 (implementation)
- Previous fix: `50bba3f` (resolved compilation errors)
