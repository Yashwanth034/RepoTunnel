//! Exact macOS app-window capture provenance.
//!
//! Do not capture a rectangle of the user's display: overlap is not ownership.
//! First bind the approved AX window to exactly one WindowServer-owned window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowCandidate {
    pub(crate) number: u32,
    pub(crate) owner_pid: u32,
    pub(crate) layer: i32,
    pub(crate) title: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[cfg(any(test, target_os = "macos"))]
pub(crate) fn exact_owned_window(
    expected_pid: u32,
    expected_title: &str,
    expected_bounds: (i32, i32, u32, u32),
    candidates: &[WindowCandidate],
) -> Result<WindowCandidate, String> {
    if expected_pid <= 1
        || expected_pid == std::process::id()
        || expected_bounds.2 == 0
        || expected_bounds.3 == 0
        || expected_bounds.2 > 4096
        || expected_bounds.3 > 4096
        || u64::from(expected_bounds.2) * u64::from(expected_bounds.3) > 8_388_608
    {
        return Err("macOS native capture has invalid app ownership or image bounds.".into());
    }
    // Require an AX title. WindowServer names may be unavailable without
    // Screen Recording consent; that condition must NOT weaken matching.
    if expected_title.is_empty() {
        return Err("macOS WindowServer cannot bind an untitled AX window safely.".into());
    }
    let mut found = None;
    for window in candidates {
        if window.number == 0
            || window.owner_pid != expected_pid
            || window.layer != 0
            || window.title != expected_title
            || (window.x, window.y, window.width, window.height) != expected_bounds
        {
            continue;
        }
        if found.replace(window.clone()).is_some() {
            return Err("macOS window capture identity is ambiguous between windows.".into());
        }
    }
    let selected = found.ok_or_else(|| {
        "macOS WindowServer has no uniquely matching app-owned AX window; desktop fallback refused.".to_string()
    })?;
    if candidates
        .iter()
        .filter(|candidate| candidate.number == selected.number)
        .take(2)
        .count()
        != 1
    {
        return Err(
            "macOS WindowServer has conflicting owners for the selected window number.".into(),
        );
    }
    Ok(selected)
}

