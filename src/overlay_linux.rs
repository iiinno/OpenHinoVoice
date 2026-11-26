//! Linux (Wayland Layer Shell + fontdue + KDE StatusNotifierItem) overlay & tray backend.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use fontdue::{Font, FontSettings};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use crate::overlay::{
    TextMask, EXIT_REQUESTED, LAST_TEXT_AVAILABLE, LAST_TEXT_REQUESTED, RECOVERY_COUNT,
    RECOVERY_REQUESTED,
};

// ---------------------------------------------------------------------------
// Font loading & text rasterization (fontdue)
// ---------------------------------------------------------------------------

struct FontPair {
    regular: Font,
    bold: Font,
}

fn find_font_file(candidates: &[&str], fc_query: &str) -> Option<PathBuf> {
    for path in candidates {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Some(p);
        }
    }
    let output = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", fc_query])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let p = PathBuf::from(s);
    p.is_file().then_some(p)
}

fn load_font_pair() -> Option<FontPair> {
    let reg_path = find_font_file(
        &[
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/OTF/ipaexg.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        ],
        ":lang=ja",
    )?;
    let reg_bytes = std::fs::read(&reg_path).ok()?;
    let regular = Font::from_bytes(
        reg_bytes,
        FontSettings {
            collection_index: 0,
            scale: 40.0,
            load_substitutions: true,
        },
    )
    .ok()?;

    let bold_path = find_font_file(
        &[
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc",
            "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Bold.ttc",
        ],
        ":lang=ja:weight=bold",
    );
    let bold = bold_path
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| {
            Font::from_bytes(
                bytes,
                FontSettings {
                    collection_index: 0,
                    scale: 40.0,
                    load_substitutions: true,
                },
            )
            .ok()
        })
        .unwrap_or_else(|| regular.clone());

    Some(FontPair { regular, bold })
}

fn fonts() -> Option<&'static FontPair> {
    static FONTS: OnceLock<Option<FontPair>> = OnceLock::new();
    FONTS.get_or_init(load_font_pair).as_ref()
}

pub fn render_text(text: &str, px_height: i32, bold: bool, max_w: i32) -> TextMask {
    if text.is_empty() || px_height <= 0 || max_w <= 0 {
        return TextMask {
            w: 0,
            h: 0,
            cov: Vec::new(),
        };
    }
    let Some(pair) = fonts() else {
        return TextMask {
            w: 1,
            h: px_height.max(1),
            cov: vec![0; px_height.max(1) as usize],
        };
    };
    let font = if bold { &pair.bold } else { &pair.regular };
    let px = px_height as f32;
    let line_metrics = font.horizontal_line_metrics(px);
    let ascent = line_metrics.map_or((px * 0.82).ceil(), |m| m.ascent.ceil());
    let descent = line_metrics.map_or((px * 0.22).ceil(), |m| (-m.descent).ceil());
    let h = ((ascent + descent).ceil() as i32).max(px_height).max(1);

    let ellipsis_m = font.metrics('…', px);
    let ellipsis_w = ellipsis_m.advance_width;
    let mut glyphs = Vec::new();
    let mut cursor_x = 0.0f32;
    let max_wf = max_w as f32;
    let chars: Vec<char> = text.chars().collect();
    for (idx, &ch) in chars.iter().enumerate() {
        let m = font.metrics(ch, px);
        let next_x = cursor_x + m.advance_width;
        if next_x > max_wf && idx < chars.len() {
            // Trim back so ellipsis fits inside max_w
            while let Some((gx, _, _)) = glyphs.last() {
                if *gx + ellipsis_w <= max_wf {
                    break;
                }
                glyphs.pop();
            }
            let ex = glyphs.last().map_or(0.0, |(gx, adv, _)| *gx + *adv);
            let (em, eb) = font.rasterize('…', px);
            glyphs.push((ex, em.advance_width, (em, eb)));
            cursor_x = (ex + em.advance_width).min(max_wf);
            break;
        }
        let (gm, gb) = font.rasterize(ch, px);
        glyphs.push((cursor_x, gm.advance_width, (gm, gb)));
        cursor_x = next_x;
    }

    let w = (cursor_x.ceil() as i32).clamp(1, max_w);
    let mut cov = vec![0u8; (w * h) as usize];
    for (gx, _, (m, bitmap)) in glyphs {
        let x0 = gx.round() as i32 + m.xmin;
        let y0 = ascent.round() as i32 - m.ymin - m.height as i32;
        for row in 0..m.height {
            let dy = y0 + row as i32;
            if dy < 0 || dy >= h {
                continue;
            }
            for col in 0..m.width {
                let dx = x0 + col as i32;
                if dx < 0 || dx >= w {
                    continue;
                }
                let src = bitmap[row * m.width + col];
                let dst = &mut cov[(dy * w + dx) as usize];
                if src > *dst {
                    *dst = src;
                }
            }
        }
    }
    TextMask { w, h, cov }
}

