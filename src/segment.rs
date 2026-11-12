//! 録音中の音声を発話ごとの区間に分ける。
//! FastConformer は複数の文を含む長い音声を 1 リクエストで渡すと文を丸ごと
//! 落とすことがあるため、アプリ側で無音を頼りに区切って、区間ごとに認識させる。
//! 区切りの数値(無音 600 ミリ秒、上限 20 秒など)は仮の値で、P0 の測定で調整する。

/// 区切りの判定に使う数値。すべてミリ秒か音量(RMS)
#[derive(Debug, Clone)]
pub struct SegmentParams {
    /// 枠の長さ
    pub frame_ms: u32,
    /// 録音の冒頭、背景雑音を学習してから区間を開くまでの長さ
    pub calibration_ms: u32,
    /// この長さの無音が続いたら区間を閉じる
    pub silence_close_ms: u32,
    /// 区間の上限。超えそうなときは静かな位置で切る
    pub max_interval_ms: u32,
    /// 上限で切るときに、直近この範囲から最も静かな枠を探す
    pub cut_search_ms: u32,
    /// 区間を閉じるとき、最後の発話の後に残す音
    pub tail_ms: u32,
    /// 学習した背景雑音のこの倍より大きい枠を発話とみなす
    pub speech_ratio: f32,
    /// 背景雑音が小さい部屋でも、これより小さい枠は無音とみなす
    pub min_speech_rms: f32,
    /// この音量より大きい枠からは背景雑音を学習しない。咳や話し声で雑音の見当が上がるのを防ぐ
    pub max_learn_rms: f32,
    /// この数の連続した発話枠があって初めて区間を開く。1 枠のクリックを拾わないため
    pub min_speech_frames: u32,
}

impl Default for SegmentParams {
    fn default() -> Self {
        SegmentParams {
            frame_ms: 20,
            calibration_ms: 200,
            silence_close_ms: 600,
            max_interval_ms: 20_000,
            cut_search_ms: 5_000,
            tail_ms: 200,
            speech_ratio: 3.0,
            min_speech_rms: 0.006,
            max_learn_rms: 0.02,
            min_speech_frames: 3,
        }
    }
}

#[derive(Debug)]
pub struct Segmenter {
    params: SegmentParams,
    sample_rate: u32,
    /// 枠に満たない端数の音
    remainder: Vec<f32>,
    /// 無音の枠から少しずつ学習する背景雑音の大きさ
    noise_floor: f32,
    /// 録音の開始から処理した枠の数
    frames_seen: u32,
    /// 開いている区間の音。閉じると区切りとして返し、空にする
    open: Vec<f32>,
    /// open の中で、最後に発話だった枠の直後の位置(サンプル数)
    speech_end: usize,
    /// 区間の末尾で無音の枠が続いている数
    silence_run: u32,
    /// 発話の連続を数える枠数。区間を開く判定に使う
    speech_run: u32,
    /// 区間を開く前の発話枠をためておく先読み
    lookahead: Vec<Vec<f32>>,
    /// 開いている区間の直近の枠の音量。上限で切る位置を探すのに使う
    open_frame_rms: Vec<f32>,
}

impl Segmenter {
    pub fn new(sample_rate: u32) -> Segmenter {
        Segmenter::with_params(sample_rate, SegmentParams::default())
    }

    pub fn with_params(sample_rate: u32, params: SegmentParams) -> Segmenter {
        Segmenter {
            params,
            sample_rate,
            remainder: Vec::new(),
            noise_floor: 0.002,
            frames_seen: 0,
            open: Vec::new(),
            speech_end: 0,
            silence_run: 0,
            speech_run: 0,
            lookahead: Vec::new(),
            open_frame_rms: Vec::new(),
        }
    }

    fn frame_samples(&self) -> usize {
        (self.sample_rate as usize * self.params.frame_ms as usize) / 1000
    }

    fn threshold(&self) -> f32 {
        (self.noise_floor * self.params.speech_ratio).max(self.params.min_speech_rms)
    }

