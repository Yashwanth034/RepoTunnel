use std::{
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{path::BaseDirectory, AppHandle, Manager};

use crate::semantic::{
    self, SemanticBounds, SemanticFindQuery, SemanticNode, SemanticNodeDraft, SemanticRefTarget,
    SemanticSnapshot, SemanticSnapshotInput, SemanticSurface,
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

const ADB_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_MDNS_ENDPOINTS: usize = 8;
const PHONE_ACCESS_FILE: &str = "phone-access.json";
const PHONE_ACCESS_SCHEMA_VERSION: u32 = 1;
const PHONE_SEMANTIC_SCOPE: &str = "phone-access";
const PHONE_HELPER_PACKAGE: &str = "com.repotunnel.phonehelper";
const PHONE_HELPER_SERVICE: &str = "com.repotunnel.phonehelper/.PhoneAccessibilityService";
const PHONE_HELPER_CONFIG_ACTION: &str = "com.repotunnel.phonehelper.CONFIGURE";
const PHONE_HELPER_SOCKET: &str = "repotunnel_phone_semantic_v1";
const PHONE_HELPER_RESOURCE: &str = "phone/repotunnel-phone-helper.apk";
const PHONE_HELPER_SHA256: &str =
    "5af15dcf8c0dfceb1bdfd68b540457da0b95cd2416de964964236eb133f215ed";
const PHONE_HELPER_READ_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PHONE_HELPER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(8);
const SCRCPY_SERVER_VERSION: &str = "4.1";
const SCRCPY_SERVER_RESOURCE: &str = "phone/scrcpy-server-v4.1";
const SCRCPY_SERVER_SHA256: &str =
    "deacb991ed2509715160ffdc7907e47b4160eb30d1566217e9047fd5b8850cae";

static PHONE_ACCESS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneDeviceSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) manufacturer: Option<String>,
    pub(crate) android_version: Option<String>,
    pub(crate) state: String,
    pub(crate) transport: String,
    pub(crate) available_transports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneDiscoveryStatus {
    pub(crate) adb_available: bool,
    pub(crate) devices: Vec<PhoneDeviceSummary>,
    pub(crate) recommended_device_id: Option<String>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PhoneAccessMode {
    Off,
    Limited,
    Full,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PhoneCapability {
    ViewScreen,
    ControlInput,
    AppControl,
    Files,
    AppInstall,
    DeviceSettings,
    Shell,
    Logs,
    NetworkTools,
}

const ALL_PHONE_CAPABILITIES: [PhoneCapability; 9] = [
    PhoneCapability::ViewScreen,
    PhoneCapability::ControlInput,
    PhoneCapability::AppControl,
    PhoneCapability::Files,
    PhoneCapability::AppInstall,
    PhoneCapability::DeviceSettings,
    PhoneCapability::Shell,
    PhoneCapability::Logs,
    PhoneCapability::NetworkTools,
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PhoneAccessSettings {
    schema_version: u32,
    selected_device_id: Option<String>,
    mode: PhoneAccessMode,
    paused: bool,
    limited_capabilities: Vec<PhoneCapability>,
}

impl Default for PhoneAccessSettings {
    fn default() -> Self {
        Self {
            schema_version: PHONE_ACCESS_SCHEMA_VERSION,
            selected_device_id: None,
            mode: PhoneAccessMode::Off,
            paused: false,
            limited_capabilities: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneAccessStatus {
    pub(crate) selected_device_id: Option<String>,
    pub(crate) mode: PhoneAccessMode,
    pub(crate) paused: bool,
    pub(crate) limited_capabilities: Vec<PhoneCapability>,
    pub(crate) granted_capabilities: Vec<PhoneCapability>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneRuntimeStatus {
    pub(crate) active: bool,
    pub(crate) device_id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) transport: Option<String>,
    pub(crate) session_started_at: Option<u64>,
    pub(crate) last_used_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneRuntimeProbe {
    pub(crate) runtime: PhoneRuntimeStatus,
    pub(crate) display_size: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneScreenFrame {
    pub(crate) mime_type: String,
    pub(crate) data_base64: String,
    pub(crate) size_bytes: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) captured_at: u64,
    pub(crate) frame_id: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PhoneKey {
    Back,
    Home,
    Enter,
    Recents,
    Escape,
    Tab,
    Delete,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

impl PhoneKey {
    fn android_keycode(self) -> &'static str {
        match self {
            Self::Back => "KEYCODE_BACK",
            Self::Home => "KEYCODE_HOME",
            Self::Enter => "KEYCODE_ENTER",
            Self::Recents => "KEYCODE_APP_SWITCH",
            Self::Escape => "KEYCODE_ESCAPE",
            Self::Tab => "KEYCODE_TAB",
            Self::Delete => "KEYCODE_DEL",
            Self::DpadUp => "KEYCODE_DPAD_UP",
            Self::DpadDown => "KEYCODE_DPAD_DOWN",
            Self::DpadLeft => "KEYCODE_DPAD_LEFT",
            Self::DpadRight => "KEYCODE_DPAD_RIGHT",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhonePackageList {
    pub(crate) packages: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneShellResult {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneLogSnapshot {
    pub(crate) text: String,
    pub(crate) truncated: bool,
    pub(crate) max_lines: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneFileList {
    pub(crate) directory: String,
    pub(crate) entries: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneFileStat {
    pub(crate) path: String,
    pub(crate) kind: String,
    pub(crate) size_bytes: u64,
    pub(crate) modified_unix_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneFileRead {
    pub(crate) path: String,
    pub(crate) data_base64: String,
    pub(crate) size_bytes: u64,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneFileWriteReceipt {
    pub(crate) path: String,
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneFileDeleteReceipt {
    pub(crate) path: String,
    pub(crate) existed: bool,
    pub(crate) deleted: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneSettingRead {
    pub(crate) exists: bool,
    pub(crate) value: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneSettingsAvailability {
    pub(crate) read_available: bool,
    pub(crate) write_available: Option<bool>,
    pub(crate) delete_available: Option<bool>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhonePackageMutationReceipt {
    pub(crate) package_name: Option<String>,
    pub(crate) source_path: Option<String>,
    pub(crate) dispatched: bool,
    pub(crate) device_accepted: bool,
    pub(crate) verified: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneAppLaunchReceipt {
    pub(crate) package_name: String,
    pub(crate) payment_sensitive: bool,
    pub(crate) payment_safe_mode: bool,
    pub(crate) dispatched: bool,
    pub(crate) device_accepted: bool,
    pub(crate) verified_foreground: bool,
    pub(crate) ui_ready: bool,
    pub(crate) dispatch_ms: u64,
    pub(crate) foreground_wait_ms: u64,
    pub(crate) ui_ready_wait_ms: u64,
    pub(crate) total_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneAppStopReceipt {
    pub(crate) package_name: String,
    pub(crate) existed: bool,
    pub(crate) dispatched: bool,
    pub(crate) device_accepted: bool,
    pub(crate) verified_stopped: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneNetworkSnapshot {
    pub(crate) interfaces: String,
    pub(crate) routes: String,
    pub(crate) dns_properties: Vec<String>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhonePingResult {
    pub(crate) host: String,
    pub(crate) count: u32,
    pub(crate) success: bool,
    pub(crate) output: String,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhoneSettingsNamespace {
    System,
    Secure,
    Global,
}

impl PhoneSettingsNamespace {
    fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Secure => "secure",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PhoneSwipeGesture {
    pub(crate) start_x_ratio: f64,
    pub(crate) start_y_ratio: f64,
    pub(crate) end_x_ratio: f64,
    pub(crate) end_y_ratio: f64,
    pub(crate) duration_ms: u32,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PhoneActionGuard {
    pub(crate) expected_frame_id: Option<u64>,
    pub(crate) expected_display_generation: Option<u64>,
    pub(crate) expected_package: Option<String>,
    pub(crate) expected_activity: Option<String>,
    pub(crate) expected_orientation: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum PhoneControlSequenceStep {
    Tap { x_ratio: f64, y_ratio: f64 },
    Swipe(PhoneSwipeGesture),
    Wait { duration_ms: u32 },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneSequenceReceipt {
    pub(crate) completed_steps: usize,
    pub(crate) total_steps: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum PhoneFastWaitCondition {
    ForegroundPackage {
        package_name: String,
        timeout_ms: u32,
    },
    FrameChanged {
        baseline_frame_id: u64,
        timeout_ms: u32,
    },
    FrameStable {
        stable_count: u32,
        interval_ms: u32,
        timeout_ms: u32,
    },
    KeyboardVisible {
        visible: bool,
        timeout_ms: u32,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum PhoneFastSequenceStep {
    Tap { x_ratio: f64, y_ratio: f64 },
    Swipe(PhoneSwipeGesture),
    Key(PhoneKey),
    TypeText(String),
    LaunchApp(String),
    Wait { duration_ms: u32 },
    WaitUntil(PhoneFastWaitCondition),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneFastSequenceReceipt {
    pub(crate) completed_steps: usize,
    pub(crate) total_steps: usize,
    pub(crate) elapsed_ms: u64,
    pub(crate) start_frame_captured_at: Option<u64>,
    pub(crate) final_frame_captured_at: Option<u64>,
    pub(crate) frame_changed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PhoneFastSequenceResult {
    pub(crate) receipt: PhoneFastSequenceReceipt,
    pub(crate) frame: Option<PhoneScreenFrame>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneSemanticSnapshotResult {
    pub(crate) snapshot: SemanticSnapshot,
    pub(crate) package_name: Option<String>,
    pub(crate) activity_name: Option<String>,
    pub(crate) orientation: String,
    pub(crate) display_generation: u64,
    pub(crate) frame_id: Option<u64>,
    pub(crate) physical_display: Option<SemanticBounds>,
    pub(crate) stream_frame: Option<SemanticBounds>,
    pub(crate) keyboard_visible: Option<bool>,
    pub(crate) source: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneObservationMetadata {
    pub(crate) display_generation: u64,
    pub(crate) frame_id: Option<u64>,
    pub(crate) orientation: Option<String>,
    pub(crate) physical_width: Option<u32>,
    pub(crate) physical_height: Option<u32>,
    pub(crate) stream_width: Option<u32>,
    pub(crate) stream_height: Option<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneSemanticActionReceipt {
    pub(crate) action: String,
    pub(crate) ref_id: String,
    pub(crate) dispatched: bool,
    pub(crate) device_accepted: bool,
    pub(crate) verified: bool,
    pub(crate) final_ui_generation: u64,
    pub(crate) dispatch_ms: u64,
    pub(crate) verification_ms: u64,
    pub(crate) total_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PhoneSemanticHelperStatus {
    pub(crate) bundled: bool,
    pub(crate) installed: bool,
    pub(crate) integrity_verified: bool,
    pub(crate) accessibility_enabled: bool,
    pub(crate) ready: bool,
    pub(crate) needs_user_enablement: bool,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompiledPhoneControlStep {
    Tap {
        x: u32,
        y: u32,
    },
    Swipe {
        start_x: u32,
        start_y: u32,
        end_x: u32,
        end_y: u32,
        duration_ms: u32,
    },
    Wait {
        duration_ms: u32,
    },
}

#[derive(Debug, Clone)]
struct PhoneRuntimeTarget {
    device_id: String,
    name: String,
    serial: String,
    transport: String,
    adb: PathBuf,
}

#[derive(Debug, Clone)]
struct ActivePhoneRuntime {
    target: PhoneRuntimeTarget,
    session_started_at: u64,
    last_used_at: u64,
    physical_display_dimensions: Option<(u32, u32)>,
    stream_dimensions: Option<(u32, u32)>,
    display_generation: u64,
    settings_write_available: Option<bool>,
    settings_write_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedPhoneLiveFrame {
    bytes: Vec<u8>,
    width: u32,
    height: u32,
    captured_at: u64,
    frame_id: u64,
}

#[derive(Debug)]
struct PhoneLiveStreamSession {
    device_id: String,
    serial: String,
    started_at: u64,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<CachedPhoneLiveFrame>>>,
    control: Arc<Mutex<Option<TcpStream>>>,
    last_error: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
struct PhoneControlShellSession {
    device_id: String,
    serial: String,
    child: Child,
    stdin: ChildStdin,
    output: Receiver<String>,
    sequence: u64,
}

#[derive(Debug, Clone)]
struct PhoneSemanticHelperSession {
    device_id: String,
    serial: String,
    adb: PathBuf,
    nonce: String,
    local_port: u16,
}

#[derive(Debug, Default)]
pub(crate) struct PhoneRuntimeState {
    inner: Mutex<Option<ActivePhoneRuntime>>,
    live_stream: Mutex<Option<PhoneLiveStreamSession>>,
    control_shell: Mutex<Option<PhoneControlShellSession>>,
    semantic_helper: Mutex<Option<PhoneSemanticHelperSession>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdbDeviceRecord {
    serial: String,
    state: String,
    transport: String,
    model_hint: Option<String>,
}

#[derive(Debug, Default)]
struct DeviceProperties {
    manufacturer: Option<String>,
    model: Option<String>,
    android_version: Option<String>,
    serial: Option<String>,
    boot_serial: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MdnsService {
    name: String,
    service_type: String,
    endpoint: String,
}

fn access_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(PHONE_ACCESS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve phone access settings: {error}"))
}

fn normalize_limited_capabilities(capabilities: &[PhoneCapability]) -> Vec<PhoneCapability> {
    ALL_PHONE_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| capabilities.contains(capability))
        .collect()
}

fn parse_access_settings(contents: &str) -> Result<PhoneAccessSettings, String> {
    if contents.trim().is_empty() {
        return Err("Saved phone access settings are empty.".to_string());
    }
    let mut settings: PhoneAccessSettings = serde_json::from_str(contents)
        .map_err(|error| format!("Saved phone access settings are invalid: {error}"))?;
    if settings.schema_version != PHONE_ACCESS_SCHEMA_VERSION {
        return Err("Saved phone access settings use an unsupported format.".to_string());
    }
    if settings.selected_device_id.as_deref().is_some_and(|id| {
        id.len() != 30
            || !id.starts_with("phone-")
            || !id[6..].bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err("Saved phone access device identity is invalid.".to_string());
    }
    settings.limited_capabilities = normalize_limited_capabilities(&settings.limited_capabilities);
    if settings.mode != PhoneAccessMode::Limited {
        settings.limited_capabilities.clear();
    }
    if settings.mode == PhoneAccessMode::Off {
        settings.paused = false;
    }
    Ok(settings)
}

fn load_access_settings_unlocked(app: &AppHandle) -> Result<PhoneAccessSettings, String> {
    let path = access_path(app)?;
    if !path.exists() {
        return Ok(PhoneAccessSettings::default());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("Could not read phone access settings: {error}"))?;
    parse_access_settings(&contents)
}

fn load_access_settings(app: &AppHandle) -> Result<PhoneAccessSettings, String> {
    let _guard = PHONE_ACCESS_LOCK
        .lock()
        .map_err(|_| "Phone access settings lock is unavailable.".to_string())?;
    load_access_settings_unlocked(app)
}

fn random_access_sibling(path: &Path, label: &str) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve phone access settings directory.".to_string())?;
    for _ in 0..16 {
        let mut random = [0u8; 12];
        getrandom::fill(&mut random)
            .map_err(|error| format!("Could not prepare phone access settings: {error}"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let candidate = parent.join(format!(".phone-access-{label}-{suffix}.json"));
        match fs::symlink_metadata(&candidate) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => {
                return Err(format!(
                    "Could not inspect phone access settings staging path: {error}"
                ))
            }
        }
    }
    Err("Could not allocate a phone access settings staging file.".to_string())
}

fn save_access_settings_unlocked(
    app: &AppHandle,
    settings: &PhoneAccessSettings,
) -> Result<(), String> {
    let path = access_path(app)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve phone access settings directory.".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create phone access settings directory: {error}"))?;

    let staged = random_access_sibling(&path, "stage")?;
    let contents = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("Could not serialize phone access settings: {error}"))?;
    let result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&staged).map_err(|error| {
            format!("Could not create phone access settings staging file: {error}")
        })?;
        file.write_all(&contents)
            .map_err(|error| format!("Could not write phone access settings: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not flush phone access settings: {error}"))?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }

    #[cfg(not(windows))]
    {
        if let Err(error) = fs::rename(&staged, &path) {
            let _ = fs::remove_file(&staged);
            return Err(format!("Could not save phone access settings: {error}"));
        }
        #[cfg(unix)]
        if let Ok(directory) = fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }

    #[cfg(windows)]
    {
        if path.exists() {
            let backup = random_access_sibling(&path, "previous")?;
            fs::rename(&path, &backup).map_err(|error| {
                format!("Could not protect previous phone access settings: {error}")
            })?;
            if let Err(error) = fs::rename(&staged, &path) {
                let restore = fs::rename(&backup, &path);
                let _ = fs::remove_file(&staged);
                return Err(match restore {
                    Ok(()) => format!(
                        "Could not save phone access settings; previous settings were restored: {error}"
                    ),
                    Err(restore_error) => format!(
                        "Could not save phone access settings: {error}. Previous settings remain in a recovery file but could not be restored automatically: {restore_error}"
                    ),
                });
            }
            let _ = fs::remove_file(&backup);
        } else if let Err(error) = fs::rename(&staged, &path) {
            let _ = fs::remove_file(&staged);
            return Err(format!("Could not save phone access settings: {error}"));
        }
    }

    Ok(())
}

fn mutate_access_settings<T>(
    app: &AppHandle,
    mutate: impl FnOnce(&mut PhoneAccessSettings) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = PHONE_ACCESS_LOCK
        .lock()
        .map_err(|_| "Phone access settings lock is unavailable.".to_string())?;
    let mut settings = load_access_settings_unlocked(app)?;
    let result = mutate(&mut settings)?;
    save_access_settings_unlocked(app, &settings)?;
    Ok(result)
}

fn capability_allowed(settings: &PhoneAccessSettings, capability: PhoneCapability) -> bool {
    if settings.paused || settings.selected_device_id.is_none() {
        return false;
    }
    match settings.mode {
        PhoneAccessMode::Off => false,
        PhoneAccessMode::Full => true,
        PhoneAccessMode::Limited => settings.limited_capabilities.contains(&capability),
    }
}

fn access_status_from(settings: &PhoneAccessSettings) -> PhoneAccessStatus {
    let granted_capabilities = ALL_PHONE_CAPABILITIES
        .iter()
        .copied()
        .filter(|capability| capability_allowed(settings, *capability))
        .collect();
    PhoneAccessStatus {
        selected_device_id: settings.selected_device_id.clone(),
        mode: settings.mode,
        paused: settings.paused,
        limited_capabilities: settings.limited_capabilities.clone(),
        granted_capabilities,
    }
}

pub(crate) fn access_status(app: &AppHandle) -> Result<PhoneAccessStatus, String> {
    load_access_settings(app).map(|settings| access_status_from(&settings))
}

fn enforce_capability(
    settings: &PhoneAccessSettings,
    device_id: &str,
    capability: PhoneCapability,
) -> Result<(), String> {
    if settings.selected_device_id.as_deref() != Some(device_id) {
        return Err("AI access is not enabled for this phone.".to_string());
    }
    if settings.paused {
        return Err("AI phone access is paused.".to_string());
    }
    match settings.mode {
        PhoneAccessMode::Off => Err("AI phone access is off.".to_string()),
        PhoneAccessMode::Full => Ok(()),
        PhoneAccessMode::Limited if settings.limited_capabilities.contains(&capability) => Ok(()),
        PhoneAccessMode::Limited => {
            Err("That phone capability is not enabled in Limited Access.".to_string())
        }
    }
}

pub(crate) fn require_capability(
    app: &AppHandle,
    device_id: &str,
    capability: PhoneCapability,
) -> Result<(), String> {
    let settings = load_access_settings(app)?;
    enforce_capability(&settings, device_id, capability)
}

fn run_bounded(program: &Path, args: &[&str], timeout: Duration) -> Result<Output, String> {
    run_bounded_with_input(program, args, None, timeout)
}

fn run_bounded_with_input(
    program: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start Android device tool: {error}"))?;

    let stdin_writer = if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Could not open Android device tool input.".to_string())?;
        let input = input.to_vec();
        Some(thread::spawn(move || stdin.write_all(&input)))
    } else {
        None
    };

    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not capture Android device tool output.".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Could not capture Android device tool errors.".to_string())?;

    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });

    let started = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|error| format!("Could not inspect Android device tool state: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(writer) = stdin_writer {
                    let _ = writer.join();
                }
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err("Android device operation timed out.".to_string());
            }
            None => thread::sleep(Duration::from_millis(20)),
        }
    };

    if let Some(writer) = stdin_writer {
        writer
            .join()
            .map_err(|_| "Could not write Android device tool input.".to_string())?
            .map_err(|error| format!("Could not write Android device tool input: {error}"))?;
    }

    let stdout = stdout_reader
        .join()
        .map_err(|_| "Could not read Android device tool output.".to_string())?
        .map_err(|error| format!("Could not read Android device tool output: {error}"))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "Could not read Android device tool errors.".to_string())?
        .map_err(|error| format!("Could not read Android device tool errors: {error}"))?;

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn quote_remote_shell_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\''"))
}

fn remote_shell_command(script: &str) -> String {
    format!("sh -c {}", quote_remote_shell_argument(script))
}

fn run_bounded_adb_script(
    adb: &Path,
    serial: &str,
    protocol: &str,
    script: &str,
    timeout: Duration,
) -> Result<Output, String> {
    let remote_command = remote_shell_command(script);
    run_bounded(adb, &["-s", serial, protocol, &remote_command], timeout)
}

fn stage_phone_transfer_file(app: &AppHandle, data: &[u8]) -> Result<PathBuf, String> {
    const MAX_TRANSFER_BYTES: usize = 8 * 1024 * 1024;
    if data.len() > MAX_TRANSFER_BYTES {
        return Err("Phone file write is limited to 8 MiB per request.".to_string());
    }

    let root = app
        .path()
        .app_cache_dir()
        .map_err(|error| format!("Could not resolve RepoTunnel cache directory: {error}"))?
        .join("phone-transfer");
    fs::create_dir_all(&root)
        .map_err(|error| format!("Could not prepare private phone transfer cache: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect private phone transfer cache: {error}"))?;
    }

    for _ in 0..16 {
        let mut random = [0u8; 12];
        getrandom::fill(&mut random)
            .map_err(|error| format!("Could not prepare phone transfer file: {error}"))?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = root.join(format!("upload-{suffix}.bin"));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);

        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(data).and_then(|_| file.sync_all()) {
                    let _ = fs::remove_file(&path);
                    return Err(format!("Could not stage phone file transfer: {error}"));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not create phone transfer staging file: {error}"
                ))
            }
        }
    }

    Err("Could not allocate a private phone transfer staging file.".to_string())
}

fn adb_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for variable in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = env::var_os(variable) {
            candidates.push(
                PathBuf::from(root)
                    .join("platform-tools")
                    .join(if cfg!(windows) { "adb.exe" } else { "adb" }),
            );
        }
    }

    if let Some(home) = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }) {
        let home = PathBuf::from(home);
        if cfg!(target_os = "macos") {
            candidates.push(home.join("Library/Android/sdk/platform-tools/adb"));
        } else if cfg!(windows) {
            candidates.push(home.join("AppData/Local/Android/Sdk/platform-tools/adb.exe"));
        } else {
            candidates.push(home.join("Android/Sdk/platform-tools/adb"));
        }
    }

    if cfg!(windows) {
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            candidates
                .push(PathBuf::from(local_app_data).join("Android/Sdk/platform-tools/adb.exe"));
        }
    }

    candidates.push(PathBuf::from(if cfg!(windows) { "adb.exe" } else { "adb" }));

    let mut unique = Vec::new();
    for candidate in candidates {
        if !unique.contains(&candidate) {
            unique.push(candidate);
        }
    }
    unique
}

fn resolve_adb() -> Option<PathBuf> {
    adb_candidates().into_iter().find(|candidate| {
        run_bounded(candidate, &["version"], ADB_PROBE_TIMEOUT)
            .map(|output| output.status.success())
            .unwrap_or(false)
    })
}

fn normalize_hint(value: &str) -> String {
    value.replace('_', " ").trim().to_string()
}

fn parse_adb_records(output: &str) -> Vec<AdbDeviceRecord> {
    output
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("List of devices attached"))
        .skip(1)
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('*') {
                return None;
            }

            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 2 {
                return None;
            }

            let serial = fields[0].to_string();
            let raw_state = fields[1];
            let state = match raw_state {
                "device" => "connected",
                "unauthorized" => "authorizationRequired",
                "offline" => "offline",
                _ => "unavailable",
            }
            .to_string();

            let transport = if fields.iter().skip(2).any(|field| field.starts_with("usb:")) {
                "usb"
            } else if serial.contains(':')
                || serial.starts_with("adb-")
                || serial.contains("_adb-tls-connect")
            {
                "wireless"
            } else if serial.starts_with("emulator-") {
                "unknown"
            } else {
                // Physical USB devices can be reported by some ADB versions without
                // the optional usb:<bus> metadata. A plain hardware serial is still
                // a USB transport; wireless ADB identities are handled above.
                "usb"
            }
            .to_string();

            let model_hint = fields
                .iter()
                .skip(2)
                .find_map(|field| field.strip_prefix("model:"))
                .map(normalize_hint)
                .filter(|value| !value.is_empty());

            Some(AdbDeviceRecord {
                serial,
                state,
                transport,
                model_hint,
            })
        })
        .collect()
}

fn parse_mdns_services(output: &str) -> Vec<MdnsService> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            if fields.len() < 3 {
                return None;
            }

            let name = fields.first()?.trim().trim_end_matches('.').to_string();
            let service_type = fields
                .iter()
                .find(|field| {
                    field.contains("_adb-tls-pairing._tcp")
                        || field.contains("_adb-tls-connect._tcp")
                })?
                .trim_end_matches('.')
                .to_string();
            let endpoint = fields.last()?.trim().to_string();

            if endpoint.parse::<SocketAddr>().is_err() {
                return None;
            }

            Some(MdnsService {
                name,
                service_type,
                endpoint,
            })
        })
        .collect()
}

fn clean_property_value(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty() && !value.eq_ignore_ascii_case("unknown")).then(|| value.to_string())
}

fn parse_getprop_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    let line = line.strip_prefix('[')?;
    let (key, value) = line.split_once("]: [")?;
    let value = value.strip_suffix(']')?;
    Some((key, value))
}

fn parse_properties(output: &str) -> DeviceProperties {
    let mut properties = DeviceProperties::default();

    for line in output.lines() {
        let Some((key, value)) = parse_getprop_line(line) else {
            continue;
        };
        let value = clean_property_value(value);
        match key {
            "ro.product.manufacturer" => properties.manufacturer = value,
            "ro.product.model" => properties.model = value,
            "ro.build.version.release" => properties.android_version = value,
            "ro.serialno" => properties.serial = value,
            "ro.boot.serialno" => properties.boot_serial = value,
            _ => {}
        }
    }

    properties
}

fn query_properties(adb: &Path, serial: &str) -> DeviceProperties {
    let Ok(output) = run_bounded(adb, &["-s", serial, "shell", "getprop"], ADB_PROBE_TIMEOUT)
    else {
        return DeviceProperties::default();
    };
    if !output.status.success() {
        return DeviceProperties::default();
    }

    parse_properties(&String::from_utf8_lossy(&output.stdout))
}

fn opaque_device_id(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let suffix = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("phone-{suffix}")
}

fn display_name(properties: &DeviceProperties, hint: Option<&str>) -> String {
    let model = properties
        .model
        .as_deref()
        .or(hint)
        .map(normalize_hint)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Android phone".to_string());

    let Some(manufacturer) = properties
        .manufacturer
        .as_deref()
        .map(normalize_hint)
        .filter(|value| !value.is_empty())
    else {
        return model;
    };

    if model
        .to_ascii_lowercase()
        .starts_with(&manufacturer.to_ascii_lowercase())
    {
        model
    } else {
        format!("{manufacturer} {model}")
    }
}

fn transport_rank(transport: &str) -> u8 {
    match transport {
        "usb" => 0,
        "wireless" => 1,
        _ => 2,
    }
}

fn state_rank(state: &str) -> u8 {
    match state {
        "connected" => 0,
        "authorizationRequired" => 1,
        "offline" => 2,
        _ => 3,
    }
}

fn mdns_embedded_serial(serial: &str) -> Option<String> {
    let core = serial.strip_prefix("adb-")?;
    let core = core.strip_suffix("._adb-tls-connect._tcp").unwrap_or(core);
    let (physical_serial, _) = core.rsplit_once('-')?;
    (!physical_serial.is_empty()).then(|| physical_serial.to_string())
}

fn mdns_identity_hint(adb: &Path, adb_target: &str) -> Option<String> {
    if let Some(serial) = mdns_embedded_serial(adb_target) {
        return Some(serial);
    }

    mdns_services(adb)
        .ok()?
        .into_iter()
        .find(|service| {
            service.service_type.contains("_adb-tls-connect._tcp") && service.endpoint == adb_target
        })
        .and_then(|service| mdns_embedded_serial(&service.name))
}

fn stable_identity(
    record: &AdbDeviceRecord,
    properties: &DeviceProperties,
    mdns_hint: Option<String>,
) -> String {
    properties
        .serial
        .clone()
        .or_else(|| properties.boot_serial.clone())
        .or(mdns_hint)
        .or_else(|| mdns_embedded_serial(&record.serial))
        .unwrap_or_else(|| record.serial.clone())
}

fn insert_or_merge(devices: &mut Vec<PhoneDeviceSummary>, device: PhoneDeviceSummary) {
    if let Some(existing) = devices.iter_mut().find(|item| item.id == device.id) {
        for transport in device.available_transports {
            if !existing.available_transports.contains(&transport) {
                existing.available_transports.push(transport);
            }
        }
        existing
            .available_transports
            .sort_by_key(|transport| transport_rank(transport));

        let existing_score = (
            state_rank(&existing.state),
            transport_rank(&existing.transport),
        );
        let incoming_score = (state_rank(&device.state), transport_rank(&device.transport));
        if incoming_score < existing_score {
            existing.state = device.state.clone();
            existing.transport = device.transport.clone();
        }
        if existing.name == "Android phone" && device.name != "Android phone" {
            existing.name = device.name;
        }
        if existing.manufacturer.is_none() {
            existing.manufacturer = device.manufacturer;
        }
        if existing.android_version.is_none() {
            existing.android_version = device.android_version;
        }
        return;
    }

    devices.push(device);
}

fn discover_with_adb(adb: &Path) -> PhoneDiscoveryStatus {
    let output = match run_bounded(adb, &["devices", "-l"], ADB_PROBE_TIMEOUT) {
        Ok(output) if output.status.success() => output,
        Ok(_) => {
            return PhoneDiscoveryStatus {
                adb_available: true,
                devices: Vec::new(),
                recommended_device_id: None,
                message: Some("Android device discovery is temporarily unavailable.".to_string()),
            }
        }
        Err(error) => {
            return PhoneDiscoveryStatus {
                adb_available: true,
                devices: Vec::new(),
                recommended_device_id: None,
                message: Some(error),
            }
        }
    };

    let records = parse_adb_records(&String::from_utf8_lossy(&output.stdout));
    let mut devices = Vec::new();

    for record in records {
        let properties = if record.state == "connected" {
            query_properties(adb, &record.serial)
        } else {
            DeviceProperties::default()
        };

        let mdns_hint = if properties.serial.is_none()
            && properties.boot_serial.is_none()
            && record.transport == "wireless"
        {
            mdns_identity_hint(adb, &record.serial)
        } else {
            None
        };
        let stable_identity = stable_identity(&record, &properties, mdns_hint);
        let id = opaque_device_id(&stable_identity);
        let name = display_name(&properties, record.model_hint.as_deref());

        insert_or_merge(
            &mut devices,
            PhoneDeviceSummary {
                id,
                name,
                manufacturer: properties.manufacturer,
                android_version: properties.android_version,
                state: record.state,
                transport: record.transport.clone(),
                available_transports: vec![record.transport],
            },
        );
    }

    devices.sort_by(|left, right| {
        let left_connected = left.state == "connected";
        let right_connected = right.state == "connected";
        right_connected.cmp(&left_connected).then_with(|| {
            left.name
                .to_ascii_lowercase()
                .cmp(&right.name.to_ascii_lowercase())
        })
    });

    let connected_ids = devices
        .iter()
        .filter(|device| device.state == "connected")
        .map(|device| device.id.as_str())
        .collect::<Vec<_>>();
    let recommended_device_id = if connected_ids.len() == 1 {
        Some(connected_ids[0].to_string())
    } else {
        None
    };

    let message = if devices.is_empty() {
        Some("No Android phone is currently connected.".to_string())
    } else if recommended_device_id.is_none() && connected_ids.len() > 1 {
        Some("Multiple Android phones are connected. Choose the phone you want to use.".to_string())
    } else {
        None
    };

    PhoneDiscoveryStatus {
        adb_available: true,
        devices,
        recommended_device_id,
        message,
    }
}

fn mdns_services(adb: &Path) -> Result<Vec<MdnsService>, String> {
    let output = run_bounded(adb, &["mdns", "services"], ADB_PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    Ok(parse_mdns_services(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn reconnect_discovered_wireless(adb: &Path) {
    let Ok(services) = mdns_services(adb) else {
        return;
    };

    let endpoints = services
        .into_iter()
        .filter(|service| service.service_type.contains("_adb-tls-connect._tcp"))
        .map(|service| service.endpoint)
        .take(MAX_MDNS_ENDPOINTS)
        .collect::<Vec<_>>();

    for endpoint in endpoints {
        let _ = run_bounded(adb, &["connect", &endpoint], ADB_PROBE_TIMEOUT);
    }
}

fn valid_pairing_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit())
}

fn has_connected_phone(status: &PhoneDiscoveryStatus) -> bool {
    status
        .devices
        .iter()
        .any(|device| device.state == "connected")
}

fn has_connected_wireless_phone(status: &PhoneDiscoveryStatus) -> bool {
    status.devices.iter().any(|device| {
        device.state == "connected"
            && device
                .available_transports
                .iter()
                .any(|transport| transport == "wireless")
    })
}

fn should_attempt_wireless_reconnect(status: &PhoneDiscoveryStatus) -> bool {
    !has_connected_phone(status)
}

pub(crate) fn pair_wirelessly(code: &str) -> Result<PhoneDiscoveryStatus, String> {
    let code = code.trim();
    if !valid_pairing_code(code) {
        return Err("Enter the 6-digit pairing code shown on your phone.".to_string());
    }

    let adb = resolve_adb()
        .ok_or_else(|| "Android device tools are not available on this computer.".to_string())?;
    let pairing_endpoints = mdns_services(&adb)?
        .into_iter()
        .filter(|service| service.service_type.contains("_adb-tls-pairing._tcp"))
        .map(|service| service.endpoint)
        .take(3)
        .collect::<Vec<_>>();

    let endpoint = match pairing_endpoints.as_slice() {
        [endpoint] => endpoint,
        [] => {
            return Err(
                "On your phone, open Wireless debugging → Pair device with pairing code, then try again."
                    .to_string(),
            )
        }
        _ => {
            return Err(
                "More than one phone is ready to pair. Keep the pairing-code screen open only on the phone you want to connect."
                    .to_string(),
            )
        }
    };

    let output = run_bounded(&adb, &["pair", endpoint, code], Duration::from_secs(8))?;
    if !output.status.success() {
        return Err("Wireless pairing failed. Check the pairing code and try again.".to_string());
    }

    let started = Instant::now();
    let verify_timeout = Duration::from_secs(15);
    loop {
        reconnect_discovered_wireless(&adb);
        let status = discover_with_adb(&adb);
        if has_connected_wireless_phone(&status) {
            return Ok(status);
        }
        if started.elapsed() >= verify_timeout {
            return Err(
                "Android accepted the pairing, but RepoTunnel could not verify a wireless connection within 15 seconds. Keep Wireless debugging on and try again with the fresh code Android shows."
                    .to_string(),
            );
        }
        thread::sleep(Duration::from_millis(350));
    }
}

pub(crate) fn discover() -> PhoneDiscoveryStatus {
    let Some(adb) = resolve_adb() else {
        return PhoneDiscoveryStatus {
            adb_available: false,
            devices: Vec::new(),
            recommended_device_id: None,
            message: Some("Android device tools are not available on this computer.".to_string()),
        };
    };

    let initial = discover_with_adb(&adb);
    if !should_attempt_wireless_reconnect(&initial) {
        return initial;
    }

    reconnect_discovered_wireless(&adb);
    discover_with_adb(&adb)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

fn runtime_status_from(runtime: Option<&ActivePhoneRuntime>) -> PhoneRuntimeStatus {
    match runtime {
        Some(runtime) => PhoneRuntimeStatus {
            active: true,
            device_id: Some(runtime.target.device_id.clone()),
            name: Some(runtime.target.name.clone()),
            transport: Some(runtime.target.transport.clone()),
            session_started_at: Some(runtime.session_started_at),
            last_used_at: Some(runtime.last_used_at),
        },
        None => PhoneRuntimeStatus {
            active: false,
            device_id: None,
            name: None,
            transport: None,
            session_started_at: None,
            last_used_at: None,
        },
    }
}

fn runtime_target_alive(target: &PhoneRuntimeTarget) -> bool {
    run_bounded(
        &target.adb,
        &["-s", &target.serial, "get-state"],
        Duration::from_secs(2),
    )
    .map(|output| {
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .trim()
                .eq_ignore_ascii_case("device")
    })
    .unwrap_or(false)
}

fn choose_runtime_target(mut targets: Vec<PhoneRuntimeTarget>) -> Option<PhoneRuntimeTarget> {
    targets.sort_by_key(|target| transport_rank(&target.transport));
    targets.into_iter().next()
}

fn collect_runtime_targets(adb: &Path, device_id: &str) -> Result<Vec<PhoneRuntimeTarget>, String> {
    let output = run_bounded(adb, &["devices", "-l"], ADB_PROBE_TIMEOUT)?;
    if !output.status.success() {
        return Err("Could not inspect the selected Android phone.".to_string());
    }

    let mut targets = Vec::new();
    for record in parse_adb_records(&String::from_utf8_lossy(&output.stdout)) {
        if record.state != "connected" {
            continue;
        }
        let properties = query_properties(adb, &record.serial);
        let mdns_hint = if properties.serial.is_none()
            && properties.boot_serial.is_none()
            && record.transport == "wireless"
        {
            mdns_identity_hint(adb, &record.serial)
        } else {
            None
        };
        let stable_identity = stable_identity(&record, &properties, mdns_hint);
        if opaque_device_id(&stable_identity) != device_id {
            continue;
        }

        targets.push(PhoneRuntimeTarget {
            device_id: device_id.to_string(),
            name: display_name(&properties, record.model_hint.as_deref()),
            serial: record.serial,
            transport: record.transport,
            adb: adb.to_path_buf(),
        });
    }

    targets.sort_by_key(|target| transport_rank(&target.transport));
    Ok(targets)
}

fn resolve_runtime_targets(device_id: &str) -> Result<Vec<PhoneRuntimeTarget>, String> {
    let adb = resolve_adb()
        .ok_or_else(|| "Android device tools are not available on this computer.".to_string())?;

    let targets = collect_runtime_targets(&adb, device_id)?;
    if !targets.is_empty() {
        return Ok(targets);
    }

    reconnect_discovered_wireless(&adb);
    collect_runtime_targets(&adb, device_id)
}

#[allow(dead_code)]
fn resolve_runtime_target(device_id: &str) -> Result<PhoneRuntimeTarget, String> {
    choose_runtime_target(resolve_runtime_targets(device_id)?)
        .ok_or_else(|| "The selected phone is not currently connected.".to_string())
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn raw_screencap_rgba(bytes: &[u8]) -> Option<(u32, u32, &[u8])> {
    if bytes.len() < 12 {
        return None;
    }

    let width = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    let pixel_format = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if width == 0 || height == 0 || pixel_format != 1 {
        return None;
    }

    let pixel_bytes = usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)?;

    for header_len in [16usize, 12usize] {
        if bytes.len() == header_len.checked_add(pixel_bytes)? {
            return Some((width, height, &bytes[header_len..]));
        }
    }

    None
}

fn encode_rgba_jpeg(
    ffmpeg: &Path,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, String> {
    let size = format!("{width}x{height}");
    let output = run_bounded_with_input(
        ffmpeg,
        &[
            "-v",
            "error",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &size,
            "-i",
            "pipe:0",
            "-frames:v",
            "1",
            "-q:v",
            "2",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ],
        Some(rgba),
        Duration::from_secs(4),
    )?;

    if !output.status.success()
        || output.stdout.len() < 4
        || !output.stdout.starts_with(&[0xff, 0xd8, 0xff])
        || !output.stdout.ends_with(&[0xff, 0xd9])
    {
        return Err("Could not encode the fast phone screen frame.".to_string());
    }

    Ok(output.stdout)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }

    let mut offset = 2usize;
    while offset + 4 <= bytes.len() {
        while offset < bytes.len() && bytes[offset] != 0xff {
            offset += 1;
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        if offset >= bytes.len() {
            break;
        }

        let marker = bytes[offset];
        offset += 1;
        if matches!(marker, 0xd8 | 0xd9) || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if offset + 2 > bytes.len() {
            break;
        }

        let segment_len = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]) as usize;
        if segment_len < 2 || offset + segment_len > bytes.len() {
            break;
        }

        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) && segment_len >= 7
        {
            let height = u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]) as u32;
            let width = u16::from_be_bytes([bytes[offset + 5], bytes[offset + 6]]) as u32;
            return (width > 0 && height > 0).then_some((width, height));
        }

        offset += segment_len;
    }

    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn consume_mjpeg_frames(
    buffer: &mut Vec<u8>,
    latest: &Arc<Mutex<Option<CachedPhoneLiveFrame>>>,
    next_frame_id: &mut u64,
) {
    const MAX_MJPEG_BUFFER_BYTES: usize = 16 * 1024 * 1024;
    const MAX_MJPEG_FRAME_BYTES: usize = 8 * 1024 * 1024;

    loop {
        let Some(start) = find_bytes(buffer, &[0xff, 0xd8]) else {
            if buffer.len() > 1 {
                let last = *buffer.last().unwrap_or(&0);
                buffer.clear();
                if last == 0xff {
                    buffer.push(last);
                }
            }
            break;
        };

        if start > 0 {
            buffer.drain(..start);
        }

        let Some(relative_end) = find_bytes(&buffer[2..], &[0xff, 0xd9]) else {
            if buffer.len() > MAX_MJPEG_BUFFER_BYTES {
                buffer.clear();
            }
            break;
        };
        let end = relative_end + 4;
        if end > MAX_MJPEG_FRAME_BYTES {
            buffer.drain(..end);
            continue;
        }

        let frame = buffer[..end].to_vec();
        buffer.drain(..end);
        if let Some((width, height)) = jpeg_dimensions(&frame) {
            if width <= 10_000 && height <= 10_000 {
                if let Ok(mut cached) = latest.lock() {
                    *next_frame_id = next_frame_id.wrapping_add(1).max(1);
                    *cached = Some(CachedPhoneLiveFrame {
                        bytes: frame,
                        width,
                        height,
                        captured_at: now_millis(),
                        frame_id: *next_frame_id,
                    });
                }
            }
        }
    }
}

fn read_mjpeg_stream<R: Read>(
    mut reader: R,
    latest: Arc<Mutex<Option<CachedPhoneLiveFrame>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut buffer = Vec::with_capacity(512 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    let mut next_frame_id = 0u64;

    while !stop.load(Ordering::Relaxed) {
        let read = reader
            .read(&mut chunk)
            .map_err(|error| format!("Could not read the live phone video decoder: {error}"))?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        consume_mjpeg_frames(&mut buffer, &latest, &mut next_frame_id);
    }

    Ok(())
}

fn file_sha256_hex(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("Could not open bundled phone video server: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Could not verify bundled phone video server: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let digest = digest.finalize();
    Ok(digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn resolve_phone_helper_apk(app: &AppHandle) -> Result<Option<PathBuf>, String> {
    let verify = |path: PathBuf| -> Result<Option<PathBuf>, String> {
        if !path.is_file() {
            return Ok(None);
        }
        let actual = file_sha256_hex(&path)
            .map_err(|error| format!("PHONE_HELPER_INTEGRITY_ERROR: {error}"))?;
        if actual != PHONE_HELPER_SHA256 {
            return Err(
                "PHONE_HELPER_INTEGRITY_ERROR: Bundled RepoTunnel Phone helper hash did not match the pinned release."
                    .to_string(),
            );
        }
        Ok(Some(path))
    };

    if let Ok(bundled) = app
        .path()
        .resolve(PHONE_HELPER_RESOURCE, BaseDirectory::Resource)
    {
        if let Some(path) = verify(bundled)? {
            return Ok(Some(path));
        }
    }

    let source_fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("phone")
        .join("repotunnel-phone-helper.apk");
    verify(source_fallback)
}

fn random_phone_helper_nonce() -> Result<String, String> {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce)
        .map_err(|error| format!("Could not create a private Phone helper session: {error}"))?;
    Ok(nonce.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn resolve_scrcpy_server(app: &AppHandle) -> Result<PathBuf, String> {
    let bundled = app
        .path()
        .resolve(SCRCPY_SERVER_RESOURCE, BaseDirectory::Resource)
        .map_err(|error| format!("Could not resolve bundled phone video server: {error}"))?;

    let source_fallback = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("phone")
        .join("scrcpy-server-v4.1");

    let path = if bundled.is_file() {
        bundled
    } else if source_fallback.is_file() {
        source_fallback
    } else {
        return Err("Bundled phone video server is missing.".to_string());
    };

    let actual = file_sha256_hex(&path)?;
    if actual != SCRCPY_SERVER_SHA256 {
        return Err("Bundled phone video server failed its integrity check.".to_string());
    }
    Ok(path)
}

fn scrcpy_scid(device_id: &str) -> u32 {
    let mut digest = Sha256::new();
    digest.update(device_id.as_bytes());
    digest.update(now_millis().to_le_bytes());
    digest.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    let bytes = digest.finalize();
    let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) & 0x7fff_ffff;
    value.max(1)
}

fn run_phone_live_stream_worker(
    target: PhoneRuntimeTarget,
    server_path: PathBuf,
    ffmpeg: PathBuf,
    scid: u32,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<CachedPhoneLiveFrame>>>,
    control: Arc<Mutex<Option<TcpStream>>>,
) -> Result<(), String> {
    const REMOTE_SERVER: &str = "/data/local/tmp/repotunnel-scrcpy-server-v4.1.jar";

    let server_path = server_path
        .to_str()
        .ok_or_else(|| "Bundled phone video server path is not valid UTF-8.".to_string())?;

    let push = run_bounded(
        &target.adb,
        &["-s", &target.serial, "push", server_path, REMOTE_SERVER],
        Duration::from_secs(10),
    )?;
    if !push.status.success() {
        return Err("Could not copy the live phone video server to Android.".to_string());
    }

    let socket_name = format!("localabstract:scrcpy_{scid:08x}");
    let forward = run_bounded(
        &target.adb,
        &["-s", &target.serial, "forward", "tcp:0", &socket_name],
        Duration::from_secs(4),
    )?;
    if !forward.status.success() {
        return Err("Could not create the live phone video ADB tunnel.".to_string());
    }

    let local_port = String::from_utf8_lossy(&forward.stdout)
        .trim()
        .parse::<u16>()
        .map_err(|_| {
            "Android device tools returned an invalid live-video tunnel port.".to_string()
        })?;
    let local_endpoint = format!("tcp:{local_port}");

    let result = (|| -> Result<(), String> {
        let scid_arg = format!("scid={scid:08x}");
        let mut server = Command::new(&target.adb)
            .args([
                "-s",
                &target.serial,
                "shell",
                &format!("CLASSPATH={REMOTE_SERVER}"),
                "app_process",
                "/",
                "com.genymobile.scrcpy.Server",
                SCRCPY_SERVER_VERSION,
                &scid_arg,
                "log_level=warn",
                "audio=false",
                "control=true",
                "clipboard_autosync=false",
                "tunnel_forward=true",
                "raw_stream=true",
                "max_size=1600",
                "max_fps=30",
                "video_bit_rate=6000000",
                "power_on=false",
                "cleanup=false",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not start the live phone video server: {error}"))?;

        let socket_marker = format!("@scrcpy_{scid:08x}");
        let ready_started = Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                let _ = server.kill();
                let _ = server.wait();
                return Ok(());
            }
            if let Some(status) = server.try_wait().map_err(|error| {
                format!("Could not inspect the live phone video server: {error}")
            })? {
                return Err(format!(
                    "The live phone video server exited before streaming started ({status})."
                ));
            }

            let ready = run_bounded(
                &target.adb,
                &[
                    "-s",
                    &target.serial,
                    "shell",
                    "grep",
                    "-q",
                    &socket_marker,
                    "/proc/net/unix",
                ],
                Duration::from_secs(2),
            )
            .map(|output| output.status.success())
            .unwrap_or(false);

            if ready {
                break;
            }
            if ready_started.elapsed() >= Duration::from_secs(5) {
                let _ = server.kill();
                let _ = server.wait();
                return Err("The live phone video server did not become ready in time.".to_string());
            }
            thread::sleep(Duration::from_millis(40));
        }

        let mut video_socket = TcpStream::connect(("127.0.0.1", local_port)).map_err(|error| {
            format!("Could not connect to the live phone video stream: {error}")
        })?;
        video_socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| format!("Could not configure the live phone video socket: {error}"))?;

        let control_socket = TcpStream::connect(("127.0.0.1", local_port)).map_err(|error| {
            format!("Could not connect to the live phone control stream: {error}")
        })?;
        control_socket.set_nodelay(true).map_err(|error| {
            format!("Could not configure the live phone control socket: {error}")
        })?;
        control_socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| {
                format!("Could not configure the live phone control socket: {error}")
            })?;
        let mut control_reader = control_socket
            .try_clone()
            .map_err(|error| format!("Could not clone the live phone control socket: {error}"))?;
        control_reader
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|error| {
                format!("Could not configure the live phone control reader: {error}")
            })?;
        {
            let mut slot = control
                .lock()
                .map_err(|_| "Phone live-control state is unavailable.".to_string())?;
            *slot = Some(control_socket);
        }
        let control_stop = stop.clone();
        let control_thread = thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            while !control_stop.load(Ordering::Relaxed) {
                match control_reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        });

        let mut initial_video = vec![0u8; 64 * 1024];
        let initial_read = video_socket.read(&mut initial_video).map_err(|error| {
            format!("Could not read the initial live phone video stream: {error}")
        })?;
        if initial_read == 0 {
            return Err("The live phone video socket closed before sending video.".to_string());
        }
        initial_video.truncate(initial_read);

        video_socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|error| format!("Could not configure the live phone video socket: {error}"))?;

        let mut decoder = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-an",
                "-q:v",
                "5",
                "-f",
                "image2pipe",
                "-vcodec",
                "mjpeg",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("Could not start the live phone video decoder: {error}"))?;

        let mut decoder_stdin = decoder
            .stdin
            .take()
            .ok_or_else(|| "Could not open the live phone video decoder input.".to_string())?;
        let decoder_stdout = decoder
            .stdout
            .take()
            .ok_or_else(|| "Could not open the live phone video decoder output.".to_string())?;

        let input_stop = stop.clone();
        let input_thread = thread::spawn(move || -> Result<(), String> {
            decoder_stdin
                .write_all(&initial_video)
                .map_err(|error| format!("Could not feed the initial live phone video: {error}"))?;
            let mut buffer = [0u8; 64 * 1024];
            while !input_stop.load(Ordering::Relaxed) {
                match video_socket.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => decoder_stdin.write_all(&buffer[..read]).map_err(|error| {
                        format!("Could not feed the live phone video decoder: {error}")
                    })?,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => {
                        return Err(format!(
                            "Could not read the live phone video stream: {error}"
                        ));
                    }
                }
            }
            Ok(())
        });

        let output_stop = stop.clone();
        let output_latest = latest.clone();
        let output_thread =
            thread::spawn(move || read_mjpeg_stream(decoder_stdout, output_latest, output_stop));

        let stream_result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }

            if let Some(status) = decoder.try_wait().map_err(|error| {
                format!("Could not inspect the live phone video decoder: {error}")
            })? {
                break Err(format!(
                    "The live phone video decoder exited unexpectedly ({status})."
                ));
            }

            if let Some(status) = server.try_wait().map_err(|error| {
                format!("Could not inspect the live phone video server: {error}")
            })? {
                break Err(format!(
                    "The live phone video server exited unexpectedly ({status})."
                ));
            }

            thread::sleep(Duration::from_millis(100));
        };

        let _ = decoder.kill();
        let _ = decoder.wait();
        let _ = server.kill();
        let _ = server.wait();
        if let Ok(mut slot) = control.lock() {
            *slot = None;
        }
        let _ = control_thread.join();

        let input_result = input_thread
            .join()
            .map_err(|_| "Live phone video input worker stopped unexpectedly.".to_string())?;
        let output_result = output_thread
            .join()
            .map_err(|_| "Live phone video output worker stopped unexpectedly.".to_string())?;

        stream_result?;
        input_result?;
        output_result?;
        Ok(())
    })();

    let _ = run_bounded(
        &target.adb,
        &["-s", &target.serial, "forward", "--remove", &local_endpoint],
        Duration::from_secs(3),
    );

    result
}

fn spawn_phone_live_stream(
    app: &AppHandle,
    target: &PhoneRuntimeTarget,
) -> Result<PhoneLiveStreamSession, String> {
    let server_path = resolve_scrcpy_server(app)?;
    let ffmpeg = crate::video::available_ffmpeg_program(app)
        .or_else(|| crate::video::ensure_ffmpeg_program(app).ok())
        .ok_or_else(|| {
            "RepoTunnel could not prepare its private FFmpeg helper for live phone video."
                .to_string()
        })?;
    let scid = scrcpy_scid(&target.device_id);

    let stop = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));
    let latest = Arc::new(Mutex::new(None));
    let control = Arc::new(Mutex::new(None));
    let last_error = Arc::new(Mutex::new(None));

    let worker_target = target.clone();
    let worker_stop = stop.clone();
    let worker_alive = alive.clone();
    let worker_latest = latest.clone();
    let worker_control = control.clone();
    let worker_error = last_error.clone();

    thread::spawn(move || {
        let result = run_phone_live_stream_worker(
            worker_target,
            server_path,
            ffmpeg,
            scid,
            worker_stop.clone(),
            worker_latest,
            worker_control,
        );

        if let Err(error) = result {
            if let Ok(mut last_error) = worker_error.lock() {
                *last_error = Some(error);
            }
        }
        worker_alive.store(false, Ordering::Relaxed);
    });

    Ok(PhoneLiveStreamSession {
        device_id: target.device_id.clone(),
        serial: target.serial.clone(),
        started_at: now_millis(),
        stop,
        alive,
        latest,
        control,
        last_error,
    })
}

fn spawn_phone_control_shell(
    target: &PhoneRuntimeTarget,
) -> Result<PhoneControlShellSession, String> {
    let mut child = Command::new(&target.adb)
        .args(["-s", &target.serial, "shell"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Could not start the low-latency phone control shell: {error}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open the low-latency phone control input.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Could not open the low-latency phone control output.".to_string())?;

    let (sender, output) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if sender.send(line).is_err() {
                break;
            }
        }
    });

    Ok(PhoneControlShellSession {
        device_id: target.device_id.clone(),
        serial: target.serial.clone(),
        child,
        stdin,
        output,
        sequence: 0,
    })
}

fn phone_control_shell_payload(command: &str, marker: &str) -> String {
    format!("{{ {command}; __rt_status=$?; printf '\n%s:%s\n' '{marker}' \"$__rt_status\"; }}\n")
}

fn execute_phone_control_shell_command(
    session: &mut PhoneControlShellSession,
    command: &str,
    timeout: Duration,
) -> Result<(), String> {
    session.sequence = session.sequence.wrapping_add(1).max(1);
    let marker = format!("__REPOTUNNEL_CTL_{:016x}__", session.sequence);

    while session.output.try_recv().is_ok() {}

    let payload = phone_control_shell_payload(command, &marker);
    session
        .stdin
        .write_all(payload.as_bytes())
        .and_then(|_| session.stdin.flush())
        .map_err(|error| format!("Could not send low-latency phone control input: {error}"))?;

    let started = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err("Phone control operation timed out.".to_string());
        }

        match session.output.recv_timeout(remaining) {
            Ok(line) => {
                let line = line.trim();
                let prefix = format!("{marker}:");
                if let Some(status) = line.strip_prefix(&prefix) {
                    return if status.trim() == "0" {
                        Ok(())
                    } else {
                        Err("Phone control operation did not complete.".to_string())
                    };
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                return Err("Phone control operation timed out.".to_string());
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("Low-latency phone control shell disconnected.".to_string());
            }
        }
    }
}

fn parse_display_dimensions(output: &[u8]) -> Option<(u32, u32)> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.split_once(':').map(|(_, value)| value.trim()))
        .filter_map(|value| {
            let (width, height) = value.split_once('x').or_else(|| value.split_once('X'))?;
            let width = width.trim().parse::<u32>().ok()?;
            let height = height.trim().parse::<u32>().ok()?;
            (width > 0 && height > 0).then_some((width, height))
        })
        .next_back()
}

fn oriented_input_dimensions(physical: (u32, u32), stream: Option<(u32, u32)>) -> (u32, u32) {
    let Some((stream_width, stream_height)) = stream else {
        return physical;
    };
    let physical_landscape = physical.0 > physical.1;
    let stream_landscape = stream_width > stream_height;
    if physical_landscape == stream_landscape || physical.0 == physical.1 {
        physical
    } else {
        (physical.1, physical.0)
    }
}

fn record_stream_dimensions(active: &mut ActivePhoneRuntime, stream: (u32, u32)) {
    if active.stream_dimensions != Some(stream) {
        active.stream_dimensions = Some(stream);
        active.display_generation = active.display_generation.wrapping_add(1).max(1);
    }
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn xml_attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(xml_unescape(&tag[start..end]))
}

fn xml_bool_attribute(tag: &str, name: &str) -> bool {
    xml_attribute(tag, name).as_deref() == Some("true")
}

fn parse_android_bounds(value: &str) -> Option<(u32, u32, u32, u32)> {
    let value = value.trim();
    let first_close = value.find(']')?;
    let first = value.get(1..first_close)?;
    let second_open = value[first_close + 1..].find('[')? + first_close + 1;
    let second_close = value[second_open..].find(']')? + second_open;
    let second = value.get(second_open + 1..second_close)?;
    let (left, top) = first.split_once(',')?;
    let (right, bottom) = second.split_once(',')?;
    let left = left.trim().parse::<u32>().ok()?;
    let top = top.trim().parse::<u32>().ok()?;
    let right = right.trim().parse::<u32>().ok()?;
    let bottom = bottom.trim().parse::<u32>().ok()?;
    (right > left && bottom > top).then_some((left, top, right, bottom))
}

fn android_semantic_role(class_name: &str, editable: bool) -> String {
    let class = class_name.rsplit('.').next().unwrap_or(class_name);
    if editable
        || matches!(
            class,
            "EditText" | "AutoCompleteTextView" | "MultiAutoCompleteTextView"
        )
    {
        "textbox".to_string()
    } else {
        match class {
            "Button" | "ImageButton" => "button",
            "CheckBox" => "checkbox",
            "RadioButton" => "radio",
            "Switch" | "ToggleButton" => "switch",
            "Spinner" => "combobox",
            "SeekBar" => "slider",
            "ImageView" => "image",
            "ListView" | "RecyclerView" => "list",
            "GridView" => "grid",
            "WebView" => "webview",
            "TextView" => "text",
            _ => "element",
        }
        .to_string()
    }
}

fn android_sensitive_hint(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "passcode",
        "pin",
        "otp",
        "one-time",
        "verification code",
        "cvv",
        "cvc",
        "card number",
        "credit card",
        "debit card",
        "security code",
        "upi pin",
        "credential",
        "secret",
    ]
    .iter()
    .any(|hint| value.contains(hint))
}

fn semantic_drafts_from_uiautomator(
    xml: &str,
    max_nodes: usize,
) -> Result<(Vec<SemanticNodeDraft>, bool, Option<u32>), String> {
    const MAX_NODES: usize = 800;
    let limit = max_nodes.clamp(20, MAX_NODES);
    let rotation = xml
        .split("<hierarchy")
        .nth(1)
        .and_then(|tail| tail.split('>').next())
        .and_then(|tag| xml_attribute(tag, "rotation"))
        .and_then(|value| value.parse::<u32>().ok());

    let mut drafts = Vec::new();
    let mut truncated = false;
    for (index, tail) in xml.match_indices("<node ").enumerate() {
        if drafts.len() >= limit {
            truncated = true;
            break;
        }
        let start = tail.0;
        let Some(end_relative) = xml[start..].find('>') else {
            continue;
        };
        let tag = &xml[start..start + end_relative + 1];
        let class_name = xml_attribute(tag, "class").unwrap_or_default();
        let resource_id = xml_attribute(tag, "resource-id").unwrap_or_default();
        let content_desc = xml_attribute(tag, "content-desc").unwrap_or_default();
        let raw_text = xml_attribute(tag, "text").unwrap_or_default();
        let enabled = xml_bool_attribute(tag, "enabled");
        let clickable = xml_bool_attribute(tag, "clickable");
        let focusable = xml_bool_attribute(tag, "focusable");
        let focused = xml_bool_attribute(tag, "focused");
        let scrollable = xml_bool_attribute(tag, "scrollable");
        let checked = xml_bool_attribute(tag, "checked");
        let selected = xml_bool_attribute(tag, "selected");
        let password = xml_bool_attribute(tag, "password");
        let editable = class_name.ends_with("EditText")
            || class_name.ends_with("AutoCompleteTextView")
            || class_name.ends_with("MultiAutoCompleteTextView");
        let Some((left, top, right, bottom)) = xml_attribute(tag, "bounds")
            .as_deref()
            .and_then(parse_android_bounds)
        else {
            continue;
        };

        let role = android_semantic_role(&class_name, editable);
        let fallback_name = resource_id
            .rsplit('/')
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or_default();
        let name = if !content_desc.trim().is_empty() {
            content_desc.trim().to_string()
        } else if !raw_text.trim().is_empty() {
            raw_text.trim().to_string()
        } else {
            fallback_name.to_string()
        };
        let sensitivity_context =
            format!("{role} {name} {content_desc} {resource_id} {class_name}");
        let sensitive = password
            || android_sensitive_hint(&sensitivity_context)
            || semantic::semantic_label_is_sensitive(&role, &name, &content_desc);

        let mut states = Vec::new();
        states.push(if enabled { "enabled" } else { "disabled" }.to_string());
        if focusable {
            states.push("focusable".to_string());
        }
        if focused {
            states.push("focused".to_string());
        }
        if editable {
            states.push("editable".to_string());
        }
        if scrollable {
            states.push("scrollable".to_string());
        }
        if checked {
            states.push("checked".to_string());
        }
        if selected {
            states.push("selected".to_string());
        }

        let mut actions = Vec::new();
        if enabled && (clickable || editable) {
            actions.push("click".to_string());
        }
        if enabled && editable && !sensitive {
            actions.push("type".to_string());
        }

        let backend_id =
            format!("android:{index}:{left}:{top}:{right}:{bottom}:{class_name}:{resource_id}");
        let safe_name = if sensitive {
            "Sensitive field".to_string()
        } else {
            name
        };
        let safe_description = if sensitive {
            String::new()
        } else {
            content_desc
        };
        let safe_text = if sensitive || raw_text.is_empty() {
            None
        } else {
            Some(raw_text.clone())
        };
        let safe_value = if sensitive || !editable {
            None
        } else {
            Some(raw_text)
        };

        drafts.push(SemanticNodeDraft {
            backend_id,
            role,
            name: safe_name,
            description: safe_description,
            text: safe_text,
            value: safe_value,
            states,
            actions,
            bounds: Some(SemanticBounds {
                x: f64::from(left),
                y: f64::from(top),
                width: f64::from(right - left),
                height: f64::from(bottom - top),
            }),
            sensitive,
            parent_backend_id: None,
            child_backend_ids: Vec::new(),
        });
    }

    if drafts.is_empty() {
        return Err(
            "SEMANTIC_UNAVAILABLE: Android did not expose any accessibility nodes.".to_string(),
        );
    }
    Ok((drafts, truncated, rotation))
}

fn normalized_coordinate(ratio: f64, size: u32) -> Result<u32, String> {
    if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) || size == 0 {
        return Err("Phone control coordinates must stay inside the visible screen.".to_string());
    }
    Ok(((size.saturating_sub(1) as f64) * ratio).round() as u32)
}

pub(crate) fn valid_android_package_name(package_name: &str) -> bool {
    let package_name = package_name.trim();
    !package_name.is_empty()
        && package_name.len() <= 255
        && !package_name.starts_with('.')
        && !package_name.ends_with('.')
        && !package_name.contains("..")
        && package_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_')
}

fn encoded_input_text_script(text: &str) -> Result<String, String> {
    const MAX_TEXT_CHARS: usize = 2_000;
    let count = text.chars().count();
    if count == 0 || count > MAX_TEXT_CHARS {
        return Err("Phone text input must contain between 1 and 2000 characters.".to_string());
    }
    if text.chars().any(char::is_control) {
        return Err(
            "Phone text input cannot contain control characters; use phone_key for Enter, Tab, or navigation keys."
                .to_string(),
        );
    }

    let encoded = BASE64_STANDARD.encode(text.as_bytes());
    Ok(format!(
        "text=$(printf '%s' '{encoded}' | toybox base64 -d) && input text \"$text\""
    ))
}

fn parse_foreground_component(output: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(output);
    for line in text.lines() {
        if !(line.contains("mResumedActivity")
            || line.contains("topResumedActivity")
            || line.trim_start().starts_with("ACTIVITY "))
        {
            continue;
        }
        for token in line.split_whitespace() {
            let token = token.trim_matches(|ch: char| matches!(ch, '{' | '}' | '[' | ']' | ','));
            let Some((package, activity)) = token.split_once('/') else {
                continue;
            };
            let package = package.trim();
            let activity = activity.trim();
            if valid_android_package_name(package) && !activity.is_empty() {
                return Some((package.to_string(), activity.to_string()));
            }
        }
    }
    None
}

fn parse_keyboard_visible(output: &[u8]) -> Option<bool> {
    let text = String::from_utf8_lossy(output);
    for key in [
        "mInputShown=",
        "mIsInputViewShown=",
        "inputShown=",
        "isInputViewShown=",
    ] {
        if let Some(index) = text.find(key) {
            let value = text[index + key.len()..]
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_matches(|ch: char| matches!(ch, ',' | ';' | '}' | ']'));
            if value.eq_ignore_ascii_case("true") {
                return Some(true);
            }
            if value.eq_ignore_ascii_case("false") {
                return Some(false);
            }
        }
    }
    None
}

fn parse_package_list(output: &[u8]) -> PhonePackageList {
    const MAX_PACKAGES: usize = 1_000;
    let mut packages = String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(str::trim)
        .filter(|package| valid_android_package_name(package))
        .map(str::to_string)
        .collect::<Vec<_>>();
    packages.sort();
    packages.dedup();
    let truncated = packages.len() > MAX_PACKAGES;
    packages.truncate(MAX_PACKAGES);
    PhonePackageList {
        packages,
        truncated,
    }
}

fn bounded_output_text(bytes: &[u8], max_bytes: usize) -> (String, bool) {
    let truncated = bytes.len() > max_bytes;
    let slice = if truncated {
        &bytes[..max_bytes]
    } else {
        bytes
    };
    (String::from_utf8_lossy(slice).into_owned(), truncated)
}

fn bounded_android_diagnostic(output: &Output, fallback: &str) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 2 * 1024;
    let (stderr, stderr_truncated) = bounded_output_text(&output.stderr, MAX_DIAGNOSTIC_BYTES);
    let (stdout, stdout_truncated) = bounded_output_text(&output.stdout, MAX_DIAGNOSTIC_BYTES);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        return fallback.to_string();
    };
    let mut detail = detail.replace('\0', "");
    if stderr_truncated || stdout_truncated {
        detail.push_str(" [truncated]");
    }
    format!("{fallback}: {detail}")
}

