//! The tray icon (classic Eudora's): the blue in-tray, empty, or with the green arrow when new
//! mail has come in since the window was last looked at. Classic's tray menu: Open Eudora, New
//! Message, Check Mail, Work Offline, Minimize to Tray, Exit. A click on the icon brings the
//! window back. On the Mac this is the menu bar item. The pictures are made by
//! scripts/classic_icons.sh.

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager};

const NO_MAIL: tauri::image::Image<'static> = tauri::include_image!("icons/tray-nomail.png");
const NEW_MAIL: tauri::image::Image<'static> = tauri::include_image!("icons/tray-newmail.png");

/// Minimize to Tray (classic's option): a minimized window leaves the taskbar for the tray.
pub static MINIMIZE_TO_TRAY: AtomicBool = AtomicBool::new(false);
static NEW: AtomicBool = AtomicBool::new(false);
/// Whether the icon is in the tray: a Linux desktop without the tray library (libayatana-
/// appindicator) has none, and then nothing is ever minimized to it.
pub static SHOWN: AtomicBool = AtomicBool::new(false);

fn show_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Puts the icon in the tray (at start).
pub fn install(app: &tauri::AppHandle) -> tauri::Result<()> {
    let minimize = CheckMenuItemBuilder::with_id("tray-minimize", "Minimize to Tray").checked(MINIMIZE_TO_TRAY.load(Ordering::Relaxed)).build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&MenuItemBuilder::with_id("tray-open", "Open Eudora").build(app)?)
        .separator()
        .item(&MenuItemBuilder::with_id("new", "New Message").build(app)?)
        .item(&MenuItemBuilder::with_id("check", "Check Mail").build(app)?)
        .item(&MenuItemBuilder::with_id("work-offline", "Work Offline").build(app)?)
        .separator()
        .item(&minimize)
        .separator()
        .item(&MenuItemBuilder::with_id("exit", "Exit").build(app)?)
        .build()?;
    let builder = TrayIconBuilder::with_id("eudora")
        .icon(NO_MAIL)
        .tooltip("Eudora")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| {
            let id = event.id().as_ref().to_string();
            match id.as_str() {
                "tray-open" => show_window(app),
                "tray-minimize" => {
                    let on = !MINIMIZE_TO_TRAY.load(Ordering::Relaxed);
                    MINIMIZE_TO_TRAY.store(on, Ordering::Relaxed);
                    let _ = app.emit_to("main", "tray-minimize", on);
                }
                "exit" => app.exit(0),
                other => {
                    // the same commands as the menu bar's: the window does them
                    if other == "new" {
                        show_window(app);
                    }
                    let _ = app.emit_to("main", "menu", other.to_string());
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        });
    // (Linux: Tauri's tray loads libayatana-appindicator when it's made and panics when the
    // system has none, as a bare Fedora: Eudora then runs without a tray icon)
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builder.build(app))) {
        Ok(made) => {
            made?;
            SHOWN.store(true, Ordering::Relaxed);
        }
        Err(_) => eprintln!("Eudora: no tray icon (this system has no tray library: libayatana-appindicator)"),
    }
    Ok(())
}

/// New mail (`on`) or none to show: the icon changes.
pub fn set_new_mail(app: &tauri::AppHandle, on: bool) {
    if NEW.swap(on, Ordering::Relaxed) == on {
        return;
    }
    if let Some(tray) = app.tray_by_id("eudora") {
        let _ = tray.set_icon(Some(if on { NEW_MAIL } else { NO_MAIL }));
        let _ = tray.set_tooltip(Some(if on { "Eudora: new mail" } else { "Eudora" }));
    }
}
