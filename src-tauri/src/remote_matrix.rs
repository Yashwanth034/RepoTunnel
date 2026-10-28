//! Durable cross-machine native test dispatch. A run is first persisted as
//! UNKNOWN-before-send, never automatically replayed after lost acknowledgements.
//! Tests execute only through existing remote RepoTunnel managed-process tools.
use crate::{models::Workspace, recovery_store, remote_snapshot, remote_sync};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{path::BaseDirectory, AppHandle, Manager};

const STORE: &str = "remote-native-test-runs.json";
const MAX_RUNS: usize = 32;
const MAX_STORE_SIZE: u64 = 128 * 1024;
const MAX_JOBS: usize = 4;
static LOCK: Mutex<()> = Mutex::new(());
/// Serializes local native-matrix dispatch planning through the final receipt.
/// Does not claim a cross-machine hardware reservation or survive a second
/// independent RepoTunnel coordinator process.
static DISPATCH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
fn dispatch_lock() -> &'static tokio::sync::Mutex<()> {
    DISPATCH_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[derive(Clone, Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct JobSpec {
    /// Required native operating system: windows, macos, or linux.
    /// Validated against the other computer's OS, not trusted as a label.
    pub(crate) platform: String,
    pub(crate) node_id: String,
    pub(crate) remote_workspace_id: String,
    /// Native test command executed inside the remote RepoTunnel sandbox.
    pub(crate) command: String,
    pub(crate) cwd: Option<String>,
    /// Optional minimum bytes of physically available RAM measured on the remote host.
    pub(crate) min_available_ram_bytes: Option<u64>,
    /// Optional minimum free bytes on the remote workspace filesystem.
    pub(crate) min_available_disk_bytes: Option<u64>,
    /// Requires a positively identified GPU; does not attest GPU driver or SDK readiness.
    pub(crate) require_gpu: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Job {
    platform: String,
    #[serde(default)]
    architecture: Option<String>,
    node_id: String,
    remote_workspace_id: String,
    process_id: Option<String>,
    state: String,
    /// A process exit is factual execution evidence, not proof of native UX.
    evidence: String,
    /// Last independent read-only remote observation of a cancellation.
    #[serde(default)]
    cancellation_observation: Option<String>,
    #[serde(default)]
    cancellation_observed_at: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Run {
    id: String,
    owner_hash: String,
    created_at: u64,
    local_workspace_id: String,
    source_sha256: String,
    #[serde(default)]
    dispatch_complete: bool,
    jobs: Vec<Job>,
    limitations: String,
}
#[derive(Default, Serialize, Deserialize)]
struct Runs {
    runs: Vec<Run>,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or(0)
}
fn owner_hash(owner: &str) -> String {
    let digest = Sha256::digest(owner.as_bytes());
    digest.iter().map(|part| format!("{part:02x}")).collect()
}
fn path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(STORE, BaseDirectory::AppData)
        .map_err(|_| "Cannot resolve private native test run store.".to_string())
}
fn prune_settled_history(data: &mut Runs) -> Result<(), String> {
    while data.runs.len() > MAX_RUNS {
        let index = data.runs.iter().position(|run| {
            run.dispatch_complete
                && !run.jobs.is_empty()
                && run.jobs.iter().all(|job| finished_native_job(&job.state))
        }).ok_or_else(|| "Remote native test history is full of unresolved jobs; no saved active run will be discarded. Reconcile/retain evidence before starting another matrix.".to_string())?;
        data.runs.remove(index);
    }
    Ok(())
}
fn load(path: &std::path::Path) -> Result<Runs, String> {
    let previous = path.with_file_name(format!(".{STORE}.previous"));
    for candidate in [path, previous.as_path()] {
        match fs::symlink_metadata(candidate) {
            Ok(meta) if !meta.file_type().is_file() || meta.len() > MAX_STORE_SIZE => {
                return Err("Invalid bounded native test run store.".into())
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("Could not inspect native test run store.".into())
            }
            _ => {}
        }
    }
    let recovered = recovery_store::load_json::<Runs>(path, "remote native test runs")?;
    // Native dispatches/cancellations are irreversible external operations.
    // Rolling back to an older backup can erase a persisted UNKNOWN-before-
    // send intent or re-enable a cancelled process for another stop attempt.
    // Never silently recover previous job state; preserve the old file for
    // explicit human forensics and fail closed until the primary is repaired.
    if recovered.source == recovery_store::JsonRecoverySource::Previous {
        return Err("Native test run store cannot fall back to a stale previous copy: that could replay a command or cancellation with an unknown remote outcome. Preserve the stored evidence and repair it locally before continuing.".into());
    }
    let mut data = recovered.value.unwrap_or_default();
    prune_settled_history(&mut data)?;
    Ok(data)
}
fn update(app: &AppHandle, owner: &str, run: &Run) -> Result<(), String> {
    let _guard = LOCK
        .lock()
        .map_err(|_| "Native test run store is unavailable.")?;
    let path = path(app)?;
    let mut stored = load(&path)?;
    if run.owner_hash != owner_hash(owner) {
        return Err("Native test run owner mismatch.".into());
    }
    if let Some(old) = stored.runs.iter_mut().find(|item| item.id == run.id) {
        if old.owner_hash != run.owner_hash {
            return Err("Native test run is owned by another AI.".into());
        }
        let mut next = run.clone();
        // An in-flight refresh must not roll back a durable cancellation
        // receipt recorded while it was waiting on remote network IO.
        for (incoming, saved) in next.jobs.iter_mut().zip(old.jobs.iter()) {
            if saved.state.starts_with("cancel") && !incoming.state.starts_with("cancel") {
                *incoming = saved.clone();
            } else if saved.state.starts_with("cancel")
                && saved.cancellation_observed_at.unwrap_or(0)
                    > incoming.cancellation_observed_at.unwrap_or(0)
            {
                incoming.cancellation_observation = saved.cancellation_observation.clone();
                incoming.cancellation_observed_at = saved.cancellation_observed_at;
            }
        }
        *old = next;
    } else {
        stored.runs.push(run.clone());
    }
    prune_settled_history(&mut stored)?;
    recovery_store::save_json(&path, &stored, "remote native test runs")
}
/// Human-only local dashboard: no owner hash, no credentials, no command
/// replay, no remote network or process control. AI owner-scoped history
/// continues to be served separately by list().
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DesktopRun {
    id: String,
    created_at: u64,
    local_workspace_id: String,
    dispatch_complete: bool,
    jobs: Vec<Job>,
    limitations: String,
}
impl From<&Run> for DesktopRun {
    fn from(run: &Run) -> Self {
        Self {
            id: run.id.clone(),
            created_at: run.created_at,
            local_workspace_id: run.local_workspace_id.clone(),
            dispatch_complete: run.dispatch_complete,
            jobs: run.jobs.clone(),
            limitations: run.limitations.clone(),
        }
    }
}
pub(crate) fn list_for_desktop(app: &AppHandle) -> Result<Vec<DesktopRun>, String> {
    let _guard = LOCK
        .lock()
        .map_err(|_| "Native test run store unavailable.")?;
    let runs = load(&path(app)?)?;
    Ok(runs
        .runs
        .iter()
        .rev()
        .take(20)
        .map(DesktopRun::from)
        .collect())
}
pub(crate) fn list(app: &AppHandle, owner: &str) -> Result<Vec<Run>, String> {
    let _guard = LOCK
        .lock()
        .map_err(|_| "Native test run store unavailable.")?;
    let stored = load(&path(app)?)?;
    Ok(stored
        .runs
        .into_iter()
        .filter(|run| run.owner_hash == owner_hash(owner))
        .collect())
}
fn required_platform(label: &str) -> Option<&'static str> {
    match label.to_ascii_lowercase().as_str() {
        "windows" => Some("windows"),
        "macos" | "mac" => Some("macos"),
        "linux" => Some("linux"),
        _ => None,
    }
}
fn verify_platform(spec: &JobSpec, remote: &Value) -> Result<String, String> {
    let requested = required_platform(&spec.platform).ok_or_else(|| {
        "Native test jobs must use Windows, macOS or Linux as their platform.".to_string()
    })?;
    let reported = remote.get("platform").and_then(Value::as_str);
    if reported != Some(requested) {
        return Err(format!(
            "Remote node did not attest the requested native platform {requested}; cannot label a test as running on that OS."
        ));
    }
    remote
        .get("architecture")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .map(str::to_string)
        .ok_or_else(|| "Remote native CPU architecture is unavailable.".to_string())
}
fn verify_resources(spec: &JobSpec, resources: &Value) -> Result<(), String> {
    for (minimum, field, label) in [
        (spec.min_available_ram_bytes, "memoryAvailableBytes", "RAM"),
        (
            spec.min_available_disk_bytes,
            "workspaceDiskAvailableBytes",
            "disk space",
        ),
    ] {
        if let Some(required) = minimum {
            let available = resources.get(field).and_then(Value::as_u64).ok_or_else(|| {
                format!("Remote {label} availability is unknown; cannot certify the requested minimum.")
            })?;
            if available < required {
                return Err(format!(
                    "Remote {label} is below the requested minimum; native job not dispatched."
                ));
            }
        }
    }
    if spec.require_gpu.unwrap_or(false)
        && (resources.get("gpuAvailable").and_then(Value::as_bool) != Some(true)
            || !resources
                .get("gpuDevices")
                .and_then(Value::as_array)
                .is_some_and(|devices| {
                    devices
                        .iter()
                        .any(|name| name.as_str().is_some_and(|s| !s.is_empty()))
                }))
    {
        return Err("A positively identified remote GPU was required but is not available.".into());
    }
    Ok(())
}
fn finished_native_job(state: &str) -> bool {
    matches!(
        state,
        "commandExitedZero"
            | "commandFailed"
            | "commandStopped"
            | "cancelConfirmed"
            | "cancelAlreadyNotRunning"
            | "sourceChangedBeforeDispatch"
            | "preDispatchInspectionUnavailable"
            | "resourceUnavailableBeforeDispatch"
            | "remoteCapacityUnverifiedBeforeDispatch"
    )
}
fn ensure_no_prior_active_jobs(runs: &Runs, specs: &[JobSpec]) -> Result<(), String> {
    for run in &runs.runs {
        for job in &run.jobs {
            if specs.iter().any(|spec| spec.node_id == job.node_id)
                && (!run.dispatch_complete || !finished_native_job(&job.state))
            {
                return Err(format!(
                    "Remote machine {} already has an unresolved native matrix job in run {} ({}). Inspect/reconcile the previous process before dispatching another matrix; no commands were sent.",
                    job.node_id, run.id, job.state
                ));
            }
        }
    }
    Ok(())
}
fn validate_specs(specs: &[JobSpec]) -> Result<(), String> {
    if specs.is_empty() || specs.len() > MAX_JOBS {
        return Err("Provide 1–4 explicit native test jobs.".into());
    }
    let mut unique = BTreeSet::new();
    for spec in specs {
        if spec.node_id.is_empty()
            || spec.node_id.len() > 128
            || spec.remote_workspace_id.is_empty()
            || spec.remote_workspace_id.len() > 256
            || required_platform(&spec.platform).is_none()
            || spec.platform.len() > 64
            || spec.command.is_empty()
            || spec.command.len() > 8192
            || spec.cwd.as_deref().is_some_and(|cwd| cwd.len() > 512)
            || spec.min_available_ram_bytes == Some(0)
            || spec.min_available_disk_bytes == Some(0)
            // Without per-node resource reservations, even distinct commands
            // on one machine must not launch concurrently in the same matrix.
            || !unique.insert(&spec.node_id)
        {
            return Err(
                "Invalid native test job, or repeated remote machine in one matrix. Use one job per node until resource reservations are implemented.".into(),
            );
        }
    }
    Ok(())
}
/// Detect an existing pending/running native-matrix process in the exact
/// paired remote project. The remote runtime lists active processes first,
/// with a bounded 100-process response. This is only a read-before-write
/// safety check, not an atomic lease or cross-project reservation.
fn ensure_no_remote_native_matrix_process(
    remote: &Value,
    expected_workspace: &str,
) -> Result<(), String> {
    if remote.get("workspaceId").and_then(Value::as_str) != Some(expected_workspace) {
        return Err("Remote process capacity receipt did not match the paired project; native dispatch refused.".into());
    }
    let records = remote
        .get("processes")
        .and_then(Value::as_array)
        .filter(|records| records.len() <= 100)
        .ok_or_else(|| {
            "Remote process inventory was missing, malformed or unbounded; native dispatch refused."
                .to_string()
        })?;
    for record in records {
        if record.get("workspaceId").and_then(Value::as_str) != Some(expected_workspace) {
            return Err("Remote runtime exposed a process outside the approved project; native dispatch refused.".into());
        }
        let label = record
            .get("label")
            .and_then(Value::as_str)
            .ok_or_else(|| "Remote process label could not be verified.".to_string())?;
        let state = record
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| "Remote process status could not be verified.".to_string())?;
        if !matches!(
            state,
            "pending" | "running" | "exited" | "stopped" | "failed" | "rejected"
        ) {
            return Err(
                "Remote process status is not recognized; no native dispatch was sent.".into(),
            );
        }
        if label.starts_with("Native Matrix ") && matches!(state, "pending" | "running") {
            return Err("Another native-matrix process is already pending/running in this paired remote project. Reconcile that process before launching another; no native command was sent.".into());
        }
    }
    Ok(())
}
async fn remote_native_capacity_check(spec: &JobSpec, owner: &str) -> Result<(), String> {
    let receipt = remote_sync::remote_call(
        &spec.node_id,
        owner,
        "get_workspace_runtime_status",
        json!({"workspace_id":spec.remote_workspace_id, "process_limit":100}),
    )
    .await?;
    ensure_no_remote_native_matrix_process(&receipt, &spec.remote_workspace_id)
}
fn validate_remote_snapshot(
    local: &remote_snapshot::Snapshot,
    remote: &Value,
) -> Result<(), String> {
    if local.incomplete || local.skipped_special > 0 {
        return Err("Local portable source fingerprint is incomplete or includes unsupported special files; refusing native test dispatch.".into());
    }
    let expected = local.fingerprint_sha256.as_str();
    if remote.get("fingerprintSha256").and_then(Value::as_str) != Some(expected)
        || remote.get("incomplete").and_then(Value::as_bool) != Some(false)
        || remote.get("skippedSpecial").and_then(Value::as_u64) != Some(0)
        || remote.get("files").and_then(Value::as_u64) != Some(local.files as u64)
        || remote.get("bytes").and_then(Value::as_u64) != Some(local.bytes)
    {
        return Err("Native test blocked: remote approved source snapshot does not match the local working tree, or is incomplete. Synchronize and verify source before dispatch.".into());
    }
    Ok(())
}
fn make_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| "Could not create secure native run ID.".to_string())?;
    Ok(format!(
        "native-{}",
        bytes.iter().map(|n| format!("{n:02x}")).collect::<String>()
    ))
}
pub(crate) async fn start(
    app: AppHandle,
    workspace: Workspace,
    owner: String,
    specs: Vec<JobSpec>,
) -> Result<Run, String> {
    validate_specs(&specs)?;
    let _exclusive_dispatch = dispatch_lock().lock().await;
    // A new batch must not silently oversubscribe nodes occupied by an older
    // durable matrix. Unknown outcomes remain occupied until independently
    // reconciled; this is conservative local scheduling, not a remote lease.
    {
        let _store_guard = LOCK
            .lock()
            .map_err(|_| "Native test run store is unavailable.")?;
        let previous = load(&path(&app)?)?;
        ensure_no_prior_active_jobs(&previous, &specs)?;
    }
    let local = tokio::task::spawn_blocking(move || remote_snapshot::manifest(&workspace))
        .await
        .map_err(|_| "Could not compute local source fingerprint.".to_string())??;
    if local.incomplete || local.skipped_special != 0 {
        return Err("Local source snapshot is incomplete; native test dispatch is blocked.".into());
    }
    // Preflight ALL targets before any remote command is sent.
    // Platform claims must be obtained from the remote native runtime, not
    // taken from an AI-supplied "Windows"/"Mac" label.
    let mut architectures = Vec::with_capacity(specs.len());
    for spec in &specs {
        // Discovery catches live native jobs from a different coordinator
        // that share this exact approved remote project. A separate receiver
        // process/project still needs a real global reservation protocol.
        remote_native_capacity_check(spec, &owner).await?;
        let remote = remote_sync::remote_call(
            &spec.node_id,
            &owner,
            "get_workspace_source_snapshot",
            json!({"workspace_id": spec.remote_workspace_id}),
        )
        .await?;
        validate_remote_snapshot(&local, &remote)?;
        let resources = remote_sync::remote_call(
            &spec.node_id,
            &owner,
            "get_system_resources",
            json!({"workspace_id": spec.remote_workspace_id}),
        )
        .await?;
        architectures.push(verify_platform(spec, &resources)?);
        verify_resources(spec, &resources)?;
    }
    let id = make_id()?;
    let mut run = Run {
        id, owner_hash: owner_hash(&owner), created_at: now(),
        local_workspace_id: local.workspace_id.clone(),
        source_sha256: local.fingerprint_sha256.clone(),
        dispatch_complete: false,
        jobs: specs.iter().zip(architectures.iter()).map(|(spec, arch)| Job {
            platform: required_platform(&spec.platform).unwrap_or("unknown").into(),
            architecture: Some(arch.clone()),
            node_id: spec.node_id.clone(), remote_workspace_id: spec.remote_workspace_id.clone(),
            process_id: None, state: "notDispatched".into(),
            evidence: "Source hash matched before dispatch; execution has not started.".into(),
            cancellation_observation: None,
            cancellation_observed_at: None,
        }).collect(),
        limitations: "Source parity was measured before dispatch but is not an immutable filesystem lock. A remote process can outlive network sessions. Exit code 0 proves only command completion, not native GUI behavior or installation. No automatic retry on uncertain dispatch.".into(),
    };
    update(&app, &owner, &run)?;
    for (index, spec) in specs.iter().enumerate() {
        // The initial multi-node preflight is not a filesystem lock. Recheck
        // this worker just before launching its process: previous jobs, humans
        // or build hooks might have modified its working tree meanwhile.
        let fresh = remote_sync::remote_call(
            &spec.node_id,
            &owner,
            "get_workspace_source_snapshot",
            json!({"workspace_id":spec.remote_workspace_id}),
        )
        .await;
        match fresh {
            Ok(ref source) if validate_remote_snapshot(&local, source).is_ok() => {}
            Ok(_) => {
                run.jobs[index].state = "sourceChangedBeforeDispatch".into();
                run.jobs[index].evidence =
                    "Remote source no longer matches the approved local snapshot; this native job was NOT dispatched.".into();
                update(&app, &owner, &run)?;
                continue;
            }
            Err(_) => {
                run.jobs[index].state = "preDispatchInspectionUnavailable".into();
                run.jobs[index].evidence =
                    "Could not revalidate remote source before launch; no process was dispatched."
                        .into();
                update(&app, &owner, &run)?;
                continue;
            }
        }
        if spec.min_available_ram_bytes.is_some()
            || spec.min_available_disk_bytes.is_some()
            || spec.require_gpu.unwrap_or(false)
        {
            let refreshed_resources = remote_sync::remote_call(
                &spec.node_id,
                &owner,
                "get_system_resources",
                json!({"workspace_id":spec.remote_workspace_id}),
            )
            .await;
            if !refreshed_resources.is_ok_and(|resources| {
                verify_platform(spec, &resources).is_ok()
                    && verify_resources(spec, &resources).is_ok()
            }) {
                run.jobs[index].state = "resourceUnavailableBeforeDispatch".into();
                run.jobs[index].evidence = "Remote resource requirements could not be reverified immediately before launch. No process was dispatched.".into();
                update(&app, &owner, &run)?;
                continue;
            }
        }
        if remote_native_capacity_check(spec, &owner).await.is_err() {
            run.jobs[index].state = "remoteCapacityUnverifiedBeforeDispatch".into();
            run.jobs[index].evidence =
                "Remote native capacity was occupied or could not be reverified just before launch. No command was dispatched.".into();
            update(&app, &owner, &run)?;
            continue;
        }
        run.jobs[index].state = "dispatchUnknown".into();
        run.jobs[index].evidence =
            "Intent persisted before sending. Do not replay this job blindly after a failure."
                .into();
        update(&app, &owner, &run)?;
        let started = remote_sync::remote_call(
            &spec.node_id,
            &owner,
            "start_process",
            json!({
                "workspace_id":spec.remote_workspace_id,
                "command":spec.command,
                "cwd":spec.cwd,
                "label":format!("Native Matrix {} {}",run.id,spec.platform),
            }),
        )
        .await;
        match started {
            Ok(receipt) => {
                let process_id = receipt
                    .get("process")
                    .and_then(|p| p.get("id"))
                    .or_else(|| receipt.get("id"))
                    .and_then(Value::as_str);
                run.jobs[index].process_id = process_id.map(str::to_string);
                run.jobs[index].state =
                    if receipt.get("queued").and_then(Value::as_bool) == Some(true) {
                        "reviewQueued".into()
                    } else if process_id.is_some() {
                        "submitted".into()
                    } else {
                        "dispatchUnknown".into()
                    };
                run.jobs[index].evidence = if process_id.is_some() {
                    "Remote managed process receipt recorded; still awaiting factual exit.".into()
                } else {
                    "Remote response did not provide an unambiguous process ID. Inspect remote before retrying.".into()
                };
            }
            Err(_) => {
                run.jobs[index].state = "dispatchUnknown".into();
                run.jobs[index].evidence = "Remote dispatch was rejected, timed out or lost its receipt. Inspect remote state before retrying.".into();
            }
        }
        update(&app, &owner, &run)?;
    }
    run.dispatch_complete = true;
    update(&app, &owner, &run)?;
    Ok(run)
}
// A cancellation is eligible only after dispatch has settled and a real
// process receipt is known. In-flight/ambiguous dispatches must be inspected,
// not stopped or replayed using guessed process identities.
fn cancellable_job(run: &Run, index: usize, expected_process: &str) -> Result<Job, String> {
    if expected_process.is_empty() || expected_process.len() > 128 {
        return Err(
            "Supply the exact remote managed process ID from the native matrix receipt.".into(),
        );
    }
    if !run.dispatch_complete {
        return Err("Cannot cancel while matrix dispatch is incomplete or ambiguous. Inspect the remote state first.".into());
    }
    let job = run
        .jobs
        .get(index)
        .ok_or_else(|| "Native matrix job index is out of range.".to_string())?;
    if job.process_id.as_deref() != Some(expected_process) {
        return Err("Remote process ID differs from the matrix's stored process receipt.".into());
    }
    if !matches!(job.state.as_str(), "submitted" | "running" | "unverified") {
        return Err("This native job is not eligible for a new cancellation attempt.".into());
    }
    Ok(job.clone())
}
fn process_matches_matrix_id(run_id: &str, job: &Job, receipt: &Value) -> bool {
    receipt.get("id").and_then(Value::as_str) == job.process_id.as_deref()
        && receipt.get("workspaceId").and_then(Value::as_str)
            == Some(job.remote_workspace_id.as_str())
        && receipt.get("label").and_then(Value::as_str)
            == Some(format!("Native Matrix {} {}", run_id, job.platform).as_str())
}
fn process_matches_job(run: &Run, job: &Job, receipt: &Value) -> bool {
    process_matches_matrix_id(&run.id, job, receipt)
}
fn cancel_status_class(record: &Value) -> &'static str {
    match record.get("status").and_then(Value::as_str) {
        Some("running") => "running",
        Some("exited" | "stopped" | "failed" | "rejected") => "terminal",
        Some("pending") => "pending",
        _ => "unknown",
    }
}
fn cancellation_inspection(run_id: &str, job: &Job, record: &Value) -> String {
    if !process_matches_matrix_id(run_id, job, record) {
        return "Remote process identity, project or native matrix label did not match; cancellation outcome cannot be trusted.".into();
    }
    let status = record
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let status = match status {
        "running" | "stopped" | "exited" | "failed" | "pending" | "rejected" => status,
        _ => "unknown",
    };
    let exit = record
        .get("exitCode")
        .and_then(Value::as_i64)
        .map_or_else(|| "unknown".to_string(), |code| code.to_string());
    format!("Read-only remote inspection: recorded process is {status} (exit code {exit}). An observed stop is not proof that the original cancellation request succeeded. Do not automatically replay an unknown stop.")
}
#[expect(
    clippy::too_many_arguments,
    reason = "Durable cancellation transitions require explicit run, owner, job, process and state preconditions"
)]
fn set_cancel_state(
    app: &AppHandle,
    owner: &str,
    run_id: &str,
    index: usize,
    process_id: &str,
    required_state: Option<&str>,
    next_state: &str,
    evidence: &str,
) -> Result<Run, String> {
    let _guard = LOCK
        .lock()
        .map_err(|_| "Native test run store unavailable.")?;
    let store_path = path(app)?;
    let mut stored = load(&store_path)?;
    let run = stored
        .runs
        .iter_mut()
        .find(|item| item.id == run_id && item.owner_hash == owner_hash(owner))
        .ok_or_else(|| "Native matrix run is not owned by this AI session.".to_string())?;
    if required_state.is_none() {
        cancellable_job(run, index, process_id)?;
    }
    let job = run
        .jobs
        .get_mut(index)
        .ok_or_else(|| "Native matrix job index is out of range.".to_string())?;
    if job.process_id.as_deref() != Some(process_id)
        || required_state.is_some_and(|state| job.state != state)
    {
        return Err(
            "Native cancellation state changed; inspect the saved run before trying again.".into(),
        );
    }
    job.state = next_state.into();
    job.evidence = evidence.into();
    let output = run.clone();
    recovery_store::save_json(&store_path, &stored, "remote native test runs")?;
    Ok(output)
}
/// Persist human cancellation intent and bounded state transitions under
/// the same private native-run store lock as AI cancellation. A retry cannot
/// pass the original cancellable precondition after the intent is saved.
fn set_desktop_cancel_state(
    app: &AppHandle,
    run_id: &str,
    index: usize,
    process_id: &str,
    required_state: Option<&str>,
    next_state: &str,
    evidence: &str,
) -> Result<DesktopRun, String> {
    let _guard = LOCK
        .lock()
        .map_err(|_| "Native test history is unavailable.".to_string())?;
    let store_path = path(app)?;
    let mut stored = load(&store_path)?;
    let run = stored
        .runs
        .iter_mut()
        .find(|run| run.id == run_id)
        .ok_or_else(|| "Saved native run no longer exists.".to_string())?;
    if required_state.is_none() {
        cancellable_job(run, index, process_id)?;
    }
    let job = run
        .jobs
        .get_mut(index)
        .ok_or_else(|| "Saved native job index changed.".to_string())?;
    if job.process_id.as_deref() != Some(process_id)
        || required_state.is_some_and(|required| job.state != required)
    {
        return Err("Saved native job state changed: do not repeat the stop request.".into());
    }
    job.state = next_state.into();
    job.evidence = evidence.into();
    let view = DesktopRun::from(&*run);
    recovery_store::save_json(&store_path, &stored, "remote native test runs")?;
    Ok(view)
}
/// Human-clicked, one-shot graceful native job cancellation. No automatic
/// retries. The intent survives crashes before the first remote mutation.
pub(crate) async fn cancel_for_desktop(
    app: &AppHandle,
    run_id: &str,
    index: usize,
    expected_process_id: &str,
) -> Result<DesktopRun, String> {
    let _dispatch_guard = dispatch_lock().lock().await;
    let pending = set_desktop_cancel_state(
        app, run_id, index, expected_process_id, None,
        "cancelRequested",
        "Human native stop intent durably recorded before any remote call. Do not retry an ambiguous stop.",
    )?;
    let job = pending.jobs[index].clone();
    let observation = crate::remote_fleet::inspect_native_process(
        &job.node_id,
        &job.remote_workspace_id,
        expected_process_id,
    )
    .await;
    let record = match observation {
        Ok(Some(record)) => record,
        _ => return set_desktop_cancel_state(
            app, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelInspectionUnknown",
            "The original remote process could not be verified; no stop sent. Inspect status, do not retry blindly.",
        ),
    };
    if !process_matches_matrix_id(run_id, &job, &record) {
        return set_desktop_cancel_state(
            app,
            run_id,
            index,
            expected_process_id,
            Some("cancelRequested"),
            "cancelIdentityRejected",
            "Original managed process label, identity or approved project changed. No stop sent.",
        );
    }
    match cancel_status_class(&record) {
        "running" => {},
        "terminal" => return set_desktop_cancel_state(
            app, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelAlreadyNotRunning",
            "Verified original process has already terminated; no stop sent.",
        ),
        "pending" => return set_desktop_cancel_state(
            app, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelReviewPending",
            "Process awaits human approval and may run later. No stop sent; pending review remains unresolved.",
        ),
        _ => return set_desktop_cancel_state(
            app, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelInspectionUnknown",
            "Remote process status is not verifiable. No stop sent; do not retry.",
        ),
    }
    let stop = crate::remote_fleet::stop_saved_native_process(
        &job.node_id,
        &job.remote_workspace_id,
        expected_process_id,
    )
    .await;
    let confirmed = stop.is_ok_and(|receipt| {
        process_matches_matrix_id(run_id, &job, &receipt)
            && receipt.get("status").and_then(Value::as_str) == Some("stopped")
    });
    set_desktop_cancel_state(
        app,
        run_id,
        index,
        expected_process_id,
        Some("cancelRequested"),
        if confirmed {
            "cancelConfirmed"
        } else {
            "cancelOutcomeUnknown"
        },
        if confirmed {
            "Original managed process returned a verified stopped receipt for this graceful stop. Native GUI/test acceptance is not claimed."
        } else {
            "Stop was sent, but its exact acknowledgement was lost or not verifiable. Outcome unknown: inspect only; never replay automatically."
        },
    )
}
/// Stop one *confirmed* managed process. No blind replay: an interrupted
/// dispatch or stop remains unknown until independently inspected by the human.
pub(crate) async fn cancel(
    app: &AppHandle,
    owner: &str,
    run_id: &str,
    index: usize,
    expected_process_id: &str,
) -> Result<Run, String> {
    let run = set_cancel_state(app, owner, run_id, index, expected_process_id, None,
        "cancelRequested",
        "Cancellation intent persisted. Awaiting remote process identity verification; never retry blindly.")?;
    let job = run.jobs[index].clone();
    let status = remote_sync::remote_call(
        &job.node_id,
        owner,
        "get_workspace_runtime_status",
        json!({"workspace_id":job.remote_workspace_id,"process_limit":100}),
    )
    .await;
    let Some(record) = status
        .ok()
        .and_then(|value| value.get("processes").and_then(Value::as_array).cloned())
        .and_then(|items| {
            items
                .into_iter()
                .find(|item| item.get("id").and_then(Value::as_str) == job.process_id.as_deref())
        })
    else {
        return set_cancel_state(app, owner, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelInspectionUnknown",
            "Remote process could not be verified. No stop sent; inspect remote status, do not retry blindly.");
    };
    if !process_matches_job(&run, &job, &record) {
        return set_cancel_state(
            app,
            owner,
            run_id,
            index,
            expected_process_id,
            Some("cancelRequested"),
            "cancelIdentityRejected",
            "Remote process identity, project or matrix label mismatch. No stop command sent.",
        );
    }
    match cancel_status_class(&record) {
        "running" => {}
        "terminal" => return set_cancel_state(app, owner, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelAlreadyNotRunning",
            "Remote process is in a verified terminal state. No stop command sent; inspect exit status."),
        "pending" => return set_cancel_state(app, owner, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelReviewPending",
            "Remote process is pending human review and may execute later. No stop command sent; keep this machine reserved until the review is resolved."),
        _ => return set_cancel_state(app, owner, run_id, index, expected_process_id,
            Some("cancelRequested"), "cancelInspectionUnknown",
            "Remote process status is unknown; no stop command sent and no other job should be dispatched."),
    }
    let stopped = remote_sync::remote_call(
        &job.node_id, owner, "stop_process",
        json!({"workspace_id":job.remote_workspace_id,"process_id":expected_process_id,"force":false}),
    ).await;
    match stopped {
        Ok(record) if process_matches_job(&run, &job, &record)
            && record.get("status").and_then(Value::as_str) == Some("stopped") =>
            set_cancel_state(app, owner, run_id, index, expected_process_id,
                Some("cancelRequested"), "cancelConfirmed",
                "Remote managed-process stop receipt confirmed. This does not prove native GUI or test acceptance."),
        _ => set_cancel_state(app, owner, run_id, index, expected_process_id,
                Some("cancelRequested"), "cancelOutcomeUnknown",
                "Stop was sent but acknowledgement was missing or not a verified stopped receipt. Inspect remote state; do not replay."),
    }
}

