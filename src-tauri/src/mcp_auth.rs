use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tauri::{path::BaseDirectory, AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};

const AUTH_FILE: &str = "mcp-auth.json";
const CLIENTS_FILE: &str = "mcp-clients.json";
const MAX_REGISTERED_CLIENTS: usize = 32;
const MAX_AUTH_SESSIONS: usize = 64;
const CLIENT_REGISTRATION_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;
const TOKEN_BYTES: usize = 32;
const ACCESS_TOKEN_LIFETIME_SECS: u64 = 60 * 60;
const AUTH_CODE_LIFETIME_SECS: u64 = 5 * 60;
const AUTH_STORE_VERSION: u32 = 2;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStoredAuth {
    version: u32,
    client_id: String,
    resource: String,
    access_token_hash: String,
    access_expires_at: u64,
    refresh_token_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAuthSession {
    client_id: String,
    resource: String,
    access_token_hash: String,
    access_expires_at: u64,
    refresh_token_hash: String,
    #[serde(default)]
    issued_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAuthStore {
    version: u32,
    sessions: Vec<StoredAuthSession>,
}

impl Default for StoredAuthStore {
    fn default() -> Self {
        Self {
            version: AUTH_STORE_VERSION,
            sessions: Vec::new(),
        }
    }
}

static AUTH_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CLIENT_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn auth_store_lock() -> &'static Mutex<()> {
    AUTH_STORE_LOCK.get_or_init(|| Mutex::new(()))
}

fn client_store_lock() -> &'static Mutex<()> {
    CLIENT_STORE_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegisteredClient {
    pub(crate) client_id: String,
    pub(crate) client_name: String,
    pub(crate) redirect_uris: Vec<String>,
    pub(crate) issued_at: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct IssuedTokens {
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) expires_in: u64,
}

fn now_epoch_secs() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| "System clock is earlier than the Unix epoch.".to_string())
}

fn auth_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(AUTH_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve RepoTunnel authentication storage: {error}"))
}

fn clients_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .resolve(CLIENTS_FILE, BaseDirectory::AppData)
        .map_err(|error| format!("Could not resolve OAuth client storage: {error}"))
}

fn private_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to write RepoTunnel authentication data through a symbolic link.".to_string(),
        );
    }

    let parent = path
        .parent()
        .ok_or_else(|| "Could not resolve RepoTunnel data directory.".to_string())?;

    fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create RepoTunnel data directory: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not protect RepoTunnel data directory: {error}"))?;
    }

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|error| format!("Could not save RepoTunnel authentication data: {error}"))?;

    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Could not save RepoTunnel authentication data: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            format!("Could not protect RepoTunnel authentication data: {error}")
        })?;
    }

    Ok(())
}

pub(crate) fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; TOKEN_BYTES];

    getrandom::fill(&mut bytes).map_err(|error| {
        format!("Could not generate secure RepoTunnel authentication material: {error}")
    })?;

    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn encode_hash(hash: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(hash)
}

fn decode_hash(encoded: &str) -> Result<[u8; 32], String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| "Saved RepoTunnel authentication data is invalid.".to_string())?;

    bytes
        .try_into()
        .map_err(|_| "Saved RepoTunnel authentication hash has an invalid length.".to_string())
}

pub(crate) fn verify_token(candidate: &str, expected_hash: &[u8; 32]) -> bool {
    let candidate_hash = hash_token(candidate);

    expected_hash
        .as_slice()
        .ct_eq(candidate_hash.as_slice())
        .unwrap_u8()
        == 1
}

fn validate_session(session: &StoredAuthSession) -> Result<(), String> {
    if session.client_id.trim().is_empty() || session.resource.trim().is_empty() {
        return Err("Saved RepoTunnel authentication data is invalid.".to_string());
    }
    decode_hash(&session.access_token_hash)?;
    decode_hash(&session.refresh_token_hash)?;
    Ok(())
}

