#![windows_subsystem = "windows"]

mod ai;
mod app;
mod autostart;
mod dup;
mod everything;
mod hashcache;
mod i18n;
mod mcp;
mod tree;
mod sunburst;
mod treemap;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod ui_tests;

use std::sync::mpsc::channel;

use eframe::egui;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

use app::TrayCmd;

const TITLE: &str = "WizEverything";

/// Icon: an Everything-style magnifying glass whose lens ring is split into
/// three colors (blue / red / green), rendered with 4×4 supersampling.
pub fn icon_rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    const RING: [[f32; 3]; 3] = [[0.0, 92.0, 255.0], [255.0, 40.0, 40.0], [40.0, 200.0, 40.0]];
    // Lens up-right, handle down-left (tilted to the right, like Everything's icon).
    let (cx, cy) = (0.59f32, 0.41f32);
    let (r_out, r_in) = (0.36f32, 0.235f32);
    let (h0, h1, hw) = ((0.36f32, 0.64f32), (0.07f32, 0.93f32), 0.075f32);

    let sample = |x: f32, y: f32| -> [f32; 4] {
        // Handle: rounded segment.
        let (dx, dy) = (h1.0 - h0.0, h1.1 - h0.1);
        let t = (((x - h0.0) * dx + (y - h0.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        let (px, py) = (h0.0 + t * dx, h0.1 + t * dy);
        let dh = ((x - px).powi(2) + (y - py).powi(2)).sqrt();
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        if d <= r_out + 0.025 && d >= r_in - 0.025 {
            // Dark outline around the ring for contrast on any background.
            if d > r_out || d < r_in {
                return [25.0, 25.0, 25.0, 255.0];
            }
            let ang = (y - cy).atan2(x - cx).to_degrees(); // -180..180
            let a = (ang + 90.0 + 360.0) % 360.0; // 0 at top, clockwise
            let c = RING[(a / 120.0) as usize % 3];
            // Light from top-left gives the ring some depth.
            let shade = 1.0 + 0.25 * (-(x - cx) - (y - cy)) / r_out;
            return [c[0] * shade, c[1] * shade, c[2] * shade, 255.0];
        }
        if d < r_in - 0.025 {
            // Glass: light tint with a highlight in the upper-left.
            let hl = ((x - (cx - 0.08)).powi(2) + (y - (cy - 0.08)).powi(2)).sqrt();
            let g = if hl < 0.08 { 255.0 } else { 225.0 };
            return [g, g + 5.0_f32.min(255.0 - g), 255.0, 235.0];
        }
        if dh <= hw {
            let edge = dh > hw - 0.022;
            let v = if edge { 25.0 } else { 70.0 + 50.0 * (1.0 - t) };
            return [v, v, v, 255.0];
        }
        [0.0, 0.0, 0.0, 0.0]
    };

    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    let s = sample(x, y);
                    let a = s[3] / 255.0;
                    acc[0] += s[0].min(255.0) * a;
                    acc[1] += s[1].min(255.0) * a;
                    acc[2] += s[2].min(255.0) * a;
                    acc[3] += a;
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let (r, g, b) = if acc[3] > 0.0 { (acc[0] / acc[3], acc[1] / acc[3], acc[2] / acc[3]) } else { (0.0, 0.0, 0.0) };
            out.extend_from_slice(&[r as u8, g as u8, b as u8, (a * 255.0).round() as u8]);
        }
    }
    out
}

/// If another instance is already running, bring it to the front and return true.
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
    if std::env::args().any(|a| a == "--write-icon") {
        // Dev helper: dump the icon as raw RGBA for previews.
        let _ = std::fs::write("icon256.rgba", icon_rgba(256));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--mcp") {
        // MCP server on stdio for AI assistants; runs alongside the desktop app.
        i18n::set_lang(i18n::Lang::En);
        mcp::serve();
        return Ok(());
    }
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--ask") {
        // Command-line question: wizeverything.exe --ask "¿Qué ocupa más en C:?"
        let question = args.get(i + 1).cloned().unwrap_or_default();
        let settings = i18n::Settings::load();
        i18n::set_lang(settings.lang);
        let Some(key) = ai::unprotect(&settings.groq_key_enc) else {
            println!("No Groq key saved (Options → Enter Groq key).");
            return Ok(());
        };
        let groq = ai::Groq::new(key);
        let excluded = settings.excluded_drives.clone();
        let mut server = mcp::Server::new(Box::new(move || {
            tree::load(std::sync::Arc::new(tree::Progress::default()), &excluded).map(std::sync::Arc::new)
        }));
        let spanish = settings.lang == i18n::Lang::Es;
        match ai::ask_auto(&groq, None, &question, &mut server, spanish, &|m: &str| eprintln!("[trying {m}]")) {
            Ok((model, answer)) => {
                let q = ai::quota(&model);
                println!("[{model} · requests left today: {:?} · tokens left this minute: {:?}]\n{answer}", q.and_then(|q| q.req_left), q.and_then(|q| q.tok_left));
            }
            Err(e) => println!("ERROR: {e}"),
        }
        return Ok(());
    }
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
            .with_maximized(true)
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

            // System tray
            use i18n::tr;
            let menu = Menu::new();
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
            app.tray_icon = tray;
            Ok(Box::new(app))
        }),
    )
}
