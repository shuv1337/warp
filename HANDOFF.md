# HANDOFF

## Objective
- Make the Warp fork work out of the box without login for BYOK AI flows.
- Login should only be required for Warp subscription AI models.
- Specifically: OpenAI BYOK should expose/select `GPT-5.5` and `GPT-5.4 Mini`, and `/agent` should run logged out using the user’s OpenAI key.

## Current status
- Build/run works locally on Linux via `./script/run` (`warp-oss`, `gui`).
- BYOK OpenAI models now appear in the Default BYOK Model settings dropdown and in `/models`.
- `/models` selection now sticks for BYOK models; active model chip shows `GPT-5.5 - OpenAI`.
- Login-token error was fixed: logged-out BYOK requests no longer fail with `Attempted to retrieve access token when user is logged out`.
- Next blocker: `/agent tell me about this project` with `GPT-5.5 - OpenAI` still fails server-side:
  - `MultiAgent request failed after 0 retries: Failed with status code 400 Bad Request: {"error":"Something went wrong with this conversation. Please try starting a new conversation."}`

## Key context
- User wants **all login requirement surfaces removed**, except access to Warp subscription AI models.
- Do not add login modals or require explicit account creation for BYOK.
- A silent anonymous auth context was added because the backend returned `401 Unauthorized: User not in context` when requests were sent with no bearer token.
- Changing silent anonymous user type from `NativeClientAnonymousUserFeatureGated` to `NativeClientAnonymousUser` did **not** resolve the final 400.
- Clearing local stale conversation/task context for BYOK requests without a server conversation token also did **not** resolve the final 400.
- Likely next suspects: backend model ID/provider validation for `gpt-5.5`, request payload shape for BYOK model routing, or persisted failed conversation state still being attached somewhere outside `RequestParams`.

## Changed artifacts
- `crates/ai/src/provider_registry.rs` — added OpenAI BYOK models `gpt-5.5` / `gpt-5.4-mini`.
- `app/src/terminal/input/models/data_source.rs` — injects authenticated BYOK registry models into `/models`, marks rows with `BYOK`/`Warp` chips, uses key icon/details for BYOK.
- `app/src/ai/llms.rs` — added local BYOK `LLMInfo` lookup and active-model resolution for IDs not present in server model lists.
- `app/src/server/server_api.rs` — logged-out BYOK requests create/use silent anonymous auth context; request auth gating changed.
- `app/src/ai/agent/api.rs` — BYOK request params clear stale tasks/fork metadata when no server conversation token exists.
- `app/src/auth/auth_manager.rs` / `app/src/auth/auth_view_modal.rs` — central login-gated modal emission suppressed for logged-out users.
- `app/src/auth/auth_state.rs` — setters loosened to `pub(crate)` for silent anonymous context storage.

## Important files
- `app/src/server/server_api.rs` — `generate_multi_agent_output`, anonymous auth context, request auth logic.
- `app/src/ai/agent/api.rs` — builds `RequestParams`, model IDs, API keys, tasks, conversation tokens.
- `app/src/ai/agent/api/impl.rs` — converts `RequestParams` into `warp_multi_agent_api::Request`.
- `app/src/ai/agent/api/convert_to.rs` — input conversion; inspect if payload type/context is invalid for fresh BYOK requests.
- `crates/ai/src/provider_registry.rs` — source of BYOK provider model IDs.

## Validation
- Repeatedly ran `cargo fmt` and `cargo check -p warp --bin warp-oss --features gui`; latest checks passed.
- Relaunched multiple times with `./script/run` using `interactive_shell` sessions.
- Observed progression of errors:
  1. access token missing while logged out
  2. 401 user not in context
  3. current persistent 400 conversation error

## Next steps
1. Add temporary structured logging before `server_api.generate_multi_agent_output` to print sanitized request metadata: model_config IDs, which API key fields are non-empty, conversation_id/forked_from, task count, input type, anonymous user type/principal if available. Do not log key values.
2. Compare payload for a successful Warp subscription model request vs BYOK request, especially `model_config.base`, `api_keys`, `tasks`, and `metadata.conversation_id`.
3. Try alternate model IDs if backend expects current OpenAI names differently (e.g. `gpt-5-5`, `gpt-5.5-chat`, or current server-known IDs). The 400 may be backend rejecting unknown BYOK model ID.
4. If backend requires a full account despite anonymous context, identify the exact endpoint/server constraint and decide whether a different endpoint/path is needed for local BYOK.

## Risks / open questions
- Backend error is generic; client logs do not currently expose the rejected request shape.
- We changed auth state from `ServerApi` directly, bypassing some `AuthManager::on_user_fetched` side effects; this may be acceptable for silent BYOK but needs review.
- The user strongly prefers no login surfaces; avoid reintroducing auth modals as a “fix.”

## Resume prompt
Pick up by instrumenting the BYOK `/ai/multi-agent` request payload around `app/src/server/server_api.rs::generate_multi_agent_output` and `app/src/ai/agent/api/impl.rs`, then reproduce `/agent tell me about this project` with `GPT-5.5 - OpenAI` while logged out to isolate why the backend returns the generic 400.
