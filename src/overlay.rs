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
    ShowWindow, TrackPopupMenu, UpdateLayeredWindow, GUITHREADINFO, HWND_TOPMOST, IDI_APPLICATION,
    MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MF_STRING, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SW_HIDE, SW_SHOWNOACTIVATE, TPM_RETURNCMD, ULW_ALPHA, WM_APP, WM_COMMAND, WM_CONTEXTMENU,
    WM_DESTROY, WM_RBUTTONUP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
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
const FRAME: Duration = Duration::from_millis(16);
const BARS: usize = 5;

struct Overlay {
    hwnd: isize,
    mode: Option<Mode>,
    shown_at: Instant,
    hiding_since: Option<Instant>,
    visible: bool,
    level: f32,
    work: RECT,
    scale: f32,
    theme: String,
    palette: &'static Palette,
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
            hiding_since: None,
            visible: false,
            level: 0.0,
            work: RECT::default(),
            scale: 1.0,
            theme: "hinomaru".into(),
            palette: &HINOMARU,
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
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
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

unsafe fn add_tray(window: HWND) -> Result<(), String> {
    let icon = LoadIconW(None, IDI_APPLICATION)
        .map_err(|e| format!("トレイ用アイコンを読み込めません: {e}"))?;
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
    if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        return Err(format!(
            "通知領域にトレイアイコンを登録できません: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

pub fn remove_tray() {
    unsafe {
        let data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd(),
            uID: 1,
            ..Default::default()
        };
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// 表示内容を切り替える。非表示からの表示では、いま使っているアプリのある画面の上端に出す
pub fn show(mode: Mode) {
    let mut o = lock();
    if !o.visible || o.hiding_since.is_some() {
        let (work, scale) = unsafe { anchor_monitor() };
        o.work = work;
        o.scale = scale;
        o.palette = palette_for(&o.theme);
        o.shown_at = Instant::now();
        o.level = 0.0;
    }
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

/// 設定の theme を渡す。次に表示を出すときから効く
pub fn set_theme(theme: &str) {
    lock().theme = theme.to_string();
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
    let mut o = lock();
    if o.visible && o.hiding_since.is_none() {
        o.hiding_since = Some(Instant::now());
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
        )
}

fn redraw_needed(mode: Option<&Mode>, fade_settled: bool, hiding: bool, dirty: bool) -> bool {
    dirty
        || !fade_settled
        || hiding
        || matches!(
            mode,
            Some(
                Mode::Recording { .. }
                    | Mode::Editing { .. }
                    | Mode::Recognizing { .. }
                    | Mode::Busy { .. }
            )
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
}

/// メインループから毎回呼ぶ。アニメーション中だけ描き直す
pub fn tick() {
    let mut o = lock();
    if !o.visible || o.last_frame.elapsed() < FRAME {
        return;
    }
    if !redraw_needed(
        o.mode.as_ref(),
        o.fade_settled,
        o.hiding_since.is_some(),
        DIRTY.swap(false, Ordering::AcqRel),
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
        None => ease_out((o.shown_at.elapsed().as_secs_f32() / FADE_IN.as_secs_f32()).min(1.0)),
    };
    o.fade_settled = o.shown_at.elapsed() >= FADE_IN;
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
    unsafe { present(window, mode, palette, work, scale, level, fade, masks) };
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

#[derive(Clone, Copy)]
struct Rgb(u8, u8, u8);

struct Palette {
    bg: Rgb,
    bg_alpha: f32,
    edge: Rgb,
    edge_alpha: f32,
    fg: Rgb,
    red: Rgb,
    amber: Rgb,
    shadow_alpha: f32,
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
};

/// "auto" は Windows のアプリ用のライト、ダークに合わせる。それ以外はすべて日の丸
fn palette_for(theme: &str) -> &'static Palette {
    if theme == "auto" && !windows_uses_light_theme() {
        &DARK
    } else {
        &HINOMARU
    }
}

fn windows_uses_light_theme() -> bool {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let mut value: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            windows::core::w!("AppsUseLightTheme"),
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

    fn text(&mut self, mask: &TextMask, x: i32, y: i32, c: Rgb, alpha: f32) {
        for ty in 0..mask.h {
            for tx in 0..mask.w {
                let cov = mask.cov[(ty * mask.w + tx) as usize] as f32 / 255.0;
                self.blend(x + tx, y + ty, c, alpha * cov);
            }
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

#[allow(clippy::too_many_arguments)] // 描画の 1 枚分の入力を並べて渡す
unsafe fn present(
    window: HWND,
    mode: &Mode,
    pal: &Palette,
    work: RECT,
    s: f32,
    level: f32,
    fade: f32,
    masks: Option<&CaptionMasks>,
) {
    let px = |v: f32| (v * s).round();
    let shadow = px(12.0);
    let pill_h = px(36.0);
    let pad = px(14.0);
    let gap = px(9.0);
    let font_px = px(13.0) as i32;
    let max_text = (px(440.0) as i32).min(((work.right - work.left - px(150.0) as i32) / 2).max(1));

    // 中身の幅を先に決める
    let (label, hint, dot): (Option<Arc<TextMask>>, Option<Arc<TextMask>>, Option<Rgb>) = match mode
    {
        Mode::Recording { started, preparing } => (
            Some(cached_text(
                &recording_label(started.elapsed(), false, *preparing),
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
            started,
            head,
            preparing,
        } => (
            Some(cached_text(
                &recording_label(started.elapsed(), true, *preparing),
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
            Some(if *tone == Tone::Warn {
                pal.amber
            } else {
                pal.red
            }),
        ),
    };
    let lead_w = match mode {
        Mode::Recording { .. } | Mode::Editing { .. } => {
            px(10.0) + gap + px(3.0) * BARS as f32 + px(3.0) * (BARS as f32 - 1.0)
        }
        Mode::Recognizing { .. } | Mode::Busy { .. } => px(22.0),
        Mode::Notice { .. } => px(8.0),
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
    let phase = start_phase();
    match mode {
        Mode::Recording { .. } | Mode::Editing { .. } => {
            // 脈打つ赤い点。外側の輪が広がって消える
            let p = (phase % 1.4) / 1.4;
            c.circle(
                x + px(5.0),
                cy,
                px(5.0) + px(5.0) * p,
                pal.red,
                0.35 * (1.0 - p),
            );
            c.circle(x + px(5.0), cy, px(5.0), pal.red, 1.0);
            x += px(10.0) + gap;
            // 音量の棒。実際のマイクの音量で伸び、各棒を少しずつずらして揺らす
            let shape = [0.55, 0.8, 1.0, 0.8, 0.55];
            for (i, k) in shape.iter().enumerate() {
                let wobble = 0.75 + 0.25 * ((phase * 9.0 + i as f32 * 1.7).sin() * 0.5 + 0.5);
                let bh = px(4.0) + (px(18.0) - px(4.0)) * (level * k * wobble).min(1.0);
                c.round_rect(x, cy - bh / 2.0, px(3.0), bh, px(1.5), pal.fg, 0.9, 0.0);
                x += px(6.0);
            }
            x += gap - px(3.0);
        }
        Mode::Recognizing { .. } | Mode::Busy { .. } => {
            // 3 つの点が順に明るくなる
            for i in 0..3 {
                let a = 0.3 + 0.7 * (((phase * 5.0 - i as f32 * 0.9).sin() * 0.5 + 0.5).powi(2));
                c.circle(x + px(3.0) + px(8.0) * i as f32, cy, px(2.6), pal.fg, a);
            }
            x += px(22.0) + gap;
        }
        Mode::Notice { .. } => {
            if let Some(color) = dot {
                c.circle(x + px(4.0), cy, px(4.0), color, 1.0);
            }
            x += px(8.0) + gap;
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
    blit(window, &c, wx, wy, (fade * 255.0) as u8);
    FRAME_CANVAS.with(|slot| *slot.borrow_mut() = Some(c));
}

fn clamp_to_work_area(x: i32, y: i32, w: i32, h: i32, work: RECT) -> (i32, i32) {
    (
        x.clamp(work.left, (work.right - w).max(work.left)),
        y.clamp(work.top, (work.bottom - h).max(work.top)),
    )
}

fn start_phase() -> f32 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_secs_f32()
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
    if TASKBAR_CREATED.get().copied() == Some(msg) {
        // Explorer recreates its notification area on restart; icons must be re-added.
        if let Err(e) = add_tray(window) {
            show_error(&format!("Explorer 再起動後のトレイ復元に失敗しました: {e}"));
        }
        return LRESULT(0);
    }
    match msg {
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
}
