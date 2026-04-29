# PLAN — In-process BYOK Agent Runtime + MITM Observation

> **Status:** revised-v4, ready for implementation, not started.
> **Owner:** TBD.
> **Repository:** `shuv1337/warp` / `master` as audited.
> **Primary goal:** Make logged-out BYOK `/agent` work end-to-end without sending the agent loop to `app.warp.dev`, using the user's own provider key. OpenAI first, then Anthropic/Gemini/OpenRouter/custom endpoints.
> **Secondary goal:** Preserve a path to a standalone BYOK proxy later without rewriting the runtime.

---

## 0. Executive Summary

The existing Warp OSS client builds a `warp_multi_agent_api::Request`, POSTs it to `https://app.warp.dev/ai/multi-agent`, and consumes a streamed `ResponseEvent` sequence. The server is not only proxying LLM tokens; it performs agent orchestration, builds provider tool schemas, manages task/message deltas, and emits `ClientAction`s that the local client applies.

Anonymous BYOK requests currently fail because the server endpoint still requires account context. Tweaking headers or request shape is not a viable fix. The implementation should route BYOK agent requests to a new in-process runtime that returns the same `ResponseStream` type the UI already consumes.

### v4 changes from v3

The v3 plan was reviewed against the current codebase and revised:

- **`StreamInit` field semantics are now explicit.** `init_event.run_id` is parsed into `Conversation::task_id` in `conversation.rs:1517-1521`; it must be the string form of the minted root task id, not an opaque correlation id. `init_event.conversation_id` becomes `server_conversation_token`. Documented as a hard invariant with its own test.
- **Custom endpoint model-name plumbing is a Stage B prerequisite.** `CustomEndpointConfig` in `crates/ai/src/api_keys.rs` does not currently carry a model name; `B0` adds it before any routing work depends on it.
- **`is_byok_request` must cover custom endpoints.** The existing first-turn clearing branch (`api.rs:298`) misses custom-endpoint-only conversations. Fix is part of routing prep.
- **`web_context_retrieval_enabled` is hardcoded `true` in `impl.rs:74`.** The runtime does not branch on it; it ignores the field.
- **Public runtime API returns `Result<Stream, RuntimeError>`**, not a stream of `Result<Event, RuntimeError>`. Pre-stream errors and post-`StreamInit` errors are different surfaces.
- **`http_client` is the chosen HTTP transport** (it already wraps `reqwest` with `async-compat`). The async-executor invariant test from v3 is replaced with realistic mock-server tests.
- **Escape hatch is env var only for Stage B**, with an `AISettings` boolean deferred to Stage B+1. No `ContextFlag` involvement.
- **Capture denylist is diff-based and extension-scoped**, not a full-tree text grep, to avoid false positives on legitimate `Bearer`/`Authorization`/`session` mentions in `*.rs`.
- **`existing_suggestions` is explicitly dropped** by the local runtime.
- **Tool catalog intersects with `request.settings.supported_tools`** so the runtime never advertises a tool the request didn't authorize.
- **OpenAI Responses API is flagged as Stage B+1 follow-up** for reasoning-summary support; Stage B uses Chat Completions only.

### Carried-forward v3 invariants

- Capture artifact gitignore/denylist hygiene before any MITM work.
- First local `StreamInit` creates a conversation identity that the existing client stores as `server_conversation_token`, so follow-up/tool-result requests carry task history.
- Make runtime async execution compatibility explicit.
- Ensure new `crates/byok_agent` tests actually run in CI/default validation.
- Add a collision-proof provider tool-name mapping for static and dynamic MCP tools.
- Add non-UGC observability for local routing and provider/runtime failures.
- Web search/web context flags disabled/deferred for the in-process runtime.
- Tighten validation and rollback expectations.

---

## 1. Current Architecture Snapshot

### 1.1 Agent request path

Relevant code anchors:

- `app/src/ai/agent/api.rs::RequestParams::new`
- `app/src/ai/agent/api/impl.rs::generate_multi_agent_output`
- `app/src/server/server_api.rs::ServerApi::generate_multi_agent_output`
- `app/src/ai/blocklist/action_model.rs`
- `app/src/ai/blocklist/action_model/execute.rs`
- `app/src/ai/agent/conversation.rs::initialize_output_for_response_stream`

Current flow:

1. UI/controller builds `RequestParams`.
2. `generate_multi_agent_output` converts `RequestParams` into `warp_multi_agent_api::Request`.
3. `ServerApi::generate_multi_agent_output` sends protobuf to `/ai/multi-agent`.
4. The server responds as SSE; each event contains a URL-safe-base64 encoded `ResponseEvent` protobuf.
5. The client consumes `ResponseEvent`s and applies `ClientAction`s to the local conversation/task/action models.
6. Tool execution is already client-side: shell/read/grep/MCP/etc. are executed by `BlocklistAIActionExecutor`, then the result is sent back as an `AIAgentInput::ActionResult` on the next request.

### 1.2 Important existing behavior

`RequestParams::new` currently treats a brand-new BYOK conversation specially:

- It computes a local `is_byok_request` based on **proto-shaped** keys (anthropic/openai/google/open_router/aws).
- If BYOK and no existing `server_conversation_token`, it clears:
  - `conversation_token`
  - `forked_from_conversation_token`
  - `tasks`
  - `existing_suggestions`

That means a first local BYOK request may have an empty `task_context.tasks`; the user query exists only in `request.input`.

**Important gap:** `api_keys_for_request` returns `None` when only a custom endpoint is configured. Therefore the existing `is_byok_request` is `false` for custom-endpoint-only setups, and the first-turn clearing branch never fires for them. The runtime routing layer must compensate.

The in-process runtime must therefore support:

- First request, key-authed BYOK: empty task history, user input in `request.input`, no token.
- First request, custom-endpoint-only: tasks/token may be non-empty if the conversation previously used the server path or was imported. Defensive handling required.
- Follow-up request: existing task history in `task_context.tasks`, possibly including tool results.

### 1.3 Non-negotiable invariants

The local runtime must behave like the server from the UI's perspective:

- It returns `ResponseStream = Stream<Item = Result<ResponseEvent, Arc<AIApiError>>>`.
- It emits a `StreamInit` before normal content.
- On first turn, it emits a root `CreateTask` before `AddMessagesToTask` / `AppendToMessageContent`.
- The first local `StreamInit` populates **two** fields with specific semantics:
  - `StreamInit.conversation_id` → existing client stores this as `server_conversation_token` (`conversation.rs:1517`).
  - `StreamInit.run_id` → existing client parses this into `Conversation::task_id` (`conversation.rs:1519-1521`). It must therefore be the **string form of the minted root task id**, not an opaque correlation id.
  - `StreamInit.request_id` → existing client uses this as `ServerOutputId` for the in-flight exchanges.
- The root `CreateTask` must use the same task id that was emitted in `StreamInit.run_id`.
- Subsequent requests for that conversation must preserve task history.
- Tool calls are emitted as Warp `ToolCall` messages; the client executes tools and sends results back on the next request.
- Once a stream has started, provider/runtime failures should be represented as `StreamFinished` reasons, not raw transport errors, unless the failure happens before the stream can be initialized.
- Raw provider keys, bearer tokens, prompt text, tool args, file contents, and attachment data must never be logged.

---

## 2. Goals

1. **Stage A — Observation and capture hygiene**
   Build a safe MITM capture workflow for `app.warp.dev` traffic, with committed summaries and locally retained raw captures.

2. **Stage B — In-process runtime, OpenAI first**
   Add `crates/byok_agent`, intercept BYOK agent requests, produce Warp `ResponseEvent` streams locally, and support basic text + day-one tools.

