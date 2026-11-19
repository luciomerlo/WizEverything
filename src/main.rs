#![windows_subsystem = "windows"]

mod app;
mod autostart;
mod everything;
mod i18n;
mod tree;
mod treemap;

use std::sync::mpsc::channel;

use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use app::TrayCmd;

const TITLE: &str = "WizEverything";

/// Ícono generado: un mini treemap de 4 colores.
fn icon_rgba(size: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((size * size * 4) as usize);
    let s = size as f32;
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 / s, y as f32 / s);
            let border = x == 0 || y == 0 || x == size - 1 || y == size - 1;
            let c: [u8; 3] = if border {
                [20, 20, 20]
            } else if fx < 0.58 {
                [0, 92, 255]
            } else if fy < 0.55 {
                [255, 40, 40]
            } else if fx < 0.8 {
                [40, 200, 40]
            } else {
                [255, 210, 0]
            };
            // líneas divisorias
            let line = (fx - 0.58).abs() < 1.0 / s
                || (fx > 0.58 && (fy - 0.55).abs() < 1.0 / s)
                || (fx > 0.58 && fy > 0.55 && (fx - 0.8).abs() < 1.0 / s);
            let c = if line { [20, 20, 20] } else { c };
            v.extend_from_slice(&[c[0], c[1], c[2], 255]);
        }
    }
    v
}

/// Si ya hay una instancia corriendo, la muestra y devuelve true.
fn activate_existing_instance() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow};
    let name = everything::wide("Local\\WizEverything.SingleInstance");
    unsafe {
        CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return false;
        }
        let title = everything::wide(TITLE);
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if !hwnd.is_null() {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_RESTORE);
            SetForegroundWindow(hwnd);
        }
    }
    true
}

fn main() -> eframe::Result {
    let background = std::env::args().any(|a| a == "--background");
    if activate_existing_instance() {
        return Ok(());
    }
    let settings = i18n::Settings::load();
    i18n::set_lang(settings.lang);

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([1280.0, 860.0])
            .with_min_inner_size([640.0, 420.0])
            .with_visible(!background)
            .with_icon(egui::IconData { rgba: icon_rgba(64), width: 64, height: 64 }),
        ..Default::default()
    };

    eframe::run_native(
        TITLE,
        native,
        Box::new(move |cc| {
            let (tx, rx) = channel::<TrayCmd>();

            // Bandeja del sistema
            let menu = Menu::new();
            use i18n::tr;
            let open = MenuItem::new(tr("Abrir", "Open"), true, None);
            let refresh = MenuItem::new(tr("Actualizar", "Refresh"), true, None);
            let auto = CheckMenuItem::new(tr("Iniciar con Windows", "Start with Windows"), true, autostart::is_enabled(), None);
            let quit = MenuItem::new(tr("Salir", "Quit"), true, None);
            let _ = menu.append_items(&[&open, &refresh, &PredefinedMenuItem::separator(), &auto, &PredefinedMenuItem::separator(), &quit]);
            let tray = TrayIconBuilder::new()
                .with_tooltip(TITLE)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_icon(tray_icon::Icon::from_rgba(icon_rgba(32), 32, 32).expect("icon"))
                .build()
                .ok();
            let has_tray = tray.is_some();
            // mantener vivo el ícono durante toda la ejecución
            std::mem::forget(tray);

            let (open_id, refresh_id, auto_id, quit_id) = (open.id().clone(), refresh.id().clone(), auto.id().clone(), quit.id().clone());
            let ctx = cc.egui_ctx.clone();
            let tx2 = tx.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                let cmd = if e.id == open_id {
                    TrayCmd::Show
                } else if e.id == refresh_id {
                    TrayCmd::Refresh
                } else if e.id == auto_id {
                    TrayCmd::ToggleAutostart
                } else if e.id == quit_id {
                    TrayCmd::Quit
                } else {
                    return;
                };
                let _ = tx2.send(cmd);
                ctx.request_repaint();
            }));
            let ctx = cc.egui_ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                let show = matches!(
                    e,
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
                );
                if show {
                    let _ = tx.send(TrayCmd::Show);
                    ctx.request_repaint();
                }
            }));

            let mut app = app::App::new(&cc.egui_ctx, rx, has_tray, background, settings);
            app.tray_items = Some(app::TrayItems { open, refresh, auto, quit });
            Ok(Box::new(app))
        }),
    )
}
