use std::net::TcpListener as StdTcpListener;

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
use tokio::sync::oneshot;

use crate::{app_state::AppState, mcp_auth, mcp_server::RepoTunnelMcp, public_tunnel};

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

async fn local_request_guard(
    State(policy): State<LocalRequestPolicy>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
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

    if !host_is_allowed(host, policy.port)
        && !(forwarded_https && configured_public_host_matches(&policy.app, host))
    {
        return Err(StatusCode::FORBIDDEN);
    }

    if let Some(origin) = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        if !origin_is_allowed(origin) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    if forwarded_https && request.uri().path().starts_with("/mcp") {
        let config = public_tunnel::load_config(&policy.app)
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let public_url = config.public_url.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let resource = format!("{}/mcp", public_url.trim_end_matches('/'));

        let authorized = bearer_token(&request)
            .map(|token| {
                mcp_auth::verify_access_token(&policy.app, token, &resource).unwrap_or(false)
            })
            .unwrap_or(false);

        if !authorized {
            return unauthorized_mcp_response(&public_url);
        }

        policy.app.state::<AppState>().record_remote_request();
    }

    Ok(next.run(request).await)
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

    let client = mcp_auth::register_client(
        &policy.app,
        request.client_name.as_deref(),
        &request.redirect_uris,
    )
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

    let client = mcp_auth::registered_client_for_redirect(
        &policy.app,
        &request.client_id,
        &request.redirect_uri,
    )
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

            let valid = mcp_auth::redeem_authorization_code(
                code,
                &request.client_id,
                redirect_uri,
                &request.resource,
                verifier,
            )
            .map_err(|_| {
                oauth_token_error(
                    "invalid_grant",
                    "RepoTunnel could not validate the authorization code.",
                )
            })?;

            if !valid {
                return Err(oauth_token_error(
                    "invalid_grant",
                    "Authorization code is invalid, expired, already used, or failed PKCE validation.",
                ));
            }

            mcp_auth::issue_tokens(&policy.app, &request.client_id, &request.resource).map_err(
                |_| {
                    oauth_token_error(
                        "server_error",
                        "RepoTunnel could not issue authentication tokens.",
                    )
                },
            )?
        }

        "refresh_token" => {
            let refresh_token = request
                .refresh_token
                .as_deref()
                .ok_or_else(|| oauth_token_error("invalid_request", "Missing refresh_token."))?;

            mcp_auth::rotate_refresh_token(
                &policy.app,
                refresh_token,
                &request.client_id,
                &request.resource,
            )
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

pub(crate) async fn serve(
    listener: StdTcpListener,
    port: u16,
    app: AppHandle,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), String> {
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
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(false)
                .with_json_response(true),
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
        .layer(middleware::from_fn_with_state(policy, local_request_guard));

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.await;
        })
        .await
        .map_err(|error| format!("The local MCP gateway stopped unexpectedly: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{host_is_allowed, origin_is_allowed};

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
}
