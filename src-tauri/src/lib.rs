mod access;
mod activity;
mod ai_resources;
mod ai_workspace;
mod app_state;
mod ast_grep;
mod browser;
mod browser_transaction;
mod changes;
mod chatgpt_bridge;
mod checkpoint;
mod commands;
mod completion_proof;
mod connection;
mod continuity;
mod conversation;
mod custom_applications;
mod design_intelligence;
mod design_quality;
mod desktop_control;
mod direct_https;
mod environment;
mod execution;
mod external_access;
mod filesystem;
mod free_website_templates;
mod gateway;
mod git;
mod github;
mod gmail_access;
mod hardening;
mod https_setup;
mod integrations;
mod large_project_read;
mod latency;
mod launcher;
#[cfg(any(target_os = "macos", test))]
mod macos_ax;
mod managed_capabilities;
mod mcp_auth;
mod mcp_flight_recorder;
mod mcp_registry;
mod mcp_server;
mod media_inspection;
mod mission;
mod model_hub;
mod model_trial;
mod models;
mod monitoring;
mod phone;
mod platform_sandbox;
mod project_context;
mod project_index;
mod project_memory;
mod project_setup;
mod public_tunnel;
mod rea_bridge;
mod recovery_store;
mod remote_access;
mod remote_artifact;
mod remote_artifact_fetch;
mod remote_fleet;
mod remote_matrix;
#[cfg(test)]
mod remote_project_integration_tests;
mod remote_project_sync;
mod remote_snapshot;
mod remote_source_intent;
mod remote_source_receiver;
mod remote_source_review_proof;
mod remote_source_stage;
mod remote_sync;
mod remote_view;
mod repository;
mod secret_guard;
mod secure_tunnel_keyring;
mod self_continuation;
mod semantic;
mod storage;
mod system_resources;
mod team;
mod team_comms;
mod test_impact;
mod tunnel_client_setup;
// Task Integrity lands in tested layers; later tasks wire these foundation APIs
// into production mutation surfaces. Remove this temporary allowance at the
// final integration gate once every layer has a production caller.
#[cfg(any(target_os = "macos", test))]
mod macos_window_capture;
#[allow(dead_code)]
mod task_integrity;
mod temp_workspace;
mod terminal;

