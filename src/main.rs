#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use ai_tool_dashboard::{
    adapters,
    model::{Freshness, Reading, SourceState, ToolSnapshot},
};
use chrono::{DateTime, Local};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use slint::{
    ComponentHandle, Model, ModelRc, VecModel,
    winit_030::{WinitWindowAccessor, winit},
};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime},
};

slint::include_modules!();
mod startup;
mod startup_log;

#[cfg(feature = "verification")]
mod verification;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct Preferences {
    light: bool,
    compact: bool,
    pinned: Option<bool>,
    position: Option<(i32, i32)>,
    background: Option<bool>,
}

#[derive(Default)]
struct Options {
    background: bool,
    state: Option<PathBuf>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self, &'static str> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if arg == "--background" {
                options.background = true;
            } else if arg == "--state" {
                options.state = Some(
                    args.next()
                        .filter(|value| !value.is_empty())
                        .ok_or("--state requires a path")?
                        .into(),
                );
            } else {
                return Err("unknown command line argument");
            }
        }
        Ok(options)
    }
}

enum Request {
    Refresh,
    Stop,
}

fn state_path() -> Option<PathBuf> {
    std::env::var_os("AI_DASHBOARD_STATE")
        .map(PathBuf::from)
        .or_else(|| {
            ProjectDirs::from("", "AI Tool Dashboard", "AI Tool Dashboard")
                .map(|dirs| dirs.config_dir().join("window.json"))
        })
}

fn save_preferences(
    ui: &Dashboard,
    path: &Option<PathBuf>,
    position: Option<(i32, i32)>,
) -> std::io::Result<()> {
    let path = path.as_ref().ok_or(std::io::ErrorKind::NotFound)?;
    let preferences = Preferences {
        light: ui.get_light(),
        compact: ui.get_compact(),
        pinned: Some(ui.get_pinned()),
        position,
        background: Some(ui.get_background_enabled()),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&preferences).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)
}

fn show_dashboard(ui: &Dashboard, position: Option<(i32, i32)>) {
    if ui.show().is_err() {
        return;
    }
    sync_window_style(ui);
    ui.window().with_winit_window(|window| {
        window.set_minimized(false);
        window.set_window_level(if ui.get_pinned() {
            winit::window::WindowLevel::AlwaysOnTop
        } else {
            winit::window::WindowLevel::Normal
        });
        if let Some((x, y)) = position {
            let visible = window.available_monitors().any(|monitor| {
                let origin = monitor.position();
                let size = monitor.size();
                x >= origin.x
                    && y >= origin.y
                    && x + window.outer_size().width as i32 <= origin.x + size.width as i32
                    && y + 64 <= origin.y + size.height as i32
            });
            if visible {
                window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
            }
        }
        window.focus_window();
        fit_window(window);
    });
}

fn sync_window_style(ui: &Dashboard) {
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowExtWindows;
        ui.window().with_winit_window(|window| {
            window.set_skip_taskbar(ui.get_compact());
            window.set_corner_preference(winit::platform::windows::CornerPreference::Round);
        });
    }
    #[cfg(not(target_os = "windows"))]
    let _ = ui;
}

// Expanding a small widget near the screen edge must keep the settings reachable.
fn fit_window(window: &winit::window::Window) {
    let (Ok(position), Some(monitor)) = (
        window.outer_position(),
        window
            .current_monitor()
            .or_else(|| window.primary_monitor()),
    ) else {
        return;
    };
    let origin = monitor.position();
    let screen = monitor.size();
    let size = window.outer_size();
    if size.width == 0 || size.height == 0 {
        return;
    }
    let x = position.x.clamp(
        origin.x,
        origin.x + screen.width.saturating_sub(size.width) as i32,
    );
    let y = position.y.clamp(
        origin.y,
        origin.y + screen.height.saturating_sub(size.height) as i32,
    );
    if (x, y) != (position.x, position.y) {
        window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
    }
}

fn status(freshness: Freshness) -> &'static str {
    match freshness {
        Freshness::Unknown => "未知",
        Freshness::Unsupported => "不支持",
        Freshness::Failed => "读取失败",
        Freshness::Fresh => "已更新",
        Freshness::Stale => "过期",
    }
}

