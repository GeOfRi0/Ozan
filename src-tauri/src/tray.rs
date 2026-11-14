//! Windows Sistem Tepsisi simgesi ve menüsü.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use crate::commands::AppState;

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Ozan'ı Göster", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "Tüm Sesleri Durdur", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Çıkış", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &stop, &sep, &quit])?;

    let mut builder = TrayIconBuilder::with_id("ozan-tray")
        .tooltip("Ozan")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => crate::show_main_window(app),
            "stop" => {
                if let Some(s) = app.try_state::<AppState>() {
                    s.engine.stop_all();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                crate::show_main_window(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    } else {
        builder = builder.icon(fallback_tray_icon());
    }
    builder.build(app)?;
    Ok(())
}

/// Varsayılan pencere ikonu bulunamazsa bellekten üretilen 32x32 RGBA Ozan (elmas) tepsi simgesi.
fn fallback_tray_icon() -> tauri::image::Image<'static> {
    let size = 32usize;
    let mut rgba = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let dx = (x as i32 - 15).abs();
            let dy = (y as i32 - 15).abs();
            let idx = (y * size + x) * 4;
            if dx + dy <= 12 {
                // Ozan mor/indigo vurgu rengi (#7c6cff)
                rgba[idx] = 124;
                rgba[idx + 1] = 108;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, size as u32, size as u32)
}
