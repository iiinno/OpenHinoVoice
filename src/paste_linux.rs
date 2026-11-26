//! Linux (Wayland / KDE Plasma 6) clipboard snapshot/restore and Ctrl+V injection.
//! Clipboard uses Wayland ext_data_control_manager_v1 (with zwlr_data_control_manager_v1
//! and KDE Klipper fallbacks). Paste keystroke uses org.freedesktop.portal.RemoteDesktop
//! with persist_mode=2 so permission is requested once and reused via restore_token.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

const SNAPSHOT_MAX_BYTES: usize = 64 * 1024 * 1024;
const SNAPSHOT_MAX_FORMATS: usize = 256;
const PASTE_HOLD_DELAY: Duration = Duration::from_millis(350);

// Linux evdev keycodes (linux/input-event-codes.h)
const KEY_LEFTCTRL: i32 = 29;
const KEY_V: i32 = 47;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteTarget {
    process_id: u32,
}

impl PasteTarget {
    pub fn process_id(&self) -> u32 {
        self.process_id
    }
}

pub fn capture_target() -> Result<PasteTarget, String> {
    // Wayland does not expose arbitrary foreground window PIDs to clients.
    Ok(PasteTarget { process_id: 0 })
}

#[derive(Clone)]
struct ClipboardSnapshot {
    entries: Vec<(String, Vec<u8>)>,
}

enum ClipCommand {
    Snapshot {
        reply: Sender<Result<Option<ClipboardSnapshot>, String>>,
    },
    SetText {
        text: String,
        reply: Sender<Result<Arc<AtomicBool>, String>>,
    },
    Restore {
        snapshot: ClipboardSnapshot,
        still_owned: Arc<AtomicBool>,
        reply: Sender<Result<bool, String>>,
    },
}

struct WaylandClipboardHandle {
    tx: Sender<ClipCommand>,
}

fn clipboard_worker() -> Option<&'static WaylandClipboardHandle> {
    static WORKER: OnceLock<Option<WaylandClipboardHandle>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            std::env::var_os("WAYLAND_DISPLAY")?;
            let (tx, rx) = channel::<ClipCommand>();
            let (ready_tx, ready_rx) = channel::<bool>();
            if std::thread::Builder::new()
                .name("wayland-clipboard".into())
                .spawn(move || run_wayland_clipboard(rx, ready_tx))
                .is_err()
            {
                return None;
            }
            match ready_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(true) => Some(WaylandClipboardHandle { tx }),
                _ => None,
            }
        })
        .as_ref()
}

struct ClipState {
    seat: Option<wl_seat::WlSeat>,
    ext_manager: Option<ExtDataControlManagerV1>,
    ext_device: Option<ExtDataControlDeviceV1>,
    pending_offers: HashMap<u32, Vec<String>>,
    current_offer: Option<(ExtDataControlOfferV1, Vec<String>)>,
    active_source: Option<ActiveSource>,
}

struct ActiveSource {
    _source: ExtDataControlSourceV1,
    entries: Vec<(String, Vec<u8>)>,
    still_owned: Arc<AtomicBool>,
}