3. **Stage C — Multi-provider support**
   Generalize provider adapters for Anthropic, Gemini, OpenRouter, and custom OpenAI-compatible endpoints.

4. **Stage D — Lift runtime to standalone proxy**
   Wrap the same runtime behind a local HTTP/SSE server for optional cross-device and server-like persistence.

---

## 3. Non-goals

These are not part of this plan:

- Drive sync.
- Session sharing / RTC live collaboration.
- Cloud Oz / ambient agent runs.
- Warp subscription model access without login.
- Server-side memory/rules sync.
- Team/workspace management.
- Cloud artifacts.
- Web search/web fetch through Warp's server implementation.
- AWS Bedrock provider in Stage B/C; defer because SigV4 and credential refresh are separate work.
- OpenAI Responses API and reasoning-summary streaming in Stage B; deferred to Stage B+1.

---

## 4. Stage A — MITM Observation and Capture Safety

### 4.1 Purpose

MITM captures are the reference spec for:

- Exact `ResponseEvent` ordering.
- `ClientAction` field masks.
- First-turn `CreateTask` shape and the `StreamInit` ↔ root-task-id relationship.
- Tool-call message shape.
- Error and finish semantics.
- Passive suggestion and non-agent request shapes.

### 4.2 Mandatory safety gate: capture ignore rules first

Before creating any MITM output, update `.gitignore` and add a denylist check.

#### .gitignore

Add root-level ignores:

```gitignore
# Local MITM captures; may contain credentials, prompts, file contents, cookies, and tokens.
captures/
*.flow
*.mitm
*.request.bin
*.response.bin
*.request.pb
*.response.pb
*.sse
*.decoded.pbtxt
*.decoded.json
```

If a committed redacted catalog is desired, use a separate path:

```gitignore
captures/*
!captures/README.md
!captures/INDEX.md
!captures/redacted/
!captures/redacted/**
```

#### Denylist guard

The guard is **diff-scoped, not full-tree**, to avoid false positives in legitimate code that mentions `Bearer`, `Authorization:`, `cookie`, or `session` (e.g. `server_api.rs`, request builders, doc comments).

Implementation:

- Checked-in script: `scripts/git/check-captures.sh`.
- Examines `git diff --cached --name-only --diff-filter=ACMR` (pre-commit) or `git diff --name-only "$BASE"...HEAD` (CI).
- For each changed file, reject if:
  1. The file matches a capture-artifact extension (`*.flow`, `*.mitm`, `*.request.bin`, `*.response.bin`, `*.request.pb`, `*.response.pb`, `*.sse`, `*.decoded.pbtxt`, `*.decoded.json`) and is not under `captures/redacted/`.
  2. The file is under `captures/` and not under `captures/redacted/` and not exactly `captures/README.md` or `captures/INDEX.md`.
  3. The file is under `captures/redacted/` and contains any of the secret tokens below in the staged diff (not the working tree, to limit blast radius):
     - `Authorization:`
     - `Bearer ` (followed by non-whitespace)
     - `sk-` (anchored to start of token)
     - `OPENAI_API_KEY=`, `ANTHROPIC_API_KEY=`, `GEMINI_API_KEY=`, `OPENROUTER_API_KEY=` with values
     - `Set-Cookie:`
- The guard does **not** scan `*.rs`, `*.md`, or `*.toml` for these tokens.

Validation tasks (see B0):

- Verify guard passes on a clean tree.
- Verify guard fails on a planted `captures/foo.flow`.
- Verify guard fails on a planted `captures/redacted/foo.json` containing `Authorization: Bearer xxx`.
- Verify guard does **not** fail on `app/src/server/server_api.rs` which legitimately mentions `bearer_auth`.

This must be the first Stage A implementation task.

### 4.3 MITM setup documentation

Create `docs/dev/mitm.md` with:

- Install `mitmproxy`.
- Start capture:
  - `mitmweb --listen-port 8080 --ssl-insecure`
  - or `mitmdump -w captures/$(date +%Y%m%d-%H%M%S).flow`
- Trust the mitmproxy CA:
  - Linux: document `SSL_CERT_FILE`.
  - macOS: document system keychain trust via `security add-trusted-cert`.
- Launch Warp OSS with proxy env:
  - `HTTPS_PROXY=http://127.0.0.1:8080`
  - `HTTP_PROXY=http://127.0.0.1:8080`
  - platform-specific cert env if needed.
- Verify `POST app.warp.dev/ai/multi-agent` appears decrypted.
- Document that raw captures must stay local unless explicitly scrubbed.

### 4.4 MITM addon

Create `scripts/mitm/warp_addon.py`.

Responsibilities:

- Capture only:
  - `app.warp.dev`
  - `*.app.warp.dev`
  - optionally `rtc.app.warp.dev` for future Stage D investigation.
- Write per-flow:
  - raw request body
  - raw response body
  - redacted metadata JSON
  - decoded proto or text view where possible
- Redact in committed summaries:
  - `Authorization`
  - cookies
  - provider keys
  - prompt text
  - file content
  - attachment data
  - tool args/results unless deliberately scrubbed
- For `/ai/multi-agent` SSE:
  - preserve raw SSE body with event boundaries.
  - decode each `data:` payload by:
    1. trimming quotes if present,
    2. URL-safe-base64 decoding,
    3. decoding `warp_multi_agent_api::ResponseEvent`.
- Use one of:
  - a small Rust decoder helper depending on the pinned `warp_multi_agent_api` (currently `rev = "78a78f21..."` in root `Cargo.toml:307`);
  - Python generated proto modules from the pinned `warp-proto-apis` checkout.
- Do not reference a non-existent local `crates/warp_multi_agent_api/proto` path.

### 4.5 Reference captures

Capture and summarize:

- Logged-in fresh `/agent`, text reply, no tools.
- Logged-in `/agent` using shell, read files, grep, glob.
- Logged-in `/agent` with apply diff.
- Logged-in `/agent` using MCP if configured.
- Logged-in continuation of an existing conversation.
- Logged-in passive prompt suggestion request.
- Logged-in model fetch / feature-model choices.
- Logged-out BYOK request returning current 400.
- Discovery-only: conversation list / server metadata endpoints. Do not make endpoint discovery an exit criterion.

For each capture, add to `captures/INDEX.md`:

- scenario name
- timestamp
- app commit/channel
- prompt summary
- model
- BYOK vs Warp model
- endpoint list
- relevant proto/event observations (especially `StreamInit.conversation_id` / `run_id` / `request_id` shape)
- redaction status

### 4.6 Stage A exit criteria

- `.gitignore` and pre-commit/CI denylist protect capture artifacts.
- Denylist passes on clean tree, fails on planted bad fixtures, does not flag legitimate `*.rs`/`*.md`.
- `docs/dev/mitm.md` works for a new contributor.
- At least six redacted capture summaries exist.
- A decoded `/ai/multi-agent` capture shows:
  - `StreamInit` with confirmed `conversation_id` / `run_id` / `request_id` field semantics.
  - first-turn task creation behavior, including the relationship between `StreamInit.run_id` and the root task's id.
  - `BeginTransaction` / `CommitTransaction`
  - `AddMessagesToTask`
  - `AppendToMessageContent`
  - exact field-mask paths.

---

## 5. Stage B — In-process BYOK Runtime, OpenAI First

### 5.1 New crate and public API

Create `crates/byok_agent`.

#### Public API

```rust
pub fn run_request(
    request: warp_multi_agent_api::Request,
    options: RuntimeOptions,
) -> Result<
    impl futures::Stream<
        Item = Result<warp_multi_agent_api::ResponseEvent, std::sync::Arc<AIApiError>>,
    > + Send + 'static,
    RuntimeError,
>;
```

