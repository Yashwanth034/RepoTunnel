use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tokio::sync::watch;

use crate::model_hub::{
    self, LocalChatErrorKind, LocalChatMessage, LocalModelInfo, ModelHubSnapshot, ModelProviderId,
    ModelSelection, RuntimeStatus,
};

const TRIAL_FILE: &str = "model-trial.json";
pub(crate) const TRIAL_SUITE_VERSION: &str = "stage10-v1";
const MAX_SELECTED_MODELS: usize = 12;
const MAX_TRIAL_OUTPUT_CHARS: usize = 12_000;
const CASE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CORRECTIONS: usize = 1;

static ACTIVE_TRIAL: OnceLock<Mutex<Option<watch::Sender<bool>>>> = OnceLock::new();
static ACTIVE_MODEL: OnceLock<Mutex<Option<ModelIdentity>>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TrialMode {
    Quick,
    Full,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TrialCategory {
    InstructionFollowing,
    StructuredJson,
    CodeUnderstanding,
    Planning,
    PatchReasoning,
    ReviewQuality,
    SecurityReasoning,
    TestReasoning,
    ResearchSummarization,
    ContextHandling,
    ResponseSpeed,
    Reliability,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelIdentity {
    pub(crate) provider: ModelProviderId,
    pub(crate) model_id: String,
    pub(crate) endpoint: String,
    pub(crate) runtime_version: Option<String>,
    pub(crate) metadata_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrialCategoryScore {
    pub(crate) category: TrialCategory,
    pub(crate) score: u8,
    pub(crate) evidence: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TrialModelStatus {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelTrialResult {
    pub(crate) identity: ModelIdentity,
    pub(crate) runtime_label: String,
    pub(crate) model_name: String,
    pub(crate) suite_version: String,
    pub(crate) tested_at: u64,
    pub(crate) mode: TrialMode,
    pub(crate) status: TrialModelStatus,
    pub(crate) category_scores: Vec<TrialCategoryScore>,
    pub(crate) average_latency_ms: u64,
    pub(crate) attempted_cases: u32,
    pub(crate) failed_cases: u32,
    pub(crate) malformed_cases: u32,
    pub(crate) failure_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelTrialResultView {
    #[serde(flatten)]
    pub(crate) result: ModelTrialResult,
    pub(crate) current: bool,
    pub(crate) stale_reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelTrialSnapshot {
    pub(crate) suite_version: String,
    pub(crate) running: bool,
    pub(crate) active_model: Option<ModelIdentity>,
    pub(crate) results: Vec<ModelTrialResultView>,
    pub(crate) last_cancelled_at: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
struct TrialStore {
    results: Vec<ModelTrialResult>,
    last_cancelled_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrialCaseKind {
    InstructionStructured,
    CodeReview,
    PlanningContext,
    Patch,
    Security,
    Test,
    ResearchContext,
}

#[derive(Debug)]
struct TrialCaseOutcome {
    scores: Vec<(TrialCategory, u8, String)>,
    latency_ms: u64,
    malformed: bool,
}
#[derive(Debug)]
struct TrialCaseError {
    message: String,
    cancelled: bool,
    malformed: bool,
    latency_ms: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
fn trial_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(TRIAL_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel Model Trial data: {error}"))
}
fn load_store_path(path: &Path) -> Result<TrialStore, String> {
    if !path.exists() {
        return Ok(TrialStore::default());
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Could not read Model Trial data: {error}"))?;
    if text.trim().is_empty() {
        return Ok(TrialStore::default());
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("Saved Model Trial data is invalid: {error}"))
}
fn save_store_path(path: &Path, store: &TrialStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve Model Trial data directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create Model Trial data directory: {error}"))?;
    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("Could not serialize Model Trial data: {error}"))?;
    let temporary = path.with_extension(format!("tmp-{}", now_millis()));
    fs::write(&temporary, bytes)
        .map_err(|error| format!("Could not save temporary Model Trial data: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not replace Model Trial data safely: {error}"))
}
fn load_store(app: &AppHandle) -> Result<TrialStore, String> {
    load_store_path(&trial_path(app)?)
}
fn save_store(app: &AppHandle, store: &TrialStore) -> Result<(), String> {
    save_store_path(&trial_path(app)?, store)
}
fn round_score(value: f64) -> u8 {
    ((value.clamp(0.0, 100.0) / 5.0).round() * 5.0) as u8
}

fn metadata_fingerprint(runtime: &RuntimeStatus, model: &LocalModelInfo) -> String {
    let value = json!({
        "provider": model.provider, "modelId": model.id, "runtime": runtime.label,
        "runtimeVersion": runtime.version, "sizeBytes": model.size_bytes,
        "parameterSize": model.parameter_size, "quantization": model.quantization,
        "contextWindow": model.capabilities.context_window.value,
        "structuredOutput": model.capabilities.structured_output.value,
        "toolCalling": model.capabilities.tool_calling.value, "vision": model.capabilities.vision.value,
    });
    Sha256::digest(serde_json::to_vec(&value).unwrap_or_default())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}
pub(crate) fn model_identity(runtime: &RuntimeStatus, model: &LocalModelInfo) -> ModelIdentity {
    ModelIdentity {
        provider: runtime.provider,
        model_id: model.id.clone(),
        endpoint: runtime.endpoint.clone(),
        runtime_version: runtime.version.clone(),
        metadata_fingerprint: metadata_fingerprint(runtime, model),
    }
}
fn discovered_models(
    snapshot: &ModelHubSnapshot,
) -> Vec<(ModelIdentity, &RuntimeStatus, &LocalModelInfo)> {
    snapshot
        .runtimes
        .iter()
        .filter(|runtime| runtime.reachable)
        .flat_map(|runtime| {
            runtime
                .models
                .iter()
                .map(move |model| (model_identity(runtime, model), runtime, model))
        })
        .collect()
}
fn staleness(result: &ModelTrialResult, snapshot: &ModelHubSnapshot) -> Option<String> {
    if result.suite_version != TRIAL_SUITE_VERSION {
        return Some(format!(
            "Trial suite {} is incompatible with current suite {}.",
            result.suite_version, TRIAL_SUITE_VERSION
        ));
    }
    let current = discovered_models(snapshot)
        .into_iter()
        .find(|(identity, _, _)| {
            identity.provider == result.identity.provider
                && identity.model_id == result.identity.model_id
                && identity.endpoint == result.identity.endpoint
        });
    let Some((identity, _, _)) = current else {
        return Some("Model/runtime is not currently discovered.".to_string());
    };
    if identity != result.identity {
        return Some("Model runtime or metadata changed since this trial.".to_string());
    }
    None
}
pub(crate) async fn snapshot(app: &AppHandle) -> Result<ModelTrialSnapshot, String> {
    let hub = model_hub::snapshot(app).await?;
    snapshot_with_hub(app, &hub)
}
pub(crate) fn snapshot_with_hub(
    app: &AppHandle,
    hub: &ModelHubSnapshot,
) -> Result<ModelTrialSnapshot, String> {
    let store = load_store(app)?;
    let running = ACTIVE_TRIAL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false);
    let active_model = ACTIVE_MODEL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let mut results = store
        .results
        .iter()
        .cloned()
        .map(|result| {
            let stale_reason = staleness(&result, hub);
            ModelTrialResultView {
                current: stale_reason.is_none(),
                stale_reason,
                result,
            }
        })
        .collect::<Vec<_>>();
    results.sort_by_key(|entry| std::cmp::Reverse(entry.result.tested_at));
    Ok(ModelTrialSnapshot {
        suite_version: TRIAL_SUITE_VERSION.to_string(),
        running,
        active_model,
        results,
        last_cancelled_at: store.last_cancelled_at,
    })
}

fn cases(mode: TrialMode) -> &'static [TrialCaseKind] {
    match mode {
        TrialMode::Quick => &[
            TrialCaseKind::InstructionStructured,
            TrialCaseKind::CodeReview,
            TrialCaseKind::Security,
            TrialCaseKind::Test,
        ],
        TrialMode::Full => &[
            TrialCaseKind::InstructionStructured,
            TrialCaseKind::CodeReview,
            TrialCaseKind::PlanningContext,
            TrialCaseKind::Patch,
            TrialCaseKind::Security,
            TrialCaseKind::Test,
            TrialCaseKind::ResearchContext,
        ],
    }
}
fn case_prompt(kind: TrialCaseKind) -> &'static str {
    match kind {
        TrialCaseKind::InstructionStructured => {
            r#"Return ONLY one JSON object. marker must equal RT10 and order must be exactly [gamma, alpha, beta]. Schema: {"marker":"RT10","order":["gamma","alpha","beta"]}."#
        }
        TrialCaseKind::CodeReview => {
            r#"Return ONLY JSON. Inspect: fn item(v:&[i32], i:usize)->i32 { if i <= v.len() { v[i] } else { 0 } } and: let name: Option<&str> = None; println!("{}", name.unwrap()); Schema: {"issues":["identifier"]}. Expected identifiers: off_by_one and null_guard."#
        }
        TrialCaseKind::PlanningContext => {
            r#"Return ONLY JSON. A change must preserve security boundaries, use existing architecture, and verify before completion. Return exactly ordered steps inspect, plan, implement, verify and risk state_drift. Schema: {"steps":["identifier"],"risk":"identifier"}."#
        }
        TrialCaseKind::Patch => {
            r#"Return ONLY JSON. Choose the safest workspace-write strategy: direct_write, guard_before_write, shell_redirect. It must validate workspace_boundary and preserve history. Schema: {"choice":"identifier","checks":["identifier"]}."#
        }
        TrialCaseKind::Security => {
            r#"Return ONLY JSON. Inspect: let target = root.join(user_path); fs::write(target,data)?; and later canonicalization follows a user-controlled symlink. Schema: {"issues":["identifier"]}. Expected: path_traversal and symlink_escape."#
        }
        TrialCaseKind::Test => {
            r#"Return ONLY JSON. A safe workspace-path validator changed. Return required regression IDs valid_path, traversal, symlink, stale_state. Schema: {"tests":["identifier"]}."#
        }
        TrialCaseKind::ResearchContext => {
            r#"Return ONLY JSON. Facts: A=runtime is loopback-only; B=no cloud fallback; C=model downloads are never automatic. Return exactly facts A,B,C and unsupported=false. Schema: {"facts":["A","B","C"],"unsupported":false}."#
        }
    }
}

fn score_members(actual: &[String], expected: &[&str]) -> u8 {
    let matches = expected
        .iter()
        .filter(|expected| actual.iter().any(|actual| actual == **expected))
        .count();
    round_score(matches as f64 / expected.len() as f64 * 100.0)
}
fn score_case(kind: TrialCaseKind, value: &Value) -> Vec<(TrialCategory, u8, String)> {
    use TrialCategory::*;
    match kind {
        TrialCaseKind::InstructionStructured => {
            let marker = value.get("marker").and_then(Value::as_str) == Some("RT10");
            let order = value
                .get("order")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let exact = order == ["gamma", "alpha", "beta"];
            let score = if marker && exact {
                100
            } else if marker || exact {
                50
            } else {
                0
            };
            vec![(
                InstructionFollowing,
                score,
                "exact marker/order check".to_string(),
            )]
        }
        TrialCaseKind::CodeReview => {
            let issues = value
                .get("issues")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let score = score_members(&issues, &["off_by_one", "null_guard"]);
            vec![
                (
                    CodeUnderstanding,
                    score,
                    "known bug identifiers".to_string(),
                ),
                (ReviewQuality, score, "known review findings".to_string()),
            ]
        }
        TrialCaseKind::PlanningContext => {
            let steps = value
                .get("steps")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let ordered = steps == ["inspect", "plan", "implement", "verify"];
            let risk = value.get("risk").and_then(Value::as_str) == Some("state_drift");
            let score = if ordered && risk {
                100
            } else if ordered || risk {
                60
            } else {
                0
            };
            vec![
                (
                    Planning,
                    score,
                    "controlled decomposition/order".to_string(),
                ),
                (
                    ContextHandling,
                    score,
                    "constraint/risk retention".to_string(),
                ),
            ]
        }
        TrialCaseKind::Patch => {
            let choice = value.get("choice").and_then(Value::as_str) == Some("guard_before_write");
            let checks = value
                .get("checks")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let member_score = score_members(&checks, &["workspace_boundary", "history"]);
            let score =
                round_score((if choice { 50.0 } else { 0.0 }) + f64::from(member_score) * 0.5);
            vec![(
                PatchReasoning,
                score,
                "safe patch choice/checks".to_string(),
            )]
        }
        TrialCaseKind::Security => {
            let issues = value
                .get("issues")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let score = score_members(&issues, &["path_traversal", "symlink_escape"]);
            vec![
                (
                    SecurityReasoning,
                    score,
                    "known boundary vulnerabilities".to_string(),
                ),
                (
                    ReviewQuality,
                    score,
                    "security finding coverage".to_string(),
                ),
            ]
        }
        TrialCaseKind::Test => {
            let tests = value
                .get("tests")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let score = score_members(
                &tests,
                &["valid_path", "traversal", "symlink", "stale_state"],
            );
            vec![(
                TestReasoning,
                score,
                "required regression cases".to_string(),
            )]
        }
        TrialCaseKind::ResearchContext => {
            let facts = value
                .get("facts")
                .and_then(Value::as_array)
                .map(|items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            let exact_facts = facts == ["A", "B", "C"];
            let unsupported = value.get("unsupported").and_then(Value::as_bool) == Some(false);
            let score = if exact_facts && unsupported {
                100
            } else if exact_facts || unsupported {
                60
            } else {
                0
            };
            vec![
                (
                    ResearchSummarization,
                    score,
                    "bounded supported facts".to_string(),
                ),
                (
                    ContextHandling,
                    score,
                    "fact retention/no invention".to_string(),
                ),
            ]
        }
    }
}
fn parse_json_object(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    let trimmed = if trimmed.starts_with("```") {
        let body = trimmed
            .find('\n')
            .map(|index| &trimmed[index + 1..])
            .unwrap_or(trimmed);
        body.strip_suffix("```")
            .map(str::trim)
            .unwrap_or(body.trim())
    } else {
        trimmed
    };
    let value: Value = serde_json::from_str(trimmed).map_err(|error| error.to_string())?;
    if !value.is_object() {
        return Err("structured output was not a JSON object".to_string());
    }
    Ok(value)
}

async fn generate_case_response(
    selection: ModelSelection,
    prompt: &str,
    cancel: watch::Receiver<bool>,
) -> Result<String, TrialCaseError> {
    let started = Instant::now();
    let messages = vec![
        LocalChatMessage { role:"system".to_string(), content:"You are running a small deterministic RepoTunnel local Model Trial. Return only the requested JSON object. Do not use tools, browse, or add prose.".to_string() },
        LocalChatMessage { role:"user".to_string(), content:prompt.to_string() },
    ];
    let mut output = String::new();
    let future = model_hub::stream_chat_structured(selection, messages, cancel, |chunk| {
        if output.chars().count().saturating_add(chunk.chars().count()) > MAX_TRIAL_OUTPUT_CHARS {
            return Err("trial output exceeded bounded limit".to_string());
        }
        output.push_str(chunk);
        Ok(())
    });
    match tokio::time::timeout(CASE_TIMEOUT, future).await {
        Ok(Ok(())) => Ok(output),
        Ok(Err(error)) => Err(TrialCaseError {
            cancelled: error.kind == LocalChatErrorKind::Cancelled,
            malformed: error.kind == LocalChatErrorKind::InvalidStream,
            message: error.message,
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }),
        Err(_) => Err(TrialCaseError {
            cancelled: false,
            malformed: false,
            message: "Trial case timed out.".to_string(),
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }),
    }
}
async fn run_case(
    selection: ModelSelection,
    kind: TrialCaseKind,
    cancel: watch::Receiver<bool>,
) -> Result<TrialCaseOutcome, TrialCaseError> {
    let started = Instant::now();
    let mut malformed = false;
    let mut prompt = case_prompt(kind).to_string();
    for attempt in 0..=MAX_CORRECTIONS {
        let response = generate_case_response(selection.clone(), &prompt, cancel.clone()).await?;
        match parse_json_object(&response) {
            Ok(value) => {
                let mut scores = score_case(kind, &value);
                scores.push((
                    TrialCategory::StructuredJson,
                    if attempt == 0 { 100 } else { 60 },
                    if attempt == 0 {
                        "valid JSON first try".to_string()
                    } else {
                        "valid JSON after one correction".to_string()
                    },
                ));
                return Ok(TrialCaseOutcome {
                    scores,
                    latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    malformed,
                });
            }
            Err(error) => {
                malformed = true;
                if attempt >= MAX_CORRECTIONS {
                    return Err(TrialCaseError {
                        message: format!(
                            "Malformed structured output after bounded correction: {error}"
                        ),
                        cancelled: false,
                        malformed: true,
                        latency_ms: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    });
                }
                prompt = format!("{}\n\nYour previous response was invalid. Return ONLY the exact requested JSON object; no markdown or prose.",case_prompt(kind));
            }
        }
    }
    unreachable!()
}

fn aggregate_scores(
    outcomes: &[TrialCaseOutcome],
    attempted: u32,
    failed: u32,
    malformed: u32,
) -> Vec<TrialCategoryScore> {
    let mut grouped: HashMap<TrialCategory, Vec<(u8, String)>> = HashMap::new();
    for outcome in outcomes {
        for (category, score, evidence) in &outcome.scores {
            grouped
                .entry(*category)
                .or_default()
                .push((*score, evidence.clone()));
        }
    }
    let average_latency = if outcomes.is_empty() {
        0
    } else {
        outcomes
            .iter()
            .map(|outcome| outcome.latency_ms)
            .sum::<u64>()
            / outcomes.len() as u64
    };
    let speed = if average_latency == 0 {
        0
    } else if average_latency <= 2_000 {
        100
    } else if average_latency <= 4_000 {
        80
    } else if average_latency <= 8_000 {
        60
    } else if average_latency <= 15_000 {
        40
    } else {
        20
    };
    grouped
        .entry(TrialCategory::ResponseSpeed)
        .or_default()
        .push((speed, "average bounded case latency".to_string()));
    let reliability = if attempted == 0 {
        0
    } else {
        round_score(
            100.0
                - failed as f64 / attempted as f64 * 100.0
                - malformed as f64 / attempted as f64 * 25.0,
        )
    };
    grouped
        .entry(TrialCategory::Reliability)
        .or_default()
        .push((reliability, "failure/malformed-output rate".to_string()));
    let order = [
        TrialCategory::InstructionFollowing,
        TrialCategory::StructuredJson,
        TrialCategory::CodeUnderstanding,
        TrialCategory::Planning,
        TrialCategory::PatchReasoning,
        TrialCategory::ReviewQuality,
        TrialCategory::SecurityReasoning,
        TrialCategory::TestReasoning,
        TrialCategory::ResearchSummarization,
        TrialCategory::ContextHandling,
        TrialCategory::ResponseSpeed,
        TrialCategory::Reliability,
    ];
    order
        .into_iter()
        .filter_map(|category| {
            let entries = grouped.get(&category)?;
            let score = round_score(
                entries
                    .iter()
                    .map(|(score, _)| f64::from(*score))
                    .sum::<f64>()
                    / entries.len() as f64,
            );
            let mut unique = HashSet::new();
            let evidence = entries
                .iter()
                .filter_map(|(_, e)| {
                    if unique.insert(e.as_str()) {
                        Some(e.as_str())
                    } else {
                        None
                    }
                })
                .take(3)
                .collect::<Vec<_>>()
                .join("; ");
            Some(TrialCategoryScore {
                category,
                score,
                evidence,
            })
        })
        .collect()
}

async fn trial_one_model(
    identity: ModelIdentity,
    runtime: &RuntimeStatus,
    model: &LocalModelInfo,
    mode: TrialMode,
    cancel: watch::Receiver<bool>,
) -> ModelTrialResult {
    let selection = ModelSelection {
        provider: runtime.provider,
        model_id: model.id.clone(),
        endpoint: runtime.endpoint.clone(),
    };
    let mut outcomes = Vec::new();
    let mut failed = 0u32;
    let mut malformed = 0u32;
    let mut attempted = 0u32;
    let mut failure_reason = None;
    let mut cancelled = false;
    for kind in cases(mode) {
        if *cancel.borrow() {
            cancelled = true;
            failure_reason = Some("Trial cancelled.".to_string());
            break;
        }
        attempted += 1;
        match run_case(selection.clone(), *kind, cancel.clone()).await {
            Ok(outcome) => {
                if outcome.malformed {
                    malformed += 1;
                }
                outcomes.push(outcome);
            }
            Err(error) => {
                if error.malformed {
                    malformed += 1;
                }
                if error.cancelled {
                    cancelled = true;
                    failure_reason = Some("Trial cancelled.".to_string());
                    break;
                }
                failed += 1;
                failure_reason.get_or_insert(error.message);
                outcomes.push(TrialCaseOutcome {
                    scores: Vec::new(),
                    latency_ms: error.latency_ms,
                    malformed: error.malformed,
                });
            }
        }
    }
    let average_latency_ms = if outcomes.is_empty() {
        0
    } else {
        outcomes.iter().map(|o| o.latency_ms).sum::<u64>() / outcomes.len() as u64
    };
    let status = if cancelled {
        TrialModelStatus::Cancelled
    } else if failed == attempted && attempted > 0 {
        TrialModelStatus::Failed
    } else {
        TrialModelStatus::Completed
    };
    ModelTrialResult {
        identity,
        runtime_label: runtime.label.clone(),
        model_name: model.name.clone(),
        suite_version: TRIAL_SUITE_VERSION.to_string(),
        tested_at: now_millis(),
        mode,
        status,
        category_scores: aggregate_scores(&outcomes, attempted, failed, malformed),
        average_latency_ms,
        attempted_cases: attempted,
        failed_cases: failed,
        malformed_cases: malformed,
        failure_reason,
    }
}
fn replace_result(store: &mut TrialStore, result: ModelTrialResult) {
    if result.status == TrialModelStatus::Cancelled
        && store.results.iter().any(|existing| {
            existing.identity == result.identity && existing.status == TrialModelStatus::Completed
        })
    {
        return;
    }
    store
        .results
        .retain(|existing| existing.identity != result.identity);
    store.results.push(result);
    if store.results.len() > 60 {
        store
            .results
            .sort_by_key(|entry| std::cmp::Reverse(entry.tested_at));
        store.results.truncate(60);
    }
}
struct TrialGuard;
impl Drop for TrialGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = ACTIVE_TRIAL.get_or_init(|| Mutex::new(None)).lock() {
            *guard = None;
        }
        if let Ok(mut guard) = ACTIVE_MODEL.get_or_init(|| Mutex::new(None)).lock() {
            *guard = None;
        }
    }
}

pub(crate) async fn run_trial(
    app: &AppHandle,
    mode: TrialMode,
    selections: Vec<ModelSelection>,
) -> Result<ModelTrialSnapshot, String> {
    if selections.is_empty() {
        return Err("Select at least one discovered local model for Model Trial.".to_string());
    }
    if selections.len() > MAX_SELECTED_MODELS {
        return Err(format!(
            "Model Trial is limited to {MAX_SELECTED_MODELS} selected local models per run."
        ));
    }
    let hub = model_hub::snapshot(app).await?;
    let discovered = discovered_models(&hub);
    let mut unique = HashSet::new();
    let mut candidates = Vec::new();
    for selection in selections {
        let endpoint = model_hub::validate_loopback_endpoint(&selection.endpoint)?;
        let key = (
            selection.provider,
            selection.model_id.clone(),
            endpoint.clone(),
        );
        if !unique.insert(key) {
            continue;
        }
        let Some((identity, runtime, model)) = discovered.iter().find(|(_, runtime, model)| {
            runtime.provider == selection.provider
                && runtime.endpoint == endpoint
                && model.id == selection.model_id
        }) else {
            return Err(format!(
                "{} is not currently discovered through its configured local runtime.",
                selection.model_id
            ));
        };
        candidates.push((identity.clone(), (*runtime).clone(), (*model).clone()));
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    {
        let mut active = ACTIVE_TRIAL
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| "Model Trial state is unavailable.".to_string())?;
        if active.is_some() {
            return Err(
                "A Model Trial is already running. Cancel or wait for it to finish.".to_string(),
            );
        }
        *active = Some(cancel_tx);
    }
    let _guard = TrialGuard;
    for (identity, runtime, model) in candidates {
        if *cancel_rx.borrow() {
            break;
        }
        if let Ok(mut current) = ACTIVE_MODEL.get_or_init(|| Mutex::new(None)).lock() {
            *current = Some(identity.clone());
        }
        let result = trial_one_model(identity, &runtime, &model, mode, cancel_rx.clone()).await;
        let cancelled = result.status == TrialModelStatus::Cancelled;
        let mut store = load_store(app)?;
        replace_result(&mut store, result);
        if cancelled {
            store.last_cancelled_at = Some(now_millis());
        }
        save_store(app, &store)?;
        if cancelled {
            break;
        }
        if let Ok(mut current) = ACTIVE_MODEL.get_or_init(|| Mutex::new(None)).lock() {
            *current = None;
        }
    }
    let refreshed = model_hub::snapshot(app).await?;
    snapshot_with_hub(app, &refreshed)
}

pub(crate) fn cancel_trial(app: &AppHandle) -> Result<bool, String> {
    let sender = ACTIVE_TRIAL
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "Model Trial state is unavailable.".to_string())?
        .clone();
    let Some(sender) = sender else {
        return Ok(false);
    };
    let _ = sender.send(true);
    let mut store = load_store(app)?;
    store.last_cancelled_at = Some(now_millis());
    save_store(app, &store)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_hub::{
        BooleanCapability, CapabilitySource, ModelCapabilities, NumberCapability,
    };
    fn identity(id: &str) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProviderId::Ollama,
            model_id: id.to_string(),
            endpoint: "http://127.0.0.1:11434".to_string(),
            runtime_version: Some("1".to_string()),
            metadata_fingerprint: format!("fingerprint-{id}"),
        }
    }
    fn result(id: &str, scores: &[(TrialCategory, u8)]) -> ModelTrialResult {
        ModelTrialResult {
            identity: identity(id),
            runtime_label: "Ollama".to_string(),
            model_name: id.to_string(),
            suite_version: TRIAL_SUITE_VERSION.to_string(),
            tested_at: 1,
            mode: TrialMode::Full,
            status: TrialModelStatus::Completed,
            category_scores: scores
                .iter()
                .map(|(category, score)| TrialCategoryScore {
                    category: *category,
                    score: *score,
                    evidence: "fixture".to_string(),
                })
                .collect(),
            average_latency_ms: 1000,
            attempted_cases: 7,
            failed_cases: 0,
            malformed_cases: 0,
            failure_reason: None,
        }
    }
    fn hub_with(ids: &[&str]) -> ModelHubSnapshot {
        let runtime = RuntimeStatus {
            provider: ModelProviderId::Ollama,
            label: "Ollama".to_string(),
            endpoint: "http://127.0.0.1:11434".to_string(),
            reachable: true,
            models: ids
                .iter()
                .map(|id| LocalModelInfo {
                    id: (*id).to_string(),
                    name: (*id).to_string(),
                    provider: ModelProviderId::Ollama,
                    runtime_label: "Ollama".to_string(),
                    size_bytes: None,
                    parameter_size: None,
                    quantization: None,
                    loaded: None,
                    capabilities: ModelCapabilities {
                        chat: BooleanCapability {
                            value: Some(true),
                            source: CapabilitySource::Reported,
                        },
                        tool_calling: BooleanCapability {
                            value: None,
                            source: CapabilitySource::Unknown,
                        },
                        structured_output: BooleanCapability {
                            value: Some(true),
                            source: CapabilitySource::Reported,
                        },
                        vision: BooleanCapability {
                            value: None,
                            source: CapabilitySource::Unknown,
                        },
                        context_window: NumberCapability {
                            value: Some(8192),
                            source: CapabilitySource::Reported,
                        },
                    },
                })
                .collect(),
            version: Some("1".to_string()),
            message: "ready".to_string(),
            diagnostics: None,
            checked_at: 1,
        };
        ModelHubSnapshot {
            runtimes: vec![runtime],
            selected_model: None,
            available_model_count: ids.len(),
            connected_runtime_count: 1,
            refreshed_at: 1,
        }
    }

    #[test]
    fn quick_and_full_trials_are_fixed_and_bounded() {
        assert_eq!(cases(TrialMode::Quick).len(), 4);
        assert_eq!(cases(TrialMode::Full).len(), 7);
    }
    #[test]
    fn deterministic_json_rules_score_expected_identifiers() {
        let value = json!({"issues":["path_traversal","symlink_escape"]});
        let scores = score_case(TrialCaseKind::Security, &value);
        assert!(scores
            .iter()
            .any(|(c, s, _)| *c == TrialCategory::SecurityReasoning && *s == 100));
        assert!(parse_json_object("not json").is_err());
    }
    #[test]
    fn suite_version_mismatch_is_stale() {
        let mut trial = result("one", &[(TrialCategory::Reliability, 100)]);
        trial.suite_version = "old-suite".to_string();
        let hub = ModelHubSnapshot {
            runtimes: Vec::new(),
            selected_model: None,
            available_model_count: 0,
            connected_runtime_count: 0,
            refreshed_at: 1,
        };
        assert!(staleness(&trial, &hub).unwrap().contains("incompatible"));
    }
    #[test]
    fn cancelled_retrial_does_not_erase_completed_result() {
        let mut store = TrialStore {
            results: vec![result("one", &[(TrialCategory::Reliability, 100)])],
            last_cancelled_at: None,
        };
        let mut cancelled = result("one", &[]);
        cancelled.status = TrialModelStatus::Cancelled;
        replace_result(&mut store, cancelled);
        assert_eq!(store.results.len(), 1);
        assert_eq!(store.results[0].status, TrialModelStatus::Completed);
    }
    #[test]
    fn exact_identity_change_is_not_reused() {
        let hub = hub_with(&["coder"]);
        let (identity, _, _) = discovered_models(&hub).into_iter().next().unwrap();
        let mut trial = result("coder", &[(TrialCategory::Reliability, 100)]);
        trial.identity = identity;
        assert!(staleness(&trial, &hub).is_none());

        trial.identity.runtime_version = Some("2".to_string());
        assert!(staleness(&trial, &hub)
            .as_deref()
            .is_some_and(|reason| reason.contains("changed")));
    }
}