fn android_failure_reason(output: &Output, fallback: &str) -> String {
    let diagnostic = bounded_android_diagnostic(output, fallback);
    let lower = diagnostic.to_ascii_lowercase();
    let code = if lower.contains("permission denial")
        || lower.contains("securityexception")
        || lower.contains("permission denied")
    {
        "PERMISSION_DENIED"
    } else if lower.contains("install_failed_user_restricted")
        || lower.contains("install_failed_update_incompatible")
        || lower.contains("blocked by policy")
        || lower.contains("user restriction")
    {
        "POLICY_BLOCKED"
    } else if lower.contains("install_failed_invalid_apk")
        || lower.contains("parse_error")
        || lower.contains("failed to parse")
        || lower.contains("invalid apk")
    {
        "INVALID_FORMAT"
    } else if lower.contains("insufficient storage")
        || lower.contains("install_failed_insufficient_storage")
        || lower.contains("no space left")
    {
        "STORAGE_FULL"
    } else if lower.contains("not found")
        || lower.contains("no such file")
        || lower.contains("unknown package")
        || lower.contains("does not exist")
    {
        "NOT_FOUND"
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "TIMEOUT"
    } else if lower.contains("offline") || lower.contains("device not found") {
        "TRANSPORT_LOST"
    } else {
        "ANDROID_COMMAND_FAILED"
    };
    let exit_code = output.status.code().unwrap_or(-1);
    format!("{code} [exitCode={exit_code}]: {diagnostic}")
}

