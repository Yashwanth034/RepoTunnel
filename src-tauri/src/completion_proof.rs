//! Evidence-backed completion reporting; never an authorization gate.
//! A successful tool receipt is factual evidence of that operation, not a
//! mathematical proof that an arbitrary natural-language requirement is met.
use crate::{
    changes, git,
    models::{
        BrowserActionStatus, ChangeStatus, ManagedProcessStatus, TerminalCommandStatus, Workspace,
    },
    recovery_store, secret_guard,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{path::BaseDirectory, AppHandle, Manager};

const STORE_FILE: &str = "completion-proofs.json";
const MAX_PROOFS: usize = 150;
const MAX_CRITERIA: usize = 32;
const MAX_EVIDENCE: usize = 12;
static PROOF_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static PROOF_ID_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn proof_lock() -> &'static Mutex<()> {
    PROOF_LOCK.get_or_init(|| Mutex::new(()))
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
fn bounded(input: &str, label: &str, limit: usize) -> Result<String, String> {
    let text = input.trim();
    if text.is_empty() || text.chars().count() > limit || text.chars().any(char::is_control) {
        return Err(format!(
            "{label} must contain 1..={limit} printable characters."
        ));
    }
    Ok(secret_guard::redact_text(text))
}
fn store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE_FILE, BaseDirectory::AppData)
        .map_err(|e| format!("Could not resolve completion proof storage: {e}"))
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum EvidenceKind {
    TerminalCommand,
    ManagedProcess,
    BrowserTransaction,
    Change,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EvidenceRef {
    pub(crate) kind: EvidenceKind,
    pub(crate) source_id: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Criterion {
    pub(crate) id: String,
    pub(crate) requirement: String,
    pub(crate) evidence: Vec<EvidenceRef>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Proof {
    pub(crate) id: String,
    pub(crate) workspace_id: String,
    pub(crate) request: String,
    pub(crate) created_at: u64,
    pub(crate) baseline_head: Option<String>,
    pub(crate) criteria: Vec<Criterion>,
}
#[derive(Default, Deserialize, Serialize)]
struct ProofStore {
    proofs: BTreeMap<String, Proof>,
}
fn load_path(path: &Path) -> Result<ProofStore, String> {
    Ok(recovery_store::load_json(path, "completion proof")?
        .value
        .unwrap_or_default())
}
fn save_path(path: &Path, store: &ProofStore) -> Result<(), String> {
    recovery_store::save_json(path, store, "completion proof")
}
fn load(app: &AppHandle) -> Result<ProofStore, String> {
    load_path(&store_path(app)?)
}

pub(crate) fn create(
    app: &AppHandle,
    workspace: &Workspace,
    request: String,
    requirements: Vec<String>,
) -> Result<Audit, String> {
    let request = bounded(&request, "Request", 1200)?;
    if requirements.is_empty() || requirements.len() > MAX_CRITERIA {
        return Err(format!(
            "Provide 1..={MAX_CRITERIA} concrete acceptance criteria."
        ));
    }
    let mut criteria = Vec::with_capacity(requirements.len());
    for (index, requirement) in requirements.iter().enumerate() {
        let requirement = bounded(requirement, "Acceptance criterion", 420)?;
        if criteria
            .iter()
            .any(|item: &Criterion| item.requirement == requirement)
        {
            return Err("Duplicate acceptance criteria cannot count twice.".to_string());
        }
        criteria.push(Criterion {
            id: format!("criterion-{}", index + 1),
            requirement,
            evidence: vec![],
        });
    }
    let _guard = proof_lock()
        .lock()
        .map_err(|_| "Completion proof storage is busy.")?;
    let path = store_path(app)?;
    let mut store = load_path(&path)?;
    if store.proofs.len() >= MAX_PROOFS {
        return Err(
            "Completion proof history is full; archive old records locally before creating more."
                .to_string(),
        );
    }
    let created_at = now();
    let id = format!(
        "proof-{created_at}-{}-{}",
        std::process::id(),
        PROOF_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    );
    if store.proofs.contains_key(&id) {
        return Err("Completion proof ID collision; retry creation.".to_string());
    }
    let proof = Proof {
        id: id.clone(),
        workspace_id: workspace.id.clone(),
        request,
        created_at,
        baseline_head: git::head_hash(workspace)?,
        criteria,
    };
    store.proofs.insert(id, proof.clone());
    save_path(&path, &store)?;
    audit_with(
        &proof,
        proof.baseline_head.as_deref(),
        created_at,
        |_, _| Ok(None),
    )
}
pub(crate) fn list(app: &AppHandle, workspace: &Workspace) -> Result<Vec<Proof>, String> {
    let store = load(app)?;
    let mut proofs = store
        .proofs
        .into_values()
        .filter(|proof| proof.workspace_id == workspace.id)
        .collect::<Vec<_>>();
    proofs.sort_by_key(|item| std::cmp::Reverse(item.created_at));
    Ok(proofs)
}
pub(crate) fn add_evidence(
    app: &AppHandle,
    workspace: &Workspace,
    proof_id: &str,
    criterion_id: &str,
    evidence: EvidenceRef,
) -> Result<Audit, String> {
    let evidence = EvidenceRef {
        kind: evidence.kind,
        source_id: bounded(&evidence.source_id, "Evidence ID", 256)?,
    };
    let _guard = proof_lock()
        .lock()
        .map_err(|_| "Completion proof storage is busy.")?;
    let path = store_path(app)?;
    let mut store = load_path(&path)?;
    let proof = store
        .proofs
        .get_mut(proof_id)
        .filter(|proof| proof.workspace_id == workspace.id)
        .ok_or_else(|| {
            "No completion proof with that ID belongs to this approved workspace.".to_string()
        })?;
    let criterion = proof
        .criteria
        .iter_mut()
        .find(|item| item.id == criterion_id)
        .ok_or_else(|| "No criterion with that ID belongs to this completion proof.".to_string())?;
    if criterion.evidence.len() >= MAX_EVIDENCE && !criterion.evidence.contains(&evidence) {
        return Err("This criterion has reached its evidence reference limit.".to_string());
    }
    // Validate receipt existence, workspace, age and success BEFORE storing;
    // outdated, pending, failed, or invented evidence never becomes proof.
    let receipt = receipt_status(app, workspace, &evidence)?;
    if !receipt.success || receipt.created_at < proof.created_at {
        return Err("Evidence must reference a successful RepoTunnel receipt created after the completion proof began.".to_string());
    }
    if !criterion.evidence.contains(&evidence) {
        criterion.evidence.push(evidence);
    }
    let proof = proof.clone();
    save_path(&path, &store)?;
    audit(app, workspace, &proof.id)
}
pub(crate) fn audit(
    app: &AppHandle,
    workspace: &Workspace,
    proof_id: &str,
) -> Result<Audit, String> {
    let store = load(app)?;
    let proof = store
        .proofs
        .get(proof_id)
        .filter(|proof| proof.workspace_id == workspace.id)
        .ok_or_else(|| {
            "No completion proof with that ID belongs to this approved workspace.".to_string()
        })?;
    let current_head = git::head_hash(workspace)?;
    let latest_change = changes::list_changes(app, Some(&workspace.id), 100)?
        .iter()
        .filter(|item| item.status == ChangeStatus::Applied)
        .map(|item| item.updated_at)
        .max()
        .unwrap_or(0);
    audit_with(
        proof,
        current_head.as_deref(),
        latest_change,
        |reference, _| receipt_status(app, workspace, reference).map(Some),
    )
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReceiptStatus {
    success: bool,
    created_at: u64,
    verifies_behavior: bool,
}
fn test_command(command: &str) -> bool {
    // Check the beginning of an actual shell command segment, never mere
    // mentions of "cargo test" in an echo, comment, or unrelated argument.
    // Static analysis alone is not evidence that a user behavior works.
    const RUNNERS: &[&str] = &[
        "cargo test",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "pytest",
        "vitest",
        "playwright test",
        "go test",
        "dotnet test",
        "mvn test",
        "gradle test",
        "python -m unittest",
        "python3 -m unittest",
        "python -m pytest",
        "python3 -m pytest",
    ];
    command
        .to_ascii_lowercase()
        .split([';', '\n'])
        .flat_map(|part| part.split("&&"))
        .map(str::trim_start)
        .filter(|part| !part.starts_with('#'))
        .any(|part| {
            RUNNERS.iter().any(|runner| {
                part.strip_prefix(runner)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
            })
        })
}
fn browser_transaction_verifies_behavior(result: &serde_json::Value) -> bool {
    result.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        && result.get("totalSteps").and_then(serde_json::Value::as_u64).unwrap_or(0) > 0
        && result.get("completedSteps") == result.get("totalSteps")
        // Clicking or filling alone isn't acceptance evidence.
        && result.get("steps").and_then(serde_json::Value::as_array)
            .is_some_and(|steps| steps.iter().any(|step| {
                step.get("operation").and_then(serde_json::Value::as_str)
                    .is_some_and(|op| op.eq_ignore_ascii_case("verify"))
                    && step.get("result").and_then(|value| value.get("verified"))
                        .and_then(serde_json::Value::as_bool) == Some(true)
            }))
}

fn receipt_status(
    app: &AppHandle,
    workspace: &Workspace,
    reference: &EvidenceRef,
) -> Result<ReceiptStatus, String> {
    match reference.kind {
        EvidenceKind::TerminalCommand => {
            let record = crate::terminal::get_terminal_command(app, &reference.source_id)?;
            if record.workspace_id != workspace.id {
                return Err("Receipt belongs to another workspace.".into());
            }
            Ok(ReceiptStatus {
                success: record.status == TerminalCommandStatus::Completed
                    && record.exit_code == Some(0),
                created_at: record.created_at,
                verifies_behavior: test_command(&record.command),
            })
        }
        EvidenceKind::ManagedProcess => {
            let record = crate::terminal::get_process(app, &reference.source_id)?;
            if record.workspace_id != workspace.id {
                return Err("Receipt belongs to another workspace.".into());
            }
            Ok(ReceiptStatus {
                success: record.status == ManagedProcessStatus::Exited
                    && record.exit_code == Some(0),
                created_at: record.created_at,
                verifies_behavior: test_command(&record.command),
            })
        }
        EvidenceKind::BrowserTransaction => {
            let (record, _) =
                crate::browser::get_action_with_task_binding(app, &reference.source_id)?;
            if record.workspace_id != workspace.id {
                return Err("Receipt belongs to another workspace.".into());
            }
            let transaction_passed = record
                .transaction_result
                .as_ref()
                .is_some_and(browser_transaction_verifies_behavior);
            Ok(ReceiptStatus {
                success: record.status == BrowserActionStatus::Applied && transaction_passed,
                created_at: record.created_at,
                verifies_behavior: transaction_passed,
            })
        }
        EvidenceKind::Change => {
            let record = changes::get_change_record(app, &reference.source_id)?;
            if record.workspace_id != workspace.id {
                return Err("Receipt belongs to another workspace.".into());
            }
            Ok(ReceiptStatus {
                success: record.status == ChangeStatus::Applied,
                created_at: record.created_at,
                verifies_behavior: false,
            })
        }
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CriterionAudit {
    pub(crate) id: String,
    pub(crate) requirement: String,
    pub(crate) evidence_count: usize,
    pub(crate) status: &'static str,
    pub(crate) findings: Vec<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Audit {
    pub(crate) proof_id: String,
    pub(crate) workspace_id: String,
    pub(crate) request: String,
    pub(crate) verified_criteria: usize,
    pub(crate) total_criteria: usize,
    pub(crate) evidence_backed: bool,
    pub(crate) status: &'static str,
    pub(crate) criteria: Vec<CriterionAudit>,
    pub(crate) blockers: Vec<String>,
    pub(crate) limitation: &'static str,
}
fn audit_with<F>(
    proof: &Proof,
    current_head: Option<&str>,
    latest_change_at: u64,
    mut resolver: F,
) -> Result<Audit, String>
where
    F: FnMut(&EvidenceRef, &str) -> Result<Option<ReceiptStatus>, String>,
{
    let head_changed = proof.baseline_head.as_deref() != current_head;
    // One broad green test run cannot independently establish every different
    // criterion. Require independently identifiable behavioral checks for each.
    let mut verification_uses = BTreeMap::<String, usize>::new();
    for item in &proof.criteria {
        for evidence in &item.evidence {
            if evidence.kind != EvidenceKind::Change {
                *verification_uses
                    .entry(format!("{:?}:{}", evidence.kind, evidence.source_id))
                    .or_default() += 1;
            }
        }
    }
    let mut results = Vec::with_capacity(proof.criteria.len());
    let mut blockers = Vec::new();
    for item in &proof.criteria {
        let mut findings = Vec::new();
        let mut actual_verification = false;
        let mut invalid_evidence = false;
        if item.evidence.is_empty() {
            findings.push("No factual verification receipts attached.".to_string());
        }
        for evidence in &item.evidence {
            match resolver(evidence, &item.id) {
                Ok(Some(receipt)) if receipt.success && receipt.created_at >= proof.created_at => {
                    if receipt.verifies_behavior {
                        let key = format!("{:?}:{}", evidence.kind, evidence.source_id);
                        if verification_uses.get(&key).copied().unwrap_or(0) > 1 {
                            findings.push(
                                "This test/browser receipt is reused across different criteria; attach a distinct targeted verification for each requirement."
                                    .to_string(),
                            );
                        } else if receipt.created_at >= latest_change_at {
                            actual_verification = true;
                        } else {
                            findings.push(
                                "Test/browser evidence predates later recorded project changes."
                                    .to_string(),
                            );
                        }
                    } else {
                        findings.push(
                            "An implementation receipt alone does not verify behavior.".to_string(),
                        );
                    }
                }
                Ok(Some(_)) => {
                    invalid_evidence = true;
                    findings.push("Evidence is failed, stale or predates the request.".to_string());
                }
                Ok(None) => {
                    invalid_evidence = true;
                    findings.push("Evidence receipt could not be resolved.".to_string());
                }
                Err(_) => {
                    invalid_evidence = true;
                    findings.push("Evidence is missing or no longer verifiable.".to_string());
                }
            }
        }
        if head_changed {
            findings
                .push("Git HEAD changed since the proof started; reassess criteria.".to_string());
        }
        let passed = actual_verification && !invalid_evidence && !head_changed;
        if !passed {
            blockers.push(format!(
                "{}: {}",
                item.id,
                if findings.is_empty() {
                    "No applicable passing behavioral verification"
                } else {
                    "Verification incomplete"
                }
            ));
        }
        results.push(CriterionAudit {
            id: item.id.clone(),
            requirement: item.requirement.clone(),
            evidence_count: item.evidence.len(),
            status: if passed {
                "evidenceBacked"
            } else {
                "verificationIncomplete"
            },
            findings,
        });
    }
    let verified = results
        .iter()
        .filter(|c| c.status == "evidenceBacked")
        .count();
    let complete = verified == results.len() && !results.is_empty();
    Ok(Audit {
        proof_id: proof.id.clone(), workspace_id: proof.workspace_id.clone(), request: proof.request.clone(),
        verified_criteria: verified, total_criteria: results.len(), evidence_backed: complete,
        status: if complete { "allCriteriaEvidenceBacked" } else { "verificationIncomplete" },
        criteria: results, blockers,
        limitation: "Passing receipts only prove those operations succeeded. They do not prove test relevance, complete behavioral coverage, independent review, or live deployment. Never claim fully verified merely from this report.",
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Proof {
        Proof {
            id: "proof-test".into(),
            workspace_id: "w".into(),
            request: "Task warning only".into(),
            created_at: 100,
            baseline_head: Some("head1".into()),
            criteria: vec![Criterion {
                id: "criterion-1".into(),
                requirement: "AI edit continues after warning".into(),
                evidence: vec![EvidenceRef {
                    kind: EvidenceKind::TerminalCommand,
                    source_id: "run1".into(),
                }],
            }],
        }
    }
    #[test]
    fn missing_receipt_cannot_claim_completion() {
        let proof = fixture();
        let report = audit_with(&proof, Some("head1"), 0, |_, _| Ok(None)).unwrap();
        assert!(!report.evidence_backed);
        assert_eq!(report.status, "verificationIncomplete");
    }
    #[test]
    fn failed_or_old_evidence_cannot_claim_completion() {
        let proof = fixture();
        for (success, created_at) in [(false, 120), (true, 99)] {
            let report = audit_with(&proof, Some("head1"), 0, |_, _| {
                Ok(Some(ReceiptStatus {
                    success,
                    created_at,
                    verifies_behavior: true,
                }))
            })
            .unwrap();
            assert!(!report.evidence_backed);
        }
    }
    #[test]
    fn implementation_receipts_cannot_claim_verified() {
        let proof = fixture();
        let report = audit_with(&proof, Some("head1"), 0, |_, _| {
            Ok(Some(ReceiptStatus {
                success: true,
                created_at: 130,
                verifies_behavior: false,
            }))
        })
        .unwrap();
        assert!(!report.evidence_backed);
    }
    #[test]
    fn successful_verification_is_invalidated_by_later_changes_or_head() {
        let proof = fixture();
        let resolver = |_: &EvidenceRef, _: &str| {
            Ok(Some(ReceiptStatus {
                success: true,
                created_at: 130,
                verifies_behavior: true,
            }))
        };
        assert!(
            audit_with(&proof, Some("head1"), 120, resolver)
                .unwrap()
                .evidence_backed
        );
        assert!(
            !audit_with(&proof, Some("head1"), 140, resolver)
                .unwrap()
                .evidence_backed
        );
        assert!(
            !audit_with(&proof, Some("head2"), 120, resolver)
                .unwrap()
                .evidence_backed
        );
    }
    #[test]
    fn every_criterion_requires_its_own_verification() {
        let mut proof = fixture();
        proof.criteria.push(Criterion {
            id: "criterion-2".into(),
            requirement: "No approval pause".into(),
            evidence: vec![],
        });
        let report = audit_with(&proof, Some("head1"), 100, |_, _| {
            Ok(Some(ReceiptStatus {
                success: true,
                created_at: 110,
                verifies_behavior: true,
            }))
        })
        .unwrap();
        assert_eq!(report.verified_criteria, 1);
        assert!(!report.evidence_backed);
    }
    #[test]
    fn arbitrary_success_command_is_not_a_behavioral_test() {
        assert!(!test_command("echo SUCCESS"));
        assert!(!test_command("echo 'cargo test --lib'"));
        assert!(!test_command("cargo clippy -- -D warnings"));
        assert!(!test_command("npm run check"));
        assert!(!test_command("true"));
        assert!(test_command("cargo test --lib"));
        assert!(test_command(
            "set -e; cargo test --lib && npm run test:frontend"
        ));
    }
    #[test]
    fn browser_click_without_verify_is_not_behavioral_evidence() {
        let clicked = serde_json::json!({
            "ok": true, "totalSteps": 1, "completedSteps": 1,
            "steps": [{"operation": "click", "result": {}}]
        });
        assert!(!browser_transaction_verifies_behavior(&clicked));
        let verified = serde_json::json!({
            "ok": true, "totalSteps": 2, "completedSteps": 2,
            "steps": [
                {"operation": "click", "result": {}},
                {"operation": "verify", "result": {"verified": true}}
            ]
        });
        assert!(browser_transaction_verifies_behavior(&verified));
    }
    #[test]
    fn a_single_green_suite_cannot_certify_multiple_requirements() {
        let mut proof = fixture();
        proof.criteria.push(Criterion {
            id: "criterion-2".into(),
            requirement: "MCP workflow never asks approval".into(),
            evidence: proof.criteria[0].evidence.clone(),
        });
        let report = audit_with(&proof, Some("head1"), 0, |_, _| {
            Ok(Some(ReceiptStatus {
                success: true,
                created_at: 130,
                verifies_behavior: true,
            }))
        })
        .unwrap();
        assert_eq!(report.verified_criteria, 0);
        assert!(report.criteria.iter().all(|item| item
            .findings
            .iter()
            .any(|finding| finding.contains("reused across different criteria"))));
        assert!(!report.evidence_backed);

        proof.criteria[1].evidence[0].source_id = "targeted-run-2".into();
        let report = audit_with(&proof, Some("head1"), 0, |_, _| {
            Ok(Some(ReceiptStatus {
                success: true,
                created_at: 130,
                verifies_behavior: true,
            }))
        })
        .unwrap();
        assert!(report.evidence_backed);
    }

    #[test]
    fn invalidated_evidence_cannot_be_hidden_behind_one_passing_receipt() {
        let mut proof = fixture();
        proof.criteria[0].evidence.push(EvidenceRef {
            kind: EvidenceKind::TerminalCommand,
            source_id: "run2".into(),
        });
        let audit = audit_with(&proof, Some("head1"), 0, |reference, _| {
            if reference.source_id == "run2" {
                return Ok(None);
            }
            Ok(Some(ReceiptStatus {
                success: true,
                created_at: 130,
                verifies_behavior: true,
            }))
        })
        .unwrap();
        assert!(!audit.evidence_backed);
        assert!(audit.criteria[0]
            .findings
            .iter()
            .any(|finding| finding.contains("could not be resolved")));
    }

    #[test]
    fn recovery_store_roundtrip_preserves_evidence_and_workspace() {
        let path = std::env::temp_dir().join(format!(
            "repotunnel-proof-test-{}-{}.json",
            std::process::id(),
            now()
        ));
        let proof = fixture();
        let mut data = ProofStore::default();
        data.proofs.insert(proof.id.clone(), proof);
        save_path(&path, &data).unwrap();
        assert_eq!(
            load_path(&path).unwrap().proofs["proof-test"].workspace_id,
            "w"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_file_name(format!(
            ".{}.previous",
            path.file_name().unwrap().to_string_lossy()
        )));
    }
}
