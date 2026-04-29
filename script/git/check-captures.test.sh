#!/usr/bin/env bash
#
# Self-test for script/git/check-captures.sh.
#
# Spins up an isolated, throwaway git repo in a temp directory, plants
# fixtures that exercise each validation case from PLAN-byok-runtime-revised-v4
# section 4.2, runs the guard, and asserts pass/fail.
#
# Cases:
#   1. Clean tree (no staged changes)              -> pass (exit 0)
#   2. Staged captures/foo.flow                    -> fail (exit 1)
#   3. Staged captures/redacted/foo.json with
#      "Authorization: Bearer xxx"                 -> fail (exit 1)
#   4. Staged app/src/server/server_api.rs with a
#      legitimate `bearer_auth` mention            -> pass (exit 0)
#   5. Staged captures/notes.txt                   -> fail (exit 1)
#   6. Staged captures/redacted/foo.md (clean)     -> pass (exit 0)
#
# Exit 0 if every case behaves as expected, 1 otherwise. Output is verbose so
# failures are easy to triage.
#
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
guard="${repo_root}/script/git/check-captures.sh"

if [[ ! -x "$guard" ]]; then
    echo "guard not executable: $guard" >&2
    exit 2
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

# Copy the guard into the throwaway repo so the script's own path lookups
# (`git rev-parse --show-toplevel`) resolve to the temp repo.
mkdir -p "$tmp_dir/script/git"
cp "$guard" "$tmp_dir/script/git/check-captures.sh"
chmod +x "$tmp_dir/script/git/check-captures.sh"

cd "$tmp_dir"
git init -q -b master
git config user.email "test@example.com"
git config user.name "test"
# Need an initial commit so `git diff --cached` has a HEAD.
mkdir -p captures/redacted
echo "# captures README" > captures/README.md
echo "# capture index" > captures/INDEX.md
git add captures/README.md captures/INDEX.md script/git/check-captures.sh
git commit -q -m "init"

failed_cases=()

run_case() {
    local name="$1"
    local expected_status="$2"
    shift 2

    # Reset staging area between cases.
    git reset -q

    # Run the case body, which is responsible for staging whatever it wants.
    "$@"

    local actual_status=0
    ./script/git/check-captures.sh --mode pre-commit >/tmp/check-captures.out 2>&1 || actual_status=$?

    if [[ "$actual_status" -eq "$expected_status" ]]; then
        printf '  [PASS] %s (exit=%d)\n' "$name" "$actual_status"
    else
        printf '  [FAIL] %s (expected=%d, actual=%d)\n' \
            "$name" "$expected_status" "$actual_status"
        echo "---- guard output ----"
        sed 's/^/    /' /tmp/check-captures.out
        echo "----------------------"
        failed_cases+=("$name")
    fi

    # Roll back any working-tree changes the case made.
    git reset --hard -q HEAD
    git clean -fdq
}

case_clean() {
    : # nothing staged
}

case_flow_artifact() {
    mkdir -p captures
    echo "raw flow bytes" > captures/foo.flow
    git add -f captures/foo.flow
}

case_secret_in_redacted_json() {
    mkdir -p captures/redacted
    cat > captures/redacted/foo.json <<'EOF'
{
  "headers": {
    "Authorization": "Bearer sk-thisIsTooLongAndShouldFire"
  }
}
EOF
    git add captures/redacted/foo.json
}

case_legitimate_bearer_auth_in_rs() {
    mkdir -p app/src/server
    cat > app/src/server/server_api.rs <<'EOF'
// Legitimate use of bearer_auth in Rust code. The guard MUST NOT flag this.
fn build_request(client: &Client, token: &str) {
    let _ = client.post("https://example.com/v1/x").bearer_auth(token);
}
EOF
    git add app/src/server/server_api.rs
}

case_loose_file_in_captures() {
    mkdir -p captures
    echo "scratch notes" > captures/notes.txt
    git add -f captures/notes.txt
}

case_clean_redacted_summary() {
    mkdir -p captures/redacted
    cat > captures/redacted/2026-04-29-fresh-agent.md <<'EOF'
# Redacted summary

* Scenario: logged-in fresh /agent text reply
* Endpoint: POST /ai/multi-agent
* All headers, prompts, and bodies have been scrubbed.
EOF
    git add captures/redacted/2026-04-29-fresh-agent.md
}

echo "Running check-captures self-tests..."
run_case "clean tree passes"                    0 case_clean
run_case "planted *.flow fails"                 1 case_flow_artifact
run_case "secret in captures/redacted/ fails"   1 case_secret_in_redacted_json
run_case "legitimate bearer_auth in *.rs ok"    0 case_legitimate_bearer_auth_in_rs
run_case "loose file under captures/ fails"     1 case_loose_file_in_captures
run_case "clean redacted summary passes"        0 case_clean_redacted_summary

echo
if [[ ${#failed_cases[@]} -gt 0 ]]; then
    echo "FAILED:"
    for c in "${failed_cases[@]}"; do
        echo "  - $c"
    done
    exit 1
fi

echo "All check-captures self-tests passed."
