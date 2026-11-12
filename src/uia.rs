//! Edit モードが選んだ文を TextPattern で読む。キー操作とクリップボードは使わない。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextPattern, UIA_TextPatternId,
};

pub const MAX_SELECTION_CHARS: usize = 6000;
static PROBE_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionProbe {
    Unavailable,
    TimedOut,
    None,
    Selected(String),
}

struct RunningProbe;

impl Drop for RunningProbe {
    fn drop(&mut self) {
        PROBE_RUNNING.store(false, Ordering::Release);
    }
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub fn probe_selected_text(timeout: Duration) -> SelectionProbe {
    probe_with_timeout(timeout, || {
        probe_with_com().unwrap_or(SelectionProbe::Unavailable)
    })
}

fn probe_with_timeout(
    timeout: Duration,
    probe: impl FnOnce() -> SelectionProbe + Send + 'static,
) -> SelectionProbe {
    if PROBE_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return SelectionProbe::TimedOut;
    }
    let (sender, receiver) = channel();
    std::thread::spawn(move || {
        let running = RunningProbe;
        let result = probe();
        drop(running);
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(timeout)
        .unwrap_or(SelectionProbe::TimedOut)
}

fn probe_with_com() -> Result<SelectionProbe, String> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| format!("COM を初期化できません: {e}"))?;
        let _apartment = ComApartment;
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("UI Automation を使えません: {e}"))?;
        let element = automation
            .GetFocusedElement()
            .map_err(|e| format!("フォーカスのある部品を取得できません: {e}"))?;
        selection_from_pattern(element.GetCurrentPatternAs(UIA_TextPatternId))
    }
}

fn selection_from_pattern(
    pattern: windows::core::Result<IUIAutomationTextPattern>,
) -> Result<SelectionProbe, String> {
    let Ok(pattern) = pattern else {
        return Ok(SelectionProbe::Unavailable);
    };
    unsafe {
        let ranges = pattern
            .GetSelection()
            .map_err(|e| format!("選択範囲を取得できません: {e}"))?;
        if ranges
            .Length()
            .map_err(|e| format!("選択範囲の数を取得できません: {e}"))?
            == 0
        {
            return Ok(SelectionProbe::None);
        }
        let range = ranges
            .GetElement(0)
            .map_err(|e| format!("選択範囲を取得できません: {e}"))?;
        // UTF-16 の2単位を使う文字でも、上限超過を文字数で判定できる量を読む。
        let text = range
            .GetText((MAX_SELECTION_CHARS * 2 + 2) as i32)
            .map_err(|e| format!("選択された文を取得できません: {e}"))?
            .to_string();
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

    #[test]
    fn an_unsupported_text_pattern_is_reported_as_unavailable() {
        let error =
            windows::core::Error::from_hresult(windows::core::HRESULT(0x80040204u32 as i32));
        assert_eq!(
            selection_from_pattern(Err(error)).unwrap(),
            SelectionProbe::Unavailable
        );
    }

    #[test]
    fn a_timed_out_probe_does_not_start_more_threads() {
        let (release, wait) = channel();
        let (done, finished) = channel();
        assert_eq!(
            probe_with_timeout(Duration::ZERO, move || {
                wait.recv().unwrap();
                let _ = done.send(());
                SelectionProbe::Selected("遅れて返る結果".into())
            }),
            SelectionProbe::TimedOut
        );
        assert_eq!(
            probe_with_timeout(Duration::from_millis(20), || panic!("重複した照会")),
            SelectionProbe::TimedOut
        );
        release.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while PROBE_RUNNING.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(
            probe_with_timeout(Duration::from_secs(2), || SelectionProbe::Selected(
                "次の結果".into()
            )),
            SelectionProbe::Selected("次の結果".into())
        );
    }
}