// A window-ID-only capture must contain pixels for that actual window, not
// the whole desktop if an OS utility silently ignored its -l selector.
// Retina displays normally return an integer 1x/2x/3x backing resolution.
// Allow a two-pixel border discrepancy, otherwise refuse to release pixels.
#[cfg(any(test, target_os = "macos"))]
fn verify_window_only_image_dimensions(
    window: &WindowCandidate,
    captured_width: u32,
    captured_height: u32,
) -> Result<(), String> {
    if window.width == 0
        || window.height == 0
        || captured_width == 0
        || captured_height == 0
        || captured_width > 4096
        || captured_height > 4096
        || u64::from(captured_width) * u64::from(captured_height) > 8_388_608
    {
        return Err("macOS capture has invalid window or screenshot dimensions.".into());
    }
    if !(1_u64..=3).any(|scale| {
        (u64::from(window.width) * scale).abs_diff(u64::from(captured_width)) <= 2
            && (u64::from(window.height) * scale).abs_diff(u64::from(captured_height)) <= 2
    }) {
        return Err(
            "macOS screenshot dimensions do not match its verified window; possible desktop capture refused."
                .into(),
        );
    }
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
fn ensure_window_capture_identity_unchanged(
    original: &WindowCandidate,
    current: &WindowCandidate,
) -> Result<(), String> {
    if original != current {
        return Err("Native macOS window ID, bounds or owner changed during capture.".into());
    }
    Ok(())
}

// Enforce a file-descriptor-level cap BEFORE reading or decoding the image.
// The path is inside a fresh private capture directory, but refusing symlinks
// also prevents later maintenance changes from following unintended paths.
#[cfg(any(test, target_os = "macos"))]
const MAX_NATIVE_CAPTURE_FILE_BYTES: u64 = 48 * 1024 * 1024;

#[cfg(any(test, target_os = "macos"))]
fn read_bounded_native_capture_png(path: &std::path::Path) -> Result<(Vec<u8>, u32, u32), String> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("Native macOS capture is not a safe readable file: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("Native macOS capture metadata is unavailable: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_NATIVE_CAPTURE_FILE_BYTES {
        return Err("Native macOS capture is not a bounded regular image file.".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_NATIVE_CAPTURE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Native macOS capture file read failed: {error}"))?;
    if bytes.len() as u64 > MAX_NATIVE_CAPTURE_FILE_BYTES {
        return Err("Native macOS capture changed size while being read.".into());
    }
    let (width, height) = bounded_png_dimensions(&bytes)?;
    Ok((bytes, width, height))
}

// A native PNG capture must be a bounded, real image, not arbitrary bytes
// copied from a helper process's output or an unrelated screen screenshot.
#[cfg(any(test, target_os = "macos"))]
fn bounded_png_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    use std::io::Read as _;

    const MAX_IMAGE_BYTES: usize = MAX_NATIVE_CAPTURE_FILE_BYTES as usize;
    if bytes.len() < 8 + 25 + 12 + 12
        || bytes.len() > MAX_IMAGE_BYTES
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return Err("Native macOS screenshot is not a complete bounded PNG.".into());
    }
    let mut cursor = 8usize;
    let mut dimensions = None;
    let mut idat = Vec::new();
    let mut has_idat = false;
    let mut after_idat = false;
    let mut has_end = false;

    while cursor < bytes.len() {
        let head = bytes
            .get(cursor..cursor + 8)
            .ok_or("Native macOS PNG has a truncated chunk header.")?;
        let length = u32::from_be_bytes(head[..4].try_into().unwrap()) as usize;
        let end = cursor
            .checked_add(12)
            .and_then(|v| v.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or("Native macOS PNG chunk exceeds the file boundary.")?;
        let kind = &bytes[cursor + 4..cursor + 8];
        let content = &bytes[cursor + 8..cursor + 8 + length];
        let expected_crc = u32::from_be_bytes(bytes[end - 4..end].try_into().unwrap());
        let mut crc = !0_u32;
        for &byte in kind.iter().chain(content) {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 };
            }
        }
        if !crc != expected_crc {
            return Err("Native macOS PNG chunk checksum is invalid.".into());
        }

        match kind {
            b"IHDR" if dimensions.is_none() && cursor == 8 && length == 13 => {
                let width = u32::from_be_bytes(content[0..4].try_into().unwrap());
                let height = u32::from_be_bytes(content[4..8].try_into().unwrap());
                if width == 0
                    || height == 0
                    || width > 4096
                    || height > 4096
                    || u64::from(width) * u64::from(height) > 8_388_608
                    || content[8] != 8
                    || !matches!(content[9], 2 | 6)
                    || content[10..13] != [0, 0, 0]
                {
                    return Err("Native macOS PNG has unsupported or excessive dimensions.".into());
                }
                dimensions = Some((width, height, content[9]));
            }
            b"IDAT" if dimensions.is_some() && !after_idat => {
                if idat
                    .len()
                    .checked_add(length)
                    .is_none_or(|v| v > MAX_IMAGE_BYTES)
                {
                    return Err("Native macOS PNG compressed image is too large.".into());
                }
                idat.extend_from_slice(content);
                has_idat = true;
            }
            b"IEND" if length == 0 && has_idat && end == bytes.len() => {
                has_end = true;
            }
            _ if dimensions.is_some() && kind[0].is_ascii_lowercase() => {
                if has_idat {
                    after_idat = true;
                }
            }
            _ => {
                return Err("Native macOS PNG contains an unexpected critical chunk.".into());
            }
        }
        cursor = end;
        if has_end {
            break;
        }
    }
    if !has_end || !has_idat || idat.is_empty() {
        return Err("Native macOS screenshot has no complete PNG image stream.".into());
    }
    let (width, height, color_type) =
        dimensions.ok_or("Native macOS screenshot has no valid dimensions.")?;
    let channels = if color_type == 6 { 4_u64 } else { 3_u64 };
    let expected = (u64::from(width) * channels + 1) * u64::from(height);
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(idat.as_slice())
        .take(expected + 1)
        .read_to_end(&mut raw)
        .map_err(|error| format!("Native macOS PNG image decompression failed: {error}"))?;
    if raw.len() as u64 != expected {
        return Err("Native macOS PNG has incomplete or excessive pixel data.".into());
    }
    let row_size = (u64::from(width) * channels + 1) as usize;
    if raw.chunks_exact(row_size).any(|row| row[0] > 4) {
        return Err("Native macOS PNG uses an invalid scanline filter.".into());
    }
    Ok((width, height))
}