pub fn render_text_wrapped(text: &str, px_height: i32, max_w: i32, max_lines: usize) -> TextMask {
    if text.is_empty() || px_height <= 0 || max_w <= 0 || max_lines == 0 {
        return TextMask {
            w: 0,
            h: 0,
            cov: Vec::new(),
        };
    }
    let Some(pair) = fonts() else {
        return TextMask {
            w: 1,
            h: px_height.max(1),
            cov: vec![0; px_height.max(1) as usize],
        };
    };
    let font = &pair.regular;
    let px = px_height as f32;
    let line_metrics = font.horizontal_line_metrics(px);
    let ascent = line_metrics.map_or((px * 0.82).ceil(), |m| m.ascent.ceil());
    let line_h = line_metrics
        .map_or((px * 1.25).ceil() as i32, |m| {
            m.new_line_size.ceil().max(px) as i32
        })
        .max(px_height);

    let max_wf = max_w as f32;
    let mut lines: Vec<(String, f32)> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0.0f32;
    for ch in text.chars() {
        if ch == '\n' {
            lines.push((std::mem::take(&mut cur), cur_w));
            cur_w = 0.0;
            continue;
        }
        let adv = font.metrics(ch, px).advance_width;
        if cur_w + adv > max_wf && !cur.is_empty() {
            lines.push((std::mem::take(&mut cur), cur_w));
            cur_w = 0.0;
        }
        cur.push(ch);
        cur_w += adv;
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push((cur, cur_w));
    }
    // Keep only the bottom `max_lines` lines (newest transcript).
    let start = lines.len().saturating_sub(max_lines);
    let visible = &lines[start..];
    let w = visible
        .iter()
        .map(|(_, lw)| lw.ceil() as i32)
        .max()
        .unwrap_or(1)
        .clamp(1, max_w);
    let h = (line_h * visible.len() as i32).max(1);
    let mut cov = vec![0u8; (w * h) as usize];

    for (line_idx, (line, _)) in visible.iter().enumerate() {
        let base_y = line_idx as i32 * line_h + ascent.round() as i32;
        let mut cursor_x = 0.0f32;
        for ch in line.chars() {
            let (m, bitmap) = font.rasterize(ch, px);
            let x0 = cursor_x.round() as i32 + m.xmin;
            let y0 = base_y - m.ymin - m.height as i32;
            for row in 0..m.height {
                let dy = y0 + row as i32;
                if dy < 0 || dy >= h {
                    continue;
                }
                for col in 0..m.width {
                    let dx = x0 + col as i32;
                    if dx < 0 || dx >= w {
                        continue;
                    }
                    let src = bitmap[row * m.width + col];
                    let dst = &mut cov[(dy * w + dx) as usize];
                    if src > *dst {
                        *dst = src;
                    }
                }
            }
            cursor_x += m.advance_width;
        }
    }
    TextMask { w, h, cov }
}

// ---------------------------------------------------------------------------
// System theme & notifications
// ---------------------------------------------------------------------------

pub fn system_uses_light_theme() -> bool {
    static CACHED: OnceLock<Mutex<(Instant, bool)>> = OnceLock::new();
    let cache =
        CACHED.get_or_init(|| Mutex::new((Instant::now() - Duration::from_secs(60), false)));
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    if guard.0.elapsed() < Duration::from_secs(5) {
        return guard.1;
    }
    let light = query_portal_light_theme().unwrap_or(false);
    *guard = (Instant::now(), light);
    light
}

fn query_portal_light_theme() -> Option<bool> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Settings",
    )
    .ok()?;
    let val: zbus::zvariant::OwnedValue = proxy
        .call("ReadOne", &("org.freedesktop.appearance", "color-scheme"))
        .ok()?;
    // 1 = prefer dark, 2 = prefer light, 0 = no preference
    let scheme: u32 = val.downcast_ref::<u32>().ok()?;
    Some(scheme != 1)
}