impl ClipState {
    fn new() -> Self {
        Self {
            seat: None,
            ext_manager: None,
            ext_device: None,
            pending_offers: HashMap::new(),
            current_offer: None,
            active_source: None,
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClipState {
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
                "wl_seat" if state.seat.is_none() => {
                    state.seat =
                        Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ()));
                }
                "ext_data_control_manager_v1" if state.ext_manager.is_none() => {
                    state.ext_manager =
                        Some(registry.bind::<ExtDataControlManagerV1, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for ClipState {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for ClipState {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state
                    .pending_offers
                    .insert(id.id().protocol_id(), Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some((old, _)) = state.current_offer.take() {
                    old.destroy();
                }
                if let Some(offer) = id {
                    let mimes = state
                        .pending_offers
                        .remove(&offer.id().protocol_id())
                        .unwrap_or_default();
                    state.current_offer = Some((offer, mimes));
                }
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.pending_offers.remove(&offer.id().protocol_id());
                offer.destroy();
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: None } => {}
            ext_data_control_device_v1::Event::Finished => {
                state.ext_device = None;
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(ClipState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            let pid = offer.id().protocol_id();
            if let Some(mimes) = state.pending_offers.get_mut(&pid) {
                if mimes.len() < SNAPSHOT_MAX_FORMATS && !mimes.contains(&mime_type) {
                    mimes.push(mime_type);
                }
            } else if let Some((cur, mimes)) = &mut state.current_offer {
                if cur.id().protocol_id() == pid
                    && mimes.len() < SNAPSHOT_MAX_FORMATS
                    && !mimes.contains(&mime_type)
                {
                    mimes.push(mime_type);
                }
            }
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for ClipState {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                let data = state.active_source.as_ref().and_then(|active| {
                    if active._source.id() == source.id() {
                        active
                            .entries
                            .iter()
                            .find(|(m, _)| *m == mime_type)
                            .map(|(_, bytes)| bytes.clone())
                    } else {
                        None
                    }
                });
                if let Some(bytes) = data {
                    std::thread::spawn(move || {
                        let mut file = std::fs::File::from(fd);
                        let _ = file.write_all(&bytes);
                    });
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                if let Some(active) = &state.active_source {
                    if active._source.id() == source.id() {
                        active.still_owned.store(false, Ordering::Release);
                        state.active_source = None;
                    }
                }
                source.destroy();
            }
            _ => {}
        }
    }
}

fn is_skippable_mime(mime: &str) -> bool {
    matches!(
        mime,
        "TARGETS"
            | "TIMESTAMP"
            | "SAVE_TARGETS"
            | "MULTIPLE"
            | "DELETE"
            | "x-kde-passwordManagerHint"
            | "application/x-kde-passwordManagerHint"
    )
}

fn read_offer_mime(
    conn: &Connection,
    offer: &ExtDataControlOfferV1,
    mime: &str,
    budget: usize,
) -> Option<Vec<u8>> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    offer.receive(mime.to_string(), write_fd.as_fd());
    drop(write_fd);
    let _ = conn.flush();

    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut file = std::fs::File::from(read_fd);
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len().saturating_add(n) > budget {
                        let _ = tx.send(None);
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(_) => {
                    let _ = tx.send(None);
                    return;
                }
            }
        }
        let _ = tx.send(Some(buf));
    });
    rx.recv_timeout(Duration::from_millis(600)).ok().flatten()
}

fn run_wayland_clipboard(rx: std::sync::mpsc::Receiver<ClipCommand>, ready_tx: Sender<bool>) {
    let Ok(conn) = Connection::connect_to_env() else {
        let _ = ready_tx.send(false);
        return;
    };
    let display = conn.display();
    let mut queue: EventQueue<ClipState> = conn.new_event_queue();
    let qh = queue.handle();
    let mut state = ClipState::new();
    let _registry = display.get_registry(&qh, ());
    if queue.roundtrip(&mut state).is_err() {
        let _ = ready_tx.send(false);
        return;
    }
    let (Some(manager), Some(seat)) = (state.ext_manager.clone(), state.seat.clone()) else {
        let _ = ready_tx.send(false);
        return;
    };
    let device = manager.get_data_device(&seat, &qh, ());
    state.ext_device = Some(device);
    if queue.roundtrip(&mut state).is_err() {
        let _ = ready_tx.send(false);
        return;
    }
    let _ = ready_tx.send(true);

    let wl_fd = conn.as_fd().as_raw_fd();
    loop {
        // Dispatch any pending Wayland events non-blockingly first.
        let _ = conn.flush();
        if let Some(guard) = conn.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: wl_fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, 15) };
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

        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                ClipCommand::Snapshot { reply } => {
                    let _ = queue.roundtrip(&mut state);
                    if let Some(active) = &state.active_source {
                        let _ = reply.send(Ok(Some(ClipboardSnapshot {
                            entries: active.entries.clone(),
                        })));
                        continue;
                    }
                    let Some((offer, mimes)) = state.current_offer.as_ref() else {
                        let _ = reply.send(Ok(None));
                        continue;
                    };
                    let mut remaining = SNAPSHOT_MAX_BYTES;
                    let mut entries = Vec::new();
                    for mime in mimes {
                        if is_skippable_mime(mime) || remaining == 0 {
                            continue;
                        }
                        if let Some(bytes) = read_offer_mime(&conn, offer, mime, remaining) {
                            remaining = remaining.saturating_sub(bytes.len());
                            entries.push((mime.clone(), bytes));
                        }
                    }
                    let snap = if entries.is_empty() {
                        None
                    } else {
                        Some(ClipboardSnapshot { entries })
                    };
                    let _ = reply.send(Ok(snap));
                }
                ClipCommand::SetText { text, reply } => {
                    let Some(device) = state.ext_device.as_ref() else {
                        let _ = reply
                            .send(Err("Wayland クリップボードデバイスが切断されました".into()));
                        continue;
                    };
                    let source = manager.create_data_source(&qh, ());
                    let bytes = text.into_bytes();
                    let mimes = [
                        "text/plain;charset=utf-8",
                        "text/plain",
                        "UTF8_STRING",
                        "STRING",
                    ];
                    let mut entries = Vec::with_capacity(mimes.len());
                    for mime in mimes {
                        source.offer(mime.to_string());
                        entries.push((mime.to_string(), bytes.clone()));
                    }
                    let still_owned = Arc::new(AtomicBool::new(true));
                    if let Some(old) = state.active_source.take() {
                        old.still_owned.store(false, Ordering::Release);
                    }
                    device.set_selection(Some(&source));
                    state.active_source = Some(ActiveSource {
                        _source: source,
                        entries,
                        still_owned: Arc::clone(&still_owned),
                    });
                    let _ = queue.roundtrip(&mut state);
                    let _ = reply.send(Ok(still_owned));
                }
                ClipCommand::Restore {
                    snapshot,
                    still_owned,
                    reply,
                } => {
                    let _ = queue.dispatch_pending(&mut state);
                    if !still_owned.load(Ordering::Acquire) {
                        let _ = reply.send(Ok(false));
                        continue;
                    }
                    let Some(device) = state.ext_device.as_ref() else {
                        let _ = reply.send(Ok(false));
                        continue;
                    };
                    if snapshot.entries.is_empty() {
                        device.set_selection(None);
                        state.active_source = None;
                        let _ = queue.roundtrip(&mut state);
                        let _ = reply.send(Ok(true));
                        continue;
                    }
                    let source = manager.create_data_source(&qh, ());
                    for (mime, _) in &snapshot.entries {
                        source.offer(mime.clone());
                    }
                    let restored_owned = Arc::new(AtomicBool::new(true));
                    device.set_selection(Some(&source));
                    state.active_source = Some(ActiveSource {
                        _source: source,
                        entries: snapshot.entries,
                        still_owned: restored_owned,
                    });
                    let _ = queue.roundtrip(&mut state);
                    let _ = reply.send(Ok(true));
                }
            }
        }
    }
}

