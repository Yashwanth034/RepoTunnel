mod access;
mod activity;
mod app_state;
mod browser;
mod changes;
mod checkpoint;
mod commands;
mod connection;
mod execution;
mod external_access;
mod filesystem;
mod gateway;
mod git;
mod hardening;
mod launcher;
mod mcp_auth;
mod mcp_server;
mod models;
mod monitoring;
mod project_index;
mod public_tunnel;
mod repository;
mod secret_guard;
mod storage;
mod team;
mod terminal;
mod versioning;
mod workflow;

use app_state::AppState;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::select_workspace,
            commands::list_workspaces,
            commands::get_workspace_health,
            commands::relocate_workspace,
            commands::add_workspace,
            commands::remove_workspace,
            commands::update_workspace_access,
            commands::update_workspace_change_policy,
            commands::update_workspace_command_policy,
            commands::check_workspace_access,
            commands::list_directory,
            commands::read_file,
            commands::search_files,
            commands::editor_save_file,
            commands::editor_create_file,
            commands::editor_create_directory,
            commands::editor_rename_entry,
            commands::editor_delete_entry,
            commands::preview_workspace_image,
            commands::open_workspace_path_local,
            commands::inspect_project,
            commands::get_workflow_readiness,
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
            commands::approve_managed_process,
            commands::reject_managed_process,
            commands::stop_managed_process,
            commands::restart_managed_process,
            commands::list_launchable_applications,
            commands::open_url,
            commands::open_workspace_path,
            commands::launch_application,
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
            commands::browser_inspect_page,
            commands::browser_take_screenshot,
            commands::get_browser_diagnostics,
            commands::list_browser_history,
            commands::approve_browser_action,
            commands::reject_browser_action,
            commands::get_monitoring_status,
            commands::start_workspace_monitoring,
            commands::stop_workspace_monitoring,
            commands::get_monitoring_snapshot,
            commands::list_monitoring_file_events,
            commands::get_git_status,
            commands::get_git_diff,
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
            commands::clear_public_tunnel,
            commands::revoke_mcp_access,
            commands::get_chat_connection_status,
            commands::start_chat_connection,
            commands::stop_chat_connection,
            commands::get_runtime_diagnostics,
            commands::set_launch_at_login,
            commands::list_team_sessions,
            commands::get_team_session,
            commands::create_team_session,
            commands::post_team_user_message,
            commands::pause_team_session,
            commands::resume_team_session,
            commands::cancel_team_session,
            commands::complete_team_session,
            commands::delete_team_session,
        ])
        .setup(|app| {
            if let Err(error) = hardening::initialize(app.handle()) {
                eprintln!("RepoTunnel production initialization warning: {error}");
            }
            let paused = storage::load_ai_access_paused(app.handle()).unwrap_or(false);
            app.state::<AppState>().set_ai_access_paused(paused);
            if let Err(error) = terminal::initialize(app.handle()) {
                hardening::log_event(app.handle(), "WARN", "terminal.initialize", &error);
            }
            if let Err(error) = monitoring::initialize(app.handle()) {
                hardening::log_event(app.handle(), "WARN", "monitoring.initialize", &error);
            }

            if public_tunnel::load_config(app.handle())
                .ok()
                .flatten()
                .is_some_and(|config| config.auto_start)
            {
                let app_handle = app.handle().clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("repotunnel-auto-connect".to_string())
                    .spawn(move || {
                        let state = app_handle.state::<AppState>();
                        if let Err(error) = state.start_public_tunnel(app_handle.clone()) {
                            hardening::log_event(
                                &app_handle,
                                "WARN",
                                "public_tunnel.auto_start",
                                &error,
                            );
                        }
                    })
                {
                    eprintln!("RepoTunnel auto-connect worker warning: {error}");
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build RepoTunnel")
        .run(|app_handle, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                monitoring::stop_all_activity();
                terminal::stop_all_activity(app_handle);
                browser::stop_all_activity();
                let state = app_handle.state::<AppState>();
                let _ = state.stop_gateway();
                hardening::shutdown(app_handle);
            }
        });
}