fn parse_auth_store(contents: &[u8]) -> Result<StoredAuthStore, String> {
    if let Ok(store) = serde_json::from_slice::<StoredAuthStore>(contents) {
        if store.version != AUTH_STORE_VERSION {
            return Err("Unsupported RepoTunnel authentication data version.".to_string());
        }
        if store.sessions.len() > MAX_AUTH_SESSIONS {
            return Err("Saved RepoTunnel authentication data has too many sessions.".to_string());
        }
        for session in &store.sessions {
            validate_session(session)?;
        }
        return Ok(store);
    }

    let legacy: LegacyStoredAuth = serde_json::from_slice(contents)
        .map_err(|_| "Saved RepoTunnel authentication data is invalid.".to_string())?;
    if legacy.version != 1 {
        return Err("Unsupported RepoTunnel authentication data version.".to_string());
    }

    let session = StoredAuthSession {
        client_id: legacy.client_id,
        resource: legacy.resource,
        access_token_hash: legacy.access_token_hash,
        access_expires_at: legacy.access_expires_at,
        refresh_token_hash: legacy.refresh_token_hash,
        issued_at: 0,
    };
    validate_session(&session)?;
    Ok(StoredAuthStore {
        version: AUTH_STORE_VERSION,
        sessions: vec![session],
    })
}

fn load_auth_store_unlocked(app: &AppHandle) -> Result<StoredAuthStore, String> {
    let path = auth_path(app)?;

    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to read RepoTunnel authentication data through a symbolic link.".to_string(),
        );
    }

    if !path.exists() {
        return Ok(StoredAuthStore::default());
    }

    let contents = fs::read(&path)
        .map_err(|error| format!("Could not read RepoTunnel authentication data: {error}"))?;

    if contents.is_empty() {
        return Ok(StoredAuthStore::default());
    }

    parse_auth_store(&contents)
}

fn save_auth_store_unlocked(app: &AppHandle, store: &StoredAuthStore) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(store)
        .map_err(|error| format!("Could not serialize RepoTunnel authentication data: {error}"))?;

    private_write(&auth_path(app)?, &contents)
}

fn append_session(store: &mut StoredAuthStore, session: StoredAuthSession) {
    if store.sessions.len() >= MAX_AUTH_SESSIONS {
        store.sessions.sort_by_key(|item| item.issued_at);
        let remove_count = store
            .sessions
            .len()
            .saturating_add(1)
            .saturating_sub(MAX_AUTH_SESSIONS);
        store.sessions.drain(0..remove_count);
    }
    store.sessions.push(session);
}

fn issue_tokens_into_store(
    store: &mut StoredAuthStore,
    client_id: &str,
    resource: &str,
) -> Result<IssuedTokens, String> {
    let access_token = generate_token()?;
    let refresh_token = generate_token()?;

    let now = now_epoch_secs()?;
    let access_expires_at = now
        .checked_add(ACCESS_TOKEN_LIFETIME_SECS)
        .ok_or_else(|| "Could not calculate RepoTunnel access-token expiry.".to_string())?;

    append_session(
        store,
        StoredAuthSession {
            client_id: client_id.to_string(),
            resource: resource.to_string(),
            access_token_hash: encode_hash(&hash_token(&access_token)),
            access_expires_at,
            refresh_token_hash: encode_hash(&hash_token(&refresh_token)),
            issued_at: now,
        },
    );

    Ok(IssuedTokens {
        access_token,
        refresh_token,
        expires_in: ACCESS_TOKEN_LIFETIME_SECS,
    })
}

fn session_access_matches(
    session: &StoredAuthSession,
    candidate: &str,
    resource: &str,
    now: u64,
) -> Result<bool, String> {
    if session.resource != resource || now >= session.access_expires_at {
        return Ok(false);
    }
    let expected = decode_hash(&session.access_token_hash)?;
    Ok(verify_token(candidate, &expected))
}

