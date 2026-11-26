use crate::config::{Hotkey, MOD_ALT, MOD_CTRL, MOD_SHIFT, MOD_WIN};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use super::{KeyEvent, MIRROR_IDLE};

static WAKE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
static KGA_CMD: Mutex<Option<Sender<KgaCommand>>> = Mutex::new(None);
static THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

const COMPONENT_ID: &str = "openhinovoice";
const COMPONENT_NAME: &str = "OpenHinoVoice";
const ACTION_HOTKEY: &str = "toggle_recording";
const ACTION_FINISH: &str = "finish_recording";
const ACTION_CANCEL: &str = "cancel_recording";

// Qt::Key constants for org.kde.KGlobalAccel
const QT_KEY_ESCAPE: i32 = 0x0100_0000;
const QT_KEY_RETURN: i32 = 0x0100_0004;
const QT_KEY_ENTER: i32 = 0x0100_0005;
const QT_KEY_F1: i32 = 0x0100_0030;
const QT_MOD_SHIFT: i32 = 0x0200_0000;
const QT_MOD_CTRL: i32 = 0x0400_0000;
const QT_MOD_ALT: i32 = 0x0800_0000;
const QT_MOD_META: i32 = 0x1000_0000;

enum KgaCommand {
    SetHotkey(Hotkey),
    SetActive(bool),
    Shutdown,
}

fn wake_pair() -> &'static (Mutex<bool>, Condvar) {
    WAKE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

pub fn wake_resident() {
    let (lock, cvar) = wake_pair();
    let mut woken = lock.lock().unwrap_or_else(|p| p.into_inner());
    *woken = true;
    cvar.notify_all();
}

pub fn wait_for_wake(timeout: Duration) {
    let (lock, cvar) = wake_pair();
    let guard = lock.lock().unwrap_or_else(|p| p.into_inner());
    if *guard {
        let mut guard = guard;
        *guard = false;
        return;
    }
    let (mut guard, _) = cvar
        .wait_timeout_while(guard, timeout, |woken| !*woken)
        .unwrap_or_else(|p| p.into_inner());
    *guard = false;
}

pub fn control_socket_path() -> PathBuf {
    let name = crate::config::mutex_name("control");
    crate::config::runtime_dir().join(format!("{name}.sock"))
}

pub fn send_control_command(cmd: &str) -> Result<(), String> {
    let path = control_socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        format!(
            "常駐中の OpenHinoVoice に接続できません ({}): {e}",
            path.display()
        )
    })?;
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("制御コマンドを送信できません: {e}"))?;
    Ok(())
}

pub fn hotkey_to_qt_key(hotkey: Hotkey) -> Option<i32> {
    let base = match hotkey.vk {
        0x20 => 0x20,
        v @ (0x30..=0x39 | 0x41..=0x5A) => v as i32,
        v @ 0x70..=0x87 => QT_KEY_F1 + (v - 0x70) as i32,
        _ => return None,
    };
    let mut mods = 0i32;
    if hotkey.mods & MOD_SHIFT != 0 {
        mods |= QT_MOD_SHIFT;
    }
    if hotkey.mods & MOD_CTRL != 0 {
        mods |= QT_MOD_CTRL;
    }
    if hotkey.mods & MOD_ALT != 0 {
        mods |= QT_MOD_ALT;
    }
    if hotkey.mods & MOD_WIN != 0 {
        mods |= QT_MOD_META;
    }
    Some(base | mods)
}

pub fn on_mirror_changed(state: u8) {
    if let Some(tx) = KGA_CMD.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send(KgaCommand::SetActive(state != MIRROR_IDLE));
    }
}

pub fn on_hotkey_changed(hotkey: Hotkey) {
    if let Some(tx) = KGA_CMD.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
        let _ = tx.send(KgaCommand::SetHotkey(hotkey));
    }
}

