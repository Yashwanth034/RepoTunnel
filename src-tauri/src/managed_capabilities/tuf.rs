use std::path::{Component, Path};

use sigstore_tuf::{FileStore, HttpRepository, Updater, UpdaterConfig};
use url::Url;

pub(crate) const CAPABILITY_TUF_REPOSITORY: &str =
    "https://raw.githubusercontent.com/Yashwanth034/RepoTunnel/managed-capabilities/tuf";

const MAX_ROOT_METADATA_BYTES: u64 = 512 * 1024;
const MAX_TIMESTAMP_METADATA_BYTES: u64 = 256 * 1024;
const MAX_SNAPSHOT_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_TARGETS_METADATA_BYTES: u64 = 2 * 1024 * 1024;
const MAX_TUF_TARGET_BYTES: u64 = 2 * 1024 * 1024;
const MAX_ROOT_ROTATIONS: u64 = 64;
const MAX_DELEGATION_DEPTH: u32 = 8;
const TUF_HTTP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// The production capability repository remains intentionally disabled until RepoTunnel's
// dedicated capability TUF root has been provisioned through the separate signing ceremony.
// This public trust anchor is compiled into the application from a dedicated provisioning slot;
// it must never be fetched from the same network path that supplies update metadata.
const PRODUCTION_PINNED_ROOT: &[u8] =
    include_bytes!("../../resources/managed-capabilities/tuf/root.json");
const UNPROVISIONED_ROOT_SENTINEL: &[u8] = b"UNPROVISIONED\n";

fn production_pinned_root() -> Result<&'static [u8], String> {
    if PRODUCTION_PINNED_ROOT.is_empty() || PRODUCTION_PINNED_ROOT == UNPROVISIONED_ROOT_SENTINEL {
        return Err(
            "RepoTunnel managed capability TUF root is not provisioned; signed capability updates are disabled."
                .to_string(),
        );
    }
    validate_bootstrap_root_shape(PRODUCTION_PINNED_ROOT)?;
    Ok(PRODUCTION_PINNED_ROOT)
}

fn validate_bootstrap_root_shape(bytes: &[u8]) -> Result<u64, String> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_ROOT_METADATA_BYTES {
        return Err("Managed capability bootstrap TUF root exceeds the safety limit.".to_string());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        format!("Managed capability bootstrap TUF root is invalid JSON: {error}")
    })?;
    if value
        .pointer("/signed/_type")
        .and_then(serde_json::Value::as_str)
        != Some("root")
    {
        return Err("Managed capability bootstrap file is not TUF root metadata.".to_string());
    }
    let version = value
        .pointer("/signed/version")
        .and_then(serde_json::Value::as_u64)
        .filter(|version| *version > 0)
        .ok_or_else(|| "Managed capability bootstrap TUF root has no valid version.".to_string())?;
    let keys = value
        .pointer("/signed/keys")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Managed capability bootstrap TUF root is missing keys.".to_string())?;
    let roles = value
        .pointer("/signed/roles")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Managed capability bootstrap TUF root is missing roles.".to_string())?;
    for role_name in ["root", "targets", "snapshot", "timestamp"] {
        let role = roles
            .get(role_name)
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| {
                format!("Managed capability bootstrap TUF root is missing the {role_name} role.")
            })?;
        let key_ids = role
            .get("keyids")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                format!("Managed capability bootstrap TUF {role_name} role has no key IDs.")
            })?;
        let mut unique_key_ids = std::collections::HashSet::new();
        for key_id in key_ids {
            let key_id = key_id
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!(
                        "Managed capability bootstrap TUF {role_name} role has an invalid key ID."
                    )
                })?;
            if !keys.contains_key(key_id) {
                return Err(format!(
                    "Managed capability bootstrap TUF {role_name} role references an unknown key."
                ));
            }
            unique_key_ids.insert(key_id);
        }
        let threshold = role
            .get("threshold")
            .and_then(serde_json::Value::as_u64)
            .filter(|threshold| *threshold > 0)
            .ok_or_else(|| {
                format!(
                    "Managed capability bootstrap TUF {role_name} role has an invalid threshold."
                )
            })?;
        if threshold as usize > unique_key_ids.len() {
            return Err(format!(
                "Managed capability bootstrap TUF {role_name} role threshold exceeds its unique keys."
            ));
        }
    }
    Ok(version)
}

