use std::{path::Path, process::Command};

#[cfg(any(target_os = "linux", test))]
use std::fs;

use crate::models::{SystemResourceSnapshot, Workspace};

#[cfg(target_os = "linux")]
#[allow(clippy::needless_return)]
fn parse_linux_meminfo() -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let Ok(text) = fs::read_to_string("/proc/meminfo") else {
            return (None, None);
        };
        let mut total = None;
        let mut available = None;
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let key = parts.next().unwrap_or_default();
            let value = parts.next().and_then(|value| value.parse::<u64>().ok());
            let bytes = value.and_then(|value| value.checked_mul(1024));
            match key {
                "MemTotal:" => total = bytes,
                "MemAvailable:" => available = bytes,
                _ => {}
            }
        }
        return (total, available);
    }
    #[cfg(not(target_os = "linux"))]
    {
        (None, None)
    }
}

// Windows provides both total and available physical memory through one
// bounded kernel call; unlike a shell probe it works in packaged desktop apps
// whose PATH may not contain system-management utilities.
#[cfg(windows)]
fn windows_physical_memory() -> (Option<u64>, Option<u64>) {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 || status.total_phys == 0 {
        return (None, None);
    }
    (
        Some(status.total_phys),
        Some(status.avail_phys.min(status.total_phys)),
    )
}

// Host VM counters include reclaimable inactive and speculative pages. These
// are an availability estimate, not an assertion about memory_pressure output.
#[cfg(target_os = "macos")]
fn macos_physical_memory() -> (Option<u64>, Option<u64>) {
    use std::ffi::CString;
    let mut total: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let name = CString::new("hw.memsize").expect("static sysctl name");
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&mut total as *mut u64).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || total == 0
        || size != std::mem::size_of::<u64>()
    {
        return (None, None);
    }

    #[link(name = "System")]
    extern "C" {
        static mach_task_self_: u32;
        fn mach_host_self() -> u32;
        fn mach_port_deallocate(task: u32, name: u32) -> i32;
        fn host_statistics64(host: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
    }
    // Darwin vm_statistics64_data_t occupies 38 natural_t words:
    // the first four are 32-bit counters, followed by nine 64-bit counters
    // (18 words), then purgeable_count at 22 and speculative_count at 23.
    // Request the complete layout; never read a partially populated buffer.
    const HOST_VM_INFO64: i32 = 4;
    const VM_STATS_WORDS: usize = 38;
    const SPECULATIVE_COUNT_WORD: usize = 23;
    let mut counters = [0_u32; VM_STATS_WORDS];
    let mut count = VM_STATS_WORDS as u32;
    let host = unsafe { mach_host_self() };
    let ok = unsafe {
        host_statistics64(
            host,
            HOST_VM_INFO64,
            counters.as_mut_ptr().cast(),
            &mut count,
        ) == 0
    };
    // mach_host_self grants a send right; release it on both success and error
    // so repeated dashboard refreshes cannot leak host-port references.
    unsafe {
        let _ = mach_port_deallocate(mach_task_self_, host);
    }
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if !ok || count != VM_STATS_WORDS as u32 || page_size <= 0 {
        return (Some(total), None);
    }
    let pages = u64::from(counters[0])
        .saturating_add(u64::from(counters[2]))
        .saturating_add(u64::from(counters[SPECULATIVE_COUNT_WORD]));
    let available = pages.saturating_mul(page_size as u64).min(total);
    (Some(total), Some(available))
}

fn physical_memory() -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        parse_linux_meminfo()
    }
    #[cfg(target_os = "macos")]
    {
        macos_physical_memory()
    }
    #[cfg(windows)]
    {
        windows_physical_memory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        (None, None)
    }
}

