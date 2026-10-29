use std::{io::Read, time::Duration};

use serde::Serialize;
use serde_json::Value;
use url::Url;

const REGISTRY_BASE: &str = "https://registry.modelcontextprotocol.io";
const SEARCH_PATH: &str = "/v0.1/servers";
const MAX_SEARCH_LIMIT: usize = 20;
const DEFAULT_SEARCH_LIMIT: usize = 8;
const MAX_QUERY_BYTES: usize = 160;
const MAX_CURSOR_BYTES: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 768 * 1024;
const MAX_PACKAGES: usize = 8;
const MAX_REMOTES: usize = 8;
const MAX_INPUT_NAMES: usize = 32;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistryRepositorySummary {
    url: String,
    source: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistryPackageSummary {
    registry_type: String,
    identifier: String,
    version: Option<String>,
    runtime_hint: Option<String>,
    transport: Option<String>,
    file_sha256_present: bool,
    secret_inputs: Vec<String>,
    required_inputs: Vec<String>,
    secret_arguments: Vec<String>,
    required_arguments: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistryRemoteSummary {
    transport: String,
    url: Option<String>,
    secret_headers: Vec<String>,
    required_headers: Vec<String>,
    secret_variables: Vec<String>,
    required_variables: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistryServerSummary {
    pub(crate) name: String,
    title: Option<String>,
    description: String,
    version: String,
    repository: Option<RegistryRepositorySummary>,
    website_url: Option<String>,
    packages: Vec<RegistryPackageSummary>,
    remotes: Vec<RegistryRemoteSummary>,
    requires_secrets: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistrySearchResponse {
    registry: &'static str,
    mode: &'static str,
    pub(crate) servers: Vec<RegistryServerSummary>,
    pub(crate) next_cursor: Option<String>,
    message: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegistryServerResponse {
    registry: &'static str,
    mode: &'static str,
    server: RegistryServerSummary,
    message: &'static str,
}

fn bounded_text(value: Option<&Value>, max_chars: usize) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(max_chars).collect::<String>())
}

fn input_names(value: Option<&Value>, secret: bool) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("isSecret")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                == secret
        })
        .filter_map(|item| {
            bounded_text(item.get("name"), 120).or_else(|| bounded_text(item.get("valueHint"), 120))
        })
        .take(MAX_INPUT_NAMES)
        .collect()
}

fn required_input_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("isRequired")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|item| {
            bounded_text(item.get("name"), 120).or_else(|| bounded_text(item.get("valueHint"), 120))
        })
        .take(MAX_INPUT_NAMES)
        .collect()
}

fn object_input_names(value: Option<&Value>, secret: bool) -> Vec<String> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, item)| {
            item.get("isSecret")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                == secret
        })
        .map(|(name, _)| name.chars().take(120).collect::<String>())
        .take(MAX_INPUT_NAMES)
        .collect()
}

fn required_object_input_names(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, item)| {
            item.get("isRequired")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|(name, _)| name.chars().take(120).collect::<String>())
        .take(MAX_INPUT_NAMES)
        .collect()
}

fn merge_input_names(groups: impl IntoIterator<Item = Vec<String>>) -> Vec<String> {
    let mut merged = Vec::new();
    for name in groups.into_iter().flatten() {
        if !merged.contains(&name) {
            merged.push(name);
            if merged.len() >= MAX_INPUT_NAMES {
                break;
            }
        }
    }
    merged
}

fn compact_package(value: &Value) -> Option<RegistryPackageSummary> {
    let registry_type = bounded_text(value.get("registryType"), 40)?;
    let identifier = bounded_text(value.get("identifier"), 300)?;
    let environment = value.get("environmentVariables");
    let runtime_arguments = value.get("runtimeArguments");
    let package_arguments = value.get("packageArguments");
    Some(RegistryPackageSummary {
        registry_type,
        identifier,
        version: bounded_text(value.get("version"), 255),
        runtime_hint: bounded_text(value.get("runtimeHint"), 80),
        transport: bounded_text(value.get("transport").and_then(|v| v.get("type")), 40),
        file_sha256_present: value
            .get("fileSha256")
            .and_then(Value::as_str)
            .is_some_and(|hash| hash.len() == 64),
        secret_inputs: input_names(environment, true),
        required_inputs: required_input_names(environment),
        secret_arguments: merge_input_names([
            input_names(runtime_arguments, true),
            input_names(package_arguments, true),
        ]),
        required_arguments: merge_input_names([
            required_input_names(runtime_arguments),
            required_input_names(package_arguments),
        ]),
    })
}

fn compact_remote(value: &Value) -> Option<RegistryRemoteSummary> {
    let transport = bounded_text(value.get("type"), 40)?;
    let headers = value.get("headers");
    let variables = value.get("variables");
    Some(RegistryRemoteSummary {
        transport,
        url: bounded_text(value.get("url"), 2048),
        secret_headers: input_names(headers, true),
        required_headers: required_input_names(headers),
        secret_variables: object_input_names(variables, true),
        required_variables: required_object_input_names(variables),
    })
}