fn card(snapshot: ToolSnapshot) -> ToolCard {
    let now = SystemTime::now();
    let freshness = snapshot.quota.freshness(now);
    let mut quotas = vec![];
    let mut summary = vec![];
    let mut expires_ms = 0.0;
    if let Reading::Observed {
        value,
        at,
        valid_for,
    } = &snapshot.quota
    {
        expires_ms = at
            .checked_add(*valid_for)
            .and_then(|expiry| expiry.duration_since(now).ok())
            .map(|delay| delay.as_millis().max(1) as f32)
            .unwrap_or_default();
        for quota in value {
            let duration = quota
                .window_minutes
                .map(|mins| {
                    if mins % 1440 == 0 {
                        format!("{} 天", mins / 1440)
                    } else if mins % 60 == 0 {
                        format!("{} 小时", mins / 60)
                    } else {
                        format!("{mins} 分钟")
                    }
                })
                .unwrap_or_else(|| "窗口未知".into());
            // Sources retain their native used-percent semantics; the UI shows remaining quota.
            let remaining = quota.used_percent.map(|used| 100.0 - used);
            summary.push(format!(
                "{} {}",
                duration
                    .replace(" 小时", "h")
                    .replace(" 天", "d")
                    .replace(" 分钟", "m"),
                remaining
                    .map(|percent| format!("{percent:.0}%"))
                    .unwrap_or_else(|| "—".into())
            ));
            quotas.push(QuotaRow {
                label: format!("{} · {}额度剩余", quota.bucket, duration).into(),
                value: remaining
                    .map(|percent| format!("{percent:.0}%"))
                    .unwrap_or_else(|| "—".into())
                    .into(),
                detail: format!(
                    "{} {} · {}",
                    status(freshness),
                    DateTime::<Local>::from(*at).format("%H:%M:%S"),
                    quota
                        .resets_at
                        .and_then(|time| i64::try_from(time).ok())
                        .and_then(|time| DateTime::from_timestamp(time, 0))
                        .map(|time| format!(
                            "{} 重置",
                            time.with_timezone(&Local).format("%m-%d %H:%M")
                        ))
                        .unwrap_or_else(|| "重置时间未知".into())
                )
                .into(),
                percent: remaining.unwrap_or(0.0) as f32,
                measured: remaining.is_some(),
            });
        }
    }
    if quotas.is_empty() {
        quotas.push(QuotaRow {
            label: "账号额度 · 剩余".into(),
            value: "—".into(),
            detail: "未取得有效额度 · 窗口及重置时间未知".into(),
            percent: 0.0,
            measured: false,
        });
    }
    let mut activity_expires_ms = 0.0;
    let (activity, activity_detail) = match &snapshot.activity {
        Reading::Observed {
            value,
            at,
            valid_for,
        } if snapshot.activity.freshness(now) == Freshness::Fresh => {
            activity_expires_ms = at
                .checked_add(*valid_for)
                .and_then(|expiry| expiry.duration_since(now).ok())
                .map(|delay| delay.as_millis().max(1) as f32)
                .unwrap_or_default();
            (
                if value.lower_bound && value.running > 0 {
                    format!("≥{}", value.running)
                } else {
                    value.running.to_string()
                },
                if value.lower_bound && value.running == 0 {
                    "已观测任务均已结束 · 覆盖有限".into()
                } else if value.lower_bound {
                    "本机已核实下限 · 等待数未知".into()
                } else {
                    value
                        .waiting
                        .map(|count| format!("{count} 个等待输入"))
                        .unwrap_or_else(|| "本机执行任务".into())
                },
            )
        }
        reading => (
            "—".into(),
            format!("{} · 未取得实时执行状态", status(reading.freshness(now))),
        ),
    };
    let summary = if summary.is_empty() {
        format!("剩余{}", status(freshness))
    } else {
        format!("余 {}", summary.join(" / "))
    };
    ToolCard {
        name: snapshot.name.into(),
        monogram: snapshot
            .name
            .chars()
            .next()
            .unwrap_or('?')
            .to_string()
            .into(),
        accent: if snapshot.id == "codex" {
            slint::Color::from_rgb_u8(119, 210, 183)
        } else {
            slint::Color::from_rgb_u8(164, 163, 246)
        },
        source: if !snapshot.quota_note.is_empty() {
            snapshot.quota_note
        } else {
            match snapshot.source {
                SourceState::Found => "已发现本地来源 · 指标待验证",
                SourceState::Missing => "未发现本地来源 · 客户端状态未知",
                SourceState::Unreadable => "本地来源读取失败 · 客户端状态未知",
            }
        }
        .into(),
        status: status(freshness).into(),
        activity: activity.into(),
        activity_detail: activity_detail.into(),
        activity_expires_ms,
        quotas: ModelRc::new(VecModel::from(quotas)),
        fresh: freshness == Freshness::Fresh,
        expires_ms,
        summary: summary.into(),
    }
}

