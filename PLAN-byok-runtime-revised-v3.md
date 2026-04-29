# PLAN — In-process BYOK Agent Runtime + MITM Observation

> **Status:** revised-v3, ready for implementation planning review, not started.  
> **Owner:** TBD.  
> **Repository:** `shuv1337/warp` / `master` as audited.  
> **Primary goal:** Make logged-out BYOK `/agent` work end-to-end without sending the agent loop to `app.warp.dev`, using the user's own provider key. OpenAI first, then Anthropic/Gemini/OpenRouter/custom endpoints.  
> **Secondary goal:** Preserve a path to a standalone BYOK proxy later without rewriting the runtime.

---

## 0. Executive Summary

The existing Warp OSS client builds a `warp_multi_agent_api::Request`, POSTs it to `https://app.warp.dev/ai/multi-agent`, and consumes a streamed `ResponseEvent` sequence. The server is not only proxying LLM tokens; it performs agent orchestration, builds provider tool schemas, manages task/message deltas, and emits `ClientAction`s that the local client applies.

Anonymous BYOK requests currently fail because the server endpoint still requires account context. Tweaking headers or request shape is not a viable fix. The implementation should route BYOK agent requests to a new in-process runtime that returns the same `ResponseStream` type the UI already consumes.

This plan includes the required revisions from codebase review:

- Add capture artifact gitignore/pre-commit hygiene **before** any MITM capture task.
- Guarantee first local `StreamInit` creates a conversation identity that the existing client stores as `server_conversation_token`, so follow-up/tool-result requests carry task history.
- Route custom OpenAI-compatible endpoint requests locally even when proto `api_keys` is `None`.
- Make runtime async execution compatibility explicit: either use `http_client` or guarantee `reqwest` is only polled under Warp's tokio background executor, with a test.
- Ensure new `crates/byok_agent` tests actually run in CI/default validation.
- Add a collision-proof provider tool-name mapping for static and dynamic MCP tools.
- Add non-UGC observability for local routing and provider/runtime failures.
- Explicitly handle web-search/web-context flags as disabled/deferred for the in-process runtime.
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

Current flow:

1. UI/controller builds `RequestParams`.
2. `generate_multi_agent_output` converts `RequestParams` into `warp_multi_agent_api::Request`.
3. `ServerApi::generate_multi_agent_output` sends protobuf to `/ai/multi-agent`.
4. The server responds as SSE; each event contains a URL-safe-base64 encoded `ResponseEvent` protobuf.
5. The client consumes `ResponseEvent`s and applies `ClientAction`s to the local conversation/task/action models.
6. Tool execution is already client-side: shell/read/grep/MCP/etc. are executed by `BlocklistAIActionExecutor`, then the result is sent back as an `AIAgentInput::ActionResult` on the next request.

### 1.2 Important existing behavior

`RequestParams::new` currently treats a brand-new BYOK conversation specially:

- It computes a local `is_byok_request`.
- If BYOK and no existing `server_conversation_token`, it clears:
  - `conversation_token`
  - `forked_from_conversation_token`
  - `tasks`
  - `existing_suggestions`

That means a first local BYOK request may have an empty `task_context.tasks`; the user query exists only in `request.input`.

The in-process runtime must therefore support both cases:

- First request: empty task history, user input in `request.input`.
- Follow-up request: existing task history in `task_context.tasks`, possibly including tool results.

### 1.3 Non-negotiable invariants

The local runtime must behave like the server from the UI's perspective:

- It returns `ResponseStream = Stream<Item = Result<ResponseEvent, Arc<AIApiError>>>`.
- It emits a `StreamInit` before normal content.
- On first turn, it emits a root `CreateTask` before `AddMessagesToTask` / `AppendToMessageContent`.
- The first local `StreamInit` must provide the conversation ID in the field the existing client uses to set `server_conversation_token`.
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

---

## 4. Stage A — MITM Observation and Capture Safety

