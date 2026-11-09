use crate::feedback::{FeedbackMotion, HudStyle};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    DrawTextW, GetMonitorInfoW, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
    DT_CALCRECT, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, HBITMAP, HDC, HFONT,
    HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTOPRIMARY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, LoadIconW,
    MessageBoxW, PostMessageW, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    ShowWindow, TrackPopupMenu, UpdateLayeredWindow, GUITHREADINFO, HWND_TOPMOST, MB_ICONERROR,
    MB_OK, MB_SETFOREGROUND, MF_STRING, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOWNOACTIVATE, TPM_RETURNCMD, ULW_ALPHA, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY,
    WM_RBUTTONUP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

pub const WM_TRAY: u32 = WM_APP + 1;
/// キーフックが録音の開始などを通知するときに送るメッセージ。待機中の
/// メッセージ待ちを起こすためのもの
pub const WM_WAKE: u32 = WM_APP + 2;
pub const CMD_EXIT: usize = 1001;
pub const CMD_SETTINGS: usize = 1002;
pub const CMD_RECOVER_TEXT: usize = 1003;

pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
pub(crate) static EXIT_TEST_LOCK: Mutex<()> = Mutex::new(());
static RECOVERY_COUNT: AtomicUsize = AtomicUsize::new(0);
static RECOVERY_REQUESTED: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();
/// リアルタイム表示の(確定, 仮)。仮は話している途中の認識で、空のときは確定だけを出す
static CAPTION: Mutex<Option<(String, String)>> = Mutex::new(None);
/// 表示の場所。"top" か "caret"
static CAPTION_ANCHOR: Mutex<String> = Mutex::new(String::new());
static DIRTY: AtomicBool = AtomicBool::new(true);

pub fn set_recovery_count(count: usize) {
    RECOVERY_COUNT.store(count, Ordering::SeqCst);
}

pub fn take_recovery_request() -> bool {
    RECOVERY_REQUESTED.swap(false, Ordering::SeqCst)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Warn,
    Error,
    Success,
    Info,
    Startup,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    Recording {
        started: Instant,
        preparing: bool,
    },
    /// Edit モードの録音。「編集」と選んだ文の先頭を出す
    Editing {
        started: Instant,
        head: String,
        preparing: bool,
    },
    Recognizing {
        preparing: bool,
    },
    Busy {
        label: String,
        hint: Option<String>,
    },
    Notice {
        tone: Tone,
        text: String,
    },
}

impl Mode {
    fn set_engine_preparing(&mut self, preparing: bool) -> bool {
        let current = match self {
            Self::Recording { preparing, .. }
            | Self::Editing { preparing, .. }
            | Self::Recognizing { preparing } => preparing,
            Self::Busy { .. } | Self::Notice { .. } => return false,
        };
        if *current == preparing {
            return false;
        }
        *current = preparing;
        true
    }
}

fn recording_label(elapsed: Duration, editing: bool, preparing: bool) -> String {
    let secs = elapsed.as_secs();
    let prefix = if editing { "編集 " } else { "" };
    if preparing {
        format!(
            "{prefix}{}:{:02} · 認識エンジン準備中・録音継続",
            secs / 60,
            secs % 60
        )
    } else {
        format!("{prefix}{}:{:02}", secs / 60, secs % 60)
    }
}

fn recognizing_text(preparing: bool) -> (&'static str, Option<&'static str>) {
    if preparing {
        ("認識エンジンを準備中", Some("音声は録音済みです"))
    } else {
        ("認識中", None)
    }
}

const FADE_IN: Duration = Duration::from_millis(140);
const FADE_OUT: Duration = Duration::from_millis(200);
const FRAME: Duration = Duration::from_millis(17);
const RICH_SETTLE: Duration = Duration::from_millis(600);
const BARS: usize = 5;

struct Overlay {
    hwnd: isize,
    mode: Option<Mode>,
    shown_at: Instant,
    mode_started: Instant,
    style: HudStyle,
    motion: FeedbackMotion,
    motion_allowed: bool,
    hiding_since: Option<Instant>,
    visible: bool,
    level: f32,
    displayed_recording_second: Option<u64>,
    work: RECT,
    scale: f32,
    theme: String,
    palette: Palette,
    last_frame: Instant,
    fade_settled: bool,
}

fn store() -> &'static Mutex<Overlay> {
    static S: OnceLock<Mutex<Overlay>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(Overlay {
            hwnd: 0,
            mode: None,
            shown_at: Instant::now(),
            mode_started: Instant::now(),
            style: HudStyle::Standard,
            motion: FeedbackMotion::System,
            motion_allowed: motion_enabled(FeedbackMotion::System),
            hiding_since: None,
            visible: false,
            level: 0.0,
            displayed_recording_second: None,
            work: RECT::default(),
            scale: 1.0,
            theme: "hinomaru".into(),
            palette: HINOMARU,
            last_frame: Instant::now(),
            fade_settled: false,
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Overlay> {
    store().lock().unwrap_or_else(|p| p.into_inner())
}

pub fn hwnd() -> HWND {
    HWND(lock().hwnd as *mut std::ffi::c_void)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// GUI-subsystem binaries have no visible stderr; surface fatal errors to the user.
pub fn show_error(message: &str) {
    let text = wide(message);
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            windows::core::w!("OpenHinoVoice"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

pub fn create() -> Result<HWND, String> {
    unsafe {
        let taskbar_created = *TASKBAR_CREATED
            .get_or_init(|| RegisterWindowMessageW(windows::core::w!("TaskbarCreated")));
        if taskbar_created == 0 {
            return Err(format!(
                "TaskbarCreated メッセージを登録できません: {}",
                std::io::Error::last_os_error()
            ));
        }
        let hinstance = GetModuleHandleW(None)
            .map_err(|e| format!("モジュールハンドルを取得できません: {e}"))?;
        let class_name = wide("OpenHinoVoiceOverlay");
        // Win32 encodes integer resource ID 1 in this pointer-shaped parameter.
        #[allow(clippy::manual_dangling_ptr)]
        let app_icon_resource = PCWSTR(1usize as *const u16);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hIcon: LoadIconW(HINSTANCE(hinstance.0), app_icon_resource).unwrap_or_default(),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        // 下のアプリへのクリックを素通しし、フォーカスも奪わない
        let window = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide("OpenHinoVoice").as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            HINSTANCE(hinstance.0),
            None,
        )
        .map_err(|e| format!("オーバーレイを作れません: {e}"))?;
        if let Err(e) = add_tray(window) {
            let _ = DestroyWindow(window);
            return Err(e);
        }
        lock().hwnd = window.0 as isize;
        Ok(window)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayKind {
    Idle,
    Recording,
    Processing,
    Error,
}
impl TrayKind {
    fn from_mode(mode: Option<&Mode>) -> Self {
        match mode {
            Some(Mode::Recording { .. } | Mode::Editing { .. }) => Self::Recording,
            Some(Mode::Recognizing { .. } | Mode::Busy { .. }) => Self::Processing,
            Some(Mode::Notice {
                tone: Tone::Error, ..
            }) => Self::Error,
            _ => Self::Idle,
        }
    }
    fn bytes(self, light: bool) -> &'static [u8] {
        match (light, self) {
            (true, Self::Idle) => include_bytes!("../assets/branding/tray/light-idle.ico"),
            (true, Self::Recording) => {
                include_bytes!("../assets/branding/tray/light-recording.ico")
            }
            (true, Self::Processing) => {
                include_bytes!("../assets/branding/tray/light-processing.ico")
            }
            (true, Self::Error) => include_bytes!("../assets/branding/tray/light-error.ico"),
            (false, Self::Idle) => include_bytes!("../assets/branding/tray/dark-idle.ico"),
            (false, Self::Recording) => {
                include_bytes!("../assets/branding/tray/dark-recording.ico")
            }
            (false, Self::Processing) => {
                include_bytes!("../assets/branding/tray/dark-processing.ico")
            }
            (false, Self::Error) => include_bytes!("../assets/branding/tray/dark-error.ico"),
        }
    }
}
struct TrayCache {
    icon: isize,
    kind: TrayKind,
    light: bool,
    pixels: i32,
}
static TRAY_CACHE: Mutex<TrayCache> = Mutex::new(TrayCache {
    icon: 0,
    kind: TrayKind::Idle,
    light: true,
    pixels: 16,
});
static TRAY_MODE: Mutex<TrayKind> = Mutex::new(TrayKind::Idle);
static TRAY_READD: AtomicBool = AtomicBool::new(false);
static ENVIRONMENT_CHANGED: AtomicBool = AtomicBool::new(false);

const TRAY_RETRY_LIMIT: u8 = 5;

#[derive(Default)]
struct TrayRetry {
    add_needed: bool,
    environment_changed: bool,
    failures: u8,
    retry_at: Option<Instant>,
}

impl TrayRetry {
    fn invalidate(&mut self, readd: bool, environment_changed: bool) {
        self.add_needed |= readd;
        self.environment_changed |= environment_changed;
        self.failures = 0;
        self.retry_at = None;
    }

    fn ready(&self, now: Instant) -> Option<(bool, bool)> {
        if self.failures >= TRAY_RETRY_LIMIT || self.retry_at.is_some_and(|at| now < at) {
            None
        } else {
            Some((self.add_needed, self.environment_changed))
        }
    }

    fn failed(&mut self, now: Instant, requires_add: bool) {
        self.add_needed |= requires_add;
        self.failures = self.failures.saturating_add(1);
        self.retry_at = (self.failures < TRAY_RETRY_LIMIT)
            .then(|| now + Duration::from_millis(250 * (1 << (self.failures - 1))));
    }

    fn succeeded(&mut self) {
        *self = Self::default();
    }
}

fn tray_retry() -> &'static Mutex<TrayRetry> {
    static RETRY: OnceLock<Mutex<TrayRetry>> = OnceLock::new();
    RETRY.get_or_init(|| Mutex::new(TrayRetry::default()))
}

/// Retry deadlines only exist after a failed tray update and expire after five attempts.
pub fn next_deadline() -> Option<Instant> {
    tray_retry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .retry_at
}

struct TrayUpdateError {
    message: String,
    requires_add: bool,
}

/// HUD を出さない場合も同じ実処理の状態を通知領域へ反映する。
pub fn set_tray_mode(mode: Option<&Mode>) {
    let kind = TrayKind::from_mode(mode);
    let changed = {
        let mut previous = TRAY_MODE.lock().unwrap_or_else(|p| p.into_inner());
        let changed = *previous != kind;
        *previous = kind;
        changed
    };
    if changed {
        tray_retry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .invalidate(false, false);
    }
}

fn tray_appearance() -> (bool, i32) {
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    let light = if high_contrast_enabled() {
        let bg = system_palette().bg;
        u32::from(bg.0) + u32::from(bg.1) + u32::from(bg.2) > 382
    } else {
        windows_light_theme_value(windows::core::w!("SystemUsesLightTheme"))
    };
    let dpi = unsafe {
        FindWindowW(windows::core::w!("Shell_TrayWnd"), None)
            .map(|window| GetDpiForWindow(window))
            .unwrap_or(96)
            .max(96)
    };
    (light, ((16 * dpi + 48) / 96).clamp(16, 64) as i32)
}

/// ICO directory lookup is bounded; Windows creates only the selected raster size.
fn ico_frame(bytes: &[u8], desired: i32) -> Option<&[u8]> {
    if bytes.get(..4)? != [0, 0, 1, 0] {
        return None;
    }
    let count = u16::from_le_bytes([*bytes.get(4)?, *bytes.get(5)?]) as usize;
    if count > 64 {
        return None;
    }
    let mut selected = None;
    for index in 0..count {
        let entry = bytes.get(6 + index * 16..6 + (index + 1) * 16)?;
        let width = if entry[0] == 0 {
            256
        } else {
            i32::from(entry[0])
        };
        let size = u32::from_le_bytes(entry[8..12].try_into().ok()?) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().ok()?) as usize;
        let image = bytes.get(offset..offset.checked_add(size)?)?;
        // Prefer the next larger raster over upscaling a smaller one.
        let rank = if width >= desired {
            width - desired
        } else {
            512 + desired - width
        };
        if selected.as_ref().is_none_or(|(best, _)| rank < *best) {
            selected = Some((rank, image));
        }
    }
    selected.map(|(_, image)| image)
}

unsafe fn update_tray(
    window: HWND,
    readd: bool,
    environment_changed: bool,
) -> Result<(), TrayUpdateError> {
    use windows::Win32::UI::Shell::NIM_MODIFY;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateIconFromResourceEx, DestroyIcon, HICON, LR_DEFAULTCOLOR,
    };
    let kind = *TRAY_MODE.lock().unwrap_or_else(|p| p.into_inner());
    let (old_icon, old_kind, old_light, old_pixels) = {
        let cache = TRAY_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        (cache.icon, cache.kind, cache.light, cache.pixels)
    };
    let (light, pixels) = if environment_changed || old_icon == 0 {
        tray_appearance()
    } else {
        (old_light, old_pixels)
    };
    if !readd && old_icon != 0 && (kind, light, pixels) == (old_kind, old_light, old_pixels) {
        return Ok(());
    }
    let frame = ico_frame(kind.bytes(light), pixels).ok_or_else(|| TrayUpdateError {
        message: "トレイアイコンが不正です".into(),
        requires_add: false,
    })?;
    let icon = CreateIconFromResourceEx(frame, true, 0x0003_0000, pixels, pixels, LR_DEFAULTCOLOR)
        .map_err(|e| TrayUpdateError {
            message: format!("トレイアイコンを作れません: {e}"),
            requires_add: false,
        })?;
    let mut tip = [0u16; 128];
    let label: Vec<u16> = "OpenHinoVoice".encode_utf16().collect();
    tip[..label.len()].copy_from_slice(&label);
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        szTip: tip,
        ..Default::default()
    };
    if !Shell_NotifyIconW(
        if readd || old_icon == 0 {
            NIM_ADD
        } else {
            NIM_MODIFY
        },
        &data,
    )
    .as_bool()
    {
        let error = std::io::Error::last_os_error();
        let _ = DestroyIcon(icon);
        return Err(TrayUpdateError {
            message: format!("通知領域のアイコンを更新できません: {error}"),
            requires_add: readd || old_icon == 0,
        });
    }
    *TRAY_CACHE.lock().unwrap_or_else(|p| p.into_inner()) = TrayCache {
        icon: icon.0 as isize,
        kind,
        light,
        pixels,
    };
    if old_icon != 0 {
        let _ = DestroyIcon(HICON(old_icon as *mut std::ffi::c_void));
    }
    Ok(())
}

unsafe fn add_tray(window: HWND) -> Result<(), String> {
    update_tray(window, true, true).map_err(|error| error.message)
}

pub fn remove_tray() {
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};
    unsafe {
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd(),
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        let old_icon = {
            let mut cache = TRAY_CACHE.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::replace(&mut cache.icon, 0)
        };
        if old_icon != 0 {
            let _ = DestroyIcon(HICON(old_icon as *mut std::ffi::c_void));
        }
    }
}

