//! 未保存の原文と保存結果を所有する。保存待ちで入力のループを止めない。

use crate::history::{Checkpoint, CheckpointResult, PasteStatus, RecoveryEntry, RecoveryWriter};
use std::collections::VecDeque;

const MAX_RECORDS: usize = 16;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const RECORD_RESERVE: usize = 1024 * 1024;

struct Record {
    entry: RecoveryEntry,
    revision: u64,
    saved_revision: u64,
    writing: Option<u64>,
    created: bool,
    enabled: bool,
    permission: std::sync::Arc<std::sync::atomic::AtomicBool>,
    complete: bool,
    paste_failed: bool,
    failure: Option<String>,
}

#[derive(Default)]
pub struct Recoveries {
    records: VecDeque<Record>,
    writer: Option<RecoveryWriter>,
}

impl Recoveries {
    pub fn reserve(&mut self, enabled: bool) -> Result<u64, String> {
        let bytes: usize = self
            .records
            .iter()
            .map(|r| r.entry.raw.len() + r.entry.formatted.as_ref().map_or(0, String::len))
            .sum();
        if self.records.len() >= MAX_RECORDS
            || bytes.saturating_add(2 * RECORD_RESERVE) > MAX_BYTES
            || self
                .records
                .iter()
                .any(|record| record.entry.raw.len() > RECORD_RESERVE)
        {
            return Err(
                "未回収の文章が増えています。トレイからコピーしてから録音してください".into(),
            );
        }
        let entry = RecoveryEntry::recognized(String::new());
        let id = entry.id;
        self.records.push_back(Record {
            entry,
            revision: 0,
            saved_revision: 0,
            writing: None,
            created: false,
            enabled,
            permission: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(enabled)),
            complete: false,
            paste_failed: false,
            failure: None,
        });
        Ok(id)
    }

    pub fn update_raw(&mut self, id: u64, raw: String) {
        if let Some(record) = self.get_mut(id) {
            if record.entry.raw != raw {
                record.entry.raw = raw;
                record.revision += 1;
                // 現在の原文は捨てない。保存上限を超えた場合は回収まで次の受付を止める。
                if record.entry.raw.len() > RECORD_RESERVE {
                    record.failure = Some("原文が履歴の保存上限を超えました".into());
                }
            }
        }
    }

    pub fn set_formatted(&mut self, id: u64, text: String) {
        if let Some(record) = self.get_mut(id) {
            if record.entry.formatted.as_ref() != Some(&text) {
                record.entry.formatted = Some(text);
                record.revision += 1;
            }
        }
    }

    pub fn mark_paste(&mut self, id: u64, success: bool) {
        if let Some(record) = self.get_mut(id) {
            record.entry.paste_status = if success {
                PasteStatus::ShortcutSent
            } else {
                PasteStatus::Failed
            };
            record.paste_failed = !success;
            record.complete = true;
            record.revision += 1;
        }
        self.collect_saved();
    }

    pub fn abandon(&mut self, id: u64) {
        if let Some(record) = self.get_mut(id) {
            record.complete = true;
            record.paste_failed = !record.entry.raw.is_empty();
        }
        self.records
            .retain(|record| record.entry.id != id || !record.entry.raw.is_empty());
    }

    pub fn mark_volatile(&mut self, id: u64, message: &str) {
        if let Some(record) = self.get_mut(id) {
            record.failure = Some(message.into());
        }
    }

    /// 保存を無効にした瞬間以降に、新しい保存要求を出さない。
    pub fn disable(&mut self) {
        for record in &mut self.records {
            record.enabled = false;
            record
                .permission
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if let Some(writer) = &self.writer {
            writer.set_enabled(false);
        }
    }

    pub fn ready(&self, id: u64) -> bool {
        self.records
            .iter()
            .find(|record| record.entry.id == id)
            .is_none_or(|record| {
                !record.enabled
                    || record.saved_revision >= record.revision
                    || record.failure.is_some()
            })
    }

    pub fn volatile(&self, id: u64) -> bool {
        self.records
            .iter()
            .any(|record| record.entry.id == id && record.failure.is_some())
    }

    pub fn expire_pending(&mut self) {
        for record in &mut self.records {
            if record.saved_revision < record.revision {
                record.failure = Some("保存の完了を確認できませんでした".into());
                record.complete = true;
            }
        }
    }

    pub fn has_unfinished_writes(&self) -> bool {
        self.records.iter().any(|record| {
            record.writing.is_some()
                || (record.enabled
                    && !record.entry.raw.is_empty()
                    && record.failure.is_none()
                    && record.saved_revision < record.revision)
        })
    }

    pub fn tick(&mut self, notify: fn()) {
        while let Some(result) = self.writer.as_ref().and_then(RecoveryWriter::poll) {
            self.acknowledge(result);
        }
        self.collect_saved();
        if self.records.is_empty() {
            self.writer = None;
            return;
        }
        let needs_writer = self.records.iter().any(Self::needs_write);
        if !needs_writer {
            return;
        }
        if self.writer.is_none() {
            match RecoveryWriter::start(notify) {
                Ok(writer) => self.writer = Some(writer),
                Err(error) => {
                    for record in &mut self.records {
                        if Self::needs_write(record) {
                            record.failure = Some(error.to_string());
                        }
                    }
                    return;
                }
            }
        }
        let writer = self.writer.as_ref().unwrap();
        writer.set_enabled(true);
        for record in &mut self.records {
            if !Self::needs_write(record) {
                continue;
            }
            let checkpoint = Checkpoint {
                entry: record.entry.clone(),
                revision: record.revision,
                create: !record.created,
                permission: record.permission.clone(),
            };
            if writer.try_submit(checkpoint).is_ok() {
                record.writing = Some(record.revision);
            }
        }
    }

    fn needs_write(record: &Record) -> bool {
        record.enabled
            && !record.entry.raw.is_empty()
            && record.writing.is_none()
            && record.saved_revision < record.revision
            && record.failure.is_none()
    }

    fn acknowledge(&mut self, result: CheckpointResult) {
        if let Some(record) = self.get_mut(result.id) {
            if record.writing != Some(result.revision) {
                return;
            }
            record.writing = None;
            record.created |= result.created;
            match result.result {
                Ok(()) => {
                    record.saved_revision = record.saved_revision.max(result.revision);
                    if result.revision == record.revision {
                        record.failure = None;
                    }
                }
                Err(error) => {
                    if record.enabled {
                        record.failure = Some(error);
                    }
                }
            }
        }
    }

    fn collect_saved(&mut self) {
        self.records.retain(|record| {
            !record.complete
                || record.paste_failed
                || record.failure.is_some()
                || (record.enabled && record.saved_revision < record.revision)
        });
    }

    pub fn count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| Self::recoverable(record))
            .count()
    }

    pub fn first_text(&self) -> Option<(u64, &str)> {
        let record = self
            .records
            .iter()
            .find(|record| Self::recoverable(record))?;
        let text = if record.paste_failed {
            record.entry.final_text()
        } else {
            &record.entry.raw
        };
        Some((record.entry.id, text))
    }

    pub fn copied(&mut self, id: u64) {
        self.records.retain(|record| record.entry.id != id);
    }

    fn recoverable(record: &Record) -> bool {
        record.complete
            && !record.entry.raw.is_empty()
            && (record.paste_failed || record.failure.is_some())
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Record> {
        self.records.iter_mut().find(|record| record.entry.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_ack_never_releases_a_newer_transcript() {
        let mut store = Recoveries::default();
        let id = store.reserve(true).unwrap();
        store.update_raw(id, "前半".into());
        store.get_mut(id).unwrap().writing = Some(1);
        store.update_raw(id, "前半と後半".into());
        store.acknowledge(CheckpointResult {
            id,
            revision: 1,
            created: true,
            result: Ok(()),
        });
        assert!(!store.ready(id));
        assert_eq!(store.get_mut(id).unwrap().entry.raw, "前半と後半");
    }

    #[test]
    fn a_save_failure_retains_the_original_after_successful_input() {
        let mut store = Recoveries::default();
        let id = store.reserve(true).unwrap();
        store.update_raw(id, "原文".into());
        store.set_formatted(id, "整形した文".into());
        store.mark_volatile(id, "ディスクが使用できません");
        store.mark_paste(id, true);
        assert_eq!(store.count(), 1);
        assert_eq!(store.first_text(), Some((id, "原文")));
        store.copied(id);
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn failed_input_keeps_the_final_text_and_full_queue_rejects_new_recording() {
        let mut store = Recoveries::default();
        for _ in 0..MAX_RECORDS {
            let id = store.reserve(false).unwrap();
            store.update_raw(id, "原文".into());
            store.set_formatted(id, "再入力する文".into());
            store.mark_paste(id, false);
        }
        assert!(store.reserve(false).is_err());
        assert_eq!(store.first_text().unwrap().1, "再入力する文");
        let id = store.first_text().unwrap().0;
        store.copied(id);
        assert!(store.reserve(false).is_ok());
    }

    #[test]
    fn opt_out_does_not_need_disk_and_success_clears_the_memory_copy() {
        let mut store = Recoveries::default();
        let id = store.reserve(false).unwrap();
        store.update_raw(id, "ローカル".into());
        assert!(store.ready(id));
        store.mark_paste(id, true);
        assert!(store.records.is_empty());
        assert!(store.writer.is_none());
    }
}