Surface rules:

- **Pre-stream errors** (invalid options, no usable provider/model, malformed request that prevents minting `StreamInit`) → `Err(RuntimeError)`. The integration site converts these into `Arc<AIApiError>` and routes through the existing single-error rx pathway in `generate_multi_agent_output` (impl.rs:135-141).
- **Post-`StreamInit` errors** (provider transport, decode, mid-stream cancellation cleanup) → in-stream `Result::Err(Arc<AIApiError>)` and/or `StreamFinished` events. `RuntimeError` does **not** appear inside the stream.
- The `Item` type matches `app/src/ai/agent/api.rs::Event` (line 134) so the routing branch needs no per-item conversion.

#### Crate layout

```text
crates/byok_agent/
  Cargo.toml
  src/
    lib.rs
    error.rs
    ids.rs
    options.rs
    runtime/
      mod.rs
      conversation.rs
      system_prompt.rs
      tools.rs
      tool_names.rs
      streaming.rs
      telemetry.rs
    providers/
      mod.rs
      openai.rs
    convert/
      from_warp.rs
      to_warp.rs
```

#### Workspace wiring

- `[workspace.dependencies] byok_agent = { path = "crates/byok_agent" }`
- `app/Cargo.toml`: `byok_agent.workspace = true`
- Add `crates/byok_agent` to `default-members` in root `Cargo.toml:11-23`. This is the **only** way `cargo test` (default invocation) will pick up its tests; the existing `default-members` list does not include the crate by default.
- Alternatively, update CI to explicitly run `cargo test -p byok_agent` and `cargo check -p byok_agent` and document this as required-status.

Do not rely only on `cargo check -p warp`; that may compile the crate but skip its unit/snapshot tests.

### 5.2 Dependencies

Expected dependencies:

- `warp_multi_agent_api`
- `futures`
- `futures-util`
- `async-stream`
- `serde`
- `serde_json`
- `uuid`
- `thiserror`
- `anyhow`
- `log`
- `prost`
- `prost-reflect`
- `http_client` (workspace path; see §5.3)

Do not add `tokio` directly. Do not add a `#[tokio::main]` or create nested runtimes.

### 5.3 Runtime execution compatibility

**Decision: use `crates/http_client`.**

Rationale:

- `crates/http_client/Cargo.toml` already depends on `async-compat`, which means `reqwest` calls are polled under tokio regardless of the calling executor. This is the same compatibility guarantee `ServerApi::generate_multi_agent_output` already relies on.
- Reuses Warp's established request execution bridge (proxy, headers, `prevent_sleep`, eventsource adapter).
- Avoids a parallel raw-`reqwest` integration that would have to re-prove polling safety per-call site.

Acceptable fallback: raw `reqwest` directly is **not** allowed in Stage B. If a real shortcoming in `http_client` is discovered (for example, no streaming JSON-lines support for a non-OpenAI provider), file a follow-up instead of inlining `reqwest`.

#### Tests for execution compatibility

Replace the v3 `local_byok_stream_consumes_provider_stream_on_app_executor` test (which would not actually prove the executor invariant from `crates/byok_agent`) with concretely testable coverage:

- Inside `crates/byok_agent`: drive `run_request` against a `mockito` / `wiremock` SSE server from a tokio test harness. Verify SSE → `ProviderDelta` → `ResponseEvent` translation end-to-end.
- Inside `app/src/ai/agent/api/impl_tests.rs`: route a real synthetic `Request` through `generate_multi_agent_output`, intercept the routed branch, and consume the stream through the same `Box::pin(stream).take_until(cancellation_rx)` adapter the production code uses.

### 5.4 RuntimeOptions

Add `RuntimeOptions` with redacted debug behavior.

```rust
pub struct RuntimeOptions {
    pub disabled: bool,
    pub custom_endpoint: Option<CustomEndpointRuntimeConfig>,
    pub allow_web_search: bool,
    pub allow_web_context_retrieval: bool,
    pub telemetry_context: RuntimeTelemetryContext,
}

pub struct CustomEndpointRuntimeConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model_name: Option<String>,
    pub model_prefix: Option<String>,
}
```

Rules:

- `Debug` for `RuntimeOptions` must redact `api_key` and `base_url` (URLs may contain auth or tenant info).
- `custom_endpoint.api_key` must never be logged.
- `model_name` is required for real custom endpoint support; `custom/openai-compatible` is a placeholder, not a provider model.
- `allow_web_search` and `allow_web_context_retrieval` default to `false` and are not driven by request proto fields; see §6.11.

#### Source field plumbing for `model_name`

`crates/ai/src/api_keys.rs::CustomEndpointConfig` does not currently carry a `model_name` field. Stage B adds one as a prerequisite (B0):

```rust
// crates/ai/src/api_keys.rs
pub struct CustomEndpointConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub model_name: Option<String>,    // NEW
    pub model_prefix: Option<String>,
}
```

Required follow-on work:

- Add `OPENAI_COMPATIBLE_MODEL_NAME` env var fallback in `with_env_fallbacks` (alongside existing `OPENAI_COMPATIBLE_BASE_URL` / `OPENAI_COMPATIBLE_MODEL_PREFIX` handling).
- Add a setting/UI input for `model_name`.
- Migrate the secure-storage shape: deserialize legacy records (no `model_name`) cleanly via `#[serde(default)]`.

#### Construction ordering

`RuntimeOptions` must be constructed **before** `params` is consumed into the request (`impl.rs:53` already moves `params.api_keys`). Either:

- Attach to `RequestParams` and pull it out before any other `params.*` move; or
- Pass as a fourth argument to `generate_multi_agent_output` from the call site.

Attaching to `RequestParams` is preferred because all required state is available in `RequestParams::new` (which already has both the `ApiKeyManager` and the model id).

### 5.5 Routing predicate

Add:

```rust
fn should_route_locally(
    request: &warp_multi_agent_api::Request,
    options: &RuntimeOptions,
) -> bool
```

Return `true` when **all** of:

1. local runtime is not disabled (`!options.disabled`),
2. `request.input.r#type` is in the supported variant set:
   - `UserInputs`
   - `ResumeConversation`
   - `QueryWithCannedResponse`
   - (additional variants only as runtime support grows; keep the list explicit, not negative)
3. either:
   - proto `settings.api_keys` contains a non-empty provider key or AWS credentials (currently the basis of `is_byok_request`), or
   - `RuntimeOptions.custom_endpoint` is usable (base URL non-empty AND model name non-empty),
4. model can be resolved to a supported local provider/model,
5. provider credentials are present for the resolved provider,
6. request does not require unsupported server-only features.

Must explicitly return `false` for:

- no key and no usable custom endpoint,
- unknown model,
- passive suggestions (`GeneratePassiveSuggestions`),
- server-only prompt suggestions,
- unsupported ambient/cloud run inputs,
- escape hatch disabled runtime.

Custom endpoint routing is essential:

- `ApiKeyManager::keys().custom_endpoint` can be set while `api_keys_for_request()` returns `None` (api_keys.rs:196-217).
- Routing must therefore check `RuntimeOptions.custom_endpoint` directly, not derive from proto `api_keys`.

#### `is_byok_request` correction in `RequestParams::new`

`api.rs:250-256` derives `is_byok_request` solely from proto `api_keys`, which excludes custom-endpoint-only configurations. The first-turn clearing branch (`api.rs:298-311`) therefore does not fire for them, and a custom-endpoint-only conversation that was previously server-routed would carry stale `tasks` / `forked_from_conversation_token` / `existing_suggestions` into the local runtime.

