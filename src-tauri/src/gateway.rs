use std::{net::TcpListener as StdTcpListener, time::Instant};

use axum::{
    body::Body,
    extract::{Form, Query, State},
    http::{header, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tauri::{AppHandle, Manager};
use tokio::sync::watch;

use crate::{
    app_state::AppState, mcp_auth, mcp_flight_recorder as flight, mcp_server::RepoTunnelMcp,
    public_tunnel,
};

#[derive(Clone)]
struct LocalRequestPolicy {
    port: u16,
    app: AppHandle,
}

#[derive(Debug, serde::Deserialize)]
struct OAuthClientRegistrationRequest {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    token_endpoint_auth_method: Option<String>,
}

fn host_is_allowed(host: &str, port: u16) -> bool {
    let host = host.trim();
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("127.0.0.1")
        || host.eq_ignore_ascii_case("[::1]")
        || host.eq_ignore_ascii_case(&format!("localhost:{port}"))
        || host.eq_ignore_ascii_case(&format!("127.0.0.1:{port}"))
        || host.eq_ignore_ascii_case(&format!("[::1]:{port}"))
}

fn origin_is_allowed(origin: &str) -> bool {
    let origin = origin.trim().to_ascii_lowercase();
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
    else {
        return false;
    };

    authority == "localhost"
        || authority.starts_with("localhost:")
        || authority == "127.0.0.1"
        || authority.starts_with("127.0.0.1:")
        || authority == "[::1]"
        || authority.starts_with("[::1]:")
}

fn secure_tunnel_request_authorized(
    is_mcp_request: bool,
    forwarded_https: bool,
    host: &str,
    port: u16,
    token: Option<&str>,
    verify_token: impl FnOnce(&str) -> bool,
) -> bool {
    is_mcp_request
        && !forwarded_https
        && host_is_allowed(host, port)
        && token.is_some_and(verify_token)
}

fn request_origin_is_authorized(origin: Option<&str>, secure_tunnel_authorized: bool) -> bool {
    origin.is_none_or(|origin| origin_is_allowed(origin) || secure_tunnel_authorized)
}

fn configured_public_host_matches(app: &AppHandle, host: &str) -> bool {
    // Keep Host validation cheap: every remote MCP request passes here, so do not
    // run provider health/status probes just to learn the configured hostname.
    let Ok(Some(config)) = public_tunnel::load_config(app) else {
        return false;
    };
    let Some(public_url) = config.public_url else {
        return false;
    };
    let Ok(parsed) = url::Url::parse(&public_url) else {
        return false;
    };
    let Some(expected) = parsed.host_str() else {
        return false;
    };
    let request_host = host
        .trim()
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or(host)
        .split(':')
        .next()
        .unwrap_or(host);
    request_host.eq_ignore_ascii_case(expected)
}

fn bearer_token(request: &Request<Body>) -> Option<&str> {
    let value = request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();

    let (scheme, token) = value.split_once(' ')?;

    if !scheme.eq_ignore_ascii_case("Bearer") {
        return None;
    }

    let token = token.trim();

    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return None;
    }

    Some(token)
}

fn unauthorized_mcp_response(public_url: &str) -> Result<Response, StatusCode> {
    let metadata_url = format!(
        "{}/.well-known/oauth-protected-resource/mcp",
        public_url.trim_end_matches('/')
    );

    let challenge = format!(r#"{} resource_metadata="{metadata_url}""#, "Bearer");

    let value = HeaderValue::from_str(&challenge).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;

    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, value);

    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));

    Ok(response)
}

// MCP Session IDs are transport capabilities. Bind each machine-authenticated
// session to the *same* human-issued credential used for initialization: a
// different machine credential must not read its SSE stream or terminate it.
// Only SHA-256 fingerprints are retained and registry entries expire.
const MAX_MACHINE_SESSIONS: usize = 256;
const MACHINE_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
#[derive(Clone)]
struct MachineSession {
    bearer_sha256: [u8; 32],
    registered_at: Instant,
}
static MACHINE_SESSIONS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::BTreeMap<String, MachineSession>>,
> = std::sync::OnceLock::new();
fn machine_sessions(
) -> &'static std::sync::Mutex<std::collections::BTreeMap<String, MachineSession>> {
    MACHINE_SESSIONS.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()))
}
fn machine_session_id(headers: &axum::http::HeaderMap) -> Option<&str> {
    let value = headers.get("mcp-session-id")?.to_str().ok()?;
    (value.len() >= 8
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(value)
}
fn machine_session_fingerprint(bearer: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bearer.as_bytes()).into()
}
fn machine_session_prune(sessions: &mut std::collections::BTreeMap<String, MachineSession>) {
    sessions.retain(|_, session| session.registered_at.elapsed() < MACHINE_SESSION_TTL);
}
fn bind_machine_session(session_id: &str, bearer: &str) -> bool {
    if session_id.len() < 8
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return false;
    }
    let Ok(mut sessions) = machine_sessions().lock() else {
        return false;
    };
    machine_session_prune(&mut sessions);
    let hash = machine_session_fingerprint(bearer);
    if let Some(previous) = sessions.get(session_id) {
        return previous.bearer_sha256 == hash;
    }
    if sessions.len() >= MAX_MACHINE_SESSIONS {
        return false;
    }
    sessions.insert(
        session_id.to_string(),
        MachineSession {
            bearer_sha256: hash,
            registered_at: Instant::now(),
        },
    );
    true
}
/// Authenticated receiver-side MCP machine-session credential fingerprint.
/// Only the outer gateway may populate MACHINE_SESSIONS after validating the
/// machine bearer, and it verifies each POST's bearer/session match before the
/// request reaches MCP. The session ID alone is NOT an import grant.
pub(crate) fn machine_session_principal(headers: &axum::http::HeaderMap) -> Option<[u8; 32]> {
    let id = machine_session_id(headers)?;
    let mut sessions = machine_sessions().lock().ok()?;
    machine_session_prune(&mut sessions);
    sessions.get(id).map(|bound| bound.bearer_sha256)
}

