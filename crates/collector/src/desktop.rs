use crate::{
    auth_types::{CodexSource, Provider},
    control::{Command, SharedUi, UiBridge, UiState},
};
use std::{
    cell::{Cell, RefCell},
    env, mem, ptr,
    sync::{Arc, Mutex},
    thread,
};
use tokio::sync::mpsc;
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{
        CreateFontW, CreateSolidBrush, DeleteObject, GetSysColorBrush, InvalidateRect, SetBkColor,
        SetTextColor, UpdateWindow, COLOR_BTNFACE, DEFAULT_CHARSET, HBRUSH, HDC, HFONT,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{BST_CHECKED, BST_UNCHECKED, EM_EMPTYUNDOBUFFER, EM_SETLIMITTEXT},
        HiDpi::{
            AdjustWindowRectExForDpi, GetDpiForSystem, SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
        },
        Input::KeyboardAndMouse::EnableWindow,
        Shell::{
            ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
            NOTIFYICONDATAW,
        },
        WindowsAndMessaging::*,
    },
};
use zeroize::Zeroizing;

const CLASS: &str = "LlmQuotaMonitorNative";
const STATUS: u32 = WM_APP + 1;
const STOPPED: u32 = WM_APP + 2;
const TRAY: u32 = WM_APP + 3;
const CLAUDE_IN: usize = 101;
const CLAUDE_OUT: usize = 102;
const GROK_IN: usize = 103;
const GROK_OUT: usize = 104;
const REFRESH: usize = 105;
const DASHBOARD: usize = 106;
const CODEX: usize = 107;
const CODE: usize = 108;
const SUBMIT: usize = 109;
const CANCEL: usize = 110;
const QUIT: usize = 111;
const RESTORE: usize = 112;
const CODEX_IN: usize = 113;
const CODEX_OUT: usize = 114;
const SOURCE_OAUTH: usize = 115;
const SOURCE_LB: usize = 116;
const KIMI_IN: usize = 117;
const KIMI_OUT: usize = 118;
// MAKEINTRESOURCEW(1): an integer resource identifier, never dereferenced as a pointer.
#[allow(clippy::manual_dangling_ptr)]
const APP_ICON: windows_sys::core::PCWSTR = 1_usize as *const u16;
const EDIT_LIMIT: usize = 8192;
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn scale(value: i32, dpi: u32) -> i32 {
    (value * dpi as i32 + 48) / 96
}
fn notice(state: &SharedUi, message: &str) {
    state
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .notice = message.into();
}

