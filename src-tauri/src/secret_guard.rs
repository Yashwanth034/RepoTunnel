use std::{fs, path::Path};

const MAX_SCAN_BYTES: u64 = 2 * 1024 * 1024;

fn placeholder(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    value.is_empty()
        || value.contains("${")
        || value.contains("{{")
        || lower.contains("process.env")
        || lower.contains("os.environ")
        || lower.contains("placeholder")
        || lower.contains("changeme")
        || lower.contains("your_")
        || lower.contains("your-")
        || lower.contains("example")
        || lower == "test"
        || lower == "secret"
}

fn trim_value(raw: &str) -> &str {
    raw.trim()
        .trim_matches(|ch| matches!(ch, '\'' | '"' | '`' | ',' | ';' | ')' | ']' | '}'))
}

fn looks_like_generic_assignment(line: &str) -> bool {
    let Some(index) = line.find('=').or_else(|| line.find(':')) else {
        return false;
    };
    let mut key = line[..index].trim();
    for prefix in ["export ", "const ", "let ", "var "] {
        if let Some(rest) = key.strip_prefix(prefix) {
            key = rest.trim();
            break;
        }
    }
    key = key.trim_matches(|ch| matches!(ch, '\'' | '"'));
    if key.is_empty()
        || key
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
    {
        return false;
    }
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    let sensitive_key = matches!(
        normalized.as_str(),
        "api_key"
            | "apikey"
            | "access_token"
            | "auth_token"
            | "secret_key"
            | "client_secret"
            | "private_key"
            | "password"
            | "passwd"
            | "token"
            | "github_token"
            | "openai_api_key"
            | "anthropic_api_key"
            | "gemini_api_key"
            | "aws_secret_access_key"
    );
    if !sensitive_key {
        return false;
    }
    let value = trim_value(&line[index + 1..]);
    value.len() >= 20
        && !value.chars().any(char::is_whitespace)
        && !value
            .chars()
            .any(|ch| matches!(ch, '{' | '}' | '(' | ')' | '[' | ']' | ';'))
        && plausible_token(value)
}

fn token_after_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let index = line.find(prefix)?;
    let tail = &line[index..];
    let token = tail
        .split(|ch: char| {
            ch.is_whitespace() || matches!(ch, '\'' | '"' | '`' | ',' | ';' | ')' | ']' | '}')
        })
        .next()
        .unwrap_or("");
    Some(token)
}

fn plausible_token(token: &str) -> bool {
    let distinct = token
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    distinct >= 8 && !placeholder(token)
}

fn looks_like_jwt(token: &str) -> bool {
    if token.len() < 40 || !token.starts_with("eyJ") {
        return false;
    }
    let parts = token.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            part.len() >= 6
                && part
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        })
}

fn jwt_in_line(line: &str) -> Option<&str> {
    line.split(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"' | '`' | ',' | ';' | ')' | ']' | '}' | '(' | '[' | '{' | '=' | ':'
            )
    })
    .find(|token| looks_like_jwt(token))
}

fn bearer_token(line: &str) -> Option<&str> {
    let lower = line.to_ascii_lowercase();
    let index = lower.find("bearer ")?;
    let tail = &line[index + "bearer ".len()..];
    let token = tail
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '\'' | '"' | '`' | ',' | ';'))
        .next()
        .unwrap_or("");
    if token.len() >= 16 && plausible_token(token) {
        Some(token)
    } else {
        None
    }
}

pub(crate) fn detect_secret(bytes: &[u8]) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("-----BEGIN PRIVATE KEY-----")
            || trimmed.starts_with("-----BEGIN RSA PRIVATE KEY-----")
            || trimmed.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
            || trimmed.starts_with("-----BEGIN EC PRIVATE KEY-----")
        {
            return Some("private key material".to_string());
        }

        for (prefix, label, min_len) in [
            ("github_pat_", "GitHub personal access token", 30usize),
            ("ghp_", "GitHub token", 24usize),
            ("gho_", "GitHub OAuth token", 24usize),
            ("ghu_", "GitHub user token", 24usize),
            ("ghs_", "GitHub server token", 24usize),
            ("ghr_", "GitHub refresh token", 24usize),
            ("sk-", "API secret key", 24usize),
            ("sk_live_", "Stripe secret key", 24usize),
            ("rk_live_", "Stripe restricted key", 24usize),
            ("xoxb-", "Slack bot token", 24usize),
            ("xoxp-", "Slack user token", 24usize),
            ("AIza", "Google API key", 24usize),
        ] {
            if let Some(token) = token_after_prefix(line, prefix) {
                if token.len() >= min_len && plausible_token(token) {
                    return Some(label.to_string());
                }
            }
        }

        if let Some(token) = token_after_prefix(line, "AKIA") {
            if token.len() >= 20
                && token
                    .chars()
                    .take(20)
                    .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit())
                && plausible_token(token)
            {
                return Some("AWS access key".to_string());
            }
        }

        if bearer_token(line).is_some() {
            return Some("bearer credential".to_string());
        }

        if jwt_in_line(line).is_some() {
            return Some("JSON web token".to_string());
        }

        if looks_like_generic_assignment(line) {
            return Some("credential-like assignment".to_string());
        }
    }
    None
}

pub(crate) fn sensitive_env_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase().replace('-', "_");
    normalized.contains("token")
        || normalized.contains("secret")
        || normalized.contains("password")
        || normalized.contains("passwd")
        || normalized.contains("api_key")
        || normalized.contains("apikey")
        || normalized.contains("private_key")
        || normalized.contains("access_key")
        || normalized.contains("credential")
        || normalized == "authorization"
        || normalized == "cookie"
}