#[allow(clippy::needless_return)]
fn load_average_1m() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        return fs::read_to_string("/proc/loadavg")
            .ok()?
            .split_whitespace()
            .next()?
            .parse::<f64>()
            .ok();
    }
    #[cfg(target_os = "macos")]
    {
        let mut averages = [0.0_f64; 1];
        if unsafe { libc::getloadavg(averages.as_mut_ptr(), 1) } == 1 {
            return averages[0].is_finite().then_some(averages[0]);
        }
        return None;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn disk_usage(path: &Path) -> (Option<u64>, Option<u64>) {
    let output = Command::new("df")
        .args(["-Pk", "--"])
        .arg(path)
        .output()
        .ok();
    let Some(output) = output.filter(|output| output.status.success()) else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().skip(1).find(|line| !line.trim().is_empty()) else {
        return (None, None);
    };
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 6 {
        return (None, None);
    }
    let total = fields
        .get(fields.len().saturating_sub(5))
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1024));
    let available = fields
        .get(fields.len().saturating_sub(3))
        .and_then(|value| value.parse::<u64>().ok())
        .and_then(|value| value.checked_mul(1024));
    (total, available)
}

#[cfg(target_os = "macos")]
fn disk_usage(path: &Path) -> (Option<u64>, Option<u64>) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return (None, None);
    };
    let mut status = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), status.as_mut_ptr()) } != 0 {
        return (None, None);
    }
    let status = unsafe { status.assume_init() };
    let block_size = status.f_frsize as u64;
    let total = (status.f_blocks as u64).saturating_mul(block_size);
    let available = (status.f_bavail as u64).saturating_mul(block_size);
    if total == 0 {
        (None, None)
    } else {
        (Some(total), Some(available.min(total)))
    }
}

#[cfg(windows)]
fn disk_usage(path: &Path) -> (Option<u64>, Option<u64>) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory: *const u16,
            available_to_caller: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }
    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<u16>>();
    let (mut available, mut total, mut free) = (0_u64, 0_u64, 0_u64);
    let result =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut free) };
    if result == 0 || total == 0 {
        (None, None)
    } else {
        (Some(total), Some(available.min(total)))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn disk_usage(_path: &Path) -> (Option<u64>, Option<u64>) {
    (None, None)
}

fn push_unique(devices: &mut Vec<String>, value: String) {
    let value = value.trim().to_string();
    if !value.is_empty() && !devices.iter().any(|existing| existing == &value) {
        devices.push(value);
    }
}

// A remote/virtual display adapter or a basic software renderer alone is not
// evidence of a hardware GPU that can accelerate local workloads.
#[cfg(any(windows, target_os = "macos", test))]
fn native_gpu_adapter_name(value: &str) -> Option<String> {
    let name = value.trim();
    let lower = name.to_ascii_lowercase();
    if name.is_empty()
        || lower == "display"
        || lower == "graphics"
        || lower.contains("basic display")
        || lower.contains("basic render")
        || lower.contains("remote display")
        || lower.contains("virtual display")
        || lower.contains("mirror driver")
        || lower.contains("software renderer")
        || lower.contains("rdp display")
    {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(windows)]
fn windows_gpu_adapters() -> Vec<String> {
    #[repr(C)]
    struct DisplayDeviceW {
        cb: u32,
        device_name: [u16; 32],
        device_string: [u16; 128],
        state_flags: u32,
        device_id: [u16; 128],
        device_key: [u16; 128],
    }
    #[link(name = "user32")]
    extern "system" {
        fn EnumDisplayDevicesW(
            device: *const u16,
            device_num: u32,
            display_device: *mut DisplayDeviceW,
            flags: u32,
        ) -> i32;
    }
    const DISPLAY_DEVICE_MIRRORING_DRIVER: u32 = 0x0000_0008;
    let mut adapters = Vec::new();
    for index in 0..32u32 {
        // cb must be reset before *every* Windows enumeration call.
        let mut display: DisplayDeviceW = unsafe { std::mem::zeroed() };
        display.cb = std::mem::size_of::<DisplayDeviceW>() as u32;
        let ok = unsafe { EnumDisplayDevicesW(std::ptr::null(), index, &mut display, 0) };
        if ok == 0 {
            break;
        }
        if display.state_flags & DISPLAY_DEVICE_MIRRORING_DRIVER != 0 {
            continue;
        }
        let length = display
            .device_string
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(display.device_string.len());
        let raw = String::from_utf16_lossy(&display.device_string[..length]);
        if let Some(name) = native_gpu_adapter_name(&raw) {
            push_unique(&mut adapters, name);
        }
        if adapters.len() >= 16 {
            break;
        }
    }
    adapters
}

#[cfg(any(target_os = "macos", test))]
fn parse_macos_gpu_adapters_json(bytes: &[u8]) -> Vec<String> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    let Some(adapters) = value
        .get("SPDisplaysDataType")
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };
    let mut devices = Vec::new();
    for adapter in adapters.iter().take(32) {
        let name = ["sppci_model", "spdisplays_chipset-model"]
            .iter()
            .filter_map(|key| adapter.get(*key).and_then(|value| value.as_str()))
            .find_map(native_gpu_adapter_name);
        if let Some(name) = name {
            push_unique(&mut devices, name);
        }
        if devices.len() >= 16 {
            break;
        }
    }
    devices
}

