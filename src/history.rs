//! 復旧用の短期履歴（DPAPI 暗号化）と、利用者が opt-in した改善用の整形ログ。
use crate::config::{self, FormatConfig};
use crate::format::FormatError;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::core::w;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject};

const MAX_HISTORY_BYTES: u64 = 16 * 1024 * 1024;
/// 復旧履歴は整形履歴（改善用の opt-in）と独立し、短期保存する。
const MAX_RECOVERY_ENTRIES: usize = 200;
const MAX_RECOVERY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
// JSON can escape a one-byte control character as six ASCII bytes.
const MAX_RECOVERY_FILE_BYTES: u64 = 12 * MAX_TEXT_BYTES as u64 + 8192;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Serialize)]
struct Entry<'a> {
    timestamp_unix_ms: u128,
    provider: &'a str,
    model: &'a str,
    prompt: &'a str,
    raw: &'a str,
    formatted: Option<&'a str>,
    error: Option<String>,
    elapsed_ms: u128,
}

pub fn path() -> PathBuf {
    config::config_path().with_file_name("format-history.jsonl")
}

pub fn record(
    cfg: &FormatConfig,
    raw: &str,
    result: &Result<String, FormatError>,
    elapsed: Duration,
) -> io::Result<()> {
    if !cfg.save_history {
        return Ok(());
    }
    let model = match cfg.provider.as_str() {
        "chatgpt" => &cfg.chatgpt_model,
        "openai" => &cfg.openai_model,
        _ => &cfg.model,
    };
    let prompt = config::current_prompt(&cfg.prompt);
    let entry = Entry {
        timestamp_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        provider: &cfg.provider,
        model,
        prompt: &prompt,
        raw,
        formatted: result.as_ref().ok().map(String::as_str),
        error: result.as_ref().err().map(ToString::to_string),
        elapsed_ms: elapsed.as_millis(),
    };
    let mut bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
    bytes.push(b'\n');
    let lock = LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    append(&path(), &bytes)
}

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // データの増えすぎを防ぎ、直前の履歴を1世代だけ保存する。
    if fs::metadata(path)
        .map(|m| m.len().saturating_add(bytes.len() as u64) > MAX_HISTORY_BYTES)
        .unwrap_or(false)
    {
        let previous = path.with_extension("previous.jsonl");
        if previous.exists() {
            fs::remove_file(&previous)?;
        }
        fs::rename(path, previous)?;
    }
    OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(bytes)
}

/// A returned id identifies one recognized utterance, regardless of AI or paste outcome.
/// ShortcutSent means Windows accepted the keystrokes, not that the target consumed the text.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PasteStatus {
    #[default]
    Pending,
    ShortcutSent,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RecoveryEntry {
    pub id: u64,
    pub timestamp_unix_ms: u64,
    pub raw: String,
    pub formatted: Option<String>,
    #[serde(default)]
    pub paste_status: PasteStatus,
}

impl RecoveryEntry {
    /// Re-paste the result selected by the user; the original transcript is always retained.
    pub fn final_text(&self) -> &str {
        self.formatted.as_deref().unwrap_or(&self.raw)
    }
}

pub fn recovery_path() -> PathBuf {
    config::config_path().with_file_name("recovery-history")
}

/// Existing opt-in diagnostic history is never imported, deleted, or enabled here.
/// Call this at ASR success *before* starting formatting or attempting any paste.
pub fn save_recognized_if_enabled(raw: &str, enabled: bool) -> io::Result<Option<u64>> {
    if enabled {
        save_recognized(raw).map(Some)
    } else {
        Ok(None)
    }
}

pub fn save_recognized(raw: &str) -> io::Result<u64> {
    check_text_size(raw)?;
    let _lock = RecoveryLock::acquire()?;
    save_recognized_at(&recovery_path(), raw)
}

pub fn update_formatted(id: u64, formatted: &str) -> io::Result<()> {
    check_text_size(formatted)?;
    let _lock = RecoveryLock::acquire()?;
    update_at(&recovery_path(), id, |entry| {
        entry.formatted = Some(formatted.to_owned());
    })
}

pub fn update_paste_status(id: u64, status: PasteStatus) -> io::Result<()> {
    let _lock = RecoveryLock::acquire()?;
    update_at(&recovery_path(), id, |entry| {
        entry.paste_status = status;
    })
}

pub fn recent(limit: usize) -> io::Result<Vec<RecoveryEntry>> {
    let _lock = RecoveryLock::acquire()?;
    recent_at(&recovery_path(), limit)
}

/// Only the encrypted short-term recovery records are removed.
/// The user-controlled format-history.jsonl remains independent.
pub fn clear_recovery() -> io::Result<()> {
    let _lock = RecoveryLock::acquire()?;
    clear_at(&recovery_path())
}

fn check_text_size(value: &str) -> io::Result<()> {
    if value.len() > MAX_TEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "認識・整形テキストが復旧履歴の1件あたり1 MiBの上限を超えました",
        ));
    }
    Ok(())
}

