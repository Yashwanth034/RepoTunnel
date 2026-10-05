use std::{
    fs,
    net::{IpAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::{app_state::AppState, direct_https};

const STANDARD_WIREGUARD_CONFIG: &str = "/etc/wireguard/rt-direct.conf";
const STANDARD_WIREGUARD_SERVICE: &str = "wg-quick@rt-direct";
const NETWORK_REDIRECT_SERVICE: &str = "repotunnel-direct-network.service";
const NETWORK_REDIRECT_UNIT: &str = "/etc/systemd/system/repotunnel-direct-network.service";
const NETWORK_REDIRECT_HELPER: &str = "/usr/local/libexec/repotunnel-direct-network";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpsSetupReadiness {
    supported_platform: bool,
    wireguard_installed: bool,
    wg_quick_installed: bool,
    nftables_installed: bool,
    openssl_installed: bool,
    certbot_ready: bool,
    pipx_installed: bool,
    systemd_available: bool,
    native_global_ipv6_available: bool,
    wireguard_interface_active: bool,
    standard_wireguard_config_present: bool,
    standard_wireguard_service_active: bool,
    nftables_rules_readable: bool,
    nftables_rules_present: bool,
    pkexec_available: bool,
    direct_https_configured: bool,
    direct_https_local_ready: bool,
    direct_https_tls_trusted: bool,
    direct_https_public_reachable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpsSetupHostnameVerification {
    valid_hostname: bool,
    dns_resolves: bool,
    ipv4_available: bool,
    ipv6_available: bool,
    health_reachable: bool,
    tls_trusted: bool,
    oauth_resource_metadata_reachable: bool,
    oauth_server_metadata_reachable: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HttpsSetupInstallResult {
    cancelled: bool,
    installed: bool,
    service_active: bool,
    network_rules_present: bool,
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-lc", &format!("command -v -- {name} >/dev/null 2>&1")])
        .status()
        .is_ok_and(|status| status.success())
}

fn command_success(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .status()
        .is_ok_and(|status| status.success())
}

fn native_global_ipv6_available() -> bool {
    let Ok(output) = Command::new("ip")
        .args(["-6", "-o", "addr", "show", "scope", "global"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|item| item.split('/').next())
        .filter_map(|item| item.parse::<std::net::Ipv6Addr>().ok())
        .any(|address| {
            !address.is_loopback()
                && !address.is_unspecified()
                && !address.is_multicast()
                && !address.is_unique_local()
                && !address.is_unicast_link_local()
        })
}

fn wireguard_interface_active() -> bool {
    let Ok(output) = Command::new("wg").arg("show").arg("interfaces").output() else {
        return false;
    };
    output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

fn nftables_status() -> (bool, bool) {
    let Ok(output) = Command::new("nft")
        .args(["list", "table", "inet", "repotunnel_direct"])
        .output()
    else {
        return (false, false);
    };

    if output.status.success() {
        return (true, true);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    let permission_denied = stderr.contains("operation not permitted")
        || stderr.contains("permission denied")
        || stderr.contains("you must be root");
    (!permission_denied, false)
}

fn normalize_hostname(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 253 {
        return None;
    }

    let candidate = if trimmed.contains("://") {
        let parsed = url::Url::parse(trimmed).ok()?;
        if parsed.scheme() != "https"
            || parsed.username() != ""
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.path() != "/"
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return None;
        }
        parsed.host_str()?.to_string()
    } else {
        if trimmed.contains('/') || trimmed.contains(':') || trimmed.contains('@') {
            return None;
        }
        trimmed.to_string()
    };

    let hostname = candidate.trim_end_matches('.').to_ascii_lowercase();
    if hostname.is_empty()
        || !hostname.contains('.')
        || hostname.len() > 253
        || hostname.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
    {
        return None;
    }

    Some(hostname)
}

fn verify_hostname_blocking(value: String) -> HttpsSetupHostnameVerification {
    let Some(hostname) = normalize_hostname(&value) else {
        return HttpsSetupHostnameVerification {
            valid_hostname: false,
            dns_resolves: false,
            ipv4_available: false,
            ipv6_available: false,
            health_reachable: false,
            tls_trusted: false,
            oauth_resource_metadata_reachable: false,
            oauth_server_metadata_reachable: false,
        };
    };

    let addresses = (hostname.as_str(), 443)
        .to_socket_addrs()
        .map(|iter| iter.map(|address| address.ip()).collect::<Vec<IpAddr>>())
        .unwrap_or_default();
    let ipv4_available = addresses.iter().any(IpAddr::is_ipv4);
    let ipv6_available = addresses.iter().any(IpAddr::is_ipv6);
    let dns_resolves = ipv4_available || ipv6_available;

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(7))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build();

    let (
        health_reachable,
        tls_trusted,
        oauth_resource_metadata_reachable,
        oauth_server_metadata_reachable,
    ) = match client {
        Ok(client) => {
            let health = client.get(format!("https://{hostname}/health")).send();
            let tls_trusted = health.is_ok();
            let health_reachable = health
                .as_ref()
                .is_ok_and(|response| response.status().is_success());

            let oauth_resource_metadata_reachable = client
                .get(format!(
                    "https://{hostname}/.well-known/oauth-protected-resource/mcp"
                ))
                .send()
                .is_ok_and(|response| response.status().is_success());
            let oauth_server_metadata_reachable = client
                .get(format!(
                    "https://{hostname}/.well-known/oauth-authorization-server"
                ))
                .send()
                .is_ok_and(|response| response.status().is_success());

            (
                health_reachable,
                tls_trusted,
                oauth_resource_metadata_reachable,
                oauth_server_metadata_reachable,
            )
        }
        Err(_) => (false, false, false, false),
    };

    HttpsSetupHostnameVerification {
        valid_hostname: true,
        dns_resolves,
        ipv4_available,
        ipv6_available,
        health_reachable,
        tls_trusted,
        oauth_resource_metadata_reachable,
        oauth_server_metadata_reachable,
    }
}

fn validate_wireguard_config(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!("Could not inspect the selected WireGuard configuration: {error}")
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Choose a regular WireGuard configuration file.".to_string());
    }
    if metadata.len() == 0 || metadata.len() > 64 * 1024 {
        return Err("The selected WireGuard configuration has an unexpected size.".to_string());
    }

    let content = fs::read_to_string(path)
        .map_err(|error| format!("Could not read the selected WireGuard configuration: {error}"))?;
    let lower = content.to_ascii_lowercase();
    let required = [
        "[interface]",
        "privatekey",
        "address",
        "[peer]",
        "publickey",
        "endpoint",
        "allowedips",
    ];
    if required.iter().any(|needle| !lower.contains(needle)) {
        return Err(
            "The selected file does not look like a complete WireGuard tunnel configuration."
                .to_string(),
        );
    }

    let mut has_ipv6_address = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let Some((raw_key, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = raw_key.trim().to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "preup" | "postup" | "predown" | "postdown" | "saveconfig"
        ) {
            return Err(
                "The selected WireGuard file contains executable hook directives. Use the original provider-generated configuration without custom hooks."
                    .to_string(),
            );
        }
        if key == "address" {
            has_ipv6_address |= raw_value.split(',').any(|candidate| {
                candidate
                    .trim()
                    .split('/')
                    .next()
                    .and_then(|value| value.parse::<std::net::Ipv6Addr>().ok())
                    .is_some()
            });
        }
    }
    if !has_ipv6_address {
        return Err(
            "The selected WireGuard configuration does not contain the required IPv6 tunnel address."
                .to_string(),
        );
    }

    Ok(content)
}

fn write_https_setup_support_files(
    app: &AppHandle,
    wireguard_config: &str,
) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let cache_root = app
        .path()
        .app_cache_dir()
        .map_err(|error| format!("Could not resolve RepoTunnel cache directory: {error}"))?
        .join("https-setup");
    fs::create_dir_all(&cache_root)
        .map_err(|error| format!("Could not prepare HTTPS setup helper files: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&cache_root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect HTTPS setup helper directory: {error}"))?;
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let stage_dir = cache_root.join(format!("stage-{}-{nonce}", std::process::id()));
    fs::create_dir(&stage_dir)
        .map_err(|error| format!("Could not create HTTPS setup staging directory: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&stage_dir, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir_all(&stage_dir);
            return Err(format!(
                "Could not protect HTTPS setup staging directory: {error}"
            ));
        }
    }

    let config_path = stage_dir.join("rt-direct.conf");
    let helper_path = stage_dir.join("repotunnel-direct-network");
    let unit_path = stage_dir.join("repotunnel-direct-network.service");
    let helper = format!(
        r#"#!/bin/sh
set -eu

ACTION="$1"
INTERFACE="$2"
TABLE="repotunnel_direct"
NFT="$(command -v nft)"
IP="$(command -v ip)"

case "$ACTION" in
  up)
    ADDRESS="$("$IP" -6 -o addr show dev "$INTERFACE" scope global | awk '{{print $4}}' | cut -d/ -f1 | head -n1)"
    [ -n "$ADDRESS" ] || exit 1
    "$NFT" delete table inet "$TABLE" 2>/dev/null || true
    "$NFT" -f - <<EOF
table inet repotunnel_direct {{
  chain prerouting {{
    type nat hook prerouting priority dstnat; policy accept;
    ip6 daddr $ADDRESS tcp dport 443 redirect to :{}
    ip6 daddr $ADDRESS tcp dport 80 redirect to :{}
  }}
}}
EOF
    ;;
  down)
    "$NFT" delete table inet "$TABLE" 2>/dev/null || true
    ;;
  *)
    exit 2
    ;;
esac
"#,
        direct_https::HTTPS_LISTEN_PORT,
        direct_https::HTTP_CHALLENGE_PORT
    );
    let unit = r#"[Unit]
Description=RepoTunnel Direct HTTPS network redirects
After=network-online.target wg-quick@rt-direct.service
Requires=wg-quick@rt-direct.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/libexec/repotunnel-direct-network up rt-direct
ExecStop=/usr/local/libexec/repotunnel-direct-network down rt-direct

[Install]
WantedBy=multi-user.target
"#;

    let staged = (|| -> Result<(), String> {
        fs::write(&config_path, wireguard_config)
            .map_err(|error| format!("Could not stage the WireGuard configuration: {error}"))?;
        fs::write(&helper_path, helper)
            .map_err(|error| format!("Could not prepare HTTPS setup network helper: {error}"))?;
        fs::write(&unit_path, unit).map_err(|error| {
            format!("Could not prepare HTTPS setup service definition: {error}")
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).map_err(
                |error| format!("Could not protect staged WireGuard configuration: {error}"),
            )?;
            fs::set_permissions(&helper_path, fs::Permissions::from_mode(0o700)).map_err(
                |error| format!("Could not protect HTTPS setup network helper: {error}"),
            )?;
            fs::set_permissions(&unit_path, fs::Permissions::from_mode(0o600)).map_err(
                |error| format!("Could not protect HTTPS setup service definition: {error}"),
            )?;
        }

        Ok(())
    })();

    if let Err(error) = staged {
        let _ = fs::remove_dir_all(&stage_dir);
        return Err(error);
    }

    Ok((stage_dir, config_path, helper_path, unit_path))
}

