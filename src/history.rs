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
    pub fn recognized(raw: String) -> Self {
        let id = new_id();
        Self {
            id,
            timestamp_unix_ms: id / 1_000_000,
            raw,
            formatted: None,
            paste_status: PasteStatus::Pending,
        }
    }

    /// Re-paste the result selected by the user; the original transcript is always retained.
    pub fn final_text(&self) -> &str {
        self.formatted.as_deref().unwrap_or(&self.raw)
    }
}

pub struct Checkpoint {
    pub entry: RecoveryEntry,
    pub revision: u64,
    pub create: bool,
    pub permission: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

pub struct CheckpointResult {
    pub id: u64,
    pub revision: u64,
    /// 保存後の整理だけが失敗しても、次回は既存項目の更新として扱う。
    pub created: bool,
    pub result: Result<(), String>,
}

/// 保存する内容の所有権だけを渡す。UI 側ではロック待ちやディスク I/O をしない。
pub struct RecoveryWriter {
    sender: std::sync::mpsc::SyncSender<Checkpoint>,
    receiver: std::sync::mpsc::Receiver<CheckpointResult>,
    enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl RecoveryWriter {
    pub fn start(notify: fn()) -> io::Result<Self> {
        let path = recovery_path();
        Self::with_sink(notify, move |checkpoint| {
            let id = checkpoint.entry.id;
            let revision = checkpoint.revision;
            let mut created = !checkpoint.create;
            let result = (|| {
                check_text_size(&checkpoint.entry.raw)?;
                if let Some(text) = &checkpoint.entry.formatted {
                    check_text_size(text)?;
                }
                let _guard = RecoveryLock::acquire()?;
                if !checkpoint.permission.load(Ordering::Acquire) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "履歴の保存を無効にしました",
                    ));
                }
                write_checkpoint_at(&path, &checkpoint, &mut created)
            })()
            .map_err(|error: io::Error| error.to_string());
            CheckpointResult {
                id,
                revision,
                created,
                result,
            }
        })
    }

    fn with_sink(
        notify: fn(),
        mut save: impl FnMut(Checkpoint) -> CheckpointResult + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, incoming) = std::sync::mpsc::sync_channel::<Checkpoint>(4);
        let (results, receiver) = std::sync::mpsc::channel();
        let enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let allowed = enabled.clone();
        std::thread::Builder::new()
            .name("recovery-history".into())
            .spawn(move || {
                while let Ok(checkpoint) = incoming.recv() {
                    let result = if allowed.load(Ordering::Acquire)
                        && checkpoint.permission.load(Ordering::Acquire)
                    {
                        save(checkpoint)
                    } else {
                        CheckpointResult {
                            id: checkpoint.entry.id,
                            revision: checkpoint.revision,
                            created: !checkpoint.create,
                            result: Err("履歴の保存を無効にしました".into()),
                        }
                    };
                    if results.send(result).is_err() {
                        break;
                    }
                    notify();
                }
            })?;
        Ok(Self {
            sender,
            receiver,
            enabled,
        })
    }

    pub fn try_submit(&self, checkpoint: Checkpoint) -> Result<(), Checkpoint> {
        self.sender
            .try_send(checkpoint)
            .map_err(|error| match error {
                std::sync::mpsc::TrySendError::Full(checkpoint)
                | std::sync::mpsc::TrySendError::Disconnected(checkpoint) => checkpoint,
            })
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn poll(&self) -> Option<CheckpointResult> {
        self.receiver.try_recv().ok()
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

/// ホームに出す入力の統計。復旧履歴から、今日と今週(7 日)の入力文字数を数える
pub struct HomeSnapshot {
    pub today_chars: usize,
    pub week_chars: usize,
    pub recent: Vec<RecoveryEntry>,
}

/// 一度だけ読み出した履歴から統計と直近の一覧を作る。
pub fn home_snapshot() -> io::Result<HomeSnapshot> {
    let entries = recent(MAX_RECOVERY_ENTRIES)?;
    let today_start = start_of_local_day_ms()?;
    Ok(snapshot_from_entries(entries, today_start))
}

fn snapshot_from_entries(mut entries: Vec<RecoveryEntry>, today_start: u64) -> HomeSnapshot {
    let week_start = today_start.saturating_sub(6 * 24 * 60 * 60 * 1000);
    let mut today_chars = 0;
    let mut week_chars = 0;
    for entry in &entries {
        let chars = entry.raw.chars().count();
        if entry.timestamp_unix_ms >= today_start {
            today_chars += chars;
        }
        if entry.timestamp_unix_ms >= week_start {
            week_chars += chars;
        }
    }
    entries.truncate(5);
    HomeSnapshot {
        today_chars,
        week_chars,
        recent: entries,
    }
}

fn start_of_local_day_ms() -> io::Result<u64> {
    let now = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    local_midnight_utc_ms(now, None)
}

fn local_midnight_utc_ms(
    date: windows::Win32::Foundation::SYSTEMTIME,
    timezone: Option<&windows::Win32::System::Time::TIME_ZONE_INFORMATION>,
) -> io::Result<u64> {
    use windows::Win32::Foundation::SYSTEMTIME;
    let midnight = SYSTEMTIME {
        wYear: date.wYear,
        wMonth: date.wMonth,
        wDay: date.wDay,
        ..SYSTEMTIME::default()
    };
    let mut utc = SYSTEMTIME::default();
    unsafe {
        windows::Win32::System::Time::TzSpecificLocalTimeToSystemTime(
            timezone.map(|tz| tz as *const _),
            &midnight,
            &mut utc,
        )
        .map_err(io::Error::other)?;
    }
    Ok(
        days_from_epoch(utc.wYear, utc.wMonth, utc.wDay) * 86_400_000
            + u64::from(utc.wHour) * 3_600_000
            + u64::from(utc.wMinute) * 60_000,
    )
}

/// 西暦からの経過日数(1970-01-01 を 0 とする)
fn days_from_epoch(year: u16, month: u16, day: u16) -> u64 {
    let (y, m) = if month <= 2 {
        (i64::from(year) - 1, i64::from(month) + 12)
    } else {
        (i64::from(year), i64::from(month))
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) as u64
}

/// Only the encrypted short-term recovery records are removed.
/// The user-controlled format-history.jsonl remains independent.
pub fn clear_recovery() -> io::Result<()> {
    let _lock = RecoveryLock::acquire()?;
    clear_at(&recovery_path())
}

/// 西暦からの経過日数の計算。既知の日付で確かめる
#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn local_midnight_has_the_correct_sign_in_east_and_west_timezones() {
        use windows::Win32::Foundation::SYSTEMTIME;
        use windows::Win32::System::Time::TIME_ZONE_INFORMATION;
        let date = SYSTEMTIME {
            wYear: 2026,
            wMonth: 11,
            wDay: 7,
            ..SYSTEMTIME::default()
        };
        let base = days_from_epoch(2026, 11, 7) * 86_400_000;
        for (bias, expected) in [(-540, base - 9 * 3_600_000), (480, base + 8 * 3_600_000)] {
            let timezone = TIME_ZONE_INFORMATION {
                Bias: bias,
                ..TIME_ZONE_INFORMATION::default()
            };
            assert_eq!(
                local_midnight_utc_ms(date, Some(&timezone)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn daylight_transition_uses_the_offset_at_midnight() {
        use windows::Win32::Foundation::SYSTEMTIME;
        use windows::Win32::System::Time::TIME_ZONE_INFORMATION;
        let timezone = TIME_ZONE_INFORMATION {
            Bias: 480,
            DaylightBias: -60,
            StandardDate: SYSTEMTIME {
                wMonth: 11,
                wDay: 1,
                wHour: 2,
                ..SYSTEMTIME::default()
            },
            DaylightDate: SYSTEMTIME {
                wMonth: 3,
                wDay: 2,
                wHour: 2,
                ..SYSTEMTIME::default()
            },
            ..TIME_ZONE_INFORMATION::default()
        };
        for (day, hours) in [(1, 7), (2, 8)] {
            let date = SYSTEMTIME {
                wYear: 2026,
                wMonth: 11,
                wDay: day,
                ..SYSTEMTIME::default()
            };
            let expected = days_from_epoch(2026, 11, day) * 86_400_000 + hours * 3_600_000;
            assert_eq!(
                local_midnight_utc_ms(date, Some(&timezone)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn days_from_epoch_matches_known_dates() {
        assert_eq!(days_from_epoch(1970, 1, 1), 0);
        assert_eq!(days_from_epoch(1970, 3, 1), 59); // うるう年でない 2 月は 28 日
        assert_eq!(days_from_epoch(2000, 3, 1), 11017); // 2000 年はうるう年
        assert_eq!(days_from_epoch(2026, 11, 7), 20764);
        // 4 年ごとのうるう年の境目
        assert_eq!(days_from_epoch(2024, 2, 29), 19782);
    }
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
        let name =
            windows::core::HSTRING::from(config::mutex_name("Local\\OpenHinoVoiceRecoveryHistory"));
        let handle = unsafe { CreateMutexW(None, false, &name) }.map_err(io::Error::other)?;
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

const CLEAR_MARKER: &str = "cleared-through";

// The reservation id is made before any queueing. Persist its deletion boundary
// before removing files, so even a delayed first checkpoint cannot revive them.
fn reject_cleared_reservation(dir: &Path, id: u64) -> io::Result<()> {
    let bytes = match fs::read(dir.join(CLEAR_MARKER)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let cutoff = u64::from_le_bytes(bytes.try_into().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "履歴削除の境界を読み取れません")
    })?);
    if id <= cutoff {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "この録音の履歴は削除されました",
        ))
    } else {
        Ok(())
    }
}

fn write_checkpoint_at(dir: &Path, checkpoint: &Checkpoint, created: &mut bool) -> io::Result<()> {
    let id = checkpoint.entry.id;
    reject_cleared_reservation(dir, id)?;
    let destination = entry_path(dir, id);
    if checkpoint.create {
        if destination.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "復旧履歴の識別子が重複しています",
            ));
        }
    } else {
        let saved = read_entry(&destination)?;
        if saved.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "復旧履歴の識別子が一致しません",
            ));
        }
    }
    write_entry(&destination, &checkpoint.entry)?;
    *created = true;
    prune_at(dir)
}