    /// 録音の音を渡し、閉じた区間を音の並びで返す
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        let mut closed = Vec::new();
        let mut samples_with_remainder = std::mem::take(&mut self.remainder);
        samples_with_remainder.extend_from_slice(samples);
        let mut frames = samples_with_remainder.chunks_exact(self.frame_samples());
        for frame in &mut frames {
            if let Some(interval) = self.process_frame(frame.to_vec()) {
                closed.push(interval);
            }
        }
        self.remainder.extend_from_slice(frames.remainder());
        closed
    }

    /// Enter のあとに呼ぶ。開いている区間を最後まで返す
    pub fn finish(&mut self) -> Option<Vec<f32>> {
        if self.open.is_empty() && self.speech_run >= self.params.min_speech_frames {
            for frame in std::mem::take(&mut self.lookahead) {
                self.open.extend(frame);
            }
        }
        let remainder = std::mem::take(&mut self.remainder);
        if !self.open.is_empty() {
            self.open.extend(remainder);
        }
        self.lookahead.clear();
        self.speech_run = 0;
        if self.open.is_empty() {
            return None;
        }
        let interval = std::mem::take(&mut self.open);
        self.speech_end = 0;
        self.silence_run = 0;
        self.speech_run = 0;
        self.open_frame_rms.clear();
        Some(interval)
    }

    /// 開いている区間の音。話している途中の仮の認識に使う
    pub fn open_samples(&self) -> &[f32] {
        &self.open
    }

    #[cfg(test)]
    pub fn is_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// 背景雑音の学習値。診断のために残す
    #[cfg(test)]
    pub fn noise_floor(&self) -> f32 {
        self.noise_floor
    }

    fn process_frame(&mut self, frame: Vec<f32>) -> Option<Vec<f32>> {
        self.frames_seen += 1;
        let level = crate::audio::rms(&frame);
        let speaking = level >= self.threshold();
        let calibrating = self.frames_seen * self.params.frame_ms <= self.params.calibration_ms;
        // 無音の枠から背景雑音を少しずつ学んで追従させる。咳や話し声のような大きい音は学ばない。
        // キャリブレーションの間はmin_speech_rms を超えて学ばない。話し声を雑音と誤って学ぶのを防ぐ
        if level < self.params.max_learn_rms {
            let target = if calibrating {
                level.min(self.params.min_speech_rms)
            } else if level < self.threshold() {
                level
            } else {
                self.noise_floor
            };
            self.noise_floor += (target - self.noise_floor) * 0.05;
        }
        if self.open.is_empty() {
            if calibrating {
                if speaking {
                    self.speech_run += 1;
                    self.lookahead.push(frame);
                } else {
                    self.speech_run = 0;
                    self.lookahead.clear();
                }
                return None;
            }
            return self.detect_start(frame, speaking);
        }
        self.grow_interval(frame, level, speaking)
    }

    fn detect_start(&mut self, frame: Vec<f32>, speaking: bool) -> Option<Vec<f32>> {
        if !speaking {
            self.speech_run = 0;
            self.lookahead.clear();
            return None;
        }
        self.lookahead.push(frame);
        self.speech_run += 1;
        if self.speech_run < self.params.min_speech_frames {
            return None;
        }
        // 先読みにためた発話の枠から区間を開く
        for buffered in std::mem::take(&mut self.lookahead) {
            self.open_frame_rms.push(crate::audio::rms(&buffered));
            self.speech_end += buffered.len();
            self.open.extend(buffered);
        }
        self.speech_run = 0;
        self.silence_run = 0;
        None
    }

    fn grow_interval(&mut self, frame: Vec<f32>, level: f32, speaking: bool) -> Option<Vec<f32>> {
        self.open.extend_from_slice(&frame);
        self.open_frame_rms.push(level);
        let window_frames = (self.params.cut_search_ms / self.params.frame_ms) as usize;
        if self.open_frame_rms.len() > window_frames {
            self.open_frame_rms.remove(0);
        }
        // 上限の判定を発話の判定より先に行う。話し続けている間も切れるようにするため
        if self.open.len() as u64 * 1000
            >= self.params.max_interval_ms as u64 * self.sample_rate as u64
        {
            return Some(self.cut_at_quietest());
        }
        if speaking {
            self.speech_end = self.open.len();
            self.silence_run = 0;
            return None;
        }
        self.silence_run += 1;
        if self.silence_run * self.params.frame_ms >= self.params.silence_close_ms {
            return Some(self.close_after_silence());
        }
        None
    }

    /// 区間を閉じる。最後の発話の後に少しの音を残し、それ以降の無音は捨てる
    fn close_after_silence(&mut self) -> Vec<f32> {
        let tail = self.sample_rate as usize * self.params.tail_ms as usize / 1000;
        let end = (self.speech_end + tail).min(self.open.len());
        let interval = self.open.drain(..end).collect();
        self.open.clear();
        self.speech_end = 0;
        self.silence_run = 0;
        self.speech_run = 0;
        self.open_frame_rms.clear();
        interval
    }

    /// 上限に達したとき、直近の範囲で最も静かな枠の直後で切る。
    /// 同じ音量の枠が並ぶときはいちばん後ろを選ぶ。無音の部分を前の区間に残すため。
    /// 切った後ろは次の区間の始まりになる(話し続けているので発話判定をやり直さない)
    fn cut_at_quietest(&mut self) -> Vec<f32> {
        let frame_samples = self.frame_samples();
        let mut quietest = 0;
        for (i, level) in self.open_frame_rms.iter().enumerate() {
            if *level <= self.open_frame_rms[quietest] {
                quietest = i;
            }
        }
        // open_frame_rms は直近の分だけ持つので、末尾からの位置で切る場所を出す
        let keep_frames = self.open_frame_rms.len() - quietest - 1;
        let cut = self.open.len().saturating_sub(keep_frames * frame_samples);
        let cut = cut.clamp(frame_samples, self.open.len() - frame_samples);
        let rest = self.open.split_off(cut);
        let interval = std::mem::replace(&mut self.open, rest);
        self.open_frame_rms = self.open_frame_rms.split_off(quietest + 1);
        self.speech_end = self.open.len();
        self.silence_run = 0;
        interval
    }
}

