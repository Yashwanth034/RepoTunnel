use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::AppHandle;

use crate::{browser, launcher, models::Workspace};

const AI_WORKSPACE_LEASE_MS: u64 = 120_000;

#[derive(Clone, Debug, Default)]
struct OwnedResources {
    browser_session: bool,
    browser_tabs: BTreeSet<String>,
    browser_download_routing: bool,
    ai_workspace_sessions: BTreeMap<String, u64>,
    launched_pids: BTreeSet<u32>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ResourceKey {
    workspace_id: String,
    client_key: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiResourceCleanupResult {
    pub(crate) browser_tabs_closed: usize,
    pub(crate) browser_download_routing_released: bool,
    pub(crate) browser_session_closed: bool,
    pub(crate) ai_workspace_closed: bool,
    pub(crate) launched_applications_closed: usize,
    pub(crate) errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AiWorkspaceAppOwnershipStatus {
    pub(crate) app_session_id: String,
    pub(crate) owner_session_id: Option<String>,
    pub(crate) owner_last_seen: Option<u64>,
    pub(crate) lease_expires_at: Option<u64>,
    pub(crate) owner_lease_active: bool,
    pub(crate) owned_by_current_session: bool,
    pub(crate) can_reclaim_if_stale: bool,
    pub(crate) lease_duration_seconds: u64,
}

static REGISTRY: OnceLock<Mutex<HashMap<ResourceKey, OwnedResources>>> = OnceLock::new();

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .ok()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0)
}

pub(crate) fn opaque_owner_id(client_key: &str) -> String {
    let digest = Sha256::digest(client_key.as_bytes());
    let short = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("ai-owner-{short}")
}

fn registry() -> &'static Mutex<HashMap<ResourceKey, OwnedResources>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(workspace_id: &str, client_key: &str) -> ResourceKey {
    ResourceKey {
        workspace_id: workspace_id.to_string(),
        client_key: client_key.to_string(),
    }
}

fn with_owned<F>(workspace_id: &str, client_key: &str, update: F)
where
    F: FnOnce(&mut OwnedResources),
{
    if let Ok(mut guard) = registry().lock() {
        update(guard.entry(key(workspace_id, client_key)).or_default());
    }
}

pub(crate) fn own_browser_session(workspace_id: &str, client_key: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.browser_session = true
    });
}

pub(crate) fn release_browser_session(workspace_id: &str, client_key: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.browser_session = false
    });
}

pub(crate) fn own_browser_tab(workspace_id: &str, client_key: &str, tab_id: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.browser_tabs.insert(tab_id.to_string());
    });
}

pub(crate) fn release_browser_tab(workspace_id: &str, client_key: &str, tab_id: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.browser_tabs.remove(tab_id);
    });
}

pub(crate) fn release_all_browser_tabs(workspace_id: &str, client_key: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.browser_tabs.clear();
    });
}

pub(crate) fn assert_browser_access_available(
    _workspace_id: &str,
    _client_key: &str,
) -> Result<(), String> {
    // Each AI session owns an independent managed-browser runtime/profile.
    // Cross-AI browser use is therefore safe and must not serialize the project.
    Ok(())
}

pub(crate) fn claim_browser_download_routing(
    workspace_id: &str,
    client_key: &str,
) -> Result<(), String> {
    registry()
        .lock()
        .map_err(|_| "AI resource ownership state is unavailable.".to_string())?
        .entry(key(workspace_id, client_key))
        .or_default()
        .browser_download_routing = true;
    Ok(())
}

pub(crate) fn release_browser_download_routing(workspace_id: &str, client_key: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.browser_download_routing = false;
    });
}

pub(crate) fn assert_browser_stop_available(
    _workspace_id: &str,
    _client_key: &str,
) -> Result<(), String> {
    // Stopping one AI's browser no longer affects another AI's independent runtime.
    Ok(())
}

