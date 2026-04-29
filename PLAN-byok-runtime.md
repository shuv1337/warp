# PLAN — In-process BYOK runtime + MITM observation of `app.warp.dev`

> **Status:** revised-v2 after deep codebase audit, not started.
> **Owner:** TBD.
> **Goal:** Logged-out BYOK `/agent` works end-to-end without contacting `app.warp.dev` for the agent loop, using the user's own provider key (OpenAI first, others next). Built so the runtime can be lifted into a standalone proxy later for cross-device sync.

---

## Context

Today's flow for an agent request in this fork:

1. UI builds a `RequestParams` in `app/src/ai/agent/api.rs::RequestParams::new`.
2. `app/src/ai/agent/api/impl.rs::generate_multi_agent_output` converts it to `warp_multi_agent_api::Request` (proto).
3. `ServerApi::generate_multi_agent_output` POSTs proto to `https://app.warp.dev/ai/multi-agent` and returns an SSE-decoded `AIOutputStream<warp_multi_agent_api::ResponseEvent>`.
4. `BlocklistAIController` consumes that stream and applies `ClientAction`s to the local agent state model.

The Warp server is doing **agent orchestration**, not just LLM proxying. It runs the LLM call, maps Warp tools to provider tool definitions, runs the multi-step agent loop, and streams back granular `ClientAction` deltas (`BeginTransaction` / `AddMessagesToTask` / `AppendToMessageContent` / `CommitTransaction`, plus tool calls as separate messages).

We have already confirmed:
- The server returns a generic `400 Bad Request` for any anonymous-context BYOK request — even with valid keys, even with the request shape sanitized — because the multi-agent endpoint requires a real Warp account.
- mojomast/warp's "send no bearer token" workaround does not actually work; their fork hit the same 400 we see (it just rebrands and adds OpenRouter routing, which would have the same problem).
- The rest of the OSS app (Drive, RTC, telemetry, model fetch, login) is hardcoded to `app.warp.dev` and **the OSS channel refuses `--server-root-url` overrides** by design (`Channel::allows_server_url_overrides()` returns `false` for `Channel::Oss`).

There is no path to fixing logged-out BYOK by tweaking the request. We must produce the `ResponseStream` ourselves.

Additional codebase findings incorporated in this revision:

- `warp_multi_agent_api` is not a checked-in `crates/warp_multi_agent_api` crate. It is a git dependency pinned in root `Cargo.toml` to `warpdotdev/warp-proto-apis` rev `78a78f21a75432bf0141e396fb318bf1694e47f0`; the generated Rust crate exposes `get_descriptor_pool()` and `MESSAGE_DESCRIPTOR` via `prost-reflect`.
- `http_client::RequestBuilder::proto()` sends raw `application/x-protobuf` request bytes. The `/ai/multi-agent` response stream is SSE, but each message `data` payload is a quoted, URL-safe-base64-encoded protobuf `ResponseEvent`, decoded today in `app/src/server/server_api.rs`.
- `app/src/ai/agent/api.rs::RequestParams::new` currently clears `tasks` and `existing_suggestions` for a brand-new BYOK conversation with no server token. Therefore the first local runtime request must build context from `request.input`, not only from `task_context.tasks`.
- The client starts each new conversation with an optimistic root task. The first server/runtime response must create or upgrade the root task before appending messages; otherwise `AddMessagesToTask` can hit `TaskNotInitialized` / `TaskNotFound` paths in `app/src/ai/agent/conversation.rs` and `app/src/ai/agent/task.rs`.
- `app/src/ai/agent/api::ResponseStream` is `Stream<Item = Result<ResponseEvent, Arc<AIApiError>>>`. A new crate should not expose `anyhow::Error` directly to the app stream without an adapter, and provider/runtime errors should generally be represented as `StreamFinished` reasons once a stream has started.
- `ContextFlag` values are enabled by default, but `ContextFlag::set` is debug-only and `FromStr` currently parses only a subset of flags. Do not rely on a new `ContextFlag` alone as the OSS escape hatch.
- **`is_byok_request` is computed as a local variable** in `RequestParams::new` (L269–276) and is NOT stored on the `RequestParams` struct. The same check is duplicated privately as `request_has_byo_ai_credentials` in `server_api.rs` (L1281–1293) and in `log_byok_request_shape`. The routing function `should_route_locally` will need its own copy or the flag should be propagated through `RequestParams`.
- **The background executor is tokio-based** (`Background` in `warpui_core/src/async/native/executor.rs` wraps `tokio::runtime::Runtime`). The existing `http_client` bridges tokio↔smol via `async_compat::Compat`. Our `byok_agent` crate's `reqwest` calls will run inside `ModelContext::spawn()`, which already schedules on the tokio background executor — **no extra async-runtime bridging is needed** as long as the crate returns a `Stream` consumed via warpui's `ctx.spawn()`.
- **Passive suggestions and agent requests share the same code path.** Both flow through `generate_multi_agent_output` in `impl.rs`. The URL divergence (`/ai/multi-agent` vs `/ai/passive-suggestions`) happens downstream in `ServerApi` based on `request.input.type`. The local runtime interception must explicitly **skip passive suggestions** (they're server-evaluated and not BYOK-relevant).
- **MCP tool definitions are part of the request NOW.** `request.mcp_context` carries tool/resource definitions from connected MCP servers, and `CallMcpTool` / `ReadMcpResource` are in the default `supported_tools` list. Since tool execution is CLIENT-SIDE (the runtime only needs to emit `ToolCall` messages and receive results via `ActionResult` on the next request), MCP tools are not fundamentally harder than shell commands — but the runtime must dynamically map MCP tool definitions from `mcp_context` to provider tool schemas.

## Goals (this plan)

1. **Stage A — Observation:** stand up an MITM proxy capable of capturing the full TLS-decrypted request/response flow between the client and `app.warp.dev`, persist captures, and document what each endpoint is doing. This unblocks future work on Drive/RTC/sync without guessing.
2. **Stage B — In-process runtime, OpenAI:** intercept agent requests when BYOK keys are configured and serve them from a new in-process `byok_agent` crate that runs the agent loop locally. First milestone: `/agent` text reply with no tools. Second milestone: shell + read + grep + glob + apply-diff tools end-to-end. Third milestone: MCP tools.
3. **Stage C — Multi-provider:** generalize the runtime so Anthropic, Gemini, OpenRouter, and OpenAI-compatible custom endpoints plug into the same agent loop via a thin trait.
4. **Stage D — Lift to proxy:** when cross-device sync becomes desired, extract the `byok_agent` crate behind a small SSE/HTTP server with the same proto wire format. The client gains a "BYOK server URL" setting; the runtime code does not change.

Non-goals **for this plan**: fixing Drive/RTC/session-sharing/telemetry, building cloud Oz/ambient runs, replacing login. Those are separate work items unblocked by Stage A's observability.

---

## Stage A — MITM observation of `app.warp.dev`

### Why first

We're going to be guessing at proto request/response shapes for several upcoming features (resume conversation, ambient agent, Drive sync) for years. A persistent capture archive pays for itself the first time we want to know "what did the server return when X happened?" without reverse-engineering blind.

The runtime work in Stage B uses these captures as **the spec**: the easiest way to know we're producing valid `ClientAction` event streams is to compare ours against a real one for the same prompt.

### What we already know about HTTP plumbing

- Production `http_client::Client` uses `reqwest::Client::builder()` defaults → respects `HTTPS_PROXY` / `HTTP_PROXY` env vars.
- TLS root cert source: `reqwest`'s default (rustls + webpki on Linux) → respects `SSL_CERT_FILE` env var to add an extra trusted root.
- Actual `reqwest` config uses `rustls-tls-native-roots-no-provider`, `system-proxy`, and `macos-system-configuration`. On macOS, prefer trusting the mitmproxy CA in the system keychain; `SSL_CERT_FILE` may still be useful on Linux and should be documented as platform-dependent.
- WebSocket clients (`crates/websocket/`) already have their own proxy support via `HTTPS_PROXY` / `WSS_PROXY` / `ALL_PROXY` env vars.
- Test client uses `tls_built_in_root_certs(false)` and `no_proxy()` — production does **not**, so MITM works without code changes.
- One known gotcha: `app/src/ai/agent_sdk/test_support.rs` and `app/src/server/telemetry/mod.rs` build their own clients — verify those honor env proxy when relevant.
- Response SSE payloads are not plain protobuf bytes on the wire. Decode each SSE `data:` value by trimming quotes, URL-safe-base64 decoding, then decoding `warp_multi_agent_api::ResponseEvent`.

### Tasks

- [ ] **A1.** Document the standard MITM setup in `docs/dev/mitm.md`:
  - Install `mitmproxy` (`pip install mitmproxy` or distro pkg).
  - Run `mitmweb --listen-port 8080 --ssl-insecure` for the UI, or `mitmdump -w captures/$(date +%Y%m%d-%H%M%S).flow` to capture to disk.
  - Trust mitmproxy's CA: copy `~/.mitmproxy/mitmproxy-ca-cert.pem` to a known path, set `SSL_CERT_FILE=/path/to/mitmproxy-ca-cert.pem` (or merge into the system bundle).
  - Launch warp-oss with `HTTPS_PROXY=http://127.0.0.1:8080 SSL_CERT_FILE=/path/... ./script/run`.
  - Verify capture: hit `/agent` with a real Warp account and confirm `POST app.warp.dev/ai/multi-agent` shows up decrypted in the mitmweb UI.

- [ ] **A2.** Build a small mitmproxy addon (`scripts/mitm/warp_addon.py`) that:
  - Logs only flows where `flow.request.host == "app.warp.dev"` (and `*.app.warp.dev`).
  - For each flow, dumps `<timestamp>-<method>-<path>.{request,response}.bin` (raw bytes) and a sibling `.json` with method/path/status/headers/timestamps. Redact `Authorization`, provider keys, cookies, user prompts, file contents, and attachment data from committed summaries.
  - SSE bodies are saved verbatim with `\n\n` event boundaries preserved. For `/ai/multi-agent`, additionally decode each SSE event's quoted URL-safe-base64 `data` into `ResponseEvent` protobuf bytes.
  - Recognises proto endpoints (`/ai/multi-agent`, `/ai/passive-suggestions`, anything else discovered) and additionally writes a decoded text/JSON view. Do **not** point at non-existent `crates/warp_multi_agent_api/proto/*.proto`; use one of:
    - Python generated proto modules from the pinned `warp-proto-apis` checkout.
    - A tiny Rust decoder helper that depends on `warp_multi_agent_api` and uses `prost::Message` / `prost-reflect`.
  - Output dir: `captures/` (gitignored). Commit only redacted fixture snippets or `captures/INDEX.md`, not raw sensitive flows.

- [ ] **A3.** Capture canonical reference flows to disk (each labelled and summarized in `captures/INDEX.md`; raw `.flow` files stay local unless explicitly redacted):
  - Logged-in: fresh `/agent` text reply, no tools.
  - Logged-in: `/agent` that uses `RunShellCommand`, `ReadFiles`, `Grep`.
  - Logged-in: `/agent` that uses `CallMcpTool` (if MCP servers are configured).
  - Logged-in: `/agent` resume of an existing conversation.
  - Logged-in: empty conversation list fetch (`?` — endpoint TBD).
  - Logged-in: model fetch (`/get-feature-model-choices` or similar).
  - Logged-in: passive prompt-suggestion request.
  - Logged-out, BYOK request: confirm and snapshot the exact 400 (already characterized, but pin the byte-level response).

- [ ] **A4.** Write `captures/INDEX.md` cataloguing each capture: scenario, prompt, model, BYOK-or-Warp, what was tested, what's interesting.

- [ ] **A5.** Add a README section pointing developers at the MITM workflow when investigating server behaviour.

### Exit criteria for Stage A

- We have at least 6 reference captures referenced, with redacted summaries committed and raw sensitive captures kept out of git.
- A new contributor can reproduce a capture in <10 minutes by following `docs/dev/mitm.md`.
- We can decode a capture and produce the full sequence of `ResponseEvent`s for a real `/agent` request to use as the gold reference for Stage B's translator, including whether a first-turn stream includes `CreateTask` and the exact `FieldMask` paths used in `AppendToMessageContent`.

---

## Stage B — In-process runtime, OpenAI first

### Architecture

A new crate `crates/byok_agent` exposes:

```rust
// crates/byok_agent/src/lib.rs
pub fn run_request(
    request: warp_multi_agent_api::Request,
    options: RuntimeOptions,
) -> impl Stream<Item = Result<warp_multi_agent_api::ResponseEvent, RuntimeError>>;
```

`app` adapts `RuntimeError` into `Arc<AIApiError>` only for pre-stream transport/setup failures. Once the local runtime has emitted `StreamInit`, provider failures should be converted to `ResponseEvent::StreamFinished` with the appropriate reason so the existing controller renders them through `handle_response_stream_finished`.

`RuntimeOptions` is needed because the current request proto carries first-party provider keys but does **not** carry the local custom endpoint config (`base_url`, optional key, model prefix) stored in `ApiKeyManager`. It should also carry the escape-hatch override flag.

Internally:

```
crates/byok_agent/
  src/
    lib.rs                  // public entrypoint: run_request -> ResponseStream
    runtime/
      mod.rs                // agent loop: choose provider, call LLM, dispatch tool calls,
                            //   wait for tool_call_result on next request, terminate
      conversation.rs       // build provider-format messages from `task_context.tasks[].messages`
      tools.rs              // ToolCatalog: which Warp tools we expose, mapped to provider schemas
                            //   + dynamic MCP tool definitions from request.mcp_context
      streaming.rs          // assistant-token + tool-call-arg streaming state machine
      system_prompt.rs      // local agent system prompt, versioned and snapshot-tested
    providers/
      mod.rs                // trait LLMProvider; provider selection from api_keys + model id
      openai.rs             // OpenAI streaming impl (Chat Completions API for broad compatibility;
                            //   Responses API adapter for first-party OpenAI if needed)
    convert/
      from_warp.rs          // Request -> ProviderCall (messages, tools, model, opts)
      to_warp.rs            // assistant deltas / tool calls -> ResponseEvent stream
                            //   (BeginTransaction / AddMessagesToTask / AppendToMessageContent
                            //    / CommitTransaction / StreamFinished)
    error.rs                // BYOKError -> mapped onto stream as StreamFinished{InvalidApiKey,
                            //   InternalError, LlmUnavailable, ContextWindowExceeded, etc.}
    ids.rs                  // UUID/ULID generation for conversation_id, request_id, message_id, task_id
  Cargo.toml                // deps: warp_multi_agent_api, reqwest, reqwest-eventsource or eventsource-stream,
                            //   prost, prost-reflect, futures, serde, serde_json,
                            //   uuid, anyhow/thiserror, log
```

Note: `tokio` is NOT a direct dependency — the crate returns a `Stream` and all async work runs on whatever executor the caller (`app`) provides. Since `app` uses warpui's tokio-backed `Background` executor via `ModelContext::spawn()`, reqwest's tokio requirement is satisfied automatically. The crate should use `async_stream::stream!` or manual `Stream` impls, not `#[tokio::main]`.

Add the crate to root `Cargo.toml` `[workspace.dependencies]` as `byok_agent = { path = "crates/byok_agent" }`, then add `byok_agent.workspace = true` to `app/Cargo.toml`. The workspace already includes `crates/*`, but default presubmit paths may still require explicit `cargo check -p byok_agent` until it is exercised through `app`.

### Interception point

Single edit in `app/src/ai/agent/api/impl.rs::generate_multi_agent_output`. Right before the existing `server_api.generate_multi_agent_output(&request).await` line (currently L134):

```rust
// Skip passive suggestions — they're server-evaluated and not BYOK-relevant.
let is_passive = request.input.as_ref().is_some_and(|input| {
    matches!(
        input.r#type,
        Some(warp_multi_agent_api::request::input::Type::GeneratePassiveSuggestions(_))
    )
});

if !is_passive && should_route_locally(&request, &runtime_options) {
    let stream = byok_agent::run_request(request, runtime_options);
    return Ok(Box::pin(stream.map(map_runtime_error).take_until(cancellation_rx)));
}
```

`should_route_locally(&request, &options)` returns true when:
- BYOK keys are present in the request (replicate the `request_has_byo_ai_credentials` check from `server_api.rs:1281`, since that function is private and inaccessible here), AND
- `model_config.base` resolves to a known BYOK model (in our `provider_registry`), AND
- request input type is NOT `GeneratePassiveSuggestions` (passive suggestions stay server-side), AND
- runtime is not disabled by the local escape hatch (see B0).

This means logged-in users with BYOK keys *also* get routed locally, which actually fixes their experience too (the logged-in BYOK path is currently slow + lossy because the Warp server is just a transport in that case).

**Note on `convert_input` ordering:** The interception happens AFTER `convert_input(params.input)?` has already run (the full `api::Request` proto is constructed at L62–129 before the server call at L134). This means routing decisions can inspect the fully-formed request, but any `ConvertToAPITypeError` from `convert_input` will abort before we get the chance to route locally. This is acceptable — a malformed input should fail regardless.

### Tool surface (Stage B)

The multi-turn tool loop works as follows:
1. Runtime calls LLM → LLM requests a tool call
2. Runtime emits `AddMessagesToTask` with a `ToolCall` message + `CommitTransaction` + `StreamFinished(Done)`
3. Client's `BlocklistAIActionExecutor` executes the tool (shell, read files, MCP, etc.)
4. Client sends a new request with `AIAgentInput::ActionResult` containing tool results
5. Runtime receives this, feeds results + conversation history to LLM, repeats

The runtime does NOT execute tools itself. It only emits tool-call messages and processes results from subsequent requests. This means **MCP tools work the same as shell commands** — the runtime emits `ToolCall(CallMcpTool)` and the client's `call_mcp_tool.rs` executor handles the actual MCP call.

Day-1 supported tools, mapped end-to-end:

| Warp `ToolType` | provider schema | notes |
|---|---|---|
| `RUN_SHELL_COMMAND` | `run_shell_command(command, working_directory?)` | client executor wired |
| `READ_FILES` | `read_files(paths[])` | client executor wired |
| `GREP` | `grep(pattern, path?, ...)` | client executor wired |
| `FILE_GLOB_V2` | `file_glob(pattern, path?)` | client executor wired |
| `APPLY_FILE_DIFFS` | `apply_file_diffs(diffs)` | client executor wired |
| `READ_SHELL_COMMAND_OUTPUT` | `read_shell_command_output(...)` | client executor wired |
| `SEARCH_CODEBASE` | `search_codebase(query)` | client executor wired |
| `ASK_USER_QUESTION` | `ask_user_question(...)` | client executor wired |
| `CALL_MCP_TOOL` | dynamic from `request.mcp_context` | client executes via `ReconnectingPeer` |
| `READ_MCP_RESOURCE` | dynamic from `request.mcp_context` | client executes via `ReconnectingPeer` |

Day-1 stubbed (advertised as unavailable, won't appear in tool list):
- `USE_COMPUTER`, `REQUEST_COMPUTER_USE` (no client UI for headless model anyway in OSS-only flow)
- `START_AGENT`, `START_AGENT_V2`, `SEND_MESSAGE_TO_AGENT`, `SUBAGENT` (orchestration v2)
- `OPEN_CODE_REVIEW`, `INSERT_REVIEW_COMMENTS`, `FETCH_CONVERSATION` (review/server features)
- `INIT_PROJECT`, `READ_DOCUMENTS`, `EDIT_DOCUMENTS`, `CREATE_DOCUMENTS`, `READ_SKILL` (later)
- `SUGGEST_PLAN`, `SUGGEST_CREATE_PLAN`, `SUGGEST_NEW_CONVERSATION`, `SUGGEST_PROMPT` (server-implemented today; punt)
- `WRITE_TO_LONG_RUNNING_SHELL_COMMAND`, `TRANSFER_SHELL_COMMAND_CONTROL_TO_USER` (later)
- `UPLOAD_FILE_ARTIFACT` (artifact storage = server feature)

The client builds the supported-tools list from feature flags + execution profile. The runtime must advertise only the intersection of:
- tools in `request.settings.supported_tools`;
- tools implemented in `byok_agent::runtime::tools`;
- tools valid for the current session shape (`supported_cli_agent_tools` matters for CLI subagent/long-running command paths).

For **MCP tools**, the runtime must additionally:
- Parse `request.mcp_context.servers[].tools[]` to get dynamic tool definitions (name, description, input_schema as JSON).
- Convert these to provider-format function definitions (e.g., OpenAI `tools[].function`).
- When the LLM calls an MCP tool, emit a `ToolCall(CallMcpTool { server_id, name, input })` message, matching the proto format the client's `call_mcp_tool.rs` executor expects.

If a provider emits an unadvertised or unknown tool anyway, finish the stream with `InternalError` instead of emitting a malformed `ToolCall`.

### Conversation rebuild

Follow-up `Request`s carry the active task history in `task_context.tasks[].messages`. The runtime treats every request as **stateless from its own perspective**: rebuild the provider-format message array fresh each turn from the proto plus the current `request.input`. We never need server-side conversation persistence for Stage B because the client already keeps it after the initial stream.

First-turn caveat: for a brand-new BYOK conversation, `RequestParams::new` currently sends an empty `task_context.tasks` (gated on `is_byok_request && conversation.server_conversation_token.is_none()`, L319) and puts the user query only in `request.input`. The runtime must:
- derive or mint a root `task_id`;
- build provider input from `request.input.user_inputs`;
- emit a `CreateTask` for the root task before any `AddMessagesToTask` / `AppendToMessageContent`;
- use the server/runtime `request_id` consistently on emitted `Message`s so downstream history and telemetry stay coherent.

`task_context.tasks` may have multiple tasks (subagents). Stage B handles only the **primary task** (find by `agent_type == AGENT_TYPE_PRIMARY` or just task[0] for now). Subagents are stubbed to error.

Mapping from Warp `Message.message` oneof variants to provider message roles (full enumeration — the proto has **21 variants**):
- `user_query` → `role: user`, content = text + attachments
- `agent_output` → `role: assistant`, content = text
- `tool_call` → `role: assistant`, with `tool_calls: [{id, function: {name, arguments}}]`
- `tool_call_result` → `role: tool`, `tool_call_id`, content = stringified result
- `system_query` → `role: system` (or fold into the user message; provider-dependent)
- `agent_reasoning` → drop for OpenAI; surface as `role: assistant` with `extended_thinking` for Anthropic later
- `model_used` → drop (informational; runtime can emit its own)
- `summarization` → `role: assistant` (fold into context as a condensed summary if present)
- `web_search` / `web_fetch` → drop (server-side feature; see Out of scope)
- `update_todos` → drop (UI-only state)
- `server_event` → drop (server internal)
- `code_review` / `update_review_comments` → drop (review features deferred)
- `debug_output` → drop
- `artifact_event` → drop
- `invoke_skill` → drop (skill invocation deferred)
- `messages_received_from_agents` / `events_from_agents` → drop (orchestration deferred)
- `passive_suggestion_result` → drop (passive flow not routed locally)

The server-side hidden system prompt is not available from MITM captures because it is not sent by the client. Stage B needs an explicit local system prompt that describes Warp's agent behavior, tool-use protocol, safety/approval constraints, and output style. Treat this prompt as a versioned artifact in `crates/byok_agent/src/runtime/system_prompt.rs` and cover it with snapshot tests.

### ClientAction protocol (full enumeration)

The `ClientAction.action` oneof has **14 variants** (field 2 reserved). The runtime must handle or consciously skip each:

| Variant | Stage B runtime emits? | Notes |
|---|---|---|
| `CreateTask` (1) | **Yes** — first turn only, to upgrade optimistic root task | Also used for subtasks (deferred) |
| `AddMessagesToTask` (3) | **Yes** — initial assistant message, tool calls | Core streaming action |
| `UpdateTaskMessage` (4) | **No** — used for in-place updates (e.g., tool approval status) | May need later for tool result rendering |
| `AppendToMessageContent` (5) | **Yes** — streaming text tokens | Core streaming action |
| `Suggestions` (6) | **No** — server-generated prompt suggestions | Punt to Stage C+ |
| `UpdateTaskSummary` (7) | **No** — server writes task summaries for sidebar | Optional polish |
| `UpdateTaskDescription` (8) | **No** — server updates task title | Optional polish |
| `BeginTransaction` (9) | **Yes** — wraps each logical batch of actions | Core protocol |
| `CommitTransaction` (10) | **Yes** — closes batch | Core protocol |
| `RollbackTransaction` (11) | **Maybe** — on provider error mid-transaction | Emit before `StreamFinished` if a transaction is open |
| `StartNewConversation` (12) | **No** — server-initiated conversation split | Deferred |
| `UpdateTaskServerData` (13) | **No** — server metadata (URL, timestamps) | Can emit for consistency |
| `MoveMessagesToNewTask` (14) | **No** — orchestration v2 subagent task splitting | Deferred |

### StreamFinished reasons (full enumeration)

The `StreamFinished.reason` oneof has **8 variants**. The runtime maps provider errors as follows:

| Reason variant | When runtime emits |
|---|---|
| `Done` (2) | Normal completion, `finish_reason: stop` |
| `Other` (1) | Unexpected/unknown finish |
| `ReachedMaxTokenLimit` (3) | Provider returns `max_tokens` / `length` finish reason |
| `QuotaLimit` (4) | Provider returns `429` |
| `ContextWindowExceeded` (5) | Provider returns `context_length_exceeded` error |
| `LlmUnavailable` (6) | Provider returns `5xx`, network error, timeout |
| `InternalError` (7) | Runtime bug, unexpected provider response |
| `InvalidApiKey` (12) | Provider returns `401` / `invalid_api_key`. Has fields: `provider: LLMProvider`, `model_name: string` |

### Streaming response state machine

Provider streaming is normalized into `ProviderDelta`s. A Chat Completions-compatible stream yields chunks like:
```
{"choices":[{"delta":{"role":"assistant"}}]}
{"choices":[{"delta":{"content":"Hello"}}]}
{"choices":[{"delta":{"content":" world"}}]}
{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"grep","arguments":"{\"patte"}}]}}]}
{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"rn\":\"foo\"}"}}]}]}
{"choices":[{"finish_reason":"tool_calls"}]}
```

Translate to:
```
ResponseEvent::Init { conversation_id: <UUID>, request_id: <UUID>, run_id: <UUID> }
ResponseEvent::ClientActions {
  actions: [
    ClientAction::CreateTask { task: Task { id: <root_task_id>, ... } }, // first turn only
    ClientAction::BeginTransaction {},
    ClientAction::AddMessagesToTask { task_id, messages: [Message{ id: msg_a, message: AgentOutput{ text: "" } }] }
  ]
}
ResponseEvent::ClientActions {
  actions: [
    ClientAction::AppendToMessageContent { task_id, message: Message{ id: msg_a, message: AgentOutput{ text: "Hello" }}, mask: ["message.agent_output.text"] },
  ]
}
... more deltas ...
ResponseEvent::ClientActions {
  actions: [
    ClientAction::AddMessagesToTask { task_id, messages: [Message{ id: msg_b, message: ToolCall{ tool_call_id, tool: <grep variant> } }] },
    ClientAction::CommitTransaction {},
  ]
}
ResponseEvent::Finished { reason: Done {} }
```

Each `Message.id` we mint as a fresh UUID/ULID; client correlates them by id across `AppendToMessageContent` calls. `task_id` is taken from the inbound primary task when present; otherwise mint it for the first-turn `CreateTask` and reuse it for all actions in that stream. `conversation_id` should reuse `request.metadata.conversation_id` when provided and mint a new ID only for new conversations. `run_id` should be set to a stable per-stream UUID.

We must reference Stage A captures to confirm the exact ordering and `mask` field-paths the client expects. The `FieldMask` is applied using `crates/field_mask/src/lib.rs` which uses `prost_reflect::DynamicMessage` — so paths must match the proto field descriptors precisely (e.g., `message.agent_output.text`, not just `text`).

### OpenAI provider impl

- Endpoint: prefer `POST https://api.openai.com/v1/chat/completions` with `stream: true` for **broad compatibility** across OpenAI, OpenRouter, and custom OpenAI-compatible endpoints. All three use Chat Completions format. If first-party OpenAI adds Responses API benefits we need (e.g., typed streaming), add that as an opt-in adapter later — not the default path.
- Auth: `Authorization: Bearer <api_keys.openai>` from the inbound `Request.settings.api_keys.openai`.
- Tool definitions: Warp `ToolType` → OpenAI `tools[].function` schema (one shared static catalog in `runtime/tools.rs`). MCP tools are added dynamically from `request.mcp_context`.
- Reasoning: support `reasoning_effort` where the selected endpoint/model accepts it (set to `medium` by default; expose via execution profile later). Do not send unsupported reasoning fields to OpenAI-compatible providers without capability gating.
- Vision: forward image attachments as `image_url` parts when `vision_supported`.
- Errors:
  - `401 invalid_api_key` → `StreamFinished { reason: InvalidApiKey { provider, model_name } }`
  - `429` → `StreamFinished { reason: QuotaLimit {} }`
  - `400 context_length_exceeded` → `StreamFinished { reason: ContextWindowExceeded {} }`
  - `5xx` / network → `StreamFinished { reason: LlmUnavailable {} }`
  - `finish_reason: "length"` → `StreamFinished { reason: ReachedMaxTokenLimit {} }`
  - everything else → `StreamFinished { reason: InternalError { message } }`

### Cancellation

The existing `cancellation_rx: futures::channel::oneshot::Receiver<()>` plumbed through `generate_multi_agent_output` already cuts the stream when the user clicks Stop. Do not consume the same oneshot inside `byok_agent`; let `take_until(cancellation_rx)` drop the local stream. The runtime's HTTP request to the provider is cancelled by dropping the underlying streaming response/EventSource future. Verify with a long completion and a Stop click.

### Conversation persistence gap

**Important limitation:** With the Warp server, conversation history can be restored from the server after an app restart. With the local runtime, there is no server-side persistence. The client keeps conversation state in memory and in local storage (`~/Library/Application Support/dev.warp.WarpOss/`), but:
- If the app crashes mid-conversation, the conversation may be lost.
- Resume-on-error (the `ResponseStream` retry logic) still works because it resends the full `RequestParams` — but only within the same app session.
- Cross-device sync is unavailable until Stage D.

For Stage B, this is acceptable. Document the limitation and ensure conversations are written to local storage after each complete exchange. Stage D's SQLite persistence addresses the gap.

### Tasks

- [ ] **B0.** Local escape hatch. Add a private local setting or env var such as `WARP_BYOK_IN_PROCESS_RUNTIME=0` (default on) that falls back to today's `app.warp.dev` path. Do not rely solely on `ContextFlag`: it is debug-set only in non-dogfood builds and `FromStr` currently omits several existing flags. If a `ContextFlag` is still added for deep-link contexts, update both the enum and `FromStr`.

- [ ] **B1.** Create `crates/byok_agent` crate skeleton. Add to workspace dependencies and `app/Cargo.toml`. Add `warp_multi_agent_api`, `reqwest` (with `stream` feature), `reqwest-eventsource` or `eventsource-stream`, `futures`, `async-stream`, `serde`, `serde_json`, `uuid`, `thiserror`/`anyhow`, `log`, `prost`, and `prost-reflect` as deps. Do NOT add `tokio` as a direct dep — the crate is executor-agnostic. `cargo check -p byok_agent` and `cargo check -p warp --bin warp-oss` build.

- [ ] **B2.** `runtime/conversation.rs`: `Conversation::from_request(&Request) -> Conversation` that walks `task_context.tasks[primary].messages` **and** current `request.input.user_inputs`, then produces an internal message list with stable `tool_call_id`s. Must handle all 21 `Message.message` variants (map the 6 important ones, explicitly drop the rest with logging). Unit tests against fixtures (canned `Request` proto bytes from Stage A captures plus synthetic first-turn empty-task fixtures).

- [ ] **B2a.** `runtime/system_prompt.rs`: add a local Warp agent system prompt and snapshot tests. This replaces server-side hidden orchestration instructions that MITM cannot capture.

- [ ] **B3.** `runtime/tools.rs`: static `ToolCatalog` mapping Warp `ToolType` → JSON Schema `parameters`. Initially the day-1 tools above (including MCP), generated by hand from the pinned proto definitions in `warp-proto-apis/apis/multi_agent/v1/task.proto`. Build the provider tool list from the request-supported intersection. For MCP: parse `request.mcp_context.servers[].tools[]` and merge their definitions into the catalog dynamically. Round-trip tests: parse a provider tool call back into a `warp_multi_agent_api::message::ToolCall` variant and assert structural equality against a known-good capture.

- [ ] **B4.** `convert/to_warp.rs`: streaming state machine that consumes assistant/tool deltas and yields `ResponseEvent`s in the right order. Must handle all 14 `ClientAction` variants (emit the 6 core ones, consciously skip the rest). Must emit:
  - `StreamInit` with `conversation_id`, `request_id`, `run_id`
  - First-turn `CreateTask` when the request has no server task
  - `BeginTransaction`
  - Initial `AddMessagesToTask` with empty `AgentOutput`
  - `AppendToMessageContent` with `FieldMask` path `message.agent_output.text`
  - `CommitTransaction`
  - `StreamFinished` with appropriate reason
  - On mid-stream error: `RollbackTransaction` if a transaction is open, then `StreamFinished`
  Unit tested with hand-crafted delta sequences against expected event sequences and Stage A captures.

- [ ] **B5.** `providers/openai.rs`: real OpenAI Chat Completions streaming call. Integration test (gated behind `OPENAI_API_KEY` env var, off in CI) that runs a no-tool prompt and asserts the resulting `ResponseEvent` stream has `Init` → optional first-turn `CreateTask` → `BeginTransaction` → `AddMessagesToTask(AgentOutput)` → `AppendToMessageContent`* → `CommitTransaction` → `Finished{Done}`.

- [ ] **B6.** `lib.rs::run_request` ties it together: pick provider from api_keys + model id, build conversation, build tool catalog, invoke provider, run the to-warp translator. Returns the `Stream<Item = Result<ResponseEvent, _>>`.

- [ ] **B7.** Integration into client: in `app/src/ai/agent/api/impl.rs::generate_multi_agent_output`, after the `[byok-debug]` log, branch on `should_route_locally(&request, &runtime_options)` and substitute the local stream. When not routed locally, behave exactly as today. Explicitly skip passive suggestions (`GeneratePassiveSuggestions` input type). Construct `RuntimeOptions` from `params` before the proto conversion consumes it — this must include custom endpoint config from `ApiKeyManager` (not available in the proto).

- [ ] **B7a.** Propagate `RuntimeOptions` through the call chain. `generate_multi_agent_output` currently takes `(Arc<ServerApi>, RequestParams, oneshot::Receiver)`. Either add `RuntimeOptions` as a 4th param, or attach it to `RequestParams`. The `RuntimeOptions` must carry:
  - Custom endpoint config (`base_url`, `api_key`, `model_prefix`) from `ApiKeyManager`
  - Escape hatch flag
  - Any provider-specific overrides not representable in the proto

- [ ] **B8.** End-to-end test in the GUI: `/agent` text-only reply, no tools, with `gpt-5.5`. Stream renders progressively, finishes cleanly.

- [ ] **B9.** End-to-end with **shell tool only**: ask the agent to run `ls`, see it propose, approve, see result feed back, get a follow-up reply. This proves the tool_call → tool_call_result → next-request loop.

- [ ] **B10.** End-to-end with **read + grep + glob + apply-diff**: ask the agent to read a file and propose a patch. Proves multi-tool turn and the diff tool.

- [ ] **B10a.** End-to-end with **MCP tools** (if MCP servers are configured): ask the agent to use an MCP tool, see the tool call emitted, executed by client, result fed back.

- [ ] **B11.** Cancellation works mid-stream; the OpenAI request is dropped within ~1s of Stop.

- [ ] **B12.** Error-mapping smoke tests: bad key → `InvalidApiKey` reason rendered correctly in UI; oversize prompt → `ContextWindowExceeded`; network down → `LlmUnavailable`; max tokens hit → `ReachedMaxTokenLimit`.

- [ ] **B13.** Add focused tests in `app/src/ai/agent/api/impl_tests.rs` for `should_route_locally`:
  - BYOK OpenAI model + key + escape hatch on routes locally.
  - No key, unknown model, passive suggestions, or escape hatch off falls back to server.
  - Custom endpoint is routed locally only when `RuntimeOptions` carries usable endpoint config.

- [ ] **B14.** Update `HANDOFF.md` (or the local `AGENTS.md`) noting the new component and its integration point.

### Exit criteria for Stage B

- `/agent` works end-to-end logged-out with `gpt-5.5` and an OpenAI key, including tool-using prompts that run shell commands, read files, grep, glob, edit files, and call MCP tools.
- No request to `app.warp.dev/ai/multi-agent` for a BYOK request — verified by mitmproxy capture showing zero `/ai/*` traffic.
- Disabling the local escape hatch reverts to the old behaviour.

---

## Stage C — Multi-provider

### Tasks

- [ ] **C1.** `LLMProvider` trait formalised:
  ```rust
  trait LLMProvider {
      async fn stream_chat(
          &self,
          conv: &Conversation,
          tools: &ToolCatalog,
          opts: &CallOptions,
      ) -> Result<BoxStream<'static, ProviderDelta>, ProviderError>;
  }
  ```
  `ProviderDelta` is the provider-agnostic intermediate (text-token, tool-call-fragment, finish-reason) consumed by `convert/to_warp.rs`.

- [ ] **C2.** `providers/anthropic.rs`: Anthropic Messages API streaming, including `extended_thinking` blocks for the `agent_reasoning` Warp message variant. Map Anthropic's `tool_use` content blocks to `ToolCall` messages.

- [ ] **C3.** `providers/google.rs`: Gemini streaming, with the function-calling glue. Map Gemini's `functionCall` parts to `ToolCall` messages.

- [ ] **C4.** `providers/openrouter.rs`: thin wrapper that points OpenAI Chat Completions client at `https://openrouter.ai/api/v1` with the OpenRouter key. Strip the `openrouter/` prefix from the model id before sending. Add the `HTTP-Referer` and `X-Title` headers OpenRouter requires.

- [ ] **C5.** `providers/openai_compatible.rs`: same as OpenAI but with a user-configurable `base_url` (for self-hosted vLLM, ollama-with-openai-shim, LM Studio, etc.). Current custom endpoint config lives in `ApiKeyManager::keys().custom_endpoint` and is not serialized into `warp_multi_agent_api::request::settings::ApiKeys`, so plumb it through `RuntimeOptions` (addressed in B7a).

- [ ] **C6.** Provider-selection logic from request/options: priority is model-id driven (`gpt-*` → openai, `claude-*` → anthropic, `gemini-*` → google, `openrouter/*` → openrouter, `custom/*` → openai-compatible, `aws-bedrock/*` deferred). The `api_keys` block and `RuntimeOptions` are consulted to pick credentials; missing keys produce `InvalidApiKey` upfront.

- [ ] **C6a.** Add a real custom model identifier path. Today `provider_registry` exposes only `openrouter/custom` and `custom/openai-compatible`, which are placeholders rather than the actual provider model name. Add a per-provider model-name setting/input before claiming OpenRouter/custom endpoints work beyond smoke tests.

- [ ] **C7.** Per-provider integration tests, all gated on env-var presence.

### Exit criteria for Stage C

- All four providers successfully run the basic shell-tool E2E test.
- Switching providers via the model picker just works.
- A custom OpenAI-compatible endpoint (e.g. local ollama) works for at least one Llama-class model that supports tools, using an explicit configured model name rather than the placeholder `custom/openai-compatible`.

---

## Stage D — Lift to standalone proxy (future)

When cross-device sync becomes a real requirement:

- [ ] **D1.** New crate `crates/byok_proxy` (binary). Wraps `byok_agent::run_request` behind:
  - `POST /ai/multi-agent` (proto in, SSE proto out — same format as `app.warp.dev`).
  - Local SQLite for conversation persistence keyed by `conversation_id`.
  - Optional auth: a static bearer token configured in `proxy.toml`, applied to all clients.
- [ ] **D2.** Allow `--server-root-url` override in `Channel::Oss` *only when* a new `byok_proxy_url` config is set (don't open the general flag, scope the override).
- [ ] **D3.** Client-side: when `byok_proxy_url` is set, route ALL agent requests there (not just BYOK), and skip Warp's `app.warp.dev` agent endpoint entirely.
- [ ] **D4.** Multi-device test: two warp clients pointed at the same proxy share conversation history.

Until D1 ships, the in-process runtime suffices for the BYOK use case and the proxy is a future delivery, not a blocker.

---

## Risks and unknowns

- **Wire-format drift.** Upstream Warp can change the `multi_agent.v1` proto. We sync against `warpdotdev/warp` master and pin our generated proto to a known revision; CI runs `cargo check` against new generations to catch breakage early.
- **`mask` field paths.** `AppendToMessageContent` uses a `google.protobuf.FieldMask` applied via `crates/field_mask/src/lib.rs` using `prost_reflect::DynamicMessage`. For `AgentOutput.text`, the likely path is `message.agent_output.text`, not just `text`; confirm by capture and unit-test with `FieldMaskOperation::append`. `UpdateTaskMessage` also uses `FieldMask` with `FieldMaskOperation::update` (replace semantics instead of append). Our runtime may need `UpdateTaskMessage` for tool-result rendering later.
- **Tool argument streaming.** OpenAI streams tool-call argument JSON token-by-token. The Warp client may want a single `AddMessagesToTask` once arguments are complete, or it may handle progressive updates. Stage A capture answers this; default plan: buffer tool-call args until the function call is finished, then emit one `AddMessagesToTask`.
- **Server prompt gap.** MITM captures client/server protobuf traffic but not the Warp server's hidden LLM system prompt. Local runtime quality will depend on our own prompt and tool descriptions.
- **Reasoning content.** Reasoning models can stream reasoning-related events/metadata depending on endpoint. Map user-visible reasoning summaries to `agent_reasoning` only when available and when `supports_reasoning_message` is true; otherwise drop.
- **Image attachments.** `Request.input.context.attachments` uses Warp's attachment proto. Day-1: surface text-only, ignore images; revisit when we have a capture to copy.
- **Provider rate limits / 429s.** Need exponential backoff in the runtime to avoid wedging the UI on transient 429s. Already partially handled by `BlocklistAIController`'s retry logic (up to 3 retries before `has_received_client_actions`), but worth confirming it covers our error variants.
- **`mitmproxy` certificate trust.** On Linux this just works via `SSL_CERT_FILE`. On macOS, system keychain trust may be needed (`security add-trusted-cert ...`). Worth documenting, off the critical path.
- **Sensitive capture hygiene.** Raw captures can contain provider keys, prompts, file contents, attachment data, cookies, and bearer tokens. Keep raw captures gitignored and commit only redacted fixtures/summaries unless deliberately scrubbed.
- **Conversation restoration after restart.** The local runtime is stateless. If the app restarts, conversation history is only available from local storage — not from a server. This means conversations cannot be restored on a different device, and crash-recovery depends on what the client persisted before the crash. Acceptable for Stage B; addressed by Stage D.
- **Parallel tool calls.** The request has `supports_parallel_tool_calls: true`. If the LLM emits multiple tool calls in one turn, the runtime must emit all of them in one `CommitTransaction` batch. The client will execute them (sequentially or in parallel based on its own policy) and return all results in the next `ActionResult` request. Verify this works with a capture.
- **`existing_suggestions` field.** Cleared for BYOK first-turn (L325). The runtime should not need to produce this — it's a server-side feature for the suggestion UI. Confirm via capture.
- **Custom endpoint model name.** The proto `model_config.base` will be `custom/openai-compatible` — a placeholder, not the real model name. The runtime must resolve this to the actual model name from `RuntimeOptions.custom_endpoint.model_prefix` or a dedicated setting. Until this is resolved, custom endpoints only work if the provider ignores the model name (unlikely).

## Out of scope (explicit)

- Drive sync (notebooks, env vars, workflows).
- Session sharing / RTC live collaboration.
- Cloud Oz / ambient agent runs.
- Server-side memory and rules sync.
- Web search / web fetch (Warp server-implemented today). When we add these as in-runtime tools, they'll need their own search backends (Brave/Tavily).
- Login removal beyond the existing modal suppression. Login is still required for Warp subscription models.
- AWS Bedrock provider (requires SigV4 signing, separate credential flow — deferred past Stage C).

## Tracking

Use checkboxes in this file. When a stage is complete, update `HANDOFF.md` with the resume state and link back here.