### 4.1 Purpose

MITM captures are the reference spec for:

- Exact `ResponseEvent` ordering.
- `ClientAction` field masks.
- First-turn `CreateTask` shape.
- Tool-call message shape.
- Error and finish semantics.
- Passive suggestion and non-agent request shapes.

### 4.2 Mandatory safety gate: capture ignore rules first

Before creating any MITM output, update `.gitignore` and add a denylist check.

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

Add a pre-commit or CI guard that fails if any of these appear outside an allowed redacted directory:

- `*.flow`
- `*.request.bin`
- `*.response.bin`
- `Authorization:`
- `Bearer `
- `sk-`
- `OPENAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `GEMINI_API_KEY`
- `OPENROUTER_API_KEY`
- `cookie`
- `session`

This must be the first Stage A implementation task. Raw captures are security-sensitive; no cowboy archaeology.

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
  - a small Rust decoder helper depending on the pinned `warp_multi_agent_api`;
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
- relevant proto/event observations
- redaction status

### 4.6 Stage A exit criteria

- `.gitignore` and pre-commit/CI denylist protect capture artifacts.
- `docs/dev/mitm.md` works for a new contributor.
- At least six redacted capture summaries exist.
- A decoded `/ai/multi-agent` capture shows:
  - `StreamInit`
  - first-turn task creation behavior
  - `BeginTransaction` / `CommitTransaction`
  - `AddMessagesToTask`
  - `AppendToMessageContent`
  - exact field-mask paths.

---

## 5. Stage B — In-process BYOK Runtime, OpenAI First

### 5.1 New crate

Create `crates/byok_agent`.

Public API:

```rust
pub fn run_request(
    request: warp_multi_agent_api::Request,
    options: RuntimeOptions,
) -> impl futures::Stream<Item = Result<warp_multi_agent_api::ResponseEvent, RuntimeError>>;
```

`RuntimeError` is only for setup/pre-stream failures. Once `StreamInit` has been emitted, provider/runtime failures should become `ResponseEvent::StreamFinished`.

Suggested layout:

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

Add to root workspace:

- `[workspace.dependencies] byok_agent = { path = "crates/byok_agent" }`
- `app/Cargo.toml`: `byok_agent.workspace = true`

Testing/CI requirement:

- Either add `crates/byok_agent` to `default-members`, or update CI to run:
  - `cargo test -p byok_agent`
  - `cargo check -p byok_agent`
  - `cargo check -p warp --bin warp-oss`

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
- HTTP option A:
  - use existing `http_client` wrapper if adding dependency is acceptable;
- HTTP option B:
  - `reqwest` with `stream`
  - explicitly test that all calls are polled on Warp's tokio background executor.

Do not add a `#[tokio::main]` or create nested runtimes.

### 5.3 Runtime execution compatibility

The plan must choose one of two paths before implementation:

#### Preferred path: use `http_client`

Pros:

- Reuses Warp's established request execution bridge.
- Preserves headers/proxy conventions.
- Avoids accidental `reqwest` polling outside tokio.

Cons:

- Adds dependency on an internal crate and may couple runtime to app conventions.

#### Acceptable path: raw `reqwest` with explicit invariant

Raw `reqwest` is acceptable only if:

- `byok_agent::run_request` is consumed via `ModelContext::spawn()` / Warp background executor.
- A native integration test proves the stream path is polled under tokio.
- No future foreground/smol-only caller is introduced without adding `async_compat`.

Add a test named like:

```text
local_byok_stream_consumes_provider_stream_on_app_executor
```

It should create a mocked streaming provider response and consume `run_request` through the same app stream adapter used in `generate_multi_agent_output`.

### 5.4 RuntimeOptions

Add `RuntimeOptions` with redacted debug behavior.

Example shape:

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