pub(crate) fn claim_or_assert_browser_tab(
    workspace_id: &str,
    client_key: &str,
    tab_id: &str,
) -> Result<(), String> {
    assert_browser_access_available(workspace_id, client_key)?;
    let mut guard = registry()
        .lock()
        .map_err(|_| "AI resource ownership state is unavailable.".to_string())?;
    if guard.iter().any(|(candidate_key, owned)| {
        candidate_key.workspace_id == workspace_id
            && candidate_key.client_key != client_key
            && owned.browser_tabs.contains(tab_id)
    }) {
        return Err(
            "That browser tab is owned by another AI session. Use your own RepoTunnel browser tab."
                .to_string(),
        );
    }
    let owned = guard.entry(key(workspace_id, client_key)).or_default();
    owned.browser_session = true;
    owned.browser_tabs.insert(tab_id.to_string());
    Ok(())
}

pub(crate) fn browser_tab_visible(workspace_id: &str, client_key: &str, tab_id: &str) -> bool {
    let Ok(guard) = registry().lock() else {
        return false;
    };
    let mut owned_by_any = false;
    for (candidate_key, owned) in guard.iter() {
        if candidate_key.workspace_id != workspace_id || !owned.browser_tabs.contains(tab_id) {
            continue;
        }
        owned_by_any = true;
        if candidate_key.client_key == client_key {
            return true;
        }
    }
    !owned_by_any
}

pub(crate) fn ai_workspace_app_ownership(
    workspace_id: &str,
    app_session_id: &str,
    current_client_key: Option<&str>,
) -> Result<AiWorkspaceAppOwnershipStatus, String> {
    let guard = registry()
        .lock()
        .map_err(|_| "AI resource ownership state is unavailable.".to_string())?;
    let now = now_ms();

    let owner = guard.iter().find_map(|(candidate_key, owned)| {
        let last_seen = owned.ai_workspace_sessions.get(app_session_id).copied()?;
        (candidate_key.workspace_id == workspace_id).then_some((candidate_key, last_seen))
    });

    let Some((owner_key, last_seen)) = owner else {
        return Ok(AiWorkspaceAppOwnershipStatus {
            app_session_id: app_session_id.to_string(),
            owner_session_id: None,
            owner_last_seen: None,
            lease_expires_at: None,
            owner_lease_active: false,
            owned_by_current_session: false,
            can_reclaim_if_stale: true,
            lease_duration_seconds: AI_WORKSPACE_LEASE_MS / 1_000,
        });
    };

    let lease_expires_at = last_seen.saturating_add(AI_WORKSPACE_LEASE_MS);
    let lease_active = now < lease_expires_at;
    let owned_by_current_session =
        current_client_key.is_some_and(|client_key| client_key == owner_key.client_key);

    Ok(AiWorkspaceAppOwnershipStatus {
        app_session_id: app_session_id.to_string(),
        owner_session_id: Some(opaque_owner_id(&owner_key.client_key)),
        owner_last_seen: Some(last_seen),
        lease_expires_at: Some(lease_expires_at),
        owner_lease_active: lease_active,
        owned_by_current_session,
        can_reclaim_if_stale: !lease_active || owned_by_current_session,
        lease_duration_seconds: AI_WORKSPACE_LEASE_MS / 1_000,
    })
}

pub(crate) fn claim_ai_workspace_app(
    workspace_id: &str,
    client_key: &str,
    app_session_id: &str,
) -> Result<AiWorkspaceAppOwnershipStatus, String> {
    let ownership = ai_workspace_app_ownership(workspace_id, app_session_id, Some(client_key))?;
    if ownership.owner_session_id.is_some()
        && !ownership.owned_by_current_session
        && ownership.owner_lease_active
    {
        return Err(format!(
            "AI Workspace app session {app_session_id} is actively owned by another AI session ({}).",
            ownership
                .owner_session_id
                .as_deref()
                .unwrap_or("unknown-owner")
        ));
    }

    let now = now_ms();
    let mut guard = registry()
        .lock()
        .map_err(|_| "AI resource ownership state is unavailable.".to_string())?;
    for (candidate_key, owned) in guard.iter_mut() {
        if candidate_key.workspace_id == workspace_id && candidate_key.client_key != client_key {
            owned.ai_workspace_sessions.remove(app_session_id);
        }
    }
    guard
        .entry(key(workspace_id, client_key))
        .or_default()
        .ai_workspace_sessions
        .insert(app_session_id.to_string(), now);
    drop(guard);
    ai_workspace_app_ownership(workspace_id, app_session_id, Some(client_key))
}

