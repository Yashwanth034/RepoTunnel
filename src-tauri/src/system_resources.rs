use std::{fs, path::Path, process::Command};

use crate::models::{SystemResourceSnapshot, Workspace};

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
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn disk_usage(_path: &Path) -> (Option<u64>, Option<u64>) {
    (None, None)
}

fn push_unique(devices: &mut Vec<String>, value: String) {
    let value = value.trim().to_string();
    if !value.is_empty() && !devices.iter().any(|existing| existing == &value) {
        devices.push(value);
    }
}

fn gpu_devices() -> Vec<String> {
    let mut devices = Vec::new();

    if let Ok(output) = Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
    {
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                push_unique(&mut devices, line.to_string());
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = Command::new("lspci").output() {
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
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
    let (memory_total_bytes, memory_available_bytes) = parse_linux_meminfo();
    let (workspace_disk_total_bytes, workspace_disk_available_bytes) =
        disk_usage(Path::new(&workspace.path));
    let gpu_devices = gpu_devices();

    let mut notes = Vec::new();
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