/// The app and the independent settings process both access these files.
/// A named Windows mutex serializes updates, pruning and explicit clearing.
struct RecoveryLock(windows::Win32::Foundation::HANDLE);

impl RecoveryLock {
    fn acquire() -> io::Result<Self> {
        let handle =
            unsafe { CreateMutexW(None, false, w!("Local\\OpenHinoVoiceRecoveryHistory")) }
                .map_err(io::Error::other)?;
        let result = unsafe { WaitForSingleObject(handle, 5000) };
        // WAIT_OBJECT_0, WAIT_ABANDONED (the previous process exited).
        if result.0 != 0 && result.0 != 0x80 {
            let _ = unsafe { CloseHandle(handle) };
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "復旧履歴の排他ロックを取得できません",
            ));
        }
        Ok(Self(handle))
    }
}

impl Drop for RecoveryLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn new_id() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut next = now.as_nanos().min(u64::MAX as u128) as u64;
    loop {
        let previous = NEXT_ID.load(Ordering::Relaxed);
        next = next.max(previous.saturating_add(1));
        if NEXT_ID
            .compare_exchange(previous, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

fn entry_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:020}.bin"))
}

fn is_entry(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("bin"))
        && path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|stem| stem.len() == 20 && stem.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false)
}

fn entry_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if is_entry(&path) && entry.file_type()?.is_file() {
            files.push(path);
        }
    }
    files.sort_unstable();
    Ok(files)
}

fn save_recognized_at(dir: &Path, raw: &str) -> io::Result<u64> {
    fs::create_dir_all(dir)?;
    for _ in 0..10 {
        let id = new_id();
        let path = entry_path(dir, id);
        if path.exists() {
            continue;
        }
        let entry = RecoveryEntry {
            id,
            timestamp_unix_ms: id / 1_000_000,
            raw: raw.to_owned(),
            formatted: None,
            paste_status: PasteStatus::Pending,
        };
        write_entry(&path, &entry)?;
        prune_at(dir)?;
        return Ok(id);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "復旧履歴の識別子を確保できません",
    ))
}

fn update_at(dir: &Path, id: u64, update: impl FnOnce(&mut RecoveryEntry)) -> io::Result<()> {
    let path = entry_path(dir, id);
    let mut entry = read_entry(&path)?;
    if entry.id != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "復旧履歴の識別子が一致しません",
        ));
    }
    update(&mut entry);
    write_entry(&path, &entry)?;
    prune_at(dir)?;
    Ok(())
}

fn read_entry(path: &Path) -> io::Result<RecoveryEntry> {
    if fs::metadata(path)?.len() > MAX_RECOVERY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "復旧履歴ファイルが大きすぎます",
        ));
    }
    let sealed = fs::read(path)?;
    let plain = crate::chatgpt::dpapi::unprotect(&sealed).map_err(io::Error::other)?;
    let entry: RecoveryEntry = serde_json::from_slice(&plain).map_err(io::Error::other)?;
    check_text_size(&entry.raw)?;
    if let Some(formatted) = &entry.formatted {
        check_text_size(formatted)?;
    }
    Ok(entry)
}

fn write_entry(path: &Path, entry: &RecoveryEntry) -> io::Result<()> {
    let plain = serde_json::to_vec(entry).map_err(io::Error::other)?;
    let sealed = crate::chatgpt::dpapi::protect(&plain).map_err(io::Error::other)?;
    if sealed.len() as u64 > MAX_RECOVERY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "復旧履歴ファイルが大きすぎます",
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("復旧履歴の保存先がありません"))?;
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".recovery-{}-{}.tmp", std::process::id(), new_id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&sealed)?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    // REPLACE_EXISTING | WRITE_THROUGH. No truncate window when updating a saved transcript.
    let moved = unsafe {
        MoveFileExW(
            wide_path(source).as_ptr(),
            wide_path(destination).as_ptr(),
            0x1 | 0x8,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn prune_at(dir: &Path) -> io::Result<()> {
    // An interrupted atomic write can leave encrypted temporary files behind.
    // The named mutex ensures another live writer is not using them.
    for item in fs::read_dir(dir)? {
        let item = item?;
        let name = item.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".recovery-") && name.ends_with(".tmp") && item.file_type()?.is_file() {
            fs::remove_file(item.path())?;
        }
    }
    let files = entry_files(dir)?;
    let mut sizes = Vec::with_capacity(files.len());
    let mut total = 0_u64;
    for path in &files {
        let size = fs::metadata(path)?.len();
        total = total.saturating_add(size);
        sizes.push(size);
    }
    let mut count = files.len();
    for (path, size) in files.into_iter().zip(sizes) {
        if count <= MAX_RECOVERY_ENTRIES && total <= MAX_RECOVERY_BYTES {
            break;
        }
        fs::remove_file(path)?;
        count -= 1;
        total = total.saturating_sub(size);
    }
    Ok(())
}

fn recent_at(dir: &Path, limit: usize) -> io::Result<Vec<RecoveryEntry>> {
    let mut result = Vec::new();
    let mut unreadable = 0;
    for path in entry_files(dir)?
        .into_iter()
        .rev()
        .take(limit.min(MAX_RECOVERY_ENTRIES))
    {
        // A damaged entry cannot erase the remaining recoverable utterances.
        match read_entry(&path) {
            Ok(entry) => result.push(entry),
            Err(error) => {
                unreadable += 1;
                eprintln!("復旧履歴の一部を読めません（{}）: {error}", path.display());
            }
        }
    }
    // Do not claim "no recordings" when encrypted files are present but unreadable.
    if result.is_empty() && unreadable > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{unreadable} 件の復旧履歴を読み込めません（保存ファイルは保持しています）"),
        ));
    }
    Ok(result)
}

