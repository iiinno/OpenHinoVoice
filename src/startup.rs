//! ログイン時の自動起動。Windows は HKCU Run キー、Linux は XDG autostart を管理する

#[cfg(windows)]
mod imp {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };

    const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
    const NAME: PCWSTR = w!("OpenHinoVoice");

    /// 旧名 JapanVoice で自動起動を登録していたら、新しい名前と今の exe に付け替える
    pub fn migrate_legacy() {
        if crate::config::is_test_sandbox() {
            return;
        }
        const OLD: PCWSTR = w!("JapanVoice");
        unsafe {
            if RegGetValueW(HKEY_CURRENT_USER, RUN, OLD, RRF_RT_REG_SZ, None, None, None).is_ok()
                && set_enabled(true).is_ok()
            {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, OLD);
            }
        }
    }

    pub fn is_enabled() -> bool {
        if crate::config::is_test_sandbox() {
            return false;
        }
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
        if crate::config::is_test_sandbox() {
            return if on {
                Err("検証用の設定画面では自動起動を登録できません".into())
            } else {
                Ok(())
            };
        }
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
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    fn autostart_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("autostart").join("openhinovoice.desktop"))
    }

    pub fn migrate_legacy() {}

    pub fn is_enabled() -> bool {
        if crate::config::is_test_sandbox() {
            return false;
        }
        let Some(path) = autostart_path() else {
            return false;
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return false;
        };
        !content
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("Hidden=true"))
    }

    pub fn set_enabled(on: bool) -> Result<(), String> {
        if crate::config::is_test_sandbox() {
            return if on {
                Err("検証用の設定画面では自動起動を登録できません".into())
            } else {
                Ok(())
            };
        }
        let path = autostart_path().ok_or("自動起動フォルダの場所を特定できません")?;
        if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("自動起動フォルダを作れません: {e}"))?;
            }
            let escaped = exe
                .to_string_lossy()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            let entry = format!(
                "[Desktop Entry]\n\
                 Type=Application\n\
                 Version=1.0\n\
                 Name=OpenHinoVoice\n\
                 Comment=Japanese voice input with local speech recognition\n\
                 Exec=\"{escaped}\"\n\
                 Icon=openhinovoice\n\
                 Terminal=false\n\
                 Categories=Utility;Accessibility;\n\
                 X-GNOME-Autostart-enabled=true\n"
            );
            std::fs::write(&path, entry).map_err(|e| format!("自動起動を設定できません: {e}"))
        } else {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("自動起動を解除できません: {e}")),
            }
        }
    }
}

pub use imp::{is_enabled, migrate_legacy, set_enabled};