Fix:

```rust
let is_byok_request = api_keys.as_ref().is_some_and(|keys| {
    !keys.anthropic.is_empty()
        || !keys.openai.is_empty()
        || !keys.google.is_empty()
        || !keys.open_router.is_empty()
        || keys.aws_credentials.is_some()
}) || api_key_manager.keys().custom_endpoint.is_some();
```

Add a defensive guard inside the runtime as well: if `should_route_locally` flips between server and local across turns of the same conversation, drop incoming tasks/forked-from data and treat it as a first turn. (Document but do not eagerly implement; tests should cover the steady-state custom-endpoint path first.)

#### Tests in `app/src/ai/agent/api/impl_tests.rs`

- OpenAI key + supported model routes locally.
- No key falls back.
- Unknown model falls back.
- Passive suggestions fall back.
- Escape hatch falls back.
- Custom-endpoint-only with non-empty `model_name` routes locally.
- Custom-endpoint with `model_name = None` returns `false` from `should_route_locally`.
- Custom-endpoint-only on a brand-new conversation produces empty `tasks` (because the corrected `is_byok_request` triggers the clearing branch).

### 5.6 Integration point

Modify `app/src/ai/agent/api/impl.rs::generate_multi_agent_output`.

Before any `params.*` field is consumed, build `runtime_options`. After the request is constructed and after sanitized shape logging, branch:

```rust
if should_route_locally(&request, &runtime_options) {
    let stream = match byok_agent::run_request(request, runtime_options) {
        Ok(s) => s,
        Err(e) => {
            // Pre-stream error → existing single-error rx pathway.
            let (tx, rx) = async_channel::unbounded();
            let _ = tx.send(Err(Arc::new(AIApiError::from(e)))).await;
            return Ok(Box::pin(rx));
        }
    };
    let stream = stream.take_until(cancellation_rx);
    return Ok(Box::pin(stream));
}
```

Notes:

- `runtime_options` must be built before lines that move out of `params` (currently `impl.rs:53` and onward).
- Preserve existing server behavior for non-routed requests.
- Passive suggestions must stay server-side.
- This routing means logged-in BYOK can also route locally; that is acceptable if tests cover both logged-in and logged-out.

### 5.7 Escape hatch

Stage B provides one escape hatch surface:

- Environment variable: `WARP_BYOK_IN_PROCESS_RUNTIME=0` (any of `0`, `false`, `off`, `no` disables; default enabled).
- Read once when constructing `RuntimeOptions::from_env_and_settings(...)`.

Stage B+1 follow-on:

- Add an `AISettings` boolean (persisted per user). Plumb through `RequestParams::new` so QA can flip per-conversation without a restart.

`ContextFlag` is **not** used. Per code review, `ContextFlag::set` is debug-only in non-dogfood contexts and `ContextFlag::FromStr` (`crates/warp_core/src/context_flag.rs:137`) omits several flags. Avoid coupling the rollback surface to it.

Rollback behavior:

- Escape hatch restores today's server path.
- For logged-out BYOK, that means restoring the known 400 behavior, **not** a working fallback. Document this clearly in user-facing release notes.

---

## 6. Stage B Runtime Details

### 6.1 Stateless request handling

The runtime is stateless per request.

Each request rebuilds provider messages from:

- `request.task_context.tasks`
- current `request.input`
- `request.settings`
- `request.metadata`
- `request.mcp_context`

No local runtime conversation cache is required for Stage B.

### 6.2 First-turn conversation/token invariant

For first-turn local BYOK:

- inbound `request.metadata.conversation_id` may be empty.
- inbound `task_context.tasks` may be empty.
- runtime must mint:
  - `conversation_id` (UUID; emitted as `StreamInit.conversation_id`).
  - `request_id` (UUID; emitted as `StreamInit.request_id`; becomes `ServerOutputId`).
  - root `task_id` (UUID; this is the **same value** the runtime emits as `StreamInit.run_id` after string-encoding).
- runtime must emit `StreamInit` so the existing client (`conversation.rs:1492-1521`) stores:
  - `conversation_id` → `server_conversation_token`
  - `run_id` → parsed into `Conversation::task_id`
  - `request_id` → `ServerOutputId` for in-flight exchanges
- runtime must emit `CreateTask` for the root task (with id matching `StreamInit.run_id`) **before** appending messages.
- after the first response completes, the next `RequestParams::new` for that conversation must see `conversation.server_conversation_token.is_some()`.
- therefore the next request must include non-empty `tasks` and a non-empty `metadata.conversation_id`.

Add focused validation:

```text
first_local_byok_stream_init_populates_conversation_token
first_local_byok_stream_init_run_id_parses_to_root_task_id
first_local_byok_stream_init_request_id_becomes_server_output_id
first_local_byok_followup_request_preserves_tasks
tool_result_followup_has_existing_tasks_and_action_result
```

If the existing client somehow fails to store the local `StreamInit.conversation_id` into `server_conversation_token`, fix that path before adding provider support.

### 6.3 Conversation rebuild mapping

Handle all `Message.message` variants explicitly.

Map to provider messages:

- `user_query` → `user`
- `agent_output` → `assistant`
- `tool_call` → `assistant` with `tool_calls`
- `tool_call_result` → `tool`
- `system_query` → `system` or fold into system prompt
- `agent_reasoning` → provider-specific reasoning only if supported (Stage B+1)
- `summarization` → condensed context
- `model_used` → drop
- `web_search` → drop / unsupported
- `web_fetch` → drop / unsupported
- `update_todos` → drop
- `server_event` → drop
- `code_review` → drop
- `update_review_comments` → drop
- `debug_output` → drop
- `artifact_event` → drop
- `invoke_skill` → drop
- `messages_received_from_agents` → drop
- `events_from_agents` → drop
- `passive_suggestion_result` → drop

For every dropped variant, emit a safe debug log with only the variant name.

#### Drop request-level `existing_suggestions`

`request.existing_suggestions` is server-orchestrator state. The local runtime does not honor it. The runtime must:

- Read but not consume `request.existing_suggestions`.
- Not surface it to the provider.
- Not echo it back in any emitted `ResponseEvent`.

Add a fixture test that asserts a request with non-empty `existing_suggestions` produces the same provider request as the same request with `None`.

### 6.4 Local system prompt

Create `runtime/system_prompt.rs`.

Requirements:

- Versioned constant (e.g. `SYSTEM_PROMPT_V1: &str` plus a `pub const SYSTEM_PROMPT_VERSION: &str = "byok-v1";`).
- Snapshot-tested via `insta` or equivalent.
- Describes:
  - Warp agent behavior.
  - Tool use protocol.
  - Approval/safety constraints.
  - File-edit expectations.
  - Shell command caution.
  - MCP tool behavior.
  - No assumption of Warp server memory/web search.
- Does not include user-specific data.
- Does not include provider keys.

### 6.5 Tool catalog

Implement a `ToolCatalog` that builds a provider tool list from the **intersection** of:

- runtime implementation support (the static set the runtime knows how to execute / map back).
- `request.settings.supported_tools` (what the request authorized).
- `request.settings.supported_cli_agent_tools`.
- `request.mcp_context` (dynamic MCP tools).

The runtime must **not** advertise any tool name to the provider that is not present in `request.settings.supported_tools` for static tools, even if the runtime would technically support it. This keeps the request and response surfaces aligned with what `get_supported_tools` / `get_supported_cli_agent_tools` (impl.rs:223-322) authorized at request time.

Day-one runtime-supported static tools (subject to intersection):