/// 表示内容を切り替える。非表示からの表示では、いま使っているアプリのある画面の上端に出す
pub fn show(mode: Mode) {
    set_tray_mode(Some(&mode));
    let mut o = lock();
    if !o.visible || o.hiding_since.is_some() {
        let (work, scale) = unsafe { anchor_monitor() };
        o.work = work;
        o.scale = scale;
        o.palette = palette_for(&o.theme);
        o.shown_at = Instant::now();
        o.level = 0.0;
    }
    if o.mode.as_ref() != Some(&mode) {
        o.displayed_recording_second = None;
        o.mode_started = Instant::now();
    }
    o.motion_allowed = motion_enabled(o.motion);
    o.mode = Some(mode);
    o.hiding_since = None;
    o.visible = true;
    o.last_frame = Instant::now() - FRAME;
    o.fade_settled = false;
    DIRTY.store(true, Ordering::Release);
}

/// 準備状態だけを切り替え、通知・表示位置・フェードの開始時刻を保つ。
pub fn set_engine_preparing(preparing: bool) {
    if lock()
        .mode
        .as_mut()
        .is_some_and(|mode| mode.set_engine_preparing(preparing))
    {
        DIRTY.store(true, Ordering::Release);
    }
}

/// 設定の theme を渡し、現在の表示も更新する。
pub fn set_theme(theme: &str) {
    let mut o = lock();
    o.theme = theme.to_string();
    o.palette = palette_for(theme);
    DIRTY.store(true, Ordering::Release);
}

pub fn set_feedback(style: HudStyle, motion: FeedbackMotion) {
    let mut o = lock();
    o.style = style;
    o.motion = motion;
    o.motion_allowed = motion_enabled(motion);
    DIRTY.store(true, Ordering::Release);
}

pub fn motion_enabled(preference: FeedbackMotion) -> bool {
    motion_allowed(
        preference,
        system_motion_enabled() && !high_contrast_enabled(),
    )
}

fn motion_allowed(preference: FeedbackMotion, system_enabled: bool) -> bool {
    preference != FeedbackMotion::Reduced && system_enabled
}

fn system_motion_enabled() -> bool {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut enabled = BOOL(1);
    let result = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    result.is_err() || enabled.as_bool()
}

/// リアルタイム表示の文字を渡す。確定は区間の認識結果、仮は話している途中の文字。
/// 入力欄には打ち込まず、この帯だけに表示する
pub fn set_caption(confirmed: &str, partial: &str) {
    let mut caption = caption_lock();
    if caption
        .as_ref()
        .is_some_and(|(c, p)| c == confirmed && p == partial)
    {
        return;
    }
    *caption = Some((confirmed.to_string(), partial.to_string()));
    DIRTY.store(true, Ordering::Release);
}

/// リアルタイム表示を消す。録音の終わりと Esc で呼ぶ
pub fn clear_caption() {
    *caption_lock() = None;
    *CAPTION_MASKS.lock().unwrap_or_else(|p| p.into_inner()) = None;
    DIRTY.store(true, Ordering::Release);
}

/// 帯の場所を設定から渡す。"top" か "caret"。それ以外は "top" 扱いにする
pub fn set_caption_anchor(anchor: &str) {
    *CAPTION_ANCHOR.lock().unwrap_or_else(|p| p.into_inner()) = anchor.to_string();
    DIRTY.store(true, Ordering::Release);
}

fn caption_lock() -> std::sync::MutexGuard<'static, Option<(String, String)>> {
    CAPTION.lock().unwrap_or_else(|p| p.into_inner())
}