fn install_wireguard_config_blocking(
    app: AppHandle,
    source: PathBuf,
) -> Result<HttpsSetupInstallResult, String> {
    if !cfg!(target_os = "linux") {
        return Err(
            "Guided WireGuard installation is currently available on Linux only.".to_string(),
        );
    }
    if !command_exists("pkexec") {
        return Err(
            "The OS privilege helper (pkexec) is unavailable. Install PolicyKit or use the manual advanced setup."
                .to_string(),
        );
    }
    if Path::new(STANDARD_WIREGUARD_CONFIG).exists()
        || Path::new(NETWORK_REDIRECT_UNIT).exists()
        || Path::new(NETWORK_REDIRECT_HELPER).exists()
        || command_success(
            "systemctl",
            &["is-active", "--quiet", STANDARD_WIREGUARD_SERVICE],
        )
        || command_success(
            "systemctl",
            &["is-active", "--quiet", NETWORK_REDIRECT_SERVICE],
        )
        || command_success("ip", &["link", "show", "rt-direct"])
    {
        return Err(
            "An existing guided Direct HTTPS system setup was detected. RepoTunnel will not overwrite it automatically."
                .to_string(),
        );
    }
    let wireguard_config = validate_wireguard_config(&source)?;
    let (stage_dir, config_path, helper_path, unit_path) =
        write_https_setup_support_files(&app, &wireguard_config)?;

    let script = r#"set -eu
if [ -e /etc/wireguard/rt-direct.conf ] || [ -e /etc/systemd/system/repotunnel-direct-network.service ] || [ -e /usr/local/libexec/repotunnel-direct-network ]; then
  exit 73
fi
if systemctl is-active --quiet wg-quick@rt-direct.service || systemctl is-active --quiet repotunnel-direct-network.service || ip link show rt-direct >/dev/null 2>&1; then
  exit 73
fi

SUCCESS=0
cleanup() {
  if [ "$SUCCESS" -ne 1 ]; then
    systemctl disable --now repotunnel-direct-network.service >/dev/null 2>&1 || true
    systemctl disable --now wg-quick@rt-direct.service >/dev/null 2>&1 || true
    rm -f /etc/systemd/system/repotunnel-direct-network.service
    rm -f /etc/wireguard/rt-direct.conf
    rm -f /usr/local/libexec/repotunnel-direct-network
    systemctl daemon-reload >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT HUP INT TERM

install -d -m 755 /usr/local/libexec
install -m 755 "$2" /usr/local/libexec/repotunnel-direct-network
install -m 600 "$1" /etc/wireguard/rt-direct.conf
install -m 644 "$3" /etc/systemd/system/repotunnel-direct-network.service
systemctl daemon-reload
systemctl enable --now wg-quick@rt-direct.service
systemctl enable --now repotunnel-direct-network.service

SUCCESS=1
trap - EXIT HUP INT TERM
"#;

    let status_result = Command::new("pkexec")
        .args(["sh", "-c", script, "repotunnel-https-setup"])
        .arg(&config_path)
        .arg(&helper_path)
        .arg(&unit_path)
        .status();

    let _ = fs::remove_dir_all(&stage_dir);

    let status = status_result
        .map_err(|error| format!("Could not start the OS privilege prompt: {error}"))?;

    if !status.success() {
        return Err(
            "The guided WireGuard installation was cancelled or could not complete. No RepoTunnel application settings were changed."
                .to_string(),
        );
    }

    let service_active = command_success(
        "systemctl",
        &["is-active", "--quiet", STANDARD_WIREGUARD_SERVICE],
    );
    let redirect_service_active = command_success(
        "systemctl",
        &["is-active", "--quiet", NETWORK_REDIRECT_SERVICE],
    );
    let (_, nft_rules_present) = nftables_status();
    let network_rules_present = redirect_service_active || nft_rules_present;

    Ok(HttpsSetupInstallResult {
        cancelled: false,
        installed: service_active && redirect_service_active,
        service_active,
        network_rules_present,
    })
}

#[tauri::command]
pub(crate) async fn install_https_setup_wireguard_config(
    app: AppHandle,
) -> Result<HttpsSetupInstallResult, String> {
    if !cfg!(target_os = "linux") {
        return Err(
            "Guided WireGuard installation is currently available on Linux only.".to_string(),
        );
    }

    let selected = app
        .dialog()
        .file()
        .set_title("Choose the downloaded WireGuard configuration")
        .add_filter("WireGuard configuration", &["conf"])
        .blocking_pick_file();

    let Some(selected) = selected else {
        return Ok(HttpsSetupInstallResult {
            cancelled: true,
            installed: false,
            service_active: false,
            network_rules_present: false,
        });
    };
    let path = selected
        .into_path()
        .map_err(|error| format!("Could not resolve the selected configuration: {error}"))?;

    tauri::async_runtime::spawn_blocking(move || install_wireguard_config_blocking(app, path))
        .await
        .map_err(|error| format!("Guided WireGuard installation could not complete: {error}"))?
}

#[tauri::command]
pub(crate) async fn get_https_setup_readiness(
    app: AppHandle,
) -> Result<HttpsSetupReadiness, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let supported_platform = cfg!(target_os = "linux");
        let wireguard_installed = command_exists("wg");
        let wg_quick_installed = command_exists("wg-quick");
        let nftables_installed = command_exists("nft");
        let pipx_installed = command_exists("pipx");
        let systemd_available = command_exists("systemctl");
        let direct_tools = direct_https::tool_availability();
        let (nftables_rules_readable, raw_nftables_rules_present) = if nftables_installed {
            nftables_status()
        } else {
            (false, false)
        };
        let redirect_service_active = supported_platform
            && systemd_available
            && command_success(
                "systemctl",
                &["is-active", "--quiet", NETWORK_REDIRECT_SERVICE],
            );
        let nftables_rules_present = raw_nftables_rules_present || redirect_service_active;
        let state = app.state::<AppState>();
        let public_status = state.public_tunnel_status(&app)?;
        let direct_https_configured =
            public_status.configured && public_status.provider == "direct";

        Ok::<HttpsSetupReadiness, String>(HttpsSetupReadiness {
            supported_platform,
            wireguard_installed,
            wg_quick_installed,
            nftables_installed,
            openssl_installed: direct_tools.openssl_available,
            certbot_ready: direct_tools.certbot_available,
            pipx_installed,
            systemd_available,
            native_global_ipv6_available: supported_platform && native_global_ipv6_available(),
            wireguard_interface_active: wireguard_installed && wireguard_interface_active(),
            standard_wireguard_config_present: supported_platform
                && Path::new(STANDARD_WIREGUARD_CONFIG).is_file(),
            standard_wireguard_service_active: supported_platform
                && systemd_available
                && command_success(
                    "systemctl",
                    &["is-active", "--quiet", STANDARD_WIREGUARD_SERVICE],
                ),
            nftables_rules_readable,
            nftables_rules_present,
            pkexec_available: command_exists("pkexec"),
            direct_https_configured,
            direct_https_local_ready: direct_https_configured && public_status.local_ready,
            direct_https_tls_trusted: direct_https_configured && public_status.tls_trusted,
            direct_https_public_reachable: direct_https_configured
                && public_status.public_reachable,
        })
    })
    .await
    .map_err(|error| format!("HTTPS setup readiness check could not complete: {error}"))?
}

