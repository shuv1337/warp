# MITM captures

This directory holds documentation about MITM packet captures of `app.warp.dev`
traffic, used as the reference spec while building the in-process BYOK agent
runtime (`crates/byok_agent`).

## Rules of the road

1. **Do not commit raw captures.** `.gitignore` blocks `*.flow`, `*.mitm`,
   `*.request.bin`, `*.response.bin`, `*.request.pb`, `*.response.pb`, `*.sse`,
   `*.decoded.pbtxt`, and `*.decoded.json`, plus the entire top of `captures/`
   except the redacted catalog. Treat raw captures as if they contained your
   provider keys, your shell history, and the contents of your home directory —
   because they often do.
2. **Only `captures/redacted/**`, `captures/INDEX.md`, and this `README.md` are
   allowed in version control.** Everything in `captures/redacted/` must be
   hand-scrubbed before being staged. The pre-commit / CI guard
   (`script/git/check-captures.sh`) re-checks staged diffs for common secret
   patterns.
3. **Raw captures live next to this directory but outside `captures/redacted/`.**
   For example: `captures/2026-04-29-fresh-agent.flow` is allowed on disk but
   blocked from being committed; `captures/redacted/2026-04-29-fresh-agent.md`
   is the committed summary.
4. **If the guard ever fires on legitimate code,** fix the guard, not the
   `.gitignore`. The guard is intentionally diff-scoped and extension-anchored
   so that legitimate mentions of `bearer_auth` or `Authorization:` in `*.rs`
   and `*.md` files do not trip it.

## What goes where

- `captures/INDEX.md` — registry of redacted summaries. One row per scenario.
- `captures/redacted/` — Markdown summaries plus any structurally redacted JSON
  / pbtxt fragments. No raw bodies, no real keys, no real prompt text, no real
  file contents, no real tool args, no cookies.
- Anything else inside `captures/` is for local use only.

See `docs/dev/mitm.md` for setup and `script/mitm/warp_addon.py` for the
capture addon.
