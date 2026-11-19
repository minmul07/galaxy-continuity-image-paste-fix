use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, UpdateWindow};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_ProgramData, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD,
    NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowExW, GWLP_USERDATA, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowThreadProcessId, HICON, HMENU, IDI_APPLICATION, IMAGE_ICON,
    KillTimer, LR_SHARED, LoadIconW, LoadImageW, MB_ICONWARNING, MB_OK, MENUITEMINFOW, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MFS_GRAYED, MIIM_STATE, MIIM_STRING, MSG,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SM_CXSMICON,
    SM_CYSMICON, SendMessageW, SetForegroundWindow, SetMenuItemInfoW, SetTimer, SetWindowLongPtrW,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WINDOW_STYLE,
    WM_APP, WM_CLIPBOARDUPDATE, WM_CONTEXTMENU, WM_DESTROY, WM_NULL, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSEXW, WS_EX_TOOLWINDOW,
};
use windows::core::{Error, PCWSTR, PWSTR, w};

use crate::clipboard::NativeClipboard;
use crate::startup;
use crate::transfer::TransferController;
use crate::winutil::{known_folder_path, show_message_box, to_wide_null};

pub const TITLE: &str = "Galaxy Clipboard Helper";
const POLL_INTERVAL_MS: u32 = 150;
const TIMER_ID: usize = 1;
const TRAY_ICON_ID: u32 = 1;
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_DECODE_DONE: u32 = WM_APP + 2;
const MN_SIZEWINDOW: u32 = 0x01E2;

const IDM_STATUS: usize = 1001;
const IDM_STARTUP: usize = 1002;
const IDM_OPEN_TRANSFERS: usize = 1003;
const IDM_EXIT: usize = 1004;

struct TrayState {
    hwnd: HWND,
    hicon: HICON,
    clipboard: Option<NativeClipboard>,
    controller: TransferController,
    taskbar_created_msg: u32,
    timer_running: bool,
    active_menu: Option<HMENU>,
}

impl TrayState {
    fn start_timer(&mut self) {
        if !self.timer_running {
            // Guard with `timer_running` so rapid WM_CLIPBOARDUPDATE events do not reset the 150ms countdown.
            // SAFETY: `self.hwnd` is a valid window owned by this thread.
            let id = unsafe { SetTimer(self.hwnd, TIMER_ID, POLL_INTERVAL_MS, None) };
            if id != 0 {
                self.timer_running = true;
            }
        }
    }

    fn stop_timer(&mut self) {
        if self.timer_running {
            // SAFETY: `self.hwnd` is a valid window owned by this thread.
            unsafe {
                let _ = KillTimer(self.hwnd, TIMER_ID);
            }
            self.timer_running = false;
        }
    }

    fn poll_clipboard(&mut self) {
        let Some(ref clipboard) = self.clipboard else {
            self.stop_timer();
            return;
        };
        let prev_status = self
            .active_menu
            .map(|_| self.controller.status().to_string());
        let keep_running = self.controller.tick(clipboard, Instant::now());
        if let (Some(hmenu), Some(prev)) = (self.active_menu, prev_status)
            && self.controller.status() != prev
        {
            update_open_menu_status(hmenu, self.controller.status());
        }
        if !keep_running {
            self.stop_timer();
        }
    }
}

struct ActiveMenuGuard<'a>(&'a RefCell<TrayState>);

impl Drop for ActiveMenuGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.try_borrow_mut() {
            state.active_menu = None;
        }
    }
}

struct MenuGuard(HMENU);

impl Drop for MenuGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was created by `CreatePopupMenu` and not yet destroyed.
            unsafe {
                let _ = DestroyMenu(self.0);
            }
        }
    }
}

