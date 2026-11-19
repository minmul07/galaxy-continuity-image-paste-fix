use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_DIB, CF_HDROP};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
use windows::core::{Error, PWSTR, w};

use crate::image::ClipboardImage;
use crate::winutil::HandleGuard;

const MAX_QUERY_FILE_CHARS: u32 = 32768;

/// Snapshot of relevant Windows clipboard state under a single clipboard lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardSnapshot {
    pub sequence: u32,
    pub file: Option<PathBuf>,
    pub has_samsung_tag: bool,
}

/// Result of attempting to publish a decoded image to the Windows clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardWrite {
    Written,
    Busy,
    Changed,
}

/// Abstraction over Win32 clipboard and foreground checks to support deterministic state-machine tests.
pub trait ClipboardBackend {
    fn read(&self) -> Option<ClipboardSnapshot>;
    fn write_image(
        &self,
        expected_sequence: u32,
        image: &ClipboardImage,
    ) -> windows::core::Result<ClipboardWrite>;
    fn can_request_transfer(&self) -> bool;
}

/// Win32 clipboard listener and reader/writer tied to a host window handle.
pub struct NativeClipboard {
    hwnd: HWND,
    png_format: u32,
    samsung_tag_format: u32,
}

impl NativeClipboard {
    /// Registers custom clipboard formats and subscribes `hwnd` to `WM_CLIPBOARDUPDATE`.
    pub fn new(hwnd: HWND) -> windows::core::Result<Self> {
        // SAFETY: Both literals are valid null-terminated UTF-16 strings and `hwnd` is valid.
        unsafe {
            let png_format = RegisterClipboardFormatW(w!("PNG"));
            if png_format == 0 {
                return Err(Error::from_win32());
            }
            let samsung_tag_format = RegisterClipboardFormatW(w!("B0CD2DD9"));
            if samsung_tag_format == 0 {
                return Err(Error::from_win32());
            }
            AddClipboardFormatListener(hwnd)?;
            Ok(Self {
                hwnd,
                png_format,
                samsung_tag_format,
            })
        }
    }
}

impl Drop for NativeClipboard {
    fn drop(&mut self) {
        // SAFETY: `self.hwnd` was registered via `AddClipboardFormatListener` and is still alive during drop.
        unsafe {
            let _ = RemoveClipboardFormatListener(self.hwnd);
        }
    }
}

impl ClipboardBackend for NativeClipboard {
    fn read(&self) -> Option<ClipboardSnapshot> {
        let _lock = ClipboardLock::acquire(self.hwnd)?;
        // SAFETY: We hold the clipboard lock for this thread until `_lock` drops.
        unsafe {
            let mut file = None;
            if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok()
                && let Ok(handle) = GetClipboardData(CF_HDROP.0 as u32)
                && !handle.is_invalid()
            {
                let hdrop = HDROP(handle.0);
                if DragQueryFileW(hdrop, u32::MAX, None) == 1 {
                    let len = DragQueryFileW(hdrop, 0, None);
                    if len > 0 && len < MAX_QUERY_FILE_CHARS {
                        let mut buf = vec![0u16; len as usize + 1];
                        let copied = DragQueryFileW(hdrop, 0, Some(&mut buf));
                        if copied > 0 {
                            file =
                                Some(PathBuf::from(OsString::from_wide(&buf[..copied as usize])));
                        }
                    }
                }
            }

            let sequence = GetClipboardSequenceNumber();
            let has_samsung_tag = IsClipboardFormatAvailable(self.samsung_tag_format).is_ok();
            Some(ClipboardSnapshot {
                sequence,
                file,
                has_samsung_tag,
            })
        }
    }