- `RunShellCommand`
- `ReadFiles`
- `Grep`
- `FileGlob`
- `FileGlobV2`
- `ApplyFileDiffs`
- `ReadShellCommandOutput`
- `SearchCodebase`
- `AskUserQuestion`
- `CallMcpTool`
- `ReadMcpResource`

Deferred / not advertised even if requested:

- `UseComputer`
- `RequestComputerUse`
- `StartAgent`
- `StartAgentV2`
- `SendMessageToAgent`
- `Subagent`
- `OpenCodeReview`
- `InsertReviewComments`
- `FetchConversation`
- `InitProject`
- `ReadDocuments`
- `EditDocuments`
- `CreateDocuments`
- `ReadSkill`
- `SuggestNewConversation`
- `SuggestPrompt`
- `UploadFileArtifact`
- `WriteToLongRunningShellCommand`
- `TransferShellCommandControlToUser`

### 6.6 Tool name mapping and MCP collision handling

Add `runtime/tool_names.rs`.

Provider function names need a reversible mapping back to Warp actions.

Use a per-request map:

```rust
pub struct ToolNameMap {
    provider_to_warp: HashMap<String, ToolBinding>,
    warp_to_provider: HashMap<ToolBindingKey, String>,
}

pub enum ToolBinding {
    Static { tool_type: warp_multi_agent_api::ToolType },
    McpTool {
        server_id: String,
        server_name: String,
        tool_name: String,
    },
    McpResource {
        server_id: String,
        resource_uri: String,
    },
}
```

Naming rules:

- Static Warp tools use reserved names:
  - `warp__run_shell_command`
  - `warp__read_files`
  - `warp__grep`
  - `warp__file_glob`
  - `warp__apply_file_diffs`
  - etc.
- MCP tools use names like:
  - `mcp__<server_slug>__<tool_slug>__<short_hash>`
- Names must match provider constraints:
  - lowercase-ish ASCII where required,
  - max length handled via truncation + hash,
  - no spaces,
  - deterministic for the request.
- Detect collisions and resolve with hashes.
- Never let MCP define a name beginning with reserved `warp__`.
- Reverse lookup is mandatory before emitting a Warp `ToolCall`.

Tests:

- Duplicate MCP tool names on two servers.
- MCP tool named `grep`.
- Very long MCP tool names.
- Invalid characters.
- Provider returns unknown tool.
- Provider returns malformed JSON args.
- Provider returns a `warp__*` name that is not in the runtime's static set.

### 6.7 ClientAction protocol

The runtime must account for all variants and emit only supported ones.

Emit in Stage B:

- `CreateTask` — first turn root task (id matches `StreamInit.run_id`).
- `AddMessagesToTask` — assistant message and completed tool-call messages.
- `AppendToMessageContent` — streaming assistant text.
- `BeginTransaction`.
- `CommitTransaction`.
- `RollbackTransaction` — if an open transaction fails.

Do not emit in Stage B, but explicitly document/skips:

- `UpdateTaskMessage`
- `Suggestions`
- `UpdateTaskSummary`
- `UpdateTaskDescription`
- `StartNewConversation`
- `UpdateTaskServerData`
- `MoveMessagesToNewTask`

Tests should assert unsupported variants are not accidentally emitted, and that `RollbackTransaction` is **not** emitted when no transaction has been opened (e.g. failure during `StreamInit` minting).

### 6.8 StreamFinished mapping

Map provider/runtime outcomes:

- normal stop → `Done`
- provider `finish_reason = length` → `ReachedMaxTokenLimit`
- 401/invalid key → `InvalidApiKey`
- 429 → `QuotaLimit`
- context limit → `ContextWindowExceeded`
- 5xx/network/timeout → `LlmUnavailable`
- unknown provider schema/runtime bug → `InternalError`
- unknown finish reason → `Other`

Once a transaction has begun:

- emit `RollbackTransaction` first if needed.
- then emit `StreamFinished`.

Once `StreamInit` has been emitted but no transaction is open:

- emit only `StreamFinished` (no `RollbackTransaction`).

### 6.9 Streaming state machine

Provider deltas normalize to:

```rust
pub enum ProviderDelta {
    AssistantText(String),
    ToolCallStart { index: usize, id: Option<String>, name: Option<String> },
    ToolCallArgs { index: usize, args_fragment: String },
    ToolCallEnd { index: usize },
    Finish(ProviderFinish),
}
```

Translator behavior:

- Emit `StreamInit` with minted ids.
- If first turn, emit `CreateTask` with the same id used as `StreamInit.run_id`.
- Emit `BeginTransaction`.
- Emit `AddMessagesToTask` with empty assistant output before streaming text.
- For each text delta, emit `AppendToMessageContent` with exact field mask confirmed by Stage A.
- Buffer streamed tool call arguments until valid JSON is complete.
- At tool-call finish, emit one or more `ToolCall` messages in one transaction.
- Support multiple parallel tool calls in the same turn.
- Emit `CommitTransaction`.
- Emit `StreamFinished`.

Parallel tool-call validation:

- If provider returns multiple tool calls in one finish cycle, emit all `ToolCall` messages in the same committed batch.
- The client can execute sequentially or in parallel; runtime should not care.
- Next request should carry all tool results.

### 6.10 OpenAI adapter

Default endpoint:

```text
POST https://api.openai.com/v1/chat/completions
stream: true
```

**Stage B uses Chat Completions only** for compatibility with:

- OpenAI
- OpenRouter
- custom OpenAI-compatible servers

Provider config:

- API key from `request.settings.api_keys.openai`
- model from `model_config.base`
- tools from `ToolCatalog`
- `tool_choice = auto`
- `stream = true`
- do not send unsupported options to custom/OpenRouter endpoints unless capability-gated

#### Reasoning summaries are deferred

OpenAI's Chat Completions does not expose reasoning summaries for `o1` / `o3` / `gpt-5` reasoning families. The current request shape sets `supports_reasoning_message: true` (impl.rs:84). Stage B implications:

- Do not advertise reasoning to the model or attempt to emit `agent_reasoning` messages in Stage B.
- Strip reasoning continuation hints from Chat Completions responses.

Stage B+1 follow-up:

- Add an OpenAI Responses API adapter behind a provider capability flag, gated on model id (`gpt-5*`, `o1*`, `o3*`).
- Map Responses API streaming events to `agent_reasoning` Warp messages where supported.

### 6.11 Web search and web context

Current request settings include:

- `web_search_enabled` — driven by `BlocklistAIPermissions::get_web_search_enabled` and may be `true`.
- `web_context_retrieval_enabled` — **hardcoded to `true` in `impl.rs:74`** at the request builder; not a setting and not request-driven in any meaningful way.

The local runtime must not silently pretend Warp server web search exists.

Stage B behavior:

- The runtime ignores both `web_search_enabled` and `web_context_retrieval_enabled` from the proto. They are not signals of runtime capability.
- Do not advertise web search/fetch tools.
- Add a safe log when `web_search_enabled = true` and the runtime routes locally, indicating that web search is unavailable.
- Consider adding a user-visible model/system-prompt instruction: web search is unavailable in local BYOK runtime.
- Future work can add Brave/Tavily/local search as runtime tools.

### 6.12 Observability

Add non-UGC logs/telemetry.

Safe fields:

- route: local vs server
- provider ID
- model ID
- custom endpoint present yes/no, never URL if user-provided unless sanitized
- input variant
- supported tool count
- emitted tool-call count
- first-turn vs follow-up
- stream initialized yes/no
- time to first token bucket
- total stream duration bucket
- finish reason
- error category
- cancellation yes/no
- escape hatch enabled/disabled
- web-search-asked-but-unavailable yes/no

Never log:

