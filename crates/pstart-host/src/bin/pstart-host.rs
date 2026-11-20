//! Minimal GDI Host: single instance, double-Ctrl overlay, native EDIT, query list.

#![allow(unsafe_code)] // Win32 UI / hotkey FFI boundary

#[cfg(not(windows))]
fn main() {
    eprintln!("pstart-host is only supported on Windows");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(error) = host::run() {
        eprintln!("pstart-host error: {error}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
mod host {
    use std::env;
    use std::error::Error;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use pstart_core::{ErrorCode, FileId, FileItem, Frn, VolumeId};
    use pstart_host::{
        hotkey::CtrlTapState, is_app_shortcut, item_for_path, merge_results, retain_existing_paths,
        start_menu_roots, AppCatalog, Calculator, Client, ClientError, ClientResponse,
        HistoryStore, QueryScheduler, Skin,
    };
    use pstart_plugins::ComponentRuntime;
    use pstart_policy::{CapabilitySet, APP_SEARCH, COPY, OPEN};
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, GlobalFree, COLORREF, ERROR_ALREADY_EXISTS, HWND, LPARAM,
        LRESULT, RECT, WPARAM,
    };
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW,
        CreateSolidBrush, DeleteDC, DeleteObject, EndPaint, FillRect, GetMonitorInfoW,
        GetStockObject, InvalidateRect, MonitorFromPoint, SelectObject, SetBkMode, SetTextColor,
        TextOutW, UpdateWindow, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
        DEFAULT_GUI_FONT, DEFAULT_PITCH, FF_DONTCARE, FW_NORMAL, HDC, HFONT, MONITORINFO,
        MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
    };
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows_sys::Win32::UI::Input::Ime::{
        ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, GCS_COMPSTR,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SetFocus, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LCONTROL, VK_NEXT,
        VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_UP,
    };
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, CallNextHookEx, CallWindowProcW, CreateWindowExW, DefWindowProcW,
        DispatchMessageW, FindWindowW, GetClientRect, GetCursorPos, GetMessageW, GetParent,
        GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsWindowVisible,
        KillTimer, LoadCursorW, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassW,
        SendMessageW, SetForegroundWindow, SetProcessDPIAware, SetTimer, SetWindowLongPtrW,
        SetWindowPos, SetWindowTextW, SetWindowsHookExW, ShowWindow, TranslateMessage,
        UnhookWindowsHookEx, CS_HREDRAW, CS_VREDRAW, EN_CHANGE, ES_AUTOHSCROLL, ES_LEFT,
        GWLP_USERDATA, GWLP_WNDPROC, HC_ACTION, HHOOK, IDC_ARROW, KBDLLHOOKSTRUCT, MSG,
        SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOW, SW_SHOWNORMAL,
        WA_INACTIVE, WH_KEYBOARD_LL, WM_ACTIVATE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_DPICHANGED,
        WM_ERASEBKGND, WM_HOTKEY, WM_IME_COMPOSITION, WM_IME_ENDCOMPOSITION,
        WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_PAINT,
        WM_SETFOCUS, WM_SETFONT, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW, WNDPROC,
        WS_BORDER, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    const WIDTH: i32 = 688;
    const HEIGHT: i32 = 428;
    const INPUT_MARGIN: i32 = 0;
    const INPUT_HEIGHT: i32 = 32;
    const LIST_TOP: i32 = INPUT_HEIGHT + 8;
    const ROW_HEIGHT: i32 = 52;
    const VISIBLE_ROWS: usize = 7;
    const EDIT_ID: usize = 1001;
    const HOTKEY_ID: i32 = 1;
    const SMOKE_TIMER: usize = 1;
    const QUERY_READY: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 11;
    const NAV_UP: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 1;
    const NAV_DOWN: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 2;
    const NAV_ENTER: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 3;
    const NAV_ESCAPE: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 4;
    const NAV_PAGE_UP: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 5;
    const NAV_PAGE_DOWN: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 6;
    const NAV_HOME: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 7;
    const NAV_END: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 8;
    const NAV_DELETE: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 9;
    const SHOW_MESSAGE: u32 = windows_sys::Win32::UI::WindowsAndMessaging::WM_APP + 10;

    static ORIGINAL_EDIT_PROC: AtomicIsize = AtomicIsize::new(0);
    static HOTKEY_HWND: AtomicIsize = AtomicIsize::new(0);
    static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);

    const DOUBLE_CTRL_INTERVAL_MS: u64 = 400;

    static CTRL_TAP: Mutex<CtrlTapState> = Mutex::new(CtrlTapState::new());

    unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for HC_ACTION, lparam points to a KBDLLHOOKSTRUCT.
            let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let is_ctrl = event.vkCode == u32::from(VK_CONTROL)
                || event.vkCode == u32::from(VK_LCONTROL)
                || event.vkCode == u32::from(VK_RCONTROL);
            if is_ctrl {
                let message = wparam as u32;
                let mut state = match CTRL_TAP.lock() {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
                    // SAFETY: Win32 tick counter has no preconditions.
                    let now = unsafe { GetTickCount64() };
                    if state.key_down(now, DOUBLE_CTRL_INTERVAL_MS) {
                        let hwnd = HOTKEY_HWND.load(Ordering::Acquire) as HWND;
                        if !hwnd.is_null() {
                            // SAFETY: hwnd is a live window while the hook is installed.
                            unsafe {
                                PostMessageW(hwnd, WM_HOTKEY, HOTKEY_ID as usize, 0);
                            }
                        }
                    }
                } else if message == WM_KEYUP || message == WM_SYSKEYUP {
                    // SAFETY: Win32 tick counter has no preconditions.
                    let now = unsafe { GetTickCount64() };
                    state.key_up(now);
                }
            } else {
                let message = wparam as u32;
                if message == WM_KEYDOWN || message == WM_SYSKEYDOWN {
                    let mut state = match CTRL_TAP.lock() {
                        Ok(state) => state,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    state.other_key();
                }
            }
        }
        // SAFETY: forwards the hook event to the next hook.
        unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
    }

    fn install_double_ctrl_hook(hwnd: HWND) -> bool {
        // SAFETY: installs a global low-level keyboard hook for this process.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(keyboard_hook),
                GetModuleHandleW(null()),
                0,
            )
        };
        if hook.is_null() {
            return false;
        }
        KEYBOARD_HOOK.store(hook as isize, Ordering::Release);
        HOTKEY_HWND.store(hwnd as isize, Ordering::Release);
        true
    }

    fn uninstall_double_ctrl_hook() {
        HOTKEY_HWND.store(0, Ordering::Release);
        let hook = KEYBOARD_HOOK.swap(0, Ordering::AcqRel);
        if hook != 0 {
            // SAFETY: hook was returned by SetWindowsHookExW and is uninstalled once.
            unsafe {
                UnhookWindowsHookEx(hook as HHOOK);
            }
        }
    }

    struct QueryOutcome {
        generation: u64,
        worker_us: u128,
        result: Result<(ClientResponse, bool), ClientError>,
    }

    struct QueryRequest {
        generation: u64,
        text: String,
        limit: u32,
    }

    fn query_worker(
        requests: Receiver<QueryRequest>,
        outcomes: Sender<QueryOutcome>,
        socket: PathBuf,
        apps: Arc<Mutex<AppCatalog>>,
        app_plugin: Option<AppPlugin>,
        hwnd: isize,
    ) {
        let mut client = None;
        let app_search = env::var("PSTART_APP_SEARCH").map_or(true, |value| value != "0");
        let validate_paths = env::var("PSTART_VALIDATE_PATHS").is_ok_and(|value| value == "1");
        while let Ok(mut request) = requests.recv() {
            while let Ok(next) = requests.try_recv() {
                request = next;
            }
            let worker_started = Instant::now();
            let mut retried = false;
            let query_result = loop {
                if client.is_none() {
                    match Client::connect(&socket) {
                        Ok(connection) => client = Some(connection),
                        Err(error) => break Err(error),
                    }
                }
                let connection = client.as_mut().expect("worker connection is live");
                match connection.query(&request.text, request.limit) {
                    Ok(response) => break Ok(response),
                    Err(error) if error.is_offline() && !retried => {
                        retried = true;
                        client = None;
                    }
                    Err(error) => {
                        client = None;
                        break Err(error);
                    }
                }
            };
            let result = match query_result {
                Err(error) => Err(error),
                Ok(mut response) => {
                    if app_search {
                        let app_items = if let Some(plugin) = &app_plugin {
                            match plugin.runtime.run_text_host_timeout(
                                plugin.component.as_slice(),
                                &request.text,
                                Duration::from_millis(500),
                            ) {
                                Ok(output) => {
                                    parse_plugin_apps(&output.text, request.limit as usize)
                                }
                                Err(_) => apps
                                    .lock()
                                    .map(|catalog| {
                                        catalog.search(&request.text, request.limit as usize)
                                    })
                                    .unwrap_or_default(),
                            }
                        } else {
                            apps.lock()
                                .map(|catalog| {
                                    catalog.search(&request.text, request.limit as usize)
                                })
                                .unwrap_or_default()
                        };
                        if merge_results(&mut response.items, app_items, request.limit as usize) {
                            response.truncated = true;
                        }
                    }
                    if validate_paths && retain_existing_paths(&mut response.items) {
                        response.truncated = true;
                    }
                    Ok((response, false))
                }
            };
            let _ = outcomes.send(QueryOutcome {
                generation: request.generation,
                worker_us: worker_started.elapsed().as_micros(),
                result,
            });
            // SAFETY: hwnd is the host window; PostMessage is safe after destruction.
            unsafe {
                PostMessageW(hwnd as HWND, QUERY_READY, 0, 0);
            }
        }
    }

    struct ActionRequest {
        open: Option<String>,
        copy: Option<String>,
        restore: Option<String>,
    }

    #[derive(Clone)]
    struct AppPlugin {
        runtime: Arc<ComponentRuntime>,
        component: Arc<Vec<u8>>,
    }

    fn load_app_plugin(catalog: Arc<Mutex<AppCatalog>>) -> Option<AppPlugin> {
        let path = env::var_os("PSTART_APP_PLUGIN")?;
        let component = std::fs::read(path).ok()?;
        let runtime = ComponentRuntime::new()
            .with_capabilities(CapabilitySet::from_bits(APP_SEARCH))
            .with_app_search(move |query| {
                let Ok(catalog) = catalog.lock() else {
                    return String::new();
                };
                catalog
                    .search(query, 50)
                    .into_iter()
                    .map(|item| item.path)
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        println!("pstart-app-plugin=loaded");
        Some(AppPlugin {
            runtime: Arc::new(runtime),
            component: Arc::new(component),
        })
    }

    fn parse_plugin_apps(text: &str, limit: usize) -> Vec<FileItem> {
        let mut items: Vec<FileItem> = Vec::new();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let path = Path::new(line);
            if !is_app_shortcut(path) {
                continue;
            }
            let item = item_for_path(path);
            if items.iter().any(|existing| existing.path == item.path) {
                continue;
            }
            items.push(item);
            if items.len() >= limit {
                break;
            }
        }
        items
    }

    struct State {
        hwnd: HWND,
        edit: HWND,
        query_rx: Receiver<QueryOutcome>,
        query_request_tx: Sender<QueryRequest>,
        scheduler: QueryScheduler,
        calculator: Option<Calculator>,
        calculator_enabled: bool,
        action_runtime: ComponentRuntime,
        action_enabled: bool,
        action_timeout_ms: u64,
        pending_action: Option<ActionRequest>,
        action_roots: Vec<PathBuf>,
        history: Option<HistoryStore>,
        aliases: std::collections::HashMap<String, String>,
        history_tx: Option<Sender<String>>,
        query_started: Option<Instant>,
        result_generation: u64,
        history_cache: Option<Vec<String>>,
        history_cursor: Option<usize>,
        pending_history_query: Option<String>,
        history_enabled: bool,
        apps: Arc<Mutex<AppCatalog>>,
        result_limit: u32,
        theme_dark: bool,
        skin: Skin,
        items: Vec<FileItem>,
        selected: Option<usize>,
        scroll: usize,
        dpi: i32,
        font: HFONT,
        paint_dc: HDC,
        paint_bitmap: isize,
        paint_old_bitmap: isize,
        paint_width: i32,
        paint_height: i32,
        status: String,
        ime_composing: bool,
    }

    impl State {
        fn new(hwnd: HWND) -> Self {
            let socket_path = env::var_os("PSTART_SOCKET").map_or_else(
                || PathBuf::from(pstart_ipc::af_unix::DEFAULT_SOCKET_PATH),
                PathBuf::from,
            );
            let (query_tx, query_rx) = mpsc::channel();
            let history = history_path().and_then(|path| HistoryStore::open(&path).ok());
            let aliases = history
                .as_ref()
                .and_then(|store| store.aliases(100).ok())
                .unwrap_or_default()
                .into_iter()
                .collect();
            let history_tx = history
                .as_ref()
                .and_then(|store| store.path().map(std::path::Path::to_path_buf))
                .map(|path| {
                    let (sender, receiver) = mpsc::channel::<String>();
                    thread::spawn(move || {
                        let Ok(mut store) = HistoryStore::open(&path) else {
                            return;
                        };
                        while let Ok(query) = receiver.recv() {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map_or(0, |duration| duration.as_millis() as i64);
                            let _ = store.record(&query, now);
                        }
                    });
                    sender
                });
            let result_limit = history
                .as_ref()
                .and_then(|store| store.get_setting("result_limit").ok().flatten())
                .and_then(|value| value.parse::<u32>().ok())
                .map_or(50, |value| value.clamp(1, 200));
            let history_enabled = history
                .as_ref()
                .and_then(|store| store.get_setting("history.enabled").ok().flatten())
                .is_none_or(|value| value != "0");
            let theme_dark = history
                .as_ref()
                .and_then(|store| store.get_setting("theme").ok().flatten())
                .is_none_or(|value| value != "light");
            let calculator_enabled = history
                .as_ref()
                .and_then(|store| store.get_setting("plugin.calculator").ok().flatten())
                .is_none_or(|value| value != "0");
            let action_enabled = history
                .as_ref()
                .and_then(|store| store.get_setting("plugin.action").ok().flatten())
                .is_none_or(|value| value != "0");
            let action_timeout_ms = history
                .as_ref()
                .and_then(|store| store.get_setting("plugin.action_timeout_ms").ok().flatten())
                .and_then(|value| value.parse::<u64>().ok())
                .map_or(500, |value| value.clamp(50, 5000));
            let action_roots = history
                .as_ref()
                .and_then(|store| store.get_setting("plugin.action_roots").ok().flatten())
                .map(|value| {
                    value
                        .split(';')
                        .filter(|root| !root.is_empty())
                        .map(PathBuf::from)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let skin = env::var_os("PSTART_SKIN")
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| Skin::from_toml(&text).ok())
                .unwrap_or(if theme_dark {
                    Skin::dark()
                } else {
                    Skin::light()
                });
            let dpi = initial_dpi();
            let font = create_ui_font(dpi);
            let apps = Arc::new(Mutex::new(AppCatalog::load(&start_menu_roots())));
            let app_plugin = load_app_plugin(Arc::clone(&apps));
            let (query_request_tx, query_request_rx) = mpsc::channel();
            let worker_socket = socket_path.clone();
            let worker_outcome = query_tx.clone();
            let worker_apps = Arc::clone(&apps);
            let worker_plugin = app_plugin.clone();
            let worker_hwnd = hwnd as isize;
            thread::spawn(move || {
                query_worker(
                    query_request_rx,
                    worker_outcome,
                    worker_socket,
                    worker_apps,
                    worker_plugin,
                    worker_hwnd,
                );
            });
            Self {
                hwnd,
                edit: null_mut(),
                query_rx,
                query_request_tx,
                scheduler: QueryScheduler::default(),
                calculator: Calculator::new().ok(),
                calculator_enabled,
                action_runtime: ComponentRuntime::new()
                    .with_capabilities(CapabilitySet::from_bits(OPEN | COPY)),
                action_enabled,
                action_timeout_ms,
                pending_action: None,
                action_roots,
                history,
                aliases,
                history_tx,
                query_started: None,
                result_generation: 0,
                history_cache: None,
                history_cursor: None,
                pending_history_query: None,
                history_enabled,
                apps,
                result_limit,
                theme_dark,
                skin,
                items: Vec::new(),
                selected: None,
                scroll: 0,
                dpi,
                font,
                paint_dc: null_mut(),
                paint_bitmap: 0,
                paint_old_bitmap: 0,
                paint_width: 0,
                paint_height: 0,
                status: "Type to search".to_owned(),
                ime_composing: false,
            }
        }

        fn scale(&self, value: i32) -> i32 {
            scale_at(value, self.dpi)
        }

        fn publish_result_count(&self) {
            if env::var_os("PSTART_SMOKE_TITLE").is_none() {
                return;
            }
            let title = if env::var_os("PSTART_SMOKE_SELECTION").is_some() {
                format!(
                    "pSTART - {} - {}",
                    self.items.len(),
                    self.selected.map_or(0, |index| index + 1)
                )
            } else {
                format!("pSTART - {}", self.items.len())
            };
            let title = if env::var_os("PSTART_SMOKE_GENERATION").is_some() {
                format!("{title} - g{}", self.result_generation)
            } else {
                title
            };
            let title = wide(&title);
            // SAFETY: hwnd is the live overlay window.
            unsafe {
                SetWindowTextW(self.hwnd, title.as_ptr());
            }
        }

        fn invalidate_pending(&mut self) {
            self.scheduler.invalidate();
        }

        fn query_from_edit(&mut self) {
            // SAFETY: edit is a live child control for this state.
            let length = unsafe { GetWindowTextLengthW(self.edit) };
            if length <= 0 {
                self.invalidate_pending();
                self.items.clear();
                self.selected = None;
                self.scroll = 0;
                self.status = "Type to search".to_owned();
                self.publish_result_count();
                return;
            }
            let mut buffer = vec![0u16; (length + 1) as usize];
            // SAFETY: buffer has capacity for the text plus terminator.
            let copied = unsafe { GetWindowTextW(self.edit, buffer.as_mut_ptr(), length + 1) };
            let text = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
            let text = text.trim();
            if text.is_empty() {
                self.invalidate_pending();
                self.items.clear();
                self.selected = None;
                self.scroll = 0;
                self.status = "Type to search".to_owned();
                self.publish_result_count();
                return;
            }
            let generation = self.scheduler.begin();
            self.query_started = Some(Instant::now());
            if let Some(path) = text.strip_prefix("/backup ") {
                if path.is_empty() {
                    self.status = "Usage: /backup <path>".to_owned();
                    return;
                }
                let result = match &self.history {
                    Some(store) => store.backup_to(std::path::Path::new(path)),
                    None => Err(pstart_host::HistoryError::InvalidPath),
                };
                match result {
                    Ok(()) => {
                        self.status = format!("Backed up: {path}");
                        // SAFETY: hwnd is the live overlay.
                        unsafe {
                            ShowWindow(self.hwnd, SW_HIDE);
                        }
                    }
                    Err(error) => self.status = format!("Backup failed: {error}"),
                }
                return;
            }
            if let Some(path) = text.strip_prefix("/backup-delete ") {
                if path.is_empty() {
                    self.status = "Usage: /backup-delete <path>".to_owned();
                    return;
                }
                let result = match &self.history {
                    Some(store) => store.delete_restore_backup(std::path::Path::new(path)),
                    None => Err(pstart_host::HistoryError::InvalidPath),
                };
                self.status = match result {
                    Ok(()) => format!("Backup deleted: {path}"),
                    Err(error) => format!("Delete failed: {error}"),
                };
                return;
            }
            if let Some(path) = text.strip_prefix("/restore ") {
                if path.is_empty() {
                    self.status = "Usage: /restore <path>".to_owned();
                    return;
                }
                self.show_action_confirmation(ActionRequest {
                    open: None,
                    copy: None,
                    restore: Some(path.to_owned()),
                });
                return;
            }
            if let Some(root) = text.strip_prefix("/allow ") {
                if root.is_empty() {
                    self.status = "Usage: /allow <path>".to_owned();
                    return;
                }
                match std::fs::canonicalize(root) {
                    Ok(path) => {
                        if !self.action_roots.contains(&path) {
                            self.action_roots.push(path.clone());
                        }
                        let joined = self
                            .action_roots
                            .iter()
                            .map(|root| root.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join(";");
                        if let Some(store) = &mut self.history {
                            let _ = store.set_setting("plugin.action_roots", &joined);
                        }
                        self.status = format!("Allowed: {}", path.display());
                    }
                    Err(error) => self.status = format!("Allow failed: {error}"),
                }
                return;
            }
            if text.eq_ignore_ascii_case("/allow-clear") {
                self.action_roots.clear();
                if let Some(store) = &mut self.history {
                    let _ = store.remove_setting("plugin.action_roots");
                }
                self.status = "Allowed roots cleared".to_owned();
                return;
            }
            if let Some(path) = text.strip_prefix("/action ") {
                if path.is_empty() {
                    self.status = "Usage: /action <path>".to_owned();
                    return;
                }
                if !self.action_enabled {
                    self.status = "Action plugin disabled".to_owned();
                    return;
                }
                let component = include_bytes!("../../../../plugins/action.wasm");
                match self.action_runtime.run_text_host_timeout(
                    component,
                    path,
                    std::time::Duration::from_millis(self.action_timeout_ms),
                ) {
                    Ok(output) => {
                        let open = output.opens.first().cloned();
                        if let Some(path) = &open {
                            if let Err(error) = pstart_host::validate_plugin_action_path(path) {
                                self.status = format!("Action denied: {error}");
                                return;
                            }
                            let canonical = std::fs::canonicalize(path).ok();
                            if canonical.as_ref().is_none_or(|path| {
                                !pstart_host::action_allowed(path, &self.action_roots)
                            }) {
                                self.status = "Action denied: outside allowed roots".to_owned();
                                return;
                            }
                        }
                        self.show_action_confirmation(ActionRequest {
                            open,
                            copy: output.copies.first().cloned(),
                            restore: None,
                        });
                    }
                    Err(error) => self.status = format!("Action failed: {error}"),
                }
                return;
            }
            if let Some(value) = text.strip_prefix("/copy ") {
                if value.is_empty() {
                    self.status = "Usage: /copy <text>".to_owned();
                    return;
                }
                match copy_to_clipboard(value) {
                    Ok(()) => {
                        self.status = "Copied".to_owned();
                        // SAFETY: hwnd is the live overlay.
                        unsafe {
                            ShowWindow(self.hwnd, SW_HIDE);
                        }
                    }
                    Err(error) => self.status = format!("Copy failed: {error}"),
                }
                return;
            }
            if let Some(path) = text.strip_prefix("/open ") {
                if path.is_empty() {
                    self.status = "Usage: /open <path>".to_owned();
                    return;
                }
                match shell_open(path) {
                    Ok(()) => {
                        self.status = format!("Opened: {path}");
                        // SAFETY: hwnd is the live overlay.
                        unsafe {
                            ShowWindow(self.hwnd, SW_HIDE);
                        }
                    }
                    Err(error) => self.status = format!("Open failed: {error}"),
                }
                return;
            }
            if let Some(arguments) = text.strip_prefix("/alias ") {
                self.save_alias(arguments);
                return;
            }
            if text.eq_ignore_ascii_case("/plugins") {
                self.show_plugins();
                return;
            }
            if text.eq_ignore_ascii_case("/settings") {
                self.show_settings();
                return;
            }
            if let Some(result) = self.calculator_result(text) {
                self.record_history(text);
                self.items = vec![FileItem {
                    id: FileId::new(VolumeId::new("plugin").expect("fixed volume"), Frn::new(0)),
                    name: format!("= {result}"),
                    path: format!("plugin://calculator/{result}"),
                    attributes: 0,
                    is_dir: false,
                }];
                self.selected = Some(0);
                self.scroll = 0;
                self.status = format!("= {result}");
                self.publish_result_count();
                // SAFETY: hwnd is the live overlay.
                unsafe {
                    InvalidateRect(self.hwnd, null(), 0);
                }
                return;
            }
            self.status = "Searching...".to_owned();
            self.record_history(text);
            self.pending_history_query = Some(text.to_owned());
            let text = self.resolve_alias(text);
            let _ = self.query_request_tx.send(QueryRequest {
                generation,
                text,
                limit: self.result_limit,
            });
        }

        fn poll_query(&mut self) -> (bool, bool) {
            let mut changed = false;
            while let Ok(outcome) = self.query_rx.try_recv() {
                if !self.scheduler.accept(outcome.generation) {
                    continue;
                }
                let worker_us = outcome.worker_us;
                match outcome.result {
                    Ok((response, syncing)) => {
                        self.pending_history_query = None;
                        let truncated = response.truncated;
                        self.items = response.items;
                        self.selected = (!self.items.is_empty()).then_some(0);
                        self.scroll = 0;
                        self.status = result_status(self.items.len(), syncing, truncated);
                    }
                    Err(error) => {
                        self.pending_history_query = None;
                        self.status = query_failure_status(&error);
                        self.items.clear();
                        self.selected = None;
                        self.scroll = 0;
                    }
                }
                self.result_generation = self.result_generation.wrapping_add(1);
                self.publish_result_count();
                changed = true;
                if let Some(started) = self.query_started.take() {
                    if std::env::var_os("PSTART_UI_LATENCY").is_some() {
                        eprintln!(
                            "ui_latency_us={} worker_us={}",
                            started.elapsed().as_micros(),
                            worker_us
                        );
                    }
                }
            }
            (changed, self.scheduler.is_pending())
        }

        fn record_history(&mut self, query: &str) {
            if !self.history_enabled {
                return;
            }
            if let Some(cache) = &self.history_cache {
                let recalled = self
                    .history_cursor
                    .and_then(|index| cache.get(index))
                    .is_some_and(|entry| entry == query);
                if !recalled {
                    self.history_cache = None;
                    self.history_cursor = None;
                }
            }
            if let Some(sender) = &self.history_tx {
                let _ = sender.send(query.to_owned());
            }
        }

        fn resolve_alias(&self, text: &str) -> String {
            self.aliases
                .get(text)
                .cloned()
                .unwrap_or_else(|| text.to_owned())
        }

        fn calculator_result(&self, text: &str) -> Option<String> {
            if !self.calculator_enabled {
                return None;
            }
            let expression = text.strip_prefix('=')?;
            if expression.is_empty() {
                return None;
            }
            if expression.contains('.') {
                let value = pstart_host::evaluate_decimal(expression)
                    .map(pstart_host::format_decimal)
                    .unwrap_or_else(|error| format!("error: {error}"));
                return Some(value);
            }
            if expression.contains(['(', ')']) || expression.starts_with(['-', '+']) {
                let value = pstart_host::evaluate_expression(expression)
                    .map_or_else(|error| format!("error: {error}"), |value| value.to_string());
                return Some(value);
            }
            let index = expression.find(['+', '-', '*', '/'])?;
            let left = &expression[..index];
            let right = &expression[index + 1..];
            let max_len = if expression.as_bytes()[index] == b'*' {
                4
            } else {
                8
            };
            if left.is_empty()
                || right.is_empty()
                || left.len() > max_len
                || right.len() > max_len
                || !left.bytes().all(|byte| byte.is_ascii_digit())
                || !right.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            self.calculator
                .as_ref()
                .and_then(|calculator| calculator.evaluate(expression).ok())
        }

        fn record_mru(&mut self, path: &str) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis() as i64);
            if let Some(history) = &mut self.history {
                let _ = history.record_mru(path, now);
            }
        }

        fn recall_history(&mut self, older: bool) -> bool {
            if !self.history_enabled {
                return false;
            }
            if self.history_cache.is_none() {
                let Some(history) = &self.history else {
                    return false;
                };
                let Ok(entries) = history.recent(pstart_host::MAX_HISTORY_RESULTS) else {
                    return false;
                };
                if entries.is_empty() {
                    return false;
                }
                self.history_cache = Some(entries.into_iter().map(|entry| entry.query).collect());
            }
            let Some(entries) = &self.history_cache else {
                return false;
            };
            if entries.is_empty() {
                return false;
            }
            let next = if older {
                self.history_cursor
                    .map_or(Some(0), |index| Some((index + 1).min(entries.len() - 1)))
            } else {
                self.history_cursor.and_then(|index| index.checked_sub(1))
            };
            let Some(index) = next else {
                // SAFETY: edit is a live child.
                unsafe {
                    SetWindowTextW(self.edit, wide("").as_ptr());
                }
                self.history_cursor = None;
                return true;
            };
            self.history_cursor = Some(index);
            let text = wide(&entries[index]);
            // SAFETY: edit is a live child; setting text triggers EN_CHANGE.
            unsafe {
                SetWindowTextW(self.edit, text.as_ptr());
                SetFocus(self.edit);
            }
            true
        }

        fn delete_history_entry(&mut self) -> bool {
            let Some(index) = self.history_cursor else {
                return false;
            };
            let Some(query) = self
                .history_cache
                .as_ref()
                .and_then(|cache| cache.get(index))
                .cloned()
            else {
                return false;
            };
            if let Some(history) = &mut self.history {
                if history.remove(&query).is_err() {
                    return false;
                }
            }
            let Some(cache) = self.history_cache.as_mut() else {
                return false;
            };
            cache.remove(index);
            let next = if cache.is_empty() {
                None
            } else {
                Some(index.saturating_sub(1).min(cache.len() - 1))
            };
            self.history_cursor = next;
            let text = next
                .and_then(|index| cache.get(index))
                .cloned()
                .unwrap_or_default();
            // SAFETY: edit is a live child; setting text triggers EN_CHANGE.
            unsafe {
                SetWindowTextW(self.edit, wide(&text).as_ptr());
                SetFocus(self.edit);
            }
            true
        }

        fn move_selection(&mut self, delta: isize) {
            if let Some(next) =
                pstart_host::selection::move_selection(self.selected, self.items.len(), delta)
            {
                self.select_index(next);
            }
        }

        fn select_index(&mut self, index: usize) {
            let Some(index) = pstart_host::selection::selection_index(index, self.items.len())
            else {
                return;
            };
            self.selected = Some(index);
            self.scroll =
                pstart_host::selection::scroll_for_selection(self.scroll, index, VISIBLE_ROWS);
            self.clamp_scroll();
            self.publish_result_count();
        }

        fn scroll_by(&mut self, delta: isize) {
            self.scroll = pstart_host::selection::scroll_by(
                self.scroll,
                self.items.len(),
                VISIBLE_ROWS,
                delta,
            );
        }

        fn clamp_scroll(&mut self) {
            self.scroll = self
                .scroll
                .min(self.items.len().saturating_sub(VISIBLE_ROWS));
        }

        fn show_action_confirmation(&mut self, request: ActionRequest) {
            self.scheduler.invalidate();
            let label = request
                .open
                .clone()
                .or_else(|| request.copy.clone())
                .or_else(|| request.restore.clone())
                .unwrap_or_default();
            self.pending_action = Some(request);
            self.items = vec![
                FileItem {
                    id: FileId::new(VolumeId::new("confirm").expect("fixed volume"), Frn::new(0)),
                    name: format!("Run: {label}"),
                    path: "confirm://execute".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(VolumeId::new("confirm").expect("fixed volume"), Frn::new(1)),
                    name: "Cancel".to_owned(),
                    path: "confirm://cancel".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
            ];
            self.selected = Some(0);
            self.scroll = 0;
            self.status = "Confirm action".to_owned();
            self.publish_result_count();
            // SAFETY: hwnd is the live overlay.
            unsafe {
                InvalidateRect(self.hwnd, null(), 0);
            }
        }

        fn execute_action(&mut self) {
            let Some(request) = self.pending_action.take() else {
                self.status = "No pending action".to_owned();
                return;
            };
            if let Some(path) = request.restore {
                let pre_backup =
                    self.history
                        .as_ref()
                        .and_then(|store| store.path())
                        .map(|database| {
                            let millis = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_or(0, |duration| duration.as_millis());
                            database.with_extension(format!("pre-restore-{millis}.db"))
                        });
                if let Some(target) = &pre_backup {
                    if let Some(store) = &self.history {
                        if let Err(error) = store.backup_to(target) {
                            self.status = format!("Restore failed: pre-backup: {error}");
                            return;
                        }
                        let _ = store.prune_pre_restore_backups(5);
                    }
                }
                let result = match &mut self.history {
                    Some(store) => store.restore_from(std::path::Path::new(&path)),
                    None => Err(pstart_host::HistoryError::InvalidPath),
                };
                match result {
                    Ok(()) => {
                        self.reload_settings();
                        self.show_settings();
                        self.status = "Restored".to_owned();
                    }
                    Err(error) => self.status = format!("Restore failed: {error}"),
                }
                return;
            }
            let mut failure = None;
            if let Some(copy) = request.copy {
                if let Err(error) = copy_to_clipboard(&copy) {
                    failure = Some(format!("Copy failed: {error}"));
                }
            }
            if failure.is_none() {
                if let Some(open) = request.open {
                    if let Err(error) = shell_open(&open) {
                        failure = Some(format!("Open failed: {error}"));
                    }
                }
            }
            if let Some(error) = failure {
                self.status = error;
            } else {
                self.status = "Action completed".to_owned();
                // SAFETY: hwnd is the live overlay.
                unsafe {
                    ShowWindow(self.hwnd, SW_HIDE);
                }
            }
        }

        fn save_alias(&mut self, arguments: &str) {
            let mut parts = arguments.split_whitespace();
            let (Some(name), Some(target), None) = (parts.next(), parts.next(), parts.next())
            else {
                self.status = "Usage: /alias <name> <target>".to_owned();
                return;
            };
            let Some(store) = &mut self.history else {
                self.status = "History unavailable".to_owned();
                return;
            };
            if store.set_alias(name, target).is_err() {
                self.status = "Alias rejected".to_owned();
                return;
            }
            self.aliases.insert(name.to_owned(), target.to_owned());
            self.show_settings();
        }

        fn reload_settings(&mut self) {
            if let Some(value) = self
                .history
                .as_ref()
                .and_then(|store| store.get_setting("result_limit").ok().flatten())
            {
                if let Ok(limit) = value.parse::<u32>() {
                    self.result_limit = limit.clamp(1, 200);
                }
            }
            if let Some(value) = self
                .history
                .as_ref()
                .and_then(|store| store.get_setting("history.enabled").ok().flatten())
            {
                self.history_enabled = value != "0";
            }
            if let Some(value) = self
                .history
                .as_ref()
                .and_then(|store| store.get_setting("theme").ok().flatten())
            {
                self.theme_dark = value != "light";
                self.skin = if self.theme_dark {
                    Skin::dark()
                } else {
                    Skin::light()
                };
            }
            if let Some(value) = self
                .history
                .as_ref()
                .and_then(|store| store.get_setting("plugin.calculator").ok().flatten())
            {
                self.calculator_enabled = value != "0";
            }
            self.history_cache = None;
            self.history_cursor = None;
        }

        fn show_plugins(&mut self) {
            self.scheduler.invalidate();
            let mut items = vec![
                FileItem {
                    id: FileId::new(VolumeId::new("plugins").expect("fixed volume"), Frn::new(0)),
                    name: format!(
                        "Calculator: {}",
                        if self.calculator_enabled { "On" } else { "Off" }
                    ),
                    path: "plugins://calculator".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(VolumeId::new("plugins").expect("fixed volume"), Frn::new(1)),
                    name: format!(
                        "Action component: {}",
                        if self.action_enabled { "On" } else { "Off" }
                    ),
                    path: "plugins://action".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(VolumeId::new("plugins").expect("fixed volume"), Frn::new(2)),
                    name: "WIT component: Ready".to_owned(),
                    path: "plugins://wit".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(VolumeId::new("plugins").expect("fixed volume"), Frn::new(3)),
                    name: "Host API: record/config/search/open/copy".to_owned(),
                    path: "plugins://api".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(VolumeId::new("plugins").expect("fixed volume"), Frn::new(4)),
                    name: format!("Allowed roots: {}", self.action_roots.len()),
                    path: "plugins://roots".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(VolumeId::new("plugins").expect("fixed volume"), Frn::new(5)),
                    name: format!("Action timeout: {} ms", self.action_timeout_ms),
                    path: "plugins://timeout".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
            ];
            for (index, root) in self.action_roots.iter().enumerate() {
                items.push(FileItem {
                    id: FileId::new(
                        VolumeId::new("plugins").expect("fixed volume"),
                        Frn::new(6 + index as u64),
                    ),
                    name: format!("Root: {}", root.display()),
                    path: format!("plugins://root/{index}"),
                    attributes: 0,
                    is_dir: false,
                });
            }
            items.push(FileItem {
                id: FileId::new(
                    VolumeId::new("plugins").expect("fixed volume"),
                    Frn::new(1024),
                ),
                name: format!("pSTART {}", env!("CARGO_PKG_VERSION")),
                path: "plugins://about".to_owned(),
                attributes: 0,
                is_dir: false,
            });
            self.items = items;
            self.selected = Some(0);
            self.scroll = 0;
            self.status = "Plugins".to_owned();
            self.publish_result_count();
            // SAFETY: hwnd is the live overlay.
            unsafe {
                InvalidateRect(self.hwnd, null(), 0);
            }
        }

        fn show_settings(&mut self) {
            self.scheduler.invalidate();
            self.history_cache = None;
            self.history_cursor = None;
            let history = if self.history_enabled { "On" } else { "Off" };
            let aliases = self
                .history
                .as_ref()
                .and_then(|store| store.aliases(10).ok())
                .unwrap_or_default();
            let mut items = vec![
                FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(0),
                    ),
                    name: format!("History: {history}"),
                    path: "settings://history".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(1),
                    ),
                    name: format!("Result limit: {}", self.result_limit),
                    path: "settings://limit".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(2),
                    ),
                    name: "Clear history".to_owned(),
                    path: "settings://clear-history".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(3),
                    ),
                    name: format!("Theme: {}", if self.theme_dark { "Dark" } else { "Light" }),
                    path: "settings://theme".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(4),
                    ),
                    name: format!(
                        "Calculator: {}",
                        if self.calculator_enabled { "On" } else { "Off" }
                    ),
                    path: "settings://calculator".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
                FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(5),
                    ),
                    name: "Plugins".to_owned(),
                    path: "settings://plugins".to_owned(),
                    attributes: 0,
                    is_dir: false,
                },
            ];
            for (index, (name, target)) in aliases.into_iter().enumerate() {
                items.push(FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(6 + index as u64),
                    ),
                    name: format!("Alias: {name} -> {target}"),
                    path: format!("settings://alias/{name}"),
                    attributes: 0,
                    is_dir: false,
                });
            }
            let backups = self
                .history
                .as_ref()
                .and_then(|store| store.pre_restore_backups(5).ok())
                .unwrap_or_default();
            for (index, path) in backups.into_iter().enumerate() {
                items.push(FileItem {
                    id: FileId::new(
                        VolumeId::new("settings").expect("fixed volume"),
                        Frn::new(100 + index as u64),
                    ),
                    name: format!(
                        "Backup: {}",
                        path.file_name()
                            .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
                    ),
                    path: format!("backup://{}", path.display()),
                    attributes: 0,
                    is_dir: false,
                });
            }
            items.push(FileItem {
                id: FileId::new(
                    VolumeId::new("settings").expect("fixed volume"),
                    Frn::new(1024),
                ),
                name: format!("pSTART {}", env!("CARGO_PKG_VERSION")),
                path: "settings://about".to_owned(),
                attributes: 0,
                is_dir: false,
            });
            self.items = items;
            self.selected = Some(0);
            self.scroll = 0;
            self.status = "Settings".to_owned();
            self.publish_result_count();
            // SAFETY: hwnd is the live overlay.
            unsafe {
                InvalidateRect(self.hwnd, null(), 0);
            }
        }

        fn toggle_setting(&mut self, key: &str) {
            match key {
                "history" => {
                    self.history_enabled = !self.history_enabled;
                    if let Some(store) = &mut self.history {
                        let value = if self.history_enabled { "1" } else { "0" };
                        let _ = store.set_setting("history.enabled", value);
                    }
                }
                "limit" => {
                    self.result_limit = match self.result_limit {
                        50 => 100,
                        100 => 200,
                        _ => 50,
                    };
                    if let Some(store) = &mut self.history {
                        let _ = store.set_setting("result_limit", &self.result_limit.to_string());
                    }
                }
                "clear-history" => {
                    if let Some(store) = &mut self.history {
                        let _ = store.clear_history();
                    }
                    self.history_cache = None;
                    self.history_cursor = None;
                }
                "theme" => {
                    self.theme_dark = !self.theme_dark;
                    self.skin = if self.theme_dark {
                        Skin::dark()
                    } else {
                        Skin::light()
                    };
                    if let Some(store) = &mut self.history {
                        let value = if self.theme_dark { "dark" } else { "light" };
                        let _ = store.set_setting("theme", value);
                    }
                }
                "calculator" => {
                    self.calculator_enabled = !self.calculator_enabled;
                    if let Some(store) = &mut self.history {
                        let value = if self.calculator_enabled { "1" } else { "0" };
                        let _ = store.set_setting("plugin.calculator", value);
                    }
                }
                key if key.starts_with("alias/") => {
                    let name = key.trim_start_matches("alias/");
                    if let Some(store) = &mut self.history {
                        let _ = store.remove_alias(name);
                    }
                    self.aliases.remove(name);
                }
                _ => {}
            }
        }

        fn accept_selection(&mut self) {
            let Some(path) = self
                .selected
                .and_then(|index| self.items.get(index))
                .map(|item| item.path.clone())
            else {
                return;
            };
            if let Some(plugin) = path.strip_prefix("plugins://") {
                if let Some(index) = plugin.strip_prefix("root/") {
                    if let Ok(index) = index.parse::<usize>() {
                        if index < self.action_roots.len() {
                            self.action_roots.remove(index);
                        }
                    }
                    if let Some(store) = &mut self.history {
                        if self.action_roots.is_empty() {
                            let _ = store.remove_setting("plugin.action_roots");
                        } else {
                            let joined = self
                                .action_roots
                                .iter()
                                .map(|root| root.to_string_lossy())
                                .collect::<Vec<_>>()
                                .join(";");
                            let _ = store.set_setting("plugin.action_roots", &joined);
                        }
                    }
                    self.show_plugins();
                    return;
                }
                match plugin {
                    "calculator" => {
                        self.toggle_setting("calculator");
                        self.show_plugins();
                    }
                    "action" => {
                        self.action_enabled = !self.action_enabled;
                        if let Some(store) = &mut self.history {
                            let value = if self.action_enabled { "1" } else { "0" };
                            let _ = store.set_setting("plugin.action", value);
                        }
                        self.show_plugins();
                    }
                    "timeout" => {
                        self.action_timeout_ms = match self.action_timeout_ms {
                            250 => 500,
                            500 => 1000,
                            _ => 250,
                        };
                        if let Some(store) = &mut self.history {
                            let _ = store.set_setting(
                                "plugin.action_timeout_ms",
                                &self.action_timeout_ms.to_string(),
                            );
                        }
                        self.show_plugins();
                    }
                    "roots" => {
                        self.action_roots.clear();
                        if let Some(store) = &mut self.history {
                            let _ = store.remove_setting("plugin.action_roots");
                        }
                        self.show_plugins();
                    }
                    _ => {}
                }
                return;
            }
            if let Some(backup) = path.strip_prefix("backup://") {
                self.show_action_confirmation(ActionRequest {
                    open: None,
                    copy: None,
                    restore: Some(backup.to_owned()),
                });
                return;
            }
            if let Some(command) = path.strip_prefix("confirm://") {
                match command {
                    "execute" => self.execute_action(),
                    "cancel" => {
                        self.pending_action = None;
                        self.status = "Cancelled".to_owned();
                        // SAFETY: hwnd is the live overlay.
                        unsafe {
                            ShowWindow(self.hwnd, SW_HIDE);
                        }
                    }
                    _ => {}
                }
                return;
            }
            if let Some(setting) = path.strip_prefix("settings://") {
                if setting == "plugins" {
                    self.show_plugins();
                    return;
                }
                self.toggle_setting(setting);
                self.show_settings();
                return;
            }
            if let Some(value) = path.strip_prefix("plugin://calculator/") {
                match copy_to_clipboard(value) {
                    Ok(()) => {
                        self.status = format!("Copied: {value}");
                        // SAFETY: hwnd is the live overlay.
                        unsafe {
                            ShowWindow(self.hwnd, SW_HIDE);
                        }
                    }
                    Err(error) => self.status = format!("Copy failed: {error}"),
                }
                return;
            }
            match shell_open(&path) {
                Ok(()) => {
                    self.status = format!("Opened: {path}");
                    self.record_mru(&path);
                    // SAFETY: hwnd is the live overlay.
                    unsafe {
                        ShowWindow(self.hwnd, SW_HIDE);
                    }
                }
                Err(error) => {
                    self.status = format!("Open failed: {error}");
                }
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn Error>> {
        if env::args().any(|argument| matches!(argument.as_str(), "--version" | "-V" | "version")) {
            println!("pstart-host {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        enable_dpi_awareness();
        let smoke = env::args().any(|argument| argument == "--smoke");
        let mutex_name = wide("Local\\pSTART-Host-Singleton");
        // SAFETY: name is NUL-terminated and owned for the call.
        let mutex = unsafe { CreateMutexW(null(), 1, mutex_name.as_ptr()) };
        if mutex.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: GetLastError is thread-local and immediately read.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            let class = wide("PStartHostWindow");
            // SAFETY: class is NUL-terminated; title is optional.
            let existing = unsafe { FindWindowW(class.as_ptr(), null()) };
            if !existing.is_null() {
                // SAFETY: existing is the running host window.
                unsafe {
                    PostMessageW(existing, SHOW_MESSAGE, 0, 0);
                }
            }
            // SAFETY: mutex is owned by this process.
            unsafe {
                CloseHandle(mutex);
            }
            return Ok(());
        }

        // SAFETY: null requests the current executable module.
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        let class_name = wide("PStartHostWindow");
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: null_mut(),
            // SAFETY: system arrow cursor resource.
            hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };
        // SAFETY: class is initialized and class_name lives for the call.
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: all pointers are valid for the call.
        let dpi = initial_dpi();
        let width = scale_at(WIDTH, dpi);
        let height = scale_at(HEIGHT, dpi);
        let (x, y) = overlay_position(
            width,
            height,
            scale_at(INPUT_MARGIN + INPUT_HEIGHT / 2, dpi),
        );
        let title = wide("pSTART");
        // SAFETY: class is registered; title lives for the call.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                x,
                y,
                width,
                height,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        if hwnd.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        if smoke {
            let smoke_ms = env::var("PSTART_SMOKE_MS")
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(250)
                .clamp(100, 300_000);
            // SAFETY: hwnd is live.
            unsafe {
                ShowWindow(hwnd, SW_SHOW);
                UpdateWindow(hwnd);
                SetTimer(hwnd, SMOKE_TIMER, smoke_ms, None);
            }
        }

        let mut message = MSG::default();
        loop {
            // SAFETY: message is writable; null hwnd receives thread messages.
            let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
            if result == 0 {
                break;
            }
            if result < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: message was populated by GetMessageW.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        // SAFETY: mutex is owned by this process.
        unsafe {
            CloseHandle(mutex);
        }
        Ok(())
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY: user data is either null or a Box<State> installed in WM_CREATE.
        let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut State;
        match message {
            WM_CREATE => {
                let mut state = Box::new(State::new(hwnd));
                let dpi = state.dpi;
                let edit = create_edit(hwnd, dpi);
                if edit.is_null() {
                    return -1;
                }
                state.edit = edit;
                if !state.font.is_null() {
                    // SAFETY: edit is live and font is owned by State.
                    unsafe {
                        SendMessageW(edit, WM_SETFONT, state.font as usize, 1);
                    }
                }
                let pointer = Box::into_raw(state);
                // SAFETY: pointer is a leaked Box<State> stored on the window.
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer as isize);
                }
                // SAFETY: hwnd is live.
                if !install_double_ctrl_hook(hwnd) {
                    // SAFETY: pointer is valid.
                    unsafe {
                        (*pointer).status = "Double Ctrl unavailable".to_owned();
                    }
                }
                layout(hwnd, edit, dpi);
                0
            }
            WM_DPICHANGED => {
                if !pointer.is_null() {
                    let dpi = (wparam & 0xffff) as i32;
                    let suggested = lparam as *const RECT;
                    // SAFETY: pointer is a live State; suggested may be null.
                    unsafe {
                        let state = &mut *pointer;
                        state.dpi = dpi;
                        let old = state.font;
                        state.font = create_ui_font(dpi);
                        if !state.font.is_null() {
                            SendMessageW(state.edit, WM_SETFONT, state.font as usize, 1);
                        }
                        if !old.is_null() {
                            DeleteObject(old);
                        }
                        if !suggested.is_null() {
                            let rect = &*suggested;
                            SetWindowPos(
                                hwnd,
                                null_mut(),
                                rect.left,
                                rect.top,
                                rect.right - rect.left,
                                rect.bottom - rect.top,
                                SWP_NOZORDER | SWP_NOACTIVATE,
                            );
                        }
                        layout(hwnd, state.edit, dpi);
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            WM_SIZE => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    layout(hwnd, unsafe { (*pointer).edit }, unsafe { (*pointer).dpi });
                }
                0
            }
            WM_COMMAND => {
                let id = wparam & 0xffff;
                let notification = ((wparam >> 16) & 0xffff) as u32;
                if id == EDIT_ID && notification == EN_CHANGE && !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        (*pointer).query_from_edit();
                    }
                    // SAFETY: invalidate the result area.
                    unsafe {
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            QUERY_READY => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    let (changed, _) = unsafe { (*pointer).poll_query() };
                    if changed {
                        // SAFETY: invalidate the result area.
                        unsafe {
                            InvalidateRect(hwnd, null(), 0);
                        }
                    }
                }
                0
            }
            WM_HOTKEY if wparam as i32 == HOTKEY_ID => {
                // SAFETY: hwnd is live.
                let visible = unsafe { IsWindowVisible(hwnd) } != 0;
                if visible {
                    // SAFETY: hide the overlay.
                    unsafe {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                } else {
                    // SAFETY: show, focus, and reposition the overlay on the cursor monitor.
                    unsafe {
                        if !pointer.is_null() {
                            let dpi = (*pointer).dpi;
                            let width = scale_at(WIDTH, dpi);
                            let height = scale_at(HEIGHT, dpi);
                            let (x, y) = overlay_position(
                                width,
                                height,
                                scale_at(INPUT_MARGIN + INPUT_HEIGHT / 2, dpi),
                            );
                            if let Ok(mut apps) = (*pointer).apps.lock() {
                                apps.refresh(&start_menu_roots());
                            }
                            SetWindowPos(hwnd, null_mut(), x, y, width, height, SWP_NOZORDER);
                        }
                        ShowWindow(hwnd, SW_SHOWNORMAL);
                        BringWindowToTop(hwnd);
                        SetForegroundWindow(hwnd);
                        if !pointer.is_null() {
                            SetFocus((*pointer).edit);
                            SendMessageW((*pointer).edit, WM_SETFOCUS, 0, 0);
                        }
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            NAV_UP | NAV_DOWN => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        let edit_empty = GetWindowTextLengthW((*pointer).edit) <= 0;
                        if edit_empty && matches!(message, NAV_UP | NAV_DOWN) {
                            (*pointer).recall_history(message == NAV_UP);
                        } else {
                            (*pointer).move_selection(if message == NAV_UP { -1 } else { 1 });
                        }
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            NAV_PAGE_UP | NAV_PAGE_DOWN => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        (*pointer).move_selection(if message == NAV_PAGE_UP {
                            -(VISIBLE_ROWS as isize)
                        } else {
                            VISIBLE_ROWS as isize
                        });
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            NAV_HOME | NAV_END => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        let index = if message == NAV_HOME {
                            0
                        } else {
                            (*pointer).items.len().saturating_sub(1)
                        };
                        (*pointer).select_index(index);
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            NAV_ENTER => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        (*pointer).accept_selection();
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            NAV_DELETE => {
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        (*pointer).delete_history_entry();
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            NAV_ESCAPE => {
                let mut cancelled = false;
                if !pointer.is_null() {
                    // SAFETY: pointer is a live State.
                    unsafe {
                        let state = &mut *pointer;
                        if state.scheduler.is_pending() {
                            state.scheduler.invalidate();
                            if let Some(query) = state.pending_history_query.take() {
                                if let Some(history) = &mut state.history {
                                    let _ = history.remove(&query);
                                }
                            }
                            state.status = "Search cancelled".to_owned();
                            InvalidateRect(hwnd, null(), 0);
                            cancelled = true;
                        }
                    }
                }
                if !cancelled {
                    // SAFETY: hide the overlay.
                    unsafe {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
                0
            }
            SHOW_MESSAGE => {
                if !pointer.is_null() {
                    // SAFETY: activate the existing overlay.
                    unsafe {
                        ShowWindow(hwnd, SW_SHOW);
                        SetForegroundWindow(hwnd);
                        SetFocus((*pointer).edit);
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            WM_LBUTTONDOWN => {
                if !pointer.is_null() {
                    let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                    // SAFETY: pointer is a live State.
                    unsafe {
                        let list_top = (*pointer).scale(LIST_TOP);
                        if y >= list_top {
                            let row = ((y - list_top) / (*pointer).scale(ROW_HEIGHT)) as usize;
                            let index = (*pointer).scroll + row;
                            if index < (*pointer).items.len() {
                                (*pointer).select_index(index);
                                InvalidateRect(hwnd, null(), 0);
                            }
                        }
                    }
                }
                0
            }
            WM_MOUSEWHEEL => {
                if !pointer.is_null() {
                    let delta = ((wparam >> 16) & 0xffff) as i16 as i32;
                    // SAFETY: pointer is a live State.
                    unsafe {
                        (*pointer).scroll_by(if delta > 0 { -3 } else { 3 });
                        InvalidateRect(hwnd, null(), 0);
                    }
                }
                0
            }
            WM_ACTIVATE => {
                let action = (wparam & 0xffff) as u32;
                if action == WA_INACTIVE {
                    // SAFETY: hide when focus leaves the overlay.
                    unsafe {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
                0
            }
            WM_ERASEBKGND => 1,
            WM_PAINT => {
                paint(hwnd, pointer);
                0
            }
            WM_TIMER if wparam == SMOKE_TIMER => {
                // SAFETY: timer is owned by this window.
                unsafe {
                    KillTimer(hwnd, SMOKE_TIMER);
                    PostQuitMessage(0);
                }
                0
            }
            WM_KEYDOWN => {
                if wparam == VK_ESCAPE as usize {
                    // SAFETY: hide the overlay.
                    unsafe {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
                0
            }
            WM_DESTROY => {
                uninstall_double_ctrl_hook();
                if !pointer.is_null() {
                    // SAFETY: pointer is live; GDI objects are owned by State.
                    unsafe {
                        release_paint_buffer(&mut *pointer);
                    }
                    // SAFETY: pointer is live; font is owned by State.
                    unsafe {
                        if !(*pointer).font.is_null() {
                            DeleteObject((*pointer).font);
                            (*pointer).font = null_mut();
                        }
                    }
                    // SAFETY: reclaim the State exactly once.
                    unsafe {
                        drop(Box::from_raw(pointer));
                    }
                    // SAFETY: clear user data after freeing.
                    unsafe {
                        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    }
                }
                // SAFETY: end the message loop.
                unsafe {
                    PostQuitMessage(0);
                }
                0
            }
            _ => {
                // SAFETY: default processing for all other messages.
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
        }
    }

    unsafe extern "system" fn edit_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let parent = unsafe { GetParent(hwnd) };
        let pointer = unsafe { GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut State };
        if matches!(
            message,
            WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION | WM_IME_ENDCOMPOSITION
        ) && !pointer.is_null()
        {
            // SAFETY: pointer is the live State stored on the parent.
            unsafe {
                match message {
                    WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
                        (*pointer).ime_composing = true;
                    }
                    WM_IME_ENDCOMPOSITION => (*pointer).ime_composing = false,
                    _ => {}
                }
            }
        }
        let tracked = if pointer.is_null() {
            false
        } else {
            // SAFETY: pointer is the live State stored on the parent.
            unsafe { (*pointer).ime_composing }
        };
        if message == WM_KEYDOWN && !is_ime_composing(hwnd) && !tracked {
            let navigation = match wparam as u16 {
                VK_UP => Some(NAV_UP),
                VK_DOWN => Some(NAV_DOWN),
                VK_PRIOR => Some(NAV_PAGE_UP),
                VK_NEXT => Some(NAV_PAGE_DOWN),
                VK_HOME => Some(NAV_HOME),
                VK_END => Some(NAV_END),
                VK_DELETE if unsafe { GetWindowTextLengthW(hwnd) } <= 0 => Some(NAV_DELETE),
                VK_RETURN => Some(NAV_ENTER),
                VK_ESCAPE => Some(NAV_ESCAPE),
                _ => None,
            };
            if let Some(navigation) = navigation {
                // SAFETY: parent receives a user message.
                unsafe {
                    PostMessageW(parent, navigation, 0, 0);
                }
                return 0;
            }
        }
        let original = ORIGINAL_EDIT_PROC.load(Ordering::Relaxed);
        // SAFETY: stored value came from SetWindowLongPtrW for this control.
        let original: WNDPROC = unsafe { std::mem::transmute(original) };
        // SAFETY: forwarding the original message to the original procedure.
        unsafe { CallWindowProcW(original, hwnd, message, wparam, lparam) }
    }

    unsafe fn create_edit(parent: HWND, dpi: i32) -> HWND {
        let class = wide("EDIT");
        let empty = wide("");
        let margin = scale_at(16, dpi);
        // SAFETY: class/empty are NUL-terminated for the call.
        let edit = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                empty.as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_BORDER | (ES_LEFT as u32) | (ES_AUTOHSCROLL as u32),
                margin,
                margin,
                scale_at(688, dpi),
                scale_at(30, dpi),
                parent,
                EDIT_ID as *mut _,
                GetModuleHandleW(null()),
                null(),
            )
        };
        if !edit.is_null() {
            // SAFETY: edit is live.
            unsafe {
                SendMessageW(
                    edit,
                    WM_SETFONT,
                    GetStockObject(DEFAULT_GUI_FONT) as usize,
                    1,
                );
                let original =
                    SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as usize as isize);
                ORIGINAL_EDIT_PROC.store(original, Ordering::Relaxed);
            }
        }
        edit
    }

    unsafe fn layout(hwnd: HWND, edit: HWND, dpi: i32) {
        let mut client = RECT::default();
        // SAFETY: client is writable and hwnd is live.
        unsafe {
            GetClientRect(hwnd, &mut client);
        }
        let width = client.right - client.left;
        if !edit.is_null() {
            // SAFETY: edit is a live child.
            unsafe {
                MoveWindow(edit, 0, 0, width, scale_at(INPUT_HEIGHT, dpi), 1);
            }
        }
    }

    unsafe fn release_paint_buffer(state: &mut State) {
        // SAFETY: cached GDI objects belong to this state.
        unsafe {
            if !state.paint_dc.is_null() {
                if state.paint_bitmap != 0 {
                    SelectObject(state.paint_dc, state.paint_old_bitmap as _);
                    DeleteObject(state.paint_bitmap as _);
                }
                DeleteDC(state.paint_dc);
            }
        }
        state.paint_dc = null_mut();
        state.paint_bitmap = 0;
        state.paint_old_bitmap = 0;
        state.paint_width = 0;
        state.paint_height = 0;
    }

    unsafe fn ensure_paint_buffer(state: &mut State, hdc: HDC, width: i32, height: i32) -> HDC {
        if !state.paint_dc.is_null() && state.paint_width == width && state.paint_height == height {
            return state.paint_dc;
        }
        // SAFETY: cached GDI objects belong to this state.
        unsafe {
            release_paint_buffer(state);
        }
        if width <= 0 || height <= 0 {
            return hdc;
        }
        // SAFETY: dimensions come from the live client rect.
        let mem_dc = unsafe { CreateCompatibleDC(hdc) };
        let bitmap = if mem_dc.is_null() {
            null_mut()
        } else {
            // SAFETY: dimensions come from the live client rect.
            unsafe { CreateCompatibleBitmap(hdc, width, height) }
        };
        if mem_dc.is_null() || bitmap.is_null() {
            // SAFETY: only non-null objects are released.
            unsafe {
                if !bitmap.is_null() {
                    DeleteObject(bitmap);
                }
                if !mem_dc.is_null() {
                    DeleteDC(mem_dc);
                }
            }
            return hdc;
        }
        // SAFETY: bitmap is a live GDI object and mem_dc is a live DC.
        let old_bitmap = unsafe { SelectObject(mem_dc, bitmap) };
        state.paint_dc = mem_dc;
        state.paint_bitmap = bitmap as isize;
        state.paint_old_bitmap = old_bitmap as isize;
        state.paint_width = width;
        state.paint_height = height;
        mem_dc
    }

    unsafe fn paint(hwnd: HWND, pointer: *mut State) {
        let mut paint = PAINTSTRUCT::default();
        // SAFETY: paint is writable and hwnd is live.
        let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
        let mut client = RECT::default();
        // SAFETY: client is writable.
        unsafe {
            GetClientRect(hwnd, &mut client);
        }
        // SAFETY: pointer is either null or a live State.
        let skin = if pointer.is_null() {
            Skin::dark()
        } else {
            unsafe { (*pointer).skin }
        };
        let width = client.right - client.left;
        let height = client.bottom - client.top;
        let target = if pointer.is_null() {
            hdc
        } else {
            // SAFETY: pointer is a live State.
            unsafe { ensure_paint_buffer(&mut *pointer, hdc, width, height) }
        };
        // SAFETY: GDI brush creation and fill.
        unsafe {
            let background = CreateSolidBrush(skin.background);
            FillRect(target, &client, background);
            DeleteObject(background);
        }
        if !pointer.is_null() {
            // SAFETY: pointer is a live State.
            let state = unsafe { &*pointer };
            // SAFETY: stock font selected for the duration of the paint.
            unsafe {
                let font = if state.font.is_null() {
                    GetStockObject(DEFAULT_GUI_FONT)
                } else {
                    state.font
                };
                let old = SelectObject(target, font);
                SetBkMode(target, TRANSPARENT as i32);
                for (index, item) in state
                    .items
                    .iter()
                    .enumerate()
                    .skip(state.scroll)
                    .take(VISIBLE_ROWS)
                {
                    let top = state.scale(LIST_TOP)
                        + (index - state.scroll) as i32 * state.scale(ROW_HEIGHT);
                    if state.selected == Some(index) {
                        let selected = RECT {
                            left: state.scale(16),
                            top: top - state.scale(4),
                            right: client.right - state.scale(16),
                            bottom: top + state.scale(40),
                        };
                        let brush = CreateSolidBrush(state.skin.selection);
                        FillRect(target, &selected, brush);
                        DeleteObject(brush);
                    }
                    draw_text(target, &item.name, state.scale(24), top, state.skin.text);
                    draw_text(
                        target,
                        &item.path,
                        state.scale(24),
                        top + state.scale(20),
                        state.skin.path,
                    );
                }
                if state.items.is_empty() {
                    draw_text(
                        target,
                        &state.status,
                        state.scale(24),
                        state.scale(LIST_TOP),
                        rgb(180, 180, 190),
                    );
                } else {
                    draw_text(
                        target,
                        &state.status,
                        state.scale(24),
                        client.bottom - state.scale(24),
                        rgb(140, 140, 150),
                    );
                }
                SelectObject(target, old);
            }
        }
        // SAFETY: blit the completed buffer and release the paint DC.
        unsafe {
            if target != hdc {
                BitBlt(hdc, 0, 0, width, height, target, 0, 0, SRCCOPY);
            }
            EndPaint(hwnd, &paint);
        }
    }

    unsafe fn draw_text(hdc: HDC, text: &str, x: i32, y: i32, color: COLORREF) {
        const DISPLAY_UNITS: usize = 48;
        let mut utf16: Vec<u16> = text.encode_utf16().take(DISPLAY_UNITS).collect();
        if text.encode_utf16().count() > DISPLAY_UNITS {
            utf16.truncate(DISPLAY_UNITS - 2);
            utf16.push(0x2026);
        }
        // SAFETY: hdc is valid for painting; utf16 pointer/length are valid.
        unsafe {
            SetTextColor(hdc, color);
            TextOutW(hdc, x, y, utf16.as_ptr(), utf16.len() as i32);
        }
    }

    fn history_path() -> Option<PathBuf> {
        if let Some(path) = env::var_os("PSTART_HOST_DB") {
            return Some(PathBuf::from(path));
        }
        let base = env::var_os("LOCALAPPDATA")?;
        let directory = PathBuf::from(base).join("pSTART");
        std::fs::create_dir_all(&directory).ok()?;
        Some(directory.join("host.db"))
    }

    fn enable_dpi_awareness() {
        // SAFETY: process-wide DPI setting, called before any window.
        if unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } == 0
        {
            // SAFETY: fallback for older Windows.
            unsafe {
                SetProcessDPIAware();
            }
        }
    }

    fn initial_dpi() -> i32 {
        if let Ok(value) = env::var("PSTART_FORCE_DPI") {
            if let Ok(dpi) = value.parse::<i32>() {
                if (96..=384).contains(&dpi) {
                    return dpi;
                }
            }
        }
        // SAFETY: no arguments.
        let dpi = unsafe { GetDpiForSystem() };
        if dpi == 0 {
            96
        } else {
            dpi as i32
        }
    }

    fn scale_at(value: i32, dpi: i32) -> i32 {
        value * dpi / 96
    }

    fn overlay_position(width: i32, height: i32, input_anchor_y: i32) -> (i32, i32) {
        // SAFETY: all pointers refer to initialized locals.
        let (left, top, right, bottom) = unsafe {
            let mut cursor = POINT { x: 0, y: 0 };
            if GetCursorPos(&mut cursor) != 0 {
                let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
                if !monitor.is_null() {
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        rcMonitor: RECT {
                            left: 0,
                            top: 0,
                            right: 0,
                            bottom: 0,
                        },
                        rcWork: RECT {
                            left: 0,
                            top: 0,
                            right: 0,
                            bottom: 0,
                        },
                        dwFlags: 0,
                    };
                    if GetMonitorInfoW(monitor, &mut info) != 0 {
                        (
                            info.rcWork.left,
                            info.rcWork.top,
                            info.rcWork.right,
                            info.rcWork.bottom,
                        )
                    } else {
                        (
                            0,
                            0,
                            GetSystemMetrics(SM_CXSCREEN),
                            GetSystemMetrics(SM_CYSCREEN),
                        )
                    }
                } else {
                    (
                        0,
                        0,
                        GetSystemMetrics(SM_CXSCREEN),
                        GetSystemMetrics(SM_CYSCREEN),
                    )
                }
            } else {
                (
                    0,
                    0,
                    GetSystemMetrics(SM_CXSCREEN),
                    GetSystemMetrics(SM_CYSCREEN),
                )
            }
        };
        place_in_work_area(left, top, right, bottom, width, height, input_anchor_y)
    }

    fn place_in_work_area(
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        width: i32,
        height: i32,
        input_anchor_y: i32,
    ) -> (i32, i32) {
        let x = left + ((right - left - width) / 2).max(0);
        let center_y = top + (bottom - top) / 2;
        let y = (center_y - input_anchor_y).clamp(top, (bottom - height).max(top));
        (x, y)
    }

    fn create_ui_font(dpi: i32) -> HFONT {
        let face = wide("Segoe UI");
        // SAFETY: face is NUL-terminated and lives for the call.
        unsafe {
            CreateFontW(
                -scale_at(16, dpi),
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                u32::from(DEFAULT_CHARSET),
                u32::from(OUT_DEFAULT_PRECIS),
                u32::from(CLIP_DEFAULT_PRECIS),
                u32::from(CLEARTYPE_QUALITY),
                u32::from(DEFAULT_PITCH | FF_DONTCARE),
                face.as_ptr(),
            )
        }
    }

    fn is_ime_composing(hwnd: HWND) -> bool {
        // SAFETY: hwnd is a live EDIT control; context is released below.
        let context = unsafe { ImmGetContext(hwnd) };
        if context.is_null() {
            return false;
        }
        // SAFETY: null buffer with zero length queries the required size.
        let bytes = unsafe { ImmGetCompositionStringW(context, GCS_COMPSTR, null_mut(), 0) };
        // SAFETY: context came from ImmGetContext for this hwnd.
        unsafe {
            ImmReleaseContext(hwnd, context);
        }
        bytes > 0
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
        u32::from(red) | (u32::from(green) << 8) | (u32::from(blue) << 16)
    }

    fn copy_to_clipboard(text: &str) -> Result<(), String> {
        let mut opened = false;
        for _ in 0..20 {
            // SAFETY: null owner requests the current task clipboard.
            if unsafe { OpenClipboard(null_mut()) } != 0 {
                opened = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        if !opened {
            return Err("clipboard busy".to_owned());
        }
        let result = (|| {
            // SAFETY: clipboard is open and owned by this thread.
            if unsafe { EmptyClipboard() } == 0 {
                return Err("EmptyClipboard failed".to_owned());
            }
            let text = wide(text);
            let bytes = text.len() * std::mem::size_of::<u16>();
            // SAFETY: allocates a movable block without initializing it.
            let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
            if handle.is_null() {
                return Err("GlobalAlloc failed".to_owned());
            }
            // SAFETY: handle is a fresh movable allocation.
            let pointer = unsafe { GlobalLock(handle) } as *mut u16;
            if pointer.is_null() {
                // SAFETY: handle came from GlobalAlloc and is not owned elsewhere.
                unsafe {
                    GlobalFree(handle);
                }
                return Err("GlobalLock failed".to_owned());
            }
            // SAFETY: the block is at least `bytes` and pointer is writable.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
                GlobalUnlock(handle);
            }
            // SAFETY: clipboard owns the handle on success.
            if unsafe { SetClipboardData(CF_UNICODETEXT as u32, handle) }.is_null() {
                // SAFETY: SetClipboardData failed, so ownership remains here.
                unsafe {
                    GlobalFree(handle);
                }
                return Err("SetClipboardData failed".to_owned());
            }
            Ok(())
        })();
        // SAFETY: balances OpenClipboard.
        unsafe {
            CloseClipboard();
        }
        result
    }

    fn shell_open(path: &str) -> Result<(), String> {
        let path = pstart_host::validate_open_path(path).map_err(|error| error.to_string())?;
        if env::var_os("PSTART_OPEN_DRY_RUN").is_some() {
            return Ok(());
        }
        let operation = wide("open");
        let file = wide(&path.to_string_lossy());
        // SAFETY: operation/file are NUL-terminated and live for the call.
        let result = unsafe {
            ShellExecuteW(
                null_mut(),
                operation.as_ptr(),
                file.as_ptr(),
                null(),
                null(),
                1,
            )
        };
        if result as isize <= 32 {
            return Err(format!(
                "ShellExecuteW failed for {}: code {}",
                path.display(),
                result as isize
            ));
        }
        Ok(())
    }

    fn query_failure_status(error: &ClientError) -> String {
        if error.is_offline() {
            "Server offline".to_owned()
        } else if matches!(error, ClientError::Server(code) if *code == ErrorCode::ServerBusy.as_u32())
        {
            "Indexing...".to_owned()
        } else if matches!(error, ClientError::Server(code) if *code == ErrorCode::Internal.as_u32())
        {
            "Server error; restart pSTART-Server".to_owned()
        } else {
            format!("Query failed: {error}")
        }
    }

    fn result_status(count: usize, syncing: bool, truncated: bool) -> String {
        match (count, syncing, truncated) {
            (0, true, _) => "Indexing... no results yet".to_owned(),
            (0, false, true) => "No results shown (more available)".to_owned(),
            (0, false, false) => "No results".to_owned(),
            (count, true, true) => format!("{count} result(s) (indexing, more available)"),
            (count, true, false) => format!("{count} result(s) (indexing)"),
            (count, false, true) => format!("{count} result(s) (more available)"),
            (count, false, false) => format!("{count} result(s)"),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{place_in_work_area, query_failure_status, result_status};
        use pstart_host::ClientError;

        #[test]
        fn center_on_primary_work_area() {
            assert_eq!(
                place_in_work_area(0, 0, 1920, 1040, 720, 440, 32),
                (600, 488)
            );
        }

        #[test]
        fn center_on_negative_monitor() {
            assert_eq!(
                place_in_work_area(-1920, 0, 0, 1040, 720, 440, 32),
                (-1320, 488)
            );
        }

        #[test]
        fn clamps_to_small_work_area() {
            assert_eq!(place_in_work_area(0, 0, 600, 300, 720, 440, 32), (0, 0));
        }

        #[test]
        fn plugin_output_only_accepts_existing_shortcuts() {
            let dir = std::env::temp_dir().join(format!("pstart-plugin-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let lnk = dir.join("Notepad.lnk");
            let exe = dir.join("Notepad.exe");
            std::fs::write(&lnk, b"x").unwrap();
            std::fs::write(&exe, b"x").unwrap();
            let text = format!(
                "{}\n{}\n{}\n",
                lnk.to_string_lossy(),
                exe.to_string_lossy(),
                lnk.to_string_lossy()
            );
            let items = super::parse_plugin_apps(&text, 10);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].name, "Notepad");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn offline_errors_are_classified() {
            let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
            assert_eq!(
                query_failure_status(&ClientError::from(io)),
                "Server offline"
            );
            assert_eq!(
                query_failure_status(&ClientError::Server(15)),
                "Query failed: server error code 15"
            );
            assert_eq!(
                query_failure_status(&ClientError::Server(
                    pstart_core::ErrorCode::ServerBusy.as_u32()
                )),
                "Indexing..."
            );
            assert_eq!(
                query_failure_status(&ClientError::Server(
                    pstart_core::ErrorCode::Internal.as_u32()
                )),
                "Server error; restart pSTART-Server"
            );
        }

        #[test]
        fn result_status_mentions_indexing() {
            assert_eq!(result_status(0, false, false), "No results");
            assert_eq!(result_status(0, true, false), "Indexing... no results yet");
            assert_eq!(result_status(3, false, false), "3 result(s)");
            assert_eq!(result_status(3, true, false), "3 result(s) (indexing)");
            assert_eq!(
                result_status(0, false, true),
                "No results shown (more available)"
            );
            assert_eq!(
                result_status(3, false, true),
                "3 result(s) (more available)"
            );
            assert_eq!(
                result_status(3, true, true),
                "3 result(s) (indexing, more available)"
            );
        }
    }
}
