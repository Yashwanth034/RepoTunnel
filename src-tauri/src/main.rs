#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
fn configure_linux_video_rendering() {
    if std::env::var_os("WEBKIT_GST_DMABUF_SINK_DISABLED").is_none() {
        std::env::set_var("WEBKIT_GST_DMABUF_SINK_DISABLED", "1");
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    configure_linux_video_rendering();

    if let Some(exit_code) = repotunnel_lib::maybe_run_platform_sandbox_helper() {
        std::process::exit(exit_code);
    }
    repotunnel_lib::run();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::configure_linux_video_rendering;

    #[test]
    fn disables_webkit_gstreamer_dmabuf_video_sink_by_default() {
        const KEY: &str = "WEBKIT_GST_DMABUF_SINK_DISABLED";
        let previous = std::env::var_os(KEY);
        std::env::remove_var(KEY);

        configure_linux_video_rendering();
        assert_eq!(std::env::var(KEY).as_deref(), Ok("1"));

        std::env::set_var(KEY, "custom");
        configure_linux_video_rendering();
        assert_eq!(std::env::var(KEY).as_deref(), Ok("custom"));

        if let Some(value) = previous {
            std::env::set_var(KEY, value);
        } else {
            std::env::remove_var(KEY);
        }
    }
}