/// Import staging owner derived ONLY from the gateway's validated,
/// bearer-bound MCP session and one exact receiver human-approval receipt.
/// This cannot be minted from a user-supplied fingerprint or arbitrary MCP
/// argument. It still does NOT authorize transfer, journal mutation, or
/// impersonate an AI owner; an explicit authenticated import endpoint is
/// intentionally not enabled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MachineSourceOwner {
    pub(crate) credential_fingerprint: String,
    pub(crate) session_binding_sha256: String,
    pub(crate) stage_owner: String,
}

pub(crate) fn machine_source_owner(
    headers: &axum::http::HeaderMap,
    approval_id: &str,
) -> Option<MachineSourceOwner> {
    use sha2::{Digest, Sha256};
    let suffix = approval_id.strip_prefix("receiver-source-")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let session = machine_session_id(headers)?;
    // Registered only during a successful machine init and rechecked on
    // EVERY machine POST by the outer gateway. The caller cannot choose
    // the registered bearer digest or cross over from OAuth.
    let credential = machine_session_principal(headers)?;
    let credential_fingerprint = credential
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut bind = Sha256::new();
    bind.update(b"RepoTunnel-human-source-session-v1\0");
    bind.update((session.len() as u64).to_be_bytes());
    bind.update(session.as_bytes());
    bind.update(credential);
    let session_binding = bind.finalize();
    let session_binding_sha256 = session_binding
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut owner = Sha256::new();
    owner.update(b"RepoTunnel-human-source-owner-v1\0");
    owner.update(session_binding);
    owner.update(approval_id.as_bytes());
    let stage_owner = format!(
        "human-source-v1:{}",
        owner
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    Some(MachineSourceOwner {
        credential_fingerprint,
        session_binding_sha256,
        stage_owner,
    })
}
fn machine_session_owned(session_id: &str, bearer: &str) -> bool {
    let Ok(mut sessions) = machine_sessions().lock() else {
        return false;
    };
    machine_session_prune(&mut sessions);
    sessions
        .get(session_id)
        .is_some_and(|bound| bound.bearer_sha256 == machine_session_fingerprint(bearer))
}
// This guard applies to EVERY authenticated MCP path, including OAuth and
// private-tunnel requests. A machine-owned session cannot be repurposed under
// a different bearer just because that other path has valid authentication.
fn machine_session_not_owned_by_other_bearer(
    headers: &axum::http::HeaderMap,
    bearer: Option<&str>,
) -> bool {
    if !headers.contains_key("mcp-session-id") {
        return true;
    }
    let Some(id) = machine_session_id(headers) else {
        return false;
    };
    let Ok(mut sessions) = machine_sessions().lock() else {
        return false;
    };
    machine_session_prune(&mut sessions);
    match sessions.get(id) {
        Some(bound) => {
            bearer.is_some_and(|value| bound.bearer_sha256 == machine_session_fingerprint(value))
        }
        None => true,
    }
}

fn machine_session_request_permitted(
    method: &axum::http::Method,
    headers: &axum::http::HeaderMap,
    bearer: &str,
) -> bool {
    // Only initialization may start a new session; regular session requests,
    // including GET/SSE and DELETE, must have a known credential-bound ID.
    match machine_session_id(headers) {
        Some(session) => machine_session_owned(session, bearer),
        None => *method == axum::http::Method::POST && !headers.contains_key("mcp-session-id"),
    }
}

fn mcp_request_path(path: &str) -> bool {
    path == "/mcp" || path.starts_with("/mcp/")
}

/// Machine credentials are explicitly weaker than normal user OAuth.
/// Apply the same allowlist as the coordinator *at the receiving gateway*:
/// a client holding a machine credential cannot bypass the coordinator and
/// invoke local-only approvals, Git publishing, credential issuance, etc.
fn machine_mcp_message_allowed(message: &serde_json::Value) -> bool {
    let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
        return false;
    };
    match method {
        "initialize"
        | "ping"
        | "notifications/initialized"
        | "notifications/cancelled"
        | "tools/list" => true,
        "tools/call" => message
            .get("params")
            .and_then(|params| params.get("name"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| {
                crate::remote_fleet::allowed(name)
                    || matches!(
                        name,
                        "create_machine_source_import_stage"
                            | "stage_machine_source_import_chunk"
                            | "execute_machine_source_import"
                            | "inspect_machine_source_import_task"
                            | "inspect_machine_source_import_chunk"
                    )
            }),
        _ => false,
    }
}
async fn machine_mcp_request_scope(
    app: &AppHandle,
    bearer: &str,
    request: Request<Body>,
) -> Result<Request<Body>, StatusCode> {
    use axum::http::Method;
    if matches!(*request.method(), Method::GET | Method::DELETE) {
        return Ok(request);
    }
    if request.method() != Method::POST {
        return Err(StatusCode::METHOD_NOT_ALLOWED);
    }
    let (parts, body) = request.into_parts();
    // This bound includes 256 KiB binary source chunks and existing bounded
    // file edits, but rejects arbitrary large JSON-RPC requests.
    let bytes = axum::body::to_bytes(body, 5 * 1024 * 1024)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    let message: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    if !machine_mcp_message_allowed(&message) {
        return Err(StatusCode::FORBIDDEN);
    }
    if !parts.headers.contains_key("mcp-session-id")
        && message.get("method").and_then(serde_json::Value::as_str) != Some("initialize")
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let app = app.clone();
    let token = bearer.to_owned();
    let payload = bytes.clone();
    let verified = tokio::task::spawn_blocking(move || {
        crate::remote_access::verify_rpc(&app, &token, &payload)
    })
    .await
    .unwrap_or(false);
    if !verified {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Request::from_parts(parts, Body::from(bytes)))
}

// Outermost gateway layer: records the status even when the authorization
// guard returns early. The response-header duration does NOT measure SSE body
// completion or client round-trip time.
async fn record_gateway_request(
    State(policy): State<LocalRequestPolicy>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let started = Instant::now();
    let route = flight::route(request.uri().path());
    let method = flight::method(request.method().as_str());
    let response = next.run(request).await;
    flight::record(
        &policy.app,
        flight::Surface::Gateway,
        route,
        method,
        flight::Stage::ResponseHeaders,
        response.status().as_u16(),
        started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    );
    response
}

async fn local_request_guard(
    State(policy): State<LocalRequestPolicy>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let mut request = request;
    let mut machine_initializing_bearer: Option<String> = None;
    let Some(host) = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let forwarded_https = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"));

    let public_host_allowed = if forwarded_https && !host_is_allowed(host, policy.port) {
        let app = policy.app.clone();
        let host = host.to_owned();
        tokio::task::spawn_blocking(move || configured_public_host_matches(&app, &host))
            .await
            .unwrap_or(false)
    } else {
        false
    };
    if !host_is_allowed(host, policy.port) && !public_host_allowed {
        return Err(StatusCode::FORBIDDEN);
    }

    let is_mcp_request = mcp_request_path(request.uri().path());
    let secure_tunnel_token = request
        .headers()
        .get("x-repotunnel-secure-token")
        .and_then(|value| value.to_str().ok());
    let secure_tunnel_authorized = secure_tunnel_request_authorized(
        is_mcp_request,
        forwarded_https,
        host,
        policy.port,
        secure_tunnel_token,
        |token| {
            policy
                .app
                .state::<AppState>()
                .verify_secure_tunnel_token(token)
        },
    );
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());

    // ChatGPT may forward its Origin header through the tunnel. Permit
    // that only on /mcp requests bearing this process-bound secret; all
    // ordinary local browser and public requests retain the origin check.
    if !request_origin_is_authorized(origin, secure_tunnel_authorized) {
        return Err(StatusCode::FORBIDDEN);
    }

    if is_mcp_request && !secure_tunnel_authorized {
        // A locally issued machine credential is independently authorized.
        // It must work with any separately configured trusted HTTPS proxy,
        // even when ChatGPT public OAuth has not been configured. Do not
        // bypass the existing host/origin guards or the remote AI Pause gate.
        let machine_authorized = if let Some(token) = bearer_token(&request) {
            let app = policy.app.clone();
            let token = token.to_owned();
            tokio::task::spawn_blocking(move || {
                // Every MCP method, including SSE/DELETE, requires a still-
                // approved project and at least one valid category grant.
                crate::remote_access::verify_rpc(
                    &app,
                    &token,
                    br#"{"jsonrpc":"2.0","method":"ping"}"#,
                )
            })
            .await
            .unwrap_or(false)
        } else {
            false
        };

        if !machine_authorized {
            // Non-machine clients still use the normal public OAuth flow.
            let app = policy.app.clone();
            let config = tokio::task::spawn_blocking(move || public_tunnel::load_config(&app))
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
                .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
            let public_url = config.public_url.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
            let resource = format!("{}/mcp", public_url.trim_end_matches('/'));
            let authorized = if let Some(token) = bearer_token(&request) {
                let app = policy.app.clone();
                let token = token.to_owned();
                tokio::task::spawn_blocking(move || {
                    mcp_auth::verify_access_token(&app, &token, &resource).unwrap_or(false)
                })
                .await
                .unwrap_or(false)
            } else {
                false
            };
            if !authorized {
                return unauthorized_mcp_response(&public_url);
            }
        }

        if machine_authorized {
            // Bind MCP GET/SSE, DELETE and subsequent POSTs to the exact
            // credential that initiated the session, never a different grant.
            let bearer = bearer_token(&request)
                .ok_or(StatusCode::UNAUTHORIZED)?
                .to_owned();
            if !machine_session_request_permitted(request.method(), request.headers(), &bearer) {
                return Err(StatusCode::FORBIDDEN);
            }
            if request.method() == axum::http::Method::POST
                && !request.headers().contains_key("mcp-session-id")
            {
                machine_initializing_bearer = Some(bearer.clone());
            }
            // The remote endpoint must enforce the same bounded tool authority
            // as the coordinator; otherwise a stolen machine credential could
            // be used with a different MCP client to bypass its allowlist.
            let bearer = bearer_token(&request)
                .ok_or(StatusCode::UNAUTHORIZED)?
                .to_owned();
            request = machine_mcp_request_scope(&policy.app, &bearer, request).await?;
        }

        if forwarded_https {
            policy.app.state::<AppState>().record_remote_request();
        }
    }

    // OAuth or private-tunnel credentials must never borrow a machine-owned
    // MCP session, even if their own authorization is otherwise valid.
    if is_mcp_request
        && !machine_session_not_owned_by_other_bearer(request.headers(), bearer_token(&request))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let response = next.run(request).await;
    if let Some(bearer) = machine_initializing_bearer {
        // On a successful initialize, rmcp creates a session ID in the
        // response headers. Do not hand out a usable unbound machine session.
        if response.status().is_success() {
            let session_id =
                machine_session_id(response.headers()).ok_or(StatusCode::BAD_GATEWAY)?;
            if !bind_machine_session(session_id, &bearer) {
                return Err(StatusCode::FORBIDDEN);
            }
        }
    }
    Ok(response)
}