fn https_setup_resource_url(resource: &str) -> Option<&'static str> {
    match resource {
        "wireguard" => Some("https://www.wireguard.com/install/"),
        "route64" => Some("https://route64.org/"),
        "duckdns" => Some("https://www.duckdns.org/"),
        "ipv4-compatibility" => Some("https://v4-frontend.netiter.com/"),
        _ => None,
    }
}

#[tauri::command]
pub(crate) fn open_https_setup_resource(resource: String) -> Result<(), String> {
    let url = https_setup_resource_url(&resource)
        .ok_or_else(|| "That HTTPS setup resource is not allowed.".to_string())?;

    #[cfg(target_os = "linux")]
    let result = Command::new("xdg-open").arg(url).spawn();

    #[cfg(target_os = "macos")]
    let result = Command::new("open").arg(url).spawn();

    #[cfg(windows)]
    let result = Command::new("cmd").args(["/C", "start", "", url]).spawn();

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let result: std::io::Result<std::process::Child> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Unsupported platform",
    ));

    result
        .map(|_| ())
        .map_err(|error| format!("Could not open the HTTPS setup resource: {error}"))
}

#[tauri::command]
pub(crate) async fn verify_https_setup_hostname(
    hostname: String,
) -> Result<HttpsSetupHostnameVerification, String> {
    tauri::async_runtime::spawn_blocking(move || Ok(verify_hostname_blocking(hostname)))
        .await
        .map_err(|error| format!("HTTPS hostname verification could not complete: {error}"))?
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{https_setup_resource_url, normalize_hostname, validate_wireguard_config};

    #[test]
    fn hostname_normalization_accepts_generic_https_hosts() {
        assert_eq!(
            normalize_hostname("demo-tunnel.duckdns.org"),
            Some("demo-tunnel.duckdns.org".to_string())
        );
        assert_eq!(
            normalize_hostname("https://demo-tunnel.duckdns.org"),
            Some("demo-tunnel.duckdns.org".to_string())
        );
    }

    #[test]
    fn hostname_normalization_rejects_paths_ports_queries_and_credentials() {
        assert!(normalize_hostname("example.invalid/path").is_none());
        assert!(normalize_hostname("example.invalid:443").is_none());
        assert!(normalize_hostname("https://user@example.invalid").is_none());
        assert!(normalize_hostname("https://example.invalid/path").is_none());
        assert!(normalize_hostname("https://example.invalid/?value=1").is_none());
    }

    #[test]
    fn setup_resource_allowlist_rejects_arbitrary_urls() {
        assert_eq!(
            https_setup_resource_url("duckdns"),
            Some("https://www.duckdns.org/")
        );
        assert!(https_setup_resource_url("https://example.invalid/").is_none());
        assert!(https_setup_resource_url("../unexpected").is_none());
    }

    #[test]
    fn wireguard_validator_accepts_generic_ipv6_tunnel_config() {
        let file = tempfile::NamedTempFile::new().expect("temp config");
        fs::write(
            file.path(),
            "[Interface]\nPrivateKey = TEST_PRIVATE_KEY_PLACEHOLDER\nAddress = 2001:db8:1234::2/64\n\n[Peer]\nPublicKey = TEST_PUBLIC_KEY_PLACEHOLDER\nEndpoint = 192.0.2.10:51820\nAllowedIPs = ::/1, 8000::/1\nPersistentKeepalive = 15\n",
        )
        .expect("write config");

        assert!(validate_wireguard_config(file.path()).is_ok());
    }

    #[test]
    fn wireguard_validator_rejects_hooks_and_missing_ipv6() {
        let hook_file = tempfile::NamedTempFile::new().expect("temp config");
        fs::write(
            hook_file.path(),
            "[Interface]\nPrivateKey = TEST_PRIVATE_KEY_PLACEHOLDER\nAddress = 2001:db8:1234::2/64\nPostUp = touch /tmp/example\n\n[Peer]\nPublicKey = TEST_PUBLIC_KEY_PLACEHOLDER\nEndpoint = 192.0.2.10:51820\nAllowedIPs = ::/0\n",
        )
        .expect("write config");
        assert!(validate_wireguard_config(hook_file.path()).is_err());

        let ipv4_only = tempfile::NamedTempFile::new().expect("temp config");
        fs::write(
            ipv4_only.path(),
            "[Interface]\nPrivateKey = TEST_PRIVATE_KEY_PLACEHOLDER\nAddress = 192.0.2.2/32\n\n[Peer]\nPublicKey = TEST_PUBLIC_KEY_PLACEHOLDER\nEndpoint = 192.0.2.10:51820\nAllowedIPs = 0.0.0.0/0\n",
        )
        .expect("write config");
        assert!(validate_wireguard_config(ipv4_only.path()).is_err());
    }
}