pub(crate) fn assert_ai_workspace_app_owned(
    workspace_id: &str,
    client_key: &str,
    app_session_id: &str,
) -> Result<(), String> {
    let ownership = ai_workspace_app_ownership(workspace_id, app_session_id, Some(client_key))?;
    if !ownership.owned_by_current_session {
        return Err(format!(
            "AI Workspace app session {app_session_id} is not owned by this AI session."
        ));
    }
    let now = now_ms();
    with_owned(workspace_id, client_key, |owned| {
        owned
            .ai_workspace_sessions
            .insert(app_session_id.to_string(), now);
    });
    Ok(())
}

pub(crate) fn owned_ai_workspace_app_sessions(
    workspace_id: &str,
    client_key: &str,
) -> Result<Vec<String>, String> {
    let guard = registry()
        .lock()
        .map_err(|_| "AI resource ownership state is unavailable.".to_string())?;
    Ok(guard
        .get(&key(workspace_id, client_key))
        .map(|owned| owned.ai_workspace_sessions.keys().cloned().collect())
        .unwrap_or_default())
}

pub(crate) fn release_ai_workspace_app(workspace_id: &str, client_key: &str, app_session_id: &str) {
    with_owned(workspace_id, client_key, |owned| {
        owned.ai_workspace_sessions.remove(app_session_id);
    });
}

pub(crate) fn reclaim_ai_workspace_app_if_stale(
    workspace_id: &str,
    client_key: &str,
    app_session_id: &str,
) -> Result<AiWorkspaceAppOwnershipStatus, String> {
    let ownership = ai_workspace_app_ownership(workspace_id, app_session_id, Some(client_key))?;
    if ownership.owned_by_current_session || ownership.owner_session_id.is_none() {
        return claim_ai_workspace_app(workspace_id, client_key, app_session_id);
    }
    if ownership.owner_lease_active {
        return Err(format!(
            "AI Workspace app owner {} is still inside its active lease until {}. Stale reclaim is refused.",
            ownership
                .owner_session_id
                .as_deref()
                .unwrap_or("unknown-owner"),
            ownership.lease_expires_at.unwrap_or(0)
        ));
    }
    claim_ai_workspace_app(workspace_id, client_key, app_session_id)
}

pub(crate) fn own_launched_pid(workspace_id: &str, client_key: &str, pid: u32) {
    with_owned(workspace_id, client_key, |owned| {
        owned.launched_pids.insert(pid);
    });
}

fn detach_transient_resources_for_cleanup(
    workspace_id: &str,
    client_key: &str,
) -> Result<OwnedResources, String> {
    let mut guard = registry()
        .lock()
        .map_err(|_| "AI resource ownership state is unavailable.".to_string())?;
    let resource_key = key(workspace_id, client_key);
    let mut owned = OwnedResources::default();

    if let Some(current) = guard.get_mut(&resource_key) {
        owned.browser_session = std::mem::take(&mut current.browser_session);
        owned.browser_tabs = std::mem::take(&mut current.browser_tabs);
        owned.browser_download_routing = std::mem::take(&mut current.browser_download_routing);
        owned.launched_pids = std::mem::take(&mut current.launched_pids);

        // AI Workspace applications are durable work surfaces, not transient
        // transport resources. Keep them running across ChatGPT/MCP turn or
        // session cleanup, but expire the old transport lease immediately so
        // a reconnect can reclaim the same app session without restarting it.
        for last_seen_at in current.ai_workspace_sessions.values_mut() {
            *last_seen_at = 0;
        }
    }

    Ok(owned)
}