- API keys
- bearer tokens
- prompt text
- tool args
- tool results
- file contents
- raw file paths unless already considered safe by existing conventions
- attachment data

Add tests for redacted `Debug` output and logging helpers.

### 6.13 Cancellation

Keep existing cancellation plumbing:

- `generate_multi_agent_output` applies `.take_until(cancellation_rx)`.
- Dropping the runtime stream should drop the provider HTTP stream.

Validation:

- Long streaming completion.
- Click Stop.
- Provider stream is dropped within about one second.
- No further `AppendToMessageContent` after cancellation.
- No panic on dropped stream.
- Cancellation **before** any `BeginTransaction` does not emit `RollbackTransaction`.
- Cancellation **after** `BeginTransaction` and before `CommitTransaction` emits exactly one `RollbackTransaction` followed by exactly one `StreamFinished`.

### 6.14 Error handling

Pre-stream setup errors:

- invalid local options
- impossible provider selection
- malformed request before `StreamInit`

These return `Err(RuntimeError)` from `run_request` and are converted at the integration site to `Arc<AIApiError>` via the existing single-error rx pathway.

Post-init errors:

- emit `RollbackTransaction` if required.
- emit `StreamFinished` with mapped reason.
- do not expose raw provider error bodies if they may contain prompts or provider diagnostics with sensitive data.

---

## 7. Stage B Implementation Tasks

### B0. Safety and routing prerequisites

- [ ] Add capture artifact `.gitignore` entries.
- [ ] Add `scripts/git/check-captures.sh` (diff-scoped, extension-anchored).
- [ ] Wire pre-commit and CI to invoke the guard against the staged diff / PR diff only.
- [ ] Validate the guard: clean tree passes; planted `*.flow` fails; planted `Authorization:` in `captures/redacted/*.json` fails; legitimate `bearer_auth` in `*.rs` is **not** flagged.
- [ ] Add `model_name: Option<String>` to `crates/ai/src/api_keys.rs::CustomEndpointConfig` with `#[serde(default)]` for legacy storage.
- [ ] Add `OPENAI_COMPATIBLE_MODEL_NAME` env-var fallback in `with_env_fallbacks`.
- [ ] Add settings/UI input for `model_name`.
- [ ] Correct `is_byok_request` in `api.rs:250` to also fire when `api_key_manager.keys().custom_endpoint.is_some()`.
- [ ] Add `RuntimeOptions` and `RuntimeTelemetryContext` types.
- [ ] Read escape hatch env var `WARP_BYOK_IN_PROCESS_RUNTIME`.
- [ ] Add `should_route_locally` tests, including custom endpoint-only and `model_name = None`.

### B1. Crate skeleton

- [ ] Create `crates/byok_agent`.
- [ ] Add to `default-members` in root `Cargo.toml` (or update CI to run `cargo test -p byok_agent` as required-status).
- [ ] Add workspace/app dependencies. No direct `tokio` or `reqwest`; route through `http_client`.
- [ ] Add basic public API `run_request(...) -> Result<Stream, RuntimeError>` and `RuntimeError`.
- [ ] Add a compile-only smoke test.

### B2. Conversation reconstruction

- [ ] Implement `Conversation::from_request`.
- [ ] Support empty first-turn tasks + `request.input`.
- [ ] Support follow-up tasks + action results.
- [ ] Drop `request.existing_suggestions` and add a fixture test.
- [ ] Explicitly handle/drop every message variant.
- [ ] Add first-turn fixture tests.
- [ ] Add follow-up tool-result fixture tests.
- [ ] Add Stage A capture-based fixture tests.

### B3. System prompt

- [ ] Add versioned local system prompt (`SYSTEM_PROMPT_VERSION`).
- [ ] Snapshot test it.
- [ ] Include no server-only capabilities unless runtime implements them.

### B4. Tool catalog and names

- [ ] Implement static `ToolCatalog` as the intersection of runtime-supported, `request.settings.supported_tools`, and `request.settings.supported_cli_agent_tools`.
- [ ] Implement `ToolNameMap`.
- [ ] Add MCP dynamic tool parsing.
- [ ] Add collision tests (including provider returning a `warp__*` name not in static set).
- [ ] Add unknown-tool/malformed-args tests.
- [ ] Add proto structural equality tests for generated Warp `ToolCall` messages.

### B5. Translator to Warp events

- [ ] Mint `conversation_id`, `request_id`, `task_id`. Emit `StreamInit` with `run_id == task_id.to_string()`.
- [ ] Emit first-turn `CreateTask` using the same task id.
- [ ] Emit transactions and assistant message creation.
- [ ] Emit streaming `AppendToMessageContent`.
- [ ] Emit buffered tool calls.
- [ ] Emit commit/rollback.
- [ ] Emit `StreamFinished`.
- [ ] Confirm exact field masks using Stage A captures.
- [ ] Test with multiple parallel tool calls.
- [ ] Test that no `RollbackTransaction` is emitted when no transaction is open.

### B6. OpenAI provider (Chat Completions only in Stage B)

- [ ] Implement Chat Completions streaming via `http_client`.
- [ ] Implement error classification.
- [ ] Implement provider delta parser.
- [ ] Add mock streaming tests (`wiremock`/`mockito`) driven from a tokio harness.
- [ ] Add gated real `OPENAI_API_KEY` integration test.
- [ ] Defer Responses API and reasoning summaries to Stage B+1.

### B7. App integration

- [ ] Build `RuntimeOptions` from `RequestParams` / `ApiKeyManager` **before** any field of `params` is moved (currently `impl.rs:53` consumes `params.api_keys`).
- [ ] Add routing branch in `generate_multi_agent_output`. Pre-stream `RuntimeError` → existing single-error rx pathway. Post-init events flow through `take_until(cancellation_rx)`.
- [ ] Preserve server path for non-routed requests.
- [ ] Skip passive suggestions.
- [ ] Map `RuntimeError` → `AIApiError` at the integration site (not inside the stream).

### B8. Conversation-token continuity validation

- [ ] Test first local `StreamInit.conversation_id` populates `server_conversation_token` in the existing client.
- [ ] Test first local `StreamInit.run_id` parses into `Conversation::task_id`.
- [ ] Test first local `StreamInit.request_id` becomes `ServerOutputId` for in-flight exchanges.
- [ ] Test second request has `conversation_token.is_some()`.
- [ ] Test second request has non-empty `tasks`.
- [ ] Test tool-result follow-up uses existing root task and does not hit `TaskNotInitialized`.

### B9. GUI smoke tests

- [ ] Logged-out OpenAI BYOK text-only `/agent`.
- [ ] Streaming renders progressively.
- [ ] Finish state is clean.
- [ ] No request to `/ai/multi-agent` in MITM capture.

### B10. Tool E2E tests

- [ ] Shell command: ask agent to run `ls`, approve, feed result back, get follow-up.
- [ ] Read file.
- [ ] Grep.
- [ ] Glob.
- [ ] Apply diff.
- [ ] Multiple tool calls in one turn.
- [ ] MCP call when MCP configured.

### B11. Cancellation and errors

- [ ] Stop during long stream.
- [ ] Bad API key → `InvalidApiKey`.
- [ ] Quota/429 → `QuotaLimit`.
- [ ] Context length → `ContextWindowExceeded`.
- [ ] Network down → `LlmUnavailable`.
- [ ] Max tokens → `ReachedMaxTokenLimit`.
- [ ] Cancel before `BeginTransaction` → no `RollbackTransaction`.
- [ ] Cancel between `BeginTransaction` and `CommitTransaction` → exactly one `RollbackTransaction` then one `StreamFinished`.

### B12. Observability

