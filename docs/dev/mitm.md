# MITM observation of `app.warp.dev`

This guide is for developers working on `crates/byok_agent` (the in-process
BYOK agent runtime) who need to observe how Warp talks to `app.warp.dev`.
Captures are the reference spec for the `ResponseEvent` ordering, `ClientAction`
field masks, `StreamInit` field semantics, and tool-call shape we have to
reproduce locally.

> ⚠️ **Captures are sensitive.** They contain provider keys, prompts, file
> contents, cookies, and tokens. Do not commit them. The `.gitignore` and the
> `script/git/check-captures.sh` guard exist specifically to keep raw captures
> and secrets out of the repo. See `captures/README.md`.

## 1. Install mitmproxy

```bash
# Linux (recommended: pipx for an isolated install)
pipx install mitmproxy

# macOS
brew install mitmproxy
```

Verify:

```bash
mitmdump --version
```

You should see `Mitmproxy: 10.x` or newer.

## 2. Trust the mitmproxy CA

Run `mitmproxy` once in any mode to generate `~/.mitmproxy/mitmproxy-ca-cert.pem`.

### Linux

Two reliable options:

* **Per-process via `SSL_CERT_FILE`** (recommended for ad-hoc use):

  ```bash
  export SSL_CERT_FILE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"
  ```

  Note: this sets the *only* trust anchor for that process to the mitmproxy
  CA, which is fine for the Warp client during a capture session but breaks
  general TLS until you unset it. Open a fresh terminal for unrelated work.

* **System-wide** (Debian/Ubuntu):

  ```bash
  sudo cp ~/.mitmproxy/mitmproxy-ca-cert.pem \
      /usr/local/share/ca-certificates/mitmproxy.crt
  sudo update-ca-certificates
  ```

  On Arch / Fedora the path is different; consult your distro docs.

### macOS

```bash
sudo security add-trusted-cert \
    -d -r trustRoot \
    -k /Library/Keychains/System.keychain \
    ~/.mitmproxy/mitmproxy-ca-cert.pem
```

You will be prompted for your password.

## 3. Start a capture

Pick one:

* Web UI (good for inspecting responses interactively):

  ```bash
  mitmweb --listen-port 8080 --ssl-insecure
  ```

  Open <http://127.0.0.1:8081> for the inspector.

* Headless dump file (good for repro fixtures):

  ```bash
  mkdir -p captures
  mitmdump -w "captures/$(date -u +%Y%m%dT%H%M%SZ).flow" --listen-port 8080
  ```

* With the Warp-aware addon (recommended; see §5):

  ```bash
  mkdir -p captures
  mitmdump -s script/mitm/warp_addon.py \
      --listen-port 8080 \
      --set output_dir=captures
  ```

Raw flow files live under `captures/` and are gitignored. Do not move them
into `captures/redacted/`; that directory is for hand-scrubbed Markdown
summaries only.

## 4. Launch Warp through the proxy

In a separate terminal:

```bash
# Same shell will be used to run Warp:
export HTTPS_PROXY=http://127.0.0.1:8080
export HTTP_PROXY=http://127.0.0.1:8080
# Linux only — see §2 for caveats.
export SSL_CERT_FILE="$HOME/.mitmproxy/mitmproxy-ca-cert.pem"

# Build + open Warp OSS (see AGENTS.md for full bootstrap):
./script/run --dont-open
codesign --force --deep --options runtime --sign - \
    target/debug/bundle/osx/WarpOss.app \
    --entitlements script/Debug-Entitlements.plist     # macOS only
./target/debug/bundle/osx/WarpOss.app/Contents/MacOS/warp-oss
```

You should see `POST app.warp.dev/ai/multi-agent` decrypted in the inspector
when you exercise `/agent`.

If TLS fails to decrypt, double-check that:

* the env vars are set in the *same shell* that launches `warp-oss`,
* the mitmproxy CA is trusted by that process (Linux: `SSL_CERT_FILE`;
  macOS: System keychain),
* you are not pointing at the wrong proxy port.

## 5. Use the Warp-aware addon

`script/mitm/warp_addon.py` filters traffic to `app.warp.dev`, writes
per-flow artifacts into `captures/`, redacts sensitive headers in the
committed metadata view, and decodes `/ai/multi-agent` SSE bodies into
something a human can read.

It is a thin wrapper around `mitmproxy`'s addon API — see the script for
hooks, configurable `output_dir`, and which paths are skipped.

The addon depends on `warp-proto-apis` for protobuf decoding. The repo
pin is `rev = "78a78f21a75432bf0141e396fb318bf1694e47f0"` (see root
`Cargo.toml`). The addon is conservative: if the matching Python proto
modules are not on `PYTHONPATH` it falls back to writing the raw,
URL-safe-base64-decoded bytes. You can always finish decoding by hand or
with a small Rust helper.

## 6. Reference scenarios to capture

For Stage A exit criteria we need at least these scenarios. Record each
locally, then write a hand-scrubbed Markdown summary into
`captures/redacted/` and add a row to `captures/INDEX.md`:

1. Logged-in fresh `/agent`, text reply, no tools.
2. Logged-in `/agent` using shell, read files, grep, glob.
3. Logged-in `/agent` with apply diff.
4. Logged-in `/agent` using MCP if configured.
5. Logged-in continuation of an existing conversation.
6. Logged-in passive prompt suggestion request.
7. Logged-in model fetch / feature-model choices.
8. Logged-out BYOK request returning the current 400.
9. Discovery-only: conversation list / server metadata endpoints.

For each, the redacted summary must clearly call out:

* `StreamInit.conversation_id`, `StreamInit.run_id`, and
  `StreamInit.request_id` shape and semantics. We rely on the existing
  client storing these into `server_conversation_token`,
  `Conversation::task_id`, and `ServerOutputId`, respectively.
* Whether `CreateTask` is emitted on the first turn, and how the task id
  relates to `StreamInit.run_id`.
* Where `BeginTransaction` / `CommitTransaction` boundaries fall.
* The exact field-mask paths used by `AppendToMessageContent` and
  `AddMessagesToTask`.
* Tool-call message shape (function name, args, ids) and how tool
  results come back on the following request.

## 7. Cleaning up

Once you are done with a session:

```bash
unset HTTPS_PROXY HTTP_PROXY SSL_CERT_FILE
# macOS: optionally remove the trusted root.
# sudo security remove-trusted-cert -d ~/.mitmproxy/mitmproxy-ca-cert.pem
```

Raw `.flow` / `.mitm` / `.sse` / `*.decoded.json` files are local-only.
The repo guard will refuse them, and `.gitignore` keeps them out of
`git status`. If you ever want to share one with a teammate, reach for
`captures/redacted/` and write a sanitized summary instead — never copy
raw bytes around.