struct App {
    hwnd: HWND,
    font: HFONT,
    controls: Vec<(usize, HWND)>,
    labels: [HWND; 4],
    error_flags: [bool; 4],
    error_brush: HBRUSH,
    service: HWND,
    prompt: HWND,
    notice: HWND,
    state: SharedUi,
    commands: mpsc::Sender<Command>,
    worker: Option<thread::JoinHandle<()>>,
    tray: bool,
    quitting: bool,
    stopped: bool,
    last_service: String,
    last_notice: String,
    last_prompt: String,
    last_rows: [String; 4],
    last_errors: [bool; 4],
    taskbar_created: u32,
}
impl App {
    fn control(&self, id: usize) -> HWND {
        self.controls
            .iter()
            .find(|(key, _)| *key == id)
            .map_or(ptr::null_mut(), |(_, hwnd)| *hwnd)
    }
    unsafe fn child(
        &mut self,
        id: usize,
        class: &str,
        text: &str,
        style: u32,
        rect: [i32; 4],
        dpi: u32,
    ) -> Result<HWND, Box<dyn std::error::Error>> {
        let hwnd = CreateWindowExW(
            if id == CODE { WS_EX_CLIENTEDGE } else { 0 },
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            scale(rect[0], dpi),
            scale(rect[1], dpi),
            scale(rect[2], dpi),
            scale(rect[3], dpi),
            self.hwnd,
            id as HMENU,
            GetModuleHandleW(ptr::null()),
            ptr::null(),
        );
        if hwnd.is_null() {
            return Err("Cannot create a native Windows control".into());
        }
        SendMessageW(hwnd, WM_SETFONT, self.font as usize, 1);
        self.controls.push((id, hwnd));
        Ok(hwnd)
    }
    unsafe fn add_tray(&mut self) {
        let mut icon: NOTIFYICONDATAW = mem::zeroed();
        icon.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        icon.hWnd = self.hwnd;
        icon.uID = 1;
        icon.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        icon.uCallbackMessage = TRAY;
        icon.hIcon = LoadIconW(GetModuleHandleW(ptr::null()), APP_ICON);
        let tip = wide("LLM Quota Monitor - двойной щелчок: открыть");
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        self.tray = Shell_NotifyIconW(NIM_ADD, &icon) != 0;
        if !self.tray {
            notice(
                &self.state,
                "Трей недоступен: закрытие окна завершит приложение.",
            );
        }
    }
    unsafe fn remove_tray(&mut self) {
        if self.tray {
            let mut icon: NOTIFYICONDATAW = mem::zeroed();
            icon.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
            icon.hWnd = self.hwnd;
            icon.uID = 1;
            Shell_NotifyIconW(NIM_DELETE, &icon);
            self.tray = false;
        }
    }
    unsafe fn open(&self, url: &str) {
        if ShellExecuteW(
            self.hwnd,
            wide("open").as_ptr(),
            wide(url).as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        ) as isize
            <= 32
        {
            notice(&self.state,"Не удалось открыть системный браузер. Проверьте браузер по умолчанию и повторите вход.");
        }
    }
    fn send(&self, command: Command) -> bool {
        match self.commands.try_send(command) {
            Ok(()) => true,
            Err(_) => {
                notice(&self.state,"Команда не отправлена: сервис занят или остановлен. Повторите после завершения запроса.");
                false
            }
        }
    }
    unsafe fn clear_code(&self) {
        SetWindowTextW(self.control(CODE), wide("").as_ptr());
        SendMessageW(self.control(CODE), EM_EMPTYUNDOBUFFER, 0, 0);
    }
    unsafe fn shutdown(&mut self) {
        if self.quitting {
            return;
        }
        self.clear_code();
        if self.stopped {
            DestroyWindow(self.hwnd);
            return;
        }
        if self.send(Command::Shutdown) {
            self.quitting = true;
            notice(
                &self.state,
                "Завершение: ожидаем текущий запрос и сохраняем токены…",
            );
        }
        self.update();
    }
    unsafe fn update(&mut self) {
        let (running, busy, manual, service, message, rows, errors, prompt, codex_source) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let rows: [String; 4] = std::array::from_fn(|index| {
                format!(
                    "{}\r\n{}\r\n{}",
                    ["Claude", "Codex / GPT", "Grok", "Kimi"][index],
                    state.providers[index].auth_status,
                    state.providers[index].quota_status
                )
            });
            let errors: [bool; 4] = std::array::from_fn(|index| state.providers[index].quota_error);
            (
                state.service_running,
                state.busy,
                state.manual_code,
                state.api_status.clone(),
                state.notice.clone(),
                rows,
                errors,
                state.prompt.take(),
                state.codex_source,
            )
        };
        if self.last_service != service {
            SetWindowTextW(self.service, wide(&service).as_ptr());
            self.last_service = service;
        }
        if self.last_notice != message {
            SetWindowTextW(self.notice, wide(&message).as_ptr());
            self.last_notice = message;
        }
        for (index, row) in rows.into_iter().enumerate() {
            if self.last_rows[index] != row {
                SetWindowTextW(self.labels[index], wide(&row).as_ptr());
                self.last_rows[index] = row;
            }
            self.error_flags[index] = errors[index];
            if self.last_errors[index] != errors[index] {
                InvalidateRect(self.labels[index], ptr::null(), 1);
                self.last_errors[index] = errors[index];
            }
        }
        for id in [
            CLAUDE_IN,
            CLAUDE_OUT,
            GROK_IN,
            GROK_OUT,
            KIMI_IN,
            KIMI_OUT,
            REFRESH,
            CODEX_IN,
            CODEX_OUT,
            SOURCE_OAUTH,
            SOURCE_LB,
        ] {
            EnableWindow(
                self.control(id),
                i32::from(running && busy.is_none() && !self.quitting),
            );
        }
        for id in [DASHBOARD, CODEX] {
            EnableWindow(self.control(id), i32::from(running && !self.quitting));
        }
        let oauth = codex_source == CodexSource::OAuth;
        for id in [CODEX_IN, CODEX_OUT] {
            ShowWindow(self.control(id), if oauth { SW_SHOW } else { SW_HIDE });
        }
        ShowWindow(self.control(CODEX), if oauth { SW_HIDE } else { SW_SHOW });
        SendMessageW(
            self.control(SOURCE_OAUTH),
            BM_SETCHECK,
            if oauth { BST_CHECKED } else { BST_UNCHECKED } as usize,
            0,
        );
        SendMessageW(
            self.control(SOURCE_LB),
            BM_SETCHECK,
            if oauth { BST_UNCHECKED } else { BST_CHECKED } as usize,
            0,
        );
        for id in [CODE, SUBMIT] {
            EnableWindow(
                self.control(id),
                i32::from(manual && busy.is_some() && !self.quitting),
            );
        }
        EnableWindow(
            self.control(CANCEL),
            i32::from(busy.is_some() && !self.quitting),
        );
        EnableWindow(self.control(QUIT), i32::from(!self.quitting));
        if !manual {
            self.clear_code();
        }
        if let Some(prompt) = prompt {
            let instructions = format!("{} OAuth: {}", prompt.provider.name(), prompt.instructions);
            if self.last_prompt != instructions {
                SetWindowTextW(self.prompt, wide(&instructions).as_ptr());
                self.last_prompt = instructions;
            }
            self.open(&prompt.url);
        } else if busy.is_none() && !self.last_prompt.is_empty() {
            SetWindowTextW(
                self.prompt,
                wide(
                    "Авторизация выполняется только в системном браузере. Пароль сюда не вводить.",
                )
                .as_ptr(),
            );
            self.last_prompt.clear();
        }
    }
    unsafe fn action(&mut self, id: usize) {
        match id {
            CLAUDE_IN => {
                self.send(Command::Login(Provider::Claude));
            }
            CLAUDE_OUT => {
                self.send(Command::Logout(Provider::Claude));
            }
            GROK_IN => {
                self.send(Command::Login(Provider::Grok));
            }
            GROK_OUT => {
                self.send(Command::Logout(Provider::Grok));
            }
            KIMI_IN => {
                self.send(Command::Login(Provider::Kimi));
            }
            KIMI_OUT => {
                self.send(Command::Logout(Provider::Kimi));
            }
            CODEX_IN => {
                self.send(Command::Login(Provider::Codex));
            }
            CODEX_OUT => {
                self.send(Command::Logout(Provider::Codex));
            }
            SOURCE_OAUTH => {
                self.send(Command::SetCodexSource(CodexSource::OAuth));
            }
            SOURCE_LB => {
                self.send(Command::SetCodexSource(CodexSource::CodexLb));
            }
            REFRESH => {
                self.send(Command::Refresh);
            }
            DASHBOARD => {
                let address = env::var("QUOTA_BIND").unwrap_or_else(|_| "0.0.0.0:8765".into());
                if let Ok(mut address) = address.parse::<std::net::SocketAddr>() {
                    if address.ip().is_unspecified() {
                        address.set_ip(match address.ip() {
                            std::net::IpAddr::V4(_) => "127.0.0.1".parse().unwrap(),
                            std::net::IpAddr::V6(_) => "::1".parse().unwrap(),
                        });
                    }
                    self.open(&format!("http://{address}/"));
                } else {
                    notice(&self.state, "Некорректный адрес локального HTTP-сервиса.");
                }
            }
            CODEX => {
                let url = self
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .codex_url
                    .clone();
                if let Some(url) = url {
                    self.open(&url);
                }
            }
            SUBMIT => {
                let length =
                    GetWindowTextLengthW(self.control(CODE)).clamp(0, EDIT_LIMIT as i32) as usize;
                let mut buffer = Zeroizing::new(vec![0_u16; length + 1]);
                let count =
                    GetWindowTextW(self.control(CODE), buffer.as_mut_ptr(), buffer.len() as i32)
                        .max(0) as usize;
                let code = String::from_utf16(&buffer[..count]).map(Zeroizing::new);
                self.clear_code();
                match code {
                    Ok(code) if !code.trim().is_empty() => {
                        self.send(Command::SubmitCode(code));
                    }
                    _ => notice(
                        &self.state,
                        "Вставьте выданный OAuth код или полный callback URL; пароль не нужен.",
                    ),
                }
            }
            CANCEL => {
                self.clear_code();
                self.send(Command::CancelLogin);
            }
            QUIT => self.shutdown(),
            RESTORE => {
                ShowWindow(self.hwnd, SW_RESTORE);
                SetForegroundWindow(self.hwnd);
            }
            _ => {}
        }
        self.update();
    }
    unsafe fn tray_menu(&mut self) {
        let menu = CreatePopupMenu();
        if menu.is_null() {
            return;
        }
        AppendMenuW(menu, MF_STRING, RESTORE, wide("Открыть монитор").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
        AppendMenuW(menu, MF_STRING, QUIT, wide("Выход").as_ptr());
        let mut position: POINT = mem::zeroed();
        GetCursorPos(&mut position);
        SetForegroundWindow(self.hwnd);
        let command = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            position.x,
            position.y,
            0,
            self.hwnd,
            ptr::null(),
        );
        DestroyMenu(menu);
        PostMessageW(self.hwnd, WM_NULL, 0, 0);
        if command > 0 {
            self.action(command as usize);
        }
    }
}
impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            self.remove_tray();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if !self.font.is_null() {
            unsafe {
                DeleteObject(self.font);
            }
        }
        if !self.error_brush.is_null() {
            unsafe {
                DeleteObject(self.error_brush);
            }
        }
    }
}
// ShellExecute/menus and control notifications may re-enter WndProc. Defer those messages
// rather than creating overlapping mutable references to App across Win32 calls.
struct WindowData {
    app: RefCell<App>,
    pending: RefCell<Vec<(u32, WPARAM, LPARAM)>>,
    overflow: Cell<bool>,
}
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_DESTROY {
        PostQuitMessage(0);
        return 0;
    }
    if message == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowData;
    if pointer.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    if message == WM_COMMAND && wparam >> 16 != BN_CLICKED as usize {
        return 0;
    }
    let window = &*pointer;
    let Ok(mut app) = window.app.try_borrow_mut() else {
        if matches!(
            message,
            STATUS | STOPPED | TRAY | WM_COMMAND | WM_CLOSE | WM_QUERYENDSESSION | WM_ENDSESSION
        ) {
            let mut pending = window.pending.borrow_mut();
            if !pending.contains(&(message, wparam, lparam)) {
                if pending.len() < 16 {
                    pending.push((message, wparam, lparam));
                } else {
                    window.overflow.set(true);
                }
            }
            return isize::from(message == WM_QUERYENDSESSION);
        }
        return DefWindowProcW(hwnd, message, wparam, lparam);
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if message == app.taskbar_created {
            app.add_tray();
            return 0;
        }
        if message == WM_CTLCOLORSTATIC {
            let control = lparam as HWND;
            if let Some(index) = app.labels.iter().position(|label| *label == control) {
                if app.error_flags[index] {
                    let hdc = wparam as HDC;
                    SetTextColor(hdc, 0x0000_00D0);
                    SetBkColor(hdc, 0x00F0_F0F0);
                    return app.error_brush as LRESULT;
                }
            }
        }
        match message {
            WM_COMMAND => {
                app.action(wparam & 0xffff);
                0
            }
            STATUS => {
                app.update();
                0
            }
            STOPPED => {
                app.stopped = true;
                app.update();
                if app.quitting {
                    app.remove_tray();
                    DestroyWindow(hwnd);
                }
                0
            }
            TRAY => {
                match lparam as u32 {
                    WM_LBUTTONDBLCLK => app.action(RESTORE),
                    WM_RBUTTONUP | WM_CONTEXTMENU => app.tray_menu(),
                    _ => {}
                }
                0
            }
            WM_CLOSE => {
                if app.tray && !app.quitting {
                    ShowWindow(hwnd, SW_HIDE);
                } else {
                    app.shutdown();
                }
                0
            }
            WM_QUERYENDSESSION => 1,
            WM_ENDSESSION => {
                if wparam != 0 {
                    app.shutdown();
                }
                0
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }));
    if window.overflow.replace(false) {
        notice(
            &app.state,
            "Слишком много команд интерфейса; повторите последнее действие.",
        );
    }
    drop(app);
    for (message, wparam, lparam) in window.pending.borrow_mut().drain(..) {
        PostMessageW(hwnd, message, wparam, lparam);
    }
    match result {
        Ok(result) => result,
        Err(_) => {
            PostQuitMessage(1);
            0
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
        let instance = GetModuleHandleW(ptr::null());
        let class = wide(CLASS);
        let app_icon = LoadIconW(instance, APP_ICON);
        if app_icon.is_null() {
            return Err("Cannot load the embedded application icon".into());
        }
        let descriptor = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            hIcon: app_icon,
            hCursor: LoadCursorW(ptr::null_mut(), IDC_ARROW),
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszClassName: class.as_ptr(),
            ..mem::zeroed()
        };
        if RegisterClassW(&descriptor) == 0 {
            return Err("Cannot register the native window class".into());
        }
        let dpi = GetDpiForSystem().max(96);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: scale(740, dpi),
            bottom: scale(626, dpi),
        };
        if AdjustWindowRectExForDpi(&mut rect, style, 0, 0, dpi) == 0 {
            return Err("Cannot calculate the native window size".into());
        }
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("LLM Quota Monitor").as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            UnregisterClassW(class.as_ptr(), instance);
            return Err("Cannot create the native quota window".into());
        }
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, app_icon as isize);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, app_icon as isize);
        let state = Arc::new(Mutex::new(UiState::default()));
        let (commands, receiver) = mpsc::channel(16);
        let mut app = App {
            hwnd,
            font: ptr::null_mut(),
            controls: Vec::with_capacity(20),
            labels: [ptr::null_mut(); 4],
            error_flags: [false; 4],
            error_brush: ptr::null_mut(),
            service: ptr::null_mut(),
            prompt: ptr::null_mut(),
            notice: ptr::null_mut(),
            state: Arc::clone(&state),
            commands,
            worker: None,
            tray: false,
            quitting: false,
            stopped: false,
            last_service: String::new(),
            last_notice: String::new(),
            last_prompt: String::new(),
            last_rows: std::array::from_fn(|_| String::new()),
            last_errors: [false; 4],
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
        };
        let setup = (|| -> Result<(), Box<dyn std::error::Error>> {
            app.font = CreateFontW(
                -scale(14, dpi),
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                0,
                0,
                wide("Segoe UI").as_ptr(),
            );
            if app.font.is_null() {
                return Err("Cannot create the native interface font".into());
            }
            app.error_brush = CreateSolidBrush(0x00F0_F0F0);
            if app.error_brush.is_null() {
                return Err("Cannot create the native error highlight brush".into());
            }
            app.child(
                200,
                "STATIC",
                "LLM Quota Monitor - подписочные лимиты",
                0,
                [16, 14, 700, 24],
                dpi,
            )?;
            app.service = app.child(
                201,
                "STATIC",
                "Запуск локального HTTP-сервиса…",
                0,
                [16, 43, 700, 38],
                dpi,
            )?;
            for (index, y) in [86, 166, 246, 326].into_iter().enumerate() {
                app.labels[index] = app.child(
                    210 + index,
                    "STATIC",
                    ["Claude", "Codex / GPT", "Grok", "Kimi"][index],
                    0,
                    [16, y, 492, 80],
                    dpi,
                )?;
            }
            for (id, text, rect) in [
                (CLAUDE_IN, "Войти Claude", [520, 90, 126, 30]),
                (CLAUDE_OUT, "Выйти", [652, 90, 72, 30]),
                (CODEX, "Открыть CodexLB", [520, 199, 204, 30]),
                (CODEX_IN, "Войти Codex", [520, 199, 126, 30]),
                (CODEX_OUT, "Выйти", [652, 199, 72, 30]),
                (GROK_IN, "Войти Grok", [520, 250, 126, 30]),
                (GROK_OUT, "Выйти", [652, 250, 72, 30]),
                (KIMI_IN, "Войти Kimi", [520, 330, 126, 30]),
                (KIMI_OUT, "Выйти", [652, 330, 72, 30]),
                (SUBMIT, "Отправить код", [520, 456, 126, 28]),
                (CANCEL, "Отмена", [652, 456, 72, 28]),
                (REFRESH, "Обновить квоты", [16, 546, 144, 32]),
                (DASHBOARD, "Открыть dashboard", [172, 546, 172, 32]),
                (QUIT, "Выход", [612, 546, 112, 32]),
            ] {
                app.child(
                    id,
                    "BUTTON",
                    text,
                    WS_TABSTOP | BS_PUSHBUTTON as u32,
                    rect,
                    dpi,
                )?;
            }
            app.child(
                SOURCE_OAUTH,
                "BUTTON",
                "OAuth",
                WS_TABSTOP | WS_GROUP | BS_AUTORADIOBUTTON as u32,
                [520, 170, 94, 24],
                dpi,
            )?;
            app.child(
                SOURCE_LB,
                "BUTTON",
                "CodexLB",
                WS_TABSTOP | BS_AUTORADIOBUTTON as u32,
                [618, 170, 106, 24],
                dpi,
            )?;
            app.prompt = app.child(
                220,
                "STATIC",
                "Авторизация выполняется только в системном браузере. Пароль сюда не вводить.",
                0,
                [16, 410, 708, 42],
                dpi,
            )?;
            let edit = app.child(
                CODE,
                "EDIT",
                "",
                WS_TABSTOP | ES_AUTOHSCROLL as u32 | ES_PASSWORD as u32,
                [16, 456, 492, 28],
                dpi,
            )?;
            SendMessageW(edit, EM_SETLIMITTEXT, EDIT_LIMIT, 0);
            app.notice = app.child(221, "STATIC", "", 0, [16, 490, 708, 50], dpi)?;
            app.child(
                222,
                "STATIC",
                "Закрытие окна - в трей. Токены хранятся только на ПК; ESP32 получает лишь квоты.",
                0,
                [16, 586, 708, 24],
                dpi,
            )?;
            Ok(())
        })();
        if let Err(error) = setup {
            DestroyWindow(hwnd);
            UnregisterClassW(class.as_ptr(), instance);
            return Err(error);
        }
        app.add_tray();
        app.update();
        let window = Box::new(WindowData {
            app: RefCell::new(app),
            pending: RefCell::new(Vec::new()),
            overflow: Cell::new(false),
        });
        SetWindowLongPtrW(
            hwnd,
            GWLP_USERDATA,
            (&*window as *const WindowData) as isize,
        );
        ShowWindow(hwnd, SW_SHOWNORMAL);
        UpdateWindow(hwnd);
        let handle = hwnd as usize;
        let notify = Arc::new(move || {
            PostMessageW(handle as HWND, STATUS, 0, 0);
        });
        let worker = thread::Builder::new()
            .name("quota-backend".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())
                        .and_then(|runtime| {
                            runtime
                                .block_on(crate::run(Some(UiBridge {
                                    state: Arc::clone(&state),
                                    commands: receiver,
                                    notify,
                                })))
                                .map_err(|error| error.to_string())
                        })
                }))
                .unwrap_or_else(|_| {
                    Err("Внутренняя ошибка сервиса; закройте приложение и запустите снова.".into())
                });
                if let Err(error) = result {
                    let mut ui = state.lock().unwrap_or_else(|poison| poison.into_inner());
                    ui.service_running = false;
                    ui.api_status = "HTTP-сервис не запущен".into();
                    ui.notice = error;
                }
                PostMessageW(handle as HWND, STOPPED, 0, 0);
            });
        match worker {
            Ok(worker) => window.app.borrow_mut().worker = Some(worker),
            Err(error) => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DestroyWindow(hwnd);
                UnregisterClassW(class.as_ptr(), instance);
                return Err(error.into());
            }
        }
        let mut message: MSG = mem::zeroed();
        let message_error = loop {
            let result = GetMessageW(&mut message, ptr::null_mut(), 0, 0);
            if result <= 0 {
                break result < 0;
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        };
        {
            let app = window.app.borrow();
            if !app.stopped {
                let _ = app.commands.blocking_send(Command::Shutdown);
            }
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        DestroyWindow(hwnd);
        drop(window);
        UnregisterClassW(class.as_ptr(), instance);
        if message_error || message.wParam != 0 {
            return Err("Native Windows message loop stopped unexpectedly".into());
        }
        Ok(())
    }
}