async fn register_oauth_client(
    State(policy): State<LocalRequestPolicy>,
    Json(request): Json<OAuthClientRegistrationRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    if request
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|value| value != "none")
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_client_metadata",
                "error_description": "RepoTunnel supports public OAuth clients only."
            })),
        ));
    }

    if let Some(grants) = request.grant_types.as_ref() {
        if !grants.iter().any(|value| value == "authorization_code")
            || grants
                .iter()
                .any(|value| value != "authorization_code" && value != "refresh_token")
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_client_metadata",
                    "error_description": "Unsupported OAuth grant type."
                })),
            ));
        }
    }

    if let Some(types) = request.response_types.as_ref() {
        if types.is_empty() || types.iter().any(|value| value != "code") {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid_client_metadata",
                    "error_description": "RepoTunnel supports response_type=code."
                })),
            ));
        }
    }

    let app = policy.app.clone();
    let client_name = request.client_name.clone();
    let redirect_uris = request.redirect_uris.clone();
    let client = tokio::task::spawn_blocking(move || {
        mcp_auth::register_client(&app, client_name.as_deref(), &redirect_uris)
    })
    .await
    .map_err(|_| StatusCode::BAD_REQUEST)
    .and_then(|result| result.map_err(|_| StatusCode::BAD_REQUEST))
    .map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_client_metadata",
                "error_description": "RepoTunnel rejected the OAuth client registration."
            })),
        )
    })?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "client_id": client.client_id,
            "client_id_issued_at": client.issued_at,
            "client_name": client.client_name,
            "redirect_uris": client.redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none"
        })),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct OAuthAuthorizeQuery {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: String,
    code_challenge_method: String,
    resource: String,
}