pub fn show_error(message: &str) {
    eprintln!("OpenHinoVoice: {message}");
    let _ = std::process::Command::new("notify-send")
        .args([
            "-a",
            "OpenHinoVoice",
            "-u",
            "critical",
            "OpenHinoVoice",
            message,
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ---------------------------------------------------------------------------
// Wayland Layer Shell floating HUD overlay (zwlr_layer_shell_v1)
// ---------------------------------------------------------------------------

enum LayerCmd {
    Present {
        w: i32,
        h: i32,
        argb: Vec<u8>,
        top_margin: i32,
    },
    Hide,
    Shutdown,
}

static LAYER_TX: Mutex<Option<Sender<LayerCmd>>> = Mutex::new(None);
static LAYER_THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

struct LayerState {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<ZwlrLayerSurfaceV1>,
    configured: bool,
    closed: bool,
    active_buf: Option<(wl_buffer::WlBuffer, wl_shm_pool::WlShmPool, File)>,
    visible: bool,
}

impl LayerState {
    fn new() -> Self {
        Self {
            compositor: None,
            shm: None,
            layer_shell: None,
            surface: None,
            layer_surface: None,
            configured: false,
            closed: false,
            active_buf: None,
            visible: false,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for LayerState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() => {
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(4),
                        qh,
                        (),
                    ));
                }
                "wl_shm" if state.shm.is_none() => {
                    state.shm =
                        Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ()));
                }
                "zwlr_layer_shell_v1" if state.layer_shell.is_none() => {
                    state.layer_shell =
                        Some(registry.bind::<ZwlrLayerShellV1, _, _>(name, version.min(4), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_shm::WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_shm_pool::WlShmPool,
        _: wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_buffer::WlBuffer,
        _: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for LayerState {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for LayerState {
    fn event(
        state: &mut Self,
        surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, .. } => {
                surface.ack_configure(serial);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

fn create_shm_file(bytes: &[u8]) -> Option<File> {
    let name = c"openhinovoice-hud";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    let mut file = if fd >= 0 {
        unsafe { File::from(OwnedFd::from_raw_fd(fd)) }
    } else {
        let path = crate::config::runtime_dir().join(format!("hud-{}.shm", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        let _ = std::fs::remove_file(&path);
        f
    };
    file.write_all(bytes).ok()?;
    file.flush().ok()?;
    Some(file)
}

fn run_layer_overlay(rx: std::sync::mpsc::Receiver<LayerCmd>) {
    let Ok(conn) = Connection::connect_to_env() else {
        return;
    };
    let display = conn.display();
    let mut queue: EventQueue<LayerState> = conn.new_event_queue();
    let qh = queue.handle();
    let mut state = LayerState::new();
    let _registry = display.get_registry(&qh, ());
    if queue.roundtrip(&mut state).is_err() {
        return;
    }
    let (Some(compositor), Some(shm), Some(layer_shell)) = (
        state.compositor.clone(),
        state.shm.clone(),
        state.layer_shell.clone(),
    ) else {
        return;
    };

    let wl_fd = conn.as_fd().as_raw_fd();
    loop {
        // Coalesce queued frames so we only render the latest frame.
        let mut pending = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(cmd) => Some(cmd),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        while let Ok(next) = rx.try_recv() {
            match next {
                LayerCmd::Shutdown => {
                    pending = Some(LayerCmd::Shutdown);
                    break;
                }
                other => pending = Some(other),
            }
        }

        if let Some(cmd) = pending {
            match cmd {
                LayerCmd::Shutdown => break,
                LayerCmd::Hide => {
                    if state.visible {
                        if let Some(ls) = state.layer_surface.take() {
                            ls.destroy();
                        }
                        if let Some(s) = state.surface.take() {
                            s.destroy();
                        }
                        if let Some((buf, pool, _)) = state.active_buf.take() {
                            buf.destroy();
                            pool.destroy();
                        }
                        state.configured = false;
                        state.visible = false;
                        let _ = conn.flush();
                    }
                }
                LayerCmd::Present {
                    w,
                    h,
                    argb,
                    top_margin,
                } => {
                    if w <= 0 || h <= 0 || argb.len() != (w * h * 4) as usize {
                        continue;
                    }
                    if state.surface.is_none() || state.closed {
                        state.closed = false;
                        state.configured = false;
                        let wl_surf = compositor.create_surface(&qh, ());
                        // Empty input region => 100% click-through!
                        let empty_region = compositor.create_region(&qh, ());
                        wl_surf.set_input_region(Some(&empty_region));
                        empty_region.destroy();

                        let ls = layer_shell.get_layer_surface(
                            &wl_surf,
                            None,
                            Layer::Overlay,
                            "openhinovoice-hud".to_string(),
                            &qh,
                            (),
                        );
                        ls.set_anchor(Anchor::Top);
                        ls.set_keyboard_interactivity(KeyboardInteractivity::None);
                        ls.set_exclusive_zone(-1);
                        ls.set_margin(top_margin.max(0), 0, 0, 0);
                        ls.set_size(w as u32, h as u32);
                        wl_surf.commit();
                        state.surface = Some(wl_surf);
                        state.layer_surface = Some(ls);
                        let _ = queue.roundtrip(&mut state);
                    } else if let Some(ls) = state.layer_surface.as_ref() {
                        ls.set_margin(top_margin.max(0), 0, 0, 0);
                        ls.set_size(w as u32, h as u32);
                    }

                    if !state.configured {
                        let _ = queue.roundtrip(&mut state);
                    }
                    if let (Some(wl_surf), Some(file)) =
                        (state.surface.as_ref(), create_shm_file(&argb))
                    {
                        let stride = w * 4;
                        let pool = shm.create_pool(file.as_fd(), stride * h, &qh, ());
                        let buf =
                            pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, &qh, ());
                        wl_surf.attach(Some(&buf), 0, 0);
                        wl_surf.damage_buffer(0, 0, w, h);
                        wl_surf.commit();
                        if let Some((old_buf, old_pool, _)) =
                            state.active_buf.replace((buf, pool, file))
                        {
                            old_buf.destroy();
                            old_pool.destroy();
                        }
                        state.visible = true;
                        let _ = conn.flush();
                    }
                }
            }
        }

        // Pump Wayland events without blocking.
        if let Some(guard) = conn.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: wl_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
            if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
                if guard.read().is_err() {
                    break;
                }
            } else {
                drop(guard);
            }
        }
        if queue.dispatch_pending(&mut state).is_err() {
            break;
        }
    }
}

pub fn present_buffer(w: i32, h: i32, argb: Vec<u8>, top_margin: i32) {
    if let Some(tx) = LAYER_TX.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send(LayerCmd::Present {
            w,
            h,
            argb,
            top_margin,
        });
    }
}

pub fn hide_overlay() {
    if let Some(tx) = LAYER_TX.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send(LayerCmd::Hide);
    }
}

// ---------------------------------------------------------------------------
// KDE StatusNotifierItem System Tray (ksni)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinuxTrayKind {
    Idle,
    Recording,
    Processing,
    Error,
}

struct OhvTray {
    kind: LinuxTrayKind,
    tooltip: String,
    recovery_count: usize,
    last_text_available: bool,
}

fn decode_png_to_ksni_icon(bytes: &[u8]) -> Option<ksni::Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let size = reader.output_buffer_size().unwrap_or(0);
    if size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let info = reader.next_frame(&mut buf).ok()?;
    let raw = &buf[..info.buffer_size()];
    let w = info.width as i32;
    let h = info.height as i32;
    let mut argb = Vec::with_capacity((w * h * 4) as usize);
    match info.color_type {
        png::ColorType::Rgba => {
            for px in raw.as_chunks::<4>().0 {
                argb.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
            }
        }
        png::ColorType::Rgb => {
            for px in raw.as_chunks::<3>().0 {
                argb.extend_from_slice(&[255, px[0], px[1], px[2]]);
            }
        }
        _ => return None,
    }
    Some(ksni::Icon {
        width: w,
        height: h,
        data: argb,
    })
}

fn tray_icon_pixmap(kind: LinuxTrayKind) -> Vec<ksni::Icon> {
    let light = system_uses_light_theme();
    let bytes: &[u8] = match (light, kind) {
        (true, LinuxTrayKind::Idle) => include_bytes!("../assets/branding/tray/light-idle.png"),
        (true, LinuxTrayKind::Recording) => {
            include_bytes!("../assets/branding/tray/light-recording.png")
        }
        (true, LinuxTrayKind::Processing) => {
            include_bytes!("../assets/branding/tray/light-processing.png")
        }
        (true, LinuxTrayKind::Error) => include_bytes!("../assets/branding/tray/light-error.png"),
        (false, LinuxTrayKind::Idle) => include_bytes!("../assets/branding/tray/dark-idle.png"),
        (false, LinuxTrayKind::Recording) => {
            include_bytes!("../assets/branding/tray/dark-recording.png")
        }
        (false, LinuxTrayKind::Processing) => {
            include_bytes!("../assets/branding/tray/dark-processing.png")
        }
        (false, LinuxTrayKind::Error) => include_bytes!("../assets/branding/tray/dark-error.png"),
    };
    decode_png_to_ksni_icon(bytes).into_iter().collect()
}

impl ksni::Tray for OhvTray {
    fn id(&self) -> String {
        "openhinovoice".into()
    }

    fn title(&self) -> String {
        "OpenHinoVoice".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        tray_icon_pixmap(self.kind)
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "OpenHinoVoice".into(),
            description: self.tooltip.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).arg("--settings").spawn();
        }
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let mut items: Vec<ksni::MenuItem<Self>> = Vec::new();
        items.push(
            StandardItem {
                label: "設定".into(),
                activate: Box::new(|_| {
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).arg("--settings").spawn();
                    }
                }),
                ..Default::default()
            }
            .into(),
        );
        if self.recovery_count > 0 {
            items.push(
                StandardItem {
                    label: format!("未入力テキストをコピー（残り{}件）", self.recovery_count),
                    activate: Box::new(|_| {
                        RECOVERY_REQUESTED.store(true, Ordering::SeqCst);
                        crate::hook::wake_resident();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        if self.last_text_available {
            items.push(
                StandardItem {
                    label: "直近の認識テキストをコピー".into(),
                    activate: Box::new(|_| {
                        LAST_TEXT_REQUESTED.store(true, Ordering::SeqCst);
                        crate::hook::wake_resident();
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(ksni::MenuItem::Separator);
        items.push(
            StandardItem {
                label: "終了".into(),
                activate: Box::new(|_| {
                    EXIT_REQUESTED.store(true, Ordering::SeqCst);
                    crate::hook::wake_resident();
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

static TRAY_HANDLE: Mutex<Option<ksni::blocking::Handle<OhvTray>>> = Mutex::new(None);
static CREATED: AtomicBool = AtomicBool::new(false);

pub fn create_linux_overlay_and_tray() -> Result<(), String> {
    if CREATED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let (tx, rx) = channel();
        *LAYER_TX.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        if let Ok(handle) = std::thread::Builder::new()
            .name("wayland-overlay".into())
            .spawn(move || run_layer_overlay(rx))
        {
            *LAYER_THREAD.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
        }
    }
    if !crate::config::is_test_sandbox() {
        use ksni::blocking::TrayMethods;
        let tray = OhvTray {
            kind: LinuxTrayKind::Idle,
            tooltip: "OpenHinoVoice - 待機中".into(),
            recovery_count: RECOVERY_COUNT.load(Ordering::SeqCst),
            last_text_available: LAST_TEXT_AVAILABLE.load(Ordering::SeqCst),
        };
        match tray.spawn() {
            Ok(handle) => {
                *TRAY_HANDLE.lock().unwrap_or_else(|p| p.into_inner()) = Some(handle);
            }
            Err(e) => {
                eprintln!("トレイアイコンを登録できません: {e}");
            }
        }
    }
    Ok(())
}

pub fn update_linux_tray(kind: LinuxTrayKind, tooltip: &str) {
    let recovery_count = RECOVERY_COUNT.load(Ordering::SeqCst);
    let last_text_available = LAST_TEXT_AVAILABLE.load(Ordering::SeqCst);
    let tooltip = tooltip.to_owned();
    if let Some(handle) = TRAY_HANDLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
    {
        handle.update(move |tray| {
            tray.kind = kind;
            tray.tooltip = tooltip;
            tray.recovery_count = recovery_count;
            tray.last_text_available = last_text_available;
        });
    }
}

pub fn refresh_linux_tray_menu() {
    let recovery_count = RECOVERY_COUNT.load(Ordering::SeqCst);
    let last_text_available = LAST_TEXT_AVAILABLE.load(Ordering::SeqCst);
    if let Some(handle) = TRAY_HANDLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
    {
        handle.update(move |tray| {
            tray.recovery_count = recovery_count;
            tray.last_text_available = last_text_available;
        });
    }
}

pub fn remove_linux_tray() {
    if let Some(handle) = TRAY_HANDLE.lock().unwrap_or_else(|p| p.into_inner()).take() {
        handle.shutdown();
    }
    if let Some(tx) = LAYER_TX.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = tx.send(LayerCmd::Shutdown);
    }
    if let Some(thread) = LAYER_THREAD
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        let _ = thread.join();
    }
    CREATED.store(false, Ordering::SeqCst);
}