fn caption_anchor() -> String {
    CAPTION_ANCHOR
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// テスト用に帯の文字を読む
#[cfg(test)]
pub fn caption_for_test() -> Option<(String, String)> {
    caption_lock().clone()
}

/// 入力先のキャレットの画面座標(キャレット行の下端)。取れないときは None で、
/// 帯は画面の上端に出す。キー操作もクリップボードも使わない
unsafe fn caret_screen_position() -> Option<POINT> {
    let foreground = GetForegroundWindow();
    let thread = GetWindowThreadProcessId(foreground, None);
    if thread == 0 {
        return None;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if GetGUIThreadInfo(thread, &mut info).is_err() || info.hwndCaret.0.is_null() {
        return None;
    }
    let mut point = POINT {
        x: info.rcCaret.left,
        y: info.rcCaret.bottom,
    };
    if ClientToScreen(info.hwndCaret, &mut point).as_bool() {
        Some(point)
    } else {
        None
    }
}

pub fn hide() {
    set_tray_mode(None);
    let mut o = lock();
    if o.visible && o.hiding_since.is_none() {
        o.hiding_since = Some(if o.motion_allowed {
            Instant::now()
        } else {
            Instant::now() - FADE_OUT
        });
        DIRTY.store(true, Ordering::Release);
    }
}

/// フェードアウト中も更新が必要なので、完全に非表示になってからアイドル待機する。
pub fn is_visible() -> bool {
    lock().visible
}

/// 静止した通知ではタイマーを回さず、次の状態変更まで待機できる。
pub fn needs_animation() -> bool {
    let o = lock();
    o.visible
        && redraw_needed(
            o.mode.as_ref(),
            o.fade_settled,
            o.hiding_since.is_some(),
            DIRTY.load(Ordering::Acquire),
            o.motion_allowed,
            rich_settling(o.mode.as_ref(), o.style, o.mode_started.elapsed()),
        )
}

fn redraw_needed(
    mode: Option<&Mode>,
    fade_settled: bool,
    hiding: bool,
    dirty: bool,
    animate: bool,
    rich_settling: bool,
) -> bool {
    dirty
        || hiding
        || (animate && (!fade_settled || rich_settling))
        || (animate
            && matches!(
                mode,
                Some(
                    Mode::Recording { .. }
                        | Mode::Editing { .. }
                        | Mode::Recognizing { .. }
                        | Mode::Busy { .. }
                )
            ))
}

fn rich_settling(mode: Option<&Mode>, style: HudStyle, elapsed: Duration) -> bool {
    style == HudStyle::Rich
        && elapsed < RICH_SETTLE
        && matches!(
            mode,
            Some(Mode::Notice {
                tone: Tone::Success | Tone::Startup,
                ..
            })
        )
}

/// 録音中のマイクの音を渡す。表示の棒の高さに使う
pub fn push_level(samples: &[f32]) {
    if samples.is_empty() {
        return;
    }
    let target = level_from_rms(crate::audio::rms(samples));
    let mut o = lock();
    // 立ち上がりは速く、下がりはゆっくり
    let k = if target > o.level { 0.6 } else { 0.18 };
    o.level += (target - o.level) * k;
    // Audio arrival is already an event: update the clock without introducing a
    // reduced-motion animation timer or continuous meter movement.
    let second = recording_second(o.mode.as_ref());
    if o.motion_allowed || second != o.displayed_recording_second {
        DIRTY.store(true, Ordering::Release);
    }
}

fn recording_second(mode: Option<&Mode>) -> Option<u64> {
    match mode {
        Some(Mode::Recording { started, .. } | Mode::Editing { started, .. }) => {
            Some(started.elapsed().as_secs())
        }
        _ => None,
    }
}

/// メインループから毎回呼ぶ。アニメーション中だけ描き直す
pub fn tick() {
    let environment_changed = ENVIRONMENT_CHANGED.swap(false, Ordering::AcqRel);
    let window = hwnd();
    if !window.0.is_null() {
        let readd = TRAY_READD.swap(false, Ordering::AcqRel);
        let now = Instant::now();
        let attempt = {
            let mut retry = tray_retry().lock().unwrap_or_else(|p| p.into_inner());
            if readd || environment_changed {
                retry.invalidate(readd, environment_changed);
            }
            retry.ready(now)
        };
        if let Some((readd, refresh)) = attempt {
            let result = unsafe { update_tray(window, readd, refresh) };
            let mut retry = tray_retry().lock().unwrap_or_else(|p| p.into_inner());
            match result {
                Ok(()) => retry.succeeded(),
                Err(error) => {
                    retry.failed(now, error.requires_add);
                    eprintln!("{}", error.message);
                }
            }
        }
    }
    let mut o = lock();
    if environment_changed {
        o.palette = palette_for(&o.theme);
        o.motion_allowed = motion_enabled(o.motion);
        let (work, scale) = unsafe { anchor_monitor() };
        o.work = work;
        o.scale = scale;
    }
    if !o.visible || o.last_frame.elapsed() < FRAME {
        return;
    }
    if !redraw_needed(
        o.mode.as_ref(),
        o.fade_settled,
        o.hiding_since.is_some(),
        DIRTY.swap(false, Ordering::AcqRel),
        o.motion_allowed,
        rich_settling(o.mode.as_ref(), o.style, o.mode_started.elapsed()),
    ) {
        return;
    }
    o.last_frame = Instant::now();
    let window = HWND(o.hwnd as *mut std::ffi::c_void);
    if window.0.is_null() {
        return;
    }
    let fade = match o.hiding_since {
        Some(since) => {
            let p = since.elapsed().as_secs_f32() / FADE_OUT.as_secs_f32();
            if p >= 1.0 {
                o.visible = false;
                o.hiding_since = None;
                o.mode = None;
                unsafe {
                    let _ = ShowWindow(window, SW_HIDE);
                }
                return;
            }
            1.0 - ease_out(p)
        }
        None if !o.motion_allowed => 1.0,
        None => ease_out((o.shown_at.elapsed().as_secs_f32() / FADE_IN.as_secs_f32()).min(1.0)),
    };
    o.fade_settled = !o.motion_allowed || o.shown_at.elapsed() >= FADE_IN;
    o.displayed_recording_second = recording_second(o.mode.as_ref());
    let Some(mode) = o.mode.as_ref() else { return };
    let (work, scale, level, palette) = (o.work, o.scale, o.level, o.palette);
    // 字幕の文字は変わったときだけ描き直す。描き直した Bitmap は描画の間だけ借りる
    let caption_guard = caption_lock();
    let caption = caption_guard
        .as_ref()
        .filter(|(confirmed, partial)| !confirmed.trim().is_empty() || !partial.trim().is_empty());
    let masks_guard = caption.map(|(confirmed, partial)| {
        let font_px = px_of_scale(o.scale, 13.0);
        let max_w = px_of_scale(o.scale, 520.0)
            .min((work.right - work.left - px_of_scale(o.scale, 44.0)).max(1));
        let mut guard = CAPTION_MASKS.lock().unwrap_or_else(|p| p.into_inner());
        let reusable = guard.as_ref().is_some_and(|m| {
            m.key.0 == *confirmed && m.key.1 == *partial && m.font_px == font_px && m.max_w == max_w
        });
        if !reusable {
            *guard = Some(unsafe { caption_masks(confirmed, partial, font_px, max_w) });
        }
        guard
    });
    let masks = masks_guard.as_ref().map(|g| g.as_ref().unwrap());
    let visuals = Visuals {
        style: o.style,
        animate: o.motion_allowed,
        elapsed: o.mode_started.elapsed(),
        recording_elapsed: match mode {
            Mode::Recording { started, .. } | Mode::Editing { started, .. } => started.elapsed(),
            _ => Duration::ZERO,
        },
    };
    unsafe {
        present(
            window, mode, &palette, work, scale, level, fade, masks, visuals,
        )
    };
}

fn px_of_scale(scale: f32, v: f32) -> i32 {
    (v * scale).round() as i32
}

fn ease_out(p: f32) -> f32 {
    1.0 - (1.0 - p).powi(3)
}

/// -50dB で 0、-10dB で 1 になるよう対数で割り当てる。人の声は -35〜-15dB あたりに来る
fn level_from_rms(rms: f32) -> f32 {
    let db = 20.0 * rms.max(1e-6).log10();
    ((db + 50.0) / 40.0).clamp(0.0, 1.0)
}

unsafe fn anchor_monitor() -> (RECT, f32) {
    let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let _ = GetMonitorInfoW(monitor, &mut info);
    let (mut dx, mut dy) = (96u32, 96u32);
    let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
    (info.rcWork, dx as f32 / 96.0)
}

// ---- 描画 ----
// 1 ピクセル 1 つの u32(BGRA、乗算済みアルファ)の画像に描き、UpdateLayeredWindow で出す

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

#[derive(Clone, Copy)]
struct Palette {
    bg: Rgb,
    bg_alpha: f32,
    edge: Rgb,
    edge_alpha: f32,
    fg: Rgb,
    red: Rgb,
    amber: Rgb,
    shadow_alpha: f32,
    high_contrast: bool,
}

const DARK: Palette = Palette {
    bg: Rgb(24, 24, 27),
    bg_alpha: 0.92,
    edge: Rgb(255, 255, 255),
    edge_alpha: 0.12,
    fg: Rgb(255, 255, 255),
    red: Rgb(255, 69, 58),
    amber: Rgb(255, 179, 64),
    shadow_alpha: 0.30,
    high_contrast: false,
};

// 白地に赤い点で日の丸。赤は国旗の色(#BC002D)
const HINOMARU: Palette = Palette {
    bg: Rgb(252, 252, 250),
    bg_alpha: 0.97,
    edge: Rgb(0, 0, 0),
    edge_alpha: 0.10,
    fg: Rgb(28, 28, 30),
    red: Rgb(188, 0, 45),
    amber: Rgb(214, 124, 0),
    shadow_alpha: 0.22,
    high_contrast: false,
};

/// "auto" は Windows のアプリ用のライト、ダークに合わせる。それ以外はすべて日の丸
fn palette_for(theme: &str) -> Palette {
    if high_contrast_enabled() {
        system_palette()
    } else if theme == "dark" || (theme == "auto" && !windows_uses_light_theme()) {
        DARK
    } else {
        HINOMARU
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewEnvironment {
    background: Rgb,
    foreground: Rgb,
    accent: Rgb,
    warning: Rgb,
    high_contrast: bool,
    animate: bool,
}

fn environment_for(palette: Palette, animate: bool) -> PreviewEnvironment {
    PreviewEnvironment {
        background: palette.bg,
        foreground: palette.fg,
        accent: palette.red,
        warning: palette.amber,
        high_contrast: palette.high_contrast,
        animate,
    }
}

/// Read only when a settings frame is already requested; this creates no timer.
pub fn preview_environment(theme: &str, motion: FeedbackMotion) -> PreviewEnvironment {
    let palette = palette_for(theme);
    let animate = motion_allowed(motion, system_motion_enabled() && !palette.high_contrast);
    environment_for(palette, animate)
}

fn high_contrast_enabled() -> bool {
    use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut value = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            value.cbSize,
            Some((&mut value as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && value.dwFlags.contains(HCF_HIGHCONTRASTON)
}

fn system_palette() -> Palette {
    use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_WINDOW, COLOR_WINDOWTEXT};
    let rgb = |value: u32| Rgb(value as u8, (value >> 8) as u8, (value >> 16) as u8);
    let background = unsafe { rgb(GetSysColor(COLOR_WINDOW)) };
    let foreground = unsafe { rgb(GetSysColor(COLOR_WINDOWTEXT)) };
    Palette {
        bg: background,
        bg_alpha: 1.0,
        edge: foreground,
        edge_alpha: 1.0,
        fg: foreground,
        red: foreground,
        amber: foreground,
        shadow_alpha: 0.0,
        high_contrast: true,
    }
}

fn windows_uses_light_theme() -> bool {
    windows_light_theme_value(windows::core::w!("AppsUseLightTheme"))
}

fn windows_light_theme_value(name: PCWSTR) -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            name,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut std::ffi::c_void),
            Some(&mut size),
        )
    };
    err.is_err() || value != 0
}

struct Canvas {
    w: i32,
    h: i32,
    px: Vec<[f32; 4]>, // 乗算済みの b, g, r, a(0〜1)
}

thread_local! {
    static FRAME_CANVAS: RefCell<Option<Canvas>> = const { RefCell::new(None) };
}

impl Canvas {
    fn new(w: i32, h: i32) -> Canvas {
        Canvas {
            w,
            h,
            px: vec![[0.0; 4]; (w * h) as usize],
        }
    }

    fn blend(&mut self, x: i32, y: i32, c: Rgb, alpha: f32) {
        if alpha <= 0.0 || x < 0 || y < 0 || x >= self.w || y >= self.h {
            return;
        }
        let a = alpha.min(1.0);
        let p = &mut self.px[(y * self.w + x) as usize];
        let src = [
            c.2 as f32 / 255.0 * a,
            c.1 as f32 / 255.0 * a,
            c.0 as f32 / 255.0 * a,
            a,
        ];
        for i in 0..4 {
            p[i] = src[i] + p[i] * (1.0 - a);
        }
    }

    /// 角丸の長方形を符号付き距離で塗る。縁は 1 ピクセル幅でなめらかにする
    #[allow(clippy::too_many_arguments)] // Drawing primitive keeps explicit geometry.
    fn round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        r: f32,
        c: Rgb,
        alpha: f32,
        soften: f32,
    ) {
        let (x0, y0) = (
            (x - soften - 1.0).floor() as i32,
            (y - soften - 1.0).floor() as i32,
        );
        let (x1, y1) = (
            (x + w + soften + 1.0).ceil() as i32,
            (y + h + soften + 1.0).ceil() as i32,
        );
        for py in y0..y1 {
            for px in x0..x1 {
                let d = round_rect_sdf(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, r);
                let cov = coverage(d, soften);
                self.blend(px, py, c, alpha * cov);
            }
        }
    }

    /// 角丸の長方形の内側 1 ピクセルの縁だけを塗る
    #[allow(clippy::too_many_arguments)] // Drawing primitive keeps explicit geometry.
    fn stroke_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgb, alpha: f32) {
        for py in (y.floor() as i32)..((y + h).ceil() as i32) {
            for px in (x.floor() as i32)..((x + w).ceil() as i32) {
                let d = round_rect_sdf(px as f32 + 0.5, py as f32 + 0.5, x, y, w, h, r);
                self.blend(
                    px,
                    py,
                    c,
                    alpha * (coverage(d, 0.0) - coverage(d + 1.0, 0.0)),
                );
            }
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, alpha: f32) {
        self.round_rect(cx - r, cy - r, r * 2.0, r * 2.0, r, c, alpha, 0.0);
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, radius: f32, width: f32, color: Rgb, alpha: f32) {
        let outer = radius + width * 0.5 + 1.0;
        for y in (cy - outer).floor() as i32..=(cy + outer).ceil() as i32 {
            for x in (cx - outer).floor() as i32..=(cx + outer).ceil() as i32 {
                let distance =
                    ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let coverage = (width * 0.5 + 0.5 - (distance - radius).abs()).clamp(0.0, 1.0);
                self.blend(x, y, color, alpha * coverage);
            }
        }
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32), width: f32, color: Rgb, alpha: f32) {
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length_squared = dx * dx + dy * dy;
        let pad = width * 0.5 + 1.0;
        for y in (from.1.min(to.1) - pad).floor() as i32..=(from.1.max(to.1) + pad).ceil() as i32 {
            for x in
                (from.0.min(to.0) - pad).floor() as i32..=(from.0.max(to.0) + pad).ceil() as i32
            {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                let t = if length_squared > 0.0 {
                    ((px - from.0) * dx + (py - from.1) * dy) / length_squared
                } else {
                    0.0
                }
                .clamp(0.0, 1.0);
                let distance =
                    ((px - from.0 - t * dx).powi(2) + (py - from.1 - t * dy).powi(2)).sqrt();
                self.blend(
                    x,
                    y,
                    color,
                    alpha * (width * 0.5 + 0.5 - distance).clamp(0.0, 1.0),
                );
            }
        }
    }

    fn text(&mut self, mask: &TextMask, x: i32, y: i32, c: Rgb, alpha: f32) {
        for ty in 0..mask.h {
            for tx in 0..mask.w {
                let cov = mask.cov[(ty * mask.w + tx) as usize] as f32 / 255.0;
                self.blend(x + tx, y + ty, c, alpha * cov);
            }
        }
    }
}