fn oauth_redirect(
    redirect_uri: &str,
    values: &[(&str, &str)],
) -> Result<Redirect, (StatusCode, String)> {
    let mut uri = url::Url::parse(redirect_uri).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "Invalid OAuth redirect URI.".to_string(),
        )
    })?;

    {
        let mut pairs = uri.query_pairs_mut();
        for (key, value) in values {
            pairs.append_pair(key, value);
        }
    }

    Ok(Redirect::to(uri.as_str()))
}

async fn authorize_oauth_client(
    State(policy): State<LocalRequestPolicy>,
    Query(request): Query<OAuthAuthorizeQuery>,
) -> Result<Redirect, (StatusCode, String)> {
    if request.response_type != "code" {
        return Err((
            StatusCode::BAD_REQUEST,
            "RepoTunnel supports response_type=code only.".to_string(),
        ));
    }

    if request.code_challenge_method != "S256" {
        return Err((
            StatusCode::BAD_REQUEST,
            "RepoTunnel requires PKCE code_challenge_method=S256.".to_string(),
        ));
    }

    if request
        .state
        .as_ref()
        .is_some_and(|state| state.len() > 1024)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "OAuth state value is too large.".to_string(),
        ));
    }

    let app = policy.app.clone();
    let client_id = request.client_id.clone();
    let redirect_uri = request.redirect_uri.clone();
    let client = tokio::task::spawn_blocking(move || {
        mcp_auth::registered_client_for_redirect(&app, &client_id, &redirect_uri)
    })
    .await
    .map_err(|_| ())
    .and_then(|result| result.map_err(|_| ()))
    .map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "Could not validate the OAuth client.".to_string(),
        )
    })?
    .ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "OAuth client or redirect URI is not registered.".to_string(),
        )
    })?;

    // Bind authorization to this installation's current public MCP resource.
    let current_resource = policy
        .app
        .state::<AppState>()
        .public_tunnel_status(&policy.app)
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "RepoTunnel public connection status is unavailable.".to_string(),
            )
        })?
        .mcp_url
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "RepoTunnel public MCP connection is not available.".to_string(),
            )
        })?;

    if request.resource != current_resource {
        return Err((
            StatusCode::BAD_REQUEST,
            "OAuth resource does not match this RepoTunnel MCP endpoint.".to_string(),
        ));
    }

    let app = policy.app.clone();
    let client_name = client.client_name.clone();
    let resource = request.resource.clone();

    let approved = tokio::task::spawn_blocking(move || {
        mcp_auth::request_pairing_approval(&app, &client_name, &resource)
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "RepoTunnel could not complete the local pairing approval.".to_string(),
        )
    })?;

    if !approved {
        let mut values = vec![("error", "access_denied")];

        if let Some(state) = request.state.as_deref() {
            values.push(("state", state));
        }

        return oauth_redirect(&request.redirect_uri, &values);
    }

    let code = mcp_auth::create_authorization_code(
        &request.client_id,
        &request.redirect_uri,
        &request.resource,
        &request.code_challenge,
    )
    .map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "RepoTunnel could not create the OAuth authorization code.".to_string(),
        )
    })?;

    let mut values = vec![("code", code.as_str())];

    if let Some(state) = request.state.as_deref() {
        values.push(("state", state));
    }

    oauth_redirect(&request.redirect_uri, &values)
}