// Windows test fixtures sometimes receive extended-length \\?\ drive paths
// from tempfile. cmd.exe's mklink /J accepts ordinary drive paths instead.
// This helper is TEST-ONLY and does not change production path validation.
#[cfg(all(test, windows))]
fn windows_test_junction_command(link: &std::path::Path, target: &std::path::Path) -> String {
    fn cmd_path(path: &std::path::Path) -> String {
        let text = path.to_string_lossy();
        let value = text.strip_prefix(r"\\?\").unwrap_or(&text);
        assert!(
            std::path::Path::new(value).is_absolute()
                && !value.chars().any(|c| c == '"' || c == '\n' || c == '\r'),
            "Windows junction fixture requires an absolute unquoted drive path"
        );
        value.to_string()
    }
    format!(r#"mklink /J "{}" "{}""#, cmd_path(link), cmd_path(target))
}
mod ui_transaction;
mod updates;
mod versioning;
mod video;
mod video_assets;
mod video_director;
mod video_html;
mod video_narration;
mod video_narration_managed;
mod video_preview;
mod video_production;
mod video_qa;
mod video_render;
mod video_scene;
mod video_story_render;
mod visual_regression;
#[cfg(windows)]
mod windows_human_source;
#[cfg(any(windows, test))]
mod windows_uia;
mod workflow;

use app_state::AppState;
use tauri::Manager;

pub fn maybe_run_platform_sandbox_helper() -> Option<i32> {
    platform_sandbox::maybe_run_helper()
}

pub fn maybe_run_managed_process_supervisor() -> Option<i32> {
    terminal::maybe_run_managed_process_supervisor()
}

pub fn maybe_run_ai_workspace_github_proxy() -> Option<i32> {
    terminal::maybe_run_ai_workspace_github_proxy()
}

fn install_rustls_crypto_provider() {
    // RepoTunnel's dependency graph intentionally contains both AWS-LC (ngrok/axum-server)
    // and ring (reqwest/updater). Rustls 0.23 refuses to guess when both are enabled, so
    // choose one process-wide provider before any TLS client/server is constructed.
    // install_default is idempotent for our startup purpose: if a provider was already
    // installed by an earlier library initializer, keep that valid provider instead.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn initialize_local_runtime(app: &tauri::AppHandle) {
    // GitHub CLI authentication is global RepoTunnel state, not a Commands-page
    // feature. Prewarm it at application startup using the existing persistent
    // human gh profile. This is read-only: it never logs out, refreshes, replaces,
    // or copies the user's credential.
    if let Err(error) = std::thread::Builder::new()
        .name("repotunnel-github-status".to_string())
        .spawn(|| {
            let _ = github::status();
        })
    {
        eprintln!("RepoTunnel GitHub status prewarm warning: {error}");
    }

    let state = app.state::<AppState>();
    match state.ai_workspace.initialize(app) {
        Ok(true) => hardening::log_event(
            app,
            "INFO",
            "ai_workspace.recover",
            "Reattached the durable RepoTunnel AI Workspace session.",
        ),
        Ok(false) => {}
        Err(error) => hardening::log_event(app, "WARN", "ai_workspace.recover", &error),
    }
    if let Err(error) = terminal::initialize(app) {
        hardening::log_event(app, "WARN", "terminal.initialize", &error);
    }
    if let Err(error) = chatgpt_bridge::initialize(app) {
        hardening::log_event(app, "WARN", "chatgpt_bridge.initialize", &error);
    }
    if let Err(error) = monitoring::initialize(app) {
        hardening::log_event(app, "WARN", "monitoring.initialize", &error);
    }
    if let Err(error) = task_integrity::drift::initialize(app) {
        hardening::log_event(app, "WARN", "task_integrity.watch_initialize", &error);
    }
}

fn initialize_connection_runtime(app: &tauri::AppHandle) {
    // The MCP/Direct HTTPS transport is RepoTunnel's recovery/control channel.
    // Keep it independent from optional product features so connection recovery cannot
    // strand the application with Direct HTTPS and MCP unavailable.
    if public_tunnel::load_config(app)
        .ok()
        .flatten()
        .is_some_and(|config| config.auto_start)
    {
        let app_handle = app.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("repotunnel-auto-connect".to_string())
            .spawn(move || {
                let state = app_handle.state::<AppState>();
                if let Err(error) = state.start_public_tunnel(app_handle.clone()) {
                    hardening::log_event(&app_handle, "WARN", "public_tunnel.auto_start", &error);
                }
            })
        {
            eprintln!("RepoTunnel auto-connect worker warning: {error}");
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_rustls_crypto_provider();
    platform_sandbox::recover_stale_state();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::default())
        .invoke_handler({
            let handler: fn(tauri::ipc::Invoke<tauri::Wry>) -> bool = tauri::generate_handler![
                commands::select_workspace,
                commands::list_workspaces,
                commands::issue_remote_machine_access,
                commands::revoke_remote_machine_access,
                commands::list_remote_machine_access_grants,
                commands::revoke_remote_machine_access_grant,
                commands::rotate_remote_machine_access_grant,
                commands::connect_remote_machine,
                commands::disconnect_remote_machine,
                commands::reconnect_remote_machine,
                commands::list_remote_machines,
                commands::list_remembered_remote_machines,
                commands::get_local_remote_machine_identity,
                commands::remember_remote_machine,
                commands::forget_remembered_remote_machine,
                commands::probe_remote_machine,
                commands::inspect_remote_machine_capacity,
                commands::inspect_remote_machine_browser,
                commands::inspect_remote_machine_jobs,
                commands::preview_remote_source_diff_desktop,
                commands::preflight_remote_create_only_desktop,
                commands::record_remote_source_review_intent_desktop,
                commands::list_remote_source_review_intents_desktop,
                commands::withdraw_remote_source_review_intent_desktop,
                commands::reconcile_remote_source_review_intent_desktop,
                commands::inspect_remote_receiver_source_approval_desktop,
                commands::import_approved_remote_source_desktop,
                commands::reconcile_verified_remote_source_import_desktop,
                commands::export_signed_incoming_source_review_desktop,
                commands::approve_incoming_source_review_desktop,
                commands::withdraw_incoming_source_approval_desktop,
                commands::list_incoming_source_approvals_desktop,
                commands::inspect_incoming_source_approval_desktop,
                commands::list_remote_native_test_runs_desktop,
                commands::reconcile_remote_native_test_job_desktop,
                commands::cancel_remote_native_test_job_desktop,
                commands::view_remote_machine_screen,
                commands::list_downloaded_remote_artifacts,
                commands::export_remote_artifact,
                commands::get_workspace_health,
                commands::relocate_workspace,
                commands::add_workspace,
                commands::create_project,
                commands::remove_workspace,
                commands::update_workspace_access,
                commands::update_workspace_change_policy,
                commands::update_workspace_command_policy,
                commands::check_workspace_access,
                commands::list_directory,
                commands::list_directory_page,
                commands::read_file,
                commands::search_files,
                commands::editor_save_file,
                commands::editor_create_file,
                commands::editor_create_directory,
                commands::editor_rename_entry,
                commands::editor_delete_entry,
                commands::preview_workspace_image,
                commands::open_workspace_path_local,
                commands::get_project_setup,
                commands::prepare_project,
                commands::get_project_memory,
                commands::get_resume_snapshot,
                commands::get_task_integrity_status,
                commands::approve_task_integrity_expansion,
                commands::reject_task_integrity_expansion,
                commands::cancel_task_integrity,
                commands::list_scope_expansions,
                commands::approve_scope_expansion,
                commands::reject_scope_expansion,
                commands::update_project_memory,
                commands::inspect_project,
                commands::get_workflow_readiness,
                commands::get_environment_diagnostics,
                commands::list_tool_capabilities,
                commands::get_phone_discovery,
                commands::pair_phone_wirelessly,
                commands::get_phone_access_status,
                commands::select_phone_device,
                commands::set_phone_access_mode,
                commands::set_phone_access_paused,
                commands::get_phone_runtime_status,
                commands::ensure_phone_runtime,
                commands::clear_phone_runtime,
                commands::probe_phone_runtime,
                commands::get_phone_screen_frame,
                commands::phone_tap,
                commands::phone_swipe,
                commands::phone_key_event,
                commands::phone_type_text,
                commands::get_system_resource_snapshot,
                commands::create_temp_workspace,
                commands::list_temp_workspaces,
                commands::inspect_temp_workspace,
                commands::set_temp_workspace_preserved,
                commands::cleanup_temp_workspace,
                commands::temp_workspace_file_action,
                commands::create_file,
                commands::write_file,
                commands::patch_file,
                commands::create_directory,
                commands::rename_entry,
                commands::move_entry,
                commands::delete_entry,
                commands::list_changes,
                commands::approve_change,
                commands::reject_change,
                commands::undo_change,
                commands::get_version_timeline,
                commands::get_activity_timeline,
                commands::clear_version_history,
                commands::get_history_settings,
                commands::update_history_settings,
                commands::restore_version,
                commands::get_execution_status,
                commands::list_command_presets,
                commands::run_workspace_command,
                commands::list_command_history,
                commands::approve_command,
                commands::reject_command,
                commands::run_terminal_command,
                commands::run_local_terminal_command,
                commands::list_terminal_history,
                commands::approve_terminal_command,
                commands::reject_terminal_command,
                commands::start_managed_process,
                commands::start_local_managed_process,
                commands::list_managed_processes,
                commands::read_managed_process_output,
                commands::wait_managed_process,
                commands::approve_managed_process,
                commands::reject_managed_process,
                commands::stop_managed_process,
                commands::restart_managed_process,
                commands::list_launchable_applications,
                custom_applications::list_custom_applications,
                custom_applications::pick_custom_application,
                custom_applications::discover_custom_applications,
                custom_applications::add_discovered_custom_application,
                custom_applications::remove_custom_application,
                commands::get_github_connection_status,
                commands::connect_github,
                commands::cancel_github_connection,
                commands::disconnect_github,
                commands::list_deep_integrations,
                commands::set_deep_integration_enabled,
                commands::list_desktop_control_applications,
                commands::get_desktop_control_enabled,
                commands::set_desktop_control_enabled,
                commands::get_gmail_access_enabled,
                commands::set_gmail_access_enabled,
                commands::get_ai_workspace_status,
                commands::list_ai_workspace_recovery_evidence,
                commands::queue_ai_workspace_recovery_for_review,
                commands::start_ai_workspace,
                commands::stop_ai_workspace,
                commands::get_ai_workspace_frame,
                commands::ai_workspace_action,
                commands::ai_workspace_sequence,
                commands::open_url,
                commands::open_workspace_path,
                commands::launch_application,
                commands::launch_discovered_desktop_application,
                commands::list_launch_history,
                commands::approve_launch_action,
                commands::reject_launch_action,
                commands::list_automation_browsers,
                commands::get_browser_automation_status,
                commands::start_browser_automation,
                commands::stop_browser_automation,
                commands::list_browser_tabs,
                commands::browser_open_tab,
                commands::browser_activate_tab,
                commands::browser_close_tab,
                commands::browser_navigate,
                commands::browser_click,
                commands::browser_type,
                commands::browser_scroll,
                commands::browser_reload,
                commands::browser_configure_downloads,
                commands::list_browser_downloads,
                commands::cancel_browser_download,
                commands::browser_upload_file,
                commands::browser_inspect_page,
                commands::browser_pick_element,
                commands::get_browser_visual_selection,
                commands::browser_take_screenshot,
                commands::get_browser_diagnostics,
                commands::list_browser_history,
                commands::approve_browser_action,
                commands::reject_browser_action,
                commands::inspect_media_file,
                commands::extract_media_frame,
                commands::validate_media_decode,
                commands::get_video_tools_status,
                commands::install_video_tools,
                commands::start_video_analysis,
                commands::get_video_analysis_job,
                commands::list_video_analysis_jobs,
                commands::get_video_analysis_result,
                commands::cancel_video_analysis,
                commands::clear_video_cache,
                commands::create_video_project,
                commands::list_video_projects,
                commands::import_video_project_folder,
                commands::set_video_project_pinned,
                commands::set_video_project_resource_policy,
                commands::upsert_video_project_scene,
                commands::get_video_project_scene,
                commands::list_video_project_scenes,
                commands::list_video_project_files,
                commands::read_video_project_text_file,
                commands::delete_video_project,
                commands::get_video_project,
                commands::get_video_story_capabilities,
                commands::compile_video_story_plan,
                commands::get_video_story_plan,
                commands::get_video_story_animatic_plan,
                commands::render_video_story_animatic,
                commands::get_video_story_qa,
                commands::get_video_story_render_queue,
                commands::record_video_story_shot_render,
                commands::start_video_story_shot_render,
                commands::get_video_story_shot_render,
                commands::cancel_video_story_shot_render,
                commands::update_video_project_status,
                commands::write_video_project_document,
                commands::read_video_project_document,
                commands::start_video_project_recording,
                commands::get_video_project_recording,
                commands::stop_video_project_recording,
                commands::validate_video_project_scene,
                commands::render_video_project_scene,
                commands::render_video_project_diagram,
                commands::start_video_project_render,
                commands::get_video_project_render,
                commands::list_video_project_renders,
                commands::cancel_video_project_render,
                commands::get_video_project_pipeline_status,
                commands::clean_video_project,
                commands::qa_video_project,
                commands::render_video_project_timeline,
                commands::prepare_video_project_preview,
                commands::prepare_video_project_file_preview,
                commands::read_video_preview_chunk,
                commands::list_video_asset_sources,
                commands::record_video_asset_license,
                commands::get_video_narration_providers,
                commands::create_video_project_subtitles,
                commands::synthesize_video_project_narration,
                commands::get_monitoring_status,
                commands::start_workspace_monitoring,
                commands::stop_workspace_monitoring,
                commands::get_monitoring_snapshot,
                commands::list_monitoring_file_events,
                commands::get_git_status,
                commands::get_git_diff,
                commands::get_git_diff_check,
                commands::get_git_log,
                commands::request_git_stage,
                commands::request_git_commit,
                commands::list_git_actions,
                commands::approve_git_action,
                commands::reject_git_action,
                commands::request_git_restore_file,
                commands::get_file_info,
                commands::create_checkpoint,
                commands::list_checkpoints,
                commands::compare_checkpoint,
                commands::restore_checkpoint,
                commands::delete_checkpoint,
                commands::rename_checkpoint,
                commands::set_checkpoint_pinned,
                commands::clear_checkpoints,
                commands::run_safety_scan,
                commands::get_ai_access_status,
                commands::set_ai_access_paused,
                commands::get_gateway_status,
                commands::start_gateway,
                commands::stop_gateway,
                commands::get_public_tunnel_status,
                commands::configure_public_tunnel,
                commands::restart_public_tunnel,
                commands::provision_direct_https_certificate,
                https_setup::get_https_setup_readiness,
                https_setup::install_https_setup_wireguard_config,
                https_setup::open_https_setup_resource,
                https_setup::verify_https_setup_hostname,
                commands::clear_public_tunnel,
                commands::revoke_mcp_access,
                commands::get_chat_connection_status,
                commands::install_official_tunnel_client,
                commands::start_chat_connection,
                commands::stop_chat_connection,
                commands::pause_chat_connection,
                commands::resume_chat_connection,
                commands::remember_chat_connection,
                commands::get_model_hub,
                commands::refresh_model_runtime,
                commands::update_model_runtime_endpoint,
                commands::set_selected_local_model,
                commands::test_local_model,
                commands::get_model_trial,
                commands::run_model_trial,
                commands::cancel_model_trial,
                commands::list_home_conversations,
                commands::get_home_conversation,
                commands::create_home_conversation,
                commands::delete_home_conversation,
                commands::list_home_context_files,
                commands::begin_home_chat,
                commands::cancel_home_chat,
                commands::get_runtime_diagnostics,
                commands::set_launch_at_login,
                commands::get_update_status,
                commands::check_for_updates,
                commands::set_auto_update_checks,
                commands::defer_update,
                commands::install_update_and_restart,
                commands::create_agent_mission,
                commands::list_agent_missions,
                commands::get_agent_mission,
                commands::submit_agent_mission_directive,
                commands::submit_agent_mission_question,
                commands::pause_agent_mission,
                commands::resume_agent_mission,
                commands::stop_agent_mission,
                commands::rename_mission_agent,
                commands::remove_mission_agent,
                commands::restore_mission_agent,
                commands::archive_agent_mission,
                commands::restore_archived_agent_mission,
                commands::delete_agent_mission,
                commands::approve_agent_mission_gate,
                commands::reject_agent_mission_gate,
                commands::accept_agent_mission_result,
                commands::list_team_sessions,
                commands::get_team_session,
                commands::disconnect_team_agent,
                commands::create_team_session,
                commands::post_team_user_message,
                commands::pause_team_session,
                commands::resume_team_session,
                commands::cancel_team_session,
                commands::complete_team_session,
                commands::delete_team_session,
            ];
            handler
        })
        .setup(|app| {
            if let Err(error) = hardening::initialize(app.handle()) {
                eprintln!("RepoTunnel production initialization warning: {error}");
            }
            let paused = match storage::load_ai_access_paused(app.handle()) {
                Ok(paused) => paused,
                Err(error) => {
                    eprintln!(
                        "RepoTunnel AI access settings could not be loaded; AI access remains paused: {error}"
                    );
                    true
                }
            };
            app.state::<AppState>().set_ai_access_paused(paused);
            updates::complete_post_update_health_check(app.handle());
            initialize_connection_runtime(app.handle());
            initialize_local_runtime(app.handle());
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build RepoTunnel")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                task_integrity::drift::stop_all_task_watches();
                monitoring::stop_all_activity();
                terminal::stop_transient_activity_for_shutdown();
                browser::stop_all_activity();
                desktop_control::shutdown_helper();
                video::stop_all_activity();
                video_production::stop_all_activity();
                video_preview::clear_all(app_handle);
                model_hub::stop_owned_local_runtimes();
                rea_bridge::shutdown();
                chatgpt_bridge::shutdown();
                let state = app_handle.state::<AppState>();
                state.ai_workspace.shutdown(app_handle);
                let _ = state.stop_gateway();
                hardening::shutdown(app_handle);
            }
        });
}
