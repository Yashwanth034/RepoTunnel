//! Read-only, conservative test selection for changed approved-workspace files.
//! A recommendation is not an exhaustive Rust/JS dependency graph and never
//! grants permission to skip a release gate or claim Completion Proof.
use crate::{access::canonical_workspace_root, git, models::Workspace};
use serde::Serialize;
use std::{collections::BTreeSet, fs, path::Path};

const MAX_CHANGED_PATHS: usize = 256;
const MAX_TARGETED_COMMANDS: usize = 8;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImpactCommand {
    pub(crate) command: String,
    pub(crate) why: String,
    pub(crate) changed_paths: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TestImpactPlan {
    pub(crate) workspace_id: String,
    pub(crate) head: Option<String>,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) targeted: Vec<ImpactCommand>,
    pub(crate) final_checks: Vec<ImpactCommand>,
    pub(crate) unclassified_paths: Vec<String>,
    pub(crate) high_risk: bool,
    pub(crate) requires_full_rust_regression: bool,
    pub(crate) limitations: Vec<String>,
    pub(crate) coverage: &'static str,
}

const RUST_FULL: &str =
    "cargo test --locked --manifest-path src-tauri/Cargo.toml --lib -- --test-threads=4";
const RUST_CLIPPY: &str =
    "cargo clippy --locked --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings";