fn validate_https_base(value: &str) -> Result<Url, String> {
    let url = Url::parse(value)
        .map_err(|_| "Managed capability TUF repository URL is invalid.".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Managed capability TUF repository must use a plain HTTPS origin.".to_string());
    }
    Ok(url)
}

pub(crate) fn validate_repository_base(value: &str) -> Result<(), String> {
    let url = validate_https_base(value)?;
    let expected = Url::parse(CAPABILITY_TUF_REPOSITORY)
        .expect("RepoTunnel managed capability TUF repository constant must be valid");
    if url != expected {
        return Err(
            "Managed capability TUF repository origin is not RepoTunnel's pinned repository."
                .to_string(),
        );
    }
    Ok(())
}

fn validate_target_path(target_path: &str) -> Result<(), String> {
    if target_path.is_empty()
        || target_path.len() > 256
        || target_path.contains('\0')
        || target_path.contains('\\')
        || Path::new(target_path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("Managed capability TUF target path is invalid.".to_string());
    }
    Ok(())
}

fn updater_config() -> UpdaterConfig {
    UpdaterConfig {
        root_max_length: MAX_ROOT_METADATA_BYTES,
        timestamp_max_length: MAX_TIMESTAMP_METADATA_BYTES,
        snapshot_max_length: MAX_SNAPSHOT_METADATA_BYTES,
        targets_max_length: MAX_TARGETS_METADATA_BYTES,
        target_max_length: MAX_TUF_TARGET_BYTES,
        max_root_rotations: MAX_ROOT_ROTATIONS,
        max_delegations: MAX_DELEGATION_DEPTH,
    }
}

fn build_tuf_http_client_with_timeout(
    timeout: std::time::Duration,
) -> Result<sigstore_tuf::reqwest::Client, String> {
    sigstore_tuf::reqwest::Client::builder()
        .redirect(sigstore_tuf::reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|error| format!("Could not initialize managed capability HTTPS client: {error}"))
}

fn build_tuf_http_client() -> Result<sigstore_tuf::reqwest::Client, String> {
    build_tuf_http_client_with_timeout(TUF_HTTP_REQUEST_TIMEOUT)
}

async fn fetch_verified_target_from_allowed_base(
    repository_base: &str,
    pinned_root: &[u8],
    cache_dir: &Path,
    target_path: &str,
) -> Result<Vec<u8>, String> {
    validate_https_base(repository_base)?;
    validate_target_path(target_path)?;
    super::protect_directory(cache_dir)?;

    let repository = HttpRepository::new(repository_base)
        .map_err(|error| format!("Could not initialize managed capability TUF transport: {error}"))?
        .with_http_client(build_tuf_http_client()?);
    let mut updater = Updater::new(repository, pinned_root)
        .map_err(|error| format!("Could not verify pinned TUF root: {error}"))?
        .with_config(updater_config())
        .with_store(FileStore::new(cache_dir));
    let now = jiff::Timestamp::now();
    updater
        .refresh(now)
        .await
        .map_err(|error| format!("Managed capability TUF refresh failed: {error}"))?;
    updater
        .get_target(target_path, now)
        .await
        .map_err(|error| format!("Managed capability TUF target verification failed: {error}"))
}

pub(crate) async fn fetch_verified_target(
    repository_base: &str,
    pinned_root: &[u8],
    cache_dir: &Path,
    target_path: &str,
) -> Result<Vec<u8>, String> {
    validate_repository_base(repository_base)?;
    fetch_verified_target_from_allowed_base(repository_base, pinned_root, cache_dir, target_path)
        .await
}