/// 区間ごとの認識結果を順につなぐ。Haas が句読点を出すかどうかは P0 の測定で
/// 確かめる。出ない場合も日本語は間に空白を挟まずつないで読める
pub fn join_intervals(texts: &[String]) -> String {
    let mut joined = String::new();
    for text in texts {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            joined.push_str(trimmed);
        }
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16000;

    /// 大きさ level の正弦波。話し声の代わり
    fn tone(seconds: f64, level: f32) -> Vec<f32> {
        let n = (RATE as f64 * seconds) as usize;
        (0..n).map(|i| (i as f32 * 0.05).sin() * level).collect()
    }

    fn silence(seconds: f64) -> Vec<f32> {
        vec![0.0; (RATE as f64 * seconds) as usize]
    }

    fn rms_of(samples: &[f32]) -> f32 {
        crate::audio::rms(samples)
    }

    /// 正弦波の RMS は振幅の 1/√2
    #[test]
    fn sine_rms_matches_amplitude_over_root_two() {
        let samples = tone(0.1, 0.1);
        let got = rms_of(&samples);
        let expected = 0.1 / std::f32::consts::SQRT_2;
        assert!((got - expected).abs() < 0.002, "{got} vs {expected}");
    }

    #[test]
    fn speech_from_the_start_and_the_last_incomplete_frame_are_preserved() {
        let mut segmenter = Segmenter::new(RATE);
        let samples = vec![0.3; RATE as usize + 37];
        assert!(segmenter.push(&samples).is_empty());
        assert_eq!(segmenter.finish().unwrap(), samples);
    }

    #[test]
    fn equally_quiet_frames_choose_the_latest_cut() {
        let params = SegmentParams {
            calibration_ms: 0,
            max_interval_ms: 1000,
            cut_search_ms: 500,
            ..SegmentParams::default()
        };
        let mut segmenter = Segmenter::with_params(RATE, params);
        let samples = vec![0.3; RATE as usize];
        let closed = segmenter.push(&samples);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].len(), RATE as usize - 320);
        assert_eq!(
            closed[0].len() + segmenter.finish().unwrap().len(),
            samples.len()
        );
    }

    #[test]
    fn silence_alone_produces_no_interval() {
        let mut s = Segmenter::new(RATE);
        assert!(s.push(&silence(3.0)).is_empty());
        assert!(s.finish().is_none());
    }

    #[test]
    fn two_utterances_separated_by_silence_close_into_two_intervals() {
        let mut s = Segmenter::new(RATE);
        let mut audio = Vec::new();
        audio.extend(tone(1.0, 0.1));
        audio.extend(silence(1.0));
        audio.extend(tone(1.0, 0.1));
        audio.extend(silence(0.8));
        let closed = s.push(&audio);
        assert_eq!(closed.len(), 2, "無音で 2 つに分かれる");
        // どちらも発話と尻尾を残す
        for interval in &closed {
            let seconds = interval.len() as f64 / RATE as f64;
            assert!(
                (0.8..=1.35).contains(&seconds),
                "区間の長さは発話+尻尾のはず: {seconds}"
            );
        }
        assert!(s.finish().is_none(), "すべての区間はすでに閉じている");
    }

    #[test]
    fn single_frame_click_does_not_open_an_interval() {
        let mut s = Segmenter::new(RATE);
        let mut audio = silence(0.5);
        audio.extend(tone(0.025, 0.3)); // 1 枠分のクリック
        audio.extend(silence(0.5));
        assert!(s.push(&audio).is_empty());
        assert!(s.finish().is_none());
    }

    #[test]
    fn speech_during_calibration_is_retained_until_finish() {
        let mut s = Segmenter::new(RATE);
        let samples = tone(0.15, 0.1);
        assert!(s.push(&samples).is_empty());
        assert_eq!(s.finish().unwrap(), samples);
    }

    #[test]
    fn background_noise_is_learned_and_does_not_open_intervals() {
        let mut s = Segmenter::new(RATE);
        // 雑音の RMS は約 0.01。min_speech_rms(0.006)より大きいので、学習しなければ発話になる
        let noise: Vec<f32> = (0..RATE as usize * 3)
            .map(|i| (i as f32 * 0.013).sin() * 0.0141)
            .collect();
        assert!(
            rms_of(&noise) > 0.008,
            "テストの雑音が発話しきい値を超えていること: {}",
            rms_of(&noise)
        );
        assert!(s.push(&noise).is_empty(), "雑音だけでは区間を開かない");
        let learned = s.noise_floor();
        // キャリブレーションでは min_speech_rms(0.006)まで、その後は実際の雑音(約 0.01)まで学ぶ
        assert!(learned > 0.008, "無音の枠から雑音を学習すること: {learned}");
        // 雑音の上に重ねた話し声でも区間を開く
        let mut with_speech = noise;
        for (i, sample) in tone(0.5, 0.05).into_iter().enumerate() {
            with_speech[i + RATE as usize * 2] += sample;
        }
        with_speech.extend(silence(0.8));
        let closed = s.push(&with_speech);
        assert_eq!(closed.len(), 1, "雑音に慣れたあとは話し声を拾う");
        let interval = closed.into_iter().next().unwrap();
        assert!((interval.len() as f64 / RATE as f64) > 0.4);
    }

    #[test]
    fn loud_calibration_frames_do_not_raise_the_noise_floor() {
        let mut s = Segmenter::new(RATE);
        // 録音の冒頭からいきなり話し始めても、話し声を雑音として学ばない
        let closed = s.push(&tone(2.0, 0.1));
        assert!(closed.is_empty());
        let interval = s.finish().expect("話し声で区間が開く");
        assert!((interval.len() as f64 / RATE as f64) > 1.0);
        assert!(s.noise_floor() < 0.005, "{}", s.noise_floor());
    }

    #[test]
    fn long_speech_is_cut_at_the_quietest_frame_of_the_recent_window() {
        let params = SegmentParams {
            max_interval_ms: 10_000,
            cut_search_ms: 5_000,
            ..SegmentParams::default()
        };
        let mut s = Segmenter::with_params(RATE, params);
        // 12 秒の連続音。8〜9 秒目だけ音量を下げる(上限 10 秒時点の直近 5 秒内で最も静か)
        let mut audio = tone(8.0, 0.1);
        audio.extend(tone(1.0, 0.02));
        audio.extend(tone(3.0, 0.1));
        let closed = s.push(&audio);
        let rest = s.finish().unwrap_or_default();
        assert_eq!(
            closed.len() + usize::from(!rest.is_empty()),
            2,
            "上限で 1 か所切れる"
        );
        let first = closed.first().unwrap_or(&rest);
        let cut_seconds = first.len() as f64 / RATE as f64;
        // 切れ目は静かな部分(8〜9 秒)の近くにある。10 秒付近(上限に達した位置)ではない
        assert!(
            (7.9..=9.2).contains(&cut_seconds),
            "切れ目は最も静かな部分の近くのはず: {cut_seconds}"
        );
    }

    #[test]
    fn finish_returns_the_open_interval_and_resets() {
        let mut s = Segmenter::new(RATE);
        s.push(&tone(1.5, 0.1));
        assert!(s.is_open());
        let open_len = s.open_samples().len();
        let interval = s.finish().expect("開いている区間がある");
        assert_eq!(interval.len(), open_len);
        assert!(!s.is_open());
        assert!(s.finish().is_none());
    }

    #[test]
    fn open_samples_tracks_the_audio_pushed_so_far() {
        let mut s = Segmenter::new(RATE);
        s.push(&tone(0.3, 0.1));
        s.push(&tone(0.3, 0.1));
        // 雑音を学習している間の発話も残す
        let got = s.open_samples().len() as f64 / RATE as f64;
        assert!(
            (0.59..=0.61).contains(&got),
            "開いている区間は流した発話全体の長さのはず: {got}"
        );
    }

    #[test]
    fn very_quiet_speech_still_opens_when_the_room_is_silent() {
        let mut s = Segmenter::new(RATE);
        // 小さい話し声(RMS 約 0.021、-33dB)でも、雑音がほぼない部屋では発話になる
        s.push(&tone(2.0, 0.03));
        let interval = s.finish().expect("小さな話し声で区間が開く");
        assert!(interval.len() > RATE as usize, "区間が 1 秒分ある");
    }

    #[test]
    fn joined_intervals_keep_order_and_skip_empty_results() {
        let joined = join_intervals(&[
            "明日の会議は".to_string(),
            String::new(),
            "午後3時からです".to_string(),
        ]);
        assert_eq!(joined, "明日の会議は午後3時からです");
    }
}