pub fn install(sender: Sender<KeyEvent>, hotkey: Hotkey) -> Result<(), String> {
    RUNNING.store(true, Ordering::SeqCst);
    let mut handles = THREADS.lock().unwrap_or_else(|p| p.into_inner());

    // 1. Control Unix domain socket (works on all Linux compositors & CLI flags)
    let sock_tx = sender.clone();
    let sock_path = control_socket_path();
    let _ = std::fs::remove_file(&sock_path);
    if let Ok(listener) = UnixListener::bind(&sock_path) {
        let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
        let _ = listener.set_nonblocking(true);
        if let Ok(handle) = std::thread::Builder::new()
            .name("control-socket".into())
            .spawn(move || run_control_socket(listener, sock_tx))
        {
            handles.push(handle);
        }
    }

    // 2. KDE Plasma 6 KGlobalAccel D-Bus shortcut listener (no sudo required)
    if !crate::config::is_test_sandbox() {
        let (cmd_tx, cmd_rx) = channel::<KgaCommand>();
        *KGA_CMD.lock().unwrap_or_else(|p| p.into_inner()) = Some(cmd_tx);
        if let Ok(handle) = std::thread::Builder::new()
            .name("kglobalaccel".into())
            .spawn(move || run_kglobalaccel(sender, hotkey, cmd_rx))
        {
            handles.push(handle);
        }
    }

    Ok(())
}

pub fn uninstall() {
    RUNNING.store(false, Ordering::SeqCst);
    if let Some(tx) = KGA_CMD.lock().unwrap_or_else(|p| p.into_inner()).take() {
        let _ = tx.send(KgaCommand::Shutdown);
    }
    let handles: Vec<_> = THREADS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain(..)
        .collect();
    for handle in handles {
        let _ = handle.join();
    }
    let _ = std::fs::remove_file(control_socket_path());
}

fn dispatch_event(sender: &Sender<KeyEvent>, event: KeyEvent) {
    if matches!(event, KeyEvent::Hotkey) {
        super::PENDING_KEYS.fetch_add(1, Ordering::SeqCst);
        if sender.send(event).is_err() {
            super::acknowledge_hotkey();
            return;
        }
    } else if sender.send(event).is_err() {
        return;
    }
    wake_resident();
}

