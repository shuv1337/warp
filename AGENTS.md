# AGENTS.md — warp (shuv1337/warp fork)

Project-local guidance for coding agents working in this repo. Keep this file
honest and up-to-date; it's the handoff to whoever (human or agent) shows up next.

## What this repo is

- Fork of [`warpdotdev/warp`](https://github.com/warpdotdev/warp) — Warp, an
  agentic terminal / development environment.
- Primarily a **Rust workspace** (workspace `Cargo.toml` at root,
  app crate at `app/`, ~hundreds of crates under `crates/`).
- macOS-first; Linux/Windows scripts exist but this checkout was bootstrapped
  on **macOS Apple Silicon** only.
- Licensing: `crates/warpui` and `crates/warpui_core` are MIT; the rest is AGPL v3.
- Channels: `oss` (binary `warp-oss`) vs `local` (binary `warp`, requires
  internal `warp-channel-config` — not available without repo access).

## Repo layout (high level)

- `app/` — main application crate; produces `warp` and `warp-oss` bins.
- `crates/warpui*` — UI framework (MIT-licensed). Has its own `build.rs` that
  compiles Metal shaders.
- `crates/...` — feature crates (terminal, editor, ai, mcp, vim, completer,
  isolation_platform, server_client, etc.).
- `script/` — cross-platform build/run/release helpers. Cross-platform entry
  points (`bootstrap`, `run`, `presubmit`, `bundle`) dispatch to
  `script/{macos,linux,windows}/`.
- `script/run` — high-level run script; on macOS calls `script/macos/run`
  which invokes `cargo bundle` and produces a real `.app`.
- `.warp/`, `.cargo/`, `.config/`, `.agents/`, `.claude/` — committed tooling/config.

## Bootstrap recipe (macOS, minimal local build)

This is the **verified** path that produced a working `WarpOss.app` on this
machine. The official `./script/bootstrap` does more (docker, gcloud,
powershell, etc.) — most of that is **not needed** to just build and run.

Prereqs that must be in place:

1. **Full Xcode.app** at `/Applications/Xcode.app` (not just Command Line Tools).
   - `xcrun -find metal` and `xcrun -find metallib` must resolve.
   - `xcrun -find actool` is also required (for icon compilation, gracefully
     skipped today because the OSS channel ships no `.icon` bundle).
2. **git-lfs** — the upstream repo uses Git LFS. **Without it, `git clone`
   succeeds but the working-tree checkout fails** (`git-lfs filter-process:
   git-lfs: command not found ... fatal: the remote end hung up unexpectedly`).
   - Fix: `brew install git-lfs && git lfs install` then re-checkout
     (`git reset --hard HEAD` or re-clone).
3. **Rust toolchain** via rustup (cargo on PATH).

Step-by-step (what we ran successfully):

```bash
# 1. Clone (after git-lfs is installed)
cd ~/repos
git clone https://github.com/shuv1337/warp.git
cd warp

# 2. Brew deps (subset of bootstrap — skip docker/gcloud/powershell)
brew install jq clang-format create-dmg multitime pkgconf llvm \
             getsentry/tools/sentry-cli

# 3. Rust target
rustup target add aarch64-apple-darwin

# 4. Xcode setup
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
sudo xcodebuild -license accept
xcodebuild -runFirstLaunch

# 5. Metal toolchain (~688 MB). REQUIRED — crates/warpui/build.rs invokes
#    `xcrun metal` and `xcrun -sdk macosx metallib` to compile shaders.metal
#    into shaders.metallib. Without this the build fails inside warpui.
xcodebuild -downloadComponent MetalToolchain

# 6. cargo-bundle fork (the upstream one doesn't support --profile, which the
#    macOS run script uses). The exact pin from script/macos/bootstrap:
cargo install cargo-bundle \
  --git https://github.com/burtonageo/cargo-bundle \
  --rev ae4c76e92c08774bf54ff077b1c52e3d1cd6c16d

# 7. Pre-warm crates (optional)
cargo fetch

# 8. Build + bundle the OSS channel app (no internal warp-channel-config)
./script/run --dont-open
# Produces target/debug/bundle/osx/WarpOss.app
# First build is ~3-4 min on M-series.
```

### Codesigning gotcha

`script/macos/run` picks the first `Apple Development` cert from
`security find-identity` and signs with it. On this machine that cert was
created via API and returns `errSecInternalComponent`, breaking the script's
final step. **The bundle is fine** — re-sign ad-hoc and run:

```bash
codesign --force --deep --options runtime --sign - \
  target/debug/bundle/osx/WarpOss.app \
  --entitlements script/Debug-Entitlements.plist

open target/debug/bundle/osx/WarpOss.app
# or:
./target/debug/bundle/osx/WarpOss.app/Contents/MacOS/warp-oss
```

If you have a working Apple Development cert, the script will use it
automatically. The script falls back to ad-hoc (`--sign -`) on its own when no
matching identity is present, but it does **not** fall back when an identity
is present and signing fails — that's why we re-sign manually above.

### Channel detection

`script/run` checks for `warp-channel-config` on PATH. Without it (the OSS
case) it builds the `warp-oss` binary and the `WarpOss.app` bundle. With it,
it builds `warp` / `WarpLocal.app`. We're in the OSS case here.

## Running

- Foreground: `./target/debug/bundle/osx/WarpOss.app/Contents/MacOS/warp-oss`
- Like a real app: `open ./target/debug/bundle/osx/WarpOss.app`
- Logs: `~/Library/Logs/warp-oss.log` (channel-dependent: `warp-oss.log` for
  OSS, `warp_local.log` for local).
- App data: `~/Library/Application Support/dev.warp.WarpOss/`.

## Testing / presubmit (not yet set up here)

Not required for a minimal build. To enable:

```bash
./script/install_cargo_test_deps     # cargo-binstall, wgslfmt, cargo-nextest
./script/install_cargo_release_deps  # cargo-about (for license attribution)
./script/presubmit                   # fmt + clippy + tests
```

`./script/install_cargo_build_deps` will also try to install `diesel_cli` (skip
on CI; needed only for local DB schema work) and the internal channel config
(harmless skip for OSS contributors).

## Known skipped / deferred items

The following bootstrap steps were **intentionally skipped** because they
aren't needed for a minimal macOS build/run. Re-add as needed:

- `brew install --cask docker` — required by some integration test paths.
- `brew install google-cloud-sdk` + `gcloud auth login` — only relevant to
  release/build pipelines that pull from internal GCS.
- `brew install powershell` + `Install-Module PSScriptAnalyzer` — only used by
  PowerShell linting (`script/lint_powershell`).
- `cargo-binstall`, `cargo-nextest`, `wgslfmt`, `cargo-about`, `diesel_cli`.

## Operational notes / gotchas

- **First build is slow** (~3–4 min cold on M-series, much longer with cold
  cargo cache). `cargo fetch` ahead of time helps.
- **Metal toolchain is mandatory** even for a debug build. There's no fallback
  path that skips shader compilation.
- **`xcrun -find metallib` may report failure right after Xcode install** even
  though the binary is present. Running `xcodebuild -downloadComponent
  MetalToolchain` resolves it; afterwards `metal`/`metallib` resolve under
  `/var/run/com.apple.security.cryptexd/.../Metal.xctoolchain/usr/bin/`.
- **`cargo-bundle` must be the burtonageo fork at the pinned rev** — the
  release script passes `--profile`, which crates.io's cargo-bundle rejects.
- **No `actool` icon error**: you'll see
  `Warning: no .icon bundle found for oss channel` — ignore. The OSS channel
  has no `.icon` bundle; the script proceeds.
- **Telemetry**: per global telemetry-first standard, but Warp has its own
  established telemetry stack (Sentry framework conditionally bundled when
  `cocoa_sentry` feature is on, and channel-config-driven telemetry config).
  Do not bolt on Maple ingest here — work within the existing channel/telemetry
  config plumbing.

## When upstream changes

Watch for changes that could invalidate this recipe:

- `script/macos/bootstrap` — new prereqs.
- `script/macos/install_build_deps` — Metal toolchain step or new components.
- `crates/warpui/build.rs` — shader compile pipeline.
- `script/macos/run` and `app/Cargo.toml` — bundle/profile/feature changes.
- The `cargo-bundle` rev pin in `script/macos/bootstrap`.

If any of those change, update the steps above (and this section).