- `Debug` for `RuntimeOptions` must redact keys.
- `custom_endpoint.api_key` must never be logged.
- `model_name` is required for real custom endpoint support; `custom/openai-compatible` is a placeholder, not a provider model.
- `allow_web_search` and `allow_web_context_retrieval` should default to `false` for local runtime until a non-Warp backend exists.

`RuntimeOptions` must be constructed before `params` is consumed into the request. It can be:

- attached to `RequestParams`, or
- passed as a fourth argument to `generate_multi_agent_output`.

Attaching to `RequestParams` is cleaner if all required state is available in `RequestParams::new`.

### 5.5 Routing predicate

Add:

```rust
fn should_route_locally(
    request: &warp_multi_agent_api::Request,
    options: &RuntimeOptions,
) -> bool
```

Return `true` when:

- local runtime is not disabled,
- input is not `GeneratePassiveSuggestions`,
- request is an agent-style input supported by Stage B,
- either:
  - proto `settings.api_keys` contains a non-empty provider key or AWS credentials, or
  - `RuntimeOptions.custom_endpoint` is usable,
- model can be resolved to a supported local provider/model,
- provider credentials are present,
- request does not require unsupported server-only features.

Must explicitly return `false` for:

- no key and no custom endpoint,
- unknown model,
- passive suggestions,
- server-only prompt suggestions,
- unsupported ambient/cloud run inputs,
- escape hatch disabled runtime.

Custom endpoint routing is important:

- `ApiKeyManager::keys().custom_endpoint` can be set while `api_keys_for_request()` returns `None`.
- Therefore routing must not require proto `api_keys` when `RuntimeOptions.custom_endpoint` exists.

Add tests in `app/src/ai/agent/api/impl_tests.rs`:

- OpenAI key + supported model routes locally.
- No key falls back.
- Unknown model falls back.
- Passive suggestions fall back.
- Escape hatch falls back.
- Custom endpoint-only routes locally.
- Custom endpoint without usable model name fails/renders `InvalidApiKey` or setup error according to final design.

### 5.6 Integration point

Modify `app/src/ai/agent/api/impl.rs::generate_multi_agent_output`.

After the request is constructed and after sanitized shape logging, before:

```rust
server_api.generate_multi_agent_output(&request).await
```

branch to local runtime:

```rust
if should_route_locally(&request, &runtime_options) {
    let stream = byok_agent::run_request(request, runtime_options)
        .map(map_runtime_error)
        .take_until(cancellation_rx);

    return Ok(Box::pin(stream));
}
```

Notes:

- Build `runtime_options` before moving fields out of `params`.
- Preserve existing server behavior for non-routed requests.
- Passive suggestions must stay server-side.
- This routing means logged-in BYOK can also route locally; that is acceptable if tests cover both logged-in and logged-out.

### 5.7 Escape hatch

Add a local escape hatch:

- Environment variable: `WARP_BYOK_IN_PROCESS_RUNTIME=0`
- Optional setting later.

Default should be enabled.

Do not rely only on `ContextFlag`:

- `ContextFlag::set` is debug-only in non-dogfood contexts.
- `ContextFlag::FromStr` currently omits several flags.
- If a `ContextFlag` is added for deep-link/context behavior, update enum and `FromStr`.

Rollback behavior:

- Escape hatch restores today's server path.
- For logged-out BYOK, that likely means restoring the known 400 behavior, not a working fallback.
- Document this clearly.

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

This is a must-fix invariant from review.

For first-turn local BYOK:

- inbound `request.metadata.conversation_id` may be empty.
- inbound `task_context.tasks` may be empty.
- runtime must mint:
  - `conversation_id`
  - `request_id`
  - `run_id`
  - root `task_id`
- runtime must emit `StreamInit` so the existing client stores the minted `conversation_id` as `server_conversation_token`.
- runtime must emit `CreateTask` for the root task before appending messages.
- after the first response completes, the next `RequestParams::new` for that conversation must see `conversation.server_conversation_token.is_some()`.
- therefore the next request must include non-empty `tasks`.