- [ ] Add route decision log/telemetry.
- [ ] Add provider finish/error classification log/telemetry.
- [ ] Add `web-search-asked-but-unavailable` field.
- [ ] Add redaction tests (including `Debug` for `RuntimeOptions`).
- [ ] Add capture that proves no `/ai/multi-agent` traffic for routed BYOK.

### B13. Documentation

- [ ] Update `HANDOFF.md` or `AGENTS.md`.
- [ ] Document escape hatch (env var only in Stage B; settings deferred).
- [ ] Document local-runtime limitations (no web search, no reasoning summaries, no Bedrock, etc.).
- [ ] Document provider setup (including `OPENAI_COMPATIBLE_MODEL_NAME`).
- [ ] Document unsupported server-only features.

### Stage B exit criteria

- Logged-out BYOK `/agent` works with OpenAI.
- First-turn and follow-up conversations preserve task history.
- `StreamInit.run_id` semantics verified by test against `Conversation::task_id`.
- Shell/read/grep/glob/apply-diff/MCP tool loops work.
- Custom endpoint-only config (with `model_name` set) routes locally, even when proto `api_keys` is `None`.
- Custom endpoint-only with no `model_name` falls back cleanly via routing.
- Passive suggestions still use the existing server path.
- MITM shows zero `/ai/multi-agent` traffic for routed BYOK.
- Escape hatch restores old server path.
- `cargo test -p byok_agent` and relevant app tests pass.
- `cargo test` (default invocation) picks up `byok_agent` tests via `default-members` (or CI runs them as required-status).
- No capture artifacts or secrets are committed.

---

## 8. Stage C — Multi-provider Support

### 8.1 Provider trait

Add provider abstraction:

```rust
pub trait LLMProvider {
    fn stream_chat(
        &self,
        conv: Conversation,
        tools: ToolCatalog,
        opts: CallOptions,
    ) -> BoxStream<'static, Result<ProviderDelta, ProviderError>>;
}
```

Provider selection:

- `gpt-*` → OpenAI.
- `claude-*` → Anthropic.
- `gemini-*` → Google.
- `openrouter/*` → OpenRouter.
- `custom/*` → OpenAI-compatible custom endpoint.
- `aws-bedrock/*` → deferred.

Missing key/config should become `InvalidApiKey` or pre-stream setup error depending on whether stream has started.

### 8.2 Anthropic

- Use Messages API streaming.
- Map `tool_use` content blocks to Warp `ToolCall`.
- Map tool results back.
- Support extended thinking only when request/settings support reasoning messages (gated; matches Stage B+1 reasoning work).
- Error mapping same as Stage B.

### 8.3 Gemini

- Use Gemini streaming API.
- Map `functionCall` to Warp `ToolCall`.
- Map function responses/tool results.
- Handle Gemini safety/blocked responses as a finish/error reason.

### 8.4 OpenRouter

- Use OpenAI-compatible Chat Completions format.
- Base URL: `https://openrouter.ai/api/v1`.
- Use OpenRouter key.
- Strip/translate `openrouter/` model IDs as needed.
- Add OpenRouter-specific headers:
  - `HTTP-Referer`
  - `X-Title`
- Capability-gate unsupported options.

### 8.5 Custom OpenAI-compatible endpoint

- Use `RuntimeOptions.custom_endpoint`.
- Require actual `model_name` (added in B0).
- Do not assume `custom/openai-compatible` is a valid provider model.
- Support:
  - base URL
  - optional API key
  - model name (required for routing)
  - optional model prefix
- Add tests against a mock OpenAI-compatible endpoint.
- Optional real test against local Ollama/vLLM/LM Studio should be developer-gated.

### 8.6 Stage C tasks

- [ ] Implement provider trait.
- [ ] Refactor OpenAI adapter behind trait.
- [ ] Add Anthropic adapter.
- [ ] Add Gemini adapter.
- [ ] Add OpenRouter adapter.
- [ ] Add custom endpoint adapter (consuming the `model_name` plumbed in B0).
- [ ] Add provider capability flags.
- [ ] Add per-provider mock streaming tests.
- [ ] Add gated real integration tests for each provider.
- [ ] Update provider setup docs.

### Stage B+1 (between B and C)

- [ ] OpenAI Responses API adapter, gated by model id.
- [ ] `agent_reasoning` Warp message emission.
- [ ] AISettings boolean for the in-process runtime escape hatch (replaces / supplements env var).

### Stage C exit criteria

- Text-only and shell-tool E2E pass for each provider with env-gated keys.
- Model picker/provider selection routes correctly.
- Custom endpoint works with an explicit actual model name.
- Provider-specific unsupported options are not sent blindly.

---

## 9. Stage D — Standalone BYOK Proxy

Stage D is future work. The in-process runtime should be designed so this is extraction, not rewrite.

### 9.1 Proxy crate

Create `crates/byok_proxy`.

Endpoints:

- `POST /ai/multi-agent`
  - protobuf request body
  - SSE response
  - same event encoding as `app.warp.dev`
- health check
- optional model/capability endpoint
- optional capture/diagnostics endpoint with redaction

Persistence:

- SQLite conversations keyed by `conversation_id`.
- Store task history.
- Store created/updated timestamps.
- Store provider/model metadata.
- Do not store provider keys by default.

Auth:

- optional static bearer token in `proxy.toml`.

### 9.2 Client routing

Add a `byok_proxy_url` config.

Rules:

- When set, route agent requests to proxy.
- **Do not** confuse `byok_proxy_url` with `ChannelState::server_root_url()` (used in `server_api.rs:1124`). The proxy URL is an agent-runtime-only override; the channel root continues to govern auth/sync/non-agent endpoints.
- Do not broadly enable arbitrary `--server-root-url` override in OSS.
- Scope any override to agent runtime/proxy only.

### 9.3 Multi-device

- Two Warp clients point at same proxy.
- Conversation history syncs through proxy persistence.
- Tool execution remains local to whichever client receives the tool call unless a future remote execution protocol is added.

### Stage D tasks

- [ ] Create `byok_proxy`.
- [ ] Implement proto-in/SSE-out wrapper around `byok_agent::run_request`.
- [ ] Add SQLite persistence.
- [ ] Add auth token.
- [ ] Add client config (`byok_proxy_url`, distinct from channel root).
- [ ] Add multi-device smoke test.

---

## 10. Validation Matrix

### Build/test commands

Required before merge:

```bash
cargo check -p byok_agent
cargo test -p byok_agent
cargo check -p ai
cargo check -p warp --bin warp-oss
cargo test -p warp ai::agent::api
```

If `crates/byok_agent` is added to `default-members`, also:

```bash
cargo test
```

Add exact test filters once tests exist.

### Manual QA

- Logged-out OpenAI BYOK text reply.
- Logged-out OpenAI BYOK shell tool.
- Logged-out OpenAI BYOK read/grep/glob.
- Logged-out OpenAI BYOK apply diff.
- Logged-out custom endpoint smoke test (with `model_name` set).
- MCP tool call if server configured.
- Stop/cancel mid-stream.
- Bad key.
- Network failure.
- Escape hatch path (`WARP_BYOK_IN_PROCESS_RUNTIME=0`).
- Passive suggestions still behave as before.

### MITM validation

For routed BYOK:

- zero `POST /ai/multi-agent`.
- provider request goes to OpenAI/custom endpoint.
- no provider key appears in Warp logs.
- no raw capture artifacts are tracked by git.

### Git hygiene validation

- `git status --ignored` shows captures ignored.
- `scripts/git/check-captures.sh` exits 0 on a clean tree.
- Guard fails when a dummy `*.flow` file is staged outside `captures/redacted/`.
- Guard fails when a dummy `Authorization: Bearer xxx` line is staged inside `captures/redacted/*.json`.
- Guard does **not** flag normal Rust code mentioning `bearer_auth` or `Authorization`.

