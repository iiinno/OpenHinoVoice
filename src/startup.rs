//! ログイン時の自動起動。HKCU の Run キーに exe のパスを置く

use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const NAME: PCWSTR = w!("OpenHinoVoice");

/// 旧名 JapanVoice で自動起動を登録していたら、新しい名前と今の exe に付け替える
pub fn migrate_legacy() {
    const OLD: PCWSTR = w!("JapanVoice");
    unsafe {
        if RegGetValueW(HKEY_CURRENT_USER, RUN, OLD, RRF_RT_REG_SZ, None, None, None).is_ok() {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, OLD);
            let _ = set_enabled(true);
        }
    }
}

pub fn is_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN,
            NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

pub fn set_enabled(on: bool) -> Result<(), String> {
    // 有効時は常に現在の exe のパスを書き直す。ファイル移動後の stale Run 値を修復する。
    if !on && !is_enabled() {
        return Ok(());
    }
    unsafe {
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let value: Vec<u16> = format!("\"{}\"", exe.display())
                .encode_utf16()
                .chain(Some(0))
                .collect();
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN,
                NAME,
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            )
            .ok()
            .map_err(|e| format!("自動起動を設定できません: {e}"))
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, NAME)
                .ok()
                .map_err(|e| format!("自動起動を解除できません: {e}"))
        }
    }
}