Add focused validation:

```text
first_local_byok_stream_init_populates_conversation_token
first_local_byok_followup_request_preserves_tasks
tool_result_followup_has_existing_tasks_and_action_result
```

If the existing client does not store the local `StreamInit.conversation_id` into `server_conversation_token`, fix that path before adding provider support.

### 6.3 Conversation rebuild mapping

Handle all `Message.message` variants explicitly.

Map to provider messages:

- `user_query` → `user`
- `agent_output` → `assistant`
- `tool_call` → `assistant` with `tool_calls`
- `tool_call_result` → `tool`
- `system_query` → `system` or fold into system prompt
- `agent_reasoning` → provider-specific reasoning only if supported
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

### 6.4 Local system prompt

Create `runtime/system_prompt.rs`.

Requirements:

- Versioned constant.
- Snapshot-tested.
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

Implement a `ToolCatalog` that builds a provider tool list from:

- `request.settings.supported_tools`
- `request.settings.supported_cli_agent_tools`
- runtime implementation support
- `request.mcp_context`

Day-one supported static tools:

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

Deferred / not advertised:

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

### 6.7 ClientAction protocol

The runtime must account for all variants and emit only supported ones.

Emit in Stage B:

- `CreateTask` — first turn root task.
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

Tests should assert unsupported variants are not accidentally emitted.

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

- Emit `StreamInit`.
- If first turn, emit `CreateTask`.
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

Use Chat Completions first for compatibility with:

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

If adding Responses API later, keep it behind a provider capability flag.

### 6.11 Web search and web context

Current request settings may include:

- `web_search_enabled`
- `web_context_retrieval_enabled`

The local runtime must not silently pretend Warp server web search exists.

Stage B behavior:

- Do not advertise web search/fetch tools.
- Add a safe log when request asks for web search but local runtime lacks a backend.
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

### 6.14 Error handling

Pre-stream setup errors:

- invalid local options
- impossible provider selection
- malformed request before `StreamInit`

These may become `RuntimeError` and map to `Arc<AIApiError>`.

Post-init errors:

- emit `RollbackTransaction` if required.
- emit `StreamFinished` with mapped reason.
- do not expose raw provider error bodies if they may contain prompts or provider diagnostics with sensitive data.

---

## 7. Stage B Implementation Tasks

### B0. Safety and routing prerequisites

- [ ] Add capture artifact `.gitignore` entries.
- [ ] Add capture denylist pre-commit/CI guard.
- [ ] Add `RuntimeOptions`.
- [ ] Add escape hatch `WARP_BYOK_IN_PROCESS_RUNTIME=0`.
- [ ] Add `should_route_locally` tests, including custom endpoint-only.

### B1. Crate skeleton

- [ ] Create `crates/byok_agent`.
- [ ] Add workspace/app dependencies.
- [ ] Add basic public API and `RuntimeError`.
- [ ] Add CI/default-member coverage for crate tests.
- [ ] Add a compile-only smoke test.

### B2. Conversation reconstruction

- [ ] Implement `Conversation::from_request`.
- [ ] Support empty first-turn tasks + `request.input`.
- [ ] Support follow-up tasks + action results.
- [ ] Explicitly handle/drop every message variant.
- [ ] Add first-turn fixture tests.
- [ ] Add follow-up tool-result fixture tests.
- [ ] Add Stage A capture-based fixture tests.

### B3. System prompt

- [ ] Add versioned local system prompt.
- [ ] Snapshot test it.
- [ ] Include no server-only capabilities unless runtime implements them.

### B4. Tool catalog and names

- [ ] Implement static `ToolCatalog`.
- [ ] Implement `ToolNameMap`.
- [ ] Add MCP dynamic tool parsing.
- [ ] Add collision tests.
- [ ] Add unknown-tool/malformed-args tests.
- [ ] Add proto structural equality tests for generated Warp `ToolCall` messages.

