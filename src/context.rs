//! Best-effort executable name of the application captured as the paste destination.
//! The caller supplies PasteTarget's already-captured PID to avoid a foreground-window race.

#[cfg(windows)]
pub fn process_name(pid: u32) -> Option<String> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    if pid == 0 {
        return None;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut wide = vec![0u16; 32768];
        let mut size = wide.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(wide.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let full = String::from_utf16(&wide[..size as usize]).ok()?;
        let name = full.rsplit(['\\', '/']).next()?;
        (!name.is_empty()).then(|| name.to_owned())
    }
}

#[cfg(not(windows))]
pub fn process_name(_pid: u32) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn missing_process_returns_none() {
        assert!(super::process_name(0).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn current_process_can_be_identified() {
        let name = super::process_name(std::process::id()).expect("own process can be queried");
        assert!(name.to_ascii_lowercase().ends_with(".exe"));
    }
}
