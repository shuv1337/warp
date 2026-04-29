# Capture index

Registry of redacted MITM capture summaries. One row per scenario. Raw flows
must stay local; only summaries under `captures/redacted/` may be committed.

| Scenario | Date (UTC) | App commit / channel | Model | BYOK? | Endpoint(s) | Summary file | Notes |
|----------|-----------|----------------------|-------|-------|-------------|--------------|-------|

<!--
Add rows as captures are recorded and scrubbed. Required columns:
  - Scenario:        e.g. "Logged-in fresh /agent text reply (no tools)"
  - Date (UTC):      ISO 8601, e.g. 2026-04-29T18:00Z
  - App commit:      short SHA + channel (oss / local)
  - Model:           e.g. "auto", "claude-4.5-sonnet", "gpt-5.4"
  - BYOK?:           "warp model" / "BYOK <provider>"
  - Endpoint(s):     comma-separated paths under app.warp.dev
  - Summary file:    relative path under captures/redacted/
  - Notes:           one-liner — anything notable about ResponseEvent shape,
                     StreamInit ids, ClientAction field masks, error behavior

The Stage A exit criteria require at least six redacted capture summaries
across these scenarios:

  1. Logged-in fresh /agent, text reply, no tools.
  2. Logged-in /agent using shell, read files, grep, glob.
  3. Logged-in /agent with apply diff.
  4. Logged-in /agent using MCP if configured.
  5. Logged-in continuation of an existing conversation.
  6. Logged-in passive prompt suggestion request.
  7. Logged-in model fetch / feature-model choices.
  8. Logged-out BYOK request returning current 400.
  9. Discovery-only: conversation list / server metadata endpoints.

A decoded /ai/multi-agent capture must show:
  - StreamInit with confirmed conversation_id / run_id / request_id semantics
  - first-turn task creation, including the relationship between
    StreamInit.run_id and the root task's id
  - BeginTransaction / CommitTransaction
  - AddMessagesToTask
  - AppendToMessageContent
  - exact field-mask paths
-->