### B5. Translator to Warp events

- [ ] Emit `StreamInit`.
- [ ] Emit first-turn `CreateTask`.
- [ ] Emit transactions and assistant message creation.
- [ ] Emit streaming `AppendToMessageContent`.
- [ ] Emit buffered tool calls.
- [ ] Emit commit/rollback.
- [ ] Emit `StreamFinished`.
- [ ] Confirm exact field masks using Stage A captures.
- [ ] Test with multiple parallel tool calls.

### B6. OpenAI provider

- [ ] Implement Chat Completions streaming.
- [ ] Implement error classification.
- [ ] Implement provider delta parser.
- [ ] Add mock streaming tests.
- [ ] Add gated real `OPENAI_API_KEY` integration test.

### B7. App integration

- [ ] Build `RuntimeOptions` from `RequestParams` / `ApiKeyManager`.
- [ ] Add routing branch in `generate_multi_agent_output`.
- [ ] Preserve server path for non-routed requests.
- [ ] Skip passive suggestions.
- [ ] Map `RuntimeError` to `AIApiError`.

### B8. Conversation-token continuity validation

- [ ] Test first local `StreamInit` populates conversation token in the existing client.
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

### B12. Observability

- [ ] Add route decision log/telemetry.
- [ ] Add provider finish/error classification log/telemetry.
- [ ] Add redaction tests.
- [ ] Add capture that proves no `/ai/multi-agent` traffic for routed BYOK.

### B13. Documentation

- [ ] Update `HANDOFF.md` or `AGENTS.md`.
- [ ] Document escape hatch.
- [ ] Document local-runtime limitations.
- [ ] Document provider setup.
- [ ] Document unsupported server-only features.

### Stage B exit criteria

- Logged-out BYOK `/agent` works with OpenAI.
- First-turn and follow-up conversations preserve task history.
- Shell/read/grep/glob/apply-diff/MCP tool loops work.
- Custom endpoint-only config routes locally, even when proto `api_keys` is `None`.
- Passive suggestions still use the existing server path.
- MITM shows zero `/ai/multi-agent` traffic for routed BYOK.
- Escape hatch restores old server path.
- `cargo test -p byok_agent` and relevant app tests pass.
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
- Support extended thinking only when request/settings support reasoning messages.
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
- Require actual model name.
- Do not assume `custom/openai-compatible` is a valid provider model.
- Support:
  - base URL
  - optional API key
  - model name
  - optional model prefix
- Add tests against a mock OpenAI-compatible endpoint.
- Optional real test against local Ollama/vLLM/LM Studio should be developer-gated.

### 8.6 Stage C tasks

- [ ] Implement provider trait.
- [ ] Refactor OpenAI adapter behind trait.
- [ ] Add Anthropic adapter.
- [ ] Add Gemini adapter.
- [ ] Add OpenRouter adapter.
- [ ] Add custom endpoint adapter.
- [ ] Add provider capability flags.
- [ ] Add per-provider mock streaming tests.
- [ ] Add gated real integration tests for each provider.
- [ ] Add model-name setting for OpenRouter/custom placeholders.
- [ ] Update provider setup docs.

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
- [ ] Add client config.
- [ ] Add multi-device smoke test.

---

## 10. Validation Matrix

### Build/test commands

Required before merge:

```bash
cargo check -p byok_agent
cargo test -p byok_agent
cargo check -p warp --bin warp-oss
cargo test -p warp ai::agent::api
```

Add exact test filters once tests exist.

### Manual QA

- Logged-out OpenAI BYOK text reply.
- Logged-out OpenAI BYOK shell tool.
- Logged-out OpenAI BYOK read/grep/glob.
- Logged-out OpenAI BYOK apply diff.
- Logged-out custom endpoint smoke test.
- MCP tool call if server configured.
- Stop/cancel mid-stream.
- Bad key.
- Network failure.
- Escape hatch path.
- Passive suggestions still behave as before.