#[derive(Debug, serde::Deserialize)]
struct OAuthTokenRequest {
    grant_type: String,
    client_id: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: String,
}

fn oauth_token_error(error: &str, description: &str) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "error": error,
            "error_description": description,
        })),
    )
}

async fn exchange_oauth_token(
    State(policy): State<LocalRequestPolicy>,
    Form(request): Form<OAuthTokenRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Tokens must always be bound to this installation's current MCP resource.
    let current_resource = policy
        .app
        .state::<AppState>()
        .public_tunnel_status(&policy.app)
        .map_err(|_| {
            oauth_token_error(
                "temporarily_unavailable",
                "RepoTunnel public connection status is unavailable.",
            )
        })?
        .mcp_url
        .ok_or_else(|| {
            oauth_token_error(
                "temporarily_unavailable",
                "RepoTunnel public MCP connection is unavailable.",
            )
        })?;

    if request.resource != current_resource {
        return Err(oauth_token_error(
            "invalid_target",
            "OAuth resource does not match this RepoTunnel MCP endpoint.",
        ));
    }

    let tokens = match request.grant_type.as_str() {
        "authorization_code" => {
            let code = request.code.as_deref().ok_or_else(|| {
                oauth_token_error("invalid_request", "Missing authorization code.")
            })?;

            let redirect_uri = request
                .redirect_uri
                .as_deref()
                .ok_or_else(|| oauth_token_error("invalid_request", "Missing redirect_uri."))?;

            let verifier = request.code_verifier.as_deref().ok_or_else(|| {
                oauth_token_error("invalid_request", "Missing PKCE code_verifier.")
            })?;

            let app = policy.app.clone();
            let code = code.to_owned();
            let redirect_uri = redirect_uri.to_owned();
            let verifier = verifier.to_owned();
            let client_id = request.client_id.clone();
            let resource = request.resource.clone();
            // Once dispatched, code redemption and token issuance complete together
            // on the blocking pool, even if the HTTP request is cancelled.
            let issued = tokio::task::spawn_blocking(move || {
                match mcp_auth::redeem_authorization_code(
                    &code,
                    &client_id,
                    &redirect_uri,
                    &resource,
                    &verifier,
                ) {
                    Ok(true) => mcp_auth::issue_tokens(&app, &client_id, &resource)
                        .map(Some)
                        .map_err(|_| "issue"),
                    Ok(false) => Ok(None),
                    Err(_) => Err("validate"),
                }
            })
            .await
            .map_err(|_| {
                oauth_token_error(
                    "server_error",
                    "RepoTunnel could not complete the OAuth exchange.",
                )
            })?;
            match issued {
                Ok(Some(tokens)) => tokens,
                Ok(None) => {
                    return Err(oauth_token_error(
                        "invalid_grant",
                        "Authorization code is invalid, expired, already used, or failed PKCE validation.",
                    ));
                }
                Err("validate") => {
                    return Err(oauth_token_error(
                        "invalid_grant",
                        "RepoTunnel could not validate the authorization code.",
                    ));
                }
                Err(_) => {
                    return Err(oauth_token_error(
                        "server_error",
                        "RepoTunnel could not issue authentication tokens.",
                    ));
                }
            }
        }

        "refresh_token" => {
            let refresh_token = request
                .refresh_token
                .as_deref()
                .ok_or_else(|| oauth_token_error("invalid_request", "Missing refresh_token."))?;

            let app = policy.app.clone();
            let refresh_token = refresh_token.to_owned();
            let client_id = request.client_id.clone();
            let resource = request.resource.clone();
            tokio::task::spawn_blocking(move || {
                mcp_auth::rotate_refresh_token(&app, &refresh_token, &client_id, &resource)
            })
            .await
            .map_err(|_| ())
            .and_then(|result| result.map_err(|_| ()))
            .map_err(|_| {
                oauth_token_error(
                    "invalid_grant",
                    "RepoTunnel could not validate the refresh token.",
                )
            })?
            .ok_or_else(|| {
                oauth_token_error(
                    "invalid_grant",
                    "Refresh token is invalid or has already been rotated.",
                )
            })?
        }

        _ => {
            return Err(oauth_token_error(
                "unsupported_grant_type",
                "RepoTunnel supports authorization_code and refresh_token grants.",
            ));
        }
    };

    Ok(Json(serde_json::json!({
        "access_token": tokens.access_token,
        "token_type": "Bearer",
        "expires_in": tokens.expires_in,
        "refresh_token": tokens.refresh_token
    })))
}

fn current_public_oauth_urls(policy: &LocalRequestPolicy) -> Result<(String, String), StatusCode> {
    let status = policy
        .app
        .state::<AppState>()
        .public_tunnel_status(&policy.app)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let public_url = status.public_url.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mcp_url = status.mcp_url.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    Ok((public_url.trim_end_matches('/').to_string(), mcp_url))
}

async fn oauth_protected_resource_metadata(
    State(policy): State<LocalRequestPolicy>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (issuer, resource) = current_public_oauth_urls(&policy)?;

    Ok(Json(serde_json::json!({
        "resource": resource,
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"]
    })))
}