    fn write_image(
        &self,
        expected_sequence: u32,
        image: &ClipboardImage,
    ) -> windows::core::Result<ClipboardWrite> {
        let dib_mem = ClipboardMemory::new(&image.dib)?;
        let png_mem = ClipboardMemory::new(&image.png)?;
        let tag_mem = ClipboardMemory::new(&1i32.to_le_bytes())?;

        let Some(_lock) = ClipboardLock::acquire(self.hwnd) else {
            return Ok(ClipboardWrite::Busy);
        };

        // SAFETY: We hold the clipboard lock; compare sequence under the same lock before mutating.
        unsafe {
            if GetClipboardSequenceNumber() != expected_sequence {
                return Ok(ClipboardWrite::Changed);
            }
            EmptyClipboard()?;
            dib_mem.publish(CF_DIB.0 as u32)?;
            png_mem.publish(self.png_format)?;
            // Keep Samsung's sync tag so Galaxy Connect does not send the converted image back to the phone.
            tag_mem.publish(self.samsung_tag_format)?;
            Ok(ClipboardWrite::Written)
        }
    }

    fn can_request_transfer(&self) -> bool {
        // SAFETY: Win32 window and process queries with validated handle and buffer bounds.
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.0.is_null() {
                return false;
            }
            let mut process_id = 0u32;
            GetWindowThreadProcessId(foreground, Some(&mut process_id));
            if process_id == 0 {
                return false;
            }
            let Some(name) = query_process_stem_by_id(process_id) else {
                return false;
            };
            is_allowed_foreground_process(&name)
        }
    }
}

fn extract_process_stem_from_wide(buf: &[u16]) -> Option<String> {
    let len = buf.iter().position(|&ch| ch == 0).unwrap_or(buf.len());
    if len == 0 {
        return None;
    }
    let exe_path = PathBuf::from(OsString::from_wide(&buf[..len]));
    exe_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
}

unsafe fn query_process_stem_by_id(process_id: u32) -> Option<String> {
    // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` is guarded by `HandleGuard`;
    // if UAC/PPL blocks opening a handle, fall back to a ToolHelp process snapshot like .NET does.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) {
            let _guard = HandleGuard(process);
            if let Some(stem) = query_process_stem(process) {
                return Some(stem);
            }
        }
        query_process_stem_from_toolhelp(process_id)
    }
}

