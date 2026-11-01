use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
    GetMonitorInfoW, MonitorFromWindow, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, DT_CALCRECT,
    DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetForegroundWindow, LoadIconW, MessageBoxW, PostMessageW, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, ShowWindow, TrackPopupMenu, UpdateLayeredWindow,
    HWND_TOPMOST, IDI_APPLICATION, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MF_STRING,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, TPM_RETURNCMD, ULW_ALPHA,
    WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_RBUTTONUP, WNDCLASSW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

pub const WM_TRAY: u32 = WM_APP + 1;
pub const CMD_EXIT: usize = 1001;
pub const CMD_SETTINGS: usize = 1002;
pub const CMD_RECOVER_TEXT: usize = 1003;

pub static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static RECOVERY_COUNT: AtomicUsize = AtomicUsize::new(0);
static RECOVERY_REQUESTED: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

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
    Recording { started: Instant },
    Busy { label: String, hint: Option<String> },
    Notice { tone: Tone, text: String },
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
}

/// 設定の theme を渡す。次に表示を出すときから効く
pub fn set_theme(theme: &str) {
    lock().theme = theme.to_string();
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

/// 録音中のマイクの音を渡す。表示の棒の高さに使う
pub fn push_level(samples: &[f32]) {
    if samples.is_empty() {
        return;
    }
    let target = level_from_rms(rms(samples));
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
    let Some(mode) = o.mode.clone() else { return };
    let (work, scale, level, palette) = (o.work, o.scale, o.level, o.palette);
    drop(o);
    unsafe { present(window, &mode, palette, work, scale, level, fade) };
}

fn ease_out(p: f32) -> f32 {
    1.0 - (1.0 - p).powi(3)
}

fn rms(samples: &[f32]) -> f32 {
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
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

/// GDI で白黒の文字を描き、その濃さを塗りの割合として取り出す。GDI は 32bit 画像のアルファを壊すため別に描く
unsafe fn render_text(text: &str, px_height: i32, bold: bool, max_w: i32) -> TextMask {
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

unsafe fn present(
    window: HWND,
    mode: &Mode,
    pal: &Palette,
    work: RECT,
    s: f32,
    level: f32,
    fade: f32,
) {
    let px = |v: f32| (v * s).round();
    let shadow = px(12.0);
    let pill_h = px(36.0);
    let pad = px(14.0);
    let gap = px(9.0);
    let font_px = px(13.0) as i32;
    let max_text = px(440.0) as i32;

    // 中身の幅を先に決める
    let (label, hint, dot): (Option<TextMask>, Option<TextMask>, Option<Rgb>) = match mode {
        Mode::Recording { started } => {
            let secs = started.elapsed().as_secs();
            (
                Some(render_text(
                    &format!("{}:{:02}", secs / 60, secs % 60),
                    font_px,
                    true,
                    max_text,
                )),
                None,
                None,
            )
        }
        Mode::Busy { label, hint } => (
            Some(render_text(label, font_px, true, max_text)),
            hint.as_deref()
                .map(|h| render_text(h, font_px, false, max_text)),
            None,
        ),
        Mode::Notice { tone, text } => (
            Some(render_text(text, font_px, true, max_text)),
            None,
            Some(if *tone == Tone::Warn {
                pal.amber
            } else {
                pal.red
            }),
        ),
    };
    let lead_w = match mode {
        Mode::Recording { .. } => {
            px(10.0) + gap + px(3.0) * BARS as f32 + px(3.0) * (BARS as f32 - 1.0)
        }
        Mode::Busy { .. } => px(22.0),
        Mode::Notice { .. } => px(8.0),
    };
    let label_w = label.as_ref().map_or(0, |m| m.w) as f32;
    let hint_w = hint.as_ref().map_or(0.0, |m| m.w as f32 + px(6.0));
    let pill_w = (pad * 2.0 + lead_w + gap + label_w + hint_w).max(px(96.0));

    let w = (pill_w + shadow * 2.0) as i32;
    let h = (pill_h + shadow * 2.0) as i32;
    let mut c = Canvas::new(w, h);
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
        Mode::Recording { .. } => {
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
        Mode::Busy { .. } => {
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
        c.text(m, x as i32, (cy - m.h as f32 / 2.0) as i32, pal.fg, 0.5);
    }

    // 画面上端の中央。下中央はチャットの入力欄など文字を入れる場所に重なりやすいので避ける。出るときは上から少し降りてくる
    let drop = px(8.0) * (1.0 - fade);
    let wx = work.left + ((work.right - work.left) - w) / 2;
    let wy = work.top + px(4.0) as i32 - drop as i32;
    blit(window, &c, wx, wy, (fade * 255.0) as u8);
}

fn start_phase() -> f32 {
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_secs_f32()
}

unsafe fn blit(window: HWND, c: &Canvas, x: i32, y: i32, alpha: u8) {
    let dc = CreateCompatibleDC(None);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: c.w,
            biHeight: -c.h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    if let Ok(bmp) = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
        let out = std::slice::from_raw_parts_mut(bits as *mut u8, (c.w * c.h * 4) as usize);
        for (i, p) in c.px.iter().enumerate() {
            for k in 0..4 {
                out[i * 4 + k] = (p[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
        let old = SelectObject(dc, HGDIOBJ(bmp.0));
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
            dc,
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
        SelectObject(dc, old);
        let _ = DeleteObject(bmp);
    }
    let _ = DeleteDC(dc);
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
        x if x == WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(window, msg, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn premultiplied_blend_over_transparent() {
        let mut c = Canvas::new(1, 1);
        c.blend(0, 0, Rgb(255, 0, 0), 0.5);
        let p = c.px[0];
        assert!((p[2] - 0.5).abs() < 1e-6 && p[0] == 0.0 && (p[3] - 0.5).abs() < 1e-6);
    }
}