// GPU telemetry must not hang a workspace status refresh if an optional OS
// utility stalls. An anonymous temporary file drains stdout without a pipe
// deadlock, and the parsed output is capped before it enters app state.
fn bounded_gpu_probe(program: &Path, args: &[&str], seconds: u64) -> Option<Vec<u8>> {
    use std::{
        io::{Read, Seek, SeekFrom},
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    let mut output_file = tempfile::tempfile().ok()?;
    let stdout = output_file.try_clone().ok()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let successful = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => break false,
        }
    };
    if !successful {
        let _ = child.kill();
    }
    let _ = child.wait();
    if !successful {
        return None;
    }
    output_file.seek(SeekFrom::Start(0)).ok()?;
    let mut contents = Vec::new();
    output_file
        .take(128 * 1024 + 1)
        .read_to_end(&mut contents)
        .ok()?;
    (contents.len() <= 128 * 1024).then_some(contents)
}

#[cfg(target_os = "macos")]
fn macos_gpu_adapters() -> Vec<String> {
    use std::sync::OnceLock;
    // The hardware rarely changes during a session; cache the bounded
    // system_profiler query instead of repeating it on every status refresh.
    static ADAPTERS: OnceLock<Vec<String>> = OnceLock::new();
    ADAPTERS
        .get_or_init(|| {
            bounded_gpu_probe(
                Path::new("/usr/sbin/system_profiler"),
                &["SPDisplaysDataType", "-json"],
                5,
            )
            .map(|output| parse_macos_gpu_adapters_json(&output))
            .unwrap_or_default()
        })
        .clone()
}

fn gpu_devices() -> Vec<String> {
    let mut devices = Vec::new();

    if let Some(output) = bounded_gpu_probe(
        Path::new("nvidia-smi"),
        &["--query-gpu=name", "--format=csv,noheader"],
        5,
    ) {
        for line in String::from_utf8_lossy(&output).lines() {
            push_unique(&mut devices, line.to_string());
        }
    }

    #[cfg(windows)]
    for adapter in windows_gpu_adapters() {
        push_unique(&mut devices, adapter);
    }

    #[cfg(target_os = "macos")]
    for adapter in macos_gpu_adapters() {
        push_unique(&mut devices, adapter);
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(output) = bounded_gpu_probe(Path::new("lspci"), &[], 3) {
            for line in String::from_utf8_lossy(&output).lines() {
                let lowered = line.to_ascii_lowercase();
                if lowered.contains("vga compatible controller")
                    || lowered.contains("3d controller")
                    || lowered.contains("display controller")
                {
                    let name = line
                        .split_once(": ")
                        .map(|(_, value)| value)
                        .unwrap_or(line);
                    push_unique(&mut devices, name.to_string());
                }
            }
        }
        if devices.is_empty() {
            if let Ok(entries) = fs::read_dir("/dev/dri") {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with("renderD") {
                        push_unique(&mut devices, format!("DRM render device {name}"));
                    }
                }
            }
        }
    }

    devices.truncate(16);
    devices
}

