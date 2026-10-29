//! Optional human-only OpenAI tunnel credential persistence in the desktop OS
//! Secret Service (GNOME Keyring / compatible keyring). Never put API keys in
//! repo files, argv, shell commands, logs, browser storage or settings JSON.
#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use std::{
    io::Write,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

#[cfg(target_os = "linux")]
#[derive(Serialize, Deserialize)]
pub(crate) struct SavedTunnel {
    pub(crate) tunnel_id: String,
    pub(crate) api_key: String,
}

// System Python's PyGObject Secret binding talks to the user's keyring.
// -I ignores PYTHONPATH/user site; the fixed script cannot be influenced by
// untrusted project files. Credentials travel only through stdin/stdout pipes.
#[cfg(target_os = "linux")]
const SCRIPT: &str = r#"
import json, sys
import gi
gi.require_version('Secret', '1')
from gi.repository import Secret
schema = Secret.Schema.new('app.repotunnel.openai-secure-tunnel', Secret.SchemaFlags.NONE,
                           {'application': Secret.SchemaAttributeType.STRING})
attrs = {'application': 'repotunnel'}
mode = sys.argv[1]
if mode == 'save':
    value = sys.stdin.read(8193)
    if len(value) > 8192:
        sys.exit(2)
    obj = json.loads(value)
    if not isinstance(obj, dict) or not isinstance(obj.get('tunnel_id'), str) or not isinstance(obj.get('api_key'), str):
        sys.exit(2)
    if not Secret.password_store_sync(schema, attrs, Secret.COLLECTION_DEFAULT,
                                      'RepoTunnel OpenAI Secure Tunnel', value, None):
        sys.exit(1)
elif mode == 'load':
    value = Secret.password_lookup_sync(schema, attrs, None)
    if value is not None:
        if len(value) > 8192:
            sys.exit(2)
        print(value, end='')
elif mode == 'forget':
    Secret.password_clear_sync(schema, attrs, None)
else:
    sys.exit(2)
"#;

#[cfg(target_os = "linux")]
fn operate(mode: &str, input: Option<&[u8]>) -> Result<Zeroizing<Vec<u8>>, String> {
    use std::io::Read;
    let mut child = Command::new("/usr/bin/python3")
        .args(["-I", "-c", SCRIPT, mode])
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "System Secret Service helper is unavailable.".to_string())?;
    if let Some(bytes) = input {
        let written = child
            .stdin
            .take()
            .ok_or_else(|| "Could not open the private keyring input pipe.".to_string())?
            .write_all(bytes);
        if written.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Could not pass credentials to the system keyring.".into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("System keyring is unavailable or locked. Unlock it and retry.".into());
            }
        }
    };
    if !status.success() {
        return Err("System keyring is unavailable or locked. Unlock it and retry.".into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    if let Some(stdout) = child.stdout.take() {
        stdout
            .take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| "Could not read the system keyring result.".to_string())?;
    }
    if bytes.len() > 8192 {
        return Err("System keyring returned an oversized credential.".into());
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
pub(crate) fn save(tunnel_id: &str, api_key: &str) -> Result<(), String> {
    crate::connection::validate_tunnel_id(tunnel_id)?;
    if api_key.is_empty() || api_key.len() > 4096 {
        return Err("OpenAI runtime key has an invalid size.".into());
    }
    let json = Zeroizing::new(
        serde_json::to_vec(&SavedTunnel {
            tunnel_id: tunnel_id.to_string(),
            api_key: api_key.to_string(),
        })
        .map_err(|_| "Could not encode secure tunnel credentials.".to_string())?,
    );
    if json.len() > 8192 {
        return Err("OpenAI runtime credentials are too large.".into());
    }
    operate("save", Some(&json)).map(|_| ())
}

#[cfg(target_os = "linux")]
pub(crate) fn load() -> Result<Option<(String, Zeroizing<String>)>, String> {
    let result = operate("load", None)?;
    if result.is_empty() {
        return Ok(None);
    }
    let saved: SavedTunnel = serde_json::from_slice(&result)
        .map_err(|_| "Saved secure tunnel keyring item is malformed.".to_string())?;
    crate::connection::validate_tunnel_id(&saved.tunnel_id)?;
    if saved.api_key.is_empty() || saved.api_key.len() > 4096 {
        return Err("Saved runtime key has an invalid size.".into());
    }
    Ok(Some((saved.tunnel_id, Zeroizing::new(saved.api_key))))
}

#[cfg(target_os = "linux")]
pub(crate) fn forget() -> Result<(), String> {
    operate("forget", None).map(|_| ())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn save(_: &str, _: &str) -> Result<(), String> {
    Err("Secure keyring persistence is available only on Linux in this build.".into())
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn load() -> Result<Option<(String, Zeroizing<String>)>, String> {
    Ok(None)
}
#[cfg(not(target_os = "linux"))]
pub(crate) fn forget() -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn helper_does_not_send_secrets_in_argv_or_shell() {
        #[cfg(target_os = "linux")]
        {
            assert!(!super::SCRIPT.contains("os.system"));
            assert!(!super::SCRIPT.contains("subprocess"));
            assert!(super::SCRIPT.contains("sys.stdin.read"));
            assert!(super::SCRIPT.contains("Secret.password_store_sync"));
            assert!(super::SCRIPT.contains("Secret.password_clear_sync"));
        }
    }
}