/// Update only the local durable observation after a HUMAN-initiated verified
/// status read. No remote stop/start/retry is implied. Cancellation state is
/// never promoted based on an independent status observation.
fn desktop_job_observation(run_id: &str, job: &mut Job, record: &Value) -> Result<(), String> {
    if !process_matches_matrix_id(run_id, job, record) {
        return Err("Saved native test process, project or run label did not match the remote receipt; no evidence was changed.".into());
    }
    if job.state.starts_with("cancel") {
        job.cancellation_observation = Some(cancellation_inspection(run_id, job, record));
        job.cancellation_observed_at = Some(now());
    } else {
        // A later bounded history snapshot may be stale: never revoke a
        // previously observed successful/failed terminal receipt for a new
        // reported pending/running status.
        if finished_native_job(&job.state) && !matches!(cancel_status_class(record), "terminal") {
            return Err("A previously settled native test cannot regress to an unverified or running receipt.".into());
        }
        describe_process(job, record);
    }
    Ok(())
}
/// Human-only durable native job reconciliation. The remote action is exactly
/// one authenticated, scoped, read-only process-history request; it never
/// acquires an AI owner, replays a job or sends stop_process.
pub(crate) async fn reconcile_for_desktop(
    app: &AppHandle,
    run_id: &str,
    job_index: usize,
    expected_process_id: &str,
) -> Result<DesktopRun, String> {
    let _dispatch_guard = dispatch_lock().lock().await;
    let original = {
        let _guard = LOCK
            .lock()
            .map_err(|_| "Native test history is unavailable.".to_string())?;
        let stored = load(&path(app)?)?;
        let run = stored
            .runs
            .iter()
            .find(|run| run.id == run_id)
            .ok_or_else(|| "The selected native test run no longer exists.".to_string())?;
        if !run.dispatch_complete {
            return Err(
                "Native dispatch is incomplete; this job's outcome requires manual review.".into(),
            );
        }
        let job = run
            .jobs
            .get(job_index)
            .ok_or_else(|| "The saved native test job does not exist.".to_string())?;
        if job.process_id.as_deref() != Some(expected_process_id)
            || expected_process_id.is_empty()
            || expected_process_id.len() > 128
        {
            return Err("Native inspection must use the exact saved managed process ID.".into());
        }
        job.clone()
    };
    let record = crate::remote_fleet::inspect_native_process(
        &original.node_id, &original.remote_workspace_id, expected_process_id,
    ).await?.ok_or_else(|| "Saved process is not in the latest 100 remote receipts. No old evidence was overwritten; inspect the remote process directly.".to_string())?;
    let _guard = LOCK
        .lock()
        .map_err(|_| "Native test history is unavailable.".to_string())?;
    let store_path = path(app)?;
    let mut stored = load(&store_path)?;
    let run = stored
        .runs
        .iter_mut()
        .find(|run| run.id == run_id)
        .ok_or_else(|| "Native test run disappeared during inspection.".to_string())?;
    if !run.dispatch_complete {
        return Err(
            "Native dispatch changed while inspection ran; no stale evidence was saved.".into(),
        );
    }
    let job = run
        .jobs
        .get_mut(job_index)
        .ok_or_else(|| "Saved native job changed during inspection.".to_string())?;
    if job.process_id != original.process_id
        || job.node_id != original.node_id
        || job.remote_workspace_id != original.remote_workspace_id
        || job.platform != original.platform
        || job.state != original.state
    {
        return Err("Saved native job changed while checking the remote status. Refresh history and inspect again.".into());
    }
    desktop_job_observation(run_id, job, &record)?;
    let result = DesktopRun::from(&*run);
    recovery_store::save_json(&store_path, &stored, "remote native test runs")?;
    Ok(result)
}
fn describe_verified_process(run_id: &str, job: &mut Job, record: &Value) {
    if !process_matches_matrix_id(run_id, job, record) {
        job.state = "unverified".into();
        job.evidence = "Remote process ID, approved project, or native matrix run label did not match the saved job; remote status cannot be attributed to this test.".into();
        return;
    }
    describe_process(job, record);
}
fn describe_process(job: &mut Job, record: &Value) {
    let status = record
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let code = record.get("exitCode").and_then(Value::as_i64);
    job.state = match (status, code) {
        ("exited", Some(0)) => "commandExitedZero".into(),
        ("exited", _) | ("failed", _) | ("rejected", _) => "commandFailed".into(),
        ("stopped", _) => "commandStopped".into(),
        ("running", _) => "running".into(),
        ("pending", _) => "reviewPending".into(),
        _ => "unverified".into(),
    };
    job.evidence=format!(
        "Observed remote managed process status {status}, exit code {}. Verify build output/logs and native GUI behavior separately.",
        code.map_or("unknown".to_string(), |value| value.to_string())
    );
}
pub(crate) async fn refresh(app: &AppHandle, owner: &str, run_id: &str) -> Result<Run, String> {
    // A read-only status refresh must not load an incomplete matrix, wait on
    // remote IO, then publish that stale copy over the dispatch receipts being
    // durably written by start(). Use the same coordinator scheduling gate.
    let _dispatch_guard = dispatch_lock().lock().await;
    let stored = list(app, owner)?;
    let mut run = stored
        .into_iter()
        .find(|run| run.id == run_id)
        .ok_or_else(|| "Native test run does not exist for this AI owner.".to_string())?;
    for job in &mut run.jobs {
        let cancelling = job.state.starts_with("cancel");
        let Some(process_id) = job.process_id.as_ref() else {
            continue;
        };
        let result = remote_sync::remote_call(
            &job.node_id,
            owner,
            "get_workspace_runtime_status",
            json!({"workspace_id": job.remote_workspace_id, "process_limit": 100}),
        )
        .await;
        match result {
            Ok(receipt) => {
                let observed =
                    receipt
                        .get("processes")
                        .and_then(Value::as_array)
                        .and_then(|items| {
                            items.iter().find(|item| {
                                item.get("id").and_then(Value::as_str) == Some(process_id)
                            })
                        });
                if let Some(record) = observed {
                    if cancelling {
                        job.cancellation_observation =
                            Some(cancellation_inspection(run_id, job, record));
                        job.cancellation_observed_at = Some(now());
                        // Preserve the original uncertain stop outcome.
                    } else {
                        describe_verified_process(run_id, job, record);
                    }
                } else if cancelling {
                    job.cancellation_observation = Some(
                        "Recorded process not present in the bounded remote process history. Cancellation outcome remains uncertain; do not replay.".into()
                    );
                    job.cancellation_observed_at = Some(now());
                } else {
                    job.state = "unverified".into();
                    job.evidence = "Remote process was not found within the bounded history; outcome remains unverified.".into();
                }
            }
            Err(_) => {
                if cancelling {
                    // Keep the last verified independent observation intact
                    // during temporary network errors or machine sleep.
                    if job.cancellation_observation.is_none() {
                        job.cancellation_observation = Some(
                            "Remote machine unavailable for read-only cancellation inspection; outcome still unknown.".into()
                        );
                    }
                } else {
                    job.state = "unverified".into();
                    job.evidence = "Cannot inspect remote job currently; reconnect the same node and refresh. Do not replay.".into();
                }
            }
        }
    }
    update(app, owner, &run)?;
    // Another caller may have recorded a cancellation while this read-only
    // refresh waited on network IO. Return the canonical persisted state,
    // not the older snapshot that was protected from overwrite by update().
    list(app, owner)?
        .into_iter()
        .find(|saved| saved.id == run_id)
        .ok_or_else(|| "Native matrix run disappeared during status refresh.".to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_native_run_receipts_omit_ai_owner_and_source_fingerprint() {
        let run = Run {
            id: "native-fixture".into(),
            owner_hash: "secret-ai-owner-hash".into(),
            created_at: 9,
            local_workspace_id: "local-ws".into(),
            source_sha256: "private-source-hash".into(),
            dispatch_complete: true,
            jobs: vec![Job {
                platform: "macos".into(),
                architecture: Some("aarch64".into()),
                node_id: "node-test".into(),
                remote_workspace_id: "remote-ws".into(),
                process_id: None,
                state: "cancelOutcomeUnknown".into(),
                evidence: "Stop outcome uncertain".into(),
                cancellation_observation: None,
                cancellation_observed_at: None,
            }],
            limitations: "Actual native GUI unverified".into(),
        };
        let view = DesktopRun::from(&run);
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(encoded.contains("cancelOutcomeUnknown"));
        assert!(encoded.contains("Actual native GUI unverified"));
        assert!(!encoded.contains("secret-ai-owner-hash"));
        assert!(!encoded.contains("private-source-hash"));
        assert!(!encoded.contains("ownerHash"));
        assert!(!encoded.contains("sourceSha256"));
    }
    #[test]
    fn desktop_reconciliation_saves_only_verified_factual_evidence_and_preserves_cancellation() {
        let template = || Job {
            platform: "linux".into(),
            architecture: Some("x86_64".into()),
            node_id: "node".into(),
            remote_workspace_id: "approved".into(),
            process_id: Some("process-5".into()),
            state: "submitted".into(),
            evidence: "Pending factual exit".into(),
            cancellation_observation: None,
            cancellation_observed_at: None,
        };
        let confirmed = json!({
            "id":"process-5","workspaceId":"approved",
            "label":"Native Matrix run-17 linux","status":"exited","exitCode":0
        });
        let mut job = template();
        desktop_job_observation("run-17", &mut job, &confirmed).unwrap();
        assert_eq!(job.state, "commandExitedZero");
        assert!(job.evidence.contains("exit code 0"));
        let bad = json!({
            "id":"process-5","workspaceId":"foreign",
            "label":"Native Matrix run-17 linux","status":"exited","exitCode":0
        });
        let mut blocked = template();
        assert!(desktop_job_observation("run-17", &mut blocked, &bad).is_err());
        assert_eq!(blocked.state, "submitted");
        let mut cancelled = template();
        cancelled.state = "cancelOutcomeUnknown".into();
        desktop_job_observation("run-17", &mut cancelled, &confirmed).unwrap();
        assert_eq!(cancelled.state, "cancelOutcomeUnknown");
        assert!(cancelled
            .cancellation_observation
            .as_deref()
            .unwrap()
            .contains("not proof"));
        assert!(cancelled.cancellation_observed_at.is_some());
        let mut settled = template();
        settled.state = "commandExitedZero".into();
        let stale = json!({
            "id":"process-5","workspaceId":"approved",
            "label":"Native Matrix run-17 linux","status":"running","exitCode":null
        });
        assert!(desktop_job_observation("run-17", &mut settled, &stale).is_err());
        assert_eq!(settled.state, "commandExitedZero");
        let mut stopped = template();
        desktop_job_observation(
            "run-17",
            &mut stopped,
            &json!({"id":"process-5","workspaceId":"approved",
                "label":"Native Matrix run-17 linux","status":"stopped"}),
        )
        .unwrap();
        assert_eq!(stopped.state, "commandStopped");
        assert!(finished_native_job(&stopped.state));
        let mut pending = template();
        desktop_job_observation(
            "run-17",
            &mut pending,
            &json!({"id":"process-5","workspaceId":"approved",
                "label":"Native Matrix run-17 linux","status":"pending"}),
        )
        .unwrap();
        assert_eq!(pending.state, "reviewPending");
        assert!(!finished_native_job(&pending.state));
    }
    #[test]
    fn desktop_stop_intent_prevents_replaying_an_uncertain_stop() {
        let run = Run {
            id: "native-one-shot".into(),
            owner_hash: owner_hash("owner"),
            created_at: 1,
            local_workspace_id: "local".into(),
            source_sha256: "sha".into(),
            dispatch_complete: true,
            jobs: vec![Job {
                platform: "linux".into(),
                architecture: None,
                node_id: "paired-node".into(),
                remote_workspace_id: "approved".into(),
                process_id: Some("process-1".into()),
                state: "running".into(),
                evidence: "Running".into(),
                cancellation_observation: None,
                cancellation_observed_at: None,
            }],
            limitations: String::new(),
        };
        assert!(cancellable_job(&run, 0, "process-1").is_ok());
        assert!(cancellable_job(&run, 0, "process-2").is_err());
        for guarded in [
            "cancelRequested",
            "cancelOutcomeUnknown",
            "cancelInspectionUnknown",
            "cancelIdentityRejected",
            "cancelReviewPending",
            "cancelConfirmed",
            "cancelAlreadyNotRunning",
            "commandStopped",
            "reviewPending",
        ] {
            let mut saved = run.clone();
            saved.jobs[0].state = guarded.into();
            assert!(
                cancellable_job(&saved, 0, "process-1").is_err(),
                "{guarded} must never permit an automatic or double manual retry"
            );
        }
    }
    #[test]
    fn cancellation_requires_completed_dispatch_exact_process_and_ownership_metadata() {
        let mut run = Run {
            id: "native-test-1".into(),
            owner_hash: owner_hash("test-owner"),
            created_at: 1,
            local_workspace_id: "local".into(),
            source_sha256: "abc".into(),
            dispatch_complete: false,
            jobs: vec![Job {
                platform: "windows".into(),
                architecture: Some("x86_64".into()),
                node_id: "node-1".into(),
                remote_workspace_id: "remote-ws".into(),
                process_id: Some("process-1".into()),
                state: "submitted".into(),
                evidence: "source matched".into(),
                cancellation_observation: None,
                cancellation_observed_at: None,
            }],
            limitations: "native acceptance pending".into(),
        };
        assert!(cancellable_job(&run, 0, "process-1").is_err());
        run.dispatch_complete = true;
        assert!(cancellable_job(&run, 0, "process-1").is_ok());
        assert!(cancellable_job(&run, 0, "process-2").is_err());
        assert!(cancellable_job(&run, 1, "process-1").is_err());
        let job = cancellable_job(&run, 0, "process-1").unwrap();
        let valid = json!({
            "id":"process-1", "workspaceId":"remote-ws",
            "label":"Native Matrix native-test-1 windows", "status":"running"
        });
        assert!(process_matches_job(&run, &job, &valid));
        let observed_running = cancellation_inspection(&run.id, &job, &valid);
        assert!(observed_running.contains("running"));
        assert!(observed_running.contains("Do not automatically replay"));
        let observed_stopped = cancellation_inspection(
            &run.id,
            &job,
            &json!({"id":"process-1","workspaceId":"remote-ws",
                "label":"Native Matrix native-test-1 windows","status":"stopped","exitCode":null}),
        );
        assert!(observed_stopped.contains("stopped"));
        assert!(observed_stopped.contains("not proof"));
        for mutation in [
            json!({"id":"process-1","workspaceId":"other",
                "label":"Native Matrix native-test-1 windows","status":"running"}),
            json!({"id":"process-1","workspaceId":"remote-ws",
                "label":"unrelated command","status":"running"}),
            json!({"id":"process-2","workspaceId":"remote-ws",
                "label":"Native Matrix native-test-1 windows","status":"running"}),
        ] {
            assert!(!process_matches_job(&run, &job, &mutation));
            assert!(cancellation_inspection(&run.id, &job, &mutation).contains("did not match"));
        }
        run.jobs[0].state = "cancelRequested".into();
        assert!(cancellable_job(&run, 0, "process-1").is_err());
        run.jobs[0].state = "dispatchUnknown".into();
        assert!(cancellable_job(&run, 0, "process-1").is_err());
        run.jobs[0].state = "reviewQueued".into();
        assert!(cancellable_job(&run, 0, "process-1").is_err());
    }

    #[test]
    fn remote_resource_limits_are_positive_optional_and_fail_closed_when_unknown() {
        let spec = JobSpec {
            platform: "linux".into(),
            node_id: "node-1".into(),
            remote_workspace_id: "remote".into(),
            command: "cargo test".into(),
            cwd: None,
            min_available_ram_bytes: Some(4 * 1024 * 1024 * 1024),
            min_available_disk_bytes: Some(8 * 1024 * 1024 * 1024),
            require_gpu: Some(true),
        };
        let healthy = json!({
            "memoryAvailableBytes":8_u64 * 1024 * 1024 * 1024,
            "workspaceDiskAvailableBytes":24_u64 * 1024 * 1024 * 1024,
            "gpuAvailable":true, "gpuDevices":["NVIDIA GPU"]
        });
        assert!(validate_specs(std::slice::from_ref(&spec)).is_ok());
        assert!(verify_resources(&spec, &healthy).is_ok());
        assert!(verify_resources(&spec, &json!({})).is_err());
        assert!(verify_resources(
            &spec,
            &json!({
                "memoryAvailableBytes":10,"workspaceDiskAvailableBytes":24_u64 * 1024 * 1024 * 1024,
                "gpuAvailable":true,"gpuDevices":["GPU"]
            })
        )
        .is_err());
        assert!(verify_resources(
            &spec,
            &json!({
                "memoryAvailableBytes":8_u64 * 1024 * 1024 * 1024,
                "workspaceDiskAvailableBytes":24_u64 * 1024 * 1024 * 1024,
                "gpuAvailable":true,"gpuDevices":[]
            })
        )
        .is_err());
        let no_requirement = JobSpec {
            min_available_ram_bytes: None,
            min_available_disk_bytes: None,
            require_gpu: Some(false),
            ..spec.clone()
        };
        assert!(verify_resources(&no_requirement, &json!({})).is_ok());
        let invalid = JobSpec {
            min_available_ram_bytes: Some(0),
            ..spec
        };
        assert!(validate_specs(&[invalid]).is_err());
    }

    #[test]
    fn stale_run_backup_never_reenables_a_cancelled_or_ambiguous_process() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STORE);
        let mut run = Run {
            id: "native-rollback-check".into(),
            owner_hash: owner_hash("owner"),
            created_at: 1,
            local_workspace_id: "local".into(),
            source_sha256: "sha".into(),
            dispatch_complete: true,
            jobs: vec![Job {
                platform: "linux".into(),
                architecture: Some("x86_64".into()),
                node_id: "node".into(),
                remote_workspace_id: "remote".into(),
                process_id: Some("process-1".into()),
                state: "running".into(),
                evidence: "old running receipt".into(),
                cancellation_observation: None,
                cancellation_observed_at: None,
            }],
            limitations: String::new(),
        };
        recovery_store::save_json(
            &path,
            &Runs {
                runs: vec![run.clone()],
            },
            "remote native test runs",
        )
        .unwrap();
        run.jobs[0].state = "cancelOutcomeUnknown".into();
        recovery_store::save_json(&path, &Runs { runs: vec![run] }, "remote native test runs")
            .unwrap();
        assert_eq!(
            load(&path).unwrap().runs[0].jobs[0].state,
            "cancelOutcomeUnknown"
        );
        // A generic recovery loader would offer the old, still-cancellable
        // "running" state. Native job safety must refuse that rollback.
        std::fs::write(&path, b"{invalid-json").unwrap();
        assert!(load(&path)
            .err()
            .expect("corrupt primary must fail closed")
            .contains("stale previous copy"));
        std::fs::remove_file(&path).unwrap();
        assert!(load(&path)
            .err()
            .expect("missing primary must fail closed")
            .contains("stale previous copy"));
    }
    #[test]
    fn history_capacity_never_discards_unresolved_native_dispatch() {
        let make_run = |index: usize, state: &str, dispatch_complete: bool| Run {
            id: format!("native-saved-{index}"),
            owner_hash: owner_hash("owner"),
            created_at: index as u64,
            local_workspace_id: "local".into(),
            source_sha256: "sha".into(),
            dispatch_complete,
            jobs: vec![Job {
                platform: "linux".into(),
                architecture: Some("x86_64".into()),
                node_id: format!("node-{index}"),
                remote_workspace_id: "remote".into(),
                process_id: Some(format!("process-{index}")),
                state: state.into(),
                evidence: String::new(),
                cancellation_observation: None,
                cancellation_observed_at: None,
            }],
            limitations: String::new(),
        };
        let mut runs = Runs {
            runs: (0..MAX_RUNS)
                .map(|index| {
                    make_run(
                        index,
                        if index == 0 {
                            "running"
                        } else {
                            "commandExitedZero"
                        },
                        true,
                    )
                })
                .collect(),
        };
        runs.runs.push(make_run(MAX_RUNS, "submitted", true));
        prune_settled_history(&mut runs).unwrap();
        assert_eq!(runs.runs.len(), MAX_RUNS);
        assert!(runs.runs.iter().any(|run| run.id == "native-saved-0"));
        assert!(runs
            .runs
            .iter()
            .any(|run| run.id == format!("native-saved-{MAX_RUNS}")));
        assert!(!runs.runs.iter().any(|run| run.id == "native-saved-1"));
        let mut unresolved = Runs {
            runs: (0..=MAX_RUNS)
                .map(|index| make_run(index, "dispatchUnknown", true))
                .collect(),
        };
        assert!(prune_settled_history(&mut unresolved).is_err());
        assert_eq!(unresolved.runs.len(), MAX_RUNS + 1);
        unresolved.runs[0].jobs[0].state = "commandExitedZero".into();
        unresolved.runs[0].dispatch_complete = false;
        assert!(prune_settled_history(&mut unresolved).is_err());
        unresolved.runs[0].dispatch_complete = true;
        prune_settled_history(&mut unresolved).unwrap();
        assert_eq!(unresolved.runs.len(), MAX_RUNS);
    }
    #[test]
    fn native_capacity_read_prevents_obvious_concurrent_runs_from_other_coordinators() {
        let receipt = json!({
            "workspaceId":"remote",
            "processes":[
                {"id":"running-job", "workspaceId":"remote",
                    "label":"Native Matrix other-coordinator linux","status":"running"},
                {"id":"terminal-job","workspaceId":"remote",
                    "label":"Native Matrix old-run linux","status":"exited"}
            ],
        });
        let err = ensure_no_remote_native_matrix_process(&receipt, "remote")
            .expect_err("another coordinator already has a running job");
        assert!(err.contains("already pending/running"));
        let mut only_terminal = receipt.clone();
        only_terminal["processes"][0]["status"] = Value::String("stopped".into());
        assert!(ensure_no_remote_native_matrix_process(&only_terminal, "remote").is_ok());
        only_terminal["processes"][0]["status"] = Value::String("pending".into());
        assert!(ensure_no_remote_native_matrix_process(&only_terminal, "remote").is_err());
        only_terminal["processes"][0]["label"] = Value::String("Unrelated user task".into());
        assert!(ensure_no_remote_native_matrix_process(&only_terminal, "remote").is_ok());
        only_terminal["processes"][1]["workspaceId"] = Value::String("foreign".into());
        assert!(ensure_no_remote_native_matrix_process(&only_terminal, "remote").is_err());
        assert!(ensure_no_remote_native_matrix_process(&receipt, "foreign").is_err());
        assert!(
            ensure_no_remote_native_matrix_process(&json!({"workspaceId":"remote"}), "remote")
                .is_err()
        );
        let invalid = json!({"workspaceId":"remote","processes":[
            {"workspaceId":"remote","label":"Native Matrix unknown","status":"unrecognized"}
        ]});
        assert!(ensure_no_remote_native_matrix_process(&invalid, "remote").is_err());
        assert!(
            finished_native_job("remoteCapacityUnverifiedBeforeDispatch"),
            "no-send preflight state must be terminal, unlike an uncertain dispatch"
        );
    }
    #[test]
    fn native_matrix_rejects_multiple_commands_targeting_one_unreserved_machine() {
        let mut first = JobSpec {
            platform: "linux".into(),
            node_id: "node-1".into(),
            remote_workspace_id: "remote-1".into(),
            command: "cargo test".into(),
            cwd: None,
            min_available_ram_bytes: Some(1024),
            min_available_disk_bytes: None,
            require_gpu: None,
        };
        let mut second = first.clone();
        second.command = "npm run build".into();
        assert!(validate_specs(&[first.clone(), second.clone()]).is_err());
        // Different paired projects on one physical node still share the
        // same memory, CPU and GPU, so treating them as separate capacity
        // reservations would be unsafe.
        second.remote_workspace_id = "remote-2".into();
        assert!(validate_specs(&[first.clone(), second.clone()]).is_err());
        // Distinct nodes can be checked independently.
        second.node_id = "node-2".into();
        assert!(validate_specs(&[first.clone(), second]).is_ok());
        first.node_id.clear();
        assert!(validate_specs(&[first]).is_err());
    }
    #[test]
    fn separate_matrices_cannot_oversubscribe_an_unresolved_remote_node() {
        let spec = JobSpec {
            platform: "linux".into(),
            node_id: "node-one".into(),
            remote_workspace_id: "remote-one".into(),
            command: "cargo test".into(),
            cwd: None,
            min_available_ram_bytes: None,
            min_available_disk_bytes: None,
            require_gpu: None,
        };
        let saved_job = Job {
            platform: "linux".into(),
            architecture: Some("x86_64".into()),
            node_id: "node-one".into(),
            remote_workspace_id: "remote-one".into(),
            process_id: Some("process-one".into()),
            state: "running".into(),
            evidence: "still active".into(),
            cancellation_observation: None,
            cancellation_observed_at: None,
        };
        let mut previous = Runs {
            runs: vec![Run {
                id: "previous-run".into(),
                owner_hash: owner_hash("owner"),
                created_at: 1,
                local_workspace_id: "local".into(),
                source_sha256: "sha".into(),
                dispatch_complete: true,
                jobs: vec![saved_job],
                limitations: String::new(),
            }],
        };
        let refusal = ensure_no_prior_active_jobs(&previous, std::slice::from_ref(&spec))
            .expect_err("unresolved running job must block another native batch");
        assert!(refusal.contains("previous-run"));
        assert!(refusal.contains("no commands were sent"));
        let other = JobSpec {
            node_id: "node-two".into(),
            ..spec.clone()
        };
        assert!(ensure_no_prior_active_jobs(&previous, &[other]).is_ok());
        for unresolved in [
            "submitted",
            "reviewQueued",
            "dispatchUnknown",
            "unverified",
            "cancelRequested",
            "cancelOutcomeUnknown",
            "cancelInspectionUnknown",
            "cancelIdentityRejected",
            "reviewPending",
        ] {
            previous.runs[0].jobs[0].state = unresolved.into();
            assert!(ensure_no_prior_active_jobs(&previous, std::slice::from_ref(&spec)).is_err());
        }
        for complete in [
            "commandExitedZero",
            "commandFailed",
            "commandStopped",
            "cancelConfirmed",
            "cancelAlreadyNotRunning",
            "sourceChangedBeforeDispatch",
            "preDispatchInspectionUnavailable",
            "resourceUnavailableBeforeDispatch",
        ] {
            previous.runs[0].jobs[0].state = complete.into();
            assert!(ensure_no_prior_active_jobs(&previous, std::slice::from_ref(&spec)).is_ok());
        }
        previous.runs[0].dispatch_complete = false;
        assert!(ensure_no_prior_active_jobs(&previous, &[spec]).is_err());
    }
    #[test]
    fn preflight_refuses_mismatched_missing_or_incomplete_source() {
        let local = remote_snapshot::Snapshot::test_fixture("abc", 2, 30);
        let ok = json!({"fingerprintSha256":"abc", "incomplete":false,
                        "skippedSpecial":0, "files":2, "bytes":30});
        assert!(validate_remote_snapshot(&local, &ok).is_ok());
        assert!(
            validate_remote_snapshot(&local, &json!({"fingerprintSha256":"different"})).is_err()
        );
        assert!(validate_remote_snapshot(
            &local,
            &json!({"fingerprintSha256":"abc",
            "incomplete":true,"skippedSpecial":0,"files":2,"bytes":30})
        )
        .is_err());
    }
    #[test]
    fn cancelling_pending_review_cannot_free_a_remote_machine_capacity_slot() {
        for status in [
            "running", "pending", "exited", "stopped", "failed", "rejected", "unknown",
        ] {
            let result = cancel_status_class(&json!({"status":status}));
            assert_eq!(
                result,
                match status {
                    "running" => "running",
                    "pending" => "pending",
                    "exited" | "stopped" | "failed" | "rejected" => "terminal",
                    _ => "unknown",
                }
            );
        }
        assert_eq!(cancel_status_class(&json!({})), "unknown");
        assert!(!finished_native_job("cancelReviewPending"));
        assert!(!finished_native_job("cancelInspectionUnknown"));
        assert!(finished_native_job("cancelAlreadyNotRunning"));
    }
    #[test]
    fn remote_process_status_requires_real_exit_code() {
        let mut job = Job {
            platform: "windows".into(),
            architecture: None,
            node_id: "node".into(),
            remote_workspace_id: "ws".into(),
            process_id: Some("process-1".into()),
            state: "submitted".into(),
            evidence: String::new(),
            cancellation_observation: None,
            cancellation_observed_at: None,
        };
        describe_process(&mut job, &json!({"status":"running","exitCode":null}));
        assert_eq!(job.state, "running");
        describe_process(&mut job, &json!({"status":"exited","exitCode":0}));
        assert_eq!(job.state, "commandExitedZero");
        describe_process(&mut job, &json!({"status":"exited","exitCode":1}));
        assert_eq!(job.state, "commandFailed");
        describe_process(&mut job, &json!({"status":"exited","exitCode":null}));
        assert_eq!(job.state, "commandFailed");
        describe_process(&mut job, &json!({"status":"stopped","exitCode":null}));
        assert_eq!(job.state, "commandStopped");
        assert!(finished_native_job(&job.state));
        assert!(job.evidence.contains("status stopped"));
    }
    #[test]
    fn refresh_process_status_requires_matching_id_project_and_matrix_label() {
        let new_job = || Job {
            platform: "windows".into(),
            architecture: Some("x86_64".into()),
            node_id: "node-1".into(),
            remote_workspace_id: "approved-remote".into(),
            process_id: Some("process-77".into()),
            state: "running".into(),
            evidence: "last status running".into(),
            cancellation_observation: None,
            cancellation_observed_at: None,
        };
        let receipt = json!({
            "id":"process-77", "workspaceId":"approved-remote",
            "label":"Native Matrix native-run-2 windows",
            "status":"exited","exitCode":0
        });
        let mut verified = new_job();
        describe_verified_process("native-run-2", &mut verified, &receipt);
        assert_eq!(verified.state, "commandExitedZero");
        assert!(verified.evidence.contains("status exited"));
        for mismatch in [
            json!({"id":"process-77","workspaceId":"other-project",
                "label":"Native Matrix native-run-2 windows","status":"exited","exitCode":0}),
            json!({"id":"process-77","workspaceId":"approved-remote",
                "label":"Unrelated task","status":"exited","exitCode":0}),
            json!({"id":"process-78","workspaceId":"approved-remote",
                "label":"Native Matrix native-run-2 windows","status":"exited","exitCode":0}),
            json!({"id":"process-77","workspaceId":"approved-remote",
                "status":"exited","exitCode":0}),
        ] {
            let mut job = new_job();
            describe_verified_process("native-run-2", &mut job, &mismatch);
            assert_eq!(job.state, "unverified");
            assert!(job.evidence.contains("did not match"));
            assert!(!job.evidence.contains("status exited"));
        }
    }
    #[test]
    fn native_os_must_be_attested_not_inferred_from_requested_label() {
        let base = JobSpec {
            platform: "windows".into(),
            node_id: "node".into(),
            remote_workspace_id: "approved".into(),
            command: "cargo test".into(),
            cwd: None,
            min_available_ram_bytes: None,
            min_available_disk_bytes: None,
            require_gpu: None,
        };
        assert_eq!(
            verify_platform(
                &base,
                &json!({"platform":"windows","architecture":"x86_64"})
            )
            .unwrap(),
            "x86_64"
        );
        assert!(
            verify_platform(&base, &json!({"platform":"linux","architecture":"x86_64"})).is_err()
        );
        assert!(verify_platform(&base, &json!({"architecture":"x86_64"})).is_err());
        assert!(validate_specs(&[JobSpec {
            platform: "Solaris".into(),
            ..base
        }])
        .is_err());
    }

    #[test]
    fn bounded_job_specs_prevent_unexpected_fanout() {
        let sample = JobSpec {
            platform: "windows".into(),
            node_id: "node-a".into(),
            remote_workspace_id: "ws-a".into(),
            command: "cargo test".into(),
            cwd: None,
            min_available_ram_bytes: None,
            min_available_disk_bytes: None,
            require_gpu: None,
        };
        assert!(validate_specs(std::slice::from_ref(&sample)).is_ok());
        assert!(validate_specs(&[sample.clone(), sample.clone()]).is_err());
        assert!(validate_specs(&vec![sample; 5]).is_err());
    }
}