fn run_control_socket(listener: UnixListener, sender: Sender<KeyEvent>) {
    while RUNNING.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    match line.trim() {
                        "hotkey" | "trigger" => dispatch_event(&sender, KeyEvent::Hotkey),
                        "enter" | "finish" => dispatch_event(&sender, KeyEvent::Enter),
                        "esc" | "cancel" => dispatch_event(&sender, KeyEvent::Esc),
                        "exit" | "quit" => {
                            crate::overlay::EXIT_REQUESTED.store(true, Ordering::SeqCst);
                            wake_resident();
                        }
                        "settings" => {
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = std::process::Command::new(exe).arg("--settings").spawn();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn action_id(action: &'static str, friendly: &'static str) -> Vec<&'static str> {
    vec![COMPONENT_ID, action, COMPONENT_NAME, friendly]
}

fn set_kga_shortcut(proxy: &zbus::blocking::Proxy<'_>, id: &[&str], keys: &[i32]) {
    // flags: SetPresent (0x2) | NoAutoloading (0x4) = 6
    let _: Result<Vec<i32>, _> = proxy.call("setShortcut", &(id, keys, 6u32));
    let _: Result<(), _> = proxy.call("setForeignShortcut", &(id, keys));
}

fn run_kglobalaccel(
    sender: Sender<KeyEvent>,
    initial_hotkey: Hotkey,
    cmd_rx: std::sync::mpsc::Receiver<KgaCommand>,
) {
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(kga) = zbus::blocking::Proxy::new(
        &conn,
        "org.kde.kglobalaccel",
        "/kglobalaccel",
        "org.kde.KGlobalAccel",
    ) else {
        return;
    };

    let hotkey_id = action_id(ACTION_HOTKEY, "録音を開始");
    let finish_id = action_id(ACTION_FINISH, "録音を確定");
    let cancel_id = action_id(ACTION_CANCEL, "録音を中止");

    let _: Result<(), _> = kga.call("doRegister", &(&hotkey_id,));
    let _: Result<(), _> = kga.call("doRegister", &(&finish_id,));
    let _: Result<(), _> = kga.call("doRegister", &(&cancel_id,));

    if let Some(qt_key) = hotkey_to_qt_key(initial_hotkey) {
        set_kga_shortcut(&kga, &hotkey_id, &[qt_key]);
    }
    // Enter and Escape must stay unbound while idle.
    set_kga_shortcut(&kga, &finish_id, &[]);
    set_kga_shortcut(&kga, &cancel_id, &[]);
    let _: Result<(), _> = kga.call("setInactive", &(&finish_id,));
    let _: Result<(), _> = kga.call("setInactive", &(&cancel_id,));

    // Spawn a dedicated listener thread for globalShortcutPressed signals.
    let signal_conn = conn.clone();
    let signal_tx = sender.clone();
    let signal_thread = std::thread::Builder::new()
        .name("kglobalaccel-signals".into())
        .spawn(move || {
            let Ok(comp_proxy) = zbus::blocking::Proxy::new(
                &signal_conn,
                "org.kde.kglobalaccel",
                "/component/openhinovoice",
                "org.kde.kglobalaccel.Component",
            ) else {
                return;
            };
            let Ok(mut stream) = comp_proxy.receive_signal("globalShortcutPressed") else {
                return;
            };
            while RUNNING.load(Ordering::Acquire) {
                let Some(msg) = stream.next() else {
                    break;
                };
                if !RUNNING.load(Ordering::Acquire) {
                    break;
                }
                let Ok((component, action, _timestamp)) =
                    msg.body().deserialize::<(String, String, i64)>()
                else {
                    continue;
                };
                if component != COMPONENT_ID {
                    continue;
                }
                match action.as_str() {
                    ACTION_HOTKEY => dispatch_event(&signal_tx, KeyEvent::Hotkey),
                    ACTION_FINISH => dispatch_event(&signal_tx, KeyEvent::Enter),
                    ACTION_CANCEL => dispatch_event(&signal_tx, KeyEvent::Esc),
                    _ => {}
                }
            }
        })
        .ok();

    let mut active = false;
    while RUNNING.load(Ordering::Acquire) {
        match cmd_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(KgaCommand::SetHotkey(hk)) => {
                if let Some(qt_key) = hotkey_to_qt_key(hk) {
                    set_kga_shortcut(&kga, &hotkey_id, &[qt_key]);
                }
            }
            Ok(KgaCommand::SetActive(now_active)) => {
                if now_active != active {
                    active = now_active;
                    if active {
                        set_kga_shortcut(&kga, &finish_id, &[QT_KEY_RETURN, QT_KEY_ENTER]);
                        set_kga_shortcut(&kga, &cancel_id, &[QT_KEY_ESCAPE]);
                    } else {
                        set_kga_shortcut(&kga, &finish_id, &[]);
                        set_kga_shortcut(&kga, &cancel_id, &[]);
                        let _: Result<(), _> = kga.call("setInactive", &(&finish_id,));
                        let _: Result<(), _> = kga.call("setInactive", &(&cancel_id,));
                    }
                }
            }
            Ok(KgaCommand::Shutdown) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Always release Enter, Esc, and hotkey cleanly on exit.
    set_kga_shortcut(&kga, &finish_id, &[]);
    set_kga_shortcut(&kga, &cancel_id, &[]);
    let _: Result<(), _> = kga.call("setInactive", &(&finish_id,));
    let _: Result<(), _> = kga.call("setInactive", &(&cancel_id,));
    let _: Result<(), _> = kga.call("setInactive", &(&hotkey_id,));
    drop(conn);
    if let Some(t) = signal_thread {
        let _ = t; // detaches if blocked on dbus socket read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_common_hotkeys_to_qt_keycodes() {
        let f8 = crate::config::parse_hotkey("F8").unwrap();
        assert_eq!(hotkey_to_qt_key(f8), Some(QT_KEY_F1 + 7));
        let combo = crate::config::parse_hotkey("Ctrl+Alt+J").unwrap();
        assert_eq!(
            hotkey_to_qt_key(combo),
            Some(b'J' as i32 | QT_MOD_CTRL | QT_MOD_ALT)
        );
        let space = crate::config::parse_hotkey("Super+Space").unwrap();
        assert_eq!(hotkey_to_qt_key(space), Some(0x20 | QT_MOD_META));
    }

    #[test]
    fn wait_for_wake_returns_immediately_after_wake_resident() {
        wake_resident();
        let start = std::time::Instant::now();
        wait_for_wake(Duration::from_secs(5));
        assert!(start.elapsed() < Duration::from_millis(200));
    }
}
