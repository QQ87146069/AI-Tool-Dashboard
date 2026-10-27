//! Opt-in renderer/callback verification. No Windows input injection or client mutations.
use super::*;
use slint::Model;
use std::{fs::File, io::BufWriter};

pub fn configure_taskbar_identity() {
    #[cfg(target_os = "windows")]
    if std::env::var("AI_DASHBOARD_VERIFY_MODE").as_deref() == Ok("taskbar") {
        #[link(name = "shell32")]
        unsafe extern "system" {
            fn SetCurrentProcessExplicitAppUserModelID(id: *const u16) -> i32;
        }
        let id: Vec<u16> = format!("AI.Tool.Dashboard.Verification.{}", std::process::id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // Isolate this test's actual taskbar button from the user's live dashboard.
        unsafe {
            SetCurrentProcessExplicitAppUserModelID(id.as_ptr());
        }
    }
}

pub fn install(
    ui: &Dashboard,
    tray: &DashboardTray,
    startup: Option<&startup::Startup>,
) -> Option<slint::Timer> {
    let directory = PathBuf::from(std::env::var_os("AI_DASHBOARD_VERIFY_DIR")?);
    std::fs::create_dir_all(&directory).ok()?;
    let renderer_path = directory.join("renderer.json");
    let requested = std::env::var("AI_DASHBOARD_RENDERER").unwrap_or_else(|_| "software".into());
    let recorded = Rc::new(RefCell::new(false));
    let notifier_path = renderer_path.clone();
    let notifier_requested = requested.clone();
    let notifier = ui.window().set_rendering_notifier(move |_, api| {
        if !*recorded.borrow() {
            let data = serde_json::json!({"requested": notifier_requested, "graphicsApi": format!("{api:?}")});
            let _ = std::fs::write(&notifier_path, data.to_string());
            *recorded.borrow_mut() = true;
        }
    });
    if let Err(error) = notifier {
        let data =
            serde_json::json!({"requested": requested, "notifierError": format!("{error:?}")});
        let _ = std::fs::write(renderer_path, data.to_string());
    }
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    let mode = std::env::var("AI_DASHBOARD_VERIFY_MODE").unwrap_or_else(|_| "snapshots".into());
    if mode == "taskbar" {
        let weak_tray = tray.as_weak();
        let mut step = 0;
        let mut report = Vec::new();
        let mut pending = None::<String>;
        let mut wait_ticks = 0;
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(600), move || {
            let (Some(ui), Some(tray)) = (weak.upgrade(), weak_tray.upgrade()) else { return; };
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                if let Some(name) = &pending {
                    if std::fs::read_to_string(directory.join("taskbar-ack.txt")).ok().as_deref() != Some(name.as_str()) {
                        wait_ticks += 1;
                        if wait_ticks > 40 { return Err("taskbar observer did not acknowledge stage".into()); }
                        return Ok(());
                    }
                    pending = None;
                    wait_ticks = 0;
                }
                let stage = match step { 1 => Some("initial"), 3 => Some("compact"), 5 => Some("full"),
                    7 => Some("compact-again"), 9 => Some("hidden"), 11 => Some("tray-restored"),
                    13 => Some("settings"), 15 => Some("compact-final"), _ => None };
                if let Some(name) = stage {
                    if name != "hidden" { capture(&ui, &directory, name, &mut report)?; }
                    std::fs::write(directory.join("taskbar-stage.json"), serde_json::to_vec(&serde_json::json!({
                        "name":name,"pid":std::process::id(),"compact":ui.get_compact(),"visible":ui.window().is_visible()
                    }))?)?;
                    pending = Some(name.into());
                    step += 1;
                    return Ok(());
                }
                match step {
                    0 => {
                        if ui.window().is_visible() { return Err("background launch unexpectedly visible".into()); }
                        report.push(serde_json::json!({"initialCompact":ui.get_compact(),"backgroundInitiallyHidden":true}));
                        tray.invoke_show_dashboard();
                        ui.window().with_winit_window(|window| window.set_title(&format!("AI Tool Dashboard Verification {}", std::process::id())));
                    }
                    2 => ui.set_compact(true),
                    4 => ui.set_compact(false),
                    6 => ui.set_compact(true),
                    8 => tray.invoke_hide_dashboard(),
                    10 => {
                        if ui.window().is_visible() { return Err("tray hide failed".into()); }
                        report.push(serde_json::json!({"trayHidden":true}));
                        tray.invoke_show_dashboard();
                    }
                    12 => {
                        if !ui.get_compact() { return Err("tray restore lost compact mode".into()); }
                        tray.invoke_show_settings();
                    }
                    14 => {
                        if ui.get_compact() || !ui.get_settings_open() { return Err("settings did not restore full mode".into()); }
                        ui.set_settings_open(false);
                        ui.set_compact(true);
                    }
                    16 => {
                        std::fs::write(directory.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
                        ui.invoke_quit();
                    }
                    _ => (),
                }
                Ok(())
            })();
            if let Err(error) = result {
                let _ = std::fs::write(directory.join("error.txt"), error.to_string());
                ui.invoke_quit();
            }
            if pending.is_none() { step += 1; }
        });
        return Some(timer);
    }
    if mode == "tray" || mode == "tray-startup" {
        let weak_tray = tray.as_weak();
        let startup_was_enabled = startup.and_then(|s| s.enabled().ok());
        let mut step = 0;
        let mut report = Vec::new();
        let mut hidden_checks = 0;
        let mut checked_before_hide = String::new();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(400), move || {
            let (Some(ui), Some(tray)) = (weak.upgrade(), weak_tray.upgrade()) else { return; };
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                match step {
                    0 => {
                        report.push(serde_json::json!({"initialWindowVisible": ui.window().is_visible(), "trayComponentCreated": true, "autostartInitially": startup_was_enabled}));
                        tray.invoke_show_dashboard();
                        ui.set_compact(true);
                        ui.window().with_winit_window(|window| {
                            if let Some(monitor) = window.current_monitor() {
                                let origin = monitor.position(); let size = monitor.size();
                                window.set_outer_position(winit::dpi::PhysicalPosition::new(
                                    origin.x + size.width as i32 - 300,
                                    origin.y + size.height as i32 - 70,
                                ));
                            }
                        });
                        let delayed = tray.as_weak();
                        slint::Timer::single_shot(Duration::from_millis(100), move || {
                            if let Some(tray) = delayed.upgrade() { tray.invoke_show_settings(); }
                        });
                    }
                    1 => {
                        if !ui.window().is_visible() || !ui.get_settings_open() { return Err("tray did not restore settings window".into()); }
                        let fits = ui.window().with_winit_window(|window| {
                            let Ok(position) = window.outer_position() else { return false; };
                            let size = window.outer_size();
                            window.available_monitors().any(|monitor| {
                                let origin = monitor.position(); let screen = monitor.size();
                                position.x >= origin.x && position.y >= origin.y
                                    && position.x + size.width as i32 <= origin.x + screen.width as i32
                                    && position.y + size.height as i32 <= origin.y + screen.height as i32
                            })
                        }).unwrap_or(false);
                        if !fits { return Err("expanded settings window extends outside screen".into()); }
                        report.push(serde_json::json!({"settingsFitsScreen": true}));
                        capture(&ui, &directory, "settings", &mut report)?;
                        // Only register temporarily when no matching entry was enabled. The outer
                        // verification runner checks that this app's startup slot is absent first.
                        if mode == "tray-startup" {
                            if startup_was_enabled != Some(false) { return Err("startup test requires a disabled entry".into()); }
                            tray.invoke_autostart_changed(true);
                            if !ui.get_autostart() || !tray.get_autostart() { return Err("startup enable failed".into()); }
                            report.push(serde_json::json!({"startupEnabled": true}));
                            tray.invoke_autostart_changed(false);
                            if ui.get_autostart() || tray.get_autostart() { return Err("startup disable failed".into()); }
                            report.push(serde_json::json!({"startupDisabled": true}));
                        }
                        ui.invoke_background_changed(true);
                        if !ui.get_background_enabled() || !tray.get_background_enabled() { return Err("background setting failed".into()); }
                        checked_before_hide = ui.get_checked().to_string();
                        ui.invoke_close_window();
                    }
                    2 => {
                        if ui.window().is_visible() { return Err("close did not hide window".into()); }
                        hidden_checks += 1;
                        if hidden_checks < 10 || ui.get_checked().as_str() == checked_before_hide {
                            if hidden_checks >= 50 { return Err("no background source update observed".into()); }
                            return Ok(());
                        }
                        report.push(serde_json::json!({"closeHidden": true, "stillRunning": true, "backgroundUpdated": true, "hiddenCheckMillis": hidden_checks * 400}));
                        tray.invoke_show_dashboard();
                    }
                    3 => {
                        if !ui.window().is_visible() { return Err("tray show failed".into()); }
                        report.push(serde_json::json!({"trayRestored": true}));
                        tray.invoke_background_changed(false);
                        if ui.get_background_enabled() || tray.get_background_enabled() { return Err("background disable failed".into()); }
                        tray.invoke_hide_dashboard();
                    }
                    4 => {
                        if ui.window().is_visible() { return Err("explicit tray hide failed".into()); }
                        tray.invoke_show_dashboard();
                    }
                    5 => {
                        ui.set_settings_open(false);
                        ui.set_compact(true);
                    }
                    6 => {
                        capture(&ui, &directory, "compact", &mut report)?;
                        std::fs::write(directory.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
                        if mode == "tray" { ui.invoke_close_window(); }
                        else { tray.invoke_quit(); }
                    }
                    _ => (),
                }
                step += 1;
                Ok(())
            })();
            if let Err(error) = result {
                if mode == "tray-startup" { ui.invoke_autostart_changed(false); }
                let _ = std::fs::write(directory.join("error.txt"), error.to_string());
                ui.invoke_quit();
            }
        });
        return Some(timer);
    }
    if mode == "activity-expiry" || mode == "zcode-expiry" {
        let mut armed = false;
        let mut expected = 0;
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
            let Some(ui) = weak.upgrade() else { return; };
            if ui.get_refreshing() { return; }
            if !armed && mode == "zcode-expiry" && !ui.get_tools().iter().any(|row| row.name == "ZCode" && row.activity_expires_ms > 0.0) { return; }
            let result = (|| -> Result<(), Box<dyn std::error::Error>> {
                let tools = ui.get_tools();
                if !armed {
                    let mut count = 0;
                    for index in 0..tools.row_count() {
                        if let Some(mut row) = tools.row_data(index) && row.activity_expires_ms > 0.0 {
                            row.activity_expires_ms = 50.0;
                            tools.set_row_data(index, row);
                            count += 1;
                        }
                    }
                    if count == 0 { return Err("no real activity observation available".into()); }
                    expected = count;
                    ui.set_compact(true);
                    armed = true;
                } else {
                    let expired = tools.iter().filter(|row| row.activity == "—" && row.activity_detail.contains("过期")).count();
                    if expired != expected { return Err("activity expiry callback did not invalidate all counts".into()); }
                    std::fs::write(directory.join("report.json"), serde_json::to_vec_pretty(&serde_json::json!({"activityExpiryVerified": true, "expired": expired}))?)?;
                    ui.invoke_quit();
                }
                Ok(())
            })();
            if let Err(error) = result {
                let _ = std::fs::write(directory.join("error.txt"), error.to_string());
                ui.invoke_quit();
            }
        });
        return Some(timer);
    }
    if mode == "minimized" || mode == "updates" {
        let timer_mode = if mode == "minimized" {
            slint::TimerMode::SingleShot
        } else {
            slint::TimerMode::Repeated
        };
        timer.start(timer_mode, Duration::from_secs(1), move || {
            if let Some(ui) = weak.upgrade() {
                if mode == "minimized" {
                    ui.invoke_minimize();
                } else {
                    ui.invoke_refresh();
                }
            }
        });
        return Some(timer);
    }
    let mut step = 0;
    let mut report = Vec::new();
    let mut expected_expired = 0;
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(600), move || {
        let Some(ui) = weak.upgrade() else { return };
        if step == 0 && ui.get_refreshing() { return; }
        if step == 0 && mode == "zcode" && !ui.get_tools().iter().any(|row| row.name == "ZCode" && row.activity_expires_ms > 0.0) { return; }
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            match step {
                0 => {
                    report.push(serde_json::json!({"initialLight": ui.get_light(), "initialCompact": ui.get_compact(), "initialPinned": ui.get_pinned(), "tools": ui.get_tools().row_count()}));
                    ui.window().with_winit_window(|window| {
                        report.push(serde_json::json!({"nativeWindow": true, "decorated": window.is_decorated()}));
                    });
                    capture(&ui, &directory, "initial", &mut report)?;
                    ui.set_light(false);
                    ui.set_compact(false);
                }
                1 => {
                    capture(&ui, &directory, "dark", &mut report)?;
                    if mode == "expiry" {
                        let tools = ui.get_tools();
                        for index in 0..tools.row_count() {
                            if let Some(mut row) = tools.row_data(index) && row.fresh {
                                expected_expired += 1;
                                row.expires_ms = 50.0;
                                tools.set_row_data(index, row);
                            }
                        }
                        ui.set_compact(true);
                    }
                    ui.set_light(true);
                    if mode != "expiry" { ui.invoke_refresh(); }
                }
                2 => {
                    if mode == "expiry" {
                        let expired = ui.get_tools().iter().filter(|row| row.status == "过期" && !row.fresh).count();
                        if expected_expired == 0 || expired != expected_expired { return Err("quota expiry callbacks did not invalidate all fresh rows".into()); }
                        report.push(serde_json::json!({"expiryCallbacks": expired, "expiryVerified": true}));
                    }
                    capture(&ui, &directory, "light", &mut report)?;
                    ui.set_compact(true);
                }
                3 => {
                    capture(&ui, &directory, "compact", &mut report)?;
                    ui.set_light(false);
                    capture(&ui, &directory, "compact-dark", &mut report)?;
                    ui.set_pinned(false);
                    ui.invoke_pin_changed(false);
                    ui.window().with_winit_window(|window| {
                        if let Some(monitor) = window.primary_monitor() {
                            let origin = monitor.position();
                            window.set_outer_position(winit::dpi::PhysicalPosition::new(origin.x + 60, origin.y + 60));
                        }
                    });
                    ui.invoke_minimize();
                }
                4 => {
                    ui.window().with_winit_window(|window| {
                        report.push(serde_json::json!({"minimized": window.is_minimized(), "position": window.outer_position().ok().map(|p| (p.x, p.y))}));
                        window.set_minimized(false);
                    });
                }
                5 => {
                    ui.window().with_winit_window(|window| report.push(serde_json::json!({"restored": window.is_minimized() == Some(false)})));
                    capture(&ui, &directory, "restored", &mut report)?;
                    ui.set_compact(false);
                }
                6 => {
                    capture(&ui, &directory, "expanded", &mut report)?;
                    std::fs::write(directory.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
                    ui.invoke_quit();
                }
                _ => (),
            }
            Ok(())
        })();
        if let Err(error) = result {
            // Errors originate only from this app's rendering/output paths.
            let _ = std::fs::write(directory.join("error.txt"), error.to_string());
            ui.invoke_quit();
        }
        step += 1;
    });
    Some(timer)
}

fn capture(
    ui: &Dashboard,
    directory: &std::path::Path,
    name: &str,
    report: &mut Vec<serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    let pixels = ui.window().take_snapshot()?;
    if !ui.window().is_visible() {
        return Err("snapshot window is hidden".into());
    }
    let nonblank = pixels.as_slice().windows(2).any(|pair| pair[0] != pair[1]);
    if !nonblank {
        return Err("renderer produced a blank snapshot".into());
    }
    let mut encoder = png::Encoder::new(
        BufWriter::new(File::create(directory.join(format!("{name}.png")))?),
        pixels.width(),
        pixels.height(),
    );
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(pixels.as_bytes())?;
    let activity: Vec<_> = ui.get_tools().iter().map(|row| serde_json::json!({"tool": row.name.as_str(), "count": row.activity.as_str(), "detail": row.activity_detail.as_str()})).collect();
    report.push(serde_json::json!({"snapshot": name, "width": pixels.width(), "height": pixels.height(), "nonblank": nonblank, "activity": activity, "compact": ui.get_compact(), "visible": ui.window().is_visible(), "windowsSkipTaskbarRequested": ui.get_compact()}));
    Ok(())
}
