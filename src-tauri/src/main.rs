#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    if let Some(exit_code) = repotunnel_lib::maybe_run_platform_sandbox_helper() {
        std::process::exit(exit_code);
    }
    repotunnel_lib::run();
}