fn tone_color(palette: &Palette, tone: Tone) -> Rgb {
    if palette.high_contrast {
        return palette.fg;
    }
    match tone {
        Tone::Warn => palette.amber,
        Tone::Error => palette.red,
        Tone::Success if palette.bg.0 < 128 => Rgb(100, 218, 160),
        Tone::Success => Rgb(17, 118, 82),
        Tone::Info => palette.fg,
        Tone::Startup => palette.red,
    }
}

fn draw_status_icon(canvas: &mut Canvas, tone: Tone, x: f32, y: f32, scale: f32, color: Rgb) {
    let point = |dx: f32, dy: f32| (x + dx * scale, y + dy * scale);
    let stroke = 1.7 * scale;
    match tone {
        Tone::Startup => {
            canvas.line(point(-5.0, -5.5), point(-5.0, 5.5), stroke, color, 1.0);
            canvas.line(point(5.0, -5.5), point(5.0, 5.5), stroke, color, 1.0);
            let wave = [
                (-5.0, 0.0),
                (-2.0, 0.0),
                (-1.0, -2.0),
                (1.0, 2.0),
                (2.0, 0.0),
                (5.0, 0.0),
            ];
            for segment in wave.windows(2) {
                canvas.line(
                    point(segment[0].0, segment[0].1),
                    point(segment[1].0, segment[1].1),
                    stroke,
                    color,
                    1.0,
                );
            }
        }
        Tone::Success => {
            canvas.line(point(-5.0, 0.0), point(-1.0, 4.0), stroke, color, 1.0);
            canvas.line(point(-1.0, 4.0), point(6.0, -4.0), stroke, color, 1.0);
        }
        Tone::Warn => {
            canvas.line(point(0.0, -6.0), point(-6.0, 5.0), stroke, color, 1.0);
            canvas.line(point(-6.0, 5.0), point(6.0, 5.0), stroke, color, 1.0);
            canvas.line(point(6.0, 5.0), point(0.0, -6.0), stroke, color, 1.0);
            canvas.line(point(0.0, -1.8), point(0.0, 0.8), stroke, color, 1.0);
            canvas.circle(x, y + 3.0 * scale, 0.8 * scale, color, 1.0);
        }
        Tone::Error => {
            canvas.stroke_circle(x, y, 6.0 * scale, stroke, color, 1.0);
            canvas.line(point(-2.0, -2.0), point(2.0, 2.0), stroke, color, 1.0);
            canvas.line(point(-2.0, 2.0), point(2.0, -2.0), stroke, color, 1.0);
        }
        Tone::Info => {
            canvas.stroke_circle(x, y, 6.0 * scale, stroke, color, 1.0);
            canvas.line(point(0.0, -0.5), point(0.0, 3.0), stroke, color, 1.0);
            canvas.circle(x, y - 3.0 * scale, 0.8 * scale, color, 1.0);
        }
    }
}

fn round_rect_sdf(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let qx = (px - cx).abs() - (w / 2.0 - r);
    let qy = (py - cy).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - r
}

/// 距離 d(内側が負)から塗りの割合を出す。soften > 0 なら影のようにぼかす
fn coverage(d: f32, soften: f32) -> f32 {
    if soften > 0.0 {
        let t = (1.0 - d / soften).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    } else {
        (0.5 - d).clamp(0.0, 1.0)
    }
}

struct TextMask {
    w: i32,
    h: i32,
    cov: Vec<u8>,
}

struct CachedText {
    text: String,
    font_px: i32,
    bold: bool,
    max_w: i32,
    mask: Arc<TextMask>,
}

static TEXT_MASKS: Mutex<Vec<CachedText>> = Mutex::new(Vec::new());

unsafe fn cached_text(text: &str, font_px: i32, bold: bool, max_w: i32) -> Arc<TextMask> {
    let mut cache = TEXT_MASKS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(entry) = cache.iter().find(|entry| {
        entry.text == text && entry.font_px == font_px && entry.bold == bold && entry.max_w == max_w
    }) {
        return Arc::clone(&entry.mask);
    }
    let mask = Arc::new(render_text(text, font_px, bold, max_w));
    if cache.len() == 8 {
        cache.remove(0);
    }
    cache.push(CachedText {
        text: text.to_owned(),
        font_px,
        bold,
        max_w,
        mask: Arc::clone(&mask),
    });
    mask
}

/// 折り返して描いた文字。長い文章は下から max_lines 行だけを残す(古い行は上へ送る)
unsafe fn render_text_wrapped(
    text: &str,
    px_height: i32,
    max_w: i32,
    max_lines: usize,
) -> TextMask {
    if text.is_empty() {
        return TextMask {
            w: 0,
            h: 0,
            cov: Vec::new(),
        };
    }
    let dc = CreateCompatibleDC(None);
    let font: HFONT = CreateFontW(
        -px_height,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1, // DEFAULT_CHARSET
        0,
        0,
        4, // ANTIALIASED_QUALITY
        0,
        PCWSTR(wide("Yu Gothic UI").as_ptr()),
    );
    let old_font = SelectObject(dc, HGDIOBJ(font.0));
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let flags = DT_WORDBREAK | DT_NOPREFIX;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: max_w,
        bottom: 0,
    };
    DrawTextW(dc, &mut buf, &mut rc, flags | DT_CALCRECT);
    let w = (rc.right - rc.left).clamp(1, max_w);
    let full_h = (rc.bottom - rc.top).max(px_height);
    let mut metrics = windows::Win32::Graphics::Gdi::TEXTMETRICW::default();
    let line_h = if windows::Win32::Graphics::Gdi::GetTextMetricsW(dc, &mut metrics).as_bool() {
        metrics.tmHeight.max(1)
    } else {
        px_height.max(1)
    };
    let h = full_h.min(line_h * max_lines as i32);
    let mut cov = vec![0u8; (w * h) as usize];
    let skip_rows = ((full_h - h) / line_h) * line_h;
    let full_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut full_bits: *mut std::ffi::c_void = std::ptr::null_mut();
    if let Ok(full_bmp) = CreateDIBSection(dc, &full_info, DIB_RGB_COLORS, &mut full_bits, None, 0)
    {
        let old_bmp = SelectObject(dc, HGDIOBJ(full_bmp.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FFFFFF));
        let mut rc = RECT {
            left: 0,
            top: -skip_rows,
            right: w,
            bottom: full_h - skip_rows,
        };
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        DrawTextW(dc, &mut buf, &mut rc, flags);
        let full_pixels = std::slice::from_raw_parts(full_bits as *const u8, (w * h * 4) as usize);
        for (i, pixel) in cov.iter_mut().enumerate() {
            *pixel = full_pixels[i * 4 + 1];
        }
        SelectObject(dc, old_bmp);
        let _ = DeleteObject(full_bmp);
    }
    SelectObject(dc, old_font);
    let _ = DeleteObject(font);
    let _ = DeleteDC(dc);
    TextMask { w, h, cov }
}