// A fixed, non-interactive WindowServer-ID capture contract. Apple defines
// -a as "do not capture attached windows" and -o as "omit window shadows".
// Neither full-screen (-S), region (-R), nor interactive (-i) is permitted.
#[cfg(any(test, target_os = "macos"))]
fn macos_window_only_screencapture_args(window_number: u32) -> Result<[String; 8], String> {
    if window_number == 0 {
        return Err("Cannot capture an unverified macOS WindowServer ID.".into());
    }
    Ok([
        "-x".into(),
        "-o".into(),
        "-a".into(),
        "-r".into(),
        "-l".into(),
        window_number.to_string(),
        "-t".into(),
        "png".into(),
    ])
}

#[cfg(target_os = "macos")]
mod native {
    use std::{
        ffi::{c_void, CStr},
        os::raw::c_char,
        path::Path,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use super::{
        ensure_window_capture_identity_unchanged, exact_owned_window,
        read_bounded_native_capture_png, verify_window_only_image_dimensions, WindowCandidate,
    };

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGWindowListCopyWindowInfo(options: u32, reference: u32) -> *const c_void;
        fn CGRectMakeWithDictionaryRepresentation(
            dictionary: *const c_void,
            rect: *mut CGRect,
        ) -> bool;
        static kCGWindowNumber: *const c_void;
        static kCGWindowOwnerPID: *const c_void;
        static kCGWindowLayer: *const c_void;
        static kCGWindowBounds: *const c_void;
        static kCGWindowName: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(value: *const c_void);
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFDictionaryGetValue(dictionary: *const c_void, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(number: *const c_void, kind: i32, result: *mut c_void) -> bool;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            capacity: isize,
            encoding: u32,
        ) -> bool;
    }

    struct OwnedArray(*const c_void);
    impl Drop for OwnedArray {
        fn drop(&mut self) {
            unsafe { CFRelease(self.0) };
        }
    }
    unsafe fn number(dictionary: *const c_void, key: *const c_void) -> Option<i32> {
        let value = unsafe { CFDictionaryGetValue(dictionary, key) };
        if value.is_null() {
            return None;
        }
        let mut output = 0_i32;
        // CFNumber kCFNumberSInt32Type
        unsafe { CFNumberGetValue(value, 3, (&mut output as *mut i32).cast()) }.then_some(output)
    }
    fn integral(value: f64) -> Option<i32> {
        (value.is_finite()
            && value.fract() == 0.0
            && value >= i32::MIN as f64
            && value <= i32::MAX as f64)
            .then_some(value as i32)
    }
    fn length(value: f64) -> Option<u32> {
        (value.is_finite() && value.fract() == 0.0 && value > 0.0 && value <= 4096.0)
            .then_some(value as u32)
    }

