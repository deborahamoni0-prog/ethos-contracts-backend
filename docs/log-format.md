# Structured Log Format & Analysis

`backend/src/log_analysis.rs` parses raw log lines into structured
entries so they can be searched and pattern-matched instead of grepped by
hand.

## Expected log line format

```
<RFC3339 timestamp> <LEVEL> <target>: <message> [key=value ...] [key="quoted value" ...]
```

Example:

```
2026-07-26T10:00:00Z INFO checkin_handler: vault released id=42 region="eu-west-1"
```

Every part is optional and parsed best-effort - a bare message with no
timestamp/level/target still parses, it just leaves those fields `null`
and treats the whole line as `message`.

Parsing extracts:

- `timestamp` - RFC3339, if the first token parses as one.
- `level` - one of `TRACE`/`DEBUG`/`INFO`/`WARN`/`ERROR` (case-insensitive).
- `target` - a token immediately after the level ending in `:`.
- `fields` - any `key=value` or `key="quoted value"` token, extracted into
  a map.
- `message` - whatever tokens are left.

## API

```
POST /logs/ingest
{"lines": ["2026-07-26T10:00:00Z ERROR svc: vault release failed id=2"]}
=> [{"raw": "...", "level": "ERROR", "fields": {"id": "2"}, "message": "vault release failed", ...}]

GET /logs/search?level=error&query=failed&limit=50
GET /logs/search?pattern=vault*failed*
```

- `level` - exact level match (case-insensitive).
- `query` - substring match against the message or raw line.
- `pattern` - glob-style match against the raw line, where `*` matches any
  run of characters (e.g. `vault*failed*`).
- `limit` - max results, newest first (default 100).

Filters combine with AND when more than one is supplied.