fn klipper_get_text() -> Option<String> {
    let conn = zbus::blocking::Connection::session().ok()?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.klipper",
        "/klipper",
        "org.kde.klipper.klipper",
    )
    .ok()?;
    proxy.call::<_, _, String>("getClipboardContents", &()).ok()
}

fn klipper_set_text(text: &str) -> Result<(), String> {
    let conn = zbus::blocking::Connection::session()
        .map_err(|e| format!("セッションバスに接続できません: {e}"))?;
    let proxy = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.klipper",
        "/klipper",
        "org.kde.klipper.klipper",
    )
    .map_err(|e| format!("Klipper に接続できません: {e}"))?;
    proxy
        .call::<_, _, ()>("setClipboardContents", &(text,))
        .map_err(|e| format!("Klipper にクリップボードを設定できません: {e}"))
}

pub fn copy_text_to_clipboard(
    text: &str,
    _owner: crate::overlay::OverlayHandle,
) -> Result<(), String> {
    if text.contains('\0') {
        return Err("入力文字列に NUL が含まれています".into());
    }
    let mut ok = false;
    if let Some(worker) = clipboard_worker() {
        let (tx, rx) = channel();
        if worker
            .tx
            .send(ClipCommand::SetText {
                text: text.to_owned(),
                reply: tx,
            })
            .is_ok()
            && matches!(rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(_)))
        {
            ok = true;
        }
    }
    // Also mirror into KDE Klipper so copies from short-lived --settings processes persist.
    if klipper_set_text(text).is_ok() {
        ok = true;
    }
    if ok {
        Ok(())
    } else {
        Err("クリップボードへ書き込めません".into())
    }
}

