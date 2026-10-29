#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

for command in python3 cargo; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "Missing required command: $command" >&2
    exit 1
  }
done

python3 - <<'PY'
from pathlib import Path

source = Path("src-tauri/src/task_integrity/gateway.rs").read_text()

def function_body(name: str) -> str:
    marker = f"fn {name}"
    start = source.find(marker)
    if start < 0:
        raise SystemExit(f"Missing Task Integrity hot-path function: {name}")
    brace = source.find("{", start)
    if brace < 0:
        raise SystemExit(f"Could not parse Task Integrity hot-path function: {name}")
    depth = 0
    for index in range(brace, len(source)):
        char = source[index]
        if char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return source[start:index + 1]
    raise SystemExit(f"Unterminated Task Integrity hot-path function: {name}")

hot_path = "\n".join(
    function_body(name)
    for name in (
        "authorize_mutation_for_focus",
        "authorize_mutation_with_loader",
        "authorize_workspace_mutation_with_loader",
        "authorize_mutation",
        "authorize_workspace_mutation",
    )
)
for forbidden in (
    "project_index::",
    "monitoring::scan_workspace",
    "fs::read_dir",
    "WalkDir",
    "git::",
    "git_status",
):
    if forbidden in hot_path:
        raise SystemExit(
            f"Task Integrity authorization hot path unexpectedly references {forbidden!r}"
        )

print("Task Integrity static hot-path invariant passed.")
PY

cargo test --locked --manifest-path src-tauri/Cargo.toml task15_ -- --test-threads=1 --nocapture
cargo test --locked --manifest-path src-tauri/Cargo.toml task8_verification_setup_does_not_create_or_copy_private_source_view -- --test-threads=1
cargo test --locked --manifest-path src-tauri/Cargo.toml task11_bounded_reconciliation_checks_only_touched_generations_and_reports_stale_paths -- --test-threads=1
cargo test --locked --manifest-path src-tauri/Cargo.toml task11_drift_core_and_mutation_gateway_do_not_call_periodic_monitoring_scan -- --test-threads=1

if [[ "$(uname -s)" == "Linux" ]]; then
  cargo test --locked --manifest-path src-tauri/Cargo.toml task11_linux_event_watcher_reports_change_and_idle_does_not_rescan_project -- --test-threads=1
fi

echo "Task Integrity performance/mutation-surface invariants passed."
