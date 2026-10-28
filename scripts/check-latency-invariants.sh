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

python3 src-tauri/tests/helper_serve_protocol_test.py

pushd src-tauri >/dev/null
cargo test --locked latency::tests:: -- --nocapture
cargo test --locked persistent_helper_ -- --nocapture
cargo test --locked workspace_cache_loads_source_once_and_returns_clones -- --nocapture
cargo test --locked permission_store_cache_loads_source_once_and_returns_clones -- --nocapture
cargo test --locked failed_workspace_save_does_not_publish_new_cache_state -- --nocapture
cargo test --locked failed_permission_save_does_not_publish_new_cache_state -- --nocapture
cargo test --locked fresh_semantic_scope_skips_fallback_and_wrong_scope_forces_it -- --nocapture
cargo test --locked semantic_observation_scope_is_bound_to_workspace_session_app_and_snapshot -- --nocapture
cargo test --locked task9_semantic_hot_path_avoids_task_integrity_and_storage_lookup -- --nocapture
cargo test --locked streaming_fingerprint_matches_legacy_json_hash -- --nocapture
cargo test --locked transaction_timings_are_fixed_numeric_redacted_fields -- --nocapture
cargo test --locked success_envelope_serialization_matches_existing_json_shape -- --nocapture
cargo test --locked mission::events::tests::cursor_read_does_not_parse_full_history -- --nocapture
cargo test --locked mission::scale_tests::task15_scale_matrix_keeps_status_heartbeat_scheduler_and_resource_admission_bounded -- --nocapture
popd >/dev/null

if grep -Eq 'project_index::|ProjectIndex|search_project|scan_repository' \
  src-tauri/src/mission/agents.rs \
  src-tauri/src/mission/model.rs \
  src-tauri/src/mission/scheduler.rs \
  src-tauri/src/mission/context.rs; then
  echo "Mission heartbeat/status/scheduler hot paths must not scan the project index or repository." >&2
  exit 1
fi

if ! grep -q 'ready_task_ids()' src-tauri/src/mission/scheduler.rs; then
  echo "Mission scheduler must consume the incremental ready-task projection." >&2
  exit 1
fi

if grep -Eq 'thread::sleep|tokio::time::sleep|tokio::time::interval' src-tauri/src/mission/*.rs; then
  echo "Mission Control must not introduce idle sleep/poll loops." >&2
  exit 1
fi

if [[ "${1:-}" == "--sample" ]]; then
  python3 src-tauri/tests/helper_latency_sample.py
fi

echo "Latency invariants passed."
