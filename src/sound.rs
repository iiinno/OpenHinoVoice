//! 録音と入力の効果音。非同期再生中のPCMは、停止を確認するまで所有する。

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

const MAX_PCM_BYTES: usize = 256 * 1024;
const START_CAPTURE_MARGIN: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sound {
    Start,
    Stop,
    Done,
    Error,
}

fn wav(sound: Sound) -> &'static [u8] {
    match sound {
        Sound::Start => include_bytes!("../assets/sounds/start.wav"),
        Sound::Stop => include_bytes!("../assets/sounds/stop.wav"),
        Sound::Done => include_bytes!("../assets/sounds/done.wav"),
        Sound::Error => include_bytes!("../assets/sounds/error.wav"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaybackOwner {
    Production,
    Preview(u64),
}

struct Playing {
    owner: PlaybackOwner,
    _pcm: Box<[u8]>,
}

trait PlaybackBackend {
    fn play(&mut self, pcm: &[u8]) -> bool;
    fn stop(&mut self) -> bool;
}

struct WinMm;

impl PlaybackBackend for WinMm {
    fn play(&mut self, pcm: &[u8]) -> bool {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
        unsafe {
            PlaySoundW(
                windows::core::PCWSTR(pcm.as_ptr().cast()),
                None,
                SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
            )
            .as_bool()
        }
    }

    fn stop(&mut self) -> bool {
        use windows::Win32::Media::Audio::{PlaySoundW, SND_SYNC};
        unsafe { PlaySoundW(windows::core::PCWSTR::null(), None, SND_SYNC).as_bool() }
    }
}

struct Player<B> {
    backend: B,
    playing: Option<Playing>,
}

impl<B: PlaybackBackend> Player<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            playing: None,
        }
    }

    fn stop(&mut self) -> bool {
        if self.playing.is_none() {
            return true;
        }
        if !self.backend.stop() {
            return false;
        }
        self.playing = None;
        true
    }

    fn stop_preview(&mut self, token: u64) -> bool {
        if self.playing.as_ref().map(|p| p.owner) != Some(PlaybackOwner::Preview(token)) {
            return false;
        }
        self.stop()
    }

    fn play(&mut self, sound: Sound, volume: u8, owner: PlaybackOwner) -> bool {
        self.play_pcm(wav(sound), volume, owner)
    }

    fn play_pcm(&mut self, source: &[u8], volume: u8, owner: PlaybackOwner) -> bool {
        // PlaySoundの停止失敗時は旧バッファを残し、再利用も解放もしない。
        if !self.stop() || volume == 0 || source.len() > MAX_PCM_BYTES {
            return false;
        }
        let pcm = scale_wav(source, volume.min(100)).into_boxed_slice();
        if !self.backend.play(&pcm) {
            return false;
        }
        self.playing = Some(Playing { owner, _pcm: pcm });
        true
    }

    fn start_recording(&mut self, enabled: bool, volume: u8) -> Duration {
        let volume = if enabled { volume } else { 0 };
        if self.play(Sound::Start, volume, PlaybackOwner::Production) {
            duration(Sound::Start) + START_CAPTURE_MARGIN
        } else {
            Duration::ZERO
        }
    }
}

fn player() -> MutexGuard<'static, Player<WinMm>> {
    // staticの所有で、通常終了時もWinMMより先に再生中PCMを解放しない。
    static PLAYER: OnceLock<Mutex<Player<WinMm>>> = OnceLock::new();
    PLAYER
        .get_or_init(|| Mutex::new(Player::new(WinMm)))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

pub fn play(sound: Sound, volume: u8) {
    player().play(sound, volume, PlaybackOwner::Production);
}

/// 古い完了音や試聴を、マイク取得を始める前に止める。
pub fn stop() -> bool {
    player().stop()
}

/// 開始音を再生できた場合だけ、録音から除外する時間を返す。
pub fn start_recording(enabled: bool, volume: u8) -> Duration {
    player().start_recording(enabled, volume)
}

#[allow(dead_code)] // 設定からの試聴IPCを接続する入口。
pub fn play_preview(sound: Sound, volume: u8, token: u64) -> bool {
    player().play(sound, volume, PlaybackOwner::Preview(token))
}

#[allow(dead_code)] // 遅れた試聴停止は、その世代が再生中の場合だけ適用する。
pub fn stop_preview(token: u64) -> bool {
    player().stop_preview(token)
}

