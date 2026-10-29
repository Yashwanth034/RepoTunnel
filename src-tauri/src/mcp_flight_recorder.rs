//! Privacy-minimal MCP transport diagnostics. This records only fixed-category
//! metadata, never caller-supplied headers, paths, query strings, IPs, bodies,
//! tool names, arguments, credentials, or response contents.
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex, OnceLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::recovery_store;

const FILE: &str = "mcp-flight-recorder.json";
const MAX_EVENTS: usize = 256;
const MAX_SHOWN: usize = 100;
const SAVE_EVERY_MS: u64 = 30_000;
const MAX_FILE_BYTES: u64 = 128 * 1024;
static RECORDER: OnceLock<Arc<Recorder>> = OnceLock::new();

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|span| u64::try_from(span.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Surface {
    Gateway,
    DirectHttps,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Route {
    Mcp,
    Health,
    OAuth,
    Other,
}

pub(crate) fn route(path: &str) -> Route {
    if path == "/mcp" || path.starts_with("/mcp/") {
        Route::Mcp
    } else if path == "/health" {
        Route::Health
    } else if matches!(path, "/token" | "/register" | "/authorize")
        || path.starts_with("/.well-known/oauth-")
    {
        Route::OAuth
    } else {
        Route::Other
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum RequestMethod {
    Get,
    Post,
    Delete,
    Other,
}
pub(crate) fn method(value: &str) -> RequestMethod {
    match value {
        "GET" => RequestMethod::Get,
        "POST" => RequestMethod::Post,
        "DELETE" => RequestMethod::Delete,
        _ => RequestMethod::Other,
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Stage {
    ResponseHeaders,
    UpstreamConnect,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Outcome {
    HeadersReturned,
    Redirect,
    BadRequest,
    AuthChallenge,
    AccessDenied,
    NotFound,
    UpstreamFailure,
    ServiceUnavailable,
    ServerFailure,
    OtherClientError,
}
fn outcome(status: u16, stage: Stage) -> Outcome {
    if stage == Stage::UpstreamConnect {
        return Outcome::UpstreamFailure;
    }
    match status {
        200..=299 => Outcome::HeadersReturned,
        300..=399 => Outcome::Redirect,
        400 => Outcome::BadRequest,
        401 => Outcome::AuthChallenge,
        403 => Outcome::AccessDenied,
        404 => Outcome::NotFound,
        502 | 504 => Outcome::UpstreamFailure,
        503 => Outcome::ServiceUnavailable,
        500..=599 => Outcome::ServerFailure,
        _ => Outcome::OtherClientError,
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Event {
    id: u64,
    timestamp_ms: u64,
    surface: Surface,
    route: Route,
    method: RequestMethod,
    stage: Stage,
    outcome: Outcome,
    http_status: u16,
    response_header_latency_ms: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Stored {
    events: VecDeque<Event>,
}
#[derive(Debug)]
struct State {
    stored: Stored,
    next_id: u64,
    last_snapshot_ms: u64,
    dropped_snapshots: u64,
}
struct Recorder {
    state: Mutex<State>,
    sender: Option<SyncSender<Stored>>,
    storage_healthy: Arc<AtomicBool>,
}

fn load_store(path: &Path) -> Result<Stored, String> {
    // The recovery loader may fall back to a previous copy; bound and inspect
    // BOTH files before parsing either one.
    let previous = path.with_file_name(format!(
        ".{}.previous",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(FILE),
    ));
    for candidate in [path, previous.as_path()] {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_BYTES => {
                return Err("MCP diagnostics file is not a regular bounded file.".to_string());
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err("MCP diagnostics file cannot be inspected.".to_string());
            }
            _ => {}
        }
    }
    let mut store = recovery_store::load_json::<Stored>(path, "MCP diagnostics")?
        .value
        .unwrap_or_default();
    while store.events.len() > MAX_EVENTS {
        store.events.pop_front();
    }
    Ok(store)
}

impl Recorder {
    fn new(path: Option<PathBuf>) -> Arc<Self> {
        let (stored, healthy) = match path.as_ref() {
            Some(path) => match load_store(path) {
                Ok(value) => (value, true),
                Err(_) => (Stored::default(), false),
            },
            None => (Stored::default(), false),
        };
        let next_id = stored
            .events
            .back()
            .map_or(1, |entry| entry.id.saturating_add(1));
        let storage_healthy = Arc::new(AtomicBool::new(healthy));
        let sender = path.and_then(|path| {
            let (tx, rx) = mpsc::sync_channel::<Stored>(8);
            let status = Arc::clone(&storage_healthy);
            let worker = std::thread::Builder::new()
                .name("mcp-flight-recorder".into())
                .spawn(move || {
                    for snapshot in rx {
                        let ok =
                            recovery_store::save_json(&path, &snapshot, "MCP diagnostics").is_ok();
                        status.store(ok, Ordering::SeqCst);
                    }
                });
            if worker.is_err() {
                storage_healthy.store(false, Ordering::SeqCst);
                return None;
            }
            Some(tx)
        });
        Arc::new(Self {
            state: Mutex::new(State {
                stored,
                next_id,
                last_snapshot_ms: 0,
                dropped_snapshots: 0,
            }),
            sender,
            storage_healthy,
        })
    }

    fn record(
        &self,
        surface: Surface,
        route: Route,
        method: RequestMethod,
        stage: Stage,
        http_status: u16,
        elapsed_ms: u64,
    ) {
        let timestamp_ms = now_ms();
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let event = Event {
            id: state.next_id,
            timestamp_ms,
            surface,
            route,
            method,
            stage,
            outcome: outcome(http_status, stage),
            http_status,
            response_header_latency_ms: elapsed_ms,
        };
        state.next_id = state.next_id.saturating_add(1);
        if state.stored.events.len() == MAX_EVENTS {
            state.stored.events.pop_front();
        }
        state.stored.events.push_back(event);
        let needs_flush = http_status >= 400
            || stage == Stage::UpstreamConnect
            || timestamp_ms.saturating_sub(state.last_snapshot_ms) >= SAVE_EVERY_MS;
        if needs_flush {
            if let Some(tx) = &self.sender {
                match tx.try_send(state.stored.clone()) {
                    Ok(()) => state.last_snapshot_ms = timestamp_ms,
                    Err(_) => state.dropped_snapshots = state.dropped_snapshots.saturating_add(1),
                }
            }
        }
    }

    fn snapshot(&self, limit: usize) -> DiagnosticReport {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let events: Vec<_> = state
            .stored
            .events
            .iter()
            .rev()
            .take(limit.clamp(1, MAX_SHOWN))
            .cloned()
            .collect();
        let http_error_responses_in_window = state
            .stored
            .events
            .iter()
            .filter(|event| event.http_status >= 400)
            .count();
        let auth_challenges_in_window = state
            .stored
            .events
            .iter()
            .filter(|event| event.outcome == Outcome::AuthChallenge)
            .count();
        DiagnosticReport {
            retained_events: state.stored.events.len(),
            http_error_responses_in_window,
            auth_challenges_in_window,
            dropped_persistence_snapshots: state.dropped_snapshots,
            persistence_healthy: self.storage_healthy.load(Ordering::SeqCst),
            events,
            limitations: "Only traffic reaching the RepoTunnel gateway/Direct HTTPS proxy is visible. Upstream DNS/TLS/network failures before arrival, request body/tool failures inside HTTP 200 responses, SSE stream completion, and client round-trip latency are NOT detected. Authorization challenges can be normal OAuth handshakes. Routes/methods are coarse fixed categories; no request content, URLs, hostnames, IPs, credentials, or tool arguments are stored.",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiagnosticReport {
    retained_events: usize,
    http_error_responses_in_window: usize,
    auth_challenges_in_window: usize,
    dropped_persistence_snapshots: u64,
    persistence_healthy: bool,
    events: Vec<Event>,
    limitations: &'static str,
}

fn instance(app: &AppHandle) -> &'static Arc<Recorder> {
    RECORDER.get_or_init(|| {
        let path = app.path().resolve(FILE, BaseDirectory::AppData).ok();
        Recorder::new(path)
    })
}
pub(crate) fn initialize(app: &AppHandle) {
    let _ = instance(app);
}

pub(crate) fn record(
    app: &AppHandle,
    surface: Surface,
    route: Route,
    method: RequestMethod,
    stage: Stage,
    status: u16,
    elapsed_ms: u64,
) {
    instance(app).record(surface, route, method, stage, status, elapsed_ms);
}

pub(crate) fn diagnostics(app: &AppHandle, limit: usize) -> DiagnosticReport {
    instance(app).snapshot(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_path() -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "repotunnel-flight-test-{}-{seq}.json",
            std::process::id()
        ))
    }

    #[test]
    fn fixed_categories_never_copy_user_urls_or_secrets() {
        assert_eq!(route("/mcp/?access_token=SECRET"), Route::Mcp);
        assert_eq!(route("/token"), Route::OAuth);
        assert_eq!(route("/mcp-other?secret=ABC"), Route::Other);
        assert_eq!(method("STRANGE-SECRET-METHOD"), RequestMethod::Other);
        let recorder = Recorder::new(None);
        recorder.record(
            Surface::Gateway,
            route("/mcp/private?token=SECRET"),
            method("STRANGE-SECRET-METHOD"),
            Stage::ResponseHeaders,
            401,
            31,
        );
        let serialized = serde_json::to_string(&recorder.snapshot(20)).unwrap();
        for secret in ["SECRET", "private", "STRANGE-SECRET-METHOD", "access_token"] {
            assert!(
                !serialized.contains(secret),
                "diagnostics must never serialize user data"
            );
        }
        assert!(serialized.contains("authChallenge"));
    }

    #[test]
    fn bounded_ring_preserves_latest_and_reports_failures() {
        let recorder = Recorder::new(None);
        for i in 0..(MAX_EVENTS + 15) {
            let status = if i == MAX_EVENTS + 14 { 502 } else { 200 };
            recorder.record(
                Surface::Gateway,
                Route::Mcp,
                RequestMethod::Post,
                Stage::ResponseHeaders,
                status,
                i as u64,
            );
        }
        let report = recorder.snapshot(5000);
        assert_eq!(report.retained_events, MAX_EVENTS);
        assert_eq!(report.events.len(), MAX_SHOWN);
        assert_eq!(report.events[0].http_status, 502);
        assert_eq!(report.http_error_responses_in_window, 1);
        assert!(!report.persistence_healthy);
    }

    #[test]
    fn upstream_error_is_distinguished_from_regular_502() {
        assert_eq!(
            outcome(502, Stage::UpstreamConnect),
            Outcome::UpstreamFailure
        );
        assert_eq!(
            outcome(503, Stage::ResponseHeaders),
            Outcome::ServiceUnavailable
        );
        assert_eq!(outcome(403, Stage::ResponseHeaders), Outcome::AccessDenied);
    }

    #[test]
    fn bounded_store_roundtrips_through_private_recovery_format() {
        let path = temp_path();
        let recorder = Recorder::new(None);
        recorder.record(
            Surface::Gateway,
            Route::Mcp,
            RequestMethod::Get,
            Stage::ResponseHeaders,
            403,
            12,
        );
        let stored = recorder.state.lock().unwrap().stored.clone();
        recovery_store::save_json(&path, &stored, "MCP diagnostics").unwrap();
        let restored = load_store(&path).unwrap();
        assert_eq!(restored.events.len(), 1);
        assert_eq!(restored.events[0].outcome, Outcome::AccessDenied);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_file_name(format!(
            ".{}.previous",
            path.file_name().unwrap().to_string_lossy()
        )));
    }

    #[test]
    fn asynchronous_writer_persists_failure_and_recovers_after_restart() {
        let path = temp_path();
        let recorder = Recorder::new(Some(path.clone()));
        recorder.record(
            Surface::DirectHttps,
            Route::Mcp,
            RequestMethod::Post,
            Stage::UpstreamConnect,
            502,
            8,
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            if load_store(&path).is_ok_and(|value| {
                value
                    .events
                    .back()
                    .is_some_and(|event| event.outcome == Outcome::UpstreamFailure)
            }) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "error snapshot was not persisted"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(recorder);
        let restored = Recorder::new(Some(path.clone()));
        let report = restored.snapshot(10);
        assert_eq!(report.events.len(), 1);
        assert_eq!(report.events[0].http_status, 502);
        assert_eq!(report.events[0].stage, Stage::UpstreamConnect);
        drop(restored);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_file_name(format!(
            ".{}.previous",
            path.file_name().unwrap().to_string_lossy()
        )));
    }

    #[test]
    fn rejects_oversized_recovery_backup() {
        let path = temp_path();
        let previous = path.with_file_name(format!(
            ".{}.previous",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&previous, vec![b'x'; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert!(
            load_store(&path).is_err(),
            "recovery must not parse an oversized backup"
        );
        let _ = std::fs::remove_file(previous);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_persistent_store_without_following_it() {
        use std::os::unix::fs::symlink;
        let path = temp_path();
        let outside = path.with_extension("outside");
        std::fs::write(&outside, b"private").unwrap();
        symlink(&outside, &path).unwrap();
        assert!(load_store(&path).is_err());
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(outside);
    }
}
