#!/usr/bin/env bash
#
# check-captures.sh
#
# Diff-scoped capture-artifact guard. Used both as a pre-commit hook and in CI
# to keep raw MITM captures and provider secrets out of the repo while keeping
# the redacted catalog (captures/redacted/**, captures/README.md,
# captures/INDEX.md) committable.
#
# Design constraints (PLAN-byok-runtime-revised-v4.md, sections 4.2 and 11):
#
#   * Diff-scoped, never full-tree text grep. Legitimate mentions of "bearer",
#     "Authorization:", "session", and "cookie" in *.rs / *.md / *.toml must
#     not trip this check.
#   * Extension-anchored. We reject capture-artifact file extensions outright
#     (anywhere in the tree) and reject anything inside `captures/` that is
#     not in the redacted allowlist.
#   * Secret-token scanning is limited to staged diffs of files under
#     `captures/redacted/`. We never scan source-code files for secret-looking
#     strings.
#   * Reads the staged diff (pre-commit) or `<BASE>...HEAD` (CI) only. Untracked
#     working-tree noise is intentionally ignored.
#
# Modes:
#
#   pre-commit     — default. Inspects `--cached` diff against HEAD.
#   ci             — inspects `<BASE>...HEAD` diff. BASE defaults to
#                    origin/master; can be overridden with --base <ref>.
#
# Exit codes:
#
#   0   — no violations
#   1   — at least one violation found
#   2   — usage / environment error
#
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage:
  script/git/check-captures.sh [--mode pre-commit|ci] [--base <ref>]

Options:
  --mode pre-commit   Inspect `git diff --cached` against HEAD (default).
  --mode ci           Inspect `git diff <base>...HEAD`.
  --base <ref>        Base ref for --mode ci (default: origin/master, falling
                      back to the merge-base of HEAD against master).
  -h | --help         Show this message.

Notes:
  * Capture artifact extensions are denied anywhere in the tree:
        *.flow *.mitm *.request.bin *.response.bin
        *.request.pb *.response.pb *.sse
        *.decoded.pbtxt *.decoded.json
  * Inside captures/, only README.md, INDEX.md, and redacted/** are allowed.
  * Files under captures/redacted/ are scanned for common secret tokens in
    their staged diff content; source-code files are NOT scanned.
USAGE
}

mode="pre-commit"
base_ref=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --mode)
            mode="${2:-}"
            shift 2
            ;;
        --base)
            base_ref="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "check-captures: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ "$mode" != "pre-commit" && "$mode" != "ci" ]]; then
    echo "check-captures: --mode must be 'pre-commit' or 'ci' (got: $mode)" >&2
    exit 2
fi

# Resolve diff range.
diff_args=()
if [[ "$mode" == "pre-commit" ]]; then
    # Staged diff against HEAD.
    diff_args=(--cached)
else
    if [[ -z "$base_ref" ]]; then
        if git rev-parse --verify --quiet origin/master >/dev/null; then
            base_ref="origin/master"
        elif git rev-parse --verify --quiet master >/dev/null; then
            base_ref="$(git merge-base HEAD master 2>/dev/null || echo master)"
        else
            echo "check-captures: cannot determine base ref; pass --base <ref>." >&2
            exit 2
        fi
    fi
    diff_args=("${base_ref}...HEAD")
fi

# Files in the diff (added / copied / modified / renamed). Use NUL-delimited
# output so paths with spaces or shell-special characters round-trip safely.
mapfile -d '' -t changed_files < <(
    git diff --name-only --diff-filter=ACMR -z "${diff_args[@]}" -- || true
)

if [[ ${#changed_files[@]} -eq 0 ]]; then
    exit 0
fi

# Capture artifact extensions. Anchored at the end of the path with a literal
# dot; matched as suffix-of-basename, not anywhere in the path.
denied_exts_re='\.(flow|mitm|request\.bin|response\.bin|request\.pb|response\.pb|sse|decoded\.pbtxt|decoded\.json)$'

# Allowed paths inside `captures/`.
allowed_capture_paths_re='^captures/(README\.md|INDEX\.md|redacted/.*|redacted)$'

# Secret-token patterns to look for in the *staged diff* of files under
# captures/redacted/. These match additions only (the leading `+` in unified
# diff hunks). We deliberately keep this list small and high-signal so it
# almost never false-positives in the rare case someone hand-crafts a redacted
# fixture that legitimately mentions one of these strings.
#
# Each pattern is documented inline.
secret_patterns=(
    # HTTP Authorization header value.
    '^\+.*[Aa]uthorization:[[:space:]]*[A-Za-z0-9._~+/=-]'
    # Inline `Bearer <token>` with at least one non-whitespace character after.
    '^\+.*Bearer[[:space:]]+[A-Za-z0-9._~+/=-]'
    # OpenAI-style key prefix.
    '^\+.*\bsk-[A-Za-z0-9]{8,}'
    # Provider-key env var assignments with a value.
    '^\+.*\b(OPENAI|ANTHROPIC|GEMINI|OPENROUTER)_API_KEY=[A-Za-z0-9._~+/=-]'
    # Set-Cookie header (sets sensitive auth state).
    '^\+.*Set-Cookie:[[:space:]]*[A-Za-z0-9_=-]'
)

violations=()

for path in "${changed_files[@]}"; do
    [[ -z "$path" ]] && continue

    is_under_redacted=0
    case "$path" in
        captures/redacted/*) is_under_redacted=1 ;;
    esac

    is_under_captures=0
    case "$path" in
        captures/*) is_under_captures=1 ;;
    esac

    # 1. Capture-artifact extensions are denied unless they live under
    #    captures/redacted/. (We allow them under redacted/ on the assumption
    #    that someone deliberately scrubbed them; the secret-token guard below
    #    will still examine their content.)
    if [[ "$path" =~ $denied_exts_re ]] && [[ $is_under_redacted -eq 0 ]]; then
        violations+=("denied capture-artifact extension: $path")
        continue
    fi

    # 2. Anything else under captures/ that isn't redacted/, README.md, or
    #    INDEX.md is rejected outright.
    if [[ $is_under_captures -eq 1 ]] && [[ ! "$path" =~ $allowed_capture_paths_re ]]; then
        violations+=("file under captures/ is not in the redacted allowlist: $path")
        continue
    fi

    # 3. For files under captures/redacted/, scan their staged diff for
    #    secret-looking additions. We never scan source-code paths.
    if [[ $is_under_redacted -eq 1 ]]; then
        # Diff for this single file. Use -- to terminate options safely.
        if ! diff_text="$(git diff "${diff_args[@]}" -- "$path" 2>/dev/null)"; then
            continue
        fi
        [[ -z "$diff_text" ]] && continue

        for pattern in "${secret_patterns[@]}"; do
            if grep -E -q "$pattern" <<<"$diff_text"; then
                violations+=("possible secret in staged diff of $path (pattern: ${pattern})")
                break
            fi
        done
    fi
done

if [[ ${#violations[@]} -gt 0 ]]; then
    {
        echo "check-captures: refusing diff."
        echo
        for v in "${violations[@]}"; do
            echo "  - $v"
        done
        echo
        echo "If a finding is a false positive, fix the rule in"
        echo "script/git/check-captures.sh; do not relax .gitignore."
    } >&2
    exit 1
fi

exit 0