fn source_module(relative: &str) -> Option<String> {
    let path = relative.strip_prefix("src-tauri/src/")?;
    let stem = path.strip_suffix(".rs")?;
    if matches!(stem, "lib" | "main") {
        return None;
    }
    let module = stem.strip_suffix("/mod").unwrap_or(stem);
    if module.is_empty()
        || !module.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
    {
        return None;
    }
    Some(module.replace('/', "::"))
}
fn critical_rust_path(path: &str) -> bool {
    const CORE: &[&str] = &[
        "access",
        "git",
        "gateway",
        "mcp_server",
        "models",
        "changes",
        "storage",
        "commands",
        "lib",
        "main",
        "platform_sandbox",
        "terminal",
        "workflow",
        "security",
        "task_integrity",
        "mission",
        "team",
        "mcp_auth",
        "direct_https",
        "recovery_store",
        "completion_proof",
    ];
    let module = source_module(path);
    module.as_ref().is_none_or(|name| {
        CORE.iter()
            .any(|core| name == core || name.starts_with(&format!("{core}::")))
    })
}
fn read_small_project_text(root: &Path, path: &Path, max_bytes: u64) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return None;
    }
    // Never follow a linked source/manifest beyond the approved project root.
    // Use the resolved path for reading, not the potentially retargetable leaf.
    let root = root.canonicalize().ok()?;
    let resolved = path.canonicalize().ok()?;
    if !resolved.starts_with(&root) {
        return None;
    }
    fs::read_to_string(resolved).ok()
}
fn rust_module_registered(root: &Path, module: &str) -> bool {
    let parts = module.split("::").collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        let parent_file = if index == 0 {
            root.join("src-tauri/src/lib.rs")
        } else {
            let parent = parts[..index].join("/");
            let as_file = root.join(format!("src-tauri/src/{parent}.rs"));
            if as_file.is_file() {
                as_file
            } else {
                root.join(format!("src-tauri/src/{parent}/mod.rs"))
            }
        };
        let Some(contents) = read_small_project_text(root, &parent_file, 2 * 1024 * 1024) else {
            return false;
        };
        if !contents.contains(&format!("mod {part};"))
            && !contents.contains(&format!("mod {part} {{"))
        {
            return false;
        }
    }
    true
}
fn module_has_tests(root: &Path, path: &str) -> bool {
    // This is an availability hint, not proof that the test cases are relevant.
    // Keep test planning cheap even for unexpectedly huge or binary source.
    let Some(contents) = read_small_project_text(root, &root.join(path), 2 * 1024 * 1024) else {
        return false;
    };
    contents.contains("#[cfg(test)]")
        && contents.contains("mod tests")
        && (contents.contains("#[test]")
            || contents.contains("#[tokio::test]")
            || contents.contains("#[rstest]"))
}
fn node_scripts(root: &Path) -> BTreeSet<String> {
    let Some(contents) = read_small_project_text(root, &root.join("package.json"), 256 * 1024)
    else {
        return BTreeSet::new();
    };
    serde_json::from_str::<serde_json::Value>(&contents)
        .ok()
        .and_then(|package| {
            package
                .get("scripts")
                .and_then(|value| value.as_object())
                .cloned()
        })
        .map(|scripts| scripts.into_iter().map(|(name, _)| name).collect())
        .unwrap_or_default()
}
fn insert(commands: &mut Vec<ImpactCommand>, command: &str, why: &str, changes: &[String]) {
    if let Some(previous) = commands.iter_mut().find(|item| item.command == command) {
        for path in changes {
            if !previous.changed_paths.contains(path) {
                previous.changed_paths.push(path.clone());
            }
        }
    } else {
        commands.push(ImpactCommand {
            command: command.into(),
            why: why.into(),
            changed_paths: changes.to_vec(),
        });
    }
}
fn analyze_paths(
    workspace_id: &str,
    head: Option<String>,
    paths: Vec<String>,
    root: &Path,
) -> TestImpactPlan {
    let scripts = node_scripts(root);
    let mut targeted = Vec::new();
    let mut final_checks = Vec::new();
    let mut unclassified = Vec::new();
    let mut notes = vec![
        "This is a conservative change-path/test-name heuristic, NOT a complete transitive import or runtime dependency analysis.".into(),
        "The recommendations are not executed. Inspect the user's acceptance criteria and run broader checks for shared APIs, releases, or uncertain impacts.".into(),
    ];
    let mut rust = Vec::new();
    let mut rust_all = false;
    let mut rust_critical = false;
    let mut frontend = Vec::new();
    let mut browser = Vec::new();
    let mut python = Vec::new();
    let mut infra = Vec::new();
    let mut docs = Vec::new();
    for path in &paths {
        if path.starts_with("src-tauri/src/") && path.ends_with(".rs") {
            rust.push(path.clone());
            rust_critical |= critical_rust_path(path);
        } else if path.starts_with("src-tauri/tests/") && path.ends_with(".rs") {
            rust.push(path.clone());
        } else if path == "src-tauri/Cargo.toml"
            || path == "src-tauri/build.rs"
            || path == "Cargo.lock"
            || path.starts_with("src-tauri/.cargo/")
            || path.starts_with(".cargo/")
            || path == "rust-toolchain.toml"
        {
            rust_all = true;
            infra.push(path.clone());
        } else if path.starts_with("src-tauri/tests/")
            && (path.ends_with(".cjs") || path.ends_with(".mjs") || path.ends_with(".js"))
        {
            browser.push(path.clone());
        } else if path.ends_with(".py")
            && (path.starts_with("src-tauri/resources/")
                || path.starts_with("tests/")
                || path.starts_with("scripts/"))
        {
            python.push(path.clone());
        } else if path.starts_with("src/")
            || path.starts_with("frontend/")
            || path.starts_with("web/")
            || path.starts_with("client/")
            || (path.starts_with("tests/") && (path.ends_with(".ts") || path.ends_with(".tsx")))
        {
            frontend.push(path.clone());
        } else if matches!(
            path.as_str(),
            "package.json"
                | "package-lock.json"
                | "pnpm-lock.yaml"
                | "yarn.lock"
                | "tsconfig.json"
                | "vite.config.ts"
                | "vitest.config.ts"
                | "src-tauri/tauri.conf.json"
        ) || path.starts_with(".github/")
            || path.starts_with("scripts/")
        {
            infra.push(path.clone());
        } else if path.starts_with("docs/") || path.ends_with(".md") {
            docs.push(path.clone());
        } else {
            unclassified.push(path.clone());
        }
    }
    let has_rust = !rust.is_empty() || rust_all;
    let rust_infra = rust_all
        || infra.iter().any(|path| {
            path.starts_with(".github/")
                || path.starts_with("scripts/")
                || path == "src-tauri/tauri.conf.json"
        });
    let has_node = !frontend.is_empty()
        || infra.iter().any(|p| {
            matches!(
                p.as_str(),
                "package.json"
                    | "package-lock.json"
                    | "pnpm-lock.yaml"
                    | "yarn.lock"
                    | "tsconfig.json"
                    | "vite.config.ts"
                    | "vitest.config.ts"
            )
        });
    let mut missing_rust_tests = Vec::new();
    if has_rust {
        for path in &rust {
            if let Some(module) = source_module(path) {
                if rust_module_registered(root, &module) && module_has_tests(root, path) {
                    insert(
                        &mut targeted,
                        &format!("cargo test --locked --manifest-path src-tauri/Cargo.toml --lib '{module}::tests::' -- --test-threads=2"),
                        "Direct tests in a changed Rust module; does not cover reverse dependencies.",
                        std::slice::from_ref(path),
                    );
                } else {
                    missing_rust_tests.push(path.clone());
                }
            } else if let Some(filename) = path
                .strip_prefix("src-tauri/tests/")
                .and_then(|p| p.strip_suffix(".rs"))
            {
                if filename
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    insert(&mut targeted, &format!("cargo test --locked --manifest-path src-tauri/Cargo.toml --test {filename}"),
                        "Direct Rust integration test file.", std::slice::from_ref(path));
                } else {
                    missing_rust_tests.push(path.clone());
                }
            } else {
                missing_rust_tests.push(path.clone());
            }
        }
        rust_critical |= rust_all || !missing_rust_tests.is_empty();
        if !missing_rust_tests.is_empty() {
            notes.push(format!(
                "No reliable direct Rust test filter was found for {} changed source file(s); the broader Rust suite remains necessary.",
                missing_rust_tests.len()
            ));
        }
        insert(
            &mut final_checks,
            RUST_CLIPPY,
            "Compile and lint the entire Rust app, including non-test targets.",
            &rust,
        );
        if rust_critical {
            insert(&mut final_checks, RUST_FULL,
                "Shared/security/build change, missing direct tests, or uncertain impact requires the full Rust library regression.",
                &rust);
        } else {
            notes.push("Full Rust library regression is still recommended at a release/merge boundary; targeted tests are not exhaustive.".into());
        }
    }
    if has_node {
        if scripts.contains("test:frontend") {
            insert(
                &mut targeted,
                "npm run test:frontend",
                "Frontend test suite; no sound affected-test import graph has been established.",
                &frontend,
            );
        } else if scripts.contains("test") {
            insert(
                &mut targeted,
                "npm test",
                "Available frontend test script; exact affected tests are not inferred.",
                &frontend,
            );
        } else if !frontend.is_empty() {
            notes.push("Frontend source changed but no recognized test script exists; inspect its test runner manually.".into());
            unclassified.extend(frontend.iter().cloned());
        }
        if scripts.contains("check") {
            insert(
                &mut final_checks,
                "npm run check",
                "TypeScript type check for affected frontend modules.",
                &frontend,
            );
        }
    }
    if !browser.is_empty() && scripts.contains("test:browser") {
        insert(
            &mut targeted,
            "npm run test:browser",
            "Existing managed-browser helper test suite.",
            &browser,
        );
    }
    if !python.is_empty() {
        if root
            .join("src-tauri/resources/ai_workspace/test_ai_workspace_security.py")
            .is_file()
        {
            insert(
                &mut targeted,
                "python3 -m unittest discover -s src-tauri/resources/ai_workspace -p 'test_*.py'",
                "Existing Python AI Workspace security test discovery.",
                &python,
            );
        } else {
            notes.push("Python tests are not discoverable by this project's known test location; inspect and select a correct test runner.".into());
            unclassified.extend(python.iter().cloned());
        }
    }
    if !infra.is_empty() {
        notes.push("Build/configuration/tooling changes require evaluating every affected language; targeted tests alone are insufficient.".into());
        if rust_infra && root.join("src-tauri/Cargo.toml").is_file() && !has_rust {
            insert(
                &mut final_checks,
                RUST_FULL,
                "Build/tooling changes may affect Rust.",
                &infra,
            );
            insert(
                &mut final_checks,
                RUST_CLIPPY,
                "Rust compile and lint gate.",
                &infra,
            );
        }
        if scripts.contains("test:frontend") && !has_node {
            insert(
                &mut final_checks,
                "npm run test:frontend",
                "Build/tooling changes may affect frontend.",
                &infra,
            );
        }
    }
    if !unclassified.is_empty() {
        notes.push("Some changed files are outside recognized test mappings; manually review the affected components before claiming verification.".into());
    }
    if !docs.is_empty()
        && rust.is_empty()
        && frontend.is_empty()
        && infra.is_empty()
        && browser.is_empty()
        && python.is_empty()
        && unclassified.is_empty()
    {
        notes.push("Only documentation files changed; review documentation content and examples. No executable test is automatically selected.".into());
    }
    let high_risk = rust_critical || !infra.is_empty() || !unclassified.is_empty();
    if targeted.len() > MAX_TARGETED_COMMANDS {
        targeted.clear();
        notes.push("Too many directly affected modules for bounded targeted execution; use the wider regression instead.".into());
        if has_rust {
            insert(
                &mut final_checks,
                RUST_FULL,
                "Broad Rust impact exceeds targeted-command budget.",
                &rust,
            );
        }
    }
    TestImpactPlan {
        workspace_id: workspace_id.into(),
        head,
        changed_paths: paths,
        targeted,
        final_checks,
        unclassified_paths: unclassified,
        high_risk,
        requires_full_rust_regression: has_rust && rust_critical
            || rust_infra && root.join("src-tauri/Cargo.toml").is_file(),
        limitations: notes,
        coverage: "heuristic; not a completeness or test-pass claim",
    }
}
pub(crate) fn recommend(workspace: &Workspace) -> Result<TestImpactPlan, String> {
    let root = canonical_workspace_root(workspace)?;
    let changed = git::changed_paths_for_test_impact(workspace, MAX_CHANGED_PATHS)?;
    let head = git::head_hash(workspace)?;
    Ok(analyze_paths(&workspace.id, head, changed, &root))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(paths: &[&str]) -> TestImpactPlan {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-impact-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(root.join("src-tauri/src")).unwrap();
        fs::write(
            root.join("src-tauri/src/lib.rs"),
            "mod access;\nmod video_assets;\n",
        )
        .unwrap();
        fs::write(
            root.join("src-tauri/src/stray.rs"),
            "#[cfg(test)] mod tests { #[test] fn ignored() {} }",
        )
        .unwrap();
        fs::write(
            root.join("src-tauri/src/access.rs"),
            "#[cfg(test)] mod tests { #[test] fn test_access() {} }",
        )
        .unwrap();
        fs::write(
            root.join("src-tauri/src/video_assets.rs"),
            "#[cfg(test)] mod tests { #[test] fn test_assets() {} }",
        )
        .unwrap();
        fs::write(
            root.join("src-tauri/Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"scripts":{"test:frontend":"vitest run","check":"tsc --noEmit"}}"#,
        )
        .unwrap();
        let plan = analyze_paths(
            "test",
            Some("abc".into()),
            paths.iter().map(|p| p.to_string()).collect(),
            &root,
        );
        let _ = fs::remove_dir_all(root);
        plan
    }
    #[test]
    fn rust_module_filter_is_safe_and_path_scoped() {
        assert_eq!(
            source_module("src-tauri/src/task_integrity/gateway.rs").as_deref(),
            Some("task_integrity::gateway")
        );
        assert_eq!(source_module("src-tauri/src/lib.rs"), None);
        assert_eq!(source_module("src-tauri/src/evil;touch_x.rs"), None);
        assert_eq!(
            source_module("src-tauri/src/state/mod.rs").as_deref(),
            Some("state")
        );
    }
    #[test]
    fn shared_mcp_changes_require_full_rust() {
        let result = fixture(&["src-tauri/src/mcp_server.rs"]);
        assert!(result.requires_full_rust_regression);
        assert!(result
            .final_checks
            .iter()
            .any(|item| item.command == RUST_FULL));
    }
    #[test]
    fn rust_targeted_filters_are_discoverable_without_executing_commands() {
        let result = fixture(&["src-tauri/src/access.rs"]);
        assert!(result
            .targeted
            .iter()
            .any(|item| item.command.contains("access::tests::")));
        assert!(result.high_risk);
    }
    #[test]
    fn small_leaf_rust_change_uses_direct_tests_without_immediate_full_suite() {
        let plan = fixture(&["src-tauri/src/video_assets.rs"]);
        assert!(!plan.high_risk);
        assert!(!plan.requires_full_rust_regression);
        assert!(plan
            .targeted
            .iter()
            .any(|item| item.command.contains("video_assets::tests::")));
        assert!(plan
            .final_checks
            .iter()
            .any(|item| item.command == RUST_CLIPPY));
        assert!(!plan
            .final_checks
            .iter()
            .any(|item| item.command == RUST_FULL));
    }
    #[test]
    fn frontend_only_configuration_does_not_trigger_rust_regression() {
        let plan = fixture(&["tsconfig.json"]);
        assert!(!plan.requires_full_rust_regression);
        assert!(!plan
            .final_checks
            .iter()
            .any(|item| item.command == RUST_FULL));
        assert!(plan
            .targeted
            .iter()
            .any(|item| item.command == "npm run test:frontend"));
        assert!(plan
            .final_checks
            .iter()
            .any(|item| item.command == "npm run check"));
    }
    #[test]
    fn rust_file_not_registered_in_crate_cannot_be_treated_as_tested() {
        let plan = fixture(&["src-tauri/src/stray.rs"]);
        assert!(plan.targeted.is_empty());
        assert!(plan.requires_full_rust_regression);
        assert!(plan
            .final_checks
            .iter()
            .any(|item| item.command == RUST_FULL));
    }
    #[test]
    fn rust_without_direct_tests_requires_full_regression() {
        let plan = fixture(&["src-tauri/src/missing_module.rs"]);
        assert!(plan.requires_full_rust_regression);
        assert!(plan
            .final_checks
            .iter()
            .any(|item| item.command == RUST_FULL));
        assert!(plan.targeted.is_empty());
    }
    #[test]
    fn cargo_lock_changes_require_broad_regression() {
        let result = fixture(&["Cargo.lock"]);
        assert!(result.requires_full_rust_regression);
        assert!(result
            .final_checks
            .iter()
            .any(|item| item.command == RUST_FULL));
    }
    #[test]
    fn no_changes_produces_no_test_commands() {
        let result = fixture(&[]);
        assert!(result.targeted.is_empty());
        assert!(result.final_checks.is_empty());
    }
    #[test]
    fn unknown_paths_are_reported_not_silently_ignored() {
        let result = fixture(&["service/unknown.xyz"]);
        assert_eq!(result.unclassified_paths, vec!["service/unknown.xyz"]);
        assert!(result.high_risk);
    }
    #[test]
    fn real_git_repository_enumeration_preserves_individual_new_files() {
        use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};
        use std::{
            process::Command,
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "repotunnel-impact-real-git-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(root.join("src-tauri/src")).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .arg(&root)
            .status()
            .unwrap()
            .success());
        fs::write(
            root.join("src-tauri/src/lib.rs"),
            "mod access;\nmod new_module;\n",
        )
        .unwrap();
        fs::write(
            root.join("src-tauri/src/access.rs"),
            "#[cfg(test)] mod tests { #[test] fn safe() {} }",
        )
        .unwrap();
        assert!(Command::new("git")
            .current_dir(&root)
            .args(["add", "src-tauri/src/access.rs"])
            .status()
            .unwrap()
            .success());
        fs::write(
            root.join("src-tauri/src/new_module.rs"),
            "#[cfg(test)] mod tests { #[test] fn new_test() {} }",
        )
        .unwrap();
        let workspace = Workspace {
            id: "git-fixture".into(),
            name: "Git fixture".into(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let result = recommend(&workspace).expect("read-only impact inspection");
        assert!(result
            .changed_paths
            .contains(&"src-tauri/src/access.rs".into()));
        assert!(result
            .changed_paths
            .contains(&"src-tauri/src/new_module.rs".into()));
        assert!(result
            .targeted
            .iter()
            .any(|cmd| cmd.command.contains("access::tests::")));
        assert!(result
            .targeted
            .iter()
            .any(|cmd| cmd.command.contains("new_module::tests::")));
        assert!(result
            .final_checks
            .iter()
            .any(|cmd| cmd.command == RUST_FULL));
        let _ = fs::remove_dir_all(&root);
    }
    #[cfg(unix)]
    #[test]
    fn impact_reader_refuses_workspace_escape_through_linked_manifest() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("impact-safe-reader-{}", std::process::id()));
        let outside = root.with_extension("external");
        fs::create_dir_all(&root).unwrap();
        fs::write(&outside, r#"{"scripts":{"test":"malicious"}}"#).unwrap();
        symlink(&outside, root.join("package.json")).unwrap();
        assert!(read_small_project_text(&root, &root.join("package.json"), 1024).is_none());
        assert!(node_scripts(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(outside);
    }

    #[test]
    fn test_plan_never_returns_untrusted_shell_path() {
        let result = fixture(&["src-tauri/src/evil;touch_x.rs"]);
        assert!(result.targeted.is_empty());
    }
}
