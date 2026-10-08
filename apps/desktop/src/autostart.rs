//! Starting with Windows: a value under the user's `Run` key, the same one
//! setup writes when its "Start with Windows" box is ticked.

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Usage Monitor";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn is_enabled() -> bool {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let mut size = 0_u32;
        // SAFETY: the key and value names are NUL-terminated, and with no
        // buffer the call only reports the value's size.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut size,
            )
        };
        status == ERROR_SUCCESS && size > 2
    }

    pub fn set(enabled: bool) -> Result<(), String> {
        let key = wide(RUN_KEY);
        let name = wide(VALUE_NAME);
        let status = if enabled {
            let executable = std::env::current_exe().map_err(|error| error.to_string())?;
            let command = wide(&format!("\"{}\"", executable.display()));
            let bytes = u32::try_from(command.len() * 2).map_err(|error| error.to_string())?;
            // SAFETY: every pointer is to a live NUL-terminated UTF-16 string,
            // and `bytes` is the command's size including its NUL.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    key.as_ptr(),
                    name.as_ptr(),
                    REG_SZ,
                    command.as_ptr().cast(),
                    bytes,
                )
            }
        } else {
            // SAFETY: the key and value names are NUL-terminated.
            let status =
                unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
            // Already gone is what was asked for.
            if status == windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND {
                ERROR_SUCCESS
            } else {
                status
            }
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(status as i32).to_string())
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn set(_enabled: bool) -> Result<(), String> {
        Err("starting with the system is only supported on Windows".to_owned())
    }
}

pub(crate) use imp::{is_enabled, set};
