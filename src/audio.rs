use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

pub fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32) as i16
}

pub fn samples_to_pcm_le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&f32_to_i16(*s).to_le_bytes());
    }
    out
}

pub struct Recorder {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<f32>>>,
    pub sample_rate: u32,
    #[allow(dead_code)] // Available as device diagnostics; not read by the app yet.
    pub channels: u16,
}

impl Recorder {
    pub fn start() -> Result<Recorder, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "既定のマイクが見つかりません".to_string())?;
        let config = device
            .default_input_config()
            .map_err(|e| format!("マイクの設定を取得できません: {e}"))?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
        let stream = build_stream(&device, &config, buffer.clone())?;
        stream
            .play()
            .map_err(|e| format!("録音を開始できません: {e}"))?;
        Ok(Recorder {
            _stream: stream,
            buffer,
            sample_rate,
            channels,
        })
    }

    pub fn drain(&self) -> Vec<f32> {
        let mut guard = match self.buffer.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        std::mem::take(&mut *guard)
    }
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    buffer: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, String> {
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.clone().into();
    let err_fn = |e| eprintln!("audio error: {e}");
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let buf = buffer.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    push_mono(&buf, data.iter().copied(), channels);
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let buf = buffer.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    push_mono(
                        &buf,
                        data.iter().map(|s| *s as f32 / i16::MAX as f32),
                        channels,
                    );
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let buf = buffer.clone();
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    push_mono(
                        &buf,
                        data.iter().map(|s| (*s as f32 - 32768.0) / 32768.0),
                        channels,
                    );
                },
                err_fn,
                None,
            )
        }
        other => return Err(format!("未対応のサンプル形式です: {other:?}")),
    };
    stream.map_err(|e| format!("録音ストリームを作れません: {e}"))
}

fn push_mono<I: Iterator<Item = f32>>(buffer: &Arc<Mutex<Vec<f32>>>, samples: I, channels: usize) {
    let channels = channels.max(1);
    let mut frame_sum = 0f32;
    let mut count = 0usize;
    let mut out = Vec::new();
    for s in samples {
        frame_sum += s;
        count += 1;
        if count == channels {
            out.push(frame_sum / channels as f32);
            frame_sum = 0.0;
            count = 0;
        }
    }
    if count > 0 {
        out.push(frame_sum / count as f32);
    }
    if let Ok(mut guard) = buffer.lock() {
        guard.extend(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversion_clamps() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -32767);
    }

    #[test]
    fn pcm_encoding_is_little_endian() {
        let bytes = samples_to_pcm_le(&[0.0, 1.0]);
        assert_eq!(bytes, vec![0, 0, 0xFF, 0x7F]);
    }
}
