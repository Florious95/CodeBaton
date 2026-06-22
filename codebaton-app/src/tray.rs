//! System tray (docs/ui-design.md §6, falls under X5).
//!
//! Builds the tray icon + right-click menu, reflects the global sync status in
//! the icon, and wires the "暂停所有自动同步" toggle and push/pull/quit actions.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;

pub const TRAY_ID: &str = "main";

/// Build the tray icon on startup.
pub fn build(app: &AppHandle) -> tauri::Result<()> {
    // First run: no paired peer yet.
    let menu = build_menu(app, "无配对设备", false)?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("CodeBaton — 空闲")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            // Left click opens the main window.
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

fn build_menu(app: &AppHandle, peer: &str, peer_online: bool) -> tauri::Result<Menu<tauri::Wry>> {
    let header = MenuItem::with_id(app, "header", "CodeBaton", false, None::<&str>)?;
    let status = MenuItem::with_id(app, "status", "状态: 空闲", false, None::<&str>)?;
    let connected = MenuItem::with_id(
        app,
        "connected",
        format!(
            "已连接: {} ({})",
            peer,
            if peer_online { "在线" } else { "离线" }
        ),
        false,
        None::<&str>,
    )?;
    let push = MenuItem::with_id(
        app,
        "push_all",
        format!("推送全部到 {}", peer),
        peer_online,
        None::<&str>,
    )?;
    let pull = MenuItem::with_id(
        app,
        "pull_all",
        format!("从 {} 拉取全部", peer),
        peer_online,
        None::<&str>,
    )?;
    let open = MenuItem::with_id(app, "open", "打开主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;

    Menu::with_items(
        app,
        &[
            &header, &sep, &status, &connected, &sep, &push, &pull, &sep, &open, &sep, &quit,
        ],
    )
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => show_main(app),
        "quit" => app.exit(0),
        "push_all" => {
            show_main(app);
            let _ = app.emit("tray-action", "push_all");
        }
        "pull_all" => {
            show_main(app);
            let _ = app.emit("tray-action", "pull_all");
        }
        _ => {}
    }
}

fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// Rebuild the tray menu + tooltip to reflect current state.
pub fn refresh(
    app: &AppHandle,
    _state: &AppState,
    backend: &crate::backend::Backend,
) -> tauri::Result<()> {
    // Real primary peer from the backend's paired set, not seeded display data.
    let paired = backend.paired_peers();
    let (peer, peer_online) = paired
        .first()
        .map(|(d, online)| (d.name.clone(), *online))
        .unwrap_or_else(|| ("无配对设备".to_string(), false));
    let menu = build_menu(app, &peer, peer_online)?;

    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))?;
        // No live sync/conflict status surfaced to the tray yet → idle.
        tray.set_tooltip(Some("CodeBaton — 空闲"))?;
    }
    Ok(())
}