unsafe fn query_process_stem_from_toolhelp(process_id: u32) -> Option<String> {
    // SAFETY: `TH32CS_SNAPPROCESS` enumerates processes without requiring per-process handles.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()? };
    let _guard = HandleGuard(snapshot);

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snapshot` is valid and `entry.dwSize` is initialized before calling `Process32FirstW`.
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        return None;
    }

    loop {
        if entry.th32ProcessID == process_id {
            return extract_process_stem_from_wide(&entry.szExeFile);
        }
        // SAFETY: Continues iterating the snapshot with initialized `entry.dwSize`.
        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            return None;
        }
    }
}

unsafe fn query_process_stem(process: HANDLE) -> Option<String> {
    let mut cap = 512usize;
    loop {
        let mut buf = vec![0u16; cap];
        let mut size = u32::try_from(buf.len()).ok()?;
        // SAFETY: `buf` has length `size` and `process` is an open handle with limited query access.
        match unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut size,
            )
        } {
            Ok(()) => return extract_process_stem_from_wide(&buf[..size as usize]),
            Err(err) if err.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() && cap < 32768 => {
                cap *= 2;
            }
            Err(_) => return None,
        }
    }
}

/// Returns `false` when the foreground process is an app into which Samsung Continuity Service
/// can paste directly (`WINWORD`, `EXCEL`, `POWERPNT`, `APPLICATIONFRAMEHOST`, or `*SamsungNotes*`).
pub fn is_allowed_foreground_process(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    !matches!(
        upper.as_str(),
        "WINWORD" | "EXCEL" | "POWERPNT" | "APPLICATIONFRAMEHOST"
    ) && !upper.contains("SAMSUNGNOTES")
}

struct ClipboardLock;

impl ClipboardLock {
    fn acquire(hwnd: HWND) -> Option<Self> {
        // SAFETY: `hwnd` is a valid window owned by the current thread.
        unsafe { OpenClipboard(hwnd).ok().map(|()| Self) }
    }
}

impl Drop for ClipboardLock {
    fn drop(&mut self) {
        // SAFETY: Paired with a successful `OpenClipboard` call on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// RAII wrapper around a `GMEM_MOVEABLE` allocation prior to ownership transfer via `SetClipboardData`.
pub struct ClipboardMemory {
    hglobal: HGLOBAL,
}

impl ClipboardMemory {
    pub fn new(bytes: &[u8]) -> windows::core::Result<Self> {
        let alloc_len = bytes.len().max(1);
        // SAFETY: Allocates moveable global memory and copies `bytes` while locked.
        unsafe {
            let hglobal = GlobalAlloc(GMEM_MOVEABLE, alloc_len)?;
            let mut mem = Self { hglobal };
            let ptr = GlobalLock(mem.hglobal);
            if ptr.is_null() {
                return Err(Error::from_win32());
            }
            if !bytes.is_empty() {
                ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            }
            let _ = GlobalUnlock(mem.hglobal);
            let result = mem.hglobal;
            mem.hglobal = HGLOBAL::default();
            Ok(Self { hglobal: result })
        }
    }

    pub fn publish(mut self, format: u32) -> windows::core::Result<()> {
        // SAFETY: Clipboard is open by this thread and `self.hglobal` is unlocked valid memory.
        unsafe {
            SetClipboardData(format, HANDLE(self.hglobal.0))?;
            self.hglobal = HGLOBAL::default(); // Windows owns the allocation after SetClipboardData succeeds.
            Ok(())
        }
    }
}

impl Drop for ClipboardMemory {
    fn drop(&mut self) {
        if !self.hglobal.is_invalid() {
            // SAFETY: Allocation was not transferred to Windows clipboard.
            unsafe {
                let _ = GlobalFree(self.hglobal);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_office_uwp_host_and_samsung_notes_processes() {
        for blocked in [
            "WINWORD",
            "winword",
            "EXCEL",
            "POWERPNT",
            "ApplicationFrameHost",
            "SamsungNotes",
            "GalaxySamsungNotesHelper",
        ] {
            assert!(
                !is_allowed_foreground_process(blocked),
                "{blocked} should defer transfer"
            );
        }

        for allowed in ["chrome", "mspaint", "explorer", "Code", "notepad"] {
            assert!(
                is_allowed_foreground_process(allowed),
                "{allowed} should allow transfer"
            );
        }
    }

    #[test]
    fn clipboard_memory_allocates_and_frees_without_touching_clipboard() {
        let mem = ClipboardMemory::new(&[1, 2, 3, 4]).expect("allocate GMEM_MOVEABLE");
        drop(mem);

        // SAFETY: Registering clipboard format names is read/idempotent and does not alter clipboard data.
        unsafe {
            assert_ne!(RegisterClipboardFormatW(w!("PNG")), 0);
            assert_ne!(RegisterClipboardFormatW(w!("B0CD2DD9")), 0);
        }
    }

    #[test]
    fn extracts_process_stem_from_wide_buffers() {
        let to_buf = |s: &str| -> Vec<u16> {
            s.encode_utf16()
                .chain([0u16, b'X' as u16, b'Y' as u16])
                .collect()
        };

        assert_eq!(
            extract_process_stem_from_wide(&to_buf("Taskmgr.exe")).as_deref(),
            Some("Taskmgr")
        );
        assert_eq!(
            extract_process_stem_from_wide(&to_buf("WINWORD.EXE")).as_deref(),
            Some("WINWORD")
        );
        assert_eq!(
            extract_process_stem_from_wide(&to_buf(r"C:\Program Files\App\SamsungNotes.exe"))
                .as_deref(),
            Some("SamsungNotes")
        );
        assert_eq!(
            extract_process_stem_from_wide(&to_buf("System")).as_deref(),
            Some("System")
        );
        assert_eq!(extract_process_stem_from_wide(&[0u16, 65u16]), None);
    }

    #[test]
    fn toolhelp_fallback_resolves_current_process_stem() {
        let pid = std::process::id();
        let expected_stem = std::env::current_exe()
            .expect("current exe")
            .file_stem()
            .expect("exe stem")
            .to_string_lossy()
            .into_owned();

        // SAFETY: Querying our own running process via Win32 APIs in a test.
        unsafe {
            assert_eq!(
                query_process_stem_from_toolhelp(pid).as_deref(),
                Some(expected_stem.as_str())
            );
            assert_eq!(
                query_process_stem_by_id(pid).as_deref(),
                Some(expected_stem.as_str())
            );
        }
    }
}