fn clear_at(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let files = entry_files(dir)?;
    let cutoff = files
        .iter()
        .filter_map(|path| path.file_stem()?.to_str()?.parse::<u64>().ok())
        .fold(new_id(), u64::max);
    let temporary = dir.join(format!(".recovery-clear-{}.tmp", new_id()));
    let write_marker = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&cutoff.to_le_bytes())?;
        file.sync_all()?;
        drop(file);
        atomic_replace(&temporary, &dir.join(CLEAR_MARKER))
    })();
    if write_marker.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_marker?;
    for path in files {
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
    fn clear_rejects_an_initial_checkpoint_that_was_already_queued() {
        let dir = test_dir();
        let permission = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let queued = Checkpoint {
            entry: RecoveryEntry::recognized("削除前の原文".into()),
            revision: 1,
            create: true,
            permission: permission.clone(),
        };
        clear_at(&dir).unwrap();
        let mut created = false;
        assert!(write_checkpoint_at(&dir, &queued, &mut created).is_err());
        assert!(!created);
        assert!(recent_at(&dir, 10).unwrap().is_empty());
        let next = Checkpoint {
            entry: RecoveryEntry::recognized("削除後の新しい録音".into()),
            revision: 1,
            create: true,
            permission,
        };
        write_checkpoint_at(&dir, &next, &mut created).unwrap();
        assert!(created);
        assert_eq!(recent_at(&dir, 10).unwrap().len(), 1);
        clear_at(&dir).unwrap();
        let delayed_update = Checkpoint {
            create: false,
            revision: 2,
            ..next
        };
        assert!(write_checkpoint_at(&dir, &delayed_update, &mut created).is_err());
        assert!(recent_at(&dir, 10).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn home_snapshot_counts_all_rows_before_limiting_recent_items() {
        let day = 24 * 60 * 60 * 1000;
        let today = day * 10;
        let entries = (0..8)
            .map(|index| RecoveryEntry {
                id: index + 1,
                timestamp_unix_ms: today - index * day,
                raw: "あ😀".into(),
                formatted: None,
                paste_status: PasteStatus::Pending,
            })
            .collect();
        let snapshot = snapshot_from_entries(entries, today);
        assert_eq!(snapshot.today_chars, 2);
        assert_eq!(snapshot.week_chars, 14);
        assert_eq!(snapshot.recent.len(), 5);
    }

    #[test]
    fn writer_keeps_revision_order_and_rejects_a_revoked_permission() {
        use std::sync::atomic::AtomicBool;
        use std::sync::{mpsc::channel, Arc};
        let (entered, started) = channel();
        let (release, resume) = channel();
        let writer = RecoveryWriter::with_sink(
            || {},
            move |checkpoint| {
                entered.send(checkpoint.revision).unwrap();
                resume.recv_timeout(Duration::from_secs(2)).unwrap();
                CheckpointResult {
                    id: checkpoint.entry.id,
                    revision: checkpoint.revision,
                    created: true,
                    result: Ok(()),
                }
            },
        )
        .unwrap();
        let permission = Arc::new(AtomicBool::new(true));
        let entry = RecoveryEntry::recognized("原文".into());
        assert!(writer
            .try_submit(Checkpoint {
                entry: entry.clone(),
                revision: 1,
                create: true,
                permission: permission.clone()
            })
            .is_ok());
        assert_eq!(started.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert!(writer
            .try_submit(Checkpoint {
                entry,
                revision: 2,
                create: false,
                permission: permission.clone()
            })
            .is_ok());
        permission.store(false, Ordering::Release);
        release.send(()).unwrap();
        let first = writer
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let second = writer
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(first.revision, 1);
        assert!(first.result.is_ok());
        assert_eq!(second.revision, 2);
        assert!(second.result.is_err());
        assert!(started.try_recv().is_err(), "禁止後の保存はsinkに届かない");
    }

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