// ---------------------------------------------------------------------------
// XDG RemoteDesktop Portal (Ctrl+V injection with persistent restore_token)
// ---------------------------------------------------------------------------

static PORTAL_SEQ: AtomicU64 = AtomicU64::new(1);

fn portal_token_path() -> std::path::PathBuf {
    crate::config::config_path().with_file_name("portal_token.bin")
}

fn load_portal_token() -> Option<String> {
    let sealed = std::fs::read(portal_token_path()).ok()?;
    let plain = crate::chatgpt::dpapi::unprotect(&sealed).ok()?;
    let token = String::from_utf8(plain).ok()?;
    (!token.trim().is_empty()).then_some(token)
}

fn save_portal_token(token: &str) {
    if token.trim().is_empty() {
        return;
    }
    if let Ok(sealed) = crate::chatgpt::dpapi::protect(token.as_bytes()) {
        let path = portal_token_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, &sealed);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

struct PortalSession {
    conn: zbus::blocking::Connection,
    session_path: zbus::zvariant::OwnedObjectPath,
}

static PORTAL: OnceLock<Mutex<Option<PortalSession>>> = OnceLock::new();

fn portal_store() -> &'static Mutex<Option<PortalSession>> {
    PORTAL.get_or_init(|| Mutex::new(None))
}

fn next_token(prefix: &str) -> String {
    let n = PORTAL_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("ohv_{prefix}_{}_{n}", std::process::id())
}

fn expected_request_path(
    conn: &zbus::blocking::Connection,
    handle_token: &str,
) -> Result<String, String> {
    let unique = conn.unique_name().ok_or("D-Bus の接続名を取得できません")?;
    let sender = unique.as_str().trim_start_matches(':').replace('.', "_");
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{handle_token}"
    ))
}

fn call_portal_request(
    conn: &zbus::blocking::Connection,
    handle_token: &str,
    invoke: impl FnOnce() -> Result<zbus::zvariant::OwnedObjectPath, String>,
) -> Result<HashMap<String, zbus::zvariant::OwnedValue>, String> {
    let req_path = expected_request_path(conn, handle_token)?;
    let req_proxy = zbus::blocking::Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        req_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .map_err(|e| format!("ポータル応答の待受を作成できません: {e}"))?;
    let mut signals = req_proxy
        .receive_signal("Response")
        .map_err(|e| format!("ポータル応答の待受を開始できません: {e}"))?;

    let _returned_path = invoke()?;

    let msg = signals
        .next()
        .ok_or("ポータルからの応答を受け取れませんでした")?;
    let (code, results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) = msg
        .body()
        .deserialize()
        .map_err(|e| format!("ポータル応答の形式が不正です: {e}"))?;
    if code != 0 {
        return Err(format!(
            "RemoteDesktop ポータルの許可が得られませんでした（応答コード {code}）"
        ));
    }
    Ok(results)
}