pub(crate) fn cleanup(
    app: &AppHandle,
    workspace: &Workspace,
    client_key: &str,
) -> Result<AiResourceCleanupResult, String> {
    let owned = detach_transient_resources_for_cleanup(&workspace.id, client_key)?;
    let owner_id = opaque_owner_id(client_key);
    let browser_scope = browser::BrowserScope::new(workspace, Some(&owner_id));

    let mut result = AiResourceCleanupResult {
        browser_tabs_closed: 0,
        browser_download_routing_released: false,
        browser_session_closed: false,
        ai_workspace_closed: false,
        launched_applications_closed: 0,
        errors: Vec::new(),
    };

    if owned.browser_download_routing {
        match browser::reset_download_routing(app, &browser_scope) {
            Ok(_) => result.browser_download_routing_released = true,
            Err(error) => result
                .errors
                .push(format!("Browser download-routing cleanup: {error}")),
        }
    }

    if !owned.browser_tabs.is_empty() {
        let tabs = owned.browser_tabs.into_iter().collect::<Vec<_>>();
        match browser::cleanup_tabs_now(app, &browser_scope, &tabs) {
            Ok(count) => result.browser_tabs_closed = count,
            Err(error) => result.errors.push(format!("Browser-tab cleanup: {error}")),
        }
    }

    if owned.browser_session {
        match browser::cleanup_workspace_session(app, &browser_scope) {
            Ok(()) => result.browser_session_closed = true,
            Err(error) if error.contains("No RepoTunnel browser session") => {}
            Err(error) => result
                .errors
                .push(format!("Browser-session cleanup: {error}")),
        }
    }

    for pid in owned.launched_pids {
        match launcher::stop_ai_launched_pid(pid) {
            Ok(()) => result.launched_applications_closed += 1,
            Err(error) => result
                .errors
                .push(format!("Application PID {pid} cleanup: {error}")),
        }
    }

    Ok(result)
}

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    if let Ok(mut guard) = registry().lock() {
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[test]
    fn browser_tabs_are_isolated_by_ai_client() {
        let _guard = test_guard();
        reset_for_tests();
        own_browser_tab("workspace-a", "client-a", "tab-1");
        assert!(browser_tab_visible("workspace-a", "client-a", "tab-1"));
        assert!(!browser_tab_visible("workspace-a", "client-b", "tab-1"));
        assert!(claim_or_assert_browser_tab("workspace-a", "client-b", "tab-1").is_err());
        assert!(claim_or_assert_browser_tab("workspace-a", "client-b", "tab-2").is_ok());
    }

    #[test]
    fn per_ai_download_routing_does_not_block_other_ai_browser_mutations() {
        let _guard = test_guard();
        reset_for_tests();
        own_browser_session("workspace-a", "client-a");
        own_browser_tab("workspace-a", "client-a", "tab-a");
        assert!(claim_browser_download_routing("workspace-a", "client-a").is_ok());

        assert!(assert_browser_access_available("workspace-a", "client-b").is_ok());
        assert!(claim_or_assert_browser_tab("workspace-a", "client-b", "tab-b").is_ok());

        // Tab ownership remains isolated even though browser runtimes/download
        // routing are independent.
        assert!(claim_or_assert_browser_tab("workspace-a", "client-b", "tab-a").is_err());

        release_browser_download_routing("workspace-a", "client-a");
        assert!(assert_browser_access_available("workspace-a", "client-b").is_ok());
    }

    #[test]
    fn per_ai_browser_stop_does_not_block_other_ai_sessions() {
        let _guard = test_guard();
        reset_for_tests();
        own_browser_session("workspace-a", "client-a");
        own_browser_session("workspace-a", "client-b");

        assert!(assert_browser_stop_available("workspace-a", "client-a").is_ok());
        assert!(assert_browser_stop_available("workspace-a", "client-b").is_ok());
    }

    #[test]
    fn different_ai_sessions_can_own_different_ai_workspace_apps() {
        let _guard = test_guard();
        reset_for_tests();

        let first = claim_ai_workspace_app("workspace-a", "client-a", "app-session-a").unwrap();
        let second = claim_ai_workspace_app("workspace-a", "client-b", "app-session-b").unwrap();

        assert!(first.owned_by_current_session);
        assert!(second.owned_by_current_session);
        assert!(assert_ai_workspace_app_owned("workspace-a", "client-a", "app-session-a").is_ok());
        assert!(assert_ai_workspace_app_owned("workspace-a", "client-b", "app-session-b").is_ok());

        let collision =
            claim_ai_workspace_app("workspace-a", "client-b", "app-session-a").unwrap_err();
        assert!(collision.contains("actively owned by another AI session"));
    }

    #[test]
    fn active_ai_workspace_app_owner_cannot_be_reclaimed() {
        let _guard = test_guard();
        reset_for_tests();

        claim_ai_workspace_app("workspace-a", "client-a", "app-session-a").unwrap();
        let error = reclaim_ai_workspace_app_if_stale("workspace-a", "client-b", "app-session-a")
            .unwrap_err();
        assert!(error.contains("active lease"));

        let owner =
            ai_workspace_app_ownership("workspace-a", "app-session-a", Some("client-a")).unwrap();
        assert!(owner.owned_by_current_session);
    }

    #[test]
    fn stale_ai_workspace_app_owner_can_be_safely_reclaimed() {
        let _guard = test_guard();
        reset_for_tests();

        claim_ai_workspace_app("workspace-a", "client-a", "app-session-a").unwrap();
        {
            let mut guard = registry().lock().unwrap();
            let owned = guard
                .get_mut(&key("workspace-a", "client-a"))
                .expect("owner registry entry");
            owned.ai_workspace_sessions.insert(
                "app-session-a".to_string(),
                now_ms().saturating_sub(AI_WORKSPACE_LEASE_MS + 1),
            );
        }

        let stale =
            ai_workspace_app_ownership("workspace-a", "app-session-a", Some("client-b")).unwrap();
        assert!(!stale.owner_lease_active);
        assert!(stale.can_reclaim_if_stale);

        let reclaimed =
            reclaim_ai_workspace_app_if_stale("workspace-a", "client-b", "app-session-a").unwrap();
        assert!(reclaimed.owner_lease_active);
        assert!(reclaimed.owned_by_current_session);
        assert!(assert_ai_workspace_app_owned("workspace-a", "client-a", "app-session-a").is_err());
    }

    #[test]
    fn session_cleanup_preserves_ai_workspace_for_immediate_resume() {
        let _guard = test_guard();
        reset_for_tests();

        own_browser_session("workspace-a", "client-a");
        own_browser_tab("workspace-a", "client-a", "tab-a");
        own_launched_pid("workspace-a", "client-a", 4242);
        claim_ai_workspace_app("workspace-a", "client-a", "app-session-a").unwrap();

        let transient = detach_transient_resources_for_cleanup("workspace-a", "client-a").unwrap();

        assert!(transient.browser_session);
        assert_eq!(
            transient.browser_tabs,
            std::collections::BTreeSet::from(["tab-a".to_string()])
        );
        assert_eq!(
            transient.launched_pids,
            std::collections::BTreeSet::from([4242_u32])
        );
        let old_owner =
            ai_workspace_app_ownership("workspace-a", "app-session-a", Some("client-a")).unwrap();
        assert!(old_owner.owned_by_current_session);
        assert!(!old_owner.owner_lease_active);
        assert!(old_owner.can_reclaim_if_stale);

        let resumed =
            reclaim_ai_workspace_app_if_stale("workspace-a", "client-b", "app-session-a").unwrap();
        assert!(resumed.owned_by_current_session);
        assert!(resumed.owner_lease_active);
    }
}