async fn oauth_authorization_server_metadata(
    State(policy): State<LocalRequestPolicy>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (issuer, _) = current_public_oauth_urls(&policy)?;

    Ok(Json(serde_json::json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"]
    })))
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "RepoTunnel",
        "mcp": "streamable-http",
        "endpoint": "/mcp",
    }))
}

fn mcp_streamable_http_config() -> StreamableHttpServerConfig {
    StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(false)
        .with_sse_keep_alive(Some(std::time::Duration::from_secs(5)))
}

pub(crate) async fn serve(
    listener: StdTcpListener,
    port: u16,
    app: AppHandle,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), String> {
    // Restore bounded prior diagnostics at startup, not on the hot request path.
    flight::initialize(&app);
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("Could not configure the local gateway socket: {error}"))?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|error| format!("Could not initialize the local gateway socket: {error}"))?;

    let service_app = app.clone();
    let mcp_service: StreamableHttpService<RepoTunnelMcp, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(RepoTunnelMcp::new(service_app.clone())),
            LocalSessionManager::default().into(),
            mcp_streamable_http_config(),
        );

    let policy = LocalRequestPolicy {
        port,
        app: app.clone(),
    };

    let router = Router::new()
        .route("/health", get(health))
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth_protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth_protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth_authorization_server_metadata),
        )
        .route("/register", post(register_oauth_client))
        .route("/authorize", get(authorize_oauth_client))
        .route("/token", post(exchange_oauth_token))
        .nest_service("/mcp", mcp_service)
        .with_state(policy.clone())
        .layer(middleware::from_fn_with_state(
            policy.clone(),
            local_request_guard,
        ))
        .layer(middleware::from_fn_with_state(
            policy,
            record_gateway_request,
        ));

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            loop {
                if *shutdown.borrow() {
                    break;
                }
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .map_err(|error| format!("The local MCP gateway stopped unexpectedly: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        bind_machine_session, host_is_allowed, machine_mcp_message_allowed,
        machine_session_fingerprint, machine_session_not_owned_by_other_bearer,
        machine_session_owned, machine_session_principal, machine_session_request_permitted,
        machine_source_owner, mcp_request_path, origin_is_allowed, request_origin_is_authorized,
        secure_tunnel_request_authorized,
    };

    #[test]
    fn mcp_auth_guard_covers_local_and_forwarded_mcp_paths() {
        assert!(mcp_request_path("/mcp"));
        assert!(mcp_request_path("/mcp/"));
        assert!(mcp_request_path("/mcp/session"));
        assert!(!mcp_request_path("/health"));
        assert!(!mcp_request_path("/authorize"));
        assert!(!mcp_request_path("/mcp-unrelated"));
    }

    #[test]
    fn accepts_loopback_hosts() {
        assert!(host_is_allowed("127.0.0.1:42100", 42100));
        assert!(host_is_allowed("localhost:42100", 42100));
        assert!(host_is_allowed("[::1]:42100", 42100));
    }

    #[test]
    fn rejects_non_loopback_hosts() {
        assert!(!host_is_allowed("example.com:42100", 42100));
        assert!(!host_is_allowed("127.0.0.1.evil.test:42100", 42100));
    }

    #[test]
    fn accepts_only_loopback_web_origins() {
        assert!(origin_is_allowed("http://localhost:6274"));
        assert!(origin_is_allowed("http://127.0.0.1:6274"));
        assert!(origin_is_allowed("https://[::1]:6274"));
        assert!(!origin_is_allowed("https://example.com"));
        assert!(!origin_is_allowed("null"));
    }

    #[test]
    fn private_secure_tunnel_auth_requires_a_local_mcp_request_and_live_token() {
        let verify_good_token = |candidate: &str| candidate == "runtime-secret";
        assert!(secure_tunnel_request_authorized(
            true,
            false,
            "127.0.0.1:42100",
            42100,
            Some("runtime-secret"),
            verify_good_token,
        ));
        assert!(!secure_tunnel_request_authorized(
            true,
            false,
            "127.0.0.1:42100",
            42100,
            Some("wrong-secret"),
            |candidate| candidate == "runtime-secret",
        ));
        assert!(!secure_tunnel_request_authorized(
            false,
            false,
            "127.0.0.1:42100",
            42100,
            Some("runtime-secret"),
            |candidate| candidate == "runtime-secret",
        ));
        assert!(!secure_tunnel_request_authorized(
            true,
            true,
            "127.0.0.1:42100",
            42100,
            Some("runtime-secret"),
            |candidate| candidate == "runtime-secret",
        ));
        assert!(!secure_tunnel_request_authorized(
            true,
            false,
            "mcp.example.com",
            42100,
            Some("runtime-secret"),
            |candidate| candidate == "runtime-secret",
        ));
    }

    #[test]
    fn only_the_private_tunnel_token_can_bypass_the_chatgpt_origin_check() {
        assert!(request_origin_is_authorized(None, false));
        assert!(request_origin_is_authorized(
            Some("http://localhost:6274"),
            false
        ));
        assert!(!request_origin_is_authorized(
            Some("https://chatgpt.com"),
            false
        ));
        assert!(request_origin_is_authorized(
            Some("https://chatgpt.com"),
            true
        ));
        assert!(!request_origin_is_authorized(Some("null"), false));
    }

    #[test]
    fn machine_gateway_lists_read_only_approval_inspection_but_never_local_human_mutations() {
        let status = serde_json::json!({
            "jsonrpc":"2.0","method":"tools/call","id":1,
            "params":{"name":"inspect_machine_source_review",
                "arguments":{"workspace_id":"approved-project",
                    "approval_id":format!("receiver-source-{}", "a".repeat(32))}}
        });
        assert!(machine_mcp_message_allowed(&status));
        for name in [
            "approve_incoming_source_review_desktop",
            "withdraw_incoming_source_approval_desktop",
            "mark_dispatch_unknown_at",
            "record_remote_source_review_intent_desktop",
        ] {
            assert!(!machine_mcp_message_allowed(&serde_json::json!({
                "jsonrpc":"2.0","method":"tools/call","id":2,
                "params":{"name":name,"arguments":{"workspace_id":"approved-project"}}
            })));
        }
    }

    #[test]
    fn machine_mcp_sessions_block_cross_credential_get_delete_and_post() {
        use axum::http::{HeaderMap, HeaderValue, Method};
        let sid = "session-test-machine-project-scope-a";
        let mut headers = HeaderMap::new();
        assert!(machine_session_request_permitted(
            &Method::POST,
            &headers,
            "grant-a"
        ));
        assert!(!machine_session_request_permitted(
            &Method::GET,
            &headers,
            "grant-a"
        ));
        assert!(!machine_session_request_permitted(
            &Method::DELETE,
            &headers,
            "grant-a"
        ));
        assert!(bind_machine_session(sid, "grant-a"));
        assert!(!bind_machine_session(sid, "grant-b"));
        headers.insert("mcp-session-id", HeaderValue::from_static(sid));
        for method in [Method::GET, Method::POST, Method::DELETE] {
            assert!(machine_session_request_permitted(
                &method, &headers, "grant-a"
            ));
            assert!(!machine_session_request_permitted(
                &method, &headers, "grant-b"
            ));
        }
        assert!(machine_session_owned(sid, "grant-a"));
        assert!(!machine_session_owned(sid, "grant-b"));
        assert_eq!(
            machine_session_principal(&headers),
            Some(machine_session_fingerprint("grant-a")),
            "receiver sees only the gateway-verified bearer digest"
        );
        let unknown = HeaderMap::new();
        assert!(machine_session_principal(&unknown).is_none());
        let mut unbound = HeaderMap::new();
        unbound.insert(
            "mcp-session-id",
            HeaderValue::from_static("unknown-session-other"),
        );
        assert!(machine_session_principal(&unbound).is_none());
        // Import source owners must be derived from this *registered*
        // credential/session pair plus one exact human receiver approval.
        let approval_id = format!("receiver-source-{}", "a".repeat(32));
        let bound = machine_source_owner(&headers, &approval_id)
            .expect("real bound machine session derives owner without AI impersonation");
        assert_eq!(
            bound.credential_fingerprint,
            machine_session_fingerprint("grant-a")
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        assert!(bound.stage_owner.starts_with("human-source-v1:"));
        assert_eq!(
            machine_source_owner(&headers, &approval_id),
            Some(bound.clone())
        );
        assert_ne!(
            machine_source_owner(&headers, &format!("receiver-source-{}", "b".repeat(32)))
                .unwrap()
                .stage_owner,
            bound.stage_owner,
            "different signed receiver review gets a distinct staging owner"
        );
        assert!(machine_source_owner(&headers, "not-an-approval-id").is_none());
        let another_session = "source-import-different-session-identity";
        assert!(bind_machine_session(another_session, "grant-a"));
        let mut later = HeaderMap::new();
        later.insert("mcp-session-id", HeaderValue::from_static(another_session));
        let shifted = machine_source_owner(&later, &approval_id).unwrap();
        assert_eq!(shifted.credential_fingerprint, bound.credential_fingerprint);
        assert_ne!(shifted.session_binding_sha256, bound.session_binding_sha256);
        assert_ne!(shifted.stage_owner, bound.stage_owner);
        assert!(machine_source_owner(&unknown, &approval_id).is_none());
        assert!(machine_source_owner(&unbound, &approval_id).is_none());
        // Other valid authentication paths cannot attach to a machine-owned
        // session by presenting its session ID under a different credential.
        assert!(machine_session_not_owned_by_other_bearer(
            &headers,
            Some("grant-a")
        ));
        assert!(!machine_session_not_owned_by_other_bearer(
            &headers,
            Some("oauth-token")
        ));
        assert!(!machine_session_not_owned_by_other_bearer(&headers, None));
        headers.insert("mcp-session-id", HeaderValue::from_static("!invalid"));
        assert!(!machine_session_not_owned_by_other_bearer(
            &headers,
            Some("grant-a")
        ));
        assert!(!machine_session_request_permitted(
            &Method::GET,
            &headers,
            "grant-a"
        ));
        assert!(!machine_session_request_permitted(
            &Method::POST,
            &headers,
            "grant-a"
        ));
        let mut sessions = super::machine_sessions().lock().unwrap();
        sessions.get_mut(sid).unwrap().registered_at =
            std::time::Instant::now() - std::time::Duration::from_secs(24 * 60 * 60 + 1);
        drop(sessions);
        assert!(!machine_session_owned(sid, "grant-a"));
    }

    #[tokio::test]
    async fn real_mcp_sdk_uses_machine_bound_session_headers() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
            middleware::{self, Next},
        };
        use rmcp::{
            handler::server::ServerHandler,
            model::ServerInfo,
            tool, tool_router,
            transport::{
                streamable_http_client::{
                    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
                },
                streamable_http_server::{
                    session::local::LocalSessionManager, StreamableHttpServerConfig,
                    StreamableHttpService,
                },
            },
            ServiceExt,
        };
        #[derive(Clone)]
        struct Demo;
        #[tool_router]
        impl Demo {
            #[tool(description = "Minimal authenticated MCP session health fixture")]
            fn probe(&self) -> String {
                "verified-session".into()
            }
        }
        #[rmcp::tool_handler]
        impl ServerHandler for Demo {
            fn get_info(&self) -> ServerInfo {
                ServerInfo::default()
            }
        }
        let service: StreamableHttpService<Demo, LocalSessionManager> = StreamableHttpService::new(
            || Ok(Demo),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default().with_json_response(false),
        );
        let router = axum::Router::new()
            .nest_service("/mcp", service)
            .layer(middleware::from_fn(
                |request: Request<Body>, next: Next| async move {
                    let bearer = super::bearer_token(&request)
                        .ok_or(StatusCode::UNAUTHORIZED)?
                        .to_string();
                    if !super::machine_session_request_permitted(
                        request.method(),
                        request.headers(),
                        &bearer,
                    ) {
                        return Err::<axum::response::Response, StatusCode>(StatusCode::FORBIDDEN);
                    }
                    let initial = request.method() == axum::http::Method::POST
                        && !request.headers().contains_key("mcp-session-id");
                    let request = if initial {
                        let (parts, body) = request.into_parts();
                        let bytes = axum::body::to_bytes(body, 8192)
                            .await
                            .map_err(|_| StatusCode::BAD_REQUEST)?;
                        let json: serde_json::Value =
                            serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
                        if json.get("method").and_then(serde_json::Value::as_str)
                            != Some("initialize")
                        {
                            return Err(StatusCode::FORBIDDEN);
                        }
                        Request::from_parts(parts, Body::from(bytes))
                    } else {
                        request
                    };
                    let response = next.run(request).await;
                    if initial && response.status().is_success() {
                        let session = super::machine_session_id(response.headers())
                            .ok_or(StatusCode::BAD_GATEWAY)?;
                        if !super::bind_machine_session(session, &bearer) {
                            return Err(StatusCode::FORBIDDEN);
                        }
                    }
                    Ok(response)
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        // Loopback HTTP is test-only: production machine pairing insists on HTTPS.
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint)
                .auth_header("test-session-bound-bearer"),
        );
        let client = rmcp::model::ClientInfo::default()
            .serve(transport)
            .await
            .unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|tool| tool.name == "probe"));
        let call = rmcp::model::CallToolRequestParams::new("probe".to_owned());
        let result = client.call_tool(call).await.unwrap();
        assert!(!result.is_error.unwrap_or(false));
        server.abort();
    }

    #[test]
    fn machine_bearer_is_scoped_at_receiver_not_just_at_coordinator() {
        use serde_json::json;
        for name in [
            "browser_action",
            "ai_workspace_session",
            "run_terminal_command",
            "stage_remote_source_chunk",
            "inspect_remote_source_stage",
            "create_machine_source_import_stage",
            "stage_machine_source_import_chunk",
            "execute_machine_source_import",
            "inspect_machine_source_import_task",
            "inspect_machine_source_import_chunk",
            "temp_workspace_file_action",
            "get_remote_source_file_status",
        ] {
            assert!(
                machine_mcp_message_allowed(&json!({
                    "jsonrpc":"2.0","id":7,"method":"tools/call",
                    "params":{"name":name,"arguments":{}}
                })),
                "Expected bounded remote machine capability: {name}"
            );
        }
        for name in [
            "issue_remote_machine_access",
            "revoke_remote_machine_access",
            "github_push",
            "agent_mission_decision",
            "set_ai_access_paused",
            "remote_machine_call",
            "advance_remote_project_sync",
            "approve_change",
            "delete_workspace",
        ] {
            assert!(
                !machine_mcp_message_allowed(&json!({
                    "jsonrpc":"2.0","id":8,"method":"tools/call",
                    "params":{"name":name,"arguments":{}}
                })),
                "Machine credential must not grant local-only authority: {name}"
            );
        }
        for method in [
            "initialize",
            "notifications/initialized",
            "ping",
            "tools/list",
        ] {
            assert!(machine_mcp_message_allowed(
                &json!({"jsonrpc":"2.0","method":method})
            ));
        }
        for payload in [
            json!({"jsonrpc":"2.0","id":2,"method":"resources/read",
                "params":{"uri":"file:///private"}}),
            json!({"jsonrpc":"2.0","method":"prompts/get"}),
            json!({"jsonrpc":"2.0","method":"tools/call",
                "params":{"name":"remote_machine_call"}}),
            json!([{"jsonrpc":"2.0","method":"initialize"}]),
            json!({"jsonrpc":"2.0","method":"tools/call"}),
            json!({"jsonrpc":"2.0","method":"tools/call",
                "params":{"name":42}}),
        ] {
            assert!(!machine_mcp_message_allowed(&payload));
        }
    }

    #[test]
    fn long_mcp_tool_calls_use_sse_keep_alive_instead_of_buffered_json() {
        let config = super::mcp_streamable_http_config();
        assert!(!config.legacy_session_mode);
        assert!(!config.json_response);
        assert_eq!(
            config.sse_keep_alive,
            Some(std::time::Duration::from_secs(5))
        );
    }
}