### MITM validation

For routed BYOK:

- zero `POST /ai/multi-agent`.
- provider request goes to OpenAI/custom endpoint.
- no provider key appears in Warp logs.
- no raw capture artifacts are tracked by git.

### Git hygiene validation

- `git status --ignored` shows captures ignored.
- denylist check fails when a dummy `.flow` file is staged.
- denylist check fails when a dummy `Authorization: Bearer` line is staged outside allowed redacted files.

---

## 11. Risks and Mitigations

### Wire-format drift

Risk: upstream proto changes.

Mitigation:

- pin `warp_multi_agent_api`.
- compile/test against pinned revision.
- capture-based fixtures catch event ordering drift.
- avoid hardcoding field masks without tests.

### First-turn task/token mismatch

Risk: local first response does not update conversation token; follow-up loses tasks.

Mitigation:

- explicit B8 invariant tests.
- first-turn `CreateTask`.
- verify next `RequestParams` includes token/tasks.

### Capture secret leakage

Risk: raw MITM flows contain secrets.

Mitigation:

- Stage A0 gitignore.
- pre-commit/CI denylist.
- redacted summaries only.

### Runtime async mismatch

Risk: raw `reqwest` polled outside tokio.

Mitigation:

- prefer `http_client`, or test app executor invariant.
- no nested runtime hacks.

### Tool schema mismatch

Risk: provider tool calls cannot be converted to Warp actions.

Mitigation:

- `ToolNameMap`.
- structural proto tests.
- Stage A captures.

### MCP collisions

Risk: dynamic MCP tool names collide with static or other MCP names.

Mitigation:

- reserved prefixes.
- slug + hash.
- collision tests.

### Provider-specific API quirks

Risk: OpenAI-compatible endpoints reject OpenAI-only fields.

Mitigation:

- capability flags.
- conservative default request body.
- custom endpoint tests.

### Web search gap

Risk: model thinks web search exists because request settings enable it.

Mitigation:

- do not advertise web tools.
- system prompt says unavailable.
- safe log when disabled.

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

- Capture artifact protection is first.
- First-turn `StreamInit` / `server_conversation_token` invariant is mandatory.
- Custom endpoint-only routing is mandatory.
- Runtime async strategy is chosen.
- New crate tests are included in CI/default validation.
- Tool name/MCP collision mapping is mandatory.
- Non-UGC telemetry/logging is included.
- Web search is explicitly disabled/deferred for local runtime.

---

## 13. Tracking Checklist

### Stage A

- [ ] Add capture ignores.
- [ ] Add capture denylist.
- [ ] Write MITM docs.
- [ ] Build MITM addon.
- [ ] Capture redacted reference flows.
- [ ] Decode `/ai/multi-agent` event streams.
- [ ] Commit redacted index/summaries only.

### Stage B

- [ ] Add runtime options and routing tests.
- [ ] Create `byok_agent`.
- [ ] Add test coverage to CI/default validation.
- [ ] Rebuild conversations from request.
- [ ] Add system prompt.
- [ ] Build tool catalog.
- [ ] Build `ToolNameMap`.
- [ ] Implement OpenAI adapter.
- [ ] Translate provider deltas to Warp events.
- [ ] Integrate in `generate_multi_agent_output`.
- [ ] Validate conversation-token continuity.
- [ ] Validate tool loop.
- [ ] Validate cancellation/errors.
- [ ] Add observability.
- [ ] Update docs.

### Stage C

- [ ] Add provider trait.
- [ ] Add Anthropic.
- [ ] Add Gemini.
- [ ] Add OpenRouter.
- [ ] Add custom endpoint.
- [ ] Add provider-specific tests.

### Stage D

- [ ] Add proxy.
- [ ] Add SQLite persistence.
- [ ] Add proxy auth.
- [ ] Add client proxy config.
- [ ] Add multi-device test.

