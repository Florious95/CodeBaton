use aisync_core::DeviceId;

pub mod backend;
mod commands;
mod dto;
mod state;
mod tray;

use backend::Backend;
use state::AppState;
use tauri::{Manager, WindowEvent};

pub fn local_device_id_placeholder() -> DeviceId {
    DeviceId::new()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Startup banner to the file sink so qa can confirm logging works even when
    // the DMG is launched via `open -a` (stderr → /dev/null).
    backend::log_line("[aisync-app] starting — logs at ~/.aisync/logs/aisync.log");
    let backend = Backend::new().expect("failed to initialize CodeBaton backend");

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .manage(backend)
        .setup(|app| {
            tray::build(app.handle())?;

            // BUG-005: make sure the main window is actually drawn and frontmost
            // on launch. WKWebView can stay an unpainted (black) surface if it is
            // created hidden/occluded; explicitly show → center → focus forces a
            // paint and brings it to the foreground so any screenshot is valid.
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.center();
                let _ = win.unminimize();
                let _ = win.set_focus();
                eprintln!(
                    "[aisync-app] main window ready: visible={:?} url={:?}",
                    win.is_visible(),
                    win.url().map(|u| u.to_string())
                );
            } else {
                eprintln!("[aisync-app] WARNING: main window not found at setup");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // "最小化到托盘" — closing the main window hides it instead of
            // quitting (docs/ui-design.md §10 Cmd/Ctrl+W, behaviour setting).
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let state = window.state::<AppState>();
                    if state.settings().minimize_to_tray {
                        let _ = window.hide();
                        api.prevent_close();
                    }
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_overview,
            commands::get_peers,
            commands::get_peer_detail,
            commands::get_settings,
            commands::save_settings,
            commands::get_status_bar,
            commands::is_onboarded,
            commands::get_local_info,
            commands::complete_onboarding,
            commands::log_event,
            commands::open_path,
            commands::begin_pairing,
            commands::pending_pairing_request,
            commands::pending_project_mapping_request,
            commands::confirm_project_mapping_request,
            commands::poll_project_mapping_acks,
            commands::pending_text_message,
            commands::text_messages,
            commands::send_text_message,
            commands::request_file_transfer,
            commands::pick_files_for_transfer,
            commands::paste_files_for_transfer,
            commands::pending_file_transfer_request,
            commands::pending_file_transfers,
            commands::confirm_file_transfer_request,
            commands::accept_file_transfer,
            commands::poll_file_transfer_acks,
            commands::get_default_file_receive_dir,
            commands::set_default_file_receive_dir,
            commands::get_default_receive_dir,
            commands::set_default_receive_dir,
            commands::file_transfer_history,
            commands::pending_workspace_mapping_request,
            commands::confirm_workspace_mapping_request,
            commands::poll_workspace_mapping_acks,
            commands::confirm_pairing,
            commands::cancel_pairing,
            commands::unpair,
            commands::scan_workspace,
            commands::get_batch_plan,
            commands::get_conflict,
            commands::resolve_conflict,
            commands::get_rewrite_report,
            commands::set_auto_sync_paused,
            commands::get_auto_sync_paused,
            commands::start_sync,
            commands::cancel_sync,
            commands::add_project,
            commands::add_workspace,
            commands::enable_child,
            commands::set_project_mode,
            commands::save_exclude_rules,
            commands::delete_project,
            commands::get_serve_info,
            commands::add_peer_endpoint,
            commands::pick_directory,
        ])
        .run(tauri::generate_context!())
        .expect("error while running CodeBaton");
}