fn open_portal_session() -> Result<PortalSession, String> {
    use zbus::zvariant::{OwnedObjectPath, Value};

    let conn = zbus::blocking::Connection::session()
        .map_err(|e| format!("セッションバスに接続できません: {e}"))?;
    let rd = zbus::blocking::Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.RemoteDesktop",
    )
    .map_err(|e| format!("RemoteDesktop ポータルに接続できません: {e}"))?;

    // 1. CreateSession
    let create_token = next_token("create");
    let session_token = next_token("session");
    let create_results = call_portal_request(&conn, &create_token, || {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(create_token.as_str()));
        opts.insert("session_handle_token", Value::from(session_token.as_str()));
        rd.call::<_, _, OwnedObjectPath>("CreateSession", &(opts,))
            .map_err(|e| format!("RemoteDesktop セッションを作成できません: {e}"))
    })?;
    let session_handle_str: String = create_results
        .get("session_handle")
        .and_then(|v| v.downcast_ref::<zbus::zvariant::Str<'_>>().ok())
        .map(|s| s.to_string())
        .ok_or("RemoteDesktop セッションハンドルを取得できません")?;
    let session_path = OwnedObjectPath::try_from(session_handle_str)
        .map_err(|e| format!("セッションパスが不正です: {e}"))?;

    // 2. SelectDevices (types = 1 = KEYBOARD, persist_mode = 2 = persistent)
    let select_token = next_token("select");
    let saved_token = load_portal_token();
    call_portal_request(&conn, &select_token, || {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(select_token.as_str()));
        opts.insert("types", Value::from(1u32));
        opts.insert("persist_mode", Value::from(2u32));
        if let Some(ref rt) = saved_token {
            opts.insert("restore_token", Value::from(rt.as_str()));
        }
        rd.call::<_, _, OwnedObjectPath>("SelectDevices", &(&session_path, opts))
            .map_err(|e| format!("RemoteDesktop デバイスを選択できません: {e}"))
    })?;

    // 3. Start (may show KDE permission dialog the first time; saves restore_token)
    let start_token = next_token("start");
    let start_results = call_portal_request(&conn, &start_token, || {
        let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
        opts.insert("handle_token", Value::from(start_token.as_str()));
        rd.call::<_, _, OwnedObjectPath>("Start", &(&session_path, "", opts))
            .map_err(|e| format!("RemoteDesktop セッションを開始できません: {e}"))
    })?;
    if let Some(rt) = start_results
        .get("restore_token")
        .and_then(|v| v.downcast_ref::<zbus::zvariant::Str<'_>>().ok())
    {
        save_portal_token(rt.as_str());
    }

    Ok(PortalSession { conn, session_path })
}

fn send_ctrl_v_on(session: &PortalSession) -> Result<(), String> {
    use zbus::zvariant::Value;
    let rd = zbus::blocking::Proxy::new(
        &session.conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.RemoteDesktop",
    )
    .map_err(|e| format!("RemoteDesktop プロキシを作成できません: {e}"))?;
    let empty: HashMap<&str, Value<'_>> = HashMap::new();
    let send_key = |keycode: i32, state: u32| -> Result<(), String> {
        rd.call::<_, _, ()>(
            "NotifyKeyboardKeycode",
            &(&session.session_path, &empty, keycode, state),
        )
        .map_err(|e| format!("Ctrl+V の送信に失敗しました: {e}"))
    };
    send_key(KEY_LEFTCTRL, 1)?;
    std::thread::sleep(Duration::from_millis(15));
    let v_res = send_key(KEY_V, 1).and_then(|()| {
        std::thread::sleep(Duration::from_millis(15));
        send_key(KEY_V, 0)
    });
    std::thread::sleep(Duration::from_millis(15));
    let ctrl_res = send_key(KEY_LEFTCTRL, 0);
    v_res.and(ctrl_res)
}

fn send_paste_ctrl_v() -> Result<Duration, String> {
    let mut guard = portal_store().lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        *guard = Some(open_portal_session()?);
    }
    if let Some(session) = guard.as_ref() {
        if send_ctrl_v_on(session).is_ok() {
            return Ok(PASTE_HOLD_DELAY);
        }
    }
    // Session may have expired (e.g. after screen lock). Reconnect once.
    *guard = None;
    let session = open_portal_session()?;
    let res = send_ctrl_v_on(&session);
    *guard = Some(session);
    res.map(|()| PASTE_HOLD_DELAY)
}

/// Start the RemoteDesktop portal session in the background at app launch so
/// any one-time KDE permission dialog appears immediately instead of mid-dictation.
pub fn warm_up_portal() {
    if crate::config::is_test_sandbox() || std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("portal-warmup".into())
        .spawn(|| {
            let mut guard = portal_store().lock().unwrap_or_else(|p| p.into_inner());
            if guard.is_none() {
                if let Ok(session) = open_portal_session() {
                    *guard = Some(session);
                }
            }
        });
}