/// リアルタイム表示の帯に使う、折り返し済みの文字の Bitmap。文字が変わったときだけ描き直す
struct CaptionMasks {
    key: (String, String),
    font_px: i32,
    max_w: i32,
    confirmed: TextMask,
    partial: Option<TextMask>,
}

static CAPTION_MASKS: Mutex<Option<CaptionMasks>> = Mutex::new(None);

unsafe fn caption_masks(confirmed: &str, partial: &str, font_px: i32, max_w: i32) -> CaptionMasks {
    let confirmed_mask = render_text_wrapped(confirmed, font_px, max_w, 2);
    let partial_mask = if partial.is_empty() {
        None
    } else {
        Some(render_text_wrapped(partial, font_px, max_w, 1))
    };
    CaptionMasks {
        key: (confirmed.to_string(), partial.to_string()),
        font_px,
        max_w,
        confirmed: confirmed_mask,
        partial: partial_mask,
    }
}

/// GDI で白黒の文字を描き、その濃さを塗りの割合として取り出す。GDI は 32bit 画像のアルファを壊すため別に描く
unsafe fn render_text(text: &str, px_height: i32, bold: bool, max_w: i32) -> TextMask {
    if text.is_empty() {
        return TextMask {
            w: 0,
            h: 0,
            cov: Vec::new(),
        };
    }
    let dc = CreateCompatibleDC(None);
    let font: HFONT = CreateFontW(
        -px_height,
        0,
        0,
        0,
        if bold { 600 } else { 400 },
        0,
        0,
        0,
        1, // DEFAULT_CHARSET
        0,
        0,
        4, // ANTIALIASED_QUALITY(白黒の濃淡で描き、色にじみを出さない)
        0,
        PCWSTR(wide("Yu Gothic UI").as_ptr()),
    );
    let old_font = SelectObject(dc, HGDIOBJ(font.0));
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let flags = DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: max_w,
        bottom: px_height * 2,
    };
    DrawTextW(dc, &mut buf, &mut rc, flags | DT_CALCRECT);
    let w = (rc.right - rc.left).clamp(1, max_w);
    let h = (rc.bottom - rc.top).max(1);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut cov = vec![0u8; (w * h) as usize];
    if let Ok(bmp) = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
        let old_bmp = SelectObject(dc, HGDIOBJ(bmp.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FFFFFF));
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        let mut buf: Vec<u16> = text.encode_utf16().collect();
        DrawTextW(dc, &mut buf, &mut rc, flags);
        let pixels = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
        for i in 0..(w * h) as usize {
            cov[i] = pixels[i * 4 + 1];
        }
        SelectObject(dc, old_bmp);
        let _ = DeleteObject(bmp);
    }
    SelectObject(dc, old_font);
    let _ = DeleteObject(font);
    let _ = DeleteDC(dc);
    TextMask { w, h, cov }
}

#[derive(Debug)]
pub struct PreviewFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

#[allow(clippy::too_many_arguments)]
pub fn preview_frame(
    mode: &Mode,
    theme: &str,
    style: HudStyle,
    motion: FeedbackMotion,
    elapsed: Duration,
    level: f32,
    scale: f32,
) -> PreviewFrame {
    let scale = if scale.is_finite() {
        scale.clamp(0.5, 3.0)
    } else {
        1.0
    };
    let visuals = Visuals {
        style,
        animate: motion_enabled(motion),
        elapsed,
        recording_elapsed: elapsed,
    };
    let work = RECT {
        left: 0,
        top: 0,
        right: (1280.0 * scale) as i32,
        bottom: (720.0 * scale) as i32,
    };
    let frame =
        unsafe { render_frame(mode, &palette_for(theme), work, scale, level, None, visuals) };
    let fade = if visuals.animate {
        ease_out((elapsed.as_secs_f32() / FADE_IN.as_secs_f32()).min(1.0))
    } else {
        1.0
    };
    let mut rgba = Vec::with_capacity(frame.canvas.px.len() * 4);
    for pixel in &frame.canvas.px {
        let a = pixel[3].clamp(0.0, 1.0);
        for channel in [2, 1, 0] {
            rgba.push(if a > 0.0 {
                (pixel[channel] / a * 255.0).round().clamp(0.0, 255.0) as u8
            } else {
                0
            });
        }
        rgba.push((a * fade * 255.0).round() as u8);
    }
    let result = PreviewFrame {
        width: frame.canvas.w as usize,
        height: frame.canvas.h as usize,
        rgba,
    };
    FRAME_CANVAS.with(|slot| *slot.borrow_mut() = Some(frame.canvas));
    result
}

#[derive(Clone, Copy)]
struct Visuals {
    style: HudStyle,
    animate: bool,
    elapsed: Duration,
    recording_elapsed: Duration,
}

struct RenderedFrame {
    canvas: Canvas,
    shadow: f32,
    pill_h: f32,
    gap: f32,
    band_w: f32,
    band_h: f32,
}

#[allow(clippy::too_many_arguments)]
unsafe fn present(
    window: HWND,
    mode: &Mode,
    pal: &Palette,
    work: RECT,
    s: f32,
    level: f32,
    fade: f32,
    masks: Option<&CaptionMasks>,
    visuals: Visuals,
) {
    let frame = render_frame(mode, pal, work, s, level, masks, visuals);
    let px = |v: f32| (v * s).round();
    let (w, h) = (frame.canvas.w, frame.canvas.h);
    let (shadow, pill_h, gap, band_w, band_h) = (
        frame.shadow,
        frame.pill_h,
        frame.gap,
        frame.band_w,
        frame.band_h,
    );
    let y0 = shadow;
    // 帯の場所。"caret" は入力中の位置の近く、取れないときと "top" は画面の上端の中央。
    // 下中央はチャットの入力欄など文字を入れる場所に重なりやすいので避ける。出るときは上から少し降りてくる
    let drop = px(8.0) * (1.0 - fade);
    let band_top_in_canvas = y0 + pill_h + gap;
    let (wx, wy) = if caption_anchor() == "caret" && band_h > 0.0 {
        match unsafe { caret_screen_position() } {
            Some(caret) => {
                // 帯はキャレットの上に出す。上に space がなければ下に出す
                let band_h_px = band_h as i32;
                let above = caret.y - band_h_px - gap as i32;
                let target_y = if above >= work.top {
                    above
                } else {
                    caret.y + gap as i32
                };
                let band_w_px = band_w as i32;
                let target_x = caret.x - band_w_px / 2;
                (
                    target_x - (shadow as i32 + (w - shadow as i32 * 2 - band_w_px) / 2),
                    target_y - band_top_in_canvas as i32 - drop as i32,
                )
            }
            None => (
                work.left + ((work.right - work.left) - w) / 2,
                work.top + px(4.0) as i32 - drop as i32,
            ),
        }
    } else {
        (
            work.left + ((work.right - work.left) - w) / 2,
            work.top + px(4.0) as i32 - drop as i32,
        )
    };
    let (wx, wy) = clamp_to_work_area(wx, wy, w, h, work);
    blit(window, &frame.canvas, wx, wy, (fade * 255.0) as u8);
    FRAME_CANVAS.with(|slot| *slot.borrow_mut() = Some(frame.canvas));
}