/// Creates the hidden tray window, registers clipboard notifications, and runs the Win32 message loop.
pub fn run() -> windows::core::Result<()> {
    let samsung_folder = known_folder_path(&FOLDERID_ProgramData)?
        .join("Samsung")
        .join("SamsungContinuityService")
        .join("ClipBoard");
    let transfer_root = known_folder_path(&FOLDERID_LocalAppData)?
        .join("GalaxyClipboardHelper")
        .join("Transfers");

    // SAFETY: `GetModuleHandleW(None)` retrieves the current executable's HMODULE.
    let hmodule = unsafe { GetModuleHandleW(None)? };
    let hinstance = HINSTANCE(hmodule.0);

    let class_name = w!("GalaxyClipboardHelperTrayWindow");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        lpszClassName: class_name,
        ..Default::default()
    };

    // SAFETY: `wc` points to a valid `WNDCLASSEXW` with static class name and `wnd_proc`.
    if unsafe { RegisterClassExW(&wc) } == 0 {
        return Err(Error::from_win32());
    }

    // Use a hidden top-level tool window (not HWND_MESSAGE) so TaskbarCreated and TrackPopupMenuEx work reliably.
    // SAFETY: All strings and handles are valid for the call.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("Galaxy Clipboard Helper"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            hinstance,
            None,
        )?
    };

    let clipboard = match NativeClipboard::new(hwnd) {
        Ok(cb) => cb,
        Err(err) => {
            // SAFETY: `hwnd` was created above and has no user data yet.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err(err);
        }
    };

    let hicon = match load_tray_icon() {
        Ok(icon) => icon,
        Err(err) => {
            drop(clipboard);
            // SAFETY: `hwnd` was created above.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err(err);
        }
    };

    let raw_hwnd = hwnd.0 as usize;
    let on_decode_ready = Arc::new(move || {
        let target = HWND(raw_hwnd as *mut c_void);
        // SAFETY: Posting `WM_DECODE_DONE` to a potentially closed HWND is safe; Win32 ignores invalid handles.
        unsafe {
            let _ = PostMessageW(target, WM_DECODE_DONE, WPARAM(0), LPARAM(0));
        }
    });

    // SAFETY: Static null-terminated string literal.
    let taskbar_created_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    let controller = TransferController::new(samsung_folder, transfer_root, Some(on_decode_ready));

    let state = Box::new(RefCell::new(TrayState {
        hwnd,
        hicon,
        clipboard: Some(clipboard),
        controller,
        taskbar_created_msg,
        timer_running: false,
        active_menu: None,
    }));
    let state_ptr = Box::into_raw(state);

    // SAFETY: Attach state pointer to `hwnd` before adding the tray icon or starting the timer.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

    if !add_tray_icon(hwnd, hicon) {
        let err = Error::from_win32();
        // SAFETY: Detach pointer and destroy window before freeing `state_ptr`.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(hwnd);
            drop(Box::from_raw(state_ptr));
        }
        return Err(err);
    }

    // Perform an initial clipboard check right after startup, matching the C# implementation.
    // SAFETY: `state_ptr` is valid on this thread and not currently borrowed.
    unsafe {
        (*state_ptr).borrow_mut().start_timer();
    }

    let mut msg = MSG::default();
    loop {
        // SAFETY: Standard Win32 message loop for the current thread.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        // SAFETY: Dispatches retrieved message to `wnd_proc`.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // SAFETY: Message loop has exited; detach user data and drop the heap-allocated `TrayState`.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(state_ptr));
    }

    Ok(())
}

fn build_notify_icon_data(hwnd: HWND, hicon: HICON) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: hicon,
        ..Default::default()
    };
    let tip = to_wide_null(TITLE);
    let copy_len = tip.len().min(nid.szTip.len());
    nid.szTip[..copy_len].copy_from_slice(&tip[..copy_len]);
    nid
}

fn add_tray_icon(hwnd: HWND, hicon: HICON) -> bool {
    let nid = build_notify_icon_data(hwnd, hicon);
    // SAFETY: `nid` is initialized with valid `cbSize`, `hWnd`, and `hIcon`.
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid).as_bool() }
}

fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    // SAFETY: Removes tray icon previously registered with `(hwnd, TRAY_ICON_ID)`.
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn load_tray_icon() -> windows::core::Result<HICON> {
    // SAFETY: Queries system small-icon metrics for the current DPI and loads the stock application icon.
    unsafe {
        let cx = GetSystemMetrics(SM_CXSMICON);
        let cy = GetSystemMetrics(SM_CYSMICON);
        if cx > 0
            && cy > 0
            && let Ok(handle) = LoadImageW(None, IDI_APPLICATION, IMAGE_ICON, cx, cy, LR_SHARED)
        {
            return Ok(HICON(handle.0));
        }
        LoadIconW(None, IDI_APPLICATION)
    }
}

fn update_menu_status_item(hmenu: HMENU, status_text: &str) -> bool {
    let mut status_wide = to_wide_null(status_text);
    let mii = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STRING | MIIM_STATE,
        fState: MFS_GRAYED,
        dwTypeData: PWSTR(status_wide.as_mut_ptr()),
        ..Default::default()
    };
    // SAFETY: `hmenu` is a valid menu handle and `status_wide` is null-terminated UTF-16.
    unsafe { SetMenuItemInfoW(hmenu, IDM_STATUS as u32, false, &mii).is_ok() }
}

fn update_open_menu_status(hmenu: HMENU, status_text: &str) {
    if !update_menu_status_item(hmenu, status_text) {
        return;
    }
    // SAFETY: Enumerates top-level popup menu windows (`#32768`) owned by this UI thread.
    unsafe {
        let current_tid = GetCurrentThreadId();
        let mut menu_windows = Vec::new();
        let mut after = HWND::default();
        while let Ok(menu_hwnd) = FindWindowExW(None, after, w!("#32768"), None) {
            if GetWindowThreadProcessId(menu_hwnd, None) == current_tid {
                menu_windows.push(menu_hwnd);
            }
            after = menu_hwnd;
        }
        for menu_hwnd in menu_windows {
            let _ = SendMessageW(menu_hwnd, MN_SIZEWINDOW, WPARAM(0), LPARAM(0));
            let _ = InvalidateRect(menu_hwnd, None, true);
            let _ = UpdateWindow(menu_hwnd);
        }
    }
}