fn server_value(value: &Value) -> &Value {
    value
        .get("server")
        .filter(|server| server.is_object())
        .unwrap_or(value)
}

fn compact_server(value: &Value) -> Result<RegistryServerSummary, String> {
    let value = server_value(value);
    let name = bounded_text(value.get("name"), 200)
        .ok_or_else(|| "The MCP Registry returned a server without a valid name.".to_string())?;
    let description = bounded_text(value.get("description"), 500).unwrap_or_default();
    let version = bounded_text(value.get("version"), 255).unwrap_or_else(|| "unknown".to_string());
    let repository = value.get("repository").and_then(|repository| {
        bounded_text(repository.get("url"), 2048).map(|url| RegistryRepositorySummary {
            url,
            source: bounded_text(repository.get("source"), 80),
        })
    });
    let packages = value
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(compact_package)
        .take(MAX_PACKAGES)
        .collect::<Vec<_>>();
    let remotes = value
        .get("remotes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(compact_remote)
        .take(MAX_REMOTES)
        .collect::<Vec<_>>();
    let requires_secrets = packages
        .iter()
        .any(|package| !package.secret_inputs.is_empty() || !package.secret_arguments.is_empty())
        || remotes
            .iter()
            .any(|remote| !remote.secret_headers.is_empty() || !remote.secret_variables.is_empty());

    Ok(RegistryServerSummary {
        name,
        title: bounded_text(value.get("title"), 100),
        description,
        version,
        repository,
        website_url: bounded_text(value.get("websiteUrl"), 2048),
        packages,
        remotes,
        requires_secrets,
    })
}

fn compact_search_response(value: &Value, limit: usize) -> Result<RegistrySearchResponse, String> {
    let servers = value
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| "The MCP Registry returned an invalid server list.".to_string())?
        .iter()
        .filter_map(|server| compact_server(server).ok())
        .take(limit.clamp(1, MAX_SEARCH_LIMIT))
        .collect::<Vec<_>>();
    let next_cursor = bounded_text(
        value
            .get("metadata")
            .and_then(|metadata| metadata.get("nextCursor")),
        512,
    );
    Ok(RegistrySearchResponse {
        registry: REGISTRY_BASE,
        mode: "discovery-only",
        servers,
        next_cursor,
        message: "Registry results are metadata only. RepoTunnel does not auto-install, auto-run, or auto-connect third-party MCP servers from registry data.",
    })
}

fn validate_query(query: &str) -> Result<&str, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("MCP Registry search query cannot be empty.".to_string());
    }
    if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        return Err(format!(
            "MCP Registry search query must be at most {MAX_QUERY_BYTES} bytes and contain no control characters."
        ));
    }
    Ok(query)
}

fn validate_cursor(cursor: Option<&str>) -> Result<Option<&str>, String> {
    let Some(cursor) = cursor.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if cursor.len() > MAX_CURSOR_BYTES || cursor.chars().any(char::is_control) {
        return Err(format!(
            "MCP Registry cursor must be at most {MAX_CURSOR_BYTES} bytes and contain no control characters."
        ));
    }
    Ok(Some(cursor))
}

fn search_url(query: &str, limit: usize, cursor: Option<&str>) -> Result<Url, String> {
    let query = validate_query(query)?;
    let cursor = validate_cursor(cursor)?;
    let mut url = Url::parse(&format!("{REGISTRY_BASE}{SEARCH_PATH}"))
        .map_err(|error| format!("Could not prepare the official MCP Registry URL: {error}"))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("search", query)
            .append_pair("version", "latest")
            .append_pair("limit", &limit.clamp(1, MAX_SEARCH_LIMIT).to_string());
        if let Some(cursor) = cursor {
            pairs.append_pair("cursor", cursor);
        }
    }
    Ok(url)
}

