use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

/// Returns input device names suitable for storing in the user's settings.
/// A name is stable across reordering, but identical Windows device names
/// cannot be distinguished; the first matching input is used in that case.
pub fn input_devices() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };
    let mut names: Vec<_> = devices.filter_map(|device| device.name().ok()).collect();
    names.sort();
    names.dedup();
    names
}

pub fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32) as i16
}

/// 音の大きさ(RMS)。overlay の音量表示と segment の区切り判定で同じ計算を共有する
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

pub fn samples_to_pcm_le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        out.extend_from_slice(&f32_to_i16(*s).to_le_bytes());
    }
    out
}

pub struct Recorder {
    _stream: Option<cpal::Stream>,
    buffer: Arc<Mutex<CaptureBuffer>>,
    device_name: String,
    fallback_notice: Option<String>,
    pub sample_rate: u32,
    #[allow(dead_code)] // Available as device diagnostics; not read by the app yet.
    pub channels: u16,
}

struct CaptureBuffer {
    samples: Vec<f32>,
    limit: usize,
    error: Option<String>,
}

impl CaptureBuffer {
    fn new(sample_rate: u32) -> Self {
        Self {
            samples: Vec::with_capacity((sample_rate as usize / 10).max(1)),
            limit: (sample_rate as usize).max(1) * 2,
            error: None,
        }
    }

    fn fail(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(message);
        }
    }

    fn push(&mut self, sample: f32) -> bool {
        if self.samples.len() >= self.limit {
            self.fail(
                "録音処理が追いつかず、音声バッファの上限に達しました。録音をやり直してください"
                    .into(),
            );
            return false;
        }
        self.samples.push(sample);
        true
    }
}

impl Recorder {
    #[cfg(test)]
    pub(crate) fn fixture(sample_rate: u32, samples: Vec<f32>) -> Self {
        Self {
            _stream: None,
            buffer: Arc::new(Mutex::new(CaptureBuffer {
                samples,
                limit: sample_rate as usize * 2,
                error: None,
            })),
            device_name: "CI PCM fixture".into(),
            fallback_notice: None,
            sample_rate,
            channels: 1,
        }
    }

    #[allow(dead_code)] // Kept for standalone integrations that use the default device.
    pub fn start() -> Result<Recorder, String> {
        Self::start_with_device(None)
    }

    /// Starts recording from the saved device name, or the system default if
    /// that device was unplugged, cannot be opened, or no name was configured.
    pub fn start_with_device(name: Option<&str>) -> Result<Recorder, String> {
        let host = cpal::default_host();
        let requested = name.filter(|name| !name.is_empty());
        let chosen = requested.and_then(|name| {
            host.input_devices().ok()?.find(|device| {
                device
                    .name()
                    .as_deref()
                    .is_ok_and(|device_name| device_name == name)
            })
        });
        let fallback_reason = match chosen {
            Some(device) => match Self::start_device(device) {
                Ok(recorder) => return Ok(recorder),
                Err(error) => Some(error),
            },
            None => requested.map(|name| format!("指定したマイク「{name}」が見つかりません")),
        };
        let device = host
            .default_input_device()
            .ok_or_else(|| "既定のマイクが見つかりません".to_string())?;
        let mut recorder = Self::start_device(device)?;
        recorder.fallback_notice = fallback_reason.map(|reason| {
            format!(
                "{reason}。既定のマイク「{}」を使用しています",
                recorder.device_name
            )
        });
        Ok(recorder)
    }

    fn start_device(device: cpal::Device) -> Result<Recorder, String> {
        let device_name = device
            .name()
            .unwrap_or_else(|_| "名前を取得できないマイク".into());
        let config = device
            .default_input_config()
            .map_err(|e| format!("マイクの設定を取得できません: {e}"))?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels();
        let buffer = Arc::new(Mutex::new(CaptureBuffer::new(sample_rate)));
        let stream = build_stream(&device, &config, buffer.clone())?;
        stream
            .play()
            .map_err(|e| format!("録音を開始できません: {e}"))?;
        Ok(Recorder {
            _stream: Some(stream),
            buffer,
            device_name,
            fallback_notice: None,
            sample_rate,
            channels,
        })
    }

    pub fn drain(&self) -> Vec<f32> {
        let mut samples = Vec::new();
        self.drain_into(&mut samples);
        samples
    }

    /// 収録側と処理側のバッファを交換し、次のコールバックでも容量を再利用する。
    pub fn drain_into(&self, samples: &mut Vec<f32>) {
        drain_buffer(&self.buffer, samples);
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn fallback_notice(&self) -> Option<&str> {
        self.fallback_notice.as_deref()
    }

    pub fn take_error(&self) -> Option<String> {
        self.buffer
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .error
            .take()
    }
}

fn drain_buffer(buffer: &Mutex<CaptureBuffer>, samples: &mut Vec<f32>) {
    samples.clear();
    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
    std::mem::swap(&mut guard.samples, samples);
}

fn build_stream(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    buffer: Arc<Mutex<CaptureBuffer>>,
) -> Result<cpal::Stream, String> {
    let channels = config.channels() as usize;
    let stream_config: cpal::StreamConfig = config.clone().into();
    let errors = buffer.clone();
    let err_fn = move |e| {
        errors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .fail(format!("マイクの録音中にエラーが発生しました: {e}"));
    };
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

fn push_mono<I: Iterator<Item = f32>>(
    buffer: &Arc<Mutex<CaptureBuffer>>,
    samples: I,
    channels: usize,
) {
    let channels = channels.max(1);
    let mut frame_sum = 0f32;
    let mut count = 0usize;
    let mut guard = buffer.lock().unwrap_or_else(|p| p.into_inner());
    for s in samples {
        frame_sum += s;
        count += 1;
        if count == channels {
            if !guard.push(frame_sum / channels as f32) {
                return;
            }
            frame_sum = 0.0;
            count = 0;
        }
    }
    if count > 0 {
        guard.push(frame_sum / count as f32);
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

    #[test]
    fn stereo_frames_keep_order_across_reused_buffers() {
        let buffer = Arc::new(Mutex::new(CaptureBuffer::new(100)));
        let mut output = Vec::with_capacity(10);
        push_mono(&buffer, [0.2, 0.4, -0.5, 0.5].into_iter(), 2);
        drain_buffer(&buffer, &mut output);
        assert_eq!(output.len(), 2);
        assert!((output[0] - 0.3).abs() < 0.0001);
        assert_eq!(output[1], 0.0);
        push_mono(&buffer, [0.8, 0.8].into_iter(), 2);
        drain_buffer(&buffer, &mut output);
        assert_eq!(output, [0.8]);
        drain_buffer(&buffer, &mut output);
        assert!(output.is_empty());
    }

    #[test]
    fn an_overloaded_consumer_gets_a_bounded_buffer_and_an_error() {
        let buffer = Arc::new(Mutex::new(CaptureBuffer::new(4)));
        push_mono(&buffer, std::iter::repeat_n(0.5, 20), 1);
        let mut guard = buffer.lock().unwrap();
        assert_eq!(guard.samples, [0.5; 8]);
        assert!(guard.error.as_ref().unwrap().contains("上限"));
        let first = guard.error.clone();
        guard.fail("別のエラー".into());
        assert_eq!(guard.error, first);
    }
}