unsafe fn show_context_menu(hwnd: HWND, state_cell: &RefCell<TrayState>) {
    // Snapshot needed fields and drop the borrow immediately before `TrackPopupMenuEx`
    // enters a nested modal message loop where `WM_TIMER` / `WM_CLIPBOARDUPDATE` can arrive.
    let (status_text, transfer_root): (String, PathBuf) = {
        let state = state_cell.borrow();
        if state.active_menu.is_some() {
            return;
        }
        (
            state.controller.status().to_string(),
            state.controller.transfer_root().to_path_buf(),
        )
    };

    // SAFETY: Menu handles and strings remain valid for the duration of the popup loop.
    let cmd = unsafe {
        let Ok(hmenu) = CreatePopupMenu() else {
            return;
        };
        let _menu_guard = MenuGuard(hmenu);

        let status_wide = to_wide_null(&status_text);
        let _ = AppendMenuW(
            hmenu,
            MF_STRING | MF_GRAYED,
            IDM_STATUS,
            PCWSTR(status_wide.as_ptr()),
        );

        let startup_flags = if startup::starts_on_login() {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING | MF_UNCHECKED
        };
        let _ = AppendMenuW(
            hmenu,
            startup_flags,
            IDM_STARTUP,
            w!("Windows 로그인 시 실행"),
        );
        let _ = AppendMenuW(hmenu, MF_STRING, IDM_OPEN_TRANSFERS, w!("전송 폴더 열기"));
        let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(hmenu, MF_STRING, IDM_EXIT, w!("종료"));

        state_cell.borrow_mut().active_menu = Some(hmenu);
        // Declared after `_menu_guard` so `active_menu` is cleared before `DestroyMenu` runs.
        let _active_menu_guard = ActiveMenuGuard(state_cell);

        let _ = SetForegroundWindow(hwnd);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let selected = TrackPopupMenuEx(
            hmenu,
            (TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_RETURNCMD).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        selected.0 as usize
    };

    match cmd {
        IDM_STARTUP => {
            if let Err(err) = startup::toggle_startup() {
                show_message_box(Some(hwnd), &err.to_string(), TITLE, MB_OK | MB_ICONWARNING);
            }
        }
        IDM_OPEN_TRANSFERS => {
            if let Err(err) = startup::open_transfers_folder(&transfer_root) {
                show_message_box(Some(hwnd), &err, TITLE, MB_OK | MB_ICONWARNING);
            }
        }
        IDM_EXIT => {
            // SAFETY: Terminates the tray window and triggers `WM_DESTROY` -> `PostQuitMessage`.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
        _ => {}
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `GWLP_USERDATA` points to a live `RefCell<TrayState>` while `run()` is in its message loop.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<TrayState> };
    if ptr.is_null() {
        // SAFETY: Default handling before state attachment or after detachment.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let state_cell = unsafe { &*ptr };

    if msg == WM_CLIPBOARDUPDATE {
        if let Ok(mut state) = state_cell.try_borrow_mut() {
            state.start_timer();
        }
        return LRESULT(0);
    }

    if msg == WM_TIMER && wparam.0 == TIMER_ID {
        if let Ok(mut state) = state_cell.try_borrow_mut() {
            state.poll_clipboard();
        }
        return LRESULT(0);
    }

    if msg == WM_DECODE_DONE {
        if let Ok(mut state) = state_cell.try_borrow_mut() {
            state.poll_clipboard();
        }
        return LRESULT(0);
    }

    if msg == WM_TRAYICON {
        let mouse_msg = lparam.0 as u32;
        if mouse_msg == WM_RBUTTONUP || mouse_msg == WM_CONTEXTMENU {
            // SAFETY: No borrow of `state_cell` is held when entering `show_context_menu`.
            unsafe {
                show_context_menu(hwnd, state_cell);
            }
        }
        return LRESULT(0);
    }

    if msg == WM_DESTROY {
        if let Ok(mut state) = state_cell.try_borrow_mut() {
            state.stop_timer();
            // Drop `NativeClipboard` while `hwnd` is still valid so `RemoveClipboardFormatListener` succeeds.
            state.clipboard = None;
        }
        remove_tray_icon(hwnd);
        // SAFETY: Posts quit message to end the `GetMessageW` loop.
        unsafe {
            PostQuitMessage(0);
        }
        return LRESULT(0);
    }

    let taskbar_msg = state_cell
        .try_borrow()
        .map(|s| s.taskbar_created_msg)
        .unwrap_or(0);
    if taskbar_msg != 0 && msg == taskbar_msg {
        if let Ok(state) = state_cell.try_borrow() {
            let _ = add_tray_icon(state.hwnd, state.hicon);
        }
        return LRESULT(0);
    }

    // SAFETY: Standard default window procedure for unhandled messages.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::GetMenuItemInfoW;

    #[test]
    fn updates_status_menu_item_text_and_keeps_grayed_state() {
        // SAFETY: Creates a temporary popup menu, appends a status item, updates it, and verifies via `GetMenuItemInfoW`.
        unsafe {
            let hmenu = CreatePopupMenu().expect("create popup menu");
            let _guard = MenuGuard(hmenu);

            let initial = to_wide_null("갤럭시 클립보드 감시 중");
            AppendMenuW(
                hmenu,
                MF_STRING | MF_GRAYED,
                IDM_STATUS,
                PCWSTR(initial.as_ptr()),
            )
            .expect("append status");

            let updated_text = "클립보드에 이미지 복사됨 (1920 × 1080)";
            assert!(update_menu_status_item(hmenu, updated_text));

            let mut buf = vec![0u16; 128];
            let mut mii = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STRING | MIIM_STATE,
                dwTypeData: PWSTR(buf.as_mut_ptr()),
                cch: buf.len() as u32,
                ..Default::default()
            };
            GetMenuItemInfoW(hmenu, IDM_STATUS as u32, false, &mut mii).expect("get menu info");

            let actual = String::from_utf16_lossy(&buf[..mii.cch as usize]);
            assert_eq!(actual, updated_text);
            assert_eq!(mii.fState.0 & MFS_GRAYED.0, MFS_GRAYED.0);
        }
    }
}