pub(crate) async fn fetch_production_target(
    cache_dir: &Path,
    target_path: &str,
) -> Result<Vec<u8>, String> {
    validate_target_path(target_path)?;
    let root = production_pinned_root()?;
    super::protect_directory(cache_dir)?;
    let metadata_cache = cache_dir.join("metadata");
    super::protect_directory(&metadata_cache)?;
    fetch_verified_target(
        CAPABILITY_TUF_REPOSITORY,
        root,
        &metadata_cache,
        target_path,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::time::Duration;

    fn spawn_http_once(response: String, delay: Duration) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        address
    }

    #[test]
    fn production_capability_root_fails_closed_until_a_local_root_is_provisioned() {
        let error = production_pinned_root().unwrap_err();
        assert!(error.contains("not provisioned"), "{error}");
    }

    #[test]
    fn production_root_comes_from_the_dedicated_public_bootstrap_resource() {
        let resource_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("resources/managed-capabilities/tuf/root.json");
        assert!(
            resource_path.is_file(),
            "dedicated managed-capability TUF root provisioning resource is missing"
        );
        let resource = std::fs::read(resource_path).unwrap();
        assert_eq!(PRODUCTION_PINNED_ROOT, resource.as_slice());
    }

    #[test]
    fn bootstrap_root_shape_is_strict_and_versioned() {
        let key_id = "a".repeat(64);
        let root = serde_json::json!({
            "signed": {
                "_type": "root",
                "version": 3,
                "keys": {
                    key_id.clone(): {
                        "keytype": "ed25519",
                        "scheme": "ed25519",
                        "keyval": { "public": "fixture" }
                    }
                },
                "roles": {
                    "root": { "keyids": [key_id.clone()], "threshold": 1 },
                    "targets": { "keyids": [key_id.clone()], "threshold": 1 },
                    "snapshot": { "keyids": [key_id.clone()], "threshold": 1 },
                    "timestamp": { "keyids": [key_id], "threshold": 1 }
                }
            },
            "signatures": []
        })
        .to_string();
        assert_eq!(validate_bootstrap_root_shape(root.as_bytes()).unwrap(), 3);
        assert!(validate_bootstrap_root_shape(
            br#"{"signed":{"_type":"targets","version":1,"keys":{},"roles":{}},"signatures":[]}"#
        )
        .is_err());
    }

    #[test]
    fn bootstrap_root_requires_all_standard_roles_and_valid_threshold_keys() {
        let key_id = "a".repeat(64);
        let valid = serde_json::json!({
            "signed": {
                "_type": "root",
                "version": 1,
                "keys": {
                    key_id.clone(): {
                        "keytype": "ed25519",
                        "scheme": "ed25519",
                        "keyval": { "public": "fixture" }
                    }
                },
                "roles": {
                    "root": { "keyids": [key_id.clone()], "threshold": 1 },
                    "targets": { "keyids": [key_id.clone()], "threshold": 1 },
                    "snapshot": { "keyids": [key_id.clone()], "threshold": 1 },
                    "timestamp": { "keyids": [key_id.clone()], "threshold": 1 }
                }
            },
            "signatures": []
        });

        let mut missing_role = valid.clone();
        missing_role["signed"]["roles"]
            .as_object_mut()
            .unwrap()
            .remove("timestamp");
        assert!(validate_bootstrap_root_shape(missing_role.to_string().as_bytes()).is_err());

        let mut missing_key = valid.clone();
        missing_key["signed"]["roles"]["targets"]["keyids"] = serde_json::json!(["b".repeat(64)]);
        assert!(validate_bootstrap_root_shape(missing_key.to_string().as_bytes()).is_err());

        let mut impossible_threshold = valid.clone();
        impossible_threshold["signed"]["roles"]["root"]["threshold"] = serde_json::json!(2);
        assert!(
            validate_bootstrap_root_shape(impossible_threshold.to_string().as_bytes()).is_err()
        );
    }

    #[test]
    fn repository_base_requires_exact_https_origin_without_url_tricks() {
        assert!(validate_repository_base(
            "https://raw.githubusercontent.com/Yashwanth034/RepoTunnel/managed-capabilities/tuf"
        )
        .is_ok());
        for bad in [
            "http://raw.githubusercontent.com/Yashwanth034/RepoTunnel/managed-capabilities/tuf",
            "https://user@raw.githubusercontent.com/Yashwanth034/RepoTunnel/managed-capabilities/tuf",
            "https://raw.githubusercontent.com:444/Yashwanth034/RepoTunnel/managed-capabilities/tuf",
            "https://raw.githubusercontent.com/Other/Repo/managed-capabilities/tuf",
            "https://raw.githubusercontent.com/Yashwanth034/RepoTunnel/managed-capabilities/tuf?x=1",
            "https://raw.githubusercontent.com/Yashwanth034/RepoTunnel/managed-capabilities/tuf#fragment",
        ] {
            assert!(validate_repository_base(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn target_paths_are_bounded_and_relative() {
        assert!(validate_target_path("capabilities/index.json").is_ok());
        for bad in [
            "",
            "/absolute.json",
            "../escape.json",
            "capabilities/../escape.json",
            "capabilities\\index.json",
        ] {
            assert!(validate_target_path(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn managed_tuf_http_client_never_follows_redirects() {
        let foreign = spawn_http_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nforeign".to_string(),
            Duration::ZERO,
        );
        let redirect = spawn_http_once(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{foreign}/metadata\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
            Duration::ZERO,
        );

        let response = build_tuf_http_client()
            .unwrap()
            .get(format!("http://{redirect}/root.json"))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_redirection(),
            "managed TUF transport followed a redirect to a different endpoint"
        );
    }

    #[tokio::test]
    async fn managed_tuf_http_client_has_a_bounded_request_timeout() {
        let slow = spawn_http_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_string(),
            Duration::from_millis(500),
        );
        // A separate watchdog bounds the test without assuming wall-clock
        // scheduling fairness when the entire Rust library suite is running
        // other CPU-heavy tests in parallel. The inner request still MUST
        // report its own configured 100 ms reqwest timeout.
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            build_tuf_http_client_with_timeout(Duration::from_millis(100))
                .unwrap()
                .get(format!("http://{slow}/timestamp.json"))
                .send(),
        )
        .await
        .expect("managed TUF client must time out before the test watchdog");
        assert!(result.is_err_and(|error| error.is_timeout()));
    }

    #[tokio::test]
    async fn invalid_pinned_root_fails_before_any_target_is_trusted() {
        let temp = tempfile::tempdir().unwrap();
        let error = fetch_verified_target(
            CAPABILITY_TUF_REPOSITORY,
            b"{}",
            temp.path(),
            "capabilities/index.json",
        )
        .await
        .unwrap_err();
        assert!(error.contains("pinned TUF root"), "{error}");
    }

    #[test]
    #[ignore = "operator proof for a newly signed production bootstrap root supplied by environment"]
    fn candidate_bootstrap_root_is_self_signed_and_role_complete() {
        let path = std::env::var("REPOTUNNEL_CAPABILITY_TUF_ROOT_CANDIDATE")
            .expect("set REPOTUNNEL_CAPABILITY_TUF_ROOT_CANDIDATE to the signed public root.json");
        let bytes = std::fs::read(path).expect("could not read candidate capability TUF root");
        validate_bootstrap_root_shape(&bytes).expect("candidate root structure is invalid");
        let repository = HttpRepository::new(CAPABILITY_TUF_REPOSITORY)
            .expect("pinned capability repository URL must be valid");
        Updater::new(repository, &bytes)
            .expect("candidate root did not satisfy its own TUF root signature threshold");
    }

    #[tokio::test]
    #[ignore = "networked proof that the pinned-root TUF client interoperates with a real tuf-on-ci repository"]
    async fn pinned_public_tuf_root_verifies_a_live_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = include_bytes!("../../test-fixtures/managed-capabilities/sigstore-root-10.json");
        let bytes = fetch_verified_target_from_allowed_base(
            "https://tuf-repo-cdn.sigstore.dev",
            root,
            temp.path(),
            "trusted_root.json",
        )
        .await
        .unwrap();
        assert!(!bytes.is_empty());
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(value.get("mediaType").is_some() || value.get("tlogs").is_some());
    }
}