fn refresh_session_index(
    store: &StoredAuthStore,
    candidate: &str,
    client_id: &str,
    resource: &str,
) -> Result<Option<usize>, String> {
    for (index, session) in store.sessions.iter().enumerate() {
        if session.client_id != client_id || session.resource != resource {
            continue;
        }
        let expected = decode_hash(&session.refresh_token_hash)?;
        if verify_token(candidate, &expected) {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

pub(crate) fn issue_tokens(
    app: &AppHandle,
    client_id: &str,
    resource: &str,
) -> Result<IssuedTokens, String> {
    let _guard = auth_store_lock()
        .lock()
        .map_err(|_| "RepoTunnel authentication state is unavailable.".to_string())?;
    let mut store = load_auth_store_unlocked(app)?;
    let tokens = issue_tokens_into_store(&mut store, client_id, resource)?;
    save_auth_store_unlocked(app, &store)?;
    Ok(tokens)
}

pub(crate) fn verify_access_token(
    app: &AppHandle,
    candidate: &str,
    resource: &str,
) -> Result<bool, String> {
    let _guard = auth_store_lock()
        .lock()
        .map_err(|_| "RepoTunnel authentication state is unavailable.".to_string())?;
    let store = load_auth_store_unlocked(app)?;
    let now = now_epoch_secs()?;
    for session in &store.sessions {
        if session_access_matches(session, candidate, resource, now)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn rotate_refresh_token(
    app: &AppHandle,
    candidate: &str,
    client_id: &str,
    resource: &str,
) -> Result<Option<IssuedTokens>, String> {
    let _guard = auth_store_lock()
        .lock()
        .map_err(|_| "RepoTunnel authentication state is unavailable.".to_string())?;
    let mut store = load_auth_store_unlocked(app)?;
    let Some(index) = refresh_session_index(&store, candidate, client_id, resource)? else {
        return Ok(None);
    };

    // Rotate only the matching authorization session. Other ChatGPT/MCP sessions remain valid.
    store.sessions.remove(index);
    let tokens = issue_tokens_into_store(&mut store, client_id, resource)?;
    save_auth_store_unlocked(app, &store)?;
    Ok(Some(tokens))
}

fn valid_pkce_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

pub(crate) fn pkce_s256_challenge(verifier: &str) -> Option<String> {
    if !valid_pkce_verifier(verifier) {
        return None;
    }

    Some(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
}

pub(crate) fn verify_pkce_s256(verifier: &str, expected_challenge: &str) -> bool {
    let Some(actual) = pkce_s256_challenge(verifier) else {
        return false;
    };

    actual
        .as_bytes()
        .ct_eq(expected_challenge.as_bytes())
        .unwrap_u8()
        == 1
}

#[derive(Clone, Debug)]
struct PendingAuthorization {
    client_id: String,
    redirect_uri: String,
    resource: String,
    code_challenge: String,
    expires_at: u64,
}

static PENDING_CODES: OnceLock<Mutex<HashMap<String, PendingAuthorization>>> = OnceLock::new();

fn pending_codes() -> &'static Mutex<HashMap<String, PendingAuthorization>> {
    PENDING_CODES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn valid_redirect_uri(value: &str) -> bool {
    let Ok(uri) = url::Url::parse(value) else {
        return false;
    };

    if uri.fragment().is_some() || !uri.username().is_empty() || uri.password().is_some() {
        return false;
    }

    match uri.scheme() {
        "https" => uri.host_str().is_some(),
        "http" => uri.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
        }),
        _ => false,
    }
}

fn valid_resource(value: &str) -> bool {
    let Ok(uri) = url::Url::parse(value) else {
        return false;
    };

    uri.scheme() == "https"
        && uri.host_str().is_some()
        && uri.fragment().is_none()
        && uri.query().is_none()
        && uri.username().is_empty()
        && uri.password().is_none()
}

fn valid_pkce_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn normalize_client_registration(
    client_name: Option<&str>,
    redirect_uris: &[String],
) -> Result<(String, Vec<String>), String> {
    let name = client_name.unwrap_or("MCP client").trim();

    if name.is_empty() || name.len() > 200 {
        return Err("Invalid OAuth client name.".to_string());
    }

    if redirect_uris.is_empty() || redirect_uris.len() > 8 {
        return Err("OAuth clients must provide between 1 and 8 redirect URIs.".to_string());
    }

    let mut normalized = Vec::new();

    for value in redirect_uris {
        let value = value.trim();

        if value.len() > 2048 || !valid_redirect_uri(value) {
            return Err("OAuth client supplied an unsafe redirect URI.".to_string());
        }

        if !normalized.iter().any(|existing| existing == value) {
            normalized.push(value.to_string());
        }
    }

    Ok((name.to_string(), normalized))
}

fn load_registered_clients(app: &AppHandle) -> Result<Vec<RegisteredClient>, String> {
    let path = clients_path(app)?;

    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err("Refusing to read OAuth client data through a symbolic link.".to_string());
    }

    if !path.exists() {
        return Ok(Vec::new());
    }

    let contents =
        fs::read(&path).map_err(|error| format!("Could not read OAuth client data: {error}"))?;

    if contents.is_empty() {
        return Ok(Vec::new());
    }

    serde_json::from_slice(&contents).map_err(|_| "Saved OAuth client data is invalid.".to_string())
}

fn save_registered_clients(app: &AppHandle, clients: &[RegisteredClient]) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(clients)
        .map_err(|error| format!("Could not serialize OAuth client data: {error}"))?;

    private_write(&clients_path(app)?, &contents)
}

pub(crate) fn register_client(
    app: &AppHandle,
    client_name: Option<&str>,
    redirect_uris: &[String],
) -> Result<RegisteredClient, String> {
    let (client_name, redirect_uris) = normalize_client_registration(client_name, redirect_uris)?;
    let _guard = client_store_lock()
        .lock()
        .map_err(|_| "OAuth client registration state is unavailable.".to_string())?;

    let now = now_epoch_secs()?;
    let mut clients = load_registered_clients(app)?;

    clients
        .retain(|client| now.saturating_sub(client.issued_at) <= CLIENT_REGISTRATION_LIFETIME_SECS);

    if clients.len() >= MAX_REGISTERED_CLIENTS {
        return Err(
            "Too many OAuth clients are registered. Revoke old connections before adding another."
                .to_string(),
        );
    }

    let client = RegisteredClient {
        client_id: format!("rt_{}", generate_token()?),
        client_name,
        redirect_uris,
        issued_at: now,
    };

    clients.push(client.clone());
    save_registered_clients(app, &clients)?;

    Ok(client)
}

pub(crate) fn registered_client_for_redirect(
    app: &AppHandle,
    client_id: &str,
    redirect_uri: &str,
) -> Result<Option<RegisteredClient>, String> {
    let _guard = client_store_lock()
        .lock()
        .map_err(|_| "OAuth client registration state is unavailable.".to_string())?;
    let now = now_epoch_secs()?;

    Ok(load_registered_clients(app)?.into_iter().find(|client| {
        client.client_id == client_id
            && now.saturating_sub(client.issued_at) <= CLIENT_REGISTRATION_LIFETIME_SECS
            && client.redirect_uris.iter().any(|uri| uri == redirect_uri)
    }))
}

pub(crate) fn request_pairing_approval(app: &AppHandle, client_id: &str, resource: &str) -> bool {
    let client = client_id.chars().take(180).collect::<String>();
    let resource = resource.chars().take(220).collect::<String>();

    app.dialog()
        .message(format!(
            "An MCP client wants to connect to this RepoTunnel installation.\n\nClient: {client}\nResource: {resource}\n\nAllow this connection?"
        ))
        .buttons(MessageDialogButtons::YesNo)
        .blocking_show()
}

pub(crate) fn create_authorization_code(
    client_id: &str,
    redirect_uri: &str,
    resource: &str,
    code_challenge: &str,
) -> Result<String, String> {
    if client_id.trim().is_empty() || client_id.len() > 512 {
        return Err("Invalid OAuth client identifier.".to_string());
    }

    if !valid_redirect_uri(redirect_uri) {
        return Err("Unsafe or invalid OAuth redirect URI.".to_string());
    }

    if !valid_resource(resource) {
        return Err("Invalid OAuth MCP resource identifier.".to_string());
    }

    if !valid_pkce_challenge(code_challenge) {
        return Err("Invalid OAuth PKCE S256 challenge.".to_string());
    }

    let code = generate_token()?;
    let expires_at = now_epoch_secs()?
        .checked_add(AUTH_CODE_LIFETIME_SECS)
        .ok_or_else(|| "Could not calculate OAuth authorization-code expiry.".to_string())?;

    let mut codes = pending_codes()
        .lock()
        .map_err(|_| "OAuth authorization state is unavailable.".to_string())?;

    let now = now_epoch_secs()?;
    codes.retain(|_, pending| pending.expires_at > now);

    codes.insert(
        code.clone(),
        PendingAuthorization {
            client_id: client_id.to_string(),
            redirect_uri: redirect_uri.to_string(),
            resource: resource.to_string(),
            code_challenge: code_challenge.to_string(),
            expires_at,
        },
    );

    Ok(code)
}

pub(crate) fn redeem_authorization_code(
    code: &str,
    client_id: &str,
    redirect_uri: &str,
    resource: &str,
    code_verifier: &str,
) -> Result<bool, String> {
    let mut codes = pending_codes()
        .lock()
        .map_err(|_| "OAuth authorization state is unavailable.".to_string())?;

    // Authorization codes are single-use.
    let Some(pending) = codes.remove(code) else {
        return Ok(false);
    };

    if now_epoch_secs()? >= pending.expires_at {
        return Ok(false);
    }

    if pending.client_id != client_id
        || pending.redirect_uri != redirect_uri
        || pending.resource != resource
    {
        return Ok(false);
    }

    Ok(verify_pkce_s256(code_verifier, &pending.code_challenge))
}

pub(crate) fn revoke_tokens(app: &AppHandle) -> Result<(), String> {
    let _guard = auth_store_lock()
        .lock()
        .map_err(|_| "RepoTunnel authentication state is unavailable.".to_string())?;
    let path = auth_path(app)?;

    if fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(
            "Refusing to remove RepoTunnel authentication data through a symbolic link."
                .to_string(),
        );
    }

    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("Could not revoke RepoTunnel authentication data: {error}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        create_authorization_code, encode_hash, generate_token, hash_token,
        issue_tokens_into_store, normalize_client_registration, now_epoch_secs, parse_auth_store,
        pkce_s256_challenge, redeem_authorization_code, refresh_session_index,
        session_access_matches, valid_redirect_uri, verify_pkce_s256, verify_token,
        LegacyStoredAuth, StoredAuthStore, AUTH_STORE_VERSION,
    };

    #[test]
    fn generates_strong_url_safe_tokens() {
        let first = generate_token().unwrap();
        let second = generate_token().unwrap();

        assert_ne!(first, second);
        assert!(first.len() >= 40);
        assert!(!first.contains('='));
        assert!(!first.contains('+'));
        assert!(!first.contains('/'));
    }

    #[test]
    fn creates_and_verifies_pkce_s256_challenges() {
        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
        let challenge = pkce_s256_challenge(verifier).unwrap();

        assert!(verify_pkce_s256(verifier, &challenge));
        assert!(!verify_pkce_s256(
            "differentabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
            &challenge
        ));
    }

    #[test]
    fn rejects_invalid_pkce_verifiers() {
        assert!(pkce_s256_challenge("too-short").is_none());
        assert!(pkce_s256_challenge("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNO invalid").is_none());
    }

    #[test]
    fn validates_dynamic_client_registration() {
        let redirects = vec![
            "https://example.com/oauth/callback".to_string(),
            "http://localhost:8765/callback".to_string(),
        ];

        let (name, normalized) =
            normalize_client_registration(Some("ChatGPT"), &redirects).unwrap();

        assert_eq!(name, "ChatGPT");
        assert_eq!(normalized.len(), 2);
    }

    #[test]
    fn rejects_insecure_dynamic_client_redirects() {
        let redirects = vec!["http://example.com/oauth/callback".to_string()];

        assert!(normalize_client_registration(Some("Bad client"), &redirects).is_err());
    }

    #[test]
    fn authorization_codes_are_single_use_and_pkce_bound() {
        let verifier = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
        let challenge = pkce_s256_challenge(verifier).unwrap();

        let code = create_authorization_code(
            "test-client",
            "https://example.com/oauth/callback",
            "https://example.test/mcp",
            &challenge,
        )
        .unwrap();

        assert!(redeem_authorization_code(
            &code,
            "test-client",
            "https://example.com/oauth/callback",
            "https://example.test/mcp",
            verifier,
        )
        .unwrap());

        assert!(!redeem_authorization_code(
            &code,
            "test-client",
            "https://example.com/oauth/callback",
            "https://example.test/mcp",
            verifier,
        )
        .unwrap());
    }

    #[test]
    fn rejects_unsafe_oauth_redirect_uris() {
        assert!(valid_redirect_uri("https://example.com/callback"));
        assert!(valid_redirect_uri("http://localhost:1234/callback"));
        assert!(valid_redirect_uri("http://127.0.0.1:1234/callback"));

        assert!(!valid_redirect_uri("http://example.com/callback"));
        assert!(!valid_redirect_uri("file:///tmp/callback"));
        assert!(!valid_redirect_uri("javascript:alert(1)"));
    }

    #[test]
    fn migrates_legacy_single_session_auth_store() {
        let access_token = "legacy-access";
        let refresh_token = "legacy-refresh";
        let legacy = LegacyStoredAuth {
            version: 1,
            client_id: "legacy-client".to_string(),
            resource: "https://example.test/mcp".to_string(),
            access_token_hash: encode_hash(&hash_token(access_token)),
            access_expires_at: u64::MAX,
            refresh_token_hash: encode_hash(&hash_token(refresh_token)),
        };
        let contents = serde_json::to_vec(&legacy).unwrap();
        let store = parse_auth_store(&contents).unwrap();

        assert_eq!(store.version, AUTH_STORE_VERSION);
        assert_eq!(store.sessions.len(), 1);
        assert!(session_access_matches(
            &store.sessions[0],
            access_token,
            "https://example.test/mcp",
            1,
        )
        .unwrap());
        assert_eq!(
            refresh_session_index(
                &store,
                refresh_token,
                "legacy-client",
                "https://example.test/mcp",
            )
            .unwrap(),
            Some(0)
        );
    }

    #[test]
    fn multiple_oauth_sessions_do_not_invalidate_each_other() {
        let resource = "https://example.test/mcp";
        let mut store = StoredAuthStore::default();
        let first = issue_tokens_into_store(&mut store, "chatgpt-a", resource).unwrap();
        let second = issue_tokens_into_store(&mut store, "chatgpt-b", resource).unwrap();
        let now = now_epoch_secs().unwrap();

        assert_eq!(store.sessions.len(), 2);
        assert!(store.sessions.iter().any(|session| session_access_matches(
            session,
            &first.access_token,
            resource,
            now
        )
        .unwrap()));
        assert!(store.sessions.iter().any(|session| session_access_matches(
            session,
            &second.access_token,
            resource,
            now
        )
        .unwrap()));

        let first_index =
            refresh_session_index(&store, &first.refresh_token, "chatgpt-a", resource)
                .unwrap()
                .unwrap();
        store.sessions.remove(first_index);
        let replacement = issue_tokens_into_store(&mut store, "chatgpt-a", resource).unwrap();

        assert!(!store.sessions.iter().any(|session| session_access_matches(
            session,
            &first.access_token,
            resource,
            now
        )
        .unwrap()));
        assert!(store.sessions.iter().any(|session| session_access_matches(
            session,
            &second.access_token,
            resource,
            now
        )
        .unwrap()));
        assert!(store.sessions.iter().any(|session| {
            session_access_matches(session, &replacement.access_token, resource, now).unwrap()
        }));
        assert!(
            refresh_session_index(&store, &first.refresh_token, "chatgpt-a", resource,)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn verifies_only_the_correct_token() {
        let token = generate_token().unwrap();
        let hash = hash_token(&token);

        assert!(verify_token(&token, &hash));
        assert!(!verify_token("wrong-token", &hash));
    }
}