pub fn duration(sound: Sound) -> Duration {
    let w = wav(sound);
    match (data_range(w), fmt_rate(w)) {
        (Some(r), Some(rate)) if rate > 0 => {
            Duration::from_secs_f64(r.len() as f64 / 2.0 / rate as f64)
        }
        _ => Duration::ZERO,
    }
}

/// 16bit PCM の data 部分を音量で縮める。耳の感じ方に近づけるため音量の 2 乗を掛ける
fn scale_wav(src: &[u8], volume: u8) -> Vec<u8> {
    let mut out = src.to_vec();
    let gain = (volume as f32 / 100.0).powi(2);
    if let Some(range) = data_range(src) {
        for chunk in out[range].as_chunks_mut::<2>().0 {
            let s = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 * gain;
            chunk.copy_from_slice(&(s.round() as i16).to_le_bytes());
        }
    }
    out
}

fn chunks(w: &[u8]) -> impl Iterator<Item = (&[u8], std::ops::Range<usize>)> {
    let mut pos = 12;
    std::iter::from_fn(move || {
        let head = w.get(pos..pos + 8)?;
        let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
        let start = pos + 8;
        let end = (start + len).min(w.len());
        pos = start + len + (len & 1);
        Some((&head[0..4], start..end))
    })
}

fn data_range(w: &[u8]) -> Option<std::ops::Range<usize>> {
    chunks(w).find(|(id, _)| *id == b"data").map(|(_, r)| r)
}

