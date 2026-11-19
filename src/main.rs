#![cfg_attr(not(test), windows_subsystem = "windows")]

mod clipboard;
mod image;
mod startup;
mod transfer;
mod tray;
mod winutil;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK};
use windows::core::w;

use crate::winutil::{ComGuard, HandleGuard, show_message_box};

fn main() {
    winutil::enable_dpi_awareness();
    let _visual_styles = winutil::VisualStylesGuard::activate();

    // Prevent duplicate instances in the same Windows session, sharing the mutex name with C#.
    // SAFETY: Static null-terminated wide string literal.
    let mutex = match unsafe { CreateMutexW(None, true, w!("Local\\GalaxyClipboardHelper")) } {
        Ok(handle) => handle,
        Err(err) => {
            show_message_box(None, &err.to_string(), tray::TITLE, MB_OK | MB_ICONERROR);
            return;
        }
    };
    let _instance_guard = HandleGuard(mutex);

    // SAFETY: Checked immediately after `CreateMutexW` on the same thread.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }

    let _com = match ComGuard::new(COINIT_APARTMENTTHREADED) {
        Ok(guard) => guard,
        Err(err) => {
            show_message_box(None, &err.to_string(), tray::TITLE, MB_OK | MB_ICONERROR);
            return;
        }
    };

    if let Err(err) = tray::run() {
        show_message_box(None, &err.to_string(), tray::TITLE, MB_OK | MB_ICONERROR);
    }
}