fn android_script_failure(output: &Output, fallback: &str) -> String {
    match output.status.code() {
        Some(40) => format!("INVALID_FORMAT [exitCode=40]: {fallback}."),
        Some(44) => format!("NOT_FOUND [exitCode=44]: {fallback}."),
        _ => android_failure_reason(output, fallback),
    }
}

fn contextual_phone_error(context: &str, error: String) -> String {
    if let Some((reason, message)) = error.split_once(':') {
        let valid_reason = !reason.is_empty()
            && reason.len() <= 64
            && reason
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if valid_reason {
            return format!("{reason}: {context}:{}", message);
        }
    }
    format!("PHONE_OPERATION_FAILED: {context}: {error}")
}

fn validate_phone_shell_command(command: &str) -> Result<&str, String> {
    const MAX_COMMAND_BYTES: usize = 16 * 1024;
    let command = command.trim();
    if command.is_empty() {
        return Err("Phone shell command cannot be empty.".to_string());
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err("Phone shell command is too long.".to_string());
    }
    if command.as_bytes().contains(&0) {
        return Err("Phone shell command cannot contain NUL bytes.".to_string());
    }

    let mut previous = "";
    for raw in command.split(|character: char| {
        character.is_ascii_whitespace()
            || matches!(character, ';' | '|' | '&' | '(' | ')' | '<' | '>')
    }) {
        let token = raw
            .trim_matches(|character: char| matches!(character, '\'' | '"'))
            .trim();
        if token.is_empty() {
            continue;
        }
        let base = token
            .rsplit('/')
            .next()
            .unwrap_or(token)
            .to_ascii_lowercase();

        if matches!(
            base.as_str(),
            "input" | "sendevent" | "monkey" | "uiautomator" | "screencap" | "screenrecord" | "am"
        ) || (previous == "cmd"
            && matches!(base.as_str(), "activity" | "statusbar" | "clipboard"))
        {
            return Err(
                "POLICY_BLOCKED: phone_shell cannot run Android UI automation or screen-capture primitives. Use RepoTunnel's dedicated Phone tools so payment-app privacy and stale-state guards remain enforceable."
                    .to_string(),
            );
        }
        previous = if base == "cmd" { "cmd" } else { "" };
    }

    Ok(command)
}

fn valid_setting_key(key: &str) -> bool {
    let key = key.trim();
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_network_host(host: &str) -> bool {
    let host = host.trim();
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'_'))
}

fn parse_dns_properties(output: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(output)
        .lines()
        .filter(|line| line.to_ascii_lowercase().contains("dns"))
        .take(32)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn validate_android_path(path: &str) -> Result<&str, String> {
    const MAX_PATH_BYTES: usize = 4_096;
    let path = path.trim();
    if path.is_empty() || !path.starts_with('/') {
        return Err("Phone file path must be an absolute Android path.".to_string());
    }
    if path.len() > MAX_PATH_BYTES || path.as_bytes().contains(&0) {
        return Err("Phone file path is invalid or too long.".to_string());
    }
    Ok(path)
}

fn encoded_android_path(path: &str) -> Result<String, String> {
    let path = validate_android_path(path)?;
    Ok(BASE64_STANDARD.encode(path.as_bytes()))
}

fn parse_file_list(directory: &str, output: &[u8], was_capped: bool) -> PhoneFileList {
    const MAX_ENTRIES: usize = 512;
    let mut entries = output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .take(MAX_ENTRIES + 1)
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .collect::<Vec<_>>();
    let truncated = was_capped || entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    PhoneFileList {
        directory: directory.to_string(),
        entries,
        truncated,
    }
}

fn parse_file_stat(path: &str, output: &[u8]) -> Result<PhoneFileStat, String> {
    let text = String::from_utf8_lossy(output);
    let line = text.trim();
    let mut fields = line.splitn(3, ',');
    let kind = fields
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Could not determine Android file type.".to_string())?
        .to_string();
    let size_bytes = fields
        .next()
        .map(str::trim)
        .ok_or_else(|| "Could not determine Android file size.".to_string())?
        .parse::<u64>()
        .map_err(|_| "Android file size was invalid.".to_string())?;
    let modified_unix_seconds = fields
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "0")
        .and_then(|value| value.parse::<u64>().ok());

    Ok(PhoneFileStat {
        path: path.to_string(),
        kind,
        size_bytes,
        modified_unix_seconds,
    })
}

fn encoded_settings_put_script(
    namespace: PhoneSettingsNamespace,
    key: &str,
    value: &str,
) -> Result<String, String> {
    const MAX_SETTING_VALUE_CHARS: usize = 4_096;
    let key = key.trim();
    if !valid_setting_key(key) {
        return Err("Android settings key is invalid.".to_string());
    }
    let count = value.chars().count();
    if count > MAX_SETTING_VALUE_CHARS {
        return Err("Android settings value is too long.".to_string());
    }
    if value.chars().any(char::is_control) {
        return Err("Android settings value cannot contain control characters.".to_string());
    }
    let encoded = BASE64_STANDARD.encode(value.as_bytes());
    Ok(format!(
        "value=$(printf '%s' '{encoded}' | toybox base64 -d) && settings put {} {} \"$value\"",
        namespace.as_str(),
        key
    ))
}

fn compile_control_sequence(
    steps: &[PhoneControlSequenceStep],
    width: u32,
    height: u32,
) -> Result<Vec<CompiledPhoneControlStep>, String> {
    const MAX_SEQUENCE_STEPS: usize = 64;
    const MAX_WAIT_MS_PER_STEP: u32 = 2_000;
    const MAX_TOTAL_WAIT_MS: u32 = 10_000;

    if steps.is_empty() || steps.len() > MAX_SEQUENCE_STEPS {
        return Err("Phone sequence must contain between 1 and 64 steps.".to_string());
    }

    let mut total_wait_ms = 0u32;
    let mut has_mutation = false;
    let mut compiled = Vec::with_capacity(steps.len());

    for step in steps {
        match *step {
            PhoneControlSequenceStep::Tap { x_ratio, y_ratio } => {
                has_mutation = true;
                compiled.push(CompiledPhoneControlStep::Tap {
                    x: normalized_coordinate(x_ratio, width)?,
                    y: normalized_coordinate(y_ratio, height)?,
                });
            }
            PhoneControlSequenceStep::Swipe(gesture) => {
                if !(50..=3_000).contains(&gesture.duration_ms) {
                    return Err("Phone swipe duration must be between 50 and 3000 ms.".to_string());
                }
                has_mutation = true;
                compiled.push(CompiledPhoneControlStep::Swipe {
                    start_x: normalized_coordinate(gesture.start_x_ratio, width)?,
                    start_y: normalized_coordinate(gesture.start_y_ratio, height)?,
                    end_x: normalized_coordinate(gesture.end_x_ratio, width)?,
                    end_y: normalized_coordinate(gesture.end_y_ratio, height)?,
                    duration_ms: gesture.duration_ms,
                });
            }
            PhoneControlSequenceStep::Wait { duration_ms } => {
                if duration_ms > MAX_WAIT_MS_PER_STEP {
                    return Err("Phone sequence wait steps may be at most 2000 ms.".to_string());
                }
                total_wait_ms = total_wait_ms.saturating_add(duration_ms);
                if total_wait_ms > MAX_TOTAL_WAIT_MS {
                    return Err(
                        "Phone sequence total wait time may be at most 10000 ms.".to_string()
                    );
                }
                compiled.push(CompiledPhoneControlStep::Wait { duration_ms });
            }
        }
    }

    if !has_mutation {
        return Err("Phone sequence must contain at least one tap or swipe.".to_string());
    }

    Ok(compiled)
}

fn read_phone_helper_response(
    socket: TcpStream,
    max_response_bytes: usize,
) -> Result<Vec<u8>, String> {
    read_phone_helper_response_with_timeouts(
        socket,
        max_response_bytes,
        PHONE_HELPER_READ_POLL_INTERVAL,
        PHONE_HELPER_RESPONSE_TIMEOUT,
    )
}

fn read_phone_helper_response_with_timeouts(
    socket: TcpStream,
    max_response_bytes: usize,
    poll_interval: Duration,
    response_timeout: Duration,
) -> Result<Vec<u8>, String> {
    socket
        .set_read_timeout(Some(poll_interval))
        .map_err(|error| format!("PHONE_HELPER_UNAVAILABLE: {error}"))?;

    let deadline = Instant::now() + response_timeout;
    let mut response = Vec::new();
    let mut reader = BufReader::new(socket);

    loop {
        let remaining = max_response_bytes
            .saturating_add(1)
            .saturating_sub(response.len());
        if remaining == 0 {
            break;
        }

        match reader
            .by_ref()
            .take(remaining as u64)
            .read_until(b'\n', &mut response)
        {
            Ok(_) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "PHONE_HELPER_UNAVAILABLE: Android semantic helper did not respond within {} ms.",
                        response_timeout.as_millis()
                    ));
                }
            }
            Err(error) => return Err(format!("PHONE_HELPER_UNAVAILABLE: {error}")),
        }
    }

    Ok(response)
}

impl PhoneRuntimeState {
    fn stop_live_stream(&self) {
        if let Ok(mut stream) = self.live_stream.lock() {
            if let Some(session) = stream.take() {
                session.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    fn stop_control_shell(&self) {
        if let Ok(mut shell) = self.control_shell.lock() {
            if let Some(mut session) = shell.take() {
                let _ = session.child.kill();
                let _ = session.child.wait();
            }
        }
    }

    fn stop_semantic_helper(&self) {
        if let Ok(mut helper) = self.semantic_helper.lock() {
            if let Some(session) = helper.take() {
                let endpoint = format!("tcp:{}", session.local_port);
                let _ = run_bounded(
                    &session.adb,
                    &[
                        "-s",
                        &session.serial,
                        "forward",
                        "--remove",
                        endpoint.as_str(),
                    ],
                    Duration::from_secs(3),
                );
            }
        }
    }

    fn run_low_latency_control_on_target(
        &self,
        app: &AppHandle,
        device_id: &str,
        target: &PhoneRuntimeTarget,
        command: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;

        let mut shell = self
            .control_shell
            .lock()
            .map_err(|_| "Phone control state is unavailable.".to_string())?;

        let needs_new = match shell.as_mut() {
            Some(session)
                if session.device_id == target.device_id && session.serial == target.serial =>
            {
                !matches!(session.child.try_wait(), Ok(None))
            }
            Some(_) => true,
            None => true,
        };

        if needs_new {
            if let Some(mut previous) = shell.take() {
                let _ = previous.child.kill();
                let _ = previous.child.wait();
            }
            *shell = Some(spawn_phone_control_shell(target)?);
        }

        let result = shell
            .as_mut()
            .ok_or_else(|| "Low-latency phone control shell is unavailable.".to_string())
            .and_then(|session| execute_phone_control_shell_command(session, command, timeout));

        if result.is_err() {
            if let Some(mut failed) = shell.take() {
                let _ = failed.child.kill();
                let _ = failed.child.wait();
            }
        }
        drop(shell);

        if result.is_ok() {
            if let Ok(mut runtime) = self.inner.lock() {
                if let Some(active) = runtime
                    .as_mut()
                    .filter(|active| active.target.device_id == device_id)
                {
                    active.last_used_at = now_millis();
                }
            }
        }

        result
    }

    fn run_low_latency_control(
        &self,
        app: &AppHandle,
        device_id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let target = self.fast_target_for_action(device_id)?;
        self.ensure_foreground_ai_ui_access_allowed(&target)?;
        self.run_low_latency_control_on_target(app, device_id, &target, command, timeout)
    }

    fn cached_live_frame_for(
        &self,
        device_id: &str,
        serial: &str,
    ) -> Result<Option<CachedPhoneLiveFrame>, String> {
        let stream = self
            .live_stream
            .lock()
            .map_err(|_| "Phone live-stream state is unavailable.".to_string())?;

        let Some(session) = stream.as_ref() else {
            return Ok(None);
        };
        if session.device_id != device_id
            || session.serial != serial
            || !session.alive.load(Ordering::Relaxed)
        {
            return Ok(None);
        }

        session
            .latest
            .lock()
            .map(|frame| frame.clone())
            .map_err(|_| "Phone live-screen frame cache is unavailable.".to_string())
    }

    fn screen_frame_from_cached(frame: CachedPhoneLiveFrame) -> PhoneScreenFrame {
        let size_bytes = u64::try_from(frame.bytes.len()).unwrap_or(u64::MAX);
        PhoneScreenFrame {
            mime_type: "image/jpeg".to_string(),
            data_base64: BASE64_STANDARD.encode(frame.bytes),
            size_bytes,
            width: frame.width,
            height: frame.height,
            captured_at: frame.captured_at,
            frame_id: frame.frame_id,
        }
    }

    fn active_target_without_probe(&self, device_id: &str) -> Option<PhoneRuntimeTarget> {
        self.inner.lock().ok().and_then(|runtime| {
            runtime
                .as_ref()
                .filter(|active| active.target.device_id == device_id)
                .map(|active| active.target.clone())
        })
    }

    fn fast_target_for_action(&self, device_id: &str) -> Result<PhoneRuntimeTarget, String> {
        if let Some(target) = self.active_target_without_probe(device_id) {
            let live_stream_is_active = self
                .cached_live_frame_for(device_id, &target.serial)
                .ok()
                .flatten()
                .is_some();
            if live_stream_is_active {
                return Ok(target);
            }

            if let Ok(mut shell) = self.control_shell.lock() {
                if let Some(session) = shell.as_mut() {
                    let same_target =
                        session.device_id == target.device_id && session.serial == target.serial;
                    if same_target && matches!(session.child.try_wait(), Ok(None)) {
                        return Ok(target);
                    }
                }
            }
        }

        self.target_for_action(device_id)
    }

    fn live_stream_frame(
        &self,
        app: &AppHandle,
        target: &PhoneRuntimeTarget,
    ) -> Result<Option<CachedPhoneLiveFrame>, String> {
        const FAILED_STREAM_RETRY_DELAY_MS: u64 = 5_000;

        let mut stream = self
            .live_stream
            .lock()
            .map_err(|_| "Phone live-stream state is unavailable.".to_string())?;

        if let Some(session) = stream.as_ref() {
            let same_target =
                session.device_id == target.device_id && session.serial == target.serial;
            if same_target {
                if session.alive.load(Ordering::Relaxed) {
                    return session
                        .latest
                        .lock()
                        .map(|frame| frame.clone())
                        .map_err(|_| "Phone live-screen frame cache is unavailable.".to_string());
                }

                if now_millis().saturating_sub(session.started_at) < FAILED_STREAM_RETRY_DELAY_MS {
                    let _last_error = session
                        .last_error
                        .lock()
                        .ok()
                        .and_then(|error| error.clone());
                    return Ok(None);
                }
            }

            session.stop.store(true, Ordering::Relaxed);
        }

        let session = spawn_phone_live_stream(app, target)?;
        let latest = session.latest.clone();
        let alive = session.alive.clone();
        *stream = Some(session);
        drop(stream);

        let first_frame_started = Instant::now();
        loop {
            let frame = latest
                .lock()
                .map(|frame| frame.clone())
                .map_err(|_| "Phone live-screen frame cache is unavailable.".to_string())?;
            if frame.is_some() {
                return Ok(frame);
            }
            if !alive.load(Ordering::Relaxed)
                || first_frame_started.elapsed() >= Duration::from_secs(4)
            {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub(crate) fn observation_metadata(
        &self,
        device_id: &str,
    ) -> Result<PhoneObservationMetadata, String> {
        let (serial, display_generation, physical_display_dimensions, cached_stream_dimensions) = {
            let runtime = self
                .inner
                .lock()
                .map_err(|_| "Phone runtime state is unavailable.".to_string())?;
            let active = runtime
                .as_ref()
                .filter(|active| active.target.device_id == device_id)
                .ok_or_else(|| "The selected phone runtime is unavailable.".to_string())?;
            (
                active.target.serial.clone(),
                active.display_generation,
                active.physical_display_dimensions,
                active.stream_dimensions,
            )
        };

        let cached = self.cached_live_frame_for(device_id, &serial)?.or_else(|| {
            cached_stream_dimensions.map(|(width, height)| CachedPhoneLiveFrame {
                bytes: Vec::new(),
                width,
                height,
                captured_at: 0,
                frame_id: 0,
            })
        });
        let stream_dimensions = cached
            .as_ref()
            .map(|frame| (frame.width, frame.height))
            .or(cached_stream_dimensions);
        let orientation =
            stream_dimensions
                .or(physical_display_dimensions)
                .map(|(width, height)| {
                    if width > height {
                        "landscape".to_string()
                    } else {
                        "portrait".to_string()
                    }
                });
        let (physical_width, physical_height) = physical_display_dimensions
            .map(|(width, height)| (Some(width), Some(height)))
            .unwrap_or((None, None));
        let (stream_width, stream_height) = stream_dimensions
            .map(|(width, height)| (Some(width), Some(height)))
            .unwrap_or((None, None));

        Ok(PhoneObservationMetadata {
            display_generation,
            frame_id: cached
                .as_ref()
                .and_then(|frame| (frame.frame_id != 0).then_some(frame.frame_id)),
            orientation,
            physical_width,
            physical_height,
            stream_width,
            stream_height,
        })
    }

    pub(crate) fn live_stream_healthy(&self, device_id: &str) -> bool {
        self.live_stream
            .lock()
            .ok()
            .and_then(|stream| {
                stream
                    .as_ref()
                    .filter(|session| session.device_id == device_id)
                    .map(|session| session.alive.load(Ordering::Relaxed))
            })
            .unwrap_or(false)
    }

    pub(crate) fn status(&self) -> Result<PhoneRuntimeStatus, String> {
        let runtime = self
            .inner
            .lock()
            .map_err(|_| "Phone runtime state is unavailable.".to_string())?;
        Ok(runtime_status_from(runtime.as_ref()))
    }

    pub(crate) fn clear(&self) -> Result<PhoneRuntimeStatus, String> {
        self.stop_live_stream();
        self.stop_control_shell();
        self.stop_semantic_helper();
        let mut runtime = self
            .inner
            .lock()
            .map_err(|_| "Phone runtime state is unavailable.".to_string())?;
        if let Some(active) = runtime.as_ref() {
            semantic::invalidate_target(
                PHONE_SEMANTIC_SCOPE,
                SemanticSurface::Phone,
                &active.target.device_id,
            );
        }
        *runtime = None;
        Ok(runtime_status_from(None))
    }

    pub(crate) fn ensure(&self, device_id: &str) -> Result<PhoneRuntimeStatus, String> {
        self.ensure_preferred(device_id, None)
    }

    pub(crate) fn ensure_preferred(
        &self,
        device_id: &str,
        preferred_transport: Option<&str>,
    ) -> Result<PhoneRuntimeStatus, String> {
        let preferred_transport = preferred_transport
            .map(str::trim)
            .filter(|transport| matches!(*transport, "usb" | "wireless"));

        let mut runtime = self
            .inner
            .lock()
            .map_err(|_| "Phone runtime state is unavailable.".to_string())?;

        if let Some(active) = runtime.as_mut() {
            if active.target.device_id == device_id && runtime_target_alive(&active.target) {
                let already_preferred = preferred_transport
                    .is_none_or(|preferred| active.target.transport == preferred);
                if already_preferred {
                    active.last_used_at = now_millis();
                    return Ok(runtime_status_from(Some(active)));
                }
            }
        }

        let previous = runtime
            .as_ref()
            .filter(|active| active.target.device_id == device_id)
            .map(|active| {
                (
                    active.session_started_at,
                    active.target.serial.clone(),
                    active.physical_display_dimensions,
                    active.stream_dimensions,
                    active.display_generation,
                    active.settings_write_available,
                    active.settings_write_reason.clone(),
                )
            });

        let targets = resolve_runtime_targets(device_id)?;
        let target = preferred_transport
            .and_then(|preferred| {
                targets
                    .iter()
                    .find(|target| target.transport == preferred)
                    .cloned()
            })
            .or_else(|| choose_runtime_target(targets))
            .ok_or_else(|| "The selected phone is not currently connected.".to_string())?;

        if let Some(active) = runtime.as_mut() {
            if active.target.device_id == device_id
                && active.target.serial == target.serial
                && runtime_target_alive(&active.target)
            {
                active.target = target;
                active.last_used_at = now_millis();
                return Ok(runtime_status_from(Some(active)));
            }
        }

        let switched_target = previous
            .as_ref()
            .is_some_and(|(_, serial, ..)| serial != &target.serial);
        let now = now_millis();
        let (
            session_started_at,
            physical_display_dimensions,
            stream_dimensions,
            display_generation,
            settings_write_available,
            settings_write_reason,
        ) = previous
            .map(
                |(started_at, _, physical, stream, generation, settings_write, settings_reason)| {
                    (
                        started_at,
                        physical,
                        stream,
                        generation,
                        settings_write,
                        settings_reason,
                    )
                },
            )
            .unwrap_or((now, None, None, 1, None, None));

        if switched_target {
            // Stop transport-bound sessions before publishing the new target so a
            // concurrent screen/control request cannot create a fresh session that
            // this transport switch then tears down.
            self.stop_live_stream();
            self.stop_control_shell();
            self.stop_semantic_helper();
        }

        *runtime = Some(ActivePhoneRuntime {
            target,
            session_started_at,
            last_used_at: now,
            physical_display_dimensions,
            stream_dimensions,
            display_generation,
            settings_write_available,
            settings_write_reason,
        });
        Ok(runtime_status_from(runtime.as_ref()))
    }

    fn target_for_action(&self, device_id: &str) -> Result<PhoneRuntimeTarget, String> {
        self.ensure(device_id)?;
        let runtime = self
            .inner
            .lock()
            .map_err(|_| "Phone runtime state is unavailable.".to_string())?;
        runtime
            .as_ref()
            .filter(|active| active.target.device_id == device_id)
            .map(|active| active.target.clone())
            .ok_or_else(|| "The selected phone runtime is unavailable.".to_string())
    }

    pub(crate) fn run_authorized_adb(
        &self,
        app: &AppHandle,
        device_id: &str,
        capability: PhoneCapability,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Output, String> {
        require_capability(app, device_id, capability)?;
        let target = self.target_for_action(device_id)?;

        let mut command_args = Vec::with_capacity(args.len() + 2);
        command_args.push("-s");
        command_args.push(target.serial.as_str());
        command_args.extend_from_slice(args);
        let output = run_bounded(&target.adb, &command_args, timeout)?;

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime.as_mut() {
                if active.target.device_id == device_id {
                    active.last_used_at = now_millis();
                }
            }
        }

        Ok(output)
    }

    fn run_authorized_shell_script(
        &self,
        app: &AppHandle,
        device_id: &str,
        capability: PhoneCapability,
        script: &str,
        timeout: Duration,
    ) -> Result<Output, String> {
        require_capability(app, device_id, capability)?;
        let target = self.target_for_action(device_id)?;
        let output = run_bounded_adb_script(&target.adb, &target.serial, "shell", script, timeout)?;

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime.as_mut() {
                if active.target.device_id == device_id {
                    active.last_used_at = now_millis();
                }
            }
        }

        Ok(output)
    }

    pub(crate) fn probe_display(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhoneRuntimeProbe, String> {
        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::ViewScreen,
            &["shell", "wm", "size"],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err("Could not read the phone display size.".to_string());
        }

        let display_size = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.split_once(':').map(|(_, value)| value.trim()))
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 32
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'x' || byte == b'X')
            })
            .map(str::to_ascii_lowercase);

        Ok(PhoneRuntimeProbe {
            runtime: self.status()?,
            display_size,
        })
    }

    pub(crate) fn capture_screen(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhoneScreenFrame, String> {
        const MAX_PNG_SCREEN_FRAME_BYTES: usize = 12 * 1024 * 1024;
        const MAX_RAW_SCREEN_FRAME_BYTES: usize = 32 * 1024 * 1024;
        const SCREEN_CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

        fn capture_from_target(
            app: &AppHandle,
            target: &PhoneRuntimeTarget,
        ) -> Result<(Vec<u8>, &'static str, u32, u32), String> {
            if let Some(ffmpeg) = crate::video::available_ffmpeg_program(app) {
                if let Ok(raw) = run_bounded(
                    &target.adb,
                    &["-s", &target.serial, "exec-out", "screencap"],
                    SCREEN_CAPTURE_TIMEOUT,
                ) {
                    if raw.status.success()
                        && !raw.stdout.is_empty()
                        && raw.stdout.len() <= MAX_RAW_SCREEN_FRAME_BYTES
                    {
                        if let Some((width, height, rgba)) = raw_screencap_rgba(&raw.stdout) {
                            if width <= 10_000 && height <= 10_000 {
                                if let Ok(jpeg) = encode_rgba_jpeg(&ffmpeg, rgba, width, height) {
                                    return Ok((jpeg, "image/jpeg", width, height));
                                }
                            }
                        }
                    }
                }
            }

            let output = run_bounded(
                &target.adb,
                &["-s", &target.serial, "exec-out", "screencap", "-p"],
                SCREEN_CAPTURE_TIMEOUT,
            )?;
            if !output.status.success() {
                return Err("Could not capture the phone screen.".to_string());
            }
            if output.stdout.is_empty() || output.stdout.len() > MAX_PNG_SCREEN_FRAME_BYTES {
                return Err("Phone screen capture returned an invalid frame size.".to_string());
            }

            let (width, height) = png_dimensions(&output.stdout).ok_or_else(|| {
                "Phone screen capture did not return a valid PNG frame.".to_string()
            })?;
            if width > 10_000 || height > 10_000 {
                return Err("Phone screen dimensions are outside the supported range.".to_string());
            }

            Ok((output.stdout, "image/png", width, height))
        }

        let primary = self.target_for_action(device_id)?;
        self.ensure_foreground_ai_ui_access_allowed(&primary)?;
        if let Ok(Some(frame)) = self.live_stream_frame(app, &primary) {
            if let Ok(mut runtime) = self.inner.lock() {
                if let Some(active) = runtime
                    .as_mut()
                    .filter(|active| active.target.device_id == device_id)
                {
                    active.last_used_at = now_millis();
                }
            }

            let size_bytes = u64::try_from(frame.bytes.len()).unwrap_or(u64::MAX);
            return Ok(PhoneScreenFrame {
                mime_type: "image/jpeg".to_string(),
                data_base64: BASE64_STANDARD.encode(frame.bytes),
                size_bytes,
                width: frame.width,
                height: frame.height,
                captured_at: frame.captured_at,
                frame_id: frame.frame_id,
            });
        }

        let mut attempts = vec![primary.clone()];
        if let Ok(targets) = resolve_runtime_targets(device_id) {
            let mut alternatives = targets
                .into_iter()
                .filter(|target| target.serial != primary.serial)
                .collect::<Vec<_>>();
            alternatives.sort_by_key(|target| {
                (
                    target.transport == primary.transport,
                    transport_rank(&target.transport),
                )
            });
            attempts.extend(alternatives);
        }

        let mut last_error = "Could not capture the phone screen.".to_string();
        for target in attempts {
            match capture_from_target(app, &target) {
                Ok((frame, mime_type, width, height)) => {
                    let now = now_millis();
                    if let Ok(mut runtime) = self.inner.lock() {
                        let session_started_at = runtime
                            .as_ref()
                            .filter(|active| active.target.device_id == device_id)
                            .map(|active| active.session_started_at)
                            .unwrap_or(now);
                        let previous_generation = runtime
                            .as_ref()
                            .filter(|active| active.target.device_id == device_id)
                            .map(|active| active.display_generation)
                            .unwrap_or(0);
                        *runtime = Some(ActivePhoneRuntime {
                            target,
                            session_started_at,
                            last_used_at: now,
                            physical_display_dimensions: Some((width, height)),
                            stream_dimensions: None,
                            display_generation: previous_generation.wrapping_add(1).max(1),
                            settings_write_available: runtime
                                .as_ref()
                                .and_then(|active| active.settings_write_available),
                            settings_write_reason: runtime
                                .as_ref()
                                .and_then(|active| active.settings_write_reason.clone()),
                        });
                    }

                    let size_bytes = u64::try_from(frame.len()).unwrap_or(u64::MAX);
                    return Ok(PhoneScreenFrame {
                        mime_type: mime_type.to_string(),
                        data_base64: BASE64_STANDARD.encode(frame),
                        size_bytes,
                        width,
                        height,
                        captured_at: now,
                        frame_id: now,
                    });
                }
                Err(error) => last_error = error,
            }
        }

        Err(last_error)
    }

    pub(crate) fn local_ui_screen_frame(
        &self,
        app: &AppHandle,
        device_id: &str,
        after_captured_at: Option<u64>,
    ) -> Result<PhoneScreenFrame, String> {
        require_capability(app, device_id, PhoneCapability::ViewScreen)?;

        let target = match self.active_target_without_probe(device_id) {
            Some(target) => target,
            None => self.target_for_action(device_id)?,
        };

        let mut frame = self.cached_live_frame_for(device_id, &target.serial)?;
        if frame.is_none() {
            frame = self.live_stream_frame(app, &target)?;
        }

        if let Some(after) = after_captured_at {
            if frame
                .as_ref()
                .is_none_or(|current| current.captured_at <= after)
            {
                let started = Instant::now();
                while started.elapsed() < Duration::from_millis(120) {
                    thread::sleep(Duration::from_millis(8));
                    if let Some(current) = self.cached_live_frame_for(device_id, &target.serial)? {
                        let changed = current.captured_at > after;
                        frame = Some(current);
                        if changed {
                            break;
                        }
                    }
                }
            }
        }

        let frame = frame.ok_or_else(|| {
            "PHONE_LIVE_VIEW_UNAVAILABLE: Persistent phone video stream did not produce a frame."
                .to_string()
        })?;

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                active.last_used_at = now_millis();
                record_stream_dimensions(active, (frame.width, frame.height));
            }
        }

        Ok(Self::screen_frame_from_cached(frame))
    }