---

## 11. Risks and Mitigations

### Wire-format drift

Risk: upstream proto changes.

Mitigation:

- pin `warp_multi_agent_api` (currently `rev = "78a78f21..."` in root `Cargo.toml`).
- compile/test against pinned revision.
- capture-based fixtures catch event ordering drift.
- avoid hardcoding field masks without tests.

### First-turn task/token mismatch

Risk: local first response does not update conversation token; follow-up loses tasks.

Mitigation:

- explicit B8 invariant tests against `Conversation::initialize_output_for_response_stream`.
- first-turn `CreateTask` with id matching `StreamInit.run_id`.
- verify next `RequestParams` includes token/tasks.

### `is_byok_request` blind spot for custom endpoints

Risk: custom-endpoint-only conversation carries stale server-orchestrator state into the local runtime.

Mitigation:

- B0 corrects `is_byok_request` to include `custom_endpoint.is_some()`.
- B5 translator defensively drops orchestrator state on first local turn.

### Capture secret leakage

Risk: raw MITM flows contain secrets.

Mitigation:

- Stage A0 gitignore.
- diff-scoped, extension-anchored guard (no full-tree text grep).
- redacted summaries only.

### Runtime async mismatch

Risk: `reqwest` polled outside tokio.

Mitigation:

- use `http_client` (already wraps `reqwest` with `async-compat`).
- no nested runtime hacks.
- tokio-harness mock-server tests.

### Tool schema mismatch

Risk: provider tool calls cannot be converted to Warp actions.

Mitigation:

- `ToolNameMap`.
- structural proto tests.
- intersection with `request.settings.supported_tools` so the runtime never invents tool authority.
- Stage A captures.

### MCP collisions

Risk: dynamic MCP tool names collide with static or other MCP names.

Mitigation:

- reserved prefixes.
- slug + hash.
- collision tests including provider returning a forged `warp__*` name.

### Provider-specific API quirks

Risk: OpenAI-compatible endpoints reject OpenAI-only fields.

Mitigation:

- capability flags.
- conservative default request body.
- custom endpoint tests.

### Web search gap

Risk: model thinks web search exists because request settings enable it.

Mitigation:

- runtime ignores `web_search_enabled` and the hardcoded `web_context_retrieval_enabled = true`.
- do not advertise web tools.
- system prompt says unavailable.
- safe log when disabled.

### Reasoning-summary gap

Risk: `gpt-5*`/`o1*`/`o3*` models perform worse without reasoning summaries.

Mitigation:

- Stage B uses Chat Completions only and does not advertise reasoning.
- Stage B+1 adds Responses API behind a capability flag.

### Quality gap vs Warp server prompt

Risk: local agent performs worse than server.

Mitigation:

- versioned system prompt.
- snapshot tests.
- capture-driven improvements.
- tool descriptions tuned from observed behavior.

### Conversation restore/cross-device gap

Risk: local runtime has no server persistence.

Mitigation:

- document Stage B limitation.
- keep runtime stateless so Stage D proxy can add persistence.
- ensure local client persistence continues to work after completed exchanges.

---

## 12. Approval Gate

Do not begin code changes until these plan items are accepted:

- Capture artifact protection is first, with a **diff-scoped** guard.
- `StreamInit.conversation_id` / `run_id` / `request_id` semantics are documented as hard invariants.
- `CustomEndpointConfig.model_name` is added before custom-endpoint routing depends on it.
- `is_byok_request` is corrected to cover custom endpoints.
- `web_context_retrieval_enabled` is treated as an ignored proto field, not a runtime signal.
- Public runtime API is `Result<Stream, RuntimeError>` (pre-stream) with in-stream `Result<Event, Arc<AIApiError>>` (post-init).
- Runtime async strategy is `http_client`, with realistic mock-server tests instead of an executor-invariant test.
- New crate tests are included in CI/default validation (default-members or required-status).
- Tool catalog intersects with `request.settings.supported_tools`.
- Tool name/MCP collision mapping is mandatory.
- Non-UGC telemetry/logging is included.
- Web search and reasoning summaries are explicitly deferred for Stage B.
- Escape hatch is env-var-only in Stage B; AISettings boolean deferred to B+1.
- `request.existing_suggestions` is dropped by the local runtime.

---

## 13. Tracking Checklist

This checklist mirrors the §7 task ids exactly (B0–B13) plus Stage A and later stages. Update as work proceeds.

### Stage A

- [x] Add capture ignores. (`.gitignore`)
- [x] Add `script/git/check-captures.sh` (diff-scoped, extension-anchored).
      [Repo uses `script/` (singular) per existing convention; the v4 plan text
      references `scripts/` but the implementation lives at `script/`.]
- [x] Wire pre-commit and CI to invoke the guard against staged diff / PR diff.
      Repo-managed hook at `script/git/hooks/pre-commit`, opt-in via
      `script/git/install-hooks.sh`. CI step added to the `general-lint` job
      in `.github/workflows/ci.yml`.
- [x] Validate guard against planted bad fixtures and legitimate `*.rs` mentions
      of `Bearer`/`Authorization`. Self-test at `script/git/check-captures.test.sh`
      covers all six cases from §4.2 plus `bearer_auth` in synthetic Rust.
- [x] Write MITM docs (`docs/dev/mitm.md`).
- [x] Build MITM addon (`script/mitm/warp_addon.py`).
- [ ] Capture redacted reference flows.   <!-- requires user at the keyboard -->
- [ ] Decode `/ai/multi-agent` event streams (especially `StreamInit` field
      semantics).                          <!-- requires real captures -->
- [ ] Commit redacted index/summaries only. <!-- requires real captures -->

### Stage B

- [ ] **B0** — safety + routing prereqs (gitignore, guard, `model_name` plumbing, `is_byok_request` fix, `RuntimeOptions`, escape hatch env, routing tests).
- [ ] **B1** — crate skeleton (incl. `default-members` or CI required-status).
- [ ] **B2** — conversation reconstruction (incl. dropping `existing_suggestions`).
- [ ] **B3** — versioned system prompt + snapshot.
- [ ] **B4** — tool catalog (intersected with `request.settings.supported_tools`) and `ToolNameMap`.
- [ ] **B5** — translator emitting `StreamInit` (with correct `run_id` semantics) → `CreateTask` → transactions → `StreamFinished`.
- [ ] **B6** — OpenAI Chat Completions adapter via `http_client`.
- [ ] **B7** — `generate_multi_agent_output` integration with correct `RuntimeOptions` ordering.
- [ ] **B8** — conversation-token continuity tests.
- [ ] **B9** — GUI smoke tests.
- [ ] **B10** — tool E2E.
- [ ] **B11** — cancellation and errors (incl. no-rollback-without-transaction).
- [ ] **B12** — observability and redaction.
- [ ] **B13** — docs.

### Stage B+1

- [ ] OpenAI Responses API adapter behind capability flag.
- [ ] `agent_reasoning` emission.
- [ ] AISettings boolean escape hatch.

### Stage C

- [ ] Add provider trait.
- [ ] Add Anthropic.
- [ ] Add Gemini.
- [ ] Add OpenRouter.
- [ ] Add custom endpoint adapter (consuming `model_name`).
- [ ] Add provider-specific tests.

### Stage D

- [ ] Add proxy.
- [ ] Add SQLite persistence.
- [ ] Add proxy auth.
- [ ] Add client proxy config (`byok_proxy_url`, distinct from channel root).
- [ ] Add multi-device test.
