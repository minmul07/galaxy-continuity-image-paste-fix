use std::path::Path;

use windows::Win32::Foundation::{E_FAIL, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_EXPAND_SZ,
    REG_OPTION_NON_VOLATILE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
    RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{Error, HRESULT, PCWSTR, w};

use crate::winutil::{os_to_wide_null, to_wide_null};

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const RUN_NAME: &str = "GalaxyClipboardHelper";

struct RegKeyGuard(HKEY);

impl Drop for RegKeyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was opened via `RegOpenKeyExW` or `RegCreateKeyExW`.
            unsafe {
                let _ = RegCloseKey(self.0);
            }
        }
    }
}

/// Returns `true` when the `GalaxyClipboardHelper` string entry exists under `HKCU\...\Run`.
pub fn starts_on_login() -> bool {
    let key_wide = to_wide_null(RUN_KEY);
    let value_wide = to_wide_null(RUN_NAME);
    let mut hkey = HKEY::default();

    // SAFETY: Null-terminated wide strings and output HKEY pointer are valid.
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_wide.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let _guard = RegKeyGuard(hkey);

        let mut value_type = REG_VALUE_TYPE::default();
        let status = RegQueryValueExW(
            hkey,
            PCWSTR(value_wide.as_ptr()),
            None,
            Some(&mut value_type),
            None,
            None,
        );
        status == ERROR_SUCCESS && (value_type == REG_SZ || value_type == REG_EXPAND_SZ)
    }
}

/// Toggles the `HKCU\...\Run` startup registration and returns the new enabled state.
pub fn toggle_startup() -> windows::core::Result<bool> {
    let key_wide = to_wide_null(RUN_KEY);
    let value_wide = to_wide_null(RUN_NAME);
    let mut hkey = HKEY::default();

    // SAFETY: Null-terminated wide strings and output HKEY pointer are valid.
    unsafe {
        let status = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_wide.as_ptr()),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut hkey,
            None,
        );
        if status != ERROR_SUCCESS {
            return Err(Error::from(HRESULT::from_win32(status.0)));
        }
        let _guard = RegKeyGuard(hkey);

        if starts_on_login() {
            let del_status = RegDeleteValueW(hkey, PCWSTR(value_wide.as_ptr()));
            if del_status != ERROR_SUCCESS && del_status != ERROR_FILE_NOT_FOUND {
                return Err(Error::from(HRESULT::from_win32(del_status.0)));
            }
            Ok(false)
        } else {
            let exe_path = std::env::current_exe().map_err(|err| {
                err.raw_os_error()
                    .map(|code| Error::from(HRESULT::from_win32(code as u32)))
                    .unwrap_or_else(|| Error::from(E_FAIL))
            })?;
            let quoted_wide: Vec<u16> = std::iter::once(b'"' as u16)
                .chain(os_to_wide_null(exe_path.as_os_str()))
                .collect();
            let mut value_wide_data = Vec::with_capacity(quoted_wide.len() + 1);
            // Insert closing quote before the trailing null terminator from `os_to_wide_null`.
            let (without_nul, _) = quoted_wide.split_at(quoted_wide.len().saturating_sub(1));
            value_wide_data.extend_from_slice(without_nul);
            value_wide_data.push(b'"' as u16);
            value_wide_data.push(0);

            let bytes = std::slice::from_raw_parts(
                value_wide_data.as_ptr() as *const u8,
                value_wide_data.len() * std::mem::size_of::<u16>(),
            );
            let set_status =
                RegSetValueExW(hkey, PCWSTR(value_wide.as_ptr()), 0, REG_SZ, Some(bytes));
            if set_status != ERROR_SUCCESS {
                return Err(Error::from(HRESULT::from_win32(set_status.0)));
            }
            Ok(true)
        }
    }
}

/// Ensures `transfer_root` exists and opens it in Windows Explorer.
pub fn open_transfers_folder(transfer_root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(transfer_root).map_err(|e| e.to_string())?;
    let wide = os_to_wide_null(transfer_root.as_os_str());
    // SAFETY: `wide` is a valid null-terminated UTF-16 path string.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if (result.0 as isize) > 32 {
        Ok(())
    } else {
        Err(format!(
            "Could not open transfers folder (code: {})",
            result.0 as isize
        ))
    }
}