fn fmt_rate(w: &[u8]) -> Option<u32> {
    let (_, r) = chunks(w).find(|(id, _)| *id == b"fmt ")?;
    let f = &w[r];
    Some(u32::from_le_bytes([
        *f.get(4)?,
        *f.get(5)?,
        *f.get(6)?,
        *f.get(7)?,
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_sounds_are_16bit_mono_pcm() {
        for s in [Sound::Start, Sound::Stop, Sound::Done, Sound::Error] {
            let w = wav(s);
            assert_eq!(&w[0..4], b"RIFF");
            let (_, fmt) = chunks(w).find(|(id, _)| *id == b"fmt ").expect("fmt chunk");
            let f = &w[fmt];
            assert_eq!(u16::from_le_bytes([f[0], f[1]]), 1, "{s:?} must be PCM");
            assert_eq!(u16::from_le_bytes([f[2], f[3]]), 1, "{s:?} must be mono");
            assert_eq!(
                u16::from_le_bytes([f[14], f[15]]),
                16,
                "{s:?} must be 16-bit"
            );
            let d = duration(s);
            assert!(
                d > Duration::from_millis(30) && d < Duration::from_millis(800),
                "{s:?}: {d:?}"
            );
        }
    }

    #[test]
    fn volume_scales_samples() {
        let w = wav(Sound::Done);
        let r = data_range(w).unwrap();
        let peak = |b: &[u8]| {
            b[r.clone()]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
                .max()
                .unwrap()
        };
        let full = peak(w);
        let half = peak(&scale_wav(w, 50));
        assert!(full > 1000);
        assert!(
            (half as f32 - full as f32 * 0.25).abs() <= 1.0,
            "{full} {half}"
        );
        assert_eq!(peak(&scale_wav(w, 0)), 0);
    }
    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Play { address: usize, bytes: usize },
        Stop,
    }

    #[derive(Default)]
    struct FakeBackend {
        calls: Vec<Call>,
        reject_play: bool,
        reject_stop: bool,
        playing: Option<(usize, usize)>,
    }

    impl PlaybackBackend for FakeBackend {
        fn play(&mut self, pcm: &[u8]) -> bool {
            let address = pcm.as_ptr() as usize;
            self.calls.push(Call::Play {
                address,
                bytes: pcm.len(),
            });
            if self.reject_play {
                return false;
            }
            self.playing = Some((address, pcm.len()));
            true
        }

        fn stop(&mut self) -> bool {
            self.calls.push(Call::Stop);
            if self.reject_stop {
                return false;
            }
            self.playing = None;
            true
        }
    }

    #[test]
    fn mute_recording_stops_previous_sound_without_capture_exclusion() {
        for (enabled, volume) in [(false, 100), (true, 0)] {
            let mut player = Player::new(FakeBackend::default());
            assert!(player.play(Sound::Done, 100, PlaybackOwner::Production));
            assert_eq!(player.start_recording(enabled, volume), Duration::ZERO);
            assert_eq!(player.backend.calls.len(), 2);
            assert_eq!(player.backend.calls[1], Call::Stop);
            assert!(player.backend.playing.is_none());
            assert!(player.playing.is_none());
        }
    }

    #[test]
    fn failed_start_playback_has_no_capture_exclusion_or_owned_pcm() {
        let mut player = Player::new(FakeBackend {
            reject_play: true,
            ..FakeBackend::default()
        });
        assert_eq!(player.start_recording(true, 80), Duration::ZERO);
        assert!(player.playing.is_none());
        assert!(player.backend.playing.is_none());
        player.backend.reject_play = false;
        assert_eq!(
            player.start_recording(true, 80),
            duration(Sound::Start) + Duration::from_millis(40)
        );
    }

    #[test]
    fn recording_stops_preview_before_start_and_ignores_delayed_preview_stop() {
        let mut player = Player::new(FakeBackend::default());
        assert!(player.play(Sound::Done, 75, PlaybackOwner::Preview(7)));
        assert!(!player.start_recording(true, 80).is_zero());
        assert!(matches!(player.backend.calls[0], Call::Play { .. }));
        assert_eq!(player.backend.calls[1], Call::Stop);
        assert!(matches!(player.backend.calls[2], Call::Play { .. }));
        for sound in [Sound::Start, Sound::Done, Sound::Error] {
            if sound != Sound::Start {
                assert!(player.play(sound, 80, PlaybackOwner::Production));
            }
            let count = player.backend.calls.len();
            let pcm = player.playing.as_ref().unwrap()._pcm.as_ptr();
            assert!(!player.stop_preview(7));
            assert_eq!(player.playing.as_ref().unwrap()._pcm.as_ptr(), pcm);
            assert_eq!(player.backend.calls.len(), count);
            assert_eq!(
                player.playing.as_ref().map(|p| p.owner),
                Some(PlaybackOwner::Production)
            );
            assert!(player.backend.playing.is_some());
        }
    }

    #[test]
    fn only_the_current_preview_token_can_stop_and_release_its_pcm() {
        let mut player = Player::new(FakeBackend::default());
        assert!(player.play(Sound::Start, 60, PlaybackOwner::Preview(10)));
        assert!(player.play(Sound::Done, 60, PlaybackOwner::Preview(11)));
        let playing = player.backend.playing;
        let count = player.backend.calls.len();
        assert!(!player.stop_preview(10));
        assert_eq!(player.backend.calls.len(), count);
        assert_eq!(player.backend.playing, playing);
        assert!(player.stop_preview(11));
        assert!(player.playing.is_none());
        assert!(player.backend.playing.is_none());
    }

    #[test]
    fn failed_stop_keeps_the_original_pcm_alive_and_rejects_replacement() {
        let mut player = Player::new(FakeBackend::default());
        assert!(player.play(Sound::Done, 100, PlaybackOwner::Preview(3)));
        let original = player.playing.as_ref().unwrap()._pcm.as_ptr();
        player.backend.reject_stop = true;
        assert!(!player.play(Sound::Error, 50, PlaybackOwner::Production));
        assert_eq!(player.playing.as_ref().unwrap()._pcm.as_ptr(), original);
        assert_eq!(
            player.playing.as_ref().unwrap().owner,
            PlaybackOwner::Preview(3)
        );
        assert_eq!(player.backend.calls.len(), 2);
        assert_eq!(player.backend.calls[1], Call::Stop);
        player.backend.reject_stop = false;
        assert!(player.stop_preview(3));
        assert!(player.playing.is_none());
    }

    #[test]
    fn repeated_sound_and_volume_changes_keep_only_one_bounded_pcm() {
        let mut player = Player::new(FakeBackend::default());
        for volume in 1..=100 {
            for sound in [Sound::Start, Sound::Stop, Sound::Done, Sound::Error] {
                assert!(player.play(sound, volume, PlaybackOwner::Production));
                let pcm = &player.playing.as_ref().unwrap()._pcm;
                assert_eq!(pcm.len(), wav(sound).len());
                assert!(pcm.len() <= MAX_PCM_BYTES);
                assert_eq!(
                    player.backend.playing,
                    Some((pcm.as_ptr() as usize, pcm.len()))
                );
            }
        }
        assert!(player.stop());
        assert!(player.playing.is_none());
        assert!(player.backend.playing.is_none());
        assert_eq!(player.backend.calls.len(), 800);
    }

    #[test]
    fn oversized_audio_is_not_allocated_or_started() {
        let mut player = Player::new(FakeBackend::default());
        let source = vec![0; MAX_PCM_BYTES + 1];
        assert!(!player.play_pcm(&source, 100, PlaybackOwner::Production));
        assert!(player.backend.calls.is_empty());
        assert!(player.playing.is_none());
    }
}