    fn snapshot() -> Result<Vec<WindowCandidate>, String> {
        // The caller supplies an independently authenticated AX process PID.
        // This WindowServer listing is only used for an exact PID, name, bounds
        // and unique window-number match, never screen-region selection.
        let array = unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) };
        if array.is_null() {
            return Err("macOS WindowServer has no available GUI security session.".into());
        }
        let array = OwnedArray(array);
        let count = unsafe { CFArrayGetCount(array.0) };
        if !(0..=256).contains(&count) {
            return Err("macOS WindowServer enumeration exceeded its privacy budget.".into());
        }
        let mut result = Vec::new();
        for index in 0..count {
            let dictionary = unsafe { CFArrayGetValueAtIndex(array.0, index) };
            if dictionary.is_null() {
                continue;
            }
            let Some(owner_pid) = (unsafe { number(dictionary, kCGWindowOwnerPID) })
                .and_then(|value| u32::try_from(value).ok())
            else {
                continue;
            };
            let Some(window_number) = (unsafe { number(dictionary, kCGWindowNumber) })
                .and_then(|value| u32::try_from(value).ok())
            else {
                continue;
            };
            let Some(layer) = (unsafe { number(dictionary, kCGWindowLayer) }) else {
                continue;
            };
            let bounds = unsafe { CFDictionaryGetValue(dictionary, kCGWindowBounds) };
            let mut rect = CGRect::default();
            if bounds.is_null()
                || !unsafe { CGRectMakeWithDictionaryRepresentation(bounds, &mut rect) }
            {
                continue;
            }
            let Some((x, y, width, height)) = integral(rect.origin.x)
                .zip(integral(rect.origin.y))
                .zip(length(rect.size.width))
                .zip(length(rect.size.height))
                .map(|(((x, y), width), height)| (x, y, width, height))
            else {
                continue;
            };
            let name = unsafe { CFDictionaryGetValue(dictionary, kCGWindowName) };
            if name.is_null() {
                continue;
            }
            let mut buffer = [0_i8; 2048];
            if !unsafe {
                CFStringGetCString(
                    name,
                    buffer.as_mut_ptr(),
                    buffer.len() as isize,
                    0x0800_0100,
                )
            } {
                continue;
            }
            let title = unsafe { CStr::from_ptr(buffer.as_ptr()) }
                .to_string_lossy()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            result.push(WindowCandidate {
                number: window_number,
                owner_pid,
                layer,
                title,
                x,
                y,
                width,
                height,
            });
        }
        Ok(result)
    }

    fn bounded_command(command: &mut Command) -> Result<(), String> {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("macOS window-only capture could not start: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => {
                    return Err("macOS native window-only capture refused or failed.".into())
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(40));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "macOS native capture timed out; host desktop fallback prohibited.".into(),
                    );
                }
            }
        }
    }

    pub(crate) struct NativeWindowFrame {
        pub(crate) width: u32,
        pub(crate) height: u32,
        pub(crate) source_width: u32,
        pub(crate) source_height: u32,
        pub(crate) png: Vec<u8>,
    }

    pub(crate) fn capture_exact_window(
        pid: u32,
        title: &str,
        bounds: (i32, i32, u32, u32),
        max_width: u32,
    ) -> Result<NativeWindowFrame, String> {
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            return Err(
                "macOS Screen Recording permission is not granted; no prompt or desktop fallback."
                    .into(),
            );
        }
        let window = exact_owned_window(pid, title, bounds, &snapshot()?)?;
        let private = tempfile::tempdir().map_err(|error| {
            format!("Could not create private macOS capture directory: {error}")
        })?;
        let image = private.path().join("selected-window.png");
        // -l captures exactly one WindowServer ID; -o excludes drop shadows.
        // Never use screencapture's entire-screen or region-selection modes.
        let args = super::macos_window_only_screencapture_args(window.number)?;
        bounded_command(
            Command::new("/usr/sbin/screencapture")
                .args(args)
                .arg(&image),
        )?;
        let current = exact_owned_window(pid, title, bounds, &snapshot()?)?;
        ensure_window_capture_identity_unchanged(&window, &current)?;
        let (raw, source_width, source_height) = read_bounded_native_capture_png(&image)?;
        verify_window_only_image_dimensions(&window, source_width, source_height)?;
        let wanted = max_width.clamp(1, 4096);
        let png = if source_width > wanted {
            let scaled = private.path().join("scaled-window.png");
            bounded_command(
                Command::new("/usr/bin/sips")
                    .args(["--resampleWidth", &wanted.to_string()])
                    .arg(&image)
                    .arg("--out")
                    .arg(&scaled),
            )?;
            // sips command above MUST write only within this private directory.
            read_bounded_native_capture_png(&scaled)?
        } else {
            (raw, source_width, source_height)
        };
        let (png, width, height) = png;
        if width > wanted {
            return Err("macOS native window image exceeded requested width.".into());
        }
        Ok(NativeWindowFrame {
            width,
            height,
            source_width,
            source_height,
            png,
        })
    }
}