pub(crate) fn snapshot(workspace: &Workspace) -> SystemResourceSnapshot {
    let (memory_total_bytes, memory_available_bytes) = physical_memory();
    let (workspace_disk_total_bytes, workspace_disk_available_bytes) =
        disk_usage(Path::new(&workspace.path));
    let gpu_devices = gpu_devices();

    let mut notes = Vec::new();
    #[cfg(target_os = "macos")]
    if memory_available_bytes.is_some() {
        notes.push(
            "Available macOS RAM is estimated from free, inactive, and speculative VM pages; the OS can reclaim or compress additional memory."
                .to_string(),
        );
    }
    if memory_total_bytes.is_none() || memory_available_bytes.is_none() {
        notes.push(
            "RAM totals are unavailable on this platform/build; RepoTunnel does not guess."
                .to_string(),
        );
    }
    if workspace_disk_available_bytes.is_none() {
        notes.push(
            "Workspace disk capacity is unavailable on this platform/build; RepoTunnel does not guess."
                .to_string(),
        );
    }
    if gpu_devices.is_empty() {
        notes.push(
            "No GPU device was positively identified. This does not prove that software rendering is unavailable."
                .to_string(),
        );
    }

    SystemResourceSnapshot {
        platform: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        logical_cpu_count: std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(1),
        load_average_1m: load_average_1m(),
        memory_total_bytes,
        memory_available_bytes,
        workspace_disk_total_bytes,
        workspace_disk_available_bytes,
        gpu_available: !gpu_devices.is_empty(),
        gpu_devices,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CommandPolicy, WorkspaceAccessMode, WorkspaceChangePolicy};

    // These native smoke tests run on the existing Windows/macOS CI runners.
    // A failed platform API call must be caught before a release, not hidden
    // behind a successful Linux-only unit test.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn native_memory_and_disk_probes_report_bounded_values() {
        let (memory_total, memory_available) = physical_memory();
        let total = memory_total.expect("native memory total");
        let available = memory_available.expect("native memory available");
        assert!(total > 0);
        assert!(available <= total);

        let (disk_total, disk_available) = disk_usage(&std::env::temp_dir());
        let total = disk_total.expect("native filesystem capacity");
        let available = disk_available.expect("native filesystem free space");
        assert!(total > 0);
        assert!(available <= total);

        #[cfg(target_os = "macos")]
        {
            let load = load_average_1m().expect("native macOS system load");
            assert!(load.is_finite() && load >= 0.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn gpu_command_probe_caps_runtime_output_and_reaps_stalled_children() {
        use std::time::{Duration, Instant};

        assert_eq!(
            bounded_gpu_probe(Path::new("/bin/echo"), &["Apple M4 Pro"], 2),
            Some(b"Apple M4 Pro\n".to_vec())
        );
        assert!(bounded_gpu_probe(
            Path::new("/definitely/missing/repotunnel-gpu-probe"),
            &[],
            1,
        )
        .is_none());
        let start = Instant::now();
        assert!(bounded_gpu_probe(Path::new("/bin/sleep"), &["10"], 0).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "unresponsive GPU probes must not hang resource telemetry"
        );
        let oversized =
            bounded_gpu_probe(Path::new("/bin/sh"), &["-c", "head -c 131073 /dev/zero"], 3);
        assert!(
            oversized.is_none(),
            "oversized native probe output must fail closed"
        );
    }

    #[test]
    fn native_gpu_names_exclude_generic_remote_and_software_devices() {
        for name in [
            "",
            "Display",
            "Microsoft Basic Display Adapter",
            "Microsoft Basic Render Driver",
            "Microsoft Remote Display Adapter",
            "Virtual Display Adapter",
            "Mirror Driver",
            "Software Renderer",
            "RDP Display Driver",
        ] {
            assert!(
                native_gpu_adapter_name(name).is_none(),
                "a generic/remote renderer is not proof of local GPU hardware: {name}"
            );
        }
        for name in [
            "Apple M4 Pro",
            "Intel(R) Iris(R) Xe Graphics",
            "AMD Radeon Pro 5500M",
            "NVIDIA GeForce RTX 4070",
        ] {
            assert_eq!(native_gpu_adapter_name(name).as_deref(), Some(name));
        }
    }

    #[test]
    fn macos_gpu_json_parser_is_bounded_deduplicated_and_never_invents_adapters() {
        let text = br#"{
          "SPDisplaysDataType": [
            {"sppci_model": "Apple M3 Max", "spdisplays_ndrvs": [{"_spdisplays_resolution": "3024 x 1964"}]},
            {"spdisplays_chipset-model": "AMD Radeon Pro 5500M"},
            {"sppci_model": "Apple M3 Max"},
            {"spdisplays_chipset-model": "Microsoft Remote Display Adapter"},
            {"spdisplays_ndrvs": [{"_spdisplays_resolution": "1920 x 1080"}]},
            {"sppci_model": 17}
          ]
        }"#;
        assert_eq!(
            parse_macos_gpu_adapters_json(text),
            vec!["Apple M3 Max", "AMD Radeon Pro 5500M"]
        );
        assert!(parse_macos_gpu_adapters_json(b"not json").is_empty());
        assert!(parse_macos_gpu_adapters_json(b"{}").is_empty());
        assert!(parse_macos_gpu_adapters_json(
            br#"{"SPDisplaysDataType": [{"spdisplays_ndrvs": [{"_spdisplays_resolution": "1920 x 1080"}]}]}"#
        ).is_empty());
        let many = serde_json::json!({
            "SPDisplaysDataType": (0..50)
                .map(|index| serde_json::json!({"sppci_model": format!("GPU {index}")}))
                .collect::<Vec<_>>()
        });
        let parsed = parse_macos_gpu_adapters_json(&serde_json::to_vec(&many).unwrap());
        assert_eq!(parsed.len(), 16);
    }

    #[cfg(windows)]
    #[test]
    fn native_windows_gpu_adapter_enumeration_is_bounded() {
        let adapters = windows_gpu_adapters();
        assert!(adapters.len() <= 16);
        assert!(adapters
            .iter()
            .all(|name| native_gpu_adapter_name(name).is_some()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_macos_gpu_adapter_discovery_is_bounded() {
        let adapters = macos_gpu_adapters();
        assert!(adapters.len() <= 16);
        assert!(adapters
            .iter()
            .all(|name| native_gpu_adapter_name(name).is_some()));
    }

    #[test]
    fn resource_snapshot_is_factual_and_bounded() {
        let root =
            std::env::temp_dir().join(format!("repotunnel-resource-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace {
            id: "resource-test".to_string(),
            name: "resource-test".to_string(),
            path: root.to_string_lossy().into_owned(),
            added_at: 0,
            access_mode: WorkspaceAccessMode::ReadWrite,
            change_policy: WorkspaceChangePolicy::Automatic,
            command_policy: CommandPolicy::Automatic,
        };
        let value = snapshot(&workspace);
        assert!(value.logical_cpu_count >= 1);
        assert!(value.gpu_devices.len() <= 16);
        if let (Some(total), Some(available)) =
            (value.memory_total_bytes, value.memory_available_bytes)
        {
            assert!(total > 0);
            assert!(available <= total);
        }
        if let (Some(total), Some(available)) = (
            value.workspace_disk_total_bytes,
            value.workspace_disk_available_bytes,
        ) {
            assert!(total > 0);
            assert!(available <= total);
        }
        let _ = fs::remove_dir_all(root);
    }
}
