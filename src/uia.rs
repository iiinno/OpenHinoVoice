//! Edit モードが選んだ文を読む。UI Automation の TextPattern だけを使い、
//! キー操作を送らず、クリップボードにも触れない。
//! UI Automation は Edit がオンで、初めて使う時点で読み込む。
//! 常駐中にあらかじめ読み込むことはしない

use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
};

/// 選択の調べ方の結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionProbe {
    /// 入力先が TextPattern に対応していない。Edit を使えないことを出す
    Unavailable,
    /// 選ばれた文はない。通常の音声入力になる
    None,
    /// 選ばれた文がある。Edit として録音する
    Selected(String),
}

/// 入力先で文が選ばれているかを timeout 以内で調べる。
/// 時間内に分からなければ、通常の音声入力として始める(None を返す)
pub fn probe_selected_text(timeout: Duration) -> SelectionProbe {
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let _ = sender.send(probe_blocking());
    });
    match receiver.recv_timeout(timeout) {
        Ok(probe) => probe,
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            // 選択の有無が分からないまま Edit を試みてはいけない。
            // 通常の音声入力として録音する
            SelectionProbe::None
        }
    }
}

fn probe_blocking() -> SelectionProbe {
    let result = probe_with_com();
    match result {
        Ok(probe) => probe,
        Err(_) => SelectionProbe::None,
    }
}

fn probe_with_com() -> Result<SelectionProbe, String> {
    unsafe {
        // COM の初期化は呼びたしのたびに行う。S_OK 以外でも、すでに初期化
        // されているスレッドではそのまま使える
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("UI Automation を使えません: {e}"))?;
        // キー操作もクリップボードも使わず、フォーカスのある画面部品から読む
        let element = automation
            .GetFocusedElement()
            .map_err(|e| format!("フォーカスのある部品を取得できません: {e}"))?;
        let unknown = element
            .GetCurrentPattern(UIA_TextPatternId)
            .map_err(|e| format!("TextPattern を取得できません: {e}"))?;
        let pattern: IUIAutomationTextPattern = match unknown.cast() {
            Ok(pattern) => pattern,
            // この画面部品は TextPattern に対応していない
            Err(_) => return Ok(SelectionProbe::Unavailable),
        };
        let ranges = pattern
            .GetSelection()
            .map_err(|e| format!("選択範囲を取得できません: {e}"))?;
        let range = ranges
            .GetElement(0)
            .map_err(|e| format!("選択範囲を取得できません: {e}"))?;
        let text = range
            .GetText(-1)
            .map_err(|e| format!("選択された文を取得できません: {e}"))?;
        let text = text.to_string();
        if text.trim().is_empty() {
            Ok(SelectionProbe::None)
        } else {
            Ok(SelectionProbe::Selected(text))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 時間内に返らない調べは、通常の音声入力に落ちる(選択の有無が
    /// 分からないまま Edit を試みないことの確認)
    #[test]
    fn slow_probe_falls_back_to_none_within_the_timeout() {
        let started = Instant::now();
        // 実際の COM の呼び出しを差し替えられないので、timeout を極端に短くして
        // 別スレッドの応答を待たない経路を確かめる
        let probe = probe_selected_text(Duration::from_nanos(1));
        assert_eq!(probe, SelectionProbe::None);
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