unsafe fn render_frame(
    mode: &Mode,
    pal: &Palette,
    work: RECT,
    s: f32,
    level: f32,
    masks: Option<&CaptionMasks>,
    visuals: Visuals,
) -> RenderedFrame {
    let px = |v: f32| (v * s).round();
    let minimal = visuals.style == HudStyle::Minimal;
    let rich = visuals.style == HudStyle::Rich && !pal.high_contrast;
    let soft = visuals.style == HudStyle::Soft;
    let shadow = px(if minimal { 8.0 } else { 12.0 });
    let pill_h = px(if minimal {
        28.0
    } else if rich {
        44.0
    } else if soft {
        34.0
    } else {
        36.0
    });
    let pad = px(if minimal { 10.0 } else { 14.0 });
    let gap = px(if minimal { 6.0 } else { 9.0 });
    let level = if visuals.animate && level.is_finite() {
        level.clamp(0.0, 1.0)
    } else {
        0.35
    };
    let font_px = px(13.0) as i32;
    let max_text = (px(440.0) as i32).min(((work.right - work.left - px(150.0) as i32) / 2).max(1));

    // 中身の幅を先に決める
    let (label, hint, dot): (Option<Arc<TextMask>>, Option<Arc<TextMask>>, Option<Rgb>) = match mode
    {
        Mode::Recording { preparing, .. } => (
            Some(cached_text(
                &recording_label(visuals.recording_elapsed, false, *preparing),
                font_px,
                true,
                max_text,
            )),
            Some(cached_text(
                "Enter 確定 · Esc 取消",
                font_px,
                false,
                max_text,
            )),
            None,
        ),
        Mode::Editing {
            head, preparing, ..
        } => (
            Some(cached_text(
                &recording_label(visuals.recording_elapsed, true, *preparing),
                font_px,
                true,
                max_text,
            )),
            Some(cached_text(head, font_px, false, max_text)),
            None,
        ),
        Mode::Recognizing { preparing } => {
            let (label, hint) = recognizing_text(*preparing);
            (
                Some(cached_text(label, font_px, true, max_text)),
                hint.map(|text| cached_text(text, font_px, false, max_text)),
                None,
            )
        }
        Mode::Busy { label, hint } => (
            Some(cached_text(label, font_px, true, max_text)),
            hint.as_deref()
                .map(|h| cached_text(h, font_px, false, max_text)),
            None,
        ),
        Mode::Notice { tone, text } => (
            Some(cached_text(text, font_px, true, max_text * 2)),
            None,
            Some(tone_color(pal, *tone)),
        ),
    };
    let hint = if minimal { None } else { hint };
    let bars = if rich { 9 } else { BARS };
    let lead_w = match mode {
        Mode::Recording { .. } | Mode::Editing { .. } if minimal => px(8.0),
        Mode::Recording { .. } | Mode::Editing { .. } => {
            px(10.0) + gap + px(3.0) * bars as f32 + px(3.0) * (bars as f32 - 1.0)
        }
        Mode::Recognizing { .. } | Mode::Busy { .. } => px(if minimal { 16.0 } else { 22.0 }),
        Mode::Notice { .. } => px(16.0),
    };
    let label_w = label.as_ref().map_or(0, |m| m.w) as f32;
    let hint_w = hint.as_ref().map_or(0.0, |m| m.w as f32 + px(6.0));
    let pill_w = (pad * 2.0 + lead_w + gap + label_w + hint_w).max(px(96.0));

    // 字幕帯の大きさ。確定の 2 行と仮の 1 行を入れる
    let band_pad = px(10.0);
    let band_gap = px(6.0);
    let (confirmed_mask, partial_mask) =
        masks.map_or((None, None), |m| (Some(&m.confirmed), m.partial.as_ref()));
    let caption_shown = masks.is_some();
    let band_text_w = [confirmed_mask, partial_mask]
        .into_iter()
        .flatten()
        .map(|m| m.w as f32)
        .fold(0.0f32, f32::max);
    let band_h = if caption_shown {
        confirmed_mask.map_or(0.0, |m| m.h as f32)
            + partial_mask.map_or(0.0, |m| band_gap + m.h as f32)
            + band_pad * 2.0
    } else {
        0.0
    };
    let band_w = if caption_shown {
        band_text_w + band_pad * 2.0
    } else {
        0.0
    };

    let w = (pill_w.max(band_w) + shadow * 2.0) as i32;
    let h = (pill_h + shadow * 2.0 + band_h + if caption_shown { gap } else { 0.0 }) as i32;
    let mut c = FRAME_CANVAS
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(|| Canvas::new(w, h));
    c.w = w;
    c.h = h;
    c.px.resize((w * h) as usize, [0.0; 4]);
    c.px.fill([0.0; 4]);
    let (x0, y0) = (shadow, shadow);

    // 影、地、縁取り
    c.round_rect(
        x0,
        y0 + px(2.0),
        pill_w,
        pill_h,
        pill_h / 2.0,
        Rgb(0, 0, 0),
        pal.shadow_alpha,
        shadow,
    );
    c.round_rect(
        x0,
        y0,
        pill_w,
        pill_h,
        pill_h / 2.0,
        pal.bg,
        pal.bg_alpha,
        0.0,
    );
    c.stroke_round_rect(
        x0,
        y0,
        pill_w,
        pill_h,
        pill_h / 2.0,
        pal.edge,
        pal.edge_alpha,
    );

    let cy = y0 + pill_h / 2.0;
    let mut x = x0 + pad;
    let phase = if visuals.animate {
        visuals.elapsed.as_secs_f32()
    } else {
        0.0
    };
    match mode {
        Mode::Recording { .. } | Mode::Editing { .. } => {
            let radius = px(if minimal { 3.5 } else { 5.0 });
            let cx = x + radius;
            if rich {
                c.round_rect(
                    cx - px(10.0),
                    cy - px(10.0),
                    px(20.0),
                    px(20.0),
                    px(10.0),
                    pal.red,
                    0.1,
                    px(3.0),
                );
                if visuals.animate && phase < 0.45 {
                    let p = phase / 0.45;
                    c.stroke_circle(
                        cx,
                        cy,
                        radius + px(7.0) * ease_out(p),
                        px(1.0),
                        pal.red,
                        0.5 * (1.0 - p),
                    );
                }
            } else if !minimal && !soft && visuals.animate {
                let p = (phase % 1.4) / 1.4;
                c.circle(cx, cy, radius + px(5.0) * p, pal.red, 0.25 * (1.0 - p));
            }
            c.circle(cx, cy, radius, pal.red, 1.0);
            if minimal {
                x += lead_w + gap;
            } else {
                x += px(10.0) + gap;
                for i in 0..bars {
                    let distance = ((i as f32 / (bars - 1) as f32) * 2.0 - 1.0).abs();
                    let shape = 1.0 - distance * 0.48;
                    let wobble = if visuals.animate {
                        0.8 + 0.2
                            * (phase * if soft { 5.0 } else { 8.0 } + i as f32 * 0.8)
                                .sin()
                                .abs()
                    } else {
                        1.0
                    };
                    let height = px(4.0)
                        + px(if rich {
                            22.0
                        } else if soft {
                            13.0
                        } else {
                            14.0
                        }) * level
                            * shape
                            * wobble;
                    c.round_rect(
                        x,
                        cy - height / 2.0,
                        px(3.0),
                        height,
                        px(1.5),
                        if rich { pal.red } else { pal.fg },
                        0.9,
                        0.0,
                    );
                    x += px(6.0);
                }
                x += gap - px(3.0);
            }
        }
        Mode::Recognizing { .. } | Mode::Busy { .. } => {
            if rich {
                let cx = x + px(10.0);
                for i in 0..12 {
                    let angle = i as f32 * std::f32::consts::TAU / 12.0 + phase * 2.5;
                    let alpha = if visuals.animate {
                        0.2 + i as f32 / 12.0 * 0.7
                    } else {
                        0.7
                    };
                    c.circle(
                        cx + angle.cos() * px(7.0),
                        cy + angle.sin() * px(7.0),
                        px(1.2),
                        pal.red,
                        alpha,
                    );
                }
            } else {
                for i in 0..3 {
                    let alpha = if visuals.animate {
                        0.35 + 0.65 * ((phase * 4.0 - i as f32 * 0.9).sin() * 0.5 + 0.5).powi(2)
                    } else {
                        0.8
                    };
                    c.circle(
                        x + px(2.5) + px(if minimal { 5.5 } else { 8.0 }) * i as f32,
                        cy,
                        px(2.1),
                        pal.fg,
                        alpha,
                    );
                }
            }
            x += lead_w + gap;
        }
        Mode::Notice { tone, .. } => {
            let color = dot.unwrap_or(pal.fg);
            let cx = x + px(8.0);
            draw_status_icon(&mut c, *tone, cx, cy, s, color);
            if rich && *tone == Tone::Startup && visuals.animate && visuals.elapsed < RICH_SETTLE {
                let p = (phase / RICH_SETTLE.as_secs_f32()).clamp(0.0, 1.0);
                c.stroke_circle(
                    cx,
                    cy,
                    px(7.0) + px(9.0) * ease_out(p),
                    px(1.2),
                    color,
                    0.55 * (1.0 - p),
                );
            }
            if rich && *tone == Tone::Success && visuals.animate && visuals.elapsed < RICH_SETTLE {
                let p = (phase / RICH_SETTLE.as_secs_f32()).clamp(0.0, 1.0);
                for i in 0..6 {
                    let angle = i as f32 * std::f32::consts::TAU / 6.0 + 0.3;
                    let radius = px(18.0) * (1.0 - ease_out(p)) + px(5.0);
                    c.circle(
                        cx + angle.cos() * radius,
                        cy + angle.sin() * radius,
                        px(1.4),
                        color,
                        0.65 * (1.0 - p),
                    );
                }
            }
            x += lead_w + gap;
        }
    }
    if let Some(m) = &label {
        c.text(m, x as i32, (cy - m.h as f32 / 2.0) as i32, pal.fg, 0.95);
        x += m.w as f32 + px(6.0);
    }
    if let Some(m) = &hint {
        c.text(m, x as i32, (cy - m.h as f32 / 2.0) as i32, pal.fg, 0.72);
    }

    // 字幕帯。確定は通常の色、仮は薄い色。話している途中の文字が入力欄に入ることはない
    if let (Some(band_w), Some(band_h), true) = (Some(band_w), Some(band_h), band_h > 0.0) {
        let bx = shadow + (w as f32 - shadow * 2.0 - band_w) / 2.0;
        let by = y0 + pill_h + gap;
        c.round_rect(
            bx,
            by + px(2.0),
            band_w,
            band_h,
            px(10.0),
            Rgb(0, 0, 0),
            pal.shadow_alpha * 0.8,
            shadow,
        );
        c.round_rect(bx, by, band_w, band_h, px(10.0), pal.bg, pal.bg_alpha, 0.0);
        c.stroke_round_rect(bx, by, band_w, band_h, px(10.0), pal.edge, pal.edge_alpha);
        let text_x = bx + band_pad;
        let mut text_y = by + band_pad;
        if let Some(m) = confirmed_mask {
            c.text(m, text_x as i32, text_y as i32, pal.fg, 0.92);
            text_y += m.h as f32 + band_gap;
        }
        if let Some(m) = partial_mask {
            c.text(m, text_x as i32, text_y as i32, pal.fg, 0.72);
        }
    }

    RenderedFrame {
        canvas: c,
        shadow,
        pill_h,
        gap,
        band_w,
        band_h,
    }
}

fn clamp_to_work_area(x: i32, y: i32, w: i32, h: i32, work: RECT) -> (i32, i32) {
    (
        x.clamp(work.left, (work.right - w).max(work.left)),
        y.clamp(work.top, (work.bottom - h).max(work.top)),
    )
}

struct BlitSurface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl BlitSurface {
    unsafe fn new(w: i32, h: i32) -> Option<Self> {
        let dc = CreateCompatibleDC(None);
        if dc.0.is_null() {
            return None;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        match CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => {
                let old = SelectObject(dc, HGDIOBJ(bitmap.0));
                Some(Self {
                    dc,
                    bitmap,
                    old,
                    bits: bits.cast(),
                    w,
                    h,
                })
            }
            Err(_) => {
                let _ = DeleteDC(dc);
                None
            }
        }
    }
}

impl Drop for BlitSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap);
            let _ = DeleteDC(self.dc);
        }
    }
}

thread_local! {
    // GDI resources remain on the presenting thread and are released on resize or exit.
    static BLIT_SURFACE: RefCell<Option<BlitSurface>> = const { RefCell::new(None) };
}