// ---------------------------------------------------------------------------
// PasteTransaction (cross-platform contract matching Windows paste.rs)
// ---------------------------------------------------------------------------

enum BackendState {
    Wayland {
        previous: Option<ClipboardSnapshot>,
        still_owned: Arc<AtomicBool>,
    },
    Klipper {
        previous: Option<String>,
        written: String,
    },
    None,
}

#[must_use]
pub struct PasteTransaction {
    backend: BackendState,
    deadline: Instant,
    delivery_error: Option<String>,
    _same_thread: PhantomData<Rc<()>>,
}

#[derive(Debug)]
pub enum RestoreProgress {
    Pending,
    Waiting(String),
    Finished(Result<(), String>),
}

impl PasteTransaction {
    pub fn begin(
        text: &str,
        _target: PasteTarget,
        _owner: crate::overlay::OverlayHandle,
        now: Instant,
    ) -> Result<Self, String> {
        if text.is_empty() {
            return Err("入力テキストが空です".into());
        }
        if text.contains('\0') {
            return Err("入力文字列に NUL が含まれています".into());
        }

        let backend = if let Some(worker) = clipboard_worker() {
            let (snap_tx, snap_rx) = channel();
            worker
                .tx
                .send(ClipCommand::Snapshot { reply: snap_tx })
                .map_err(|_| "クリップボードワーカーが停止しました".to_string())?;
            let previous = snap_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "元のクリップボードの読み取りが時間切れになりました".to_string())??;

            let (set_tx, set_rx) = channel();
            worker
                .tx
                .send(ClipCommand::SetText {
                    text: text.to_owned(),
                    reply: set_tx,
                })
                .map_err(|_| "クリップボードワーカーが停止しました".to_string())?;
            let still_owned = set_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "クリップボードへの書き込みが時間切れになりました".to_string())??;

            BackendState::Wayland {
                previous,
                still_owned,
            }
        } else {
            let previous = klipper_get_text();
            klipper_set_text(text)?;
            BackendState::Klipper {
                previous,
                written: text.to_owned(),
            }
        };

        // Brief settle so the compositor announces our clipboard offer before Ctrl+V arrives.
        std::thread::sleep(Duration::from_millis(25));

        match send_paste_ctrl_v() {
            Ok(hold) => Ok(Self {
                backend,
                deadline: now + hold,
                delivery_error: None,
                _same_thread: PhantomData,
            }),
            Err(error) => {
                // Keep the transcribed text in the clipboard on paste failure so the user can Ctrl+V manually.
                Ok(Self {
                    backend: BackendState::None,
                    deadline: Instant::now(),
                    delivery_error: Some(format!(
                        "{error}（テキストはクリップボードに残しています）"
                    )),
                    _same_thread: PhantomData,
                })
            }
        }
    }

    pub fn delivery_error(&self) -> Option<&str> {
        self.delivery_error.as_deref()
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn poll_restore(&mut self, now: Instant) -> RestoreProgress {
        if now < self.deadline {
            return RestoreProgress::Pending;
        }
        self.restore_now();
        RestoreProgress::Finished(Ok(()))
    }

    fn restore_now(&mut self) {
        let state = std::mem::replace(&mut self.backend, BackendState::None);
        match state {
            BackendState::Wayland {
                previous: Some(snapshot),
                still_owned,
            } => {
                if let Some(worker) = clipboard_worker() {
                    let (tx, rx) = channel();
                    if worker
                        .tx
                        .send(ClipCommand::Restore {
                            snapshot,
                            still_owned,
                            reply: tx,
                        })
                        .is_ok()
                    {
                        let _ = rx.recv_timeout(Duration::from_secs(2));
                    }
                }
            }
            BackendState::Klipper {
                previous: Some(prev),
                written,
            } if klipper_get_text().as_deref() == Some(written.as_str()) => {
                let _ = klipper_set_text(&prev);
            }
            _ => {}
        }
    }
}

impl Drop for PasteTransaction {
    fn drop(&mut self) {
        self.restore_now();
    }
}