#[cfg(target_os = "macos")]
pub(crate) use native::{capture_exact_window, NativeWindowFrame};

#[cfg(test)]
mod tests {
    use super::*;

    fn window(number: u32, pid: u32) -> WindowCandidate {
        WindowCandidate {
            number,
            owner_pid: pid,
            layer: 0,
            title: "Approved Editor".into(),
            x: 12,
            y: 34,
            width: 800,
            height: 600,
        }
    }

    #[test]
    fn macos_single_window_capture_flags_exclude_desktop_and_interactive_modes() {
        let args = super::macos_window_only_screencapture_args(101).unwrap();
        assert_eq!(&args[5], "101");
        assert!(args.iter().any(|flag| flag == "-l"));
        assert!(args.iter().any(|flag| flag == "-a"));
        assert!(args.iter().any(|flag| flag == "-r"));
        assert!(!args
            .iter()
            .any(|flag| matches!(flag.as_str(), "-S" | "-R" | "-i")));
        assert!(super::macos_window_only_screencapture_args(0).is_err());
    }

    #[test]
    fn macos_window_capture_requires_unique_exact_ax_windowserver_ownership() {
        let pid = 7788;
        let bounds = (12, 34, 800, 600);
        let own = window(101, pid);
        let foreign = window(102, 9988);
        assert_eq!(
            exact_owned_window(
                pid,
                "Approved Editor",
                bounds,
                &[foreign.clone(), own.clone()]
            )
            .unwrap(),
            own
        );
        assert!(exact_owned_window(
            pid,
            "Approved Editor",
            bounds,
            std::slice::from_ref(&foreign)
        )
        .is_err());
        let colliding_id = WindowCandidate {
            number: own.number,
            ..foreign
        };
        assert!(exact_owned_window(
            pid, "Approved Editor", bounds, &[own.clone(), colliding_id],
        ).is_err(),
        "WindowServer number collisions with foreign-owned windows must never release their pixels");
        assert!(exact_owned_window(
            pid,
            "Approved Editor",
            bounds,
            &[own.clone(), window(103, pid)]
        )
        .is_err());
        let mut changed = own.clone();
        changed.x += 1;
        assert!(exact_owned_window(pid, "Approved Editor", bounds, &[changed]).is_err());
        let mut hidden = own.clone();
        hidden.layer = 1;
        assert!(exact_owned_window(pid, "Approved Editor", bounds, &[hidden]).is_err());
        assert!(exact_owned_window(pid, "", bounds, std::slice::from_ref(&own)).is_err());
        assert!(exact_owned_window(
            pid,
            "Approved Editor",
            (12, 34, 8192, 600),
            std::slice::from_ref(&own)
        )
        .is_err());
        assert!(exact_owned_window(std::process::id(), "Approved Editor", bounds, &[own]).is_err());
    }

    #[test]
    fn macos_screencapture_arguments_are_restricted_to_one_verified_window() {
        let args = super::macos_window_only_screencapture_args(421).unwrap();
        assert_eq!(
            args,
            ["-x", "-o", "-a", "-r", "-l", "421", "-t", "png"],
            "capture must explicitly name a single WindowServer ID"
        );
        assert!(
            super::macos_window_only_screencapture_args(0).is_err(),
            "an absent HWND/WindowServer ID must not fall back to a display capture"
        );
        for forbidden in ["-i", "-R", "-S", "-D", "-w"] {
            assert!(
                !args.iter().any(|arg| arg == forbidden),
                "native app-window capture must not enable desktop or interactive {forbidden}"
            );
        }
    }