fn redact_token_prefixes(mut line: String) -> String {
    for prefix in [
        "github_pat_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "sk-",
        "sk_live_",
        "rk_live_",
        "xoxb-",
        "xoxp-",
        "AIza",
        "AKIA",
    ] {
        loop {
            let Some(start) = line.find(prefix) else {
                break;
            };
            let end = line[start..]
                .char_indices()
                .skip(1)
                .find_map(|(offset, ch)| {
                    if ch.is_whitespace()
                        || matches!(
                            ch,
                            '\'' | '"' | '`' | ',' | ';' | ')' | ']' | '}' | '&' | '?' | '#'
                        )
                    {
                        Some(start + offset)
                    } else {
                        None
                    }
                })
                .unwrap_or(line.len());
            if end <= start + prefix.len() {
                break;
            }
            line.replace_range(start..end, "[REDACTED]");
        }
    }
    line
}

fn redact_jwts(mut line: String) -> String {
    loop {
        let tokens = line
            .split(|ch: char| {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '\'' | '"' | '`' | ',' | ';' | ')' | ']' | '}' | '(' | '[' | '{'
                    )
            })
            .filter(|token| looks_like_jwt(token))
            .map(str::to_string)
            .collect::<Vec<_>>();
        let Some(token) = tokens.first() else {
            break;
        };
        if let Some(index) = line.find(token) {
            line.replace_range(index..index + token.len(), "[REDACTED]");
        } else {
            break;
        }
    }
    line
}

fn redact_assignment(line: &str) -> Option<String> {
    let index = line.find('=').or_else(|| line.find(':'))?;
    let mut key = line[..index].trim();
    for prefix in ["export ", "const ", "let ", "var "] {
        if let Some(rest) = key.strip_prefix(prefix) {
            key = rest.trim();
            break;
        }
    }
    key = key.trim_matches(|ch| matches!(ch, '\'' | '"'));
    if !sensitive_env_key(key) {
        return None;
    }
    Some(format!("{}=[REDACTED]", line[..index].trim_end()))
}

pub(crate) fn redact_text(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut in_private_key = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with("-----BEGIN ") && trimmed.contains("PRIVATE KEY-----") {
            if !redacted.is_empty() {
                redacted.push('\n');
            }
            redacted.push_str("[REDACTED PRIVATE KEY]");
            in_private_key = true;
            continue;
        }
        if in_private_key {
            if trimmed.starts_with("-----END ") && trimmed.contains("PRIVATE KEY-----") {
                in_private_key = false;
            }
            continue;
        }

        let mut line = redact_assignment(raw).unwrap_or_else(|| raw.to_string());
        let lower = line.to_ascii_lowercase();
        if let Some(index) = lower.find("authorization:") {
            line.replace_range(index..line.len(), "authorization: [REDACTED]");
        } else if let Some(index) = lower.find("bearer ") {
            let token_start = index + "bearer ".len();
            if token_start < line.len() {
                line.replace_range(token_start..line.len(), "[REDACTED]");
            }
        }
        line = redact_token_prefixes(line);
        line = redact_jwts(line);
        if !redacted.is_empty() {
            redacted.push('\n');
        }
        redacted.push_str(&line);
    }
    if text.ends_with('\n') && !redacted.is_empty() {
        redacted.push('\n');
    }
    redacted
}

pub(crate) fn scan_file(path: &Path, display: &str) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Could not inspect '{display}' for secrets: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_SCAN_BYTES {
        return Ok(());
    }
    let bytes = fs::read(path)
        .map_err(|error| format!("Could not read '{display}' for the secret guard: {error}"))?;
    scan_bytes(display, &bytes)
}

pub(crate) fn scan_bytes(display: &str, bytes: &[u8]) -> Result<(), String> {
    if let Some(kind) = detect_secret(bytes) {
        return Err(format!(
            "RepoTunnel secret guard blocked '{display}' because it appears to contain {kind}. Remove the credential or replace it with an environment/config reference before staging, importing, committing, or pushing."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{detect_secret, redact_text, sensitive_env_key};

    #[test]
    fn detects_realistic_tokens_but_not_env_references() {
        let realistic = format!(
            "GITHUB_TOKEN={}{}",
            "ghp_AbC9dEf8GhJ7", "kLm6NpQ5rSt4UvW3xYz2"
        );
        assert!(detect_secret(realistic.as_bytes()).is_some());
        assert!(detect_secret(b"GITHUB_TOKEN=${GITHUB_TOKEN}").is_none());
        assert!(detect_secret(b"api_key = process.env.API_KEY").is_none());
        assert!(detect_secret(b"Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456").is_some());
        assert!(detect_secret(
            b"jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signatureABC123"
        )
        .is_some());
    }

    #[test]
    fn redacts_terminal_output_and_sensitive_env_keys() {
        let token = format!("ghp_{}", "AbC9dEf8GhJ7kLm6NpQ5rSt4UvW3xYz2");
        let text = format!("Authorization: Bearer {token}\nGITHUB_TOKEN={token}\nok");
        let redacted = redact_text(&text);
        assert!(!redacted.contains(&token));
        assert!(redacted.contains("[REDACTED]"));
        assert!(sensitive_env_key("OPENAI_API_KEY"));
        assert!(!sensitive_env_key("NODE_ENV"));
    }
}