fn clear_at(dir: &Path) -> io::Result<()> {
    for path in entry_files(dir)? {
        fs::remove_file(path)?;
    }
    if dir.exists() {
        for item in fs::read_dir(dir)? {
            let item = item?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".recovery-")
                && name.ends_with(".tmp")
                && item.file_type()?.is_file()
            {
                fs::remove_file(item.path())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_valid_json_lines_and_rotates_at_limit() {
        let name = format!("openhinovoice-history-test-{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("history.jsonl");
        let one = serde_json::json!({"raw":"えーと 明日", "formatted":"明日"});
        append(&file, format!("{one}\n").as_bytes()).unwrap();
        let line = fs::read_to_string(&file).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(line.trim()).unwrap(),
            one
        );
        OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_len(MAX_HISTORY_BYTES)
            .unwrap();
        append(&file, b"next\n").unwrap();
        assert!(file.with_extension("previous.jsonl").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn history_is_disabled_by_default() {
        assert!(!FormatConfig::default().save_history);
    }

    fn test_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "openhinovoice-recovery-test-{}-{}",
            std::process::id(),
            new_id()
        ))
    }

    #[test]
    fn encrypted_recovery_round_trip_and_explicit_clear() {
        let dir = test_dir();
        let raw = "えーと 明日のミーティング";
        let formatted = "明日のミーティング";
        let id = save_recognized_at(&dir, raw).unwrap();
        let path = entry_path(&dir, id);
        // Recognition is durably stored before any AI result exists.
        let records = recent_at(&dir, 10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].raw, raw);
        assert_eq!(records[0].formatted, None);
        assert_eq!(records[0].paste_status, PasteStatus::Pending);
        assert_eq!(records[0].timestamp_unix_ms, id / 1_000_000);
        // Sensitive text must not occur as plaintext in the file.
        assert!(!fs::read(&path)
            .unwrap()
            .windows(raw.len())
            .any(|part| part == raw.as_bytes()));

        update_at(&dir, id, |entry| entry.formatted = Some(formatted.into())).unwrap();
        update_at(&dir, id, |entry| entry.paste_status = PasteStatus::Failed).unwrap();
        let entry = recent_at(&dir, 10).unwrap().remove(0);
        assert_eq!(entry.raw, raw);
        assert_eq!(entry.formatted.as_deref(), Some(formatted));
        assert_eq!(entry.final_text(), formatted);
        assert_eq!(entry.paste_status, PasteStatus::Failed);

        clear_at(&dir).unwrap();
        assert!(recent_at(&dir, 10).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recent_is_newest_first_and_prunes_excess_entries() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        for id in 1..=MAX_RECOVERY_ENTRIES as u64 + 5 {
            // The prune test needs only filenames and sizes, not expensive encryption.
            fs::write(entry_path(&dir, id), [0_u8; 8]).unwrap();
        }
        prune_at(&dir).unwrap();
        let paths = entry_files(&dir).unwrap();
        assert_eq!(paths.len(), MAX_RECOVERY_ENTRIES);
        assert!(!entry_path(&dir, 1).exists());
        assert!(entry_path(&dir, MAX_RECOVERY_ENTRIES as u64 + 5).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opted_out_recovery_writes_nothing() {
        // The explicit opt-out must not create directories or touch any existing records.
        assert_eq!(
            save_recognized_if_enabled("この音声は保存しない", false).unwrap(),
            None
        );
        assert!(check_text_size(&"x".repeat(MAX_TEXT_BYTES)).is_ok());
        assert!(check_text_size(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }

    #[test]
    fn missing_or_corrupted_entry_does_not_recreate_content() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            update_at(&dir, 123, |entry| entry.formatted = Some("x".into()))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        fs::write(entry_path(&dir, 456), "bad ciphertext").unwrap();
        assert!(recent_at(&dir, 10).is_err());
        let id = save_recognized_at(&dir, "無事な録音").unwrap();
        assert_eq!(recent_at(&dir, 10).unwrap()[0].id, id);
        fs::remove_dir_all(dir).unwrap();
    }
}
