#!/usr/bin/env bash
#
# Opt-in installer for the repo-managed git hooks under script/git/hooks/.
#
# Sets `core.hooksPath` for this checkout to `script/git/hooks` so the hooks
# are versioned alongside the rest of the tree. Safe to re-run.
#
# Note: this only applies to your local clone. CI enforces the same checks
# independently in `.github/workflows/ci.yml`.
#
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

hooks_dir="script/git/hooks"
if [[ ! -d "$hooks_dir" ]]; then
    echo "install-hooks: $hooks_dir not found; aborting." >&2
    exit 1
fi

# Make sure each hook is executable; some checkouts (e.g. zip downloads, certain
# Windows shells) drop the +x bit.
find "$hooks_dir" -maxdepth 1 -type f -print0 |
    xargs -0 chmod +x

git config core.hooksPath "$hooks_dir"

echo "installed git hooks: core.hooksPath=$hooks_dir"
echo
echo "active hooks:"
ls -1 "$hooks_dir" | sed 's/^/  /'