    #[test]
    fn macos_window_only_capture_requires_bounded_real_png_header() {
        let mut png = vec![0_u8; 33];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[8..12].copy_from_slice(&13_u32.to_be_bytes());
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&800_u32.to_be_bytes());
        png[20..24].copy_from_slice(&600_u32.to_be_bytes());
        png[24] = 8;
        png[25] = 6;
        assert!(
            super::bounded_png_dimensions(&png).is_err(),
            "a syntactically plausible 33-byte PNG header has no IDAT, IEND or pixel data"
        );
        png[16..20].copy_from_slice(&10000_u32.to_be_bytes());
        assert!(super::bounded_png_dimensions(&png).is_err());
        assert!(super::bounded_png_dimensions(b"not png").is_err());
        let mut complete = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0xe0, 0x12, 0x91, 0xfb, 0x0f, 0x00, 0x01, 0xa4, 0x01, 0x3c, 0x93, 0x8b,
            0x0e, 0xb7, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        assert_eq!(super::bounded_png_dimensions(&complete).unwrap(), (1, 1));
        let private = tempfile::tempdir().unwrap();
        let real_image = private.path().join("verified-window.png");
        std::fs::write(&real_image, &complete).unwrap();
        let (disk_png, width, height) =
            super::read_bounded_native_capture_png(&real_image).unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(disk_png, complete);
        let oversized = private.path().join("oversized-window.png");
        std::fs::File::create(&oversized)
            .unwrap()
            .set_len(super::MAX_NATIVE_CAPTURE_FILE_BYTES + 1)
            .unwrap();
        assert!(
            super::read_bounded_native_capture_png(&oversized).is_err(),
            "an oversized sparse capture file must be rejected before allocation"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real_image, private.path().join("alias.png")).unwrap();
            assert!(
                super::read_bounded_native_capture_png(&private.path().join("alias.png"),).is_err(),
                "native capture must not follow a symlinked temporary screenshot"
            );
        }
        // CRC mutation must invalidate even a PNG with plausible dimensions.
        complete[29] ^= 0x01;
        assert!(super::bounded_png_dimensions(&complete).is_err());
        complete[29] ^= 0x01;
        // IEND is mandatory; a complete PNG cannot be only valid IHDR/IDAT.
        complete.truncate(complete.len() - 12);
        assert!(super::bounded_png_dimensions(&complete).is_err());
    }

    #[test]
    fn macos_window_only_pixels_must_match_verified_window_at_retina_scale() {
        let selected = window(101, 7788);
        assert!(verify_window_only_image_dimensions(&selected, 800, 600).is_ok());
        assert!(verify_window_only_image_dimensions(&selected, 1600, 1200).is_ok());
        assert!(verify_window_only_image_dimensions(&selected, 2400, 1800).is_ok());
        // A small rendering border discrepancy is tolerated without
        // accidentally permitting an unrelated whole-desktop resolution.
        assert!(verify_window_only_image_dimensions(&selected, 1602, 1199).is_ok());
        for dimensions in [
            (1440, 900),
            (1920, 1080),
            (800, 900),
            (4096, 4096),
            (0, 600),
        ] {
            assert!(
                verify_window_only_image_dimensions(&selected, dimensions.0, dimensions.1).is_err(),
                "an unrelated desktop-sized frame must not be released: {dimensions:?}"
            );
        }
        let tiny = WindowCandidate {
            width: 0,
            ..selected
        };
        assert!(verify_window_only_image_dimensions(&tiny, 100, 100).is_err());
    }

    #[test]
    fn macos_capture_rejects_window_number_reuse_after_revalidation() {
        let pid = 7788;
        let bounds = (12, 34, 800, 600);
        let initial =
            exact_owned_window(pid, "Approved Editor", bounds, &[window(101, pid)]).unwrap();
        let changed =
            exact_owned_window(pid, "Approved Editor", bounds, &[window(102, pid)]).unwrap();
        assert_ne!(initial.number, changed.number);
        assert!(
            super::ensure_window_capture_identity_unchanged(&initial, &changed).is_err(),
            "window-number reuse after a screencapture must never release pixels"
        );
        assert!(super::ensure_window_capture_identity_unchanged(&initial, &initial).is_ok());
        let mut foreign_owner = initial.clone();
        foreign_owner.owner_pid = 9988;
        assert!(super::ensure_window_capture_identity_unchanged(&initial, &foreign_owner).is_err());
        let mut moved = initial.clone();
        moved.y += 1;
        assert!(super::ensure_window_capture_identity_unchanged(&initial, &moved).is_err());
    }
}
