//! External UI smoke harness: launches pstart-host and drives Win32 messages.

#[cfg(not(windows))]
fn main() {
    eprintln!("host-ui-smoke is only supported on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(error) = smoke::run() {
        eprintln!("host-ui-smoke failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod smoke {
    use std::error::Error;
    use std::ffi::OsStr;
    use std::path::PathBuf;
    use std::process::{Child, Command};
    use std::ptr::{null, null_mut};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetDC, GetObjectW, GetPixel, ReleaseDC, HFONT, LOGFONTW,
    };
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetFocus, MapVirtualKeyW, SendInput, SetFocus, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
        KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
        VK_ESCAPE, VK_HOME, VK_RETURN, VK_UP,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowExW, FindWindowW, GetClientRect, GetForegroundWindow, GetWindowRect,
        GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        PostMessageW, SendMessageW, SetForegroundWindow, SetWindowTextW, ShowWindow, SW_SHOW,
        WM_CHAR, WM_CLOSE, WM_DPICHANGED, WM_GETFONT, WM_GETTEXT, WM_HOTKEY, WM_KEYDOWN,
        WM_SETTEXT,
    };

    use pstart_core::{FileId, FileItem, Frn, VolumeId, PROTOCOL_VERSION};
    use pstart_ipc::frame::{decode_payload, encode_payload};
    use pstart_ipc::{IpcMessage, Transport, UnixListener};

    const HOTKEY_ID: usize = 1;
    const SMOKE_ITEM_NAME: &str = "smoke.txt";
    const EM_REPLACESEL: u32 = 0x00C2;
    const EM_SETSEL: u32 = 0x00B1;

    pub fn run() -> Result<(), Box<dyn Error>> {
        // SAFETY: process-wide DPI setting; fail closed if unsupported.
        unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let exe = host_exe()?;
        let version = Command::new(&exe).arg("--version").output()?;
        if !version.status.success()
            || !String::from_utf8_lossy(&version.stdout).contains("pstart-host")
        {
            return Err("pstart-host --version failed".into());
        }
        let item_paths = (0..3)
            .map(|index| {
                std::env::temp_dir().join(format!(
                    "pstart-open-smoke-{}-{index}.cmd",
                    std::process::id()
                ))
            })
            .collect::<Vec<_>>();
        let marker_paths = (0..3)
            .map(|index| {
                std::env::temp_dir().join(format!(
                    "pstart-open-smoke-{}-{index}.flag",
                    std::process::id()
                ))
            })
            .collect::<Vec<_>>();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let history_path = std::env::temp_dir().join(format!(
            "pstart-host-history-{}-{stamp}.db",
            std::process::id()
        ));
        let action_path =
            std::env::temp_dir().join(format!("pstart-action-{}-{stamp}.cmd", std::process::id()));
        let action_marker =
            std::env::temp_dir().join(format!("pstart-action-{}-{stamp}.flag", std::process::id()));
        std::fs::write(
            &action_path,
            format!("@echo launched>\"{}\"\r\n", action_marker.display()),
        )?;
        let skin_path = std::env::temp_dir().join(format!(
            "pstart-host-skin-{}-{stamp}.toml",
            std::process::id()
        ));
        std::fs::write(
            &skin_path,
            "[colors]\nbackground='#112233'\nselection='#445566'\ntext='#ddeeff'\npath='#778899'\n",
        )?;
        let _ = std::fs::remove_file(&history_path);
        let backup_path = std::env::temp_dir().join(format!(
            "pstart-host-backup-{}-{stamp}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&backup_path);
        for (item, marker) in item_paths.iter().zip(&marker_paths) {
            std::fs::write(item, format!("@echo launched>\"{}\"\r\n", marker.display()))?;
        }
        let external_socket = std::env::var_os("PSTART_EXTERNAL_SOCKET").map(PathBuf::from);
        if external_socket.is_none() {
            let mut store = pstart_host::HistoryStore::open(&history_path)?;
            store.record_mru(&item_paths[2].to_string_lossy(), 100)?;
            store.record_mru(&item_paths[1].to_string_lossy(), 50)?;
            store.record_mru(&item_paths[2].to_string_lossy(), 200)?;
            store.set_alias("alias", "fast")?;
        }
        let (socket, server) = match external_socket {
            Some(path) => (path, None),
            None => {
                let socket = std::env::temp_dir()
                    .join(format!("pstart-ui-smoke-{}.sock", std::process::id()));
                let listener = UnixListener::bind(&socket)?;
                (socket, Some(start_server(listener, item_paths.clone())))
            }
        };
        let external = server.is_none();
        let mut command = Command::new(&exe);
        command
            .arg("--smoke")
            .env("PSTART_SMOKE_MS", if external { "30000" } else { "25000" })
            .env("PSTART_SMOKE_TITLE", "1")
            .env("PSTART_SOCKET", &socket)
            .env("PSTART_HOST_DB", &history_path);
        if external {
            command.env("PSTART_SMOKE_SELECTION", "1");
        } else {
            command
                .env("PSTART_FORCE_DPI", "144")
                .env("PSTART_VALIDATE_PATHS", "0")
                .env("PSTART_APP_SEARCH", "0")
                .env("PSTART_SKIN", &skin_path);
        }
        let mut first = command.spawn()?;
        let hwnd = wait_for_window(Duration::from_secs(3))?;
        if !external {
            let mut rect = RECT::default();
            // SAFETY: rect is writable and hwnd is live.
            unsafe { GetWindowRect(hwnd, &mut rect) };
            let width = rect.right - rect.left;
            if !(1000..=1160).contains(&width) {
                kill(&mut first);
                return Err(format!("DPI scaling did not apply; width={width}").into());
            }
        }

        let edit_class = wide("EDIT");
        let edit = wait_for_edit(hwnd, &edit_class, Duration::from_secs(3))?;
        if edit.is_null() {
            kill(&mut first);
            return Err("native EDIT control not found".into());
        }
        if !external {
            thread::sleep(Duration::from_millis(100));
            let mut client = RECT::default();
            // SAFETY: client is writable and hwnd is live.
            unsafe { GetClientRect(hwnd, &mut client) };
            // SAFETY: DC is released after the pixel read.
            let hdc = unsafe { GetDC(hwnd) };
            let pixel = unsafe { GetPixel(hdc, client.right - 5, client.bottom - 5) };
            unsafe { ReleaseDC(hwnd, hdc) };
            if pixel != 0x0033_2211 {
                kill(&mut first);
                return Err(
                    format!("custom skin background was not applied: {pixel:#010x}").into(),
                );
            }
            let height = font_height(edit).ok_or("EDIT font is unavailable")?;
            if height > -20 {
                kill(&mut first);
                return Err(format!("DPI font scaling did not apply; height={height}").into());
            }
            let mut suggested = RECT {
                left: 0,
                top: 0,
                right: 1440,
                bottom: 880,
            };
            // SAFETY: hwnd/edit are live; suggested is readable for the call.
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_DPICHANGED,
                    (192 | (192 << 16)) as usize,
                    (&mut suggested as *mut RECT) as isize,
                );
            }
            let mut resized = RECT::default();
            // SAFETY: resized is writable and hwnd is live.
            let width = unsafe {
                GetWindowRect(hwnd, &mut resized);
                resized.right - resized.left
            };
            let height = font_height(edit).unwrap_or(0);
            if !(1430..=1450).contains(&width) || height > -30 {
                kill(&mut first);
                return Err(
                    format!("runtime DPI change failed; width={width}, height={height}").into(),
                );
            }
        }
        if external {
            let query =
                std::env::var("PSTART_EXTERNAL_QUERY").unwrap_or_else(|_| "notepad.exe".to_owned());
            let query = wide(query);
            // SAFETY: edit is live; EM_REPLACESEL copies the full UTF-16 string.
            unsafe {
                SendMessageW(edit, EM_REPLACESEL, 1, query.as_ptr() as isize);
            }
        } else {
            let text = wide("ui-smoke");
            // SAFETY: edit is a live control.
            if unsafe { SetWindowTextW(edit, text.as_ptr()) } == 0 {
                kill(&mut first);
                return Err("SetWindowTextW failed".into());
            }
            // Drive the native EDIT insertion path so EN_CHANGE is sent by the control.
            // SAFETY: edit is live; WM_CHAR is processed by the edit control.
            unsafe {
                SendMessageW(edit, WM_CHAR, u32::from(b'a') as usize, 0);
            }
            // SAFETY: edit is live.
            let length = unsafe { GetWindowTextLengthW(edit) };
            let mut buffer = vec![0u16; (length + 1) as usize];
            // SAFETY: buffer is writable for length+1 units.
            let copied = unsafe { GetWindowTextW(edit, buffer.as_mut_ptr(), length + 1) };
            let readback = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
            if readback.is_empty() {
                kill(&mut first);
                return Err("EDIT text was not readable".into());
            }
        }

        let deadline = Instant::now() + Duration::from_secs(if external { 5 } else { 2 });
        loop {
            let title = window_text(hwnd);
            let matched = if external {
                parse_title(&title).is_some_and(|(count, _)| count > 0)
            } else {
                title == "pSTART - 1"
            };
            if matched {
                break;
            }
            if Instant::now() >= deadline {
                kill(&mut first);
                return Err(format!("query result not rendered; title={title}").into());
            }
            thread::sleep(Duration::from_millis(20));
        }

        if std::env::var("PSTART_REAL_IME_ONLY").ok().as_deref() == Some("1") {
            ime_probe(hwnd, edit)?;
            // SAFETY: hwnd is live; close the dedicated IME smoke host.
            unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
            let _ = wait_for_exit(&mut first, Duration::from_secs(2));
            return Ok(());
        }

        if !external {
            send_text(edit, "slow");
            thread::sleep(Duration::from_millis(200));
            send_text(edit, "fast");
            wait_title_count(hwnd, 3, Duration::from_secs(3))?;
            // The slow response arrives after the fast one and must be ignored.
            thread::sleep(Duration::from_millis(900));
            let count = parse_title(&window_text(hwnd)).map_or(0, |(count, _)| count);
            if count != 3 {
                kill(&mut first);
                return Err(
                    format!("stale query overwrote results; title={}", window_text(hwnd)).into(),
                );
            }

            // Esc cancels the pending Slow query and keeps the window open.
            send_text(edit, "slow");
            thread::sleep(Duration::from_millis(300));
            // SAFETY: edit is live; the subclass forwards Esc to NAV_ESCAPE.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_ESCAPE as usize, 0);
            }
            thread::sleep(Duration::from_millis(900));
            let count = parse_title(&window_text(hwnd)).map_or(0, |(count, _)| count);
            // SAFETY: hwnd is live.
            if count != 3 || unsafe { IsWindowVisible(hwnd) } == 0 {
                kill(&mut first);
                return Err(format!(
                    "Esc did not cancel pending query; visible={} title={}",
                    unsafe { IsWindowVisible(hwnd) },
                    window_text(hwnd)
                )
                .into());
            }
        }

        if external {
            let (count, _) =
                parse_title(&window_text(hwnd)).ok_or("selection title is malformed")?;
            if count > 1 {
                // SAFETY: edit is live; the edit subclass maps these keys to navigation.
                unsafe {
                    SendMessageW(edit, WM_KEYDOWN, VK_END as usize, 0);
                }
                wait_selected(hwnd, count)?;
                // SAFETY: edit is live.
                unsafe {
                    SendMessageW(edit, WM_KEYDOWN, VK_HOME as usize, 0);
                }
                wait_selected(hwnd, 1)?;
            }
            if let Ok(value) = std::env::var("PSTART_UI_BENCH") {
                if let Ok(iterations) = value.parse::<usize>() {
                    if iterations > 0 && count > 0 {
                        let mut samples = Vec::with_capacity(iterations);
                        for _ in 0..iterations {
                            send_text(edit, "");
                            wait_title_count(hwnd, 0, Duration::from_secs(3))?;
                            let started = Instant::now();
                            send_text(edit, "explorer.exe");
                            wait_title_count(hwnd, count, Duration::from_secs(3))?;
                            samples.push(started.elapsed().as_micros());
                        }
                        samples.sort_unstable();
                        let p50 = samples[samples.len() / 2];
                        let p95_index = (samples.len() * 95).div_ceil(100).saturating_sub(1);
                        let p95 = samples[p95_index];
                        let p99_index = (samples.len() * 99).div_ceil(100).saturating_sub(1);
                        let p99 = samples[p99_index];
                        let max = *samples.last().unwrap_or(&0);
                        println!(
                            "ui_bench n={iterations} p50_us={p50} p95_us={p95} p99_us={p99} max_us={max}"
                        );
                    }
                }
            }
        }

        // SAFETY: hwnd is live; toggle through the double-Ctrl keyboard hook.
        double_ctrl();
        thread::sleep(Duration::from_millis(150));
        // SAFETY: hwnd is live.
        if unsafe { IsWindowVisible(hwnd) } != 0 {
            kill(&mut first);
            return Err("double Ctrl did not hide the window".into());
        }
        // SAFETY: hwnd is live; toggle through the double-Ctrl keyboard hook.
        double_ctrl();
        thread::sleep(Duration::from_millis(150));
        // SAFETY: hwnd is live.
        if unsafe { IsWindowVisible(hwnd) } == 0 {
            kill(&mut first);
            return Err("double Ctrl did not show the window".into());
        }

        // Hide again; the second instance must activate the existing window.
        // SAFETY: hwnd is live.
        unsafe { PostMessageW(hwnd, WM_HOTKEY, HOTKEY_ID, 0) };
        thread::sleep(Duration::from_millis(100));
        // SAFETY: hwnd is live.
        if unsafe { IsWindowVisible(hwnd) } != 0 {
            kill(&mut first);
            return Err("hotkey did not hide the window before singleton test".into());
        }

        let mut second = Command::new(&exe)
            .arg("--smoke")
            .env("PSTART_SMOKE_MS", "5000")
            .env("PSTART_SOCKET", &socket)
            .spawn()?;
        let started = Instant::now();
        let second_exited = loop {
            if let Some(status) = second.try_wait()? {
                break status;
            }
            if started.elapsed() > Duration::from_secs(2) {
                kill(&mut second);
                kill(&mut first);
                return Err("second instance did not exit".into());
            }
            thread::sleep(Duration::from_millis(20));
        };
        if !second_exited.success() {
            kill(&mut first);
            return Err("second instance exited with failure".into());
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // SAFETY: hwnd is live.
            if unsafe { IsWindowVisible(hwnd) } != 0 {
                break;
            }
            if Instant::now() >= deadline {
                kill(&mut first);
                return Err("second instance did not activate the existing window".into());
            }
            thread::sleep(Duration::from_millis(20));
        }

        if !external {
            send_text(edit, "");
            // SAFETY: edit is live; Up on an empty EDIT recalls the last query.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_UP as usize, 0);
            }
            wait_title_count(hwnd, 3, Duration::from_secs(3))?;

            send_text(edit, "");
            // SAFETY: edit is live; the next Up recalls the older query.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_UP as usize, 0);
            }
            wait_title_count(hwnd, 1, Duration::from_secs(3))?;

            send_text(edit, "");
            // SAFETY: edit is live; Delete removes the current history entry.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DELETE as usize, 0);
            }
            wait_title_count(hwnd, 3, Duration::from_secs(3))?;

            send_text(edit, "");
            wait_title_count(hwnd, 0, Duration::from_secs(3))?;
            send_text(edit, "alias");
            wait_title_count(hwnd, 3, Duration::from_secs(3))?;

            send_text(edit, "=(2+3");
            wait_title_count(hwnd, 1, Duration::from_secs(3))?;
            // SAFETY: edit is live; Enter copies the parse error.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(200));
            let deadline = Instant::now() + Duration::from_secs(2);
            while clipboard_text().as_deref() != Some("error: invalid expression") {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("calculator parse error was not copied".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live; reopen for the valid expression.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, "");
            wait_title_count(hwnd, 0, Duration::from_secs(3))?;
            send_text(edit, "=(1.5+2.5)*4");
            wait_title_count(hwnd, 1, Duration::from_secs(3))?;
            // SAFETY: edit is live; Enter copies the calculator result.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(200));
            // SAFETY: hwnd is live.
            if unsafe { IsWindowVisible(hwnd) } != 0 {
                kill(&mut first);
                return Err("calculator Enter did not hide the overlay".into());
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while clipboard_text().as_deref() != Some("16") {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("calculator result was not copied".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live; show the overlay again.
            unsafe { PostMessageW(hwnd, WM_HOTKEY, HOTKEY_ID, 0) };
            thread::sleep(Duration::from_millis(100));
            send_text(edit, "fast");
            wait_title_count(hwnd, 3, Duration::from_secs(3))?;

            // SAFETY: edit is live; the selected item path is an executable .cmd marker.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while !marker_paths[2].exists() {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("ShellExecute open did not run the MRU marker".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            if marker_paths[0].exists() || marker_paths[1].exists() {
                kill(&mut first);
                return Err("MRU ranking did not select the highest-weight item".into());
            }
            // SAFETY: hwnd is live.
            if unsafe { IsWindowVisible(hwnd) } != 0 {
                kill(&mut first);
                return Err("successful open did not hide the overlay".into());
            }
        }

        if !external {
            // SAFETY: hwnd is live; reopen for action commands.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, "/copy action-text");
            let deadline = Instant::now() + Duration::from_secs(3);
            while clipboard_text().as_deref() != Some("action-text") {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("/copy did not update the clipboard".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live.
            let hide_deadline = Instant::now() + Duration::from_secs(3);
            while unsafe { IsWindowVisible(hwnd) } != 0 {
                if Instant::now() >= hide_deadline {
                    kill(&mut first);
                    return Err("/copy did not hide the overlay".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live; reopen for /open.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, &format!("/action {}", action_path.display()));
            thread::sleep(Duration::from_millis(300));
            if parse_title(&window_text(hwnd)).map(|(count, _)| count) == Some(2) {
                kill(&mut first);
                return Err("action was allowed without roots".into());
            }
            send_text(edit, "");
            thread::sleep(Duration::from_millis(100));
            send_text(edit, &format!("/allow {}", std::env::temp_dir().display()));
            thread::sleep(Duration::from_millis(200));
            send_text(edit, &format!("/action {}", action_path.display()));
            wait_title_count(hwnd, 2, Duration::from_secs(3))?;
            // SAFETY: edit is live; the first confirmation row executes.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !action_marker.exists() {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("/action did not launch the target".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            let expected_copy = action_path.display().to_string();
            while clipboard_text().as_deref() != Some(expected_copy.as_str()) {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("/action did not copy the component text".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live.
            if unsafe { IsWindowVisible(hwnd) } != 0 {
                kill(&mut first);
                return Err("/action did not hide the overlay".into());
            }
        }

        if !external {
            // SAFETY: hwnd is live; reopen for the settings view.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, "/settings");
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: edit is live; Down selects result limit, Enter cycles it.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            // SAFETY: edit is live; move to Clear history and activate it.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            // SAFETY: edit is live; select Theme and toggle it to light.
            unsafe {
                for _ in 0..3 {
                    SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                }
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: edit is live; select Calculator and disable it.
            unsafe {
                for _ in 0..4 {
                    SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                }
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            send_text(edit, "/alias docs *.md");
            wait_title_count(hwnd, 9, Duration::from_secs(3))?;
            thread::sleep(Duration::from_millis(300));
            // SAFETY: edit is live; move to the docs alias row and remove it.
            unsafe {
                for _ in 0..7 {
                    SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                }
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: hwnd is live.
            if unsafe { IsWindowVisible(hwnd) } == 0 {
                kill(&mut first);
                return Err("settings view closed after toggle".into());
            }
            std::fs::remove_file(&action_marker)?;
            send_text(edit, &format!("/action {}", action_path.display()));
            wait_title_count(hwnd, 2, Duration::from_secs(3))?;
            // SAFETY: edit is live; Down selects Cancel and Enter dismisses.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            if action_marker.exists() {
                kill(&mut first);
                return Err("cancelled action still ran".into());
            }
            // SAFETY: hwnd is live.
            if unsafe { IsWindowVisible(hwnd) } != 0 {
                kill(&mut first);
                return Err("cancelled action did not hide the overlay".into());
            }
            // SAFETY: hwnd is live; reopen for /backup.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, &format!("/backup {}", backup_path.display()));
            let deadline = Instant::now() + Duration::from_secs(3);
            while !backup_path.exists() {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("/backup did not create the database".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live.
            let hidden_deadline = Instant::now() + Duration::from_secs(3);
            while unsafe { IsWindowVisible(hwnd) } != 0 {
                if Instant::now() >= hidden_deadline {
                    kill(&mut first);
                    return Err("/backup did not hide the overlay".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: hwnd is live; change a setting, then restore the backup.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, "/settings");
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: edit is live; Down selects Result limit and Enter cycles it.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, &format!("/restore {}", backup_path.display()));
            wait_title_count(hwnd, 2, Duration::from_secs(3))?;
            // SAFETY: edit is live; the first confirmation row executes restore.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(500));
            wait_title_count(hwnd, 9, Duration::from_secs(3))?;
            // SAFETY: edit is live; in-memory limit must reload to 100, then cycle to 200.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            // SAFETY: edit is live; trigger restore again, then cancel.
            send_text(edit, &format!("/restore {}", backup_path.display()));
            wait_title_count(hwnd, 2, Duration::from_secs(3))?;
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            // SAFETY: hwnd is live; reopen settings and activate the backup row.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, "/settings");
            wait_title_count(hwnd, 9, Duration::from_secs(3))?;
            unsafe {
                for _ in 0..7 {
                    SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                }
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 2, Duration::from_secs(3))?;
            // SAFETY: edit is live; cancel the backup-row restore.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            thread::sleep(Duration::from_millis(100));
            // SAFETY: hwnd is live; delete a synthetic pre-restore backup.
            let fake = history_path.with_extension("pre-restore-000.db");
            std::fs::write(&fake, b"x")?;
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
            }
            thread::sleep(Duration::from_millis(100));
            send_text(edit, &format!("/backup-delete {}", fake.display()));
            let deadline = Instant::now() + Duration::from_secs(3);
            while fake.exists() {
                if Instant::now() >= deadline {
                    kill(&mut first);
                    return Err("/backup-delete did not remove the file".into());
                }
                thread::sleep(Duration::from_millis(20));
            }
            send_text(edit, "/plugins");
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: edit is live; Down selects Action and disables it.
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: edit is live; Down selects Timeout and cycles it.
            unsafe {
                for _ in 0..5 {
                    SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                }
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 8, Duration::from_secs(3))?;
            // SAFETY: edit is live; Down selects the root row and deletes it.
            unsafe {
                for _ in 0..6 {
                    SendMessageW(edit, WM_KEYDOWN, VK_DOWN as usize, 0);
                }
            }
            thread::sleep(Duration::from_millis(50));
            unsafe {
                SendMessageW(edit, WM_KEYDOWN, VK_RETURN as usize, 0);
            }
            wait_title_count(hwnd, 7, Duration::from_secs(3))?;
        }

        // SAFETY: hwnd is live; WM_CLOSE is handled by DefWindowProc.
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        if !wait_for_exit(&mut first, Duration::from_secs(2))? {
            kill(&mut first);
            return Err("host did not exit after WM_CLOSE".into());
        }
        if !external {
            let connection = rusqlite::Connection::open(&history_path)?;
            let slow: i64 = connection.query_row(
                "SELECT COUNT(*) FROM search_history WHERE query = 'slow'",
                [],
                |row| row.get(0),
            )?;
            if slow != 0 {
                return Err("deleted history entry is still present".into());
            }
            let fast: i64 = connection.query_row(
                "SELECT COUNT(*) FROM search_history WHERE query = 'fast'",
                [],
                |row| row.get(0),
            )?;
            let total: i64 =
                connection
                    .query_row("SELECT COUNT(*) FROM search_history", [], |row| row.get(0))?;
            if fast != 0 || total != 0 {
                return Err(format!("clear history left rows: fast={fast}, total={total}").into());
            }
            let mru2: i64 = connection.query_row(
                "SELECT uses FROM mru WHERE path = ?1",
                rusqlite::params![item_paths[2].to_string_lossy()],
                |row| row.get(0),
            )?;
            let mru1: i64 = connection.query_row(
                "SELECT uses FROM mru WHERE path = ?1",
                rusqlite::params![item_paths[1].to_string_lossy()],
                |row| row.get(0),
            )?;
            let mru0: i64 = connection.query_row(
                "SELECT COUNT(*) FROM mru WHERE path = ?1",
                rusqlite::params![item_paths[0].to_string_lossy()],
                |row| row.get(0),
            )?;
            if mru2 != 3 || mru1 != 1 || mru0 != 0 {
                return Err(format!(
                    "unexpected MRU weights: item0={mru0}, item1={mru1}, item2={mru2}"
                )
                .into());
            }
            let limit: String = connection.query_row(
                "SELECT value FROM settings WHERE key = 'result_limit'",
                [],
                |row| row.get(0),
            )?;
            if limit != "200" {
                return Err(format!("restored limit cycle stored {limit:?}").into());
            }
            let theme: String = connection.query_row(
                "SELECT value FROM settings WHERE key = 'theme'",
                [],
                |row| row.get(0),
            )?;
            if theme != "light" {
                return Err(format!("theme toggle stored {theme:?}").into());
            }
            let calculator: String = connection.query_row(
                "SELECT value FROM settings WHERE key = 'plugin.calculator'",
                [],
                |row| row.get(0),
            )?;
            if calculator != "0" {
                return Err(format!("calculator toggle stored {calculator:?}").into());
            }
            let docs: i64 = connection.query_row(
                "SELECT COUNT(*) FROM aliases WHERE name = 'docs'",
                [],
                |row| row.get(0),
            )?;
            if docs != 0 {
                return Err("alias row was not removed".into());
            }
            let alias_target: String = connection.query_row(
                "SELECT target FROM aliases WHERE name = 'alias'",
                [],
                |row| row.get(0),
            )?;
            if alias_target != "fast" {
                return Err(format!("preexisting alias changed: {alias_target:?}").into());
            }
            let backup = rusqlite::Connection::open(&backup_path)?;
            let backup_limit: String = backup.query_row(
                "SELECT value FROM settings WHERE key = 'result_limit'",
                [],
                |row| row.get(0),
            )?;
            if backup_limit != "100" {
                return Err(format!("backup settings mismatch: {backup_limit:?}").into());
            }
            let roots: i64 = connection.query_row(
                "SELECT COUNT(*) FROM settings WHERE key = 'plugin.action_roots'",
                [],
                |row| row.get(0),
            )?;
            if roots != 0 {
                return Err("allowed roots were not cleared".into());
            }
            let action: String = connection.query_row(
                "SELECT value FROM settings WHERE key = 'plugin.action'",
                [],
                |row| row.get(0),
            )?;
            if action != "0" {
                return Err(format!("action toggle stored {action:?}").into());
            }
            let timeout: String = connection.query_row(
                "SELECT value FROM settings WHERE key = 'plugin.action_timeout_ms'",
                [],
                |row| row.get(0),
            )?;
            if timeout != "1000" {
                return Err(format!("timeout cycle stored {timeout:?}").into());
            }
            let prefix = format!(
                "{}.pre-restore-",
                history_path
                    .file_stem()
                    .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
            );
            let pre_backup = std::fs::read_dir(history_path.parent().unwrap())?
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix));
            if !pre_backup {
                return Err("restore did not create a pre-restore backup".into());
            }
        }
        if let Some(server) = server {
            drop(server);
        }
        for path in item_paths.iter().chain(marker_paths.iter()) {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(&history_path);
        let _ = std::fs::remove_file(&skin_path);
        let _ = std::fs::remove_file(&action_path);
        let _ = std::fs::remove_file(&action_marker);
        let _ = std::fs::remove_file(&backup_path);
        let prefix = format!(
            "{}.pre-restore-",
            history_path
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned())
        );
        if let Some(parent) = history_path.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        Ok(())
    }

    fn font_height(edit: HWND) -> Option<i32> {
        // SAFETY: edit is live; WM_GETFONT returns the assigned font.
        let font = unsafe { SendMessageW(edit, WM_GETFONT, 0, 0) as HFONT };
        if font.is_null() {
            return None;
        }
        let mut logfont = LOGFONTW::default();
        // SAFETY: logfont is writable for a LOGFONTW.
        let copied = unsafe {
            GetObjectW(
                font,
                std::mem::size_of::<LOGFONTW>() as i32,
                (&mut logfont as *mut LOGFONTW).cast(),
            )
        };
        (copied != 0).then_some(logfont.lfHeight)
    }

    fn parse_title(title: &str) -> Option<(usize, usize)> {
        let rest = title.strip_prefix("pSTART - ")?;
        let mut parts = rest.split(" - ");
        let count = parts.next()?.parse::<usize>().ok()?;
        let selected = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        Some((count, selected))
    }

    fn press(vk: u8) {
        // SAFETY: synthetic keyboard input is only used by smoke probes.
        unsafe {
            let scan = MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16;
            let keyboard = |flags| INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: vk as VIRTUAL_KEY,
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            let inputs = [keyboard(0), keyboard(KEYEVENTF_KEYUP)];
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            );
        }
    }

    fn ime_probe(hwnd: HWND, edit: HWND) -> Result<(), Box<dyn Error>> {
        thread::sleep(Duration::from_millis(300));
        // SAFETY: a synthetic Alt tap releases the foreground lock for this probe.
        press(0x12);
        thread::sleep(Duration::from_millis(50));
        // SAFETY: attach to the foreground and host threads so focus can be set.
        let mut pid = 0;
        let fg_thread = unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
        let host_thread = unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        let current = unsafe { GetCurrentThreadId() };
        unsafe {
            AttachThreadInput(current, fg_thread, 1);
            AttachThreadInput(current, host_thread, 1);
            SetForegroundWindow(hwnd);
            SetFocus(edit);
        }
        thread::sleep(Duration::from_millis(150));
        // SAFETY: read-only focus query after attaching threads.
        if unsafe { GetFocus() } != edit {
            return Err("EDIT did not receive focus".into());
        }
        // SAFETY: read-only query.
        if unsafe { GetForegroundWindow() } != hwnd {
            return Err("host window is not foreground for IME probe".into());
        }
        let empty = wide("");
        let mut typed = String::new();
        for attempt in 0..2 {
            // SAFETY: edit is live; WM_SETTEXT is marshaled cross-process.
            let set = unsafe { SendMessageW(edit, WM_SETTEXT, 0, empty.as_ptr() as isize) };
            thread::sleep(Duration::from_millis(100));
            if !window_text(edit).is_empty() {
                return Err(format!("EDIT clear failed ({set}): {:?}", window_text(edit)).into());
            }
            for key in *b"ZHONG" {
                press(key);
                thread::sleep(Duration::from_millis(40));
            }
            thread::sleep(Duration::from_millis(500));
            typed = window_text(edit);
            if typed.is_empty() {
                break;
            }
            if attempt == 0 {
                // Pinyin retained English mode; Shift toggles Chinese.
                press(0x10);
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            return Err(format!("IME remained in English; edit={typed:?}").into());
        }
        let before = parse_title(&window_text(hwnd)).map_or(0, |(_, selected)| selected);
        press(VK_DOWN as u8);
        thread::sleep(Duration::from_millis(250));
        let after = parse_title(&window_text(hwnd)).map_or(0, |(_, selected)| selected);
        press(b'1');
        thread::sleep(Duration::from_millis(400));
        let text = window_text(edit);
        send_text(edit, "");
        if !text
            .chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch))
        {
            return Err(format!("no Chinese committed; typed={typed:?} edit={text:?}").into());
        }
        if before != after {
            return Err(format!(
                "IME composition did not consume Down; before={before} after={after}"
            )
            .into());
        }
        println!("real_ime=pass committed={text:?} selected={before}");
        Ok(())
    }

    fn double_ctrl() {
        press(VK_CONTROL as u8);
        thread::sleep(Duration::from_millis(80));
        press(VK_CONTROL as u8);
    }

    fn send_text(edit: HWND, text: &str) {
        let text = wide(text);
        // SAFETY: edit is live; select all then replace the selection.
        unsafe {
            SendMessageW(edit, EM_SETSEL, 0, -1);
            SendMessageW(edit, EM_REPLACESEL, 1, text.as_ptr() as isize);
        }
    }

    fn wait_title_count(
        hwnd: HWND,
        expected: usize,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if parse_title(&window_text(hwnd)).is_some_and(|(count, _)| count == expected) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "title count did not reach {expected}; title={}",
                    window_text(hwnd)
                )
                .into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_selected(hwnd: HWND, expected: usize) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if parse_title(&window_text(hwnd)).is_some_and(|(_, selected)| selected == expected) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "selection did not reach {expected}; title={}",
                    window_text(hwnd)
                )
                .into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn host_exe() -> Result<PathBuf, Box<dyn Error>> {
        let path = std::env::current_exe()?.with_file_name("pstart-host.exe");
        if !path.exists() {
            return Err(
                format!("{} does not exist; build pstart-host first", path.display()).into(),
            );
        }
        Ok(path)
    }

    fn clipboard_text() -> Option<String> {
        for _ in 0..50 {
            // SAFETY: null owner requests the current task clipboard.
            if unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            // SAFETY: clipboard is open; the returned handle is borrowed.
            let handle = unsafe { GetClipboardData(u32::from(CF_UNICODETEXT)) };
            if handle.is_null() {
                // SAFETY: balances OpenClipboard.
                unsafe {
                    CloseClipboard();
                }
                return None;
            }
            // SAFETY: handle is a clipboard Unicode-text block.
            let pointer = unsafe { GlobalLock(handle) } as *const u16;
            if pointer.is_null() {
                // SAFETY: balances OpenClipboard.
                unsafe {
                    CloseClipboard();
                }
                return None;
            }
            let mut length = 0usize;
            // SAFETY: clipboard text is NUL-terminated; cap at 128 units.
            while length < 128 && unsafe { *pointer.add(length) } != 0 {
                length += 1;
            }
            // SAFETY: pointer points to at least length UTF-16 units.
            let text =
                String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) });
            // SAFETY: balances GlobalLock.
            unsafe {
                GlobalUnlock(handle);
                CloseClipboard();
            }
            return Some(text);
        }
        None
    }

    fn wait_for_edit(
        parent: HWND,
        class: &[u16],
        timeout: Duration,
    ) -> Result<HWND, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: parent is live and class is NUL-terminated.
            let edit = unsafe { FindWindowExW(parent, null_mut(), class.as_ptr(), null()) };
            if !edit.is_null() {
                return Ok(edit);
            }
            if Instant::now() >= deadline {
                return Err("native EDIT control not found".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_window(timeout: Duration) -> Result<HWND, Box<dyn Error>> {
        let class = wide("PStartHostWindow");
        let title = wide("pSTART");
        let deadline = Instant::now() + timeout;
        loop {
            // SAFETY: class/title are NUL-terminated.
            let hwnd = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
            if !hwnd.is_null() {
                return Ok(hwnd);
            }
            if Instant::now() >= deadline {
                return Err("pstart-host window not found".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if child.try_wait()?.is_some() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn start_server(listener: UnixListener, item_paths: Vec<PathBuf>) -> thread::JoinHandle<()> {
        thread::spawn(move || loop {
            let Ok(mut transport) = listener.accept() else {
                return;
            };
            let item_paths = item_paths.clone();
            thread::spawn(move || serve(&mut transport, &item_paths));
        })
    }

    fn serve(transport: &mut impl Transport, item_paths: &[PathBuf]) {
        while let Ok(payload) = transport.receive_frame() {
            let Ok(message) = decode_payload(&payload) else {
                break;
            };
            let reply = match message {
                IpcMessage::Hello { .. } => IpcMessage::HelloAck {
                    protocol_version: PROTOCOL_VERSION,
                    server_version: "smoke".to_owned(),
                    session_id: 1,
                },
                IpcMessage::Query {
                    request_id, text, ..
                } => {
                    let count = match text.as_str() {
                        "fast" => 3,
                        "slow" => {
                            thread::sleep(Duration::from_millis(700));
                            1
                        }
                        "ui-smoke" | "a" => 1,
                        value if value.starts_with("ui-smoke") => 1,
                        _ => 0,
                    };
                    let items = (0..count)
                        .map(|index| {
                            let path = item_paths
                                .get(index as usize % item_paths.len())
                                .map_or_else(String::new, |path| {
                                    path.to_string_lossy().into_owned()
                                });
                            FileItem {
                                id: FileId::new(
                                    VolumeId::new("C:").unwrap(),
                                    Frn::new(index as u64),
                                ),
                                name: format!("{SMOKE_ITEM_NAME}{index}"),
                                path,
                                attributes: 0,
                                is_dir: false,
                            }
                        })
                        .collect();
                    IpcMessage::QueryResult {
                        request_id,
                        items,
                        truncated: false,
                    }
                }
                _ => break,
            };
            let Ok(encoded) = encode_payload(&reply) else {
                break;
            };
            if transport.send_frame(&encoded).is_err() {
                break;
            }
        }
    }

    fn window_text(hwnd: HWND) -> String {
        // SAFETY: hwnd is live.
        let length = unsafe { GetWindowTextLengthW(hwnd) };
        let mut buffer = vec![0u16; (length + 1) as usize];
        // SAFETY: buffer is writable for length+1 units; WM_GETTEXT marshals
        // text across processes (unlike GetWindowText on foreign controls).
        let copied = unsafe {
            SendMessageW(
                hwnd,
                WM_GETTEXT,
                (length + 1) as usize,
                buffer.as_mut_ptr() as isize,
            )
        };
        String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
    }

    fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
        value
            .as_ref()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }
}