    pub(crate) fn fast_screen(
        &self,
        app: &AppHandle,
        device_id: &str,
        after_captured_at: Option<u64>,
        wait_for_change_ms: u32,
    ) -> Result<(PhoneScreenFrame, bool), String> {
        require_capability(app, device_id, PhoneCapability::ViewScreen)?;

        let target = match self.active_target_without_probe(device_id) {
            Some(target) => target,
            None => self.target_for_action(device_id)?,
        };
        self.ensure_foreground_ai_ui_access_allowed(&target)?;

        let mut frame = self.cached_live_frame_for(device_id, &target.serial)?;
        if frame.is_none() {
            frame = self.live_stream_frame(app, &target)?;
        }

        let wait_for_change_ms = wait_for_change_ms.min(1_000);
        if let Some(after) = after_captured_at {
            if wait_for_change_ms > 0
                && frame
                    .as_ref()
                    .is_none_or(|current| current.captured_at <= after)
            {
                let started = Instant::now();
                while started.elapsed() < Duration::from_millis(u64::from(wait_for_change_ms)) {
                    thread::sleep(Duration::from_millis(10));
                    if let Some(current) = self.cached_live_frame_for(device_id, &target.serial)? {
                        let changed = current.captured_at > after;
                        frame = Some(current);
                        if changed {
                            break;
                        }
                    }
                }
            }
        }

        let Some(frame) = frame else {
            let fallback = self.capture_screen(app, device_id)?;
            let changed = after_captured_at
                .map(|after| fallback.captured_at > after)
                .unwrap_or(true);
            return Ok((fallback, changed));
        };

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                active.last_used_at = now_millis();
                record_stream_dimensions(active, (frame.width, frame.height));
            }
        }

        let changed = after_captured_at
            .map(|after| frame.captured_at > after)
            .unwrap_or(true);
        Ok((Self::screen_frame_from_cached(frame), changed))
    }

    fn refresh_input_dimensions(
        &self,
        app: &AppHandle,
        device_id: &str,
        target: &PhoneRuntimeTarget,
    ) -> Result<(u32, u32), String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "wm", "size"],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not read physical phone display dimensions",
            ));
        }
        let physical = parse_display_dimensions(&output.stdout)
            .ok_or_else(|| "Could not determine physical phone display dimensions.".to_string())?;
        let stream = self
            .cached_live_frame_for(device_id, &target.serial)?
            .map(|frame| (frame.width, frame.height))
            .or_else(|| {
                self.inner.lock().ok().and_then(|runtime| {
                    runtime
                        .as_ref()
                        .filter(|active| active.target.device_id == device_id)
                        .and_then(|active| active.stream_dimensions)
                })
            });

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                let changed = active.physical_display_dimensions != Some(physical)
                    || (stream.is_some() && active.stream_dimensions != stream);
                active.physical_display_dimensions = Some(physical);
                if stream.is_some() {
                    active.stream_dimensions = stream;
                }
                if changed {
                    active.display_generation = active.display_generation.wrapping_add(1).max(1);
                }
                active.last_used_at = now_millis();
            }
        }

        Ok(oriented_input_dimensions(physical, stream))
    }

    fn input_dimensions(&self, app: &AppHandle, device_id: &str) -> Result<(u32, u32), String> {
        if let Ok(runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_ref()
                .filter(|active| active.target.device_id == device_id)
            {
                if let Some(physical) = active.physical_display_dimensions {
                    let stream = self
                        .cached_live_frame_for(device_id, &active.target.serial)
                        .ok()
                        .flatten()
                        .map(|frame| (frame.width, frame.height))
                        .or(active.stream_dimensions);
                    return Ok(oriented_input_dimensions(physical, stream));
                }
            }
        }

        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::ControlInput,
            &["shell", "wm", "size"],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err("Could not read physical phone display dimensions.".to_string());
        }

        let physical = parse_display_dimensions(&output.stdout)
            .ok_or_else(|| "Could not determine physical phone display dimensions.".to_string())?;

        let mut stream = None;
        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                if active.physical_display_dimensions != Some(physical) {
                    active.physical_display_dimensions = Some(physical);
                    active.display_generation = active.display_generation.wrapping_add(1).max(1);
                }
                stream = active.stream_dimensions;
            }
        }

        if stream.is_none() {
            if let Some(target) = self.active_target_without_probe(device_id) {
                stream = self
                    .cached_live_frame_for(device_id, &target.serial)
                    .ok()
                    .flatten()
                    .map(|frame| (frame.width, frame.height));
            }
        }

        Ok(oriented_input_dimensions(physical, stream))
    }

    fn verify_action_guard(
        &self,
        device_id: &str,
        target: &PhoneRuntimeTarget,
        guard: &PhoneActionGuard,
    ) -> Result<(), String> {
        if let Some(expected_frame_id) = guard.expected_frame_id {
            let current = self
                .cached_live_frame_for(device_id, &target.serial)?
                .ok_or_else(|| {
                    "STALE_FRAME: No current live phone frame is available; no mutation was sent."
                        .to_string()
                })?;
            if current.frame_id != expected_frame_id {
                return Err(format!(
                    "STALE_FRAME: Expected phone frame {expected_frame_id}, but current frame is {}. No mutation was sent.",
                    current.frame_id
                ));
            }
        }

        if let Some(expected_generation) = guard.expected_display_generation {
            let current_generation = self
                .inner
                .lock()
                .ok()
                .and_then(|runtime| {
                    runtime
                        .as_ref()
                        .filter(|active| active.target.device_id == device_id)
                        .map(|active| active.display_generation)
                })
                .ok_or_else(|| {
                    "STALE_UI: Phone display generation is unavailable; no mutation was sent."
                        .to_string()
                })?;
            if current_generation != expected_generation {
                return Err(format!(
                    "STALE_UI: Expected display generation {expected_generation}, but current generation is {current_generation}. No mutation was sent."
                ));
            }
        }

        if let Some(expected_orientation) = guard.expected_orientation.as_deref() {
            let expected_orientation = expected_orientation.trim().to_ascii_lowercase();
            if !matches!(expected_orientation.as_str(), "portrait" | "landscape") {
                return Err(
                    "INVALID_ARGUMENT: expected_orientation must be portrait or landscape."
                        .to_string(),
                );
            }
            let current = self
                .cached_live_frame_for(device_id, &target.serial)?
                .ok_or_else(|| {
                    "STALE_UI: Current phone orientation is unavailable; no mutation was sent."
                        .to_string()
                })?;
            let current_orientation = if current.width > current.height {
                "landscape"
            } else {
                "portrait"
            };
            if current_orientation != expected_orientation {
                return Err(format!(
                    "STALE_UI: Expected {expected_orientation} orientation, but current orientation is {current_orientation}. No mutation was sent."
                ));
            }
        }

        if guard.expected_package.is_some() || guard.expected_activity.is_some() {
            let current = Self::foreground_component_on_target(target)?;
            let current_package = current.as_ref().map(|(package, _)| package.as_str());
            let current_activity = current.as_ref().map(|(_, activity)| activity.as_str());

            if let Some(expected_package) = guard.expected_package.as_deref() {
                let expected_package = expected_package.trim();
                if !valid_android_package_name(expected_package) {
                    return Err("INVALID_ARGUMENT: expected_package is invalid.".to_string());
                }
                if current_package != Some(expected_package) {
                    return Err(format!(
                        "STALE_UI: Expected foreground package {expected_package}, but current foreground package is {}. No mutation was sent.",
                        current_package.unwrap_or("unknown")
                    ));
                }
            }

            if let Some(expected_activity) = guard.expected_activity.as_deref() {
                let expected_activity = expected_activity.trim();
                if expected_activity.is_empty()
                    || expected_activity.len() > 256
                    || expected_activity.chars().any(char::is_whitespace)
                {
                    return Err("INVALID_ARGUMENT: expected_activity is invalid.".to_string());
                }
                if current_activity != Some(expected_activity) {
                    return Err(format!(
                        "STALE_UI: Expected foreground activity {expected_activity}, but current foreground activity is {}. No mutation was sent.",
                        current_activity.unwrap_or("unknown")
                    ));
                }
            }
        }

        Ok(())
    }

    pub(crate) fn local_ui_tap(
        &self,
        app: &AppHandle,
        device_id: &str,
        x_ratio: f64,
        y_ratio: f64,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let target = self.fast_target_for_action(device_id)?;
        let (width, height) = self.input_dimensions(app, device_id)?;
        let x = normalized_coordinate(x_ratio, width)?;
        let y = normalized_coordinate(y_ratio, height)?;
        let command = format!("input tap {x} {y}");
        self.run_low_latency_control_on_target(
            app,
            device_id,
            &target,
            &command,
            Duration::from_secs(2),
        )
    }

    pub(crate) fn local_ui_swipe(
        &self,
        app: &AppHandle,
        device_id: &str,
        gesture: PhoneSwipeGesture,
    ) -> Result<(), String> {
        if !(50..=3_000).contains(&gesture.duration_ms) {
            return Err("Phone swipe duration must be between 50 and 3000 ms.".to_string());
        }
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let target = self.fast_target_for_action(device_id)?;
        let (width, height) = self.input_dimensions(app, device_id)?;
        let start_x = normalized_coordinate(gesture.start_x_ratio, width)?;
        let start_y = normalized_coordinate(gesture.start_y_ratio, height)?;
        let end_x = normalized_coordinate(gesture.end_x_ratio, width)?;
        let end_y = normalized_coordinate(gesture.end_y_ratio, height)?;
        let command = format!(
            "input swipe {start_x} {start_y} {end_x} {end_y} {}",
            gesture.duration_ms
        );
        self.run_low_latency_control_on_target(
            app,
            device_id,
            &target,
            &command,
            Duration::from_secs(4),
        )
    }

    pub(crate) fn local_ui_key_event(
        &self,
        app: &AppHandle,
        device_id: &str,
        key: PhoneKey,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let target = self.fast_target_for_action(device_id)?;
        let command = format!("input keyevent {}", key.android_keycode());
        self.run_low_latency_control_on_target(
            app,
            device_id,
            &target,
            &command,
            Duration::from_secs(2),
        )
    }

    pub(crate) fn local_ui_type_text(
        &self,
        app: &AppHandle,
        device_id: &str,
        text: &str,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        if text.is_empty() || text.chars().count() > 2_000 || text.chars().any(char::is_control) {
            return Err(
                "INVALID_ARGUMENT: Phone text must contain 1..2000 non-control characters."
                    .to_string(),
            );
        }

        let target = self.fast_target_for_action(device_id)?;
        if self.live_stream_healthy(device_id) {
            self.scrcpy_clipboard_paste(app, &target, text)
        } else {
            let script = encoded_input_text_script(text)?;
            self.run_low_latency_control_on_target(
                app,
                device_id,
                &target,
                &script,
                Duration::from_secs(4),
            )
        }
    }

    pub(crate) fn tap_guarded(
        &self,
        app: &AppHandle,
        device_id: &str,
        x_ratio: f64,
        y_ratio: f64,
        guard: &PhoneActionGuard,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let target = self.fast_target_for_action(device_id)?;
        self.verify_action_guard(device_id, &target, guard)?;

        let (width, height) = self.refresh_input_dimensions(app, device_id, &target)?;
        let x = normalized_coordinate(x_ratio, width)?.to_string();
        let y = normalized_coordinate(y_ratio, height)?.to_string();

        // Revalidate immediately before dispatch so a frame/package/orientation change
        // between grounding and coordinate conversion cannot silently retarget the tap.
        self.verify_action_guard(device_id, &target, guard)?;
        let command = format!("input tap {x} {y}");
        self.run_low_latency_control(app, device_id, &command, Duration::from_secs(2))
    }

    pub(crate) fn swipe_guarded(
        &self,
        app: &AppHandle,
        device_id: &str,
        gesture: PhoneSwipeGesture,
        guard: &PhoneActionGuard,
    ) -> Result<(), String> {
        if !(50..=3_000).contains(&gesture.duration_ms) {
            return Err("Phone swipe duration must be between 50 and 3000 ms.".to_string());
        }

        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let target = self.fast_target_for_action(device_id)?;
        self.verify_action_guard(device_id, &target, guard)?;

        let (width, height) = self.refresh_input_dimensions(app, device_id, &target)?;
        let start_x = normalized_coordinate(gesture.start_x_ratio, width)?.to_string();
        let start_y = normalized_coordinate(gesture.start_y_ratio, height)?.to_string();
        let end_x = normalized_coordinate(gesture.end_x_ratio, width)?.to_string();
        let end_y = normalized_coordinate(gesture.end_y_ratio, height)?.to_string();
        let duration = gesture.duration_ms.to_string();

        self.verify_action_guard(device_id, &target, guard)?;
        let command = format!("input swipe {start_x} {start_y} {end_x} {end_y} {duration}");
        self.run_low_latency_control(app, device_id, &command, Duration::from_secs(4))
    }

    pub(crate) fn key_event(
        &self,
        app: &AppHandle,
        device_id: &str,
        key: PhoneKey,
    ) -> Result<(), String> {
        let command = format!("input keyevent {}", key.android_keycode());
        self.run_low_latency_control(app, device_id, &command, Duration::from_secs(2))
    }

    pub(crate) fn type_text_verified(
        &self,
        app: &AppHandle,
        device_id: &str,
        text: &str,
    ) -> Result<PhoneSemanticActionReceipt, String> {
        let snapshot = self.semantic_snapshot(app, device_id, 800, None)?;
        let focused = snapshot
            .snapshot
            .nodes
            .iter()
            .find(|node| {
                node.role == "textbox"
                    && node
                        .states
                        .iter()
                        .any(|state| state.eq_ignore_ascii_case("focused"))
                    && node
                        .states
                        .iter()
                        .any(|state| state.eq_ignore_ascii_case("editable"))
            })
            .ok_or_else(|| {
                "NOT_EDITABLE: No focused editable Android semantic field is available. No text was sent."
                    .to_string()
            })?;
        if focused.sensitive {
            return Err(
                "SENSITIVE_FIELD: RepoTunnel blocks AI text entry into password, OTP, payment, and credential fields."
                    .to_string(),
            );
        }
        let ref_id = focused.ref_id.clone();
        self.semantic_action(
            app,
            device_id,
            &snapshot.snapshot.snapshot_id,
            &ref_id,
            "set_text",
            Some(text),
        )
    }

    pub(crate) fn list_packages(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhonePackageList, String> {
        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::AppControl,
            &["shell", "pm", "list", "packages"],
            Duration::from_secs(5),
        )?;
        if !output.status.success() {
            return Err("Could not list phone applications.".to_string());
        }
        Ok(parse_package_list(&output.stdout))
    }

    fn package_exists_on_target(
        target: &PhoneRuntimeTarget,
        package_name: &str,
    ) -> Result<bool, String> {
        let output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "pm", "path", package_name],
            Duration::from_secs(3),
        )?;
        Ok(output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim_start().starts_with("package:")))
    }

    fn package_running_on_target(
        target: &PhoneRuntimeTarget,
        package_name: &str,
    ) -> Result<bool, String> {
        let output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "pidof", package_name],
            Duration::from_secs(3),
        )?;
        Ok(output.status.success() && !output.stdout.iter().all(u8::is_ascii_whitespace))
    }

    fn foreground_component_on_target(
        target: &PhoneRuntimeTarget,
    ) -> Result<Option<(String, String)>, String> {
        let output = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "dumpsys",
                "activity",
                "activities",
            ],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not read Android foreground activity",
            ));
        }
        Ok(parse_foreground_component(&output.stdout))
    }

    fn wait_for_foreground_package(
        target: &PhoneRuntimeTarget,
        package_name: &str,
        timeout: Duration,
    ) -> Result<bool, String> {
        let started = Instant::now();
        loop {
            if Self::foreground_component_on_target(target)?
                .as_ref()
                .is_some_and(|(package, _)| package == package_name)
            {
                return Ok(true);
            }
            if started.elapsed() >= timeout {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(120));
        }
    }

    pub(crate) fn foreground_package_is(
        &self,
        app: &AppHandle,
        device_id: &str,
        package_name: &str,
    ) -> Result<bool, String> {
        let package_name = package_name.trim();
        if !valid_android_package_name(package_name) {
            return Err("INVALID_ARGUMENT: Android package name is invalid.".to_string());
        }
        require_capability(app, device_id, PhoneCapability::AppControl)?;
        let target = self.fast_target_for_action(device_id)?;
        Ok(Self::foreground_component_on_target(&target)?
            .as_ref()
            .is_some_and(|(package, _)| package == package_name))
    }

    fn keyboard_visible_on_target(target: &PhoneRuntimeTarget) -> Result<Option<bool>, String> {
        let output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "dumpsys", "input_method"],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not read Android keyboard state",
            ));
        }
        Ok(parse_keyboard_visible(&output.stdout))
    }

    fn helper_apk_path_on_target(target: &PhoneRuntimeTarget) -> Result<Option<String>, String> {
        let output = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "pm",
                "path",
                PHONE_HELPER_PACKAGE,
            ],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Ok(None);
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix("package:"))
            .map(str::trim)
            .filter(|path| path.starts_with('/') && !path.contains('\0'))
            .map(str::to_string))
    }

    fn helper_installed_on_target(target: &PhoneRuntimeTarget) -> Result<bool, String> {
        Ok(Self::helper_apk_path_on_target(target)?.is_some())
    }

    fn helper_integrity_on_target(target: &PhoneRuntimeTarget) -> Result<bool, String> {
        let Some(path) = Self::helper_apk_path_on_target(target)? else {
            return Ok(false);
        };
        let encoded = encoded_android_path(&path)?;
        let script = format!(
            "p=$(printf '%s' '{encoded}' | toybox base64 -d) || exit 40; [ -f \"$p\" ] || exit 44; toybox sha256sum \"$p\""
        );
        let output = run_bounded_adb_script(
            &target.adb,
            &target.serial,
            "shell",
            &script,
            Duration::from_secs(5),
        )?;
        if !output.status.success() {
            return Err(android_script_failure(
                &output,
                "Could not verify the installed RepoTunnel Phone helper",
            ));
        }
        let digest = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        Ok(digest == PHONE_HELPER_SHA256)
    }

    fn helper_accessibility_enabled_on_target(target: &PhoneRuntimeTarget) -> Result<bool, String> {
        let output = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "settings",
                "get",
                "secure",
                "enabled_accessibility_services",
            ],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not read Android accessibility-service state",
            ));
        }
        let value = String::from_utf8_lossy(&output.stdout);
        Ok(value.split(':').map(str::trim).any(|component| {
            component.eq_ignore_ascii_case(PHONE_HELPER_SERVICE)
                || component.eq_ignore_ascii_case(
                    "com.repotunnel.phonehelper/com.repotunnel.phonehelper.PhoneAccessibilityService",
                )
        }))
    }

    pub(crate) fn semantic_helper_status(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhoneSemanticHelperStatus, String> {
        let target = self.target_for_action(device_id)?;
        let bundled = resolve_phone_helper_apk(app)?.is_some();
        let installed = Self::helper_installed_on_target(&target)?;
        let integrity_verified = installed && Self::helper_integrity_on_target(&target)?;
        let accessibility_enabled = integrity_verified
            && Self::helper_accessibility_enabled_on_target(&target).unwrap_or(false);
        let ready = installed && integrity_verified && accessibility_enabled;
        let message = if ready {
            "Precise semantic Phone control is ready.".to_string()
        } else if !bundled {
            "The RepoTunnel Phone helper APK has not been bundled into this build.".to_string()
        } else if !installed {
            "Install the bundled RepoTunnel Phone helper to enable precise semantic control."
                .to_string()
        } else if !integrity_verified {
            "The installed RepoTunnel Phone helper does not match this RepoTunnel build. Reinstall the bundled helper before enabling semantic control."
                .to_string()
        } else {
            "Enable RepoTunnel Phone Helper in Android Accessibility settings to enable precise semantic control. RepoTunnel normally stays enabled across payment apps and blocks AI UI access there; only the explicit payment compatibility fallback turns this Accessibility service off."
                .to_string()
        };
        Ok(PhoneSemanticHelperStatus {
            bundled,
            installed,
            integrity_verified,
            accessibility_enabled,
            ready,
            needs_user_enablement: installed && integrity_verified && !accessibility_enabled,
            message,
        })
    }

    pub(crate) fn install_semantic_helper(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhoneSemanticHelperStatus, String> {
        require_capability(app, device_id, PhoneCapability::AppInstall)?;
        let target = self.target_for_action(device_id)?;
        let apk = resolve_phone_helper_apk(app)?.ok_or_else(|| {
            "PHONE_HELPER_NOT_BUNDLED: RepoTunnel Phone helper APK is unavailable in this build."
                .to_string()
        })?;
        let apk = apk.to_str().ok_or_else(|| {
            "PHONE_HELPER_NOT_BUNDLED: RepoTunnel Phone helper APK path is invalid.".to_string()
        })?;
        let output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "install", "-r", apk],
            Duration::from_secs(90),
        )?;
        if !output.status.success()
            || !String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("success")
        {
            return Err(android_failure_reason(
                &output,
                "Could not install the RepoTunnel Phone helper",
            ));
        }
        self.stop_semantic_helper();
        self.semantic_helper_status(app, device_id)
    }

    fn obvious_payment_sensitive_package(package_name: &str) -> bool {
        let package = package_name.trim().to_ascii_lowercase();
        [
            ".bank", "banking", "wallet", "payment", "payments", "phonepe", "paytm", "paypal",
            "gpay", "bhim", "razorpay", "cashapp", "venmo", "revolut", "upi", "npci",
        ]
        .iter()
        .any(|hint| package.contains(hint))
    }

    fn helper_payment_sensitive_package(
        &self,
        target: &PhoneRuntimeTarget,
        package_name: &str,
    ) -> Result<Option<bool>, String> {
        let obvious = Self::obvious_payment_sensitive_package(package_name);
        if !Self::helper_installed_on_target(target)?
            || !Self::helper_integrity_on_target(target)?
            || !Self::helper_accessibility_enabled_on_target(target)?
        {
            return Ok(Some(obvious));
        }

        let session = self.ensure_semantic_helper_session(target)?;
        let response = Self::phone_helper_request(
            &session,
            &serde_json::json!({
                "op": "classify_payment_package",
                "packageName": package_name,
            }),
        )?;
        Ok(response
            .get("paymentSensitive")
            .and_then(serde_json::Value::as_bool)
            .map(|sensitive| sensitive || obvious)
            .or(Some(obvious)))
    }

    fn helper_foreground_policy_request(
        session: &PhoneSemanticHelperSession,
    ) -> Result<serde_json::Value, String> {
        let request = serde_json::json!({ "op": "foreground_policy" });
        let mut last_error = None;

        for attempt in 0..3 {
            match Self::phone_helper_request(session, &request) {
                Ok(response) => return Ok(response),
                Err(error) if error.starts_with("PHONE_HELPER_UNAVAILABLE:") && attempt < 2 => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            "PHONE_HELPER_UNAVAILABLE: Foreground payment policy check failed.".to_string()
        }))
    }

    fn helper_foreground_payment_sensitive(
        &self,
        target: &PhoneRuntimeTarget,
    ) -> Result<Option<bool>, String> {
        let existing_session = self
            .semantic_helper
            .lock()
            .ok()
            .and_then(|helper| helper.as_ref().cloned())
            .filter(|session| {
                session.device_id == target.device_id && session.serial == target.serial
            });

        if let Some(session) = existing_session {
            let response = Self::helper_foreground_policy_request(&session)?;
            return Ok(response
                .get("paymentSensitive")
                .and_then(serde_json::Value::as_bool));
        }

        if !Self::helper_accessibility_enabled_on_target(target).unwrap_or(false) {
            return Ok(None);
        }

        let session = self.ensure_semantic_helper_session(target)?;
        let response = Self::helper_foreground_policy_request(&session)?;
        Ok(response
            .get("paymentSensitive")
            .and_then(serde_json::Value::as_bool))
    }

    fn ensure_foreground_ai_ui_access_allowed(
        &self,
        target: &PhoneRuntimeTarget,
    ) -> Result<(), String> {
        if self
            .helper_foreground_payment_sensitive(target)?
            .unwrap_or(false)
        {
            return Err(
                "PAYMENT_APP_BLOCKED: RepoTunnel Accessibility remains enabled, but AI inspection and control are blocked while a payment-sensitive app is foreground."
                    .to_string(),
            );
        }

        if !Self::helper_accessibility_enabled_on_target(target).unwrap_or(false) {
            if let Some((package_name, _)) = Self::foreground_component_on_target(target)? {
                if Self::obvious_payment_sensitive_package(&package_name) {
                    return Err(
                        "PAYMENT_APP_BLOCKED: RepoTunnel AI inspection and control are blocked while a payment-sensitive app is foreground."
                            .to_string(),
                    );
                }
            }
        }

        Ok(())
    }

    pub(crate) fn pause_semantic_helper_for_payment(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhoneSemanticHelperStatus, String> {
        require_capability(app, device_id, PhoneCapability::AppControl)?;
        let target = self.target_for_action(device_id)?;

        if !Self::helper_installed_on_target(&target)?
            || !Self::helper_integrity_on_target(&target)?
            || !Self::helper_accessibility_enabled_on_target(&target)?
        {
            return self.semantic_helper_status(app, device_id);
        }

        let session = self.ensure_semantic_helper_session(&target)?;
        let response = Self::phone_helper_request(
            &session,
            &serde_json::json!({ "op": "pause_for_payment" }),
        )?;
        if !response
            .get("paymentSafeMode")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Err(
                "PAYMENT_SAFE_MODE_FAILED: Android helper did not confirm Payment Safe Mode."
                    .to_string(),
            );
        }

        self.stop_semantic_helper();
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !Self::helper_accessibility_enabled_on_target(&target).unwrap_or(true) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }

        let status = self.semantic_helper_status(app, device_id)?;
        if status.accessibility_enabled {
            return Err(
                "PAYMENT_SAFE_MODE_TIMEOUT: Android did not finish disabling RepoTunnel accessibility. Open Accessibility settings and turn off RepoTunnel Phone Helper before continuing with the payment."
                    .to_string(),
            );
        }
        Ok(status)
    }

    pub(crate) fn open_semantic_helper_settings(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::AppControl)?;
        let target = self.target_for_action(device_id)?;
        let output = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "am",
                "start",
                "-a",
                "android.settings.ACCESSIBILITY_SETTINGS",
            ],
            Duration::from_secs(5),
        )?;
        if output.status.success() {
            Ok(())
        } else {
            Err(android_failure_reason(
                &output,
                "Could not open Android Accessibility settings",
            ))
        }
    }

    fn phone_helper_request(
        session: &PhoneSemanticHelperSession,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        const MAX_REQUEST_BYTES: usize = 256 * 1024;
        const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

        let mut payload = request.clone();
        let object = payload.as_object_mut().ok_or_else(|| {
            "PHONE_HELPER_PROTOCOL_ERROR: Helper request must be an object.".to_string()
        })?;
        object.insert(
            "nonce".to_string(),
            serde_json::Value::String(session.nonce.clone()),
        );
        let mut encoded = serde_json::to_vec(&payload)
            .map_err(|error| format!("PHONE_HELPER_PROTOCOL_ERROR: {error}"))?;
        if encoded.len() > MAX_REQUEST_BYTES {
            return Err("PHONE_HELPER_PROTOCOL_ERROR: Helper request is too large.".to_string());
        }
        encoded.push(b'\n');

        let mut socket = TcpStream::connect(("127.0.0.1", session.local_port))
            .map_err(|error| format!("PHONE_HELPER_UNAVAILABLE: {error}"))?;
        socket
            .set_write_timeout(Some(Duration::from_secs(4)))
            .map_err(|error| format!("PHONE_HELPER_UNAVAILABLE: {error}"))?;
        socket
            .write_all(&encoded)
            .and_then(|_| socket.flush())
            .map_err(|error| format!("PHONE_HELPER_UNAVAILABLE: {error}"))?;

        let response = read_phone_helper_response(socket, MAX_RESPONSE_BYTES)?;
        if response.is_empty() {
            return Err(
                "PHONE_HELPER_UNAVAILABLE: Android semantic helper closed the connection before returning a response."
                    .to_string(),
            );
        }
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(
                "PHONE_HELPER_PROTOCOL_ERROR: Helper returned an oversized response.".to_string(),
            );
        }
        let value: serde_json::Value = serde_json::from_slice(&response).map_err(|_| {
            "PHONE_HELPER_PROTOCOL_ERROR: Helper returned invalid JSON.".to_string()
        })?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            let reason = value
                .get("reasonCode")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("PHONE_HELPER_ERROR");
            let message = value
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Android semantic helper rejected the request.");
            return Err(format!("{reason}: {message}"));
        }
        Ok(value)
    }

    fn ensure_semantic_helper_session(
        &self,
        target: &PhoneRuntimeTarget,
    ) -> Result<PhoneSemanticHelperSession, String> {
        let existing_session = self
            .semantic_helper
            .lock()
            .ok()
            .and_then(|helper| helper.as_ref().cloned());
        if let Some(session) = existing_session {
            let same_target =
                session.device_id == target.device_id && session.serial == target.serial;
            if same_target
                && Self::phone_helper_request(&session, &serde_json::json!({"op": "ping"})).is_ok()
            {
                return Ok(session);
            }

            // A transport switch (wireless <-> USB) must tear down the old ADB
            // forward before creating a helper session on the new target.
            self.stop_semantic_helper();
        }

        if !Self::helper_installed_on_target(target)? {
            return Err(
                "PHONE_HELPER_UNAVAILABLE: RepoTunnel Phone helper is not installed.".to_string(),
            );
        }
        if !Self::helper_integrity_on_target(target)? {
            return Err(
                "PHONE_HELPER_INTEGRITY_ERROR: Installed RepoTunnel Phone helper does not match the pinned helper bundled with this build."
                    .to_string(),
            );
        }
        if !Self::helper_accessibility_enabled_on_target(target)? {
            return Err(
                "PHONE_HELPER_UNAVAILABLE: RepoTunnel Phone Helper is not enabled in Android Accessibility settings."
                    .to_string(),
            );
        }

        let nonce = random_phone_helper_nonce()?;
        let broadcast = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "am",
                "broadcast",
                "-a",
                PHONE_HELPER_CONFIG_ACTION,
                "-p",
                PHONE_HELPER_PACKAGE,
                "--es",
                "nonce",
                &nonce,
            ],
            Duration::from_secs(4),
        )?;
        if !broadcast.status.success()
            || !String::from_utf8_lossy(&broadcast.stdout).contains("result=0")
        {
            return Err(android_failure_reason(
                &broadcast,
                "PHONE_HELPER_UNAVAILABLE: Could not configure the private Phone helper session",
            ));
        }

        let socket_name = format!("localabstract:{PHONE_HELPER_SOCKET}");
        let forward = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "forward",
                "tcp:0",
                socket_name.as_str(),
            ],
            Duration::from_secs(4),
        )?;
        if !forward.status.success() {
            return Err(android_failure_reason(
                &forward,
                "PHONE_HELPER_UNAVAILABLE: Could not create the private Phone helper tunnel",
            ));
        }
        let local_port = String::from_utf8_lossy(&forward.stdout)
            .trim()
            .parse::<u16>()
            .map_err(|_| {
                "PHONE_HELPER_PROTOCOL_ERROR: ADB returned an invalid helper tunnel port."
                    .to_string()
            })?;

        let session = PhoneSemanticHelperSession {
            device_id: target.device_id.clone(),
            serial: target.serial.clone(),
            adb: target.adb.clone(),
            nonce,
            local_port,
        };
        if let Err(error) = Self::phone_helper_request(&session, &serde_json::json!({"op": "ping"}))
        {
            let endpoint = format!("tcp:{local_port}");
            let _ = run_bounded(
                &target.adb,
                &[
                    "-s",
                    &target.serial,
                    "forward",
                    "--remove",
                    endpoint.as_str(),
                ],
                Duration::from_secs(3),
            );
            return Err(error);
        }

        let mut helper = self
            .semantic_helper
            .lock()
            .map_err(|_| "Phone semantic helper state is unavailable.".to_string())?;
        *helper = Some(session.clone());
        Ok(session)
    }

    fn helper_semantic_snapshot(
        &self,
        target: &PhoneRuntimeTarget,
        max_nodes: usize,
    ) -> Result<(Vec<SemanticNodeDraft>, bool, u64), String> {
        let session = self.ensure_semantic_helper_session(target)?;
        let request = serde_json::json!({
            "op": "snapshot",
            "maxNodes": max_nodes.clamp(20, 800),
        });
        let mut last_stale_error = None;
        let mut value = None;
        for attempt in 0..3 {
            match Self::phone_helper_request(&session, &request) {
                Ok(snapshot) => {
                    value = Some(snapshot);
                    break;
                }
                Err(error) if error.starts_with("STALE_UI:") => {
                    last_stale_error = Some(error);
                    if attempt < 2 {
                        thread::sleep(Duration::from_millis(75));
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let value = value.ok_or_else(|| {
            last_stale_error.unwrap_or_else(|| {
                "STALE_UI: Android UI kept changing while the semantic snapshot was collected."
                    .to_string()
            })
        })?;
        let generation = value
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                "PHONE_HELPER_PROTOCOL_ERROR: Helper snapshot generation is missing.".to_string()
            })?;
        let raw_nodes = value.get("nodes").cloned().ok_or_else(|| {
            "PHONE_HELPER_PROTOCOL_ERROR: Helper snapshot nodes are missing.".to_string()
        })?;
        let drafts = serde_json::from_value::<Vec<SemanticNodeDraft>>(raw_nodes)
            .map_err(|error| format!("PHONE_HELPER_PROTOCOL_ERROR: {error}"))?;
        if drafts.is_empty() {
            return Err(
                "SEMANTIC_UNAVAILABLE: Android helper exposed no semantic nodes.".to_string(),
            );
        }
        Ok((
            drafts,
            value
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            generation,
        ))
    }

    fn helper_semantic_action(
        &self,
        target: &PhoneRuntimeTarget,
        backend_id: &str,
        action: &str,
        text: Option<&str>,
    ) -> Result<(bool, bool), String> {
        let session = self.ensure_semantic_helper_session(target)?;
        let mut request = serde_json::json!({
            "op": "action",
            "backendId": backend_id,
            "action": action,
        });
        if let Some(text) = text {
            if let Some(object) = request.as_object_mut() {
                object.insert(
                    "text".to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        }
        let value = Self::phone_helper_request(&session, &request)?;
        Ok((
            value
                .get("deviceAccepted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            value
                .get("verified")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ))
    }

    fn uiautomator_xml_on_target(target: &PhoneRuntimeTarget) -> Result<String, String> {
        const MAX_XML_BYTES: usize = 4 * 1024 * 1024;
        let suffix = format!("{:08x}", scrcpy_scid(&target.device_id));
        let remote = format!("/data/local/tmp/repotunnel-semantic-{suffix}.xml");
        let script = format!(
            "f='{remote}'; rm -f \"$f\"; uiautomator dump \"$f\" >/dev/null 2>&1; rc=$?; if [ \"$rc\" -eq 0 ] && [ -f \"$f\" ]; then size=$(toybox stat -c '%s' \"$f\" 2>/dev/null || echo 0); if [ \"$size\" -gt {MAX_XML_BYTES} ]; then rm -f \"$f\"; exit 43; fi; cat \"$f\"; rc=$?; fi; rm -f \"$f\"; exit \"$rc\""
        );
        let mut last_error =
            "SEMANTIC_UNAVAILABLE: Android accessibility snapshot failed.".to_string();
        for attempt in 0..2 {
            let output = run_bounded_adb_script(
                &target.adb,
                &target.serial,
                "shell",
                &script,
                Duration::from_secs(6),
            )?;
            if output.status.success() {
                let xml = String::from_utf8_lossy(&output.stdout).into_owned();
                if xml.contains("<hierarchy") && xml.contains("<node ") {
                    return Ok(xml);
                }
                last_error =
                    "SEMANTIC_UNAVAILABLE: Android returned an empty accessibility hierarchy."
                        .to_string();
            } else if output.status.code() == Some(43) {
                last_error =
                    "SEMANTIC_UNAVAILABLE: Android accessibility hierarchy exceeded 4 MiB."
                        .to_string();
            } else {
                last_error = android_failure_reason(
                    &output,
                    "SEMANTIC_UNAVAILABLE: Android accessibility snapshot failed",
                );
            }
            if attempt == 0 {
                thread::sleep(Duration::from_millis(100));
            }
        }
        Err(last_error)
    }

    pub(crate) fn semantic_snapshot(
        &self,
        app: &AppHandle,
        device_id: &str,
        max_nodes: usize,
        known_hash: Option<String>,
    ) -> Result<PhoneSemanticSnapshotResult, String> {
        require_capability(app, device_id, PhoneCapability::ViewScreen)?;
        let target = self.target_for_action(device_id)?;
        self.ensure_foreground_ai_ui_access_allowed(&target)?;

        let helper_ready = Self::helper_installed_on_target(&target)?
            && Self::helper_accessibility_enabled_on_target(&target).unwrap_or(false);
        let (drafts, truncated, rotation, helper_generation, source) = if helper_ready {
            let (drafts, truncated, generation) =
                self.helper_semantic_snapshot(&target, max_nodes)?;
            (
                drafts,
                truncated,
                None,
                Some(generation),
                "accessibilityService".to_string(),
            )
        } else {
            let xml = Self::uiautomator_xml_on_target(&target)?;
            let (drafts, truncated, rotation) = semantic_drafts_from_uiautomator(&xml, max_nodes)?;
            (
                drafts,
                truncated,
                rotation,
                None,
                "uiautomatorFallback".to_string(),
            )
        };

        let foreground = Self::foreground_component_on_target(&target)?;
        let (package_name, activity_name) = foreground
            .clone()
            .map(|(package, activity)| (Some(package), Some(activity)))
            .unwrap_or((None, None));

        let physical_output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "wm", "size"],
            Duration::from_secs(3),
        )?;
        let physical = physical_output
            .status
            .success()
            .then(|| parse_display_dimensions(&physical_output.stdout))
            .flatten();

        let stream = self
            .cached_live_frame_for(device_id, &target.serial)?
            .map(|frame| (frame.width, frame.height));

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                if let Some(physical) = physical {
                    if active.physical_display_dimensions != Some(physical) {
                        active.physical_display_dimensions = Some(physical);
                        active.display_generation =
                            active.display_generation.wrapping_add(1).max(1);
                    }
                }
                if let Some(stream) = stream {
                    record_stream_dimensions(active, stream);
                }
            }
        }

        let orientation = match rotation {
            Some(1 | 3) => "landscape",
            Some(0 | 2) => "portrait",
            _ => stream
                .map(|(width, height)| {
                    if width > height {
                        "landscape"
                    } else {
                        "portrait"
                    }
                })
                .or_else(|| {
                    physical.map(|(width, height)| {
                        if width > height {
                            "landscape"
                        } else {
                            "portrait"
                        }
                    })
                })
                .unwrap_or("unknown"),
        }
        .to_string();

        let package_for_identity = package_name.as_deref().unwrap_or("unknown");
        let activity_for_identity = activity_name.as_deref().unwrap_or("unknown");
        let generation_identity = helper_generation
            .map(|generation| format!("g{generation}"))
            .unwrap_or_else(|| {
                rotation
                    .map(|value| format!("r{value}"))
                    .unwrap_or_else(|| orientation.clone())
            });
        let document_identity =
            format!("{package_for_identity}/{activity_for_identity}:{generation_identity}");
        let snapshot = semantic::publish_snapshot(SemanticSnapshotInput {
            workspace_id: PHONE_SEMANTIC_SCOPE.to_string(),
            surface: SemanticSurface::Phone,
            target_id: device_id.to_string(),
            document_identity,
            nodes: drafts,
            truncated,
            known_hash,
        })?;

        let keyboard_visible = Self::keyboard_visible_on_target(&target).ok().flatten();
        let observation = self.observation_metadata(device_id).ok();
        Ok(PhoneSemanticSnapshotResult {
            snapshot,
            package_name,
            activity_name,
            orientation,
            display_generation: observation
                .as_ref()
                .map(|value| value.display_generation)
                .unwrap_or(0),
            frame_id: observation.as_ref().and_then(|value| value.frame_id),
            physical_display: physical.map(|(width, height)| SemanticBounds {
                x: 0.0,
                y: 0.0,
                width: f64::from(width),
                height: f64::from(height),
            }),
            stream_frame: stream.map(|(width, height)| SemanticBounds {
                x: 0.0,
                y: 0.0,
                width: f64::from(width),
                height: f64::from(height),
            }),
            keyboard_visible,
            source,
        })
    }

    pub(crate) fn semantic_find(
        &self,
        device_id: &str,
        snapshot_id: &str,
        query: SemanticFindQuery,
    ) -> Result<Vec<SemanticNode>, String> {
        semantic::find_nodes(
            PHONE_SEMANTIC_SCOPE,
            SemanticSurface::Phone,
            device_id,
            snapshot_id,
            query,
        )
        .map_err(|error| error.to_string())
    }

    fn semantic_ref_target(
        device_id: &str,
        snapshot_id: &str,
        ref_id: &str,
    ) -> Result<SemanticRefTarget, String> {
        semantic::resolve_ref(
            PHONE_SEMANTIC_SCOPE,
            SemanticSurface::Phone,
            device_id,
            snapshot_id,
            ref_id,
        )
        .map_err(|error| error.to_string())
    }

    fn android_backend_bounds(backend_id: &str) -> Result<(u32, u32, u32, u32), String> {
        let mut parts = backend_id.splitn(7, ':');
        if parts.next() != Some("android") {
            return Err("STALE_REF: Phone semantic backend identity is invalid.".to_string());
        }
        let _index = parts.next();
        let parse = |value: Option<&str>| {
            value
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| "STALE_REF: Phone semantic bounds are invalid.".to_string())
        };
        let left = parse(parts.next())?;
        let top = parse(parts.next())?;
        let right = parse(parts.next())?;
        let bottom = parse(parts.next())?;
        if right <= left || bottom <= top {
            return Err("STALE_REF: Phone semantic bounds are invalid.".to_string());
        }
        Ok((left, top, right, bottom))
    }

    fn scrcpy_clipboard_paste(
        &self,
        app: &AppHandle,
        target: &PhoneRuntimeTarget,
        text: &str,
    ) -> Result<(), String> {
        const SCRCPY_SET_CLIPBOARD: u8 = 9;
        const MAX_CLIPBOARD_BYTES: usize = 64 * 1024;
        if text.is_empty() || text.len() > MAX_CLIPBOARD_BYTES || text.as_bytes().contains(&0) {
            return Err(
                "INVALID_ARGUMENT: Phone text must be non-empty, at most 64 KiB, and contain no NUL bytes."
                    .to_string(),
            );
        }

        let _ = self.live_stream_frame(app, target)?;
        let control = {
            let stream = self
                .live_stream
                .lock()
                .map_err(|_| "Phone live-stream state is unavailable.".to_string())?;
            stream
                .as_ref()
                .filter(|session| {
                    session.device_id == target.device_id
                        && session.serial == target.serial
                        && session.alive.load(Ordering::Relaxed)
                })
                .map(|session| session.control.clone())
                .ok_or_else(|| {
                    "PHONE_CONTROL_UNAVAILABLE: Persistent phone control channel is not active."
                        .to_string()
                })?
        };

        let started = Instant::now();
        loop {
            let mut slot = control
                .lock()
                .map_err(|_| "Phone live-control state is unavailable.".to_string())?;
            if let Some(socket) = slot.as_mut() {
                let mut payload = Vec::with_capacity(14 + text.len());
                payload.push(SCRCPY_SET_CLIPBOARD);
                payload.extend_from_slice(&now_millis().to_be_bytes());
                payload.push(1);
                let length = u32::try_from(text.len())
                    .map_err(|_| "Phone clipboard text is too large.".to_string())?;
                payload.extend_from_slice(&length.to_be_bytes());
                payload.extend_from_slice(text.as_bytes());
                socket
                    .write_all(&payload)
                    .and_then(|_| socket.flush())
                    .map_err(|error| {
                        format!(
                            "PHONE_CONTROL_UNAVAILABLE: Could not send Unicode text through the phone control channel: {error}"
                        )
                    })?;
                return Ok(());
            }
            drop(slot);
            if started.elapsed() >= Duration::from_millis(1_500) {
                return Err(
                    "PHONE_CONTROL_UNAVAILABLE: Persistent phone control channel did not become ready."
                        .to_string(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub(crate) fn semantic_action(
        &self,
        app: &AppHandle,
        device_id: &str,
        snapshot_id: &str,
        ref_id: &str,
        action: &str,
        text: Option<&str>,
    ) -> Result<PhoneSemanticActionReceipt, String> {
        let total_started = Instant::now();
        let action = action.trim().to_ascii_lowercase();
        if !matches!(action.as_str(), "click" | "type" | "set_text") {
            return Err(
                "INVALID_ARGUMENT: Phone semantic action must be click, type, or set_text."
                    .to_string(),
            );
        }
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        require_capability(app, device_id, PhoneCapability::ViewScreen)?;
        let policy_target = self.fast_target_for_action(device_id)?;
        self.ensure_foreground_ai_ui_access_allowed(&policy_target)?;

        let original_target = Self::semantic_ref_target(device_id, snapshot_id, ref_id)?;
        let wanted_action = if action == "click" { "click" } else { "type" };
        if !original_target
            .actions
            .iter()
            .any(|available| available.eq_ignore_ascii_case(wanted_action))
        {
            return Err(if wanted_action == "type" && original_target.sensitive {
                "SENSITIVE_FIELD: RepoTunnel blocks AI text entry into password, OTP, payment, and credential fields."
                    .to_string()
            } else if wanted_action == "type" {
                "NOT_EDITABLE: That semantic phone target does not accept text.".to_string()
            } else {
                "NOT_CLICKABLE: That semantic phone target does not advertise click.".to_string()
            });
        }
        if wanted_action == "type" && original_target.sensitive {
            return Err(
                "SENSITIVE_FIELD: RepoTunnel blocks AI text entry into password, OTP, payment, and credential fields."
                    .to_string(),
            );
        }

        let validated_text = if wanted_action == "type" {
            let text = text.ok_or_else(|| {
                "INVALID_ARGUMENT: text is required for phone semantic type/set_text.".to_string()
            })?;
            if text.is_empty() || text.chars().count() > 2_000 || text.chars().any(char::is_control)
            {
                return Err(
                    "INVALID_ARGUMENT: Phone semantic text must contain 1..2000 non-control characters."
                        .to_string(),
                );
            }
            Some(text)
        } else {
            None
        };

        // The accessibility helper keeps the last published target's window/path/signature
        // and re-resolves it against the live tree immediately before mutation. Do not take
        // another helper snapshot here: publishing that snapshot would replace the helper's
        // short-lived target map and make a valid ref stale by construction.
        if original_target.backend_id.starts_with("h:") {
            let target_runtime = self.fast_target_for_action(device_id)?;
            let (foreground_package, foreground_activity) =
                Self::foreground_component_on_target(&target_runtime)?.ok_or_else(|| {
                    "STALE_UI: RepoTunnel could not verify the foreground Android screen. No semantic action was sent."
                        .to_string()
                })?;
            let current_document_prefix = format!("{foreground_package}/{foreground_activity}:");
            if !original_target
                .document_identity
                .starts_with(&current_document_prefix)
            {
                return Err(
                    "STALE_UI: The foreground Android screen changed after this ref was observed. No semantic action was sent."
                        .to_string(),
                );
            }

            let helper_action = if wanted_action == "click" {
                "click"
            } else {
                "set_text"
            };
            let dispatch_started = Instant::now();
            let (device_accepted, helper_verified) = self.helper_semantic_action(
                &target_runtime,
                &original_target.backend_id,
                helper_action,
                validated_text,
            )?;
            let dispatch_ms =
                u64::try_from(dispatch_started.elapsed().as_millis()).unwrap_or(u64::MAX);

            let verification_started = Instant::now();
            thread::sleep(Duration::from_millis(if wanted_action == "click" {
                80
            } else {
                50
            }));
            let final_snapshot = self.semantic_snapshot(app, device_id, 800, None)?;
            let verification_ms =
                u64::try_from(verification_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let verified = if wanted_action == "type" {
                helper_verified
            } else {
                helper_verified && final_snapshot.snapshot.version > original_target.version
            };
            return Ok(PhoneSemanticActionReceipt {
                action: if wanted_action == "click" {
                    "click".to_string()
                } else {
                    "setText".to_string()
                },
                ref_id: ref_id.to_string(),
                dispatched: true,
                device_accepted,
                verified,
                final_ui_generation: final_snapshot.snapshot.version,
                dispatch_ms,
                verification_ms,
                total_ms: u64::try_from(total_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        // The uiautomator fallback has no helper-side live target revalidation, so refresh
        // before dispatch and reject the action if that snapshot/ref changed.
        let refreshed = self.semantic_snapshot(app, device_id, 800, None)?;
        let target =
            Self::semantic_ref_target(device_id, snapshot_id, ref_id).map_err(|error| {
                if error.starts_with("STALE_REF:") {
                    format!("STALE_UI: {error}")
                } else {
                    error
                }
            })?;
        if refreshed.snapshot.snapshot_id != snapshot_id {
            return Err(
                "STALE_UI: The Android accessibility tree changed after this ref was observed. No semantic action was sent."
                    .to_string(),
            );
        }

        let target_runtime = self.fast_target_for_action(device_id)?;
        let (left, top, right, bottom) = Self::android_backend_bounds(&target.backend_id)?;
        let center_x = left + (right - left) / 2;
        let center_y = top + (bottom - top) / 2;

        if wanted_action == "click" {
            let command = format!("input tap {center_x} {center_y}");
            let dispatch_started = Instant::now();
            self.run_low_latency_control(app, device_id, &command, Duration::from_secs(2))?;
            let dispatch_ms =
                u64::try_from(dispatch_started.elapsed().as_millis()).unwrap_or(u64::MAX);

            let verification_started = Instant::now();
            thread::sleep(Duration::from_millis(80));
            let final_snapshot =
                self.semantic_snapshot(app, device_id, 800, Some(refreshed.snapshot.hash.clone()))?;
            let verification_ms =
                u64::try_from(verification_started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let verified = final_snapshot.snapshot.hash != refreshed.snapshot.hash
                || final_snapshot.package_name != refreshed.package_name
                || final_snapshot.activity_name != refreshed.activity_name;
            return Ok(PhoneSemanticActionReceipt {
                action: "click".to_string(),
                ref_id: ref_id.to_string(),
                dispatched: true,
                device_accepted: true,
                verified,
                final_ui_generation: final_snapshot.snapshot.version,
                dispatch_ms,
                verification_ms,
                total_ms: u64::try_from(total_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
        }

        let text = validated_text.ok_or_else(|| {
            "INVALID_ARGUMENT: text is required for phone semantic type/set_text.".to_string()
        })?;

        // Focus the revalidated target, then prove that Android reports a focused editable
        // node before any text is sent.
        let dispatch_started = Instant::now();
        let focus_command = format!("input tap {center_x} {center_y}");
        self.run_low_latency_control(app, device_id, &focus_command, Duration::from_secs(2))?;
        thread::sleep(Duration::from_millis(100));
        let focused_snapshot = self.semantic_snapshot(app, device_id, 800, None)?;
        let focused_editable = focused_snapshot.snapshot.nodes.iter().any(|node| {
            node.role == "textbox"
                && !node.sensitive
                && node
                    .states
                    .iter()
                    .any(|state| state.eq_ignore_ascii_case("focused"))
                && node
                    .states
                    .iter()
                    .any(|state| state.eq_ignore_ascii_case("editable"))
        });
        if !focused_editable {
            return Err(
                "NOT_EDITABLE: Android did not expose a focused editable field after targeting the semantic ref. No text was sent."
                    .to_string(),
            );
        }

        // Select the current contents so set_text is replacement semantics, then use the
        // already-bundled scrcpy control channel for UTF-8 clipboard paste.
        self.run_low_latency_control(
            app,
            device_id,
            "input keycombination KEYCODE_CTRL_LEFT KEYCODE_A",
            Duration::from_secs(2),
        )?;
        self.scrcpy_clipboard_paste(app, &target_runtime, text)?;
        let dispatch_ms = u64::try_from(dispatch_started.elapsed().as_millis()).unwrap_or(u64::MAX);

        let verification_started = Instant::now();
        thread::sleep(Duration::from_millis(120));
        let final_snapshot = self.semantic_snapshot(app, device_id, 800, None)?;
        let verification_ms =
            u64::try_from(verification_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let verified = final_snapshot.snapshot.nodes.iter().any(|node| {
            node.role == "textbox"
                && !node.sensitive
                && node
                    .states
                    .iter()
                    .any(|state| state.eq_ignore_ascii_case("focused"))
                && (node.value.as_deref() == Some(text) || node.text.as_deref() == Some(text))
        });
        if !verified {
            return Err(
                "TEXT_NOT_VERIFIED: Android accepted the Unicode text operation, but RepoTunnel could not verify the exact field value."
                    .to_string(),
            );
        }

        Ok(PhoneSemanticActionReceipt {
            action: "setText".to_string(),
            ref_id: ref_id.to_string(),
            dispatched: true,
            device_accepted: true,
            verified: true,
            final_ui_generation: final_snapshot.snapshot.version,
            dispatch_ms,
            verification_ms,
            total_ms: u64::try_from(total_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    fn wait_for_fast_condition(
        &self,
        app: &AppHandle,
        device_id: &str,
        target: &PhoneRuntimeTarget,
        condition: &PhoneFastWaitCondition,
    ) -> Result<(), String> {
        match condition {
            PhoneFastWaitCondition::ForegroundPackage {
                package_name,
                timeout_ms,
            } => {
                require_capability(app, device_id, PhoneCapability::AppControl)?;
                let package_name = package_name.trim();
                if !valid_android_package_name(package_name) {
                    return Err(
                        "INVALID_ARGUMENT: wait_until foreground_package is invalid.".to_string(),
                    );
                }
                if Self::wait_for_foreground_package(
                    target,
                    package_name,
                    Duration::from_millis(u64::from(*timeout_ms)),
                )? {
                    Ok(())
                } else {
                    Err(format!(
                        "CONDITION_TIMEOUT: Foreground package did not become {package_name} within {timeout_ms} ms."
                    ))
                }
            }
            PhoneFastWaitCondition::FrameChanged {
                baseline_frame_id,
                timeout_ms,
            } => {
                require_capability(app, device_id, PhoneCapability::ViewScreen)?;
                let started = Instant::now();
                loop {
                    if let Some(frame) = self.cached_live_frame_for(device_id, &target.serial)? {
                        if frame.frame_id != *baseline_frame_id {
                            return Ok(());
                        }
                    }
                    if started.elapsed() >= Duration::from_millis(u64::from(*timeout_ms)) {
                        return Err(format!(
                            "CONDITION_TIMEOUT: Phone frame did not change from frame {baseline_frame_id} within {timeout_ms} ms."
                        ));
                    }
                    thread::sleep(Duration::from_millis(15));
                }
            }
            PhoneFastWaitCondition::FrameStable {
                stable_count,
                interval_ms,
                timeout_ms,
            } => {
                require_capability(app, device_id, PhoneCapability::ViewScreen)?;
                let started = Instant::now();
                let mut last_digest: Option<[u8; 32]> = None;
                let mut consecutive = 0u32;
                loop {
                    if let Some(frame) = self.cached_live_frame_for(device_id, &target.serial)? {
                        let digest: [u8; 32] = Sha256::digest(&frame.bytes).into();
                        if last_digest.as_ref() == Some(&digest) {
                            consecutive = consecutive.saturating_add(1);
                        } else {
                            consecutive = 1;
                            last_digest = Some(digest);
                        }
                        if consecutive >= *stable_count {
                            return Ok(());
                        }
                    }
                    if started.elapsed() >= Duration::from_millis(u64::from(*timeout_ms)) {
                        return Err(format!(
                            "CONDITION_TIMEOUT: Phone frame did not remain stable for {stable_count} samples within {timeout_ms} ms."
                        ));
                    }
                    thread::sleep(Duration::from_millis(u64::from(*interval_ms)));
                }
            }
            PhoneFastWaitCondition::KeyboardVisible {
                visible,
                timeout_ms,
            } => {
                require_capability(app, device_id, PhoneCapability::ViewScreen)?;
                let started = Instant::now();
                loop {
                    if Self::keyboard_visible_on_target(target)? == Some(*visible) {
                        return Ok(());
                    }
                    if started.elapsed() >= Duration::from_millis(u64::from(*timeout_ms)) {
                        return Err(format!(
                            "CONDITION_TIMEOUT: Android keyboard visibility did not become {visible} within {timeout_ms} ms."
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    pub(crate) fn launch_app(
        &self,
        app: &AppHandle,
        device_id: &str,
        package_name: &str,
    ) -> Result<PhoneAppLaunchReceipt, String> {
        let total_started = Instant::now();
        let package_name = package_name.trim();
        if !valid_android_package_name(package_name) {
            return Err("INVALID_ARGUMENT: Android package name is invalid.".to_string());
        }
        require_capability(app, device_id, PhoneCapability::AppControl)?;
        require_capability(app, device_id, PhoneCapability::ViewScreen)?;
        let target = self.target_for_action(device_id)?;
        if !Self::package_exists_on_target(&target, package_name)? {
            return Err(format!(
                "PACKAGE_NOT_FOUND: Android package {package_name} is not installed."
            ));
        }

        let payment_sensitive = self
            .helper_payment_sensitive_package(&target, package_name)
            .ok()
            .flatten()
            .unwrap_or_else(|| Self::obvious_payment_sensitive_package(package_name));
        let payment_safe_mode = false;

        let baseline_frame_id = self
            .live_stream_frame(app, &target)?
            .map(|frame| frame.frame_id)
            .or_else(|| {
                self.cached_live_frame_for(device_id, &target.serial)
                    .ok()
                    .flatten()
                    .map(|frame| frame.frame_id)
            })
            .ok_or_else(|| {
                "UI_NOT_READY: RepoTunnel could not establish a live frame before launching the Android application."
                    .to_string()
            })?;

        let dispatch_started = Instant::now();
        let output = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "monkey",
                "-p",
                package_name,
                "-c",
                "android.intent.category.LAUNCHER",
                "1",
            ],
            Duration::from_secs(5),
        )?;
        let dispatch_ms = u64::try_from(dispatch_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not launch that Android application",
            ));
        }

        let foreground_started = Instant::now();
        let verified_foreground =
            Self::wait_for_foreground_package(&target, package_name, Duration::from_secs(4))?;
        let foreground_wait_ms =
            u64::try_from(foreground_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !verified_foreground {
            return Err(format!(
                "APP_NOT_FOREGROUND: Android accepted the launch for {package_name}, but the package did not become foreground before timeout."
            ));
        }

        // A single changed frame can still be a transition/black frame. Require two distinct
        // post-launch frames while the target package remains foreground.
        let ui_ready_started = Instant::now();
        let mut first_new_frame_id = None;
        let mut ui_ready = false;
        while ui_ready_started.elapsed() < Duration::from_secs(2) {
            if let Some(frame) = self.cached_live_frame_for(device_id, &target.serial)? {
                if frame.frame_id > baseline_frame_id {
                    match first_new_frame_id {
                        None => first_new_frame_id = Some(frame.frame_id),
                        Some(first) if frame.frame_id > first => {
                            ui_ready = true;
                            break;
                        }
                        Some(_) => {}
                    }
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        let ui_ready_wait_ms =
            u64::try_from(ui_ready_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !ui_ready {
            return Err(format!(
                "UI_NOT_READY: {package_name} became foreground but RepoTunnel did not observe two post-launch live frames before timeout."
            ));
        }
        if !Self::wait_for_foreground_package(&target, package_name, Duration::from_millis(500))? {
            return Err(format!(
                "APP_NOT_FOREGROUND: {package_name} left the foreground while its UI was becoming ready."
            ));
        }

        Ok(PhoneAppLaunchReceipt {
            package_name: package_name.to_string(),
            payment_sensitive,
            payment_safe_mode,
            dispatched: true,
            device_accepted: true,
            verified_foreground: true,
            ui_ready: true,
            dispatch_ms,
            foreground_wait_ms,
            ui_ready_wait_ms,
            total_ms: u64::try_from(total_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }

    pub(crate) fn stop_app(
        &self,
        app: &AppHandle,
        device_id: &str,
        package_name: &str,
    ) -> Result<PhoneAppStopReceipt, String> {
        let package_name = package_name.trim();
        if !valid_android_package_name(package_name) {
            return Err("INVALID_ARGUMENT: Android package name is invalid.".to_string());
        }
        require_capability(app, device_id, PhoneCapability::AppControl)?;
        let target = self.target_for_action(device_id)?;
        if !Self::package_exists_on_target(&target, package_name)? {
            return Ok(PhoneAppStopReceipt {
                package_name: package_name.to_string(),
                existed: false,
                dispatched: false,
                device_accepted: false,
                verified_stopped: true,
            });
        }

        let output = run_bounded(
            &target.adb,
            &[
                "-s",
                &target.serial,
                "shell",
                "am",
                "force-stop",
                package_name,
            ],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not stop that Android application",
            ));
        }

        let started = Instant::now();
        let mut verified_stopped = false;
        while started.elapsed() < Duration::from_secs(2) {
            if !Self::package_running_on_target(&target, package_name)? {
                verified_stopped = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if !verified_stopped {
            return Err(format!(
                "POSTCONDITION_FAILED: Android accepted force-stop for {package_name}, but the package still has a running process."
            ));
        }

        Ok(PhoneAppStopReceipt {
            package_name: package_name.to_string(),
            existed: true,
            dispatched: true,
            device_accepted: true,
            verified_stopped: true,
        })
    }

    pub(crate) fn shell(
        &self,
        app: &AppHandle,
        device_id: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<PhoneShellResult, String> {
        const MAX_OUTPUT_BYTES: usize = 64 * 1024;
        let command = validate_phone_shell_command(command)?;
        require_capability(app, device_id, PhoneCapability::Shell)?;
        let target = self.target_for_action(device_id)?;
        self.ensure_foreground_ai_ui_access_allowed(&target)?;
        let output = self.run_authorized_shell_script(
            app,
            device_id,
            PhoneCapability::Shell,
            command,
            timeout,
        )?;
        let (stdout, stdout_truncated) = bounded_output_text(&output.stdout, MAX_OUTPUT_BYTES);
        let (stderr, stderr_truncated) = bounded_output_text(&output.stderr, MAX_OUTPUT_BYTES);
        Ok(PhoneShellResult {
            exit_code: output.status.code(),
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    pub(crate) fn logs(
        &self,
        app: &AppHandle,
        device_id: &str,
        max_lines: u32,
    ) -> Result<PhoneLogSnapshot, String> {
        const MAX_LOG_BYTES: usize = 256 * 1024;
        require_capability(app, device_id, PhoneCapability::Logs)?;
        let target = self.target_for_action(device_id)?;
        self.ensure_foreground_ai_ui_access_allowed(&target)?;
        if !(1..=1_000).contains(&max_lines) {
            return Err("Phone log snapshot max_lines must be between 1 and 1000.".to_string());
        }
        let max_lines_arg = max_lines.to_string();
        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::Logs,
            &["shell", "logcat", "-d", "-t", &max_lines_arg],
            Duration::from_secs(8),
        )?;
        if !output.status.success() {
            return Err("Could not read Android logs.".to_string());
        }
        let (text, truncated) = bounded_output_text(&output.stdout, MAX_LOG_BYTES);
        Ok(PhoneLogSnapshot {
            text,
            truncated,
            max_lines,
        })
    }

    pub(crate) fn settings_availability(
        &self,
        device_id: &str,
    ) -> Result<PhoneSettingsAvailability, String> {
        let (target, cached_write, cached_reason) = {
            let runtime = self
                .inner
                .lock()
                .map_err(|_| "Phone runtime state is unavailable.".to_string())?;
            let active = runtime
                .as_ref()
                .filter(|active| active.target.device_id == device_id)
                .ok_or_else(|| "The selected phone runtime is not active.".to_string())?;
            (
                active.target.clone(),
                active.settings_write_available,
                active.settings_write_reason.clone(),
            )
        };

        let detected = if cached_write.is_none() {
            let output = run_bounded(
                &target.adb,
                &[
                    "-s",
                    &target.serial,
                    "shell",
                    "cmd",
                    "package",
                    "check-permission",
                    "android.permission.WRITE_SECURE_SETTINGS",
                    "com.android.shell",
                    "0",
                ],
                Duration::from_secs(3),
            )?;
            if output.status.success() {
                let value = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .to_ascii_lowercase();
                if value == "-1" || value == "denied" {
                    Some(false)
                } else {
                    // A package-manager grant is not enough to prove that the concrete
                    // SettingsProvider mutation will succeed on every OEM build. Keep the
                    // effective write capability unknown until a real settings write succeeds.
                    None
                }
            } else {
                None
            }
        } else {
            cached_write
        };

        if cached_write.is_none() {
            if let Ok(mut runtime) = self.inner.lock() {
                if let Some(active) = runtime
                    .as_mut()
                    .filter(|active| active.target.device_id == device_id)
                {
                    active.settings_write_available = detected;
                    active.settings_write_reason = if detected == Some(false) {
                        Some(
                            "PERMISSION_DENIED: Android shell does not hold WRITE_SECURE_SETTINGS on this device."
                                .to_string(),
                        )
                    } else {
                        None
                    };
                }
            }
        }

        let reason = if detected == Some(false) {
            cached_reason.or_else(|| {
                Some(
                    "PERMISSION_DENIED: Android shell does not hold WRITE_SECURE_SETTINGS on this device."
                        .to_string(),
                )
            })
        } else if detected.is_none() && cached_write.is_none() {
            Some(
                "UNVERIFIED: Android settings write/delete availability is not claimed until an actual allowed settings mutation succeeds."
                    .to_string(),
            )
        } else {
            cached_reason
        };

        Ok(PhoneSettingsAvailability {
            read_available: true,
            write_available: detected,
            delete_available: detected,
            reason,
        })
    }

    fn record_settings_write_denial(&self, device_id: &str, reason: &str) {
        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                active.settings_write_available = Some(false);
                active.settings_write_reason = Some(reason.to_string());
            }
        }
    }

    fn record_settings_write_success(&self, device_id: &str) {
        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                active.settings_write_available = Some(true);
                active.settings_write_reason = None;
            }
        }
    }

    fn cached_settings_write_denial(&self, device_id: &str) -> Option<String> {
        self.inner.lock().ok().and_then(|runtime| {
            runtime
                .as_ref()
                .filter(|active| active.target.device_id == device_id)
                .and_then(|active| {
                    (active.settings_write_available == Some(false)).then(|| {
                        active.settings_write_reason.clone().unwrap_or_else(|| {
                            "PERMISSION_DENIED: Android settings writes are unavailable on this device."
                                .to_string()
                        })
                    })
                })
        })
    }

    pub(crate) fn setting_get(
        &self,
        app: &AppHandle,
        device_id: &str,
        namespace: PhoneSettingsNamespace,
        key: &str,
    ) -> Result<PhoneSettingRead, String> {
        const MAX_SETTING_OUTPUT_BYTES: usize = 16 * 1024;
        let key = key.trim();
        if !valid_setting_key(key) {
            return Err("Android settings key is invalid.".to_string());
        }
        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::DeviceSettings,
            &["shell", "settings", "get", namespace.as_str(), key],
            Duration::from_secs(3),
        )?;
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not read Android setting",
            ));
        }
        let (value, _) = bounded_output_text(&output.stdout, MAX_SETTING_OUTPUT_BYTES);
        let value = value.trim_end();
        if value == "null" {
            Ok(PhoneSettingRead {
                exists: false,
                value: None,
            })
        } else {
            Ok(PhoneSettingRead {
                exists: true,
                value: Some(value.to_string()),
            })
        }
    }

    pub(crate) fn setting_put(
        &self,
        app: &AppHandle,
        device_id: &str,
        namespace: PhoneSettingsNamespace,
        key: &str,
        value: &str,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::DeviceSettings)?;
        if let Some(reason) = self.cached_settings_write_denial(device_id) {
            return Err(reason);
        }
        let script = encoded_settings_put_script(namespace, key, value)?;
        let output = self.run_authorized_shell_script(
            app,
            device_id,
            PhoneCapability::DeviceSettings,
            &script,
            Duration::from_secs(3),
        )?;
        if output.status.success() {
            self.record_settings_write_success(device_id);
            Ok(())
        } else {
            let reason = android_failure_reason(&output, "Could not update Android setting");
            if reason.starts_with("PERMISSION_DENIED") {
                self.record_settings_write_denial(device_id, &reason);
            }
            Err(reason)
        }
    }

    pub(crate) fn setting_delete(
        &self,
        app: &AppHandle,
        device_id: &str,
        namespace: PhoneSettingsNamespace,
        key: &str,
    ) -> Result<(), String> {
        require_capability(app, device_id, PhoneCapability::DeviceSettings)?;
        if let Some(reason) = self.cached_settings_write_denial(device_id) {
            return Err(reason);
        }
        let key = key.trim();
        if !valid_setting_key(key) {
            return Err("Android settings key is invalid.".to_string());
        }
        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::DeviceSettings,
            &["shell", "settings", "delete", namespace.as_str(), key],
            Duration::from_secs(3),
        )?;
        if output.status.success() {
            self.record_settings_write_success(device_id);
            Ok(())
        } else {
            let reason = android_failure_reason(&output, "Could not delete Android setting");
            if reason.starts_with("PERMISSION_DENIED") {
                self.record_settings_write_denial(device_id, &reason);
            }
            Err(reason)
        }
    }

    pub(crate) fn list_files(
        &self,
        app: &AppHandle,
        device_id: &str,
        directory: &str,
    ) -> Result<PhoneFileList, String> {
        const MAX_LIST_BYTES: usize = 256 * 1024;
        require_capability(app, device_id, PhoneCapability::Files)?;
        let target = self.target_for_action(device_id)?;
        let directory = validate_android_path(directory)?;
        let encoded = encoded_android_path(directory)?;
        let script = format!(
            "p=$(printf '%s' '{encoded}' | toybox base64 -d) || exit 40; [ -d \"$p\" ] || exit 44; toybox find \"$p\" -mindepth 1 -maxdepth 1 -print0 | head -c {}",
            MAX_LIST_BYTES + 1
        );
        let output = run_bounded_adb_script(
            &target.adb,
            &target.serial,
            "exec-out",
            &script,
            Duration::from_secs(8),
        )?;
        if !output.status.success() {
            return Err(android_script_failure(
                &output,
                "Could not list that Android directory",
            ));
        }

        let was_capped = output.stdout.len() > MAX_LIST_BYTES;
        let mut bytes = if was_capped {
            output.stdout[..MAX_LIST_BYTES].to_vec()
        } else {
            output.stdout
        };
        if was_capped {
            if let Some(last_nul) = bytes.iter().rposition(|byte| *byte == 0) {
                bytes.truncate(last_nul + 1);
            } else {
                bytes.clear();
            }
        }
        Ok(parse_file_list(directory, &bytes, was_capped))
    }

    pub(crate) fn stat_file(
        &self,
        app: &AppHandle,
        device_id: &str,
        path: &str,
    ) -> Result<PhoneFileStat, String> {
        require_capability(app, device_id, PhoneCapability::Files)?;
        let target = self.target_for_action(device_id)?;
        let path = validate_android_path(path)?;
        let encoded = encoded_android_path(path)?;
        let script = format!(
            "p=$(printf '%s' '{encoded}' | toybox base64 -d) || exit 40; [ -e \"$p\" ] || [ -L \"$p\" ] || exit 44; toybox stat -c %F,%s,%Y \"$p\""
        );
        let output = run_bounded_adb_script(
            &target.adb,
            &target.serial,
            "exec-out",
            &script,
            Duration::from_secs(5),
        )?;
        if !output.status.success() {
            return Err(android_script_failure(
                &output,
                "Could not inspect that Android file path",
            ));
        }
        parse_file_stat(path, &output.stdout)
    }

    pub(crate) fn read_file(
        &self,
        app: &AppHandle,
        device_id: &str,
        path: &str,
    ) -> Result<PhoneFileRead, String> {
        const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
        require_capability(app, device_id, PhoneCapability::Files)?;
        let target = self.target_for_action(device_id)?;
        let path = validate_android_path(path)?;
        let encoded = encoded_android_path(path)?;
        let script = format!(
            "p=$(printf '%s' '{encoded}' | toybox base64 -d) || exit 40; [ -f \"$p\" ] || exit 44; head -c {} \"$p\"",
            MAX_FILE_BYTES + 1
        );
        let output = run_bounded_adb_script(
            &target.adb,
            &target.serial,
            "exec-out",
            &script,
            Duration::from_secs(15),
        )?;
        if !output.status.success() {
            return Err(android_script_failure(
                &output,
                "Could not read that Android file",
            ));
        }

        let truncated = output.stdout.len() > MAX_FILE_BYTES;
        let data = if truncated {
            &output.stdout[..MAX_FILE_BYTES]
        } else {
            &output.stdout
        };
        Ok(PhoneFileRead {
            path: path.to_string(),
            data_base64: BASE64_STANDARD.encode(data),
            size_bytes: u64::try_from(data.len()).unwrap_or(u64::MAX),
            truncated,
        })
    }

    pub(crate) fn write_file(
        &self,
        app: &AppHandle,
        device_id: &str,
        path: &str,
        data: &[u8],
    ) -> Result<PhoneFileWriteReceipt, String> {
        require_capability(app, device_id, PhoneCapability::Files)?;
        let path = validate_android_path(path)?;
        if path == "/" || path.ends_with('/') {
            return Err("Phone file write requires a file path, not a directory.".to_string());
        }
        let target = self.target_for_action(device_id)?;
        let staged = stage_phone_transfer_file(app, data)?;
        let local = staged.to_string_lossy().into_owned();
        let output = run_bounded(
            &target.adb,
            &["-s", &target.serial, "push", &local, path],
            Duration::from_secs(30),
        );
        let cleanup = fs::remove_file(&staged);

        let output = match (output, cleanup) {
            (Ok(output), Ok(())) => output,
            (Err(error), Ok(())) => return Err(error),
            (Ok(output), Err(cleanup_error)) if !output.status.success() => {
                return Err(format!(
                    "Could not write that Android file, and RepoTunnel could not remove its private staging file: {cleanup_error}"
                ))
            }
            (Ok(_), Err(cleanup_error)) => {
                return Err(format!(
                    "Android file was written, but RepoTunnel could not remove its private staging file: {cleanup_error}"
                ))
            }
            (Err(error), Err(cleanup_error)) => {
                return Err(format!(
                    "{error} RepoTunnel also could not remove its private staging file: {cleanup_error}"
                ))
            }
        };
        if !output.status.success() {
            return Err(android_failure_reason(
                &output,
                "Could not write that Android file",
            ));
        }

        Ok(PhoneFileWriteReceipt {
            path: path.to_string(),
            size_bytes: u64::try_from(data.len()).unwrap_or(u64::MAX),
        })
    }

    pub(crate) fn delete_file(
        &self,
        app: &AppHandle,
        device_id: &str,
        path: &str,
    ) -> Result<PhoneFileDeleteReceipt, String> {
        require_capability(app, device_id, PhoneCapability::Files)?;
        let target = self.target_for_action(device_id)?;
        let path = validate_android_path(path)?;
        if path == "/" {
            return Err("RepoTunnel will not delete the Android root path.".to_string());
        }
        let encoded = encoded_android_path(path)?;
        let script = format!(
            "p=$(printf '%s' '{encoded}' | toybox base64 -d) || exit 40; [ -f \"$p\" ] || [ -L \"$p\" ] || exit 44; rm -f -- \"$p\""
        );
        let output = run_bounded_adb_script(
            &target.adb,
            &target.serial,
            "shell",
            &script,
            Duration::from_secs(5),
        )?;
        if output.status.success() {
            Ok(PhoneFileDeleteReceipt {
                path: path.to_string(),
                existed: true,
                deleted: true,
            })
        } else if output.status.code() == Some(44) {
            Ok(PhoneFileDeleteReceipt {
                path: path.to_string(),
                existed: false,
                deleted: false,
            })
        } else {
            Err(android_failure_reason(
                &output,
                "Could not delete that Android file",
            ))
        }
    }

    pub(crate) fn install_apk_from_device(
        &self,
        app: &AppHandle,
        device_id: &str,
        apk_path: &str,
    ) -> Result<PhonePackageMutationReceipt, String> {
        require_capability(app, device_id, PhoneCapability::AppInstall)?;
        let target = self.target_for_action(device_id)?;
        let apk_path = validate_android_path(apk_path)?;
        if !apk_path.to_ascii_lowercase().ends_with(".apk") {
            return Err("Phone app installation requires an .apk file path.".to_string());
        }

        let encoded = encoded_android_path(apk_path)?;
        let script = format!(
            "p=$(printf '%s' '{encoded}' | toybox base64 -d) || exit 40; [ -f \"$p\" ] || exit 44; pm install -r \"$p\""
        );
        let output = run_bounded_adb_script(
            &target.adb,
            &target.serial,
            "shell",
            &script,
            Duration::from_secs(90),
        )?;
        if !output.status.success()
            || !String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("success")
        {
            return Err(android_script_failure(
                &output,
                "Android app installation did not complete successfully",
            ));
        }

        Ok(PhonePackageMutationReceipt {
            package_name: None,
            source_path: Some(apk_path.to_string()),
            dispatched: true,
            device_accepted: true,
            verified: false,
        })
    }

    pub(crate) fn uninstall_app(
        &self,
        app: &AppHandle,
        device_id: &str,
        package_name: &str,
    ) -> Result<PhonePackageMutationReceipt, String> {
        require_capability(app, device_id, PhoneCapability::AppInstall)?;
        let package_name = package_name.trim();
        if !valid_android_package_name(package_name) {
            return Err("INVALID_ARGUMENT: Android package name is invalid.".to_string());
        }

        let target = self.target_for_action(device_id)?;
        if !Self::package_exists_on_target(&target, package_name)? {
            return Err(format!(
                "PACKAGE_NOT_FOUND: Android package {package_name} is not installed."
            ));
        }

        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::AppInstall,
            &["shell", "pm", "uninstall", package_name],
            Duration::from_secs(30),
        )?;
        if !output.status.success()
            || !String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("success")
        {
            return Err(android_failure_reason(
                &output,
                "Android app removal did not complete successfully",
            ));
        }
        if Self::package_exists_on_target(&target, package_name)? {
            return Err(format!(
                "POSTCONDITION_FAILED: Android reported uninstall success for {package_name}, but the package is still installed."
            ));
        }

        Ok(PhonePackageMutationReceipt {
            package_name: Some(package_name.to_string()),
            source_path: None,
            dispatched: true,
            device_accepted: true,
            verified: true,
        })
    }

    pub(crate) fn network_snapshot(
        &self,
        app: &AppHandle,
        device_id: &str,
    ) -> Result<PhoneNetworkSnapshot, String> {
        const MAX_SECTION_BYTES: usize = 64 * 1024;
        require_capability(app, device_id, PhoneCapability::NetworkTools)?;
        let target = self.target_for_action(device_id)?;

        let interfaces = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "ip", "addr", "show"],
            Duration::from_secs(5),
        )?;
        let routes = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "ip", "route", "show"],
            Duration::from_secs(5),
        )?;
        let properties = run_bounded(
            &target.adb,
            &["-s", &target.serial, "shell", "getprop"],
            Duration::from_secs(5),
        )?;

        if !interfaces.status.success() || !routes.status.success() || !properties.status.success()
        {
            return Err("Could not read Android network state.".to_string());
        }

        let (interfaces_text, interfaces_truncated) =
            bounded_output_text(&interfaces.stdout, MAX_SECTION_BYTES);
        let (routes_text, routes_truncated) =
            bounded_output_text(&routes.stdout, MAX_SECTION_BYTES);
        let property_truncated = properties.stdout.len() > MAX_SECTION_BYTES;
        let property_slice = if property_truncated {
            &properties.stdout[..MAX_SECTION_BYTES]
        } else {
            &properties.stdout
        };

        Ok(PhoneNetworkSnapshot {
            interfaces: interfaces_text,
            routes: routes_text,
            dns_properties: parse_dns_properties(property_slice),
            truncated: interfaces_truncated || routes_truncated || property_truncated,
        })
    }

    pub(crate) fn ping(
        &self,
        app: &AppHandle,
        device_id: &str,
        host: &str,
        count: u32,
    ) -> Result<PhonePingResult, String> {
        const MAX_PING_BYTES: usize = 64 * 1024;
        if !(1..=5).contains(&count) {
            return Err("Phone ping count must be between 1 and 5.".to_string());
        }
        let host = host.trim();
        if !valid_network_host(host) {
            return Err("Phone network host is invalid.".to_string());
        }

        let count_arg = count.to_string();
        let output = self.run_authorized_adb(
            app,
            device_id,
            PhoneCapability::NetworkTools,
            &["shell", "ping", "-c", &count_arg, "-W", "3", host],
            Duration::from_secs(20),
        )?;
        let mut combined = output.stdout;
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.extend_from_slice(b"\n");
            }
            combined.extend_from_slice(&output.stderr);
        }
        let (text, truncated) = bounded_output_text(&combined, MAX_PING_BYTES);
        Ok(PhonePingResult {
            host: host.to_string(),
            count,
            success: output.status.success(),
            output: text,
            truncated,
        })
    }

    pub(crate) fn sequence(
        &self,
        app: &AppHandle,
        device_id: &str,
        steps: &[PhoneControlSequenceStep],
    ) -> Result<PhoneSequenceReceipt, String> {
        require_capability(app, device_id, PhoneCapability::ControlInput)?;
        let target = self.fast_target_for_action(device_id)?;
        let (initial_width, initial_height) =
            self.refresh_input_dimensions(app, device_id, &target)?;
        let compiled = compile_control_sequence(steps, initial_width, initial_height)?;

        for (index, step) in steps.iter().enumerate() {
            require_capability(app, device_id, PhoneCapability::ControlInput)?;
            match *step {
                PhoneControlSequenceStep::Tap { x_ratio, y_ratio } => {
                    let (width, height) = self.refresh_input_dimensions(app, device_id, &target)?;
                    let x = normalized_coordinate(x_ratio, width)?;
                    let y = normalized_coordinate(y_ratio, height)?;
                    let command = format!("input tap {x} {y}");
                    self.run_low_latency_control(app, device_id, &command, Duration::from_secs(2))
                        .map_err(|error| {
                            contextual_phone_error(
                                &format!(
                                "Phone sequence stopped at step {} because tap did not complete",
                                index + 1
                            ),
                                error,
                            )
                        })?;
                }
                PhoneControlSequenceStep::Swipe(gesture) => {
                    let (width, height) = self.refresh_input_dimensions(app, device_id, &target)?;
                    let start_x = normalized_coordinate(gesture.start_x_ratio, width)?;
                    let start_y = normalized_coordinate(gesture.start_y_ratio, height)?;
                    let end_x = normalized_coordinate(gesture.end_x_ratio, width)?;
                    let end_y = normalized_coordinate(gesture.end_y_ratio, height)?;
                    let command = format!(
                        "input swipe {start_x} {start_y} {end_x} {end_y} {}",
                        gesture.duration_ms
                    );
                    self.run_low_latency_control(app, device_id, &command, Duration::from_secs(4))
                        .map_err(|error| {
                            contextual_phone_error(
                                &format!(
                                "Phone sequence stopped at step {} because swipe did not complete",
                                index + 1
                            ),
                                error,
                            )
                        })?;
                }
                PhoneControlSequenceStep::Wait { duration_ms } => {
                    let mut remaining = duration_ms;
                    while remaining > 0 {
                        require_capability(app, device_id, PhoneCapability::ControlInput)?;
                        let slice = remaining.min(100);
                        thread::sleep(Duration::from_millis(u64::from(slice)));
                        remaining -= slice;
                    }
                }
            }
        }

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime.as_mut() {
                if active.target.device_id == device_id {
                    active.last_used_at = now_millis();
                }
            }
        }

        Ok(PhoneSequenceReceipt {
            completed_steps: compiled.len(),
            total_steps: compiled.len(),
        })
    }

    pub(crate) fn fast_sequence(
        &self,
        app: &AppHandle,
        device_id: &str,
        steps: &[PhoneFastSequenceStep],
        return_screen: bool,
        wait_for_frame_change_ms: u32,
        settle_ms: u32,
    ) -> Result<PhoneFastSequenceResult, String> {
        const MAX_SEQUENCE_STEPS: usize = 64;
        const MAX_WAIT_MS_PER_STEP: u32 = 2_000;
        const MAX_TOTAL_WAIT_MS: u32 = 10_000;

        if steps.is_empty() || steps.len() > MAX_SEQUENCE_STEPS {
            return Err("Fast phone sequence must contain between 1 and 64 steps.".to_string());
        }

        #[derive(Debug)]
        enum FastCompiledStep {
            Command {
                capability: PhoneCapability,
                command: String,
                timeout: Duration,
                expected_stream_dimensions: Option<(u32, u32)>,
                expected_input_dimensions: Option<(u32, u32)>,
            },
            VerifiedText {
                text: String,
            },
            LaunchApp {
                package_name: String,
            },
            Wait {
                duration_ms: u32,
            },
            WaitUntil(PhoneFastWaitCondition),
        }

        let started = Instant::now();

        let mut needs_control_dimensions = false;
        let mut total_wait_ms = 0u32;
        let mut needs_control = false;
        let mut needs_app_control = false;
        let mut needs_view = return_screen;

        for step in steps {
            match step {
                PhoneFastSequenceStep::Tap { .. } | PhoneFastSequenceStep::Swipe(_) => {
                    needs_control_dimensions = true;
                    needs_control = true;
                }
                PhoneFastSequenceStep::Key(_) => {
                    needs_control = true;
                }
                PhoneFastSequenceStep::TypeText(_) => {
                    needs_control = true;
                    needs_view = true;
                }
                PhoneFastSequenceStep::LaunchApp(_) => {
                    needs_app_control = true;
                    needs_view = true;
                }
                PhoneFastSequenceStep::Wait { duration_ms } => {
                    if *duration_ms > MAX_WAIT_MS_PER_STEP {
                        return Err(
                            "Fast phone sequence wait steps may be at most 2000 ms.".to_string()
                        );
                    }
                    total_wait_ms = total_wait_ms.saturating_add(*duration_ms);
                    if total_wait_ms > MAX_TOTAL_WAIT_MS {
                        return Err(
                            "Fast phone sequence total fixed wait time may be at most 10000 ms."
                                .to_string(),
                        );
                    }
                }
                PhoneFastSequenceStep::WaitUntil(condition) => {
                    let timeout_ms = match condition {
                        PhoneFastWaitCondition::ForegroundPackage {
                            package_name,
                            timeout_ms,
                        } => {
                            if !valid_android_package_name(package_name) {
                                return Err(
                                    "INVALID_ARGUMENT: wait_until foreground_package is invalid."
                                        .to_string(),
                                );
                            }
                            needs_app_control = true;
                            *timeout_ms
                        }
                        PhoneFastWaitCondition::FrameChanged { timeout_ms, .. } => *timeout_ms,
                        PhoneFastWaitCondition::FrameStable {
                            stable_count,
                            interval_ms,
                            timeout_ms,
                        } => {
                            if !(2..=20).contains(stable_count) {
                                return Err(
                                    "Fast phone frame_stable count must be between 2 and 20."
                                        .to_string(),
                                );
                            }
                            if !(20..=500).contains(interval_ms) {
                                return Err(
                                    "Fast phone frame_stable interval must be between 20 and 500 ms."
                                        .to_string(),
                                );
                            }
                            *timeout_ms
                        }
                        PhoneFastWaitCondition::KeyboardVisible { timeout_ms, .. } => {
                            needs_view = true;
                            *timeout_ms
                        }
                    };
                    if !(50..=10_000).contains(&timeout_ms) {
                        return Err(
                            "Fast phone wait_until timeout must be between 50 and 10000 ms."
                                .to_string(),
                        );
                    }
                }
            }
        }

        if needs_control {
            require_capability(app, device_id, PhoneCapability::ControlInput)?;
        }
        if needs_app_control {
            require_capability(app, device_id, PhoneCapability::AppControl)?;
        }
        if needs_view {
            require_capability(app, device_id, PhoneCapability::ViewScreen)?;
        }

        let target = self.fast_target_for_action(device_id)?;

        let dimensions = if needs_control_dimensions {
            Some(self.input_dimensions(app, device_id)?)
        } else {
            None
        };
        let stream_dimensions_at_compile = self
            .cached_live_frame_for(device_id, &target.serial)
            .ok()
            .flatten()
            .map(|frame| (frame.width, frame.height))
            .or_else(|| {
                self.inner.lock().ok().and_then(|runtime| {
                    runtime
                        .as_ref()
                        .filter(|active| active.target.device_id == device_id)
                        .and_then(|active| active.stream_dimensions)
                })
            });

        let mut compiled = Vec::with_capacity(steps.len());
        for step in steps {
            match step {
                PhoneFastSequenceStep::Tap { x_ratio, y_ratio } => {
                    let (width, height) = dimensions
                        .ok_or_else(|| "Phone control dimensions are unavailable.".to_string())?;
                    let x = normalized_coordinate(*x_ratio, width)?;
                    let y = normalized_coordinate(*y_ratio, height)?;
                    compiled.push(FastCompiledStep::Command {
                        capability: PhoneCapability::ControlInput,
                        command: format!("input tap {x} {y}"),
                        timeout: Duration::from_secs(2),
                        expected_stream_dimensions: stream_dimensions_at_compile,
                        expected_input_dimensions: Some((width, height)),
                    });
                }
                PhoneFastSequenceStep::Swipe(gesture) => {
                    if !(50..=3_000).contains(&gesture.duration_ms) {
                        return Err(
                            "Phone swipe duration must be between 50 and 3000 ms.".to_string()
                        );
                    }
                    let (width, height) = dimensions
                        .ok_or_else(|| "Phone control dimensions are unavailable.".to_string())?;
                    let start_x = normalized_coordinate(gesture.start_x_ratio, width)?;
                    let start_y = normalized_coordinate(gesture.start_y_ratio, height)?;
                    let end_x = normalized_coordinate(gesture.end_x_ratio, width)?;
                    let end_y = normalized_coordinate(gesture.end_y_ratio, height)?;
                    compiled.push(FastCompiledStep::Command {
                        capability: PhoneCapability::ControlInput,
                        command: format!(
                            "input swipe {start_x} {start_y} {end_x} {end_y} {}",
                            gesture.duration_ms
                        ),
                        timeout: Duration::from_secs(4),
                        expected_stream_dimensions: stream_dimensions_at_compile,
                        expected_input_dimensions: Some((width, height)),
                    });
                }
                PhoneFastSequenceStep::Key(key) => {
                    compiled.push(FastCompiledStep::Command {
                        capability: PhoneCapability::ControlInput,
                        command: format!("input keyevent {}", key.android_keycode()),
                        timeout: Duration::from_secs(2),
                        expected_stream_dimensions: None,
                        expected_input_dimensions: None,
                    });
                }
                PhoneFastSequenceStep::TypeText(text) => {
                    if text.is_empty()
                        || text.chars().count() > 2_000
                        || text.chars().any(char::is_control)
                    {
                        return Err(
                            "INVALID_ARGUMENT: Fast phone text must contain 1..2000 non-control characters."
                                .to_string(),
                        );
                    }
                    compiled.push(FastCompiledStep::VerifiedText { text: text.clone() });
                }
                PhoneFastSequenceStep::LaunchApp(package_name) => {
                    let package_name = package_name.trim();
                    if !valid_android_package_name(package_name) {
                        return Err(
                            "INVALID_ARGUMENT: Android package name is invalid.".to_string()
                        );
                    }
                    if !Self::package_exists_on_target(&target, package_name)? {
                        return Err(format!(
                            "PACKAGE_NOT_FOUND: Android package {package_name} is not installed. Fast sequence performed no mutation."
                        ));
                    }
                    compiled.push(FastCompiledStep::LaunchApp {
                        package_name: package_name.to_string(),
                    });
                }
                PhoneFastSequenceStep::Wait { duration_ms } => {
                    compiled.push(FastCompiledStep::Wait {
                        duration_ms: *duration_ms,
                    });
                }
                PhoneFastSequenceStep::WaitUntil(condition) => {
                    compiled.push(FastCompiledStep::WaitUntil(condition.clone()));
                }
            }
        }

        let mut start_frame = None;
        if return_screen {
            start_frame = self.cached_live_frame_for(device_id, &target.serial)?;
            if start_frame.is_none() {
                start_frame = self.live_stream_frame(app, &target)?;
            }
        }
        let start_frame_captured_at = start_frame.as_ref().map(|frame| frame.captured_at);

        let total_steps = compiled.len();
        for (index, step) in compiled.iter().enumerate() {
            match step {
                FastCompiledStep::Command {
                    capability,
                    command,
                    timeout,
                    expected_stream_dimensions,
                    expected_input_dimensions,
                } => {
                    require_capability(app, device_id, *capability)?;
                    if let Some(expected_stream_dimensions) = expected_stream_dimensions {
                        if let Some(current) =
                            self.cached_live_frame_for(device_id, &target.serial)?
                        {
                            let current_stream_dimensions = (current.width, current.height);
                            if current_stream_dimensions != *expected_stream_dimensions {
                                return Err(format!(
                                    "STALE_DISPLAY: Phone stream geometry changed from {}x{} to {}x{} before fast sequence step {}. No coordinate action was sent.",
                                    expected_stream_dimensions.0,
                                    expected_stream_dimensions.1,
                                    current_stream_dimensions.0,
                                    current_stream_dimensions.1,
                                    index + 1
                                ));
                            }
                        }
                    }
                    if let Some(expected_input_dimensions) = expected_input_dimensions {
                        let current_input_dimensions =
                            self.refresh_input_dimensions(app, device_id, &target)?;
                        if current_input_dimensions != *expected_input_dimensions {
                            return Err(format!(
                                "STALE_DISPLAY: Physical input geometry changed from {}x{} to {}x{} before fast sequence step {}. No coordinate action was sent.",
                                expected_input_dimensions.0,
                                expected_input_dimensions.1,
                                current_input_dimensions.0,
                                current_input_dimensions.1,
                                index + 1
                            ));
                        }
                    }
                    if let Err(error) =
                        self.run_low_latency_control(app, device_id, command, *timeout)
                    {
                        return Err(contextual_phone_error(
                            &format!("Fast phone sequence stopped at step {}", index + 1),
                            error,
                        ));
                    }
                }
                FastCompiledStep::VerifiedText { text } => {
                    if let Err(error) = self.type_text_verified(app, device_id, text) {
                        return Err(contextual_phone_error(
                            &format!("Fast phone sequence stopped at step {}", index + 1),
                            error,
                        ));
                    }
                }
                FastCompiledStep::LaunchApp { package_name } => {
                    require_capability(app, device_id, PhoneCapability::AppControl)?;
                    if let Err(error) = self.launch_app(app, device_id, package_name) {
                        return Err(contextual_phone_error(
                            &format!("Fast phone sequence stopped at step {}", index + 1),
                            error,
                        ));
                    }
                }
                FastCompiledStep::Wait { duration_ms } => {
                    let mut remaining = *duration_ms;
                    while remaining > 0 {
                        if needs_control {
                            require_capability(app, device_id, PhoneCapability::ControlInput)?;
                        } else if needs_app_control {
                            require_capability(app, device_id, PhoneCapability::AppControl)?;
                        }
                        let slice = remaining.min(100);
                        thread::sleep(Duration::from_millis(u64::from(slice)));
                        remaining -= slice;
                    }
                }
                FastCompiledStep::WaitUntil(condition) => {
                    if let Err(error) =
                        self.wait_for_fast_condition(app, device_id, &target, condition)
                    {
                        return Err(contextual_phone_error(
                            &format!("Fast phone sequence stopped at step {}", index + 1),
                            error,
                        ));
                    }
                }
            }
        }

        if let Ok(mut runtime) = self.inner.lock() {
            if let Some(active) = runtime
                .as_mut()
                .filter(|active| active.target.device_id == device_id)
            {
                active.last_used_at = now_millis();
            }
        }

        let mut final_cached = None;
        let allow_final_screen = if return_screen {
            match self.ensure_foreground_ai_ui_access_allowed(&target) {
                Ok(()) => true,
                Err(error) if error.starts_with("PAYMENT_APP_BLOCKED:") => false,
                Err(error) => return Err(error),
            }
        } else {
            false
        };
        if allow_final_screen {
            let baseline = start_frame_captured_at.unwrap_or(0);
            let wait_ms = wait_for_frame_change_ms.min(1_500);
            let wait_started = Instant::now();

            loop {
                if let Some(current) = self.cached_live_frame_for(device_id, &target.serial)? {
                    let changed = current.captured_at > baseline;
                    final_cached = Some(current);
                    if changed || wait_ms == 0 {
                        break;
                    }
                }

                if wait_started.elapsed() >= Duration::from_millis(u64::from(wait_ms)) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }

            let settle_ms = settle_ms.min(500);
            if settle_ms > 0 {
                thread::sleep(Duration::from_millis(u64::from(settle_ms)));
                if let Some(current) = self.cached_live_frame_for(device_id, &target.serial)? {
                    final_cached = Some(current);
                }
            }

            if final_cached.is_none() {
                final_cached = self.live_stream_frame(app, &target)?;
            }
        }

        let final_frame_captured_at = final_cached.as_ref().map(|frame| frame.captured_at);
        let frame_changed = match (start_frame_captured_at, final_frame_captured_at) {
            (Some(start), Some(end)) => end > start,
            (None, Some(_)) => true,
            _ => false,
        };

        let frame = final_cached.map(Self::screen_frame_from_cached);
        Ok(PhoneFastSequenceResult {
            receipt: PhoneFastSequenceReceipt {
                completed_steps: total_steps,
                total_steps,
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                start_frame_captured_at,
                final_frame_captured_at,
                frame_changed,
            },
            frame,
        })
    }
}

pub(crate) fn select_device(
    app: &AppHandle,
    device_id: Option<String>,
) -> Result<PhoneAccessStatus, String> {
    if let Some(device_id) = device_id.as_deref() {
        let discovery = discover();
        if !discovery
            .devices
            .iter()
            .any(|device| device.id == device_id)
        {
            return Err("That phone is not currently available.".to_string());
        }
    }

    mutate_access_settings(app, move |settings| {
        if settings.selected_device_id != device_id {
            settings.selected_device_id = device_id;
            settings.mode = PhoneAccessMode::Off;
            settings.paused = false;
            settings.limited_capabilities.clear();
        }
        Ok(access_status_from(settings))
    })
}

pub(crate) fn set_access_mode(
    app: &AppHandle,
    device_id: String,
    mode: PhoneAccessMode,
    limited_capabilities: Vec<PhoneCapability>,
) -> Result<PhoneAccessStatus, String> {
    let discovery = discover();
    let available = discovery
        .devices
        .iter()
        .any(|device| device.id == device_id);
    let connected = discovery
        .devices
        .iter()
        .find(|device| device.id == device_id)
        .is_some_and(|device| device.state == "connected");

    if mode != PhoneAccessMode::Off && !available {
        return Err("That phone is not currently available.".to_string());
    }
    if mode != PhoneAccessMode::Off && !connected {
        return Err("The selected phone must be connected before enabling AI access.".to_string());
    }

    let normalized = normalize_limited_capabilities(&limited_capabilities);
    if mode == PhoneAccessMode::Limited && normalized.is_empty() {
        return Err("Choose at least one capability for Limited Access.".to_string());
    }

    mutate_access_settings(app, move |settings| {
        if mode == PhoneAccessMode::Off
            && settings.selected_device_id.as_deref() != Some(device_id.as_str())
            && !available
        {
            return Err("That phone is not currently selected or available.".to_string());
        }

        if let Some(selected_device_id) = settings.selected_device_id.as_deref() {
            if selected_device_id != device_id {
                return Err(
                    "Choose this phone explicitly before changing its AI access.".to_string(),
                );
            }
        } else {
            settings.selected_device_id = Some(device_id);
        }
        settings.mode = mode;
        settings.paused = false;
        settings.limited_capabilities = if mode == PhoneAccessMode::Limited {
            normalized
        } else {
            Vec::new()
        };

        Ok(access_status_from(settings))
    })
}

pub(crate) fn set_access_paused(
    app: &AppHandle,
    paused: bool,
) -> Result<PhoneAccessStatus, String> {
    mutate_access_settings(app, move |settings| {
        if settings.mode == PhoneAccessMode::Off && paused {
            return Err("Phone access is already off.".to_string());
        }
        settings.paused = if settings.mode == PhoneAccessMode::Off {
            false
        } else {
            paused
        };
        Ok(access_status_from(settings))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_jpeg(width: u16, height: u16, marker_byte: u8) -> Vec<u8> {
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0x00, 0x11, 0x08];
        jpeg.extend_from_slice(&height.to_be_bytes());
        jpeg.extend_from_slice(&width.to_be_bytes());
        jpeg.extend_from_slice(&[
            0x03,
            0x01,
            0x11,
            0x00,
            0x02,
            0x11,
            0x00,
            0x03,
            0x11,
            marker_byte,
            0xff,
            0xd9,
        ]);
        jpeg
    }

    #[test]
    fn parses_usb_wireless_and_authorization_states() {
        let output = "List of devices attached\nR58M123 device usb:1-2 product:p model:Pixel_8 device:husky transport_id:1\n192.168.1.8:37199 device product:p model:Pixel_8 device:husky transport_id:2\nR58M999 unauthorized usb:1-3 transport_id:3\nPLAINUSB device product:p model:Pixel_8 device:husky transport_id:4\n";

        let records = parse_adb_records(output);
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].transport, "usb");
        assert_eq!(records[0].model_hint.as_deref(), Some("Pixel 8"));
        assert_eq!(records[1].transport, "wireless");
        assert_eq!(records[2].state, "authorizationRequired");
        assert_eq!(records[3].transport, "usb");
    }

    #[test]
    fn parses_wireless_mdns_services_without_exposing_other_rows() {
        let output = "List of discovered mdns services\nadb-ABC-pair _adb-tls-pairing._tcp. 192.168.1.20:37123\nadb-ABC-connect _adb-tls-connect._tcp. 192.168.1.20:39177\nprinter _http._tcp. 192.168.1.30:80\n";

        let services = parse_mdns_services(output);
        assert_eq!(services.len(), 2);
        assert_eq!(services[0].name, "adb-ABC-pair");
        assert!(services[0].service_type.contains("_adb-tls-pairing._tcp"));
        assert_eq!(services[0].endpoint, "192.168.1.20:37123");
        assert_eq!(services[1].name, "adb-ABC-connect");
        assert!(services[1].service_type.contains("_adb-tls-connect._tcp"));
    }

    #[test]
    fn pairing_code_is_exactly_six_digits() {
        assert!(valid_pairing_code("123456"));
        assert!(!valid_pairing_code("12345"));
        assert!(!valid_pairing_code("1234567"));
        assert!(!valid_pairing_code("12a456"));
    }

    #[test]
    fn parses_android_getprop_output_by_property_name() {
        let output = "[Build.BRAND]: [MTK]\n[aaudio.mmap exclusive policy]: [2]\n[ro.product.manufacturer]: [realme]\n[ro.product.model]: [RMX2156]\n[ro.build.version.release]: [12]\n[ro.serialno]: [FYIBBMAQRKMFJNAE]\n[ro.boot.serialno]: [FYIBBMAQRKMFJNAE]\n";
        let properties = parse_properties(output);

        assert_eq!(properties.manufacturer.as_deref(), Some("realme"));
        assert_eq!(properties.model.as_deref(), Some("RMX2156"));
        assert_eq!(properties.android_version.as_deref(), Some("12"));
        assert_eq!(properties.serial.as_deref(), Some("FYIBBMAQRKMFJNAE"));
        assert_eq!(properties.boot_serial.as_deref(), Some("FYIBBMAQRKMFJNAE"));
    }

    #[test]
    fn merges_transports_for_same_physical_device() {
        let id = opaque_device_id("ABC123");
        let mut devices = vec![PhoneDeviceSummary {
            id: id.clone(),
            name: "Google Pixel 8 Pro".to_string(),
            manufacturer: Some("Google".to_string()),
            android_version: Some("16".to_string()),
            state: "connected".to_string(),
            transport: "usb".to_string(),
            available_transports: vec!["usb".to_string()],
        }];

        insert_or_merge(
            &mut devices,
            PhoneDeviceSummary {
                id,
                name: "Google Pixel 8 Pro".to_string(),
                manufacturer: Some("Google".to_string()),
                android_version: Some("16".to_string()),
                state: "connected".to_string(),
                transport: "wireless".to_string(),
                available_transports: vec!["wireless".to_string()],
            },
        );

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].transport, "usb");
        assert_eq!(devices[0].available_transports, vec!["usb", "wireless"]);
    }

    #[test]
    fn mdns_alias_recovers_the_embedded_physical_serial() {
        assert_eq!(
            mdns_embedded_serial("adb-FYIBBMAQRKMFJNAE-i0GTX0._adb-tls-connect._tcp").as_deref(),
            Some("FYIBBMAQRKMFJNAE")
        );
        assert_eq!(mdns_embedded_serial("192.168.1.38:34199"), None);
    }

    #[test]
    fn mdns_endpoint_hint_maps_back_to_the_same_physical_identity() {
        let record = AdbDeviceRecord {
            serial: "192.168.1.38:34199".to_string(),
            state: "offline".to_string(),
            transport: "wireless".to_string(),
            model_hint: Some("RMX2156".to_string()),
        };
        let identity = stable_identity(
            &record,
            &DeviceProperties::default(),
            Some("FYIBBMAQRKMFJNAE".to_string()),
        );
        assert_eq!(identity, "FYIBBMAQRKMFJNAE");
        assert_eq!(
            opaque_device_id(&identity),
            opaque_device_id("FYIBBMAQRKMFJNAE")
        );
    }

    #[test]
    fn offline_wireless_does_not_override_connected_usb_for_same_phone() {
        let id = opaque_device_id("FYIBBMAQRKMFJNAE");
        let mut devices = vec![PhoneDeviceSummary {
            id: id.clone(),
            name: "RMX2156".to_string(),
            manufacturer: Some("realme".to_string()),
            android_version: Some("12".to_string()),
            state: "connected".to_string(),
            transport: "usb".to_string(),
            available_transports: vec!["usb".to_string()],
        }];

        insert_or_merge(
            &mut devices,
            PhoneDeviceSummary {
                id,
                name: "RMX2156".to_string(),
                manufacturer: Some("realme".to_string()),
                android_version: Some("12".to_string()),
                state: "offline".to_string(),
                transport: "wireless".to_string(),
                available_transports: vec!["wireless".to_string()],
            },
        );

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].state, "connected");
        assert_eq!(devices[0].transport, "usb");
        assert_eq!(devices[0].available_transports, vec!["usb", "wireless"]);
    }

    #[test]
    fn usb_connection_suppresses_background_wireless_reconnect() {
        let connected_usb = PhoneDiscoveryStatus {
            adb_available: true,
            devices: vec![PhoneDeviceSummary {
                id: opaque_device_id("FYIBBMAQRKMFJNAE"),
                name: "RMX2156".to_string(),
                manufacturer: Some("realme".to_string()),
                android_version: Some("12".to_string()),
                state: "connected".to_string(),
                transport: "usb".to_string(),
                available_transports: vec!["usb".to_string()],
            }],
            recommended_device_id: None,
            message: None,
        };
        assert!(!should_attempt_wireless_reconnect(&connected_usb));

        let disconnected = PhoneDiscoveryStatus {
            adb_available: true,
            devices: Vec::new(),
            recommended_device_id: None,
            message: None,
        };
        assert!(should_attempt_wireless_reconnect(&disconnected));
    }

    #[test]
    fn wireless_pairing_is_only_complete_after_wireless_is_connected() {
        let status = PhoneDiscoveryStatus {
            adb_available: true,
            devices: vec![PhoneDeviceSummary {
                id: opaque_device_id("FYIBBMAQRKMFJNAE"),
                name: "RMX2156".to_string(),
                manufacturer: Some("realme".to_string()),
                android_version: Some("12".to_string()),
                state: "connected".to_string(),
                transport: "wireless".to_string(),
                available_transports: vec!["wireless".to_string(), "usb".to_string()],
            }],
            recommended_device_id: None,
            message: None,
        };
        assert!(has_connected_wireless_phone(&status));
    }

    #[test]
    fn opaque_id_does_not_expose_raw_serial() {
        let id = opaque_device_id("ABC123");
        assert!(id.starts_with("phone-"));
        assert!(!id.contains("ABC123"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_drains_large_stdout_while_waiting() {
        let output = run_bounded(
            Path::new("/bin/sh"),
            &["-c", "head -c 1048576 /dev/zero"],
            Duration::from_secs(2),
        )
        .expect("large stdout should not deadlock the bounded runner");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1_048_576);
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn phone_control_shell_payload_uses_a_real_command_terminating_newline() {
        let marker = "__REPOTUNNEL_CTL_TEST__";
        let payload = phone_control_shell_payload("input keyevent 3", marker);

        assert!(payload.ends_with('\n'));
        assert!(!payload.ends_with("\\n"));
        assert!(payload.contains(marker));
        assert!(payload.contains("input keyevent 3"));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_process_streams_input_without_blocking_output() {
        let input = vec![0x5a; 512 * 1024];
        let output = run_bounded_with_input(
            Path::new("/bin/cat"),
            &[],
            Some(&input),
            Duration::from_secs(2),
        )
        .expect("bounded runner should stream stdin and stdout concurrently");

        assert!(output.status.success());
        assert_eq!(output.stdout, input);
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn raw_screen_frame_parser_accepts_android_rgba_header() {
        let width = 2u32;
        let height = 3u32;
        let mut raw = Vec::new();
        raw.extend_from_slice(&width.to_le_bytes());
        raw.extend_from_slice(&height.to_le_bytes());
        raw.extend_from_slice(&1u32.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&[0x7f; 24]);

        let (parsed_width, parsed_height, rgba) =
            raw_screencap_rgba(&raw).expect("valid RGBA screencap");
        assert_eq!((parsed_width, parsed_height), (width, height));
        assert_eq!(rgba, &[0x7f; 24]);

        raw[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(raw_screencap_rgba(&raw).is_none());
    }

    #[test]
    fn screen_frame_parser_reads_png_dimensions() {
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&1080u32.to_be_bytes());
        png[20..24].copy_from_slice(&2400u32.to_be_bytes());

        assert_eq!(png_dimensions(&png), Some((1080, 2400)));
        png[0] = 0;
        assert_eq!(png_dimensions(&png), None);
    }

    #[test]
    fn live_stream_jpeg_parser_reads_dimensions() {
        let jpeg = fake_jpeg(720, 1600, 0);
        assert_eq!(jpeg_dimensions(&jpeg), Some((720, 1600)));

        let mut invalid = jpeg;
        invalid[0] = 0;
        assert_eq!(jpeg_dimensions(&invalid), None);
    }

    #[test]
    fn live_stream_mjpeg_cache_keeps_latest_complete_frame() {
        let first = fake_jpeg(720, 1600, 0);
        let second = fake_jpeg(1600, 720, 1);
        let mut buffer = b"decoder-noise".to_vec();
        buffer.extend_from_slice(&first);
        buffer.extend_from_slice(&second);

        let latest = Arc::new(Mutex::new(None));
        let mut next_frame_id = 0;
        consume_mjpeg_frames(&mut buffer, &latest, &mut next_frame_id);

        let cached = latest
            .lock()
            .expect("latest frame cache")
            .clone()
            .expect("cached JPEG frame");
        assert_eq!((cached.width, cached.height), (1600, 720));
        assert_eq!(cached.bytes, second);
        assert!(buffer.is_empty());
    }

    #[test]
    fn fast_target_reuses_an_active_live_stream_without_adb_probe() {
        let device_id = "phone-1234567890abcdef12345678".to_string();
        let serial = "wireless-test-target".to_string();
        let target = PhoneRuntimeTarget {
            device_id: device_id.clone(),
            name: "Test Phone".to_string(),
            serial: serial.clone(),
            transport: "wireless".to_string(),
            adb: PathBuf::from("/definitely/not/a/real/adb"),
        };
        let cached = CachedPhoneLiveFrame {
            bytes: fake_jpeg(720, 1600, 0),
            width: 720,
            height: 1600,
            captured_at: 42,
            frame_id: 7,
        };

        let state = PhoneRuntimeState {
            inner: Mutex::new(Some(ActivePhoneRuntime {
                target: target.clone(),
                session_started_at: 1,
                last_used_at: 2,
                physical_display_dimensions: Some((1080, 2400)),
                stream_dimensions: Some((720, 1600)),
                display_generation: 1,
                settings_write_available: None,
                settings_write_reason: None,
            })),
            live_stream: Mutex::new(Some(PhoneLiveStreamSession {
                device_id: device_id.clone(),
                serial: serial.clone(),
                started_at: 1,
                stop: Arc::new(AtomicBool::new(false)),
                alive: Arc::new(AtomicBool::new(true)),
                latest: Arc::new(Mutex::new(Some(cached))),
                control: Arc::new(Mutex::new(None)),
                last_error: Arc::new(Mutex::new(None)),
            })),
            control_shell: Mutex::new(None),
            semantic_helper: Mutex::new(None),
        };

        let selected = state
            .fast_target_for_action(&device_id)
            .expect("active live target should be reused");
        assert_eq!(selected.device_id, device_id);
        assert_eq!(selected.serial, serial);
        assert_eq!(selected.adb, target.adb);
    }

    #[test]
    fn phone_helper_empty_response_is_transport_unavailable_not_protocol_corruption() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind helper EOF listener");
        let address = listener.local_addr().expect("helper EOF address");
        let server = thread::spawn(move || {
            let (client, _) = listener.accept().expect("accept helper EOF client");
            let mut reader = BufReader::new(client);
            let mut request = String::new();
            reader
                .read_line(&mut request)
                .expect("read helper EOF request");
            assert!(request.contains("\"op\":\"ping\""));
        });

        let session = PhoneSemanticHelperSession {
            device_id: "phone-test".to_string(),
            serial: "serial-test".to_string(),
            adb: PathBuf::from("adb"),
            nonce: "a".repeat(64),
            local_port: address.port(),
        };
        let error =
            PhoneRuntimeState::phone_helper_request(&session, &serde_json::json!({"op": "ping"}))
                .expect_err("EOF before a helper response must be a transport failure");
        server.join().expect("join helper EOF server");

        assert!(error.starts_with("PHONE_HELPER_UNAVAILABLE:"));
        assert!(error.contains("closed the connection"));
        assert!(!error.contains("PHONE_HELPER_PROTOCOL_ERROR"));
    }

    #[test]
    fn local_phone_panel_control_stays_separate_from_mcp_ai_control() {
        let commands = include_str!("commands.rs");
        let mcp = include_str!("mcp_server.rs");

        assert!(commands.contains(".local_ui_screen_frame"));
        assert!(commands.contains("state.phone.local_ui_tap"));
        assert!(commands.contains("state.phone.local_ui_swipe"));
        assert!(commands.contains("state.phone.local_ui_key_event"));
        assert!(commands.contains("state.phone.local_ui_type_text"));

        assert!(!mcp.contains("local_ui_screen_frame"));
        assert!(!mcp.contains("local_ui_tap"));
        assert!(!mcp.contains("local_ui_swipe"));
        assert!(!mcp.contains("local_ui_key_event"));
        assert!(!mcp.contains("local_ui_type_text"));
        assert!(mcp.contains("state.phone.tap_guarded"));
        assert!(mcp.contains("state.phone.swipe_guarded"));
        assert!(mcp.contains("state.phone.key_event"));
        assert!(mcp.contains(".type_text_verified"));
    }

    #[test]
    fn phone_helper_response_waits_through_transient_would_block() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind helper test listener");
        let address = listener.local_addr().expect("helper test address");
        let server = thread::spawn(move || {
            let (mut client, _) = listener.accept().expect("accept helper test client");
            thread::sleep(Duration::from_millis(180));
            client
                .write_all(b"{\"ok\":true}\n")
                .expect("write delayed helper response");
        });

        let socket = TcpStream::connect(address).expect("connect helper test client");
        let started = Instant::now();
        let response = read_phone_helper_response_with_timeouts(
            socket,
            1024,
            Duration::from_millis(40),
            Duration::from_millis(600),
        )
        .expect("delayed helper response should survive transient WouldBlock");
        server.join().expect("join helper test server");

        assert!(started.elapsed() >= Duration::from_millis(150));
        assert_eq!(response, b"{\"ok\":true}\n");
    }

    #[test]
    fn phone_helper_response_timeout_is_bounded_and_not_raw_os_error_11() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind helper timeout listener");
        let address = listener.local_addr().expect("helper timeout address");
        let server = thread::spawn(move || {
            let (_client, _) = listener.accept().expect("accept helper timeout client");
            thread::sleep(Duration::from_millis(240));
        });

        let socket = TcpStream::connect(address).expect("connect helper timeout client");
        let error = read_phone_helper_response_with_timeouts(
            socket,
            1024,
            Duration::from_millis(30),
            Duration::from_millis(120),
        )
        .expect_err("silent helper should hit the bounded response deadline");
        server.join().expect("join helper timeout server");

        assert!(error.contains("did not respond within 120 ms"));
        assert!(!error.contains("os error 11"));
        assert!(!error.contains("Resource temporarily unavailable"));
    }

    #[test]
    fn bundled_live_stream_server_matches_pinned_sha256() {
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("phone")
            .join("scrcpy-server-v4.1");
        assert_eq!(
            file_sha256_hex(&server).expect("bundled scrcpy server hash"),
            SCRCPY_SERVER_SHA256
        );
    }

    #[test]
    fn bundled_phone_helper_matches_pinned_sha256() {
        let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("resources")
            .join("phone")
            .join("repotunnel-phone-helper.apk");
        assert_eq!(
            file_sha256_hex(&helper).expect("bundled Phone helper hash"),
            PHONE_HELPER_SHA256
        );
    }

    #[test]
    fn phone_helper_payment_privacy_stays_enabled_by_default() {
        let service = include_str!(
            "../resources/phone/helper-src/app/src/main/java/com/repotunnel/phonehelper/PhoneAccessibilityService.java"
        );
        let manifest =
            include_str!("../resources/phone/helper-src/app/src/main/AndroidManifest.xml");
        let strings =
            include_str!("../resources/phone/helper-src/app/src/main/res/values/strings.xml");
        let build_helper = include_str!("../resources/phone/helper-src/build-helper.sh");

        assert!(service.contains("queryIntentActivities"));
        assert!(service.contains("upi://pay"));
        assert!(service.contains("\"classify_payment_package\""));
        assert!(service.contains("\"foreground_policy\""));
        assert!(service.contains("paymentAppForeground"));
        assert!(service.contains("TYPE_WINDOW_STATE_CHANGED"));
        assert!(service.contains("paymentSensitive = isPaymentSensitivePackage(eventPackage)"));
        assert!(service.contains("paymentAppForeground.get() && paymentSensitive"));
        assert!(service.contains("ArrayBlockingQueue"));
        assert!(service.contains("requestExecutor.execute"));
        assert!(service.contains("semanticExecutor.execute"));
        assert!(service.contains("RepoTunnelPhoneRequestWorker"));
        assert!(service.contains("RepoTunnelPhoneSemanticWorker"));
        assert!(service.contains(".put(\"paymentSensitive\", paymentAppForeground.get())"));
        assert!(service.contains("\"pause_for_payment\""));
        assert!(service.contains("\"PAYMENT_APP_BLOCKED\""));
        assert!(service.contains("blockPaymentApp()"));
        assert!(service.contains("case \"pause_for_payment\":"));
        assert!(service.contains("enterPaymentSafeMode();"));
        assert!(service.contains("currentSnapshotId = null"));
        assert!(service.contains("currentTargets = new HashMap<>()"));
        assert!(service.contains("Looper.myLooper() == Looper.getMainLooper()"));
        assert!(service.contains("main.post(this::disableSelf)"));
        assert!(manifest.contains("android.intent.category.LAUNCHER"));
        assert!(manifest.contains("android:scheme=\"upi\""));
        assert!(strings.contains("blocks AI inspection and control"));
        assert!(build_helper.contains("REPOTUNNEL_PHONE_HELPER_STOREPASS_FILE"));
        assert!(build_helper.contains("KS_PASS_ARG=\"file:$STOREPASS_FILE\""));
    }

    #[test]
    fn obvious_payment_package_names_are_fail_safe() {
        assert!(PhoneRuntimeState::obvious_payment_sensitive_package(
            "com.phonepe.app"
        ));
        assert!(PhoneRuntimeState::obvious_payment_sensitive_package(
            "net.one97.paytm"
        ));
        assert!(PhoneRuntimeState::obvious_payment_sensitive_package(
            "com.example.mobilebanking"
        ));
        assert!(PhoneRuntimeState::obvious_payment_sensitive_package(
            "in.org.npci.upiapp"
        ));
        assert!(!PhoneRuntimeState::obvious_payment_sensitive_package(
            "com.android.settings"
        ));
    }

    #[test]
    fn fallback_semantic_snapshot_redacts_sensitive_values() {
        let xml = r#"<hierarchy rotation="0">
<node index="0" text="super-secret-value" resource-id="com.example:id/password" class="android.widget.EditText" package="com.example" content-desc="Password" checkable="false" checked="false" clickable="true" enabled="true" focusable="true" focused="true" scrollable="false" long-clickable="false" password="true" selected="false" bounds="[10,20][300,90]" />
</hierarchy>"#;
        let (nodes, truncated, rotation) =
            semantic_drafts_from_uiautomator(xml, 100).expect("fallback semantic nodes");
        assert!(!truncated);
        assert_eq!(rotation, Some(0));
        assert_eq!(nodes.len(), 1);
        let node = &nodes[0];
        assert!(node.sensitive);
        assert_eq!(node.name, "Sensitive field");
        assert!(node.description.is_empty());
        assert_eq!(node.text, None);
        assert_eq!(node.value, None);
        assert!(!node.actions.iter().any(|action| action == "type"));
    }

    #[test]
    fn display_dimensions_prefer_android_override_size() {
        let output = b"Physical size: 1440x3120\nOverride size: 1080x2340\n";
        assert_eq!(parse_display_dimensions(output), Some((1080, 2340)));
    }

    #[test]
    fn downscaled_stream_never_becomes_physical_input_geometry() {
        let physical = (1080, 2400);
        let portrait_stream = (720, 1600);
        let landscape_stream = (1600, 720);

        assert_eq!(
            oriented_input_dimensions(physical, Some(portrait_stream)),
            (1080, 2400)
        );
        assert_eq!(
            oriented_input_dimensions(physical, Some(landscape_stream)),
            (2400, 1080)
        );

        let mut active = ActivePhoneRuntime {
            target: PhoneRuntimeTarget {
                device_id: "phone-geometry-test".to_string(),
                name: "Geometry Test".to_string(),
                serial: "test-serial".to_string(),
                transport: "wireless".to_string(),
                adb: PathBuf::from("adb"),
            },
            session_started_at: 1,
            last_used_at: 1,
            physical_display_dimensions: Some(physical),
            stream_dimensions: None,
            display_generation: 1,
            settings_write_available: None,
            settings_write_reason: None,
        };

        record_stream_dimensions(&mut active, portrait_stream);
        assert_eq!(active.physical_display_dimensions, Some(physical));
        assert_eq!(active.stream_dimensions, Some(portrait_stream));

        record_stream_dimensions(&mut active, landscape_stream);
        assert_eq!(active.physical_display_dimensions, Some(physical));
        assert_eq!(active.stream_dimensions, Some(landscape_stream));
        assert_eq!(active.display_generation, 3);
    }

    #[test]
    fn downscaled_stream_uses_identical_physical_coordinate_math() {
        let physical = oriented_input_dimensions((1080, 2400), Some((720, 1600)));
        assert_eq!(physical, (1080, 2400));
        assert_eq!(normalized_coordinate(0.5, physical.0).unwrap(), 540);
        assert_eq!(normalized_coordinate(0.5, physical.1).unwrap(), 1200);

        let steps = vec![
            PhoneControlSequenceStep::Tap {
                x_ratio: 0.5,
                y_ratio: 0.5,
            },
            PhoneControlSequenceStep::Swipe(PhoneSwipeGesture {
                start_x_ratio: 0.5,
                start_y_ratio: 0.8,
                end_x_ratio: 0.5,
                end_y_ratio: 0.2,
                duration_ms: 180,
            }),
        ];
        let compiled = compile_control_sequence(&steps, physical.0, physical.1).unwrap();
        assert_eq!(
            compiled[0],
            CompiledPhoneControlStep::Tap { x: 540, y: 1200 }
        );
        assert_eq!(
            compiled[1],
            CompiledPhoneControlStep::Swipe {
                start_x: 540,
                start_y: 1919,
                end_x: 540,
                end_y: 480,
                duration_ms: 180,
            }
        );
    }

    #[test]
    fn normalized_touch_coordinates_are_bounded() {
        assert_eq!(normalized_coordinate(0.0, 1080).unwrap(), 0);
        assert_eq!(normalized_coordinate(1.0, 1080).unwrap(), 1079);
        assert!(normalized_coordinate(-0.01, 1080).is_err());
        assert!(normalized_coordinate(1.01, 1080).is_err());
        assert!(normalized_coordinate(f64::NAN, 1080).is_err());
    }

    #[test]
    fn phone_network_targets_and_dns_parsing_are_bounded() {
        assert!(valid_network_host("example.com"));
        assert!(valid_network_host("2001:db8::1"));
        assert!(!valid_network_host("example.com;id"));
        assert!(!valid_network_host(""));

        let dns = parse_dns_properties(
            b"[net.dns1]: [1.1.1.1]\n[net.dns2]: [8.8.8.8]\n[ro.product.model]: [Pixel]\n",
        );
        assert_eq!(dns.len(), 2);
        assert!(dns[0].contains("dns1"));
        assert!(dns[1].contains("dns2"));
    }

    #[test]
    fn phone_file_paths_are_absolute_bounded_and_encoded() {
        assert!(validate_android_path("/sdcard/Download/file.txt").is_ok());
        assert!(validate_android_path("sdcard/file.txt").is_err());
        assert!(validate_android_path("").is_err());
        assert!(validate_android_path(&format!("/{}", "x".repeat(4_096))).is_err());

        let dangerous = "/sdcard/hello $(touch nope); file.txt";
        let encoded = encoded_android_path(dangerous).unwrap();
        assert!(!encoded.contains("touch"));
        assert_eq!(
            BASE64_STANDARD.decode(encoded).unwrap(),
            dangerous.as_bytes()
        );
    }

    #[test]
    fn phone_file_list_and_stat_parsers_are_bounded_and_structured() {
        let listing = parse_file_list("/sdcard", b"/sdcard/Download\0/sdcard/DCIM\0", false);
        assert_eq!(
            listing.entries,
            vec!["/sdcard/Download".to_string(), "/sdcard/DCIM".to_string()]
        );
        assert!(!listing.truncated);

        for (kind, size) in [
            ("regular file", 0u64),
            ("regular file", 1),
            ("regular file", 37),
            ("regular file", 1_048_577),
            ("directory", 4096),
            ("symbolic link", 12),
        ] {
            let encoded = format!("{kind},{size},1700000000\n");
            let stat = parse_file_stat("/sdcard/Download/ユニコード.txt", encoded.as_bytes())
                .expect("structured stat");
            assert_eq!(stat.kind, kind);
            assert_eq!(stat.size_bytes, size);
            assert_eq!(stat.modified_unix_seconds, Some(1_700_000_000));
        }
    }

    #[test]
    fn phone_file_list_marks_entry_overflow_as_truncated() {
        let output = (0..513)
            .flat_map(|index| format!("/sdcard/{index}\0").into_bytes())
            .collect::<Vec<_>>();
        let listing = parse_file_list("/sdcard", &output, false);
        assert_eq!(listing.entries.len(), 512);
        assert!(listing.truncated);
    }

    #[test]
    fn remote_shell_command_preserves_one_script_argument() {
        assert_eq!(quote_remote_shell_argument("a'b"), "'a'\''b'");
        assert_eq!(
            remote_shell_command("echo one; echo two"),
            "sh -c 'echo one; echo two'"
        );
    }

    #[test]
    fn phone_shell_command_and_output_are_bounded() {
        assert!(validate_phone_shell_command("echo ok").is_ok());
        assert!(validate_phone_shell_command("dumpsys activity activities").is_ok());
        assert!(validate_phone_shell_command("").is_err());
        assert!(validate_phone_shell_command("bad\0command").is_err());
        assert!(validate_phone_shell_command(&"x".repeat(16 * 1024 + 1)).is_err());
        assert!(validate_phone_shell_command("input tap 100 100").is_err());
        assert!(validate_phone_shell_command("/system/bin/input keyevent HOME").is_err());
        assert!(validate_phone_shell_command("monkey -p com.phonepe.app 1").is_err());
        assert!(validate_phone_shell_command("uiautomator dump /sdcard/window.xml").is_err());
        assert!(validate_phone_shell_command("screencap -p").is_err());
        assert!(validate_phone_shell_command("am start -n com.example/.MainActivity").is_err());
        assert!(validate_phone_shell_command(
            "cmd activity start-activity com.example/.MainActivity"
        )
        .is_err());

        assert_eq!(
            contextual_phone_error(
                "Fast phone sequence stopped at step 1",
                "PAYMENT_APP_BLOCKED: payment UI is protected".to_string(),
            ),
            "PAYMENT_APP_BLOCKED: Fast phone sequence stopped at step 1: payment UI is protected"
        );

        let (text, truncated) = bounded_output_text(b"abcdef", 4);
        assert_eq!(text, "abcd");
        assert!(truncated);
    }

    #[test]
    fn phone_settings_encoding_never_embeds_raw_value_as_shell_syntax() {
        let value = "hello $(touch /tmp/nope) ; echo test";
        let script =
            encoded_settings_put_script(PhoneSettingsNamespace::Global, "demo_key", value).unwrap();

        assert!(!script.contains(value));
        assert!(!script.contains("touch /tmp/nope"));
        assert!(script.contains("settings put global demo_key"));
        assert!(script.contains("toybox base64 -d"));
        assert!(valid_setting_key("screen_brightness"));
        assert!(!valid_setting_key("screen brightness"));
    }

    #[test]
    fn phone_settings_namespace_mapping_is_fixed() {
        assert_eq!(PhoneSettingsNamespace::System.as_str(), "system");
        assert_eq!(PhoneSettingsNamespace::Secure.as_str(), "secure");
        assert_eq!(PhoneSettingsNamespace::Global.as_str(), "global");
    }

    #[test]
    fn phone_text_encoding_never_embeds_raw_text_as_shell_syntax() {
        let text = "hello $(touch /tmp/nope) ; echo test";
        let script = encoded_input_text_script(text).unwrap();

        assert!(!script.contains(text));
        assert!(!script.contains("touch /tmp/nope"));
        assert!(script.contains("toybox base64 -d"));
        assert!(script.contains("input text"));
    }

    #[test]
    fn phone_text_input_rejects_controls_and_unbounded_content() {
        assert!(encoded_input_text_script("").is_err());
        assert!(encoded_input_text_script("hello\nworld").is_err());
        assert!(encoded_input_text_script(&"x".repeat(2_001)).is_err());
        assert!(encoded_input_text_script("normal text").is_ok());
    }

    #[test]
    fn android_package_validation_is_strict_and_package_list_is_bounded() {
        assert!(valid_android_package_name("com.example.app"));
        assert!(valid_android_package_name("android.settings"));
        assert!(!valid_android_package_name("com.example.app;id"));
        assert!(!valid_android_package_name("../escape"));
        assert!(!valid_android_package_name(".bad"));

        let parsed = parse_package_list(
            b"package:com.example.one\npackage:com.example.two\npackage:com.example.one\nnoise\n",
        );
        assert_eq!(
            parsed.packages,
            vec!["com.example.one".to_string(), "com.example.two".to_string()]
        );
        assert!(!parsed.truncated);
    }

    #[test]
    fn phone_key_mapping_is_fixed_and_not_arbitrary() {
        assert_eq!(PhoneKey::Back.android_keycode(), "KEYCODE_BACK");
        assert_eq!(PhoneKey::Home.android_keycode(), "KEYCODE_HOME");
        assert_eq!(PhoneKey::DpadRight.android_keycode(), "KEYCODE_DPAD_RIGHT");
    }

    #[test]
    fn phone_sequence_preflight_compiles_normalized_steps_before_execution() {
        let steps = vec![
            PhoneControlSequenceStep::Tap {
                x_ratio: 0.5,
                y_ratio: 0.25,
            },
            PhoneControlSequenceStep::Wait { duration_ms: 120 },
            PhoneControlSequenceStep::Swipe(PhoneSwipeGesture {
                start_x_ratio: 0.2,
                start_y_ratio: 0.8,
                end_x_ratio: 0.2,
                end_y_ratio: 0.2,
                duration_ms: 300,
            }),
        ];

        let compiled = compile_control_sequence(&steps, 1080, 2400).unwrap();
        assert_eq!(compiled.len(), 3);
        assert_eq!(
            compiled[0],
            CompiledPhoneControlStep::Tap { x: 540, y: 600 }
        );
        assert_eq!(
            compiled[1],
            CompiledPhoneControlStep::Wait { duration_ms: 120 }
        );
        assert_eq!(
            compiled[2],
            CompiledPhoneControlStep::Swipe {
                start_x: 216,
                start_y: 1919,
                end_x: 216,
                end_y: 480,
                duration_ms: 300,
            }
        );
    }

    #[test]
    fn phone_sequence_preflight_rejects_unbounded_or_wait_only_work() {
        assert!(compile_control_sequence(&[], 1080, 2400).is_err());

        let wait_only = vec![PhoneControlSequenceStep::Wait { duration_ms: 50 }];
        assert!(compile_control_sequence(&wait_only, 1080, 2400).is_err());

        let too_long_wait = vec![
            PhoneControlSequenceStep::Tap {
                x_ratio: 0.5,
                y_ratio: 0.5,
            },
            PhoneControlSequenceStep::Wait { duration_ms: 2_001 },
        ];
        assert!(compile_control_sequence(&too_long_wait, 1080, 2400).is_err());

        let too_many = vec![
            PhoneControlSequenceStep::Tap {
                x_ratio: 0.5,
                y_ratio: 0.5,
            };
            65
        ];
        assert!(compile_control_sequence(&too_many, 1080, 2400).is_err());
    }

    #[test]
    fn runtime_target_prefers_usb_for_the_same_phone() {
        let targets = vec![
            PhoneRuntimeTarget {
                device_id: "phone-0123456789abcdef01234567".to_string(),
                name: "Pixel".to_string(),
                serial: "USB123".to_string(),
                transport: "usb".to_string(),
                adb: PathBuf::from("adb"),
            },
            PhoneRuntimeTarget {
                device_id: "phone-0123456789abcdef01234567".to_string(),
                name: "Pixel".to_string(),
                serial: "192.168.1.2:40123".to_string(),
                transport: "wireless".to_string(),
                adb: PathBuf::from("adb"),
            },
        ];

        let selected = choose_runtime_target(targets).expect("target");
        assert_eq!(selected.transport, "usb");
        assert_eq!(selected.serial, "USB123");
    }

    #[test]
    fn runtime_status_does_not_expose_raw_adb_target() {
        let runtime = ActivePhoneRuntime {
            target: PhoneRuntimeTarget {
                device_id: "phone-0123456789abcdef01234567".to_string(),
                name: "Pixel".to_string(),
                serial: "192.168.1.2:40123".to_string(),
                transport: "wireless".to_string(),
                adb: PathBuf::from("/private/adb"),
            },
            session_started_at: 10,
            last_used_at: 20,
            physical_display_dimensions: None,
            stream_dimensions: None,
            display_generation: 1,
            settings_write_available: None,
            settings_write_reason: None,
        };

        let serialized = serde_json::to_string(&runtime_status_from(Some(&runtime))).unwrap();
        assert!(serialized.contains("phone-0123456789abcdef01234567"));
        assert!(serialized.contains("wireless"));
        assert!(!serialized.contains("192.168.1.2"));
        assert!(!serialized.contains("/private/adb"));
    }

    #[test]
    fn access_policy_is_off_by_default_and_full_grants_every_capability() {
        let default = PhoneAccessSettings::default();
        assert_eq!(access_status_from(&default).granted_capabilities.len(), 0);

        let full = PhoneAccessSettings {
            schema_version: PHONE_ACCESS_SCHEMA_VERSION,
            selected_device_id: Some("phone-0123456789abcdef01234567".to_string()),
            mode: PhoneAccessMode::Full,
            paused: false,
            limited_capabilities: Vec::new(),
        };
        assert_eq!(
            access_status_from(&full).granted_capabilities,
            ALL_PHONE_CAPABILITIES
        );
    }

    #[test]
    fn access_never_transfers_to_a_different_phone_identity() {
        let settings = PhoneAccessSettings {
            schema_version: PHONE_ACCESS_SCHEMA_VERSION,
            selected_device_id: Some("phone-0123456789abcdef01234567".to_string()),
            mode: PhoneAccessMode::Full,
            paused: false,
            limited_capabilities: Vec::new(),
        };

        assert!(enforce_capability(
            &settings,
            "phone-0123456789abcdef01234567",
            PhoneCapability::Shell
        )
        .is_ok());
        assert!(enforce_capability(
            &settings,
            "phone-fedcba987654321001234567",
            PhoneCapability::Shell
        )
        .is_err());
    }

    #[test]
    fn disconnected_runtime_status_does_not_keep_a_device_target() {
        let status = runtime_status_from(None);
        assert!(!status.active);
        assert_eq!(status.device_id, None);
        assert_eq!(status.transport, None);
    }

    #[test]
    fn limited_access_grants_only_selected_capabilities_and_pause_denies_all() {
        let mut limited = PhoneAccessSettings {
            schema_version: PHONE_ACCESS_SCHEMA_VERSION,
            selected_device_id: Some("phone-0123456789abcdef01234567".to_string()),
            mode: PhoneAccessMode::Limited,
            paused: false,
            limited_capabilities: vec![
                PhoneCapability::Shell,
                PhoneCapability::ViewScreen,
                PhoneCapability::Shell,
            ],
        };
        limited.limited_capabilities =
            normalize_limited_capabilities(&limited.limited_capabilities);

        assert_eq!(
            access_status_from(&limited).granted_capabilities,
            vec![PhoneCapability::ViewScreen, PhoneCapability::Shell]
        );

        limited.paused = true;
        assert!(access_status_from(&limited).granted_capabilities.is_empty());
    }

    #[test]
    fn capability_broker_is_device_bound_and_fail_closed() {
        let device_id = "phone-0123456789abcdef01234567";
        let mut settings = PhoneAccessSettings {
            schema_version: PHONE_ACCESS_SCHEMA_VERSION,
            selected_device_id: Some(device_id.to_string()),
            mode: PhoneAccessMode::Off,
            paused: false,
            limited_capabilities: Vec::new(),
        };

        assert!(enforce_capability(&settings, device_id, PhoneCapability::ViewScreen).is_err());

        settings.mode = PhoneAccessMode::Full;
        assert!(enforce_capability(&settings, device_id, PhoneCapability::Shell).is_ok());
        assert!(enforce_capability(
            &settings,
            "phone-fedcba987654321001234567",
            PhoneCapability::Shell
        )
        .is_err());

        settings.mode = PhoneAccessMode::Limited;
        settings.limited_capabilities = vec![PhoneCapability::ViewScreen];
        assert!(enforce_capability(&settings, device_id, PhoneCapability::ViewScreen).is_ok());
        assert!(enforce_capability(&settings, device_id, PhoneCapability::Files).is_err());

        settings.paused = true;
        assert!(enforce_capability(&settings, device_id, PhoneCapability::ViewScreen).is_err());
    }

    #[test]
    fn malformed_access_settings_fail_closed_instead_of_becoming_full() {
        assert!(parse_access_settings("").is_err());
        assert!(parse_access_settings("{}").is_err());
        assert!(parse_access_settings(
            r#"{"schemaVersion":1,"selectedDeviceId":"phone-bad","mode":"full","paused":false,"limitedCapabilities":[]}"#
        )
        .is_err());
        assert!(parse_access_settings(
            r#"{"schemaVersion":99,"selectedDeviceId":null,"mode":"full","paused":false,"limitedCapabilities":[]}"#
        )
        .is_err());
    }
}