unsafe fn blit(window: HWND, c: &Canvas, x: i32, y: i32, alpha: u8) {
    BLIT_SURFACE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|surface| surface.w != c.w || surface.h != c.h)
        {
            *slot = BlitSurface::new(c.w, c.h);
        }
        let Some(surface) = slot.as_ref() else { return };
        let out = std::slice::from_raw_parts_mut(surface.bits, (c.w * c.h * 4) as usize);
        for (i, p) in c.px.iter().enumerate() {
            for k in 0..4 {
                out[i * 4 + k] = (p[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let dst = POINT { x, y };
        let size = SIZE { cx: c.w, cy: c.h };
        let src = POINT { x: 0, y: 0 };
        let _ = UpdateLayeredWindow(
            window,
            None,
            Some(&dst),
            Some(&size),
            surface.dc,
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
        let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
            window,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
        );
        let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
    });
}

unsafe fn show_exit_menu(window: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let settings = wide("設定");
    let _ = AppendMenuW(menu, MF_STRING, CMD_SETTINGS, PCWSTR(settings.as_ptr()));
    let count = RECOVERY_COUNT.load(Ordering::SeqCst);
    if count > 0 {
        let recover = wide(&format!("未入力テキストをコピー（残り{count}件）"));
        let _ = AppendMenuW(menu, MF_STRING, CMD_RECOVER_TEXT, PCWSTR(recover.as_ptr()));
    }
    let label = wide("終了");
    let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, PCWSTR(label.as_ptr()));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // メニューの外をクリックしたときに閉じるため、一時的に前面にする
    let _ = SetForegroundWindow(window);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD, pt.x, pt.y, 0, window, None);
    let _ = DestroyMenu(menu);
    if cmd.0 as usize == CMD_SETTINGS {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--settings").spawn();
        }
    }
    if cmd.0 as usize == CMD_RECOVER_TEXT && count > 0 {
        RECOVERY_REQUESTED.store(true, Ordering::SeqCst);
    }
    if cmd.0 as usize == CMD_EXIT {
        EXIT_REQUESTED.store(true, Ordering::SeqCst);
        let _ = PostMessageW(
            window,
            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

unsafe extern "system" fn wnd_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(result) = crate::preview_ipc::handle_message(window, msg, wparam, lparam) {
        return result;
    }
    if TASKBAR_CREATED.get().copied() == Some(msg) {
        TRAY_READD.store(true, Ordering::Release);
        return LRESULT(0);
    }
    match msg {
        windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_SYSCOLORCHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_DPICHANGED => {
            // Defer refresh until outside Win32 callbacks and rendering locks.
            ENVIRONMENT_CHANGED.store(true, Ordering::Release);
            DIRTY.store(true, Ordering::Release);
            LRESULT(0)
        }
        x if x == windows::Win32::UI::WindowsAndMessaging::WM_CLOSE => {
            // The resident owns shutdown. Keep the clipboard owner and tray alive
            // until pending restoration/saves and recoverable text are handled.
            EXIT_REQUESTED.store(true, Ordering::SeqCst);
            LRESULT(0)
        }
        x if x == WM_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_exit_menu(window);
            }
            LRESULT(0)
        }
        x if x == WM_COMMAND => {
            if wparam.0 == CMD_EXIT {
                EXIT_REQUESTED.store(true, Ordering::SeqCst);
            } else if wparam.0 == CMD_RECOVER_TEXT && RECOVERY_COUNT.load(Ordering::SeqCst) > 0 {
                RECOVERY_REQUESTED.store(true, Ordering::SeqCst);
            }
            LRESULT(0)
        }
        // Do not post WM_QUIT here: the resident decides when shutdown is safe.
        x if x == WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_request_preserves_the_hidden_clipboard_owner_until_shutdown() {
        let _exit_guard = EXIT_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use windows::Win32::UI::WindowsAndMessaging::{
            IsWindow, IsWindowVisible, SendMessageW, UnregisterClassW, WM_CLOSE,
        };
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class = windows::core::w!("OpenHinoVoiceHiddenCloseRegression");
            let atom = RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            });
            assert_ne!(atom, 0);
            let window = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class,
                class,
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                HINSTANCE(instance.0),
                None,
            )
            .unwrap();
            let previous = EXIT_REQUESTED.swap(false, Ordering::SeqCst);
            SendMessageW(window, WM_CLOSE, WPARAM(0), LPARAM(0));
            let kept_alive = IsWindow(window).as_bool();
            let kept_hidden = !IsWindowVisible(window).as_bool();
            let requested = EXIT_REQUESTED.swap(previous, Ordering::SeqCst);
            let _ = DestroyWindow(window);
            let _ = UnregisterClassW(class, HINSTANCE(instance.0));
            assert!(
                kept_alive,
                "WM_CLOSE must not destroy the clipboard/tray owner"
            );
            assert!(
                kept_hidden,
                "the regression fixture must never show a window"
            );
            assert!(requested);
        }
    }

    #[test]
    fn preparation_updates_preserve_recording_details_and_do_not_replace_notices() {
        let started = Instant::now();
        for mut mode in [
            Mode::Recording {
                started,
                preparing: true,
            },
            Mode::Editing {
                started,
                head: "選んだ文".into(),
                preparing: true,
            },
            Mode::Recognizing { preparing: true },
        ] {
            let original = mode.clone();
            assert!(!mode.set_engine_preparing(true));
            assert!(mode.set_engine_preparing(false));
            assert!(!mode.set_engine_preparing(false));
            assert!(mode.set_engine_preparing(true));
            assert_eq!(mode, original);
        }
        for mut mode in [
            Mode::Notice {
                tone: Tone::Warn,
                text: "マイクを切り替えました".into(),
            },
            Mode::Busy {
                label: "整形中".into(),
                hint: Some("Enter で生テキスト".into()),
            },
        ] {
            let original = mode.clone();
            assert!(!mode.set_engine_preparing(true));
            assert!(!mode.set_engine_preparing(false));
            assert_eq!(mode, original);
        }
    }

    #[test]
    fn preparation_copy_distinguishes_live_recording_from_recorded_audio() {
        let elapsed = Duration::from_secs(65);
        assert_eq!(recording_label(elapsed, false, false), "1:05");
        assert_eq!(recording_label(elapsed, true, false), "編集 1:05");
        for editing in [false, true] {
            let label = recording_label(elapsed, editing, true);
            assert!(label.contains("1:05"));
            assert!(label.contains("認識エンジン準備中・録音継続"));
        }
        assert_eq!(
            recognizing_text(true),
            ("認識エンジンを準備中", Some("音声は録音済みです"))
        );
        assert_eq!(recognizing_text(false), ("認識中", None));
    }

    #[test]
    fn static_notice_stops_rendering_only_after_the_final_fade_frame() {
        let redraw_needed = |mode, fade, hiding, dirty| {
            super::redraw_needed(mode, fade, hiding, dirty, true, false)
        };
        let notice = Mode::Notice {
            tone: Tone::Warn,
            text: "確認してください".into(),
        };
        assert!(redraw_needed(Some(&notice), false, false, false));
        assert!(!redraw_needed(Some(&notice), true, false, false));
        assert!(redraw_needed(Some(&notice), true, true, false));
        assert!(redraw_needed(Some(&notice), true, false, true));
        let busy = Mode::Busy {
            label: "認識中".into(),
            hint: None,
        };
        assert!(redraw_needed(Some(&busy), true, false, false));
        assert!(redraw_needed(
            Some(&Mode::Recording {
                started: Instant::now(),
                preparing: false,
            }),
            true,
            false,
            false
        ));
    }

    #[test]
    fn caret_overlay_stays_inside_offset_work_area() {
        let work = RECT {
            left: -1920,
            top: 100,
            right: 0,
            bottom: 1180,
        };
        assert_eq!(clamp_to_work_area(-2000, 90, 800, 200, work), (-1920, 100));
        assert_eq!(clamp_to_work_area(-100, 1150, 800, 200, work), (-800, 980));
        assert_eq!(clamp_to_work_area(-1300, 300, 800, 200, work), (-1300, 300));
        assert_eq!(
            clamp_to_work_area(-1300, 300, 3000, 2000, work),
            (-1920, 100)
        );
    }

    #[test]
    fn repeated_labels_share_a_bounded_text_mask_cache() {
        let first = unsafe { cached_text("キャッシュの確認", 13, false, 440) };
        let second = unsafe { cached_text("キャッシュの確認", 13, false, 440) };
        assert!(Arc::ptr_eq(&first, &second));
        for n in 0..12 {
            unsafe {
                cached_text(&format!("時刻 {n}"), 13, false, 440);
            }
        }
        assert!(TEXT_MASKS.lock().unwrap_or_else(|p| p.into_inner()).len() <= 8);
    }

    #[test]
    fn taskbar_created_message_registration_is_stable() {
        let first = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
        let second = unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
        assert!((0xC000..=0xFFFF).contains(&first));
        assert_eq!(first, second);
    }

    #[test]
    fn level_maps_voice_range_into_meter() {
        assert_eq!(level_from_rms(0.0), 0.0);
        assert_eq!(level_from_rms(1.0), 1.0);
        let quiet_room = level_from_rms(0.002); // 約 -54dB
        let speech = level_from_rms(0.05); // 約 -26dB
        assert!(quiet_room < 0.05, "{quiet_room}");
        assert!(speech > 0.5 && speech < 0.7, "{speech}");
    }

    #[test]
    fn round_rect_edges_are_antialiased() {
        // 中心は塗り、遠くは塗らず、縁はその中間
        assert_eq!(
            coverage(round_rect_sdf(50.0, 18.0, 0.0, 0.0, 100.0, 36.0, 18.0), 0.0),
            1.0
        );
        assert_eq!(
            coverage(
                round_rect_sdf(150.0, 18.0, 0.0, 0.0, 100.0, 36.0, 18.0),
                0.0
            ),
            0.0
        );
        let edge = coverage(
            round_rect_sdf(100.0, 18.0, 0.0, 0.0, 100.0, 36.0, 18.0),
            0.0,
        );
        assert!(edge > 0.4 && edge < 0.6, "{edge}");
        // 角は丸く抜けている
        assert_eq!(
            coverage(round_rect_sdf(0.5, 0.5, 0.0, 0.0, 100.0, 36.0, 18.0), 0.0),
            0.0
        );
    }

    #[test]
    fn caption_keeps_the_latest_text_until_cleared() {
        set_caption("確定した文", "話している途中");
        assert_eq!(
            caption_for_test(),
            Some(("確定した文".into(), "話している途中".into()))
        );
        // 同じ文字を渡しても壊れない
        set_caption("確定した文", "話している途中");
        assert_eq!(
            caption_for_test(),
            Some(("確定した文".into(), "話している途中".into()))
        );
        clear_caption();
        assert_eq!(caption_for_test(), None);
    }

    #[test]
    fn clearing_the_caption_releases_its_cached_text_and_image() {
        *CAPTION_MASKS.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(unsafe { caption_masks("確定した文", "", 13, 520) });
        clear_caption();
        assert!(CAPTION_MASKS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_none());
    }

    #[test]
    fn caption_draws_a_partial_before_the_first_confirmed_interval() {
        let masks = unsafe { caption_masks("", "話している途中です", 13, 520) };
        assert!(masks.confirmed.cov.iter().all(|&pixel| pixel == 0));
        assert!(masks.partial.unwrap().cov.iter().any(|&pixel| pixel > 0));
    }

    #[test]
    fn premultiplied_blend_over_transparent() {
        let mut c = Canvas::new(1, 1);
        c.blend(0, 0, Rgb(255, 0, 0), 0.5);
        let p = c.px[0];
        assert!((p[2] - 0.5).abs() < 1e-6 && p[0] == 0.0 && (p[3] - 0.5).abs() < 1e-6);
    }
    #[test]
    fn motion_preferences_and_finite_success_do_not_schedule_idle_frames() {
        assert!(!motion_allowed(FeedbackMotion::Reduced, true));
        assert!(!motion_allowed(FeedbackMotion::System, false));
        assert!(motion_allowed(FeedbackMotion::System, true));
        let success = Mode::Notice {
            tone: Tone::Success,
            text: "入力しました".into(),
        };
        assert!(rich_settling(
            Some(&success),
            HudStyle::Rich,
            Duration::from_millis(599)
        ));
        assert!(!rich_settling(Some(&success), HudStyle::Rich, RICH_SETTLE));
        assert!(!rich_settling(
            Some(&success),
            HudStyle::Standard,
            Duration::ZERO
        ));
        assert!(!super::redraw_needed(
            Some(&success),
            true,
            false,
            false,
            false,
            true
        ));
        assert!(!super::redraw_needed(
            Some(&Mode::Recognizing { preparing: false }),
            true,
            false,
            false,
            false,
            false
        ));
        let startup = Mode::Notice {
            tone: Tone::Startup,
            text: "起動しました".into(),
        };
        assert!(rich_settling(
            Some(&startup),
            HudStyle::Rich,
            Duration::from_millis(599)
        ));
        assert!(!rich_settling(Some(&startup), HudStyle::Rich, RICH_SETTLE));
        assert!(!rich_settling(
            Some(&startup),
            HudStyle::Standard,
            Duration::ZERO
        ));
        assert!(!redraw_needed(
            Some(&startup),
            true,
            false,
            false,
            false,
            true
        ));
        assert!(!redraw_needed(
            Some(&startup),
            true,
            false,
            false,
            true,
            false
        ));
        assert!(FRAME >= Duration::from_secs_f64(1.0 / 60.0));
    }

    #[test]
    fn shared_preview_is_deterministic_and_minimal_is_smaller() {
        let mode = Mode::Recording {
            started: Instant::now(),
            preparing: false,
        };
        let render = |style| {
            preview_frame(
                &mode,
                "hinomaru",
                style,
                FeedbackMotion::Reduced,
                Duration::from_secs(3),
                0.5,
                1.0,
            )
        };
        let standard = render(HudStyle::Standard);
        let repeated = render(HudStyle::Standard);
        let minimal = render(HudStyle::Minimal);
        assert_eq!(standard.rgba, repeated.rgba);
        assert_eq!(standard.rgba.len(), standard.width * standard.height * 4);
        assert!(minimal.width < standard.width);
        assert!(minimal.height < standard.height);
        assert!(standard.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] > 0));
        assert!(standard.rgba.as_chunks::<4>().0.iter().any(|pixel| pixel[3] == 0));
    }

    #[test]
    fn notice_shapes_are_distinct_without_relying_on_color() {
        let mut images = Vec::new();
        for tone in [
            Tone::Success,
            Tone::Warn,
            Tone::Error,
            Tone::Info,
            Tone::Startup,
        ] {
            let mut canvas = Canvas::new(24, 24);
            draw_status_icon(&mut canvas, tone, 12.0, 12.0, 1.0, Rgb(255, 255, 255));
            let silhouette: Vec<u8> = canvas.px.iter().map(|p| (p[3] * 255.0) as u8).collect();
            assert!(images.iter().all(|existing| existing != &silhouette));
            images.push(silhouette);
        }
    }

    #[test]
    fn rich_particles_settle_and_reduced_motion_is_static() {
        let mode = Mode::Notice {
            tone: Tone::Success,
            text: "入力しました".into(),
        };
        let first = preview_frame(
            &mode,
            "hinomaru",
            HudStyle::Rich,
            FeedbackMotion::Reduced,
            Duration::from_millis(200),
            0.2,
            1.0,
        );
        let later = preview_frame(
            &mode,
            "hinomaru",
            HudStyle::Rich,
            FeedbackMotion::Reduced,
            Duration::from_secs(2),
            0.8,
            1.0,
        );
        assert_eq!(first.rgba, later.rgba);
        assert_eq!(first.width, later.width);
        let visuals = |elapsed| Visuals {
            style: HudStyle::Rich,
            animate: true,
            elapsed,
            recording_elapsed: Duration::ZERO,
        };
        let work = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let a =
            unsafe { render_frame(&mode, &HINOMARU, work, 1.0, 0.0, None, visuals(RICH_SETTLE)) };
        let b = unsafe {
            render_frame(
                &mode,
                &HINOMARU,
                work,
                1.0,
                0.0,
                None,
                visuals(Duration::from_secs(2)),
            )
        };
        assert_eq!(a.canvas.px, b.canvas.px);
    }

    #[test]
    fn startup_ripple_is_once_and_static_when_motion_is_reduced_or_high_contrast() {
        let mode = Mode::Notice {
            tone: Tone::Startup,
            text: "起動しました".into(),
        };
        let work = RECT {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
        };
        let render = |elapsed, animate, palette: &Palette| unsafe {
            render_frame(
                &mode,
                palette,
                work,
                1.0,
                0.0,
                None,
                Visuals {
                    style: HudStyle::Rich,
                    animate,
                    elapsed,
                    recording_elapsed: Duration::ZERO,
                },
            )
            .canvas
            .px
        };
        let early = Duration::from_millis(200);
        let later = Duration::from_millis(400);
        assert_ne!(
            render(early, true, &HINOMARU),
            render(later, true, &HINOMARU)
        );
        assert_eq!(
            render(RICH_SETTLE, true, &HINOMARU),
            render(Duration::from_secs(2), true, &HINOMARU)
        );
        assert_eq!(
            render(early, false, &HINOMARU),
            render(later, false, &HINOMARU)
        );
        let contrast = Palette {
            high_contrast: true,
            ..HINOMARU
        };
        assert_eq!(
            render(early, true, &contrast),
            render(later, true, &contrast)
        );
    }

    #[test]
    fn embedded_tray_icons_cover_all_states_and_dpi_sizes() {
        for light in [true, false] {
            let mut silhouettes = Vec::new();
            for kind in [
                TrayKind::Idle,
                TrayKind::Recording,
                TrayKind::Processing,
                TrayKind::Error,
            ] {
                let bytes = kind.bytes(light);
                for size in [16, 20, 24, 28, 32, 40, 48, 64] {
                    assert!(ico_frame(bytes, size).is_some());
                }
                assert!(silhouettes.iter().all(|previous| previous != &bytes));
                silhouettes.push(bytes);
            }
        }
        assert!(ico_frame(&[], 16).is_none());
        assert!(ico_frame(&[0, 0, 1, 0, 255, 255], 16).is_none());
    }

    #[test]
    fn tray_semantics_follow_processing_and_error_without_hud_visibility() {
        assert_eq!(TrayKind::from_mode(None), TrayKind::Idle);
        assert_eq!(
            TrayKind::from_mode(Some(&Mode::Recording {
                started: Instant::now(),
                preparing: true
            })),
            TrayKind::Recording
        );
        assert_eq!(
            TrayKind::from_mode(Some(&Mode::Recognizing { preparing: true })),
            TrayKind::Processing
        );
        assert_eq!(
            TrayKind::from_mode(Some(&Mode::Notice {
                tone: Tone::Error,
                text: "入力できません".into()
            })),
            TrayKind::Error
        );
        assert_eq!(
            TrayKind::from_mode(Some(&Mode::Notice {
                tone: Tone::Info,
                text: "取り消しました".into()
            })),
            TrayKind::Idle
        );
    }

    #[test]
    fn preview_environment_changes_with_os_palette_and_motion() {
        let light = environment_for(HINOMARU, true);
        let dark = environment_for(DARK, true);
        assert_ne!(light, dark);
        let contrast = Palette {
            high_contrast: true,
            ..DARK
        };
        assert_ne!(dark, environment_for(contrast, false));
        assert_ne!(light, environment_for(HINOMARU, false));
        let changed_foreground = Palette {
            fg: Rgb(255, 255, 0),
            ..contrast
        };
        assert_ne!(
            environment_for(contrast, false),
            environment_for(changed_foreground, false)
        );
    }

    #[test]
    fn tray_readd_intent_survives_failure_and_stops_after_success() {
        let now = Instant::now();
        let mut retry = TrayRetry::default();
        retry.invalidate(true, true);
        assert_eq!(retry.ready(now), Some((true, true)));
        retry.failed(now, true);
        assert_eq!(retry.ready(now + Duration::from_millis(249)), None);
        assert_eq!(
            retry.ready(now + Duration::from_millis(250)),
            Some((true, true))
        );
        retry.succeeded();
        assert_eq!(retry.ready(now), Some((false, false)));
        assert!(retry.retry_at.is_none());
    }

    #[test]
    fn tray_failures_have_finite_backoff_and_restart_on_a_new_state() {
        let mut now = Instant::now();
        let mut retry = TrayRetry::default();
        for attempt in 1..=TRAY_RETRY_LIMIT {
            assert!(retry.ready(now).is_some());
            retry.failed(now, true);
            if attempt < TRAY_RETRY_LIMIT {
                let next = retry.retry_at.unwrap();
                assert!(next > now);
                assert_eq!(retry.ready(now), None);
                now = next;
            }
        }
        assert!(retry.add_needed);
        assert!(retry.retry_at.is_none());
        assert_eq!(retry.ready(now + Duration::from_secs(60)), None);
        retry.invalidate(false, false);
        assert_eq!(retry.ready(now), Some((true, false)));
    }
}