fn expire_card(mut row: ToolCard) -> ToolCard {
    row.fresh = false;
    row.expires_ms = 0.0;
    row.status = "过期".into();
    row.quotas = ModelRc::new(VecModel::from(
        row.quotas
            .iter()
            .map(|mut quota| {
                quota.detail = quota.detail.replace("已更新", "过期").into();
                quota
            })
            .collect::<Vec<_>>(),
    ));
    row.summary = format!("过期 · {}", row.summary).into();
    row
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse(std::env::args_os().skip(1))?;
    let path = options
        .state
        .clone()
        .or_else(state_path)
        .map(std::path::absolute)
        .transpose()?;
    let log = startup_log::StartupLog::new(path.as_deref());
    log.record(
        if options.background {
            "launch-background"
        } else {
            "launch-visible"
        },
        None,
    );
    let result = run(options, path, &log);
    match &result {
        Ok(()) => log.record("exited", None),
        Err(error) => log.record("startup-or-event-loop-error", Some(&error.to_string())),
    }
    result
}

fn run(
    options: Options,
    path: Option<PathBuf>,
    log: &startup_log::StartupLog,
) -> Result<(), Box<dyn std::error::Error>> {
    let renderer = std::env::var("AI_DASHBOARD_RENDERER").unwrap_or_else(|_| "software".into());
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer)
        .select()?;
    let preferences = path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<Preferences>(&bytes).ok())
        .unwrap_or_default();
    #[cfg(feature = "verification")]
    verification::configure_taskbar_identity();
    let ui = Dashboard::new()?;
    let tray = DashboardTray::new()?;
    let startup = path
        .as_deref()
        .and_then(|path| startup::Startup::new(path).ok())
        .map(Rc::new);
    match startup.as_ref().map(|startup| startup.enabled()) {
        Some(Ok(enabled)) => {
            ui.set_autostart(enabled);
            tray.set_autostart(enabled);
        }
        _ => ui.set_settings_status("暂时无法读取开机启动设置。".into()),
    }
    let weak = ui.as_weak();
    ui.on_quota_expired(move |index| {
        if let Some(ui) = weak.upgrade()
            && let Ok(index) = usize::try_from(index)
            && let Some(row) = ui.get_tools().row_data(index)
            && row.fresh
        {
            ui.get_tools().set_row_data(index, expire_card(row));
        }
    });
    let weak = ui.as_weak();
    ui.on_activity_expired(move |index| {
        if let Some(ui) = weak.upgrade()
            && let Ok(index) = usize::try_from(index)
            && let Some(mut row) = ui.get_tools().row_data(index)
        {
            row.activity = "—".into();
            row.activity_detail = "过期 · 等待执行状态更新".into();
            row.activity_expires_ms = 0.0;
            ui.get_tools().set_row_data(index, row);
        }
    });
    ui.set_light(preferences.light);
    ui.set_compact(preferences.compact);
    ui.set_pinned(preferences.pinned.unwrap_or(true));
    ui.set_background_enabled(preferences.background.unwrap_or(true));
    tray.set_background_enabled(ui.get_background_enabled());
    let weak = ui.as_weak();
    ui.on_display_mode_changed(move || {
        if let Some(ui) = weak.upgrade() {
            sync_window_style(&ui);
        }
    });
    let position = Rc::new(RefCell::new(preferences.position));
    let moved = position.clone();
    let resized_ui = ui.as_weak();
    ui.window().on_winit_window_event(move |window, event| {
        if matches!(event, winit::event::WindowEvent::Resized(_)) {
            window.with_winit_window(|native| {
                if native.is_minimized() != Some(true) {
                    fit_window(native);
                }
            });
            let resized_ui = resized_ui.clone();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(ui) = resized_ui.upgrade() {
                    sync_window_style(&ui);
                }
            });
        }
        if let winit::event::WindowEvent::Moved(point) = event
            && window
                .with_winit_window(|native| native.is_minimized() != Some(true))
                .unwrap_or(false)
        {
            *moved.borrow_mut() = Some((point.x, point.y));
        }
        slint::winit_030::EventResult::Propagate
    });
    let weak = ui.as_weak();
    let save_path = path.clone();
    let saved_position = position.clone();
    ui.on_hide_to_tray(move || {
        if let Some(ui) = weak.upgrade() {
            if save_preferences(&ui, &save_path, *saved_position.borrow()).is_err() {
                ui.set_settings_status("无法保存显示设置，请检查文件权限。".into());
            }
            let _ = ui.hide();
        }
    });
    let weak = ui.as_weak();
    ui.on_close_window(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_background_enabled() {
                ui.invoke_hide_to_tray();
            } else {
                ui.invoke_quit();
            }
        }
    });
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_close_window();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let weak = ui.as_weak();
    let weak_tray = tray.as_weak();
    let startup_setting = startup.clone();
    ui.on_autostart_changed(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            let result = startup_setting
                .as_ref()
                .ok_or(std::io::ErrorKind::NotFound)
                .and_then(|startup| {
                    startup
                        .set_enabled(enabled)
                        .and_then(|_| {
                            if startup.enabled()? == enabled {
                                Ok(())
                            } else {
                                Err(std::io::ErrorKind::Other.into())
                            }
                        })
                        .map_err(|error| error.kind())
                });
            if result.is_ok() {
                ui.set_autostart(enabled);
                if let Some(tray) = weak_tray.upgrade() {
                    tray.set_autostart(enabled);
                }
                ui.set_settings_status(
                    if enabled {
                        "已开启：登录系统后自动在托盘运行。"
                    } else {
                        "已关闭开机启动。"
                    }
                    .into(),
                );
            } else {
                ui.set_settings_status("开机启动设置失败，请检查权限或程序路径。".into());
            }
        }
    });
    let weak = ui.as_weak();
    let weak_tray = tray.as_weak();
    let save_path = path.clone();
    let saved_position = position.clone();
    ui.on_background_changed(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            let previous = ui.get_background_enabled();
            ui.set_background_enabled(enabled);
            if save_preferences(&ui, &save_path, *saved_position.borrow()).is_ok() {
                if let Some(tray) = weak_tray.upgrade() {
                    tray.set_background_enabled(enabled);
                }
                ui.set_settings_status(
                    if enabled {
                        "关闭窗口后继续运行；从托盘菜单退出程序。"
                    } else {
                        "关闭窗口时将退出程序。"
                    }
                    .into(),
                );
            } else {
                ui.set_background_enabled(previous);
                ui.set_settings_status("无法保存后台运行设置，请检查文件权限。".into());
            }
        }
    });
    let weak = ui.as_weak();
    let shown_position = position.clone();
    tray.on_show_dashboard(move || {
        if let Some(ui) = weak.upgrade() {
            let position = *shown_position.borrow();
            show_dashboard(&ui, position);
        }
    });
    let weak = ui.as_weak();
    tray.on_hide_dashboard(move || {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_hide_to_tray();
        }
    });
    let weak = ui.as_weak();
    let shown_position = position.clone();
    tray.on_show_settings(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_compact(false);
            ui.set_settings_open(true);
            let position = *shown_position.borrow();
            show_dashboard(&ui, position);
        }
    });
    let weak = ui.as_weak();
    tray.on_autostart_changed(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_autostart_changed(enabled);
        }
    });
    let weak = ui.as_weak();
    tray.on_background_changed(move |enabled| {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_background_changed(enabled);
        }
    });
    let weak = ui.as_weak();
    tray.on_quit(move || {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_quit();
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|window| {
                let _ = window.drag_window();
            });
        }
    });
    let weak = ui.as_weak();
    ui.on_pin_changed(move |pinned| {
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|window| {
                window.set_window_level(if pinned {
                    winit::window::WindowLevel::AlwaysOnTop
                } else {
                    winit::window::WindowLevel::Normal
                })
            });
        }
    });
    let weak = ui.as_weak();
    ui.on_minimize(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window()
                .with_winit_window(|window| window.set_minimized(true));
        }
    });
    ui.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    let (refresh, requests) = mpsc::sync_channel(1);
    let refresh_ui = refresh.clone();
    let weak = ui.as_weak();
    ui.on_refresh(move || {
        if refresh_ui.try_send(Request::Refresh).is_ok()
            && let Some(ui) = weak.upgrade()
        {
            ui.set_refreshing(true);
        }
    });
    let weak = ui.as_weak();
    let stopping = Arc::new(AtomicBool::new(false));
    let worker_stopping = stopping.clone();
    let worker = std::thread::spawn(move || {
        let mut adapters = adapters::registered();
        'poll: loop {
            let mut snapshots = vec![];
            for adapter in &mut adapters {
                if worker_stopping.load(Ordering::Relaxed) {
                    break 'poll;
                }
                snapshots.push(adapter.probe());
            }
            if worker_stopping.load(Ordering::Relaxed) {
                break;
            }
            if weak
                .upgrade_in_event_loop(move |ui| {
                    let checked = snapshots
                        .iter()
                        .map(|snapshot| snapshot.checked_at)
                        .max()
                        .unwrap_or_else(SystemTime::now);
                    let cards: Vec<_> = snapshots.into_iter().map(card).collect();
                    let extra_rows = cards
                        .iter()
                        .map(|card| card.quotas.row_count().saturating_sub(1))
                        .sum::<usize>()
                        .min(2);
                    ui.set_full_height(568 + extra_rows as i32 * 72);
                    ui.set_tools(ModelRc::new(VecModel::from(cards)));
                    let checked: DateTime<Local> = checked.into();
                    ui.set_checked(
                        format!("状态 3 秒 · 额度 60 秒 · {}", checked.format("%H:%M:%S")).into(),
                    );
                    ui.set_refreshing(false);
                })
                .is_err()
            {
                break;
            }
            match requests.recv_timeout(Duration::from_secs(3)) {
                Ok(Request::Refresh) | Err(mpsc::RecvTimeoutError::Timeout) => (),
                Ok(Request::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    if !options.background {
        show_dashboard(&ui, preferences.position);
    }
    #[cfg(feature = "verification")]
    let _verification = verification::install(&ui, &tray, startup.as_deref());
    let ready_log = log.clone();
    slint::Timer::single_shot(Duration::from_secs(1), move || {
        ready_log.record("event-loop-ready", None)
    });
    slint::run_event_loop()?;
    ui.hide()?;
    tray.hide()?;
    stopping.store(true, Ordering::Relaxed);
    // Explicitly wake the worker even if the backend still owns UI callbacks.
    let _ = refresh.try_send(Request::Stop);
    let _ = worker.join();
    if save_preferences(&ui, &path, *position.borrow()).is_err() {
        eprintln!("无法保存看板窗口设置");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_tool_dashboard::model::{Activity, QuotaWindow};
    use slint::Model;

    #[test]
    fn login_options_preserve_state_path_and_old_preferences_default_to_background() {
        let options =
            Options::parse(["--background", "--state", "C:/My App/window.json"].map(Into::into))
                .unwrap();
        assert!(options.background);
        assert_eq!(options.state, Some(PathBuf::from("C:/My App/window.json")));
        assert!(Options::parse(["--state"].map(Into::into)).is_err());
        assert!(Options::parse(["--unknown"].map(Into::into)).is_err());
        let old: Preferences = serde_json::from_str(r#"{"light":true,"compact":true}"#).unwrap();
        assert!(old.light && old.compact && old.background.unwrap_or(true));
        let current: Preferences = serde_json::from_str(r#"{"background":false}"#).unwrap();
        assert_eq!(current.background, Some(false));
    }

    fn snapshot() -> ToolSnapshot {
        ToolSnapshot {
            id: "test",
            name: "Test",
            source: SourceState::Found,
            quota: Reading::Unknown,
            activity: Reading::Unknown,
            checked_at: SystemTime::now(),
            quota_note: "",
        }
    }

    #[test]
    fn source_discovery_renders_unknown_not_zero_or_online() {
        let view = card(snapshot());
        assert_eq!(view.status, "未知");
        assert_eq!(view.activity, "—");
        let row = view.quotas.row_data(0).unwrap();
        assert_eq!(row.value, "—");
        assert!(!row.measured);
    }

    #[test]
    fn expiry_callback_marks_every_window_stale_and_stops_the_timer() {
        let mut snapshot = snapshot();
        snapshot.quota = Reading::Observed {
            value: vec![QuotaWindow {
                bucket: "test".into(),
                window_minutes: Some(300),
                used_percent: Some(0.0),
                resets_at: None,
            }],
            at: SystemTime::now(),
            valid_for: Duration::from_secs(90),
        };
        let row = card(snapshot);
        assert!(row.fresh && row.expires_ms > 0.0);
        assert_eq!(row.quotas.row_data(0).unwrap().value, "100%");
        let expired = expire_card(row);
        assert!(!expired.fresh);
        assert_eq!(expired.expires_ms, 0.0);
        assert!(expired.quotas.row_data(0).unwrap().detail.contains("过期"));
    }

    #[test]
    fn observed_activity_lower_bound_is_explicit_in_both_modes() {
        let mut snapshot = snapshot();
        snapshot.activity = Reading::Observed {
            value: Activity {
                running: 2,
                waiting: None,
                lower_bound: true,
            },
            at: SystemTime::now(),
            valid_for: Duration::from_secs(15),
        };
        let view = card(snapshot);
        assert_eq!(view.activity, "≥2");
        assert!(view.activity_detail.contains("下限"));
        assert!(view.activity_expires_ms > 0.0);
    }

    #[test]
    fn confirmed_observed_zero_is_distinct_from_missing_activity() {
        let mut snapshot = snapshot();
        assert_eq!(card(snapshot.clone()).activity, "—");
        snapshot.activity = Reading::Observed {
            value: Activity {
                running: 0,
                waiting: None,
                lower_bound: true,
            },
            at: SystemTime::now(),
            valid_for: Duration::from_secs(15),
        };
        let view = card(snapshot);
        assert_eq!(view.activity, "0");
        assert!(view.activity_detail.contains("已观测任务均已结束"));
    }

    #[test]
    fn expired_activity_is_hidden_and_quota_is_explicitly_stale() {
        let mut snapshot = snapshot();
        let at = SystemTime::now() - Duration::from_secs(120);
        snapshot.activity = Reading::Observed {
            value: Activity {
                running: 3,
                waiting: None,
                lower_bound: false,
            },
            at,
            valid_for: Duration::from_secs(60),
        };
        snapshot.quota = Reading::Observed {
            value: vec![QuotaWindow {
                bucket: "test".into(),
                window_minutes: Some(10080),
                used_percent: Some(25.0),
                resets_at: None,
            }],
            at,
            valid_for: Duration::from_secs(60),
        };
        let view = card(snapshot);
        assert_eq!(view.activity, "—");
        assert!(view.activity_detail.contains("过期"));
        assert_eq!(view.status, "过期");
        let row = view.quotas.row_data(0).unwrap();
        assert!(row.label.contains("7 天"));
        assert!(row.detail.contains("过期"));
        assert_eq!(row.value, "75%");
    }

    #[test]
    fn remaining_quota_matches_numbers_bars_and_compact_text_without_filling_unknowns() {
        for (used, expected) in [
            (Some(0.0), Some(100.0)),
            (Some(25.0), Some(75.0)),
            (Some(100.0), Some(0.0)),
            (None, None),
        ] {
            let mut snapshot = snapshot();
            snapshot.quota = Reading::Observed {
                value: vec![QuotaWindow {
                    bucket: "test".into(),
                    window_minutes: Some(300),
                    used_percent: used,
                    resets_at: None,
                }],
                at: SystemTime::now(),
                valid_for: Duration::from_secs(60),
            };
            let view = card(snapshot);
            let row = view.quotas.row_data(0).unwrap();
            let text = expected
                .map(|percent| format!("{percent:.0}%"))
                .unwrap_or_else(|| "—".into());
            assert_eq!(row.value.as_str(), text);
            assert_eq!(row.percent, expected.unwrap_or(0.0) as f32);
            assert_eq!(row.measured, expected.is_some());
            assert!(row.label.contains("剩余"));
            assert_eq!(view.summary.as_str(), format!("余 5h {text}"));
        }
    }
}