fn valid_server_name(name: &str) -> bool {
    if name.len() < 3 || name.len() > 200 || name.chars().any(char::is_control) {
        return false;
    }
    let mut parts = name.split('/');
    let Some(namespace) = parts.next() else {
        return false;
    };
    let Some(server) = parts.next() else {
        return false;
    };
    if parts.next().is_some() || namespace.is_empty() || server.is_empty() {
        return false;
    }
    namespace
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        && server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn detail_url(server_name: &str) -> Result<Url, String> {
    let server_name = server_name.trim();
    if !valid_server_name(server_name) {
        return Err(
            "Enter an exact MCP Registry server name such as io.github.owner/server.".to_string(),
        );
    }
    let mut url = Url::parse(REGISTRY_BASE)
        .map_err(|error| format!("Could not prepare the official MCP Registry URL: {error}"))?;
    url.path_segments_mut()
        .map_err(|_| "Could not prepare the official MCP Registry path.".to_string())?
        .clear()
        .push("v0.1")
        .push("servers")
        .push(server_name)
        .push("versions")
        .push("latest");
    Ok(url)
}

fn fetch_json(url: Url) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("RepoTunnel/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("Could not prepare the MCP Registry client: {error}"))?;
    let mut response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .map_err(|error| format!("Could not reach the official MCP Registry: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "The official MCP Registry returned HTTP {}.",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("The MCP Registry response exceeded RepoTunnel's safe size limit.".to_string());
    }
    let mut bytes = Vec::with_capacity(16 * 1024);
    response
        .by_ref()
        .take((MAX_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read the MCP Registry response: {error}"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("The MCP Registry response exceeded RepoTunnel's safe size limit.".to_string());
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("The MCP Registry returned invalid JSON: {error}"))
}

pub(crate) fn search(
    query: &str,
    limit: Option<usize>,
    cursor: Option<&str>,
) -> Result<RegistrySearchResponse, String> {
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let value = fetch_json(search_url(query, limit, cursor)?)?;
    compact_search_response(&value, limit)
}

pub(crate) fn get(server_name: &str) -> Result<RegistryServerResponse, String> {
    let value = fetch_json(detail_url(server_name)?)?;
    Ok(RegistryServerResponse {
        registry: REGISTRY_BASE,
        mode: "discovery-only",
        server: compact_server(&value)?,
        message: "Registry metadata is untrusted third-party metadata. Review the repository, package, transport, required inputs, and secret requirements before connecting anything.",
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn search_url_is_fixed_to_official_registry_and_bounded() {
        let url = super::search_url("postgres", 999, Some("opaque cursor/+==")).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("registry.modelcontextprotocol.io"));
        assert_eq!(url.path(), "/v0.1/servers");
        let query = url.query().unwrap_or_default();
        assert!(query.contains("search=postgres"));
        assert!(query.contains("version=latest"));
        assert!(query.contains("limit=20"));
        assert!(query.contains("cursor=opaque+cursor%2F%2B%3D%3D"));
    }

    #[test]
    fn compact_metadata_never_returns_registry_secret_values_or_header_values() {
        let raw = json!({
            "server": {
                "name": "io.example/postgres",
                "title": "Postgres MCP",
                "description": "Database access",
                "version": "1.2.3",
                "repository": {"url": "https://github.com/example/postgres", "source": "github"},
                "packages": [{
                    "registryType": "npm",
                    "identifier": "@example/postgres-mcp",
                    "version": "1.2.3",
                    "runtimeHint": "npx",
                    "fileSha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "transport": {"type": "stdio"},
                    "runtimeArguments": [{"type": "named", "name": "--api-key", "isSecret": true, "isRequired": true, "value": "RUNTIME_SECRET_SHOULD_NEVER_RETURN"}],
                    "packageArguments": [{"type": "positional", "valueHint": "database_path", "isRequired": true}],
                    "environmentVariables": [{
                        "name": "POSTGRES_PASSWORD",
                        "isSecret": true,
                        "value": "TOP_SECRET_SHOULD_NEVER_RETURN"
                    }]
                }],
                "remotes": [{
                    "type": "streamable-http",
                    "url": "https://mcp.example.com/{tenant}/mcp",
                    "headers": [{"name": "Authorization", "isSecret": true, "value": "Bearer leaked"}],
                    "variables": {"tenant": {"isRequired": true}, "remoteToken": {"isSecret": true, "value": "REMOTE_SECRET_SHOULD_NEVER_RETURN"}}
                }]
            }
        });

        let compact = super::compact_server(&raw).unwrap();
        let serialized = serde_json::to_string(&compact).unwrap();
        assert!(serialized.contains("POSTGRES_PASSWORD"));
        assert!(serialized.contains("Authorization"));
        assert!(serialized.contains("--api-key"));
        assert!(serialized.contains("database_path"));
        assert!(serialized.contains("tenant"));
        assert!(serialized.contains("remoteToken"));
        assert!(!serialized.contains("TOP_SECRET_SHOULD_NEVER_RETURN"));
        assert!(!serialized.contains("RUNTIME_SECRET_SHOULD_NEVER_RETURN"));
        assert!(!serialized.contains("REMOTE_SECRET_SHOULD_NEVER_RETURN"));
        assert!(!serialized.contains("Bearer leaked"));
        assert!(serialized.contains("fileSha256Present"));
    }

    #[test]
    fn official_search_response_is_compacted_and_bounded() {
        let raw = json!({
            "servers": [
                {"server": {"name": "io.example/one", "description": "One", "version": "1.0.0"}},
                {"server": {"name": "io.example/two", "description": "Two", "version": "2.0.0"}}
            ],
            "metadata": {"count": 2, "nextCursor": "opaque-cursor"}
        });

        let compact = super::compact_search_response(&raw, 1).unwrap();
        assert_eq!(compact.servers.len(), 1);
        assert_eq!(compact.servers[0].name, "io.example/one");
        assert_eq!(compact.next_cursor.as_deref(), Some("opaque-cursor"));
    }

    #[test]
    fn invalid_search_and_server_names_are_rejected() {
        assert!(super::search_url("   ", 5, None).is_err());
        assert!(super::search_url("postgres", 5, Some("bad\ncursor")).is_err());
        assert!(super::detail_url("not-a-registry-name").is_err());
        assert!(super::detail_url("io.example/good-server").is_ok());
        assert!(super::detail_url("io.example/good/extra").is_err());
    }
}
