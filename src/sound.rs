//! 録音開始、確定、貼り付け、エラーの効果音。earshot で作った 16bit モノラルの WAV を exe に埋め込んで鳴らす

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

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

/// 鳴らし終わらずに次を鳴らすと前の音は止まる。音量 0 なら鳴らさない
pub fn play(sound: Sound, volume: u8) {
    use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
    if volume == 0 {
        return;
    }
    let bytes = scaled(sound, volume.min(100));
    unsafe {
        let _ = PlaySoundW(
            windows::core::PCWSTR(bytes.as_ptr().cast()),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

/// 音の長さ。録音開始の音がマイクに入った分を捨てるのに使う
pub fn duration(sound: Sound) -> Duration {
    let w = wav(sound);
    match (data_range(w), fmt_rate(w)) {
        (Some(r), Some(rate)) => Duration::from_secs_f32(r.len() as f32 / 2.0 / rate as f32),
        _ => Duration::ZERO,
    }
}

/// 音量ごとに縮めた WAV を作って取っておく。非同期再生の間はバッファが生きている必要があるので、作ったものは手放さない
type SoundCache = Mutex<HashMap<(Sound, u8), &'static [u8]>>;

fn scaled(sound: Sound, volume: u8) -> &'static [u8] {
    static CACHE: OnceLock<SoundCache> = OnceLock::new();
    let mut cache = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    cache
        .entry((sound, volume))
        .or_insert_with(|| Box::leak(scale_wav(wav(sound), volume).into_boxed_slice()))
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
}
