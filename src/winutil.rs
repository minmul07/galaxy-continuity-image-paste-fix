use std::ffi::{OsStr, OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    CloseHandle, FreeLibrary, HANDLE, HMODULE, HWND, RPC_E_CHANGED_MODE, S_FALSE, S_OK,
};
use windows::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows::Win32::System::ApplicationInstallationAndServicing::{
    ACTCTXW, ActivateActCtx, CreateActCtxW, DeactivateActCtx, ReleaseActCtx,
};
use windows::Win32::System::Com::{
    COINIT, CoCreateGuid, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, LoadLibraryW};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::Win32::UI::WindowsAndMessaging::{MESSAGEBOX_STYLE, MessageBoxW, SetProcessDPIAware};
use windows::core::{GUID, PCWSTR, w};

const ACTCTX_FLAG_RESOURCE_NAME_VALID: u32 = 0x008;
const ACTCTX_FLAG_HMODULE_VALID: u32 = 0x080;
const SHELL32_MANIFEST_RESOURCE_ID: usize = 124;

/// Encodes a UTF-8 string slice into a null-terminated UTF-16 buffer.
pub fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encodes an OS string slice into a null-terminated UTF-16 buffer.
pub fn os_to_wide_null(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// Compares two paths using Windows ordinal case-insensitive comparison.
pub fn paths_equal_ignore_case(a: &Path, b: &Path) -> bool {
    let wa: Vec<u16> = a.as_os_str().encode_wide().collect();
    let wb: Vec<u16> = b.as_os_str().encode_wide().collect();
    // SAFETY: Both slices live for the duration of the call; `CompareStringOrdinal` reads within their lengths.
    unsafe { CompareStringOrdinal(&wa, &wb, true) == CSTR_EQUAL }
}

/// RAII guard for COM initialization on the current thread.
pub struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    /// Initializes COM with `coinit`. Succeeds without scheduling uninitialization if already initialized in another mode.
    pub fn new(coinit: COINIT) -> windows::core::Result<Self> {
        // SAFETY: `CoInitializeEx` is thread-local and `pvReserved` must be `None`.
        let hr = unsafe { CoInitializeEx(None, coinit) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self {
                should_uninit: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(windows::core::Error::from(hr))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Paired with a successful `CoInitializeEx` call on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// RAII guard that closes a Win32 `HANDLE` on drop.
pub struct HandleGuard(pub HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is a valid owned handle not yet closed.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Generates a new GUID formatted as 32 lowercase hexadecimal characters without hyphens.
pub fn new_guid_hex() -> windows::core::Result<String> {
    // SAFETY: `CoCreateGuid` has no special preconditions.
    let guid = unsafe { CoCreateGuid()? };
    Ok(format!(
        "{:08x}{:04x}{:04x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        guid.data1,
        guid.data2,
        guid.data3,
        guid.data4[0],
        guid.data4[1],
        guid.data4[2],
        guid.data4[3],
        guid.data4[4],
        guid.data4[5],
        guid.data4[6],
        guid.data4[7],
    ))
}

/// Resolves a Windows known folder path via `SHGetKnownFolderPath`.
pub fn known_folder_path(folder_id: &GUID) -> windows::core::Result<PathBuf> {
    // SAFETY: `folder_id` is a valid GUID pointer; returned buffer is freed with `CoTaskMemFree`.
    unsafe {
        let pwstr = SHGetKnownFolderPath(folder_id, KF_FLAG_DEFAULT, None)?;
        let len = (0..).take_while(|&i| *pwstr.0.add(i) != 0).count();
        let slice = std::slice::from_raw_parts(pwstr.0, len);
        let path = PathBuf::from(OsString::from_wide(slice));
        CoTaskMemFree(Some(pwstr.0 as *const c_void));
        Ok(path)
    }
}

/// Displays a modal Win32 message box.
pub fn show_message_box(hwnd: Option<HWND>, text: &str, title: &str, flags: MESSAGEBOX_STYLE) {
    let wide_text = to_wide_null(text);
    let wide_title = to_wide_null(title);
    // SAFETY: Both pointers are valid null-terminated UTF-16 strings for the call duration.
    unsafe {
        MessageBoxW(
            hwnd,
            PCWSTR(wide_text.as_ptr()),
            PCWSTR(wide_title.as_ptr()),
            flags,
        );
    }
}

/// Enables Per-Monitor V2 DPI awareness for the process, falling back to older APIs on legacy Windows.
pub fn enable_dpi_awareness() {
    // SAFETY: Sets process-wide DPI awareness before any windows are created.
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err()
            && SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE).is_err()
        {
            let _ = SetProcessDPIAware();
        }
    }
}

struct LoadedModuleGuard(Option<HMODULE>);

impl Drop for LoadedModuleGuard {
    fn drop(&mut self) {
        if let Some(module) = self.0.take() {
            // SAFETY: `module` was loaded via `LoadLibraryW` and not yet freed.
            unsafe {
                let _ = FreeLibrary(module);
            }
        }
    }
}

/// RAII guard that activates Common Controls v6 visual styles via `shell32.dll`'s embedded manifest.
pub struct VisualStylesGuard {
    handle: HANDLE,
    cookie: usize,
}

impl VisualStylesGuard {
    /// Creates and activates an activation context referencing `shell32.dll` resource `124`.
    pub fn activate() -> Option<Self> {
        // SAFETY: `shell32.dll` is a standard Windows system DLL; resource ID 124 is its ComCtl v6 manifest.
        unsafe {
            let (shell32, _loaded_guard) = match GetModuleHandleW(w!("shell32.dll")) {
                Ok(module) => (module, LoadedModuleGuard(None)),
                Err(_) => {
                    let module = LoadLibraryW(w!("shell32.dll")).ok()?;
                    (module, LoadedModuleGuard(Some(module)))
                }
            };

            let actctx = ACTCTXW {
                cbSize: std::mem::size_of::<ACTCTXW>() as u32,
                dwFlags: ACTCTX_FLAG_HMODULE_VALID | ACTCTX_FLAG_RESOURCE_NAME_VALID,
                lpResourceName: PCWSTR(SHELL32_MANIFEST_RESOURCE_ID as *const u16),
                hModule: shell32,
                ..Default::default()
            };
            let handle = CreateActCtxW(&actctx).ok()?;
            let mut cookie = 0usize;
            if ActivateActCtx(Some(handle), &mut cookie).is_err() {
                ReleaseActCtx(handle);
                return None;
            }
            Some(Self { handle, cookie })
        }
    }
}

impl Drop for VisualStylesGuard {
    fn drop(&mut self) {
        // SAFETY: `self.cookie` and `self.handle` come from a successful `ActivateActCtx`/`CreateActCtxW`.
        unsafe {
            let _ = DeactivateActCtx(0, self.cookie);
            ReleaseActCtx(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enables_dpi_awareness_and_activates_visual_styles() {
        enable_dpi_awareness();
        let guard = VisualStylesGuard::activate();
        assert!(
            guard.is_some(),
            "expected shell32 Common Controls v6 activation context to activate"
        );
        drop(guard);
    }
}
