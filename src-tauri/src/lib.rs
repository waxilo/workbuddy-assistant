mod accounts;
mod auth_file;
mod checkin;
mod commands;
mod logs;
mod netfix;
mod notify;
mod oauth;
mod proxy;
mod refresh;
mod scheduler;
mod stealth;
mod tray;

use tauri_plugin_autostart::MacosLauncher;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        // 用 LaunchAgent 而非 AppleScript，登录时静默启动、不弹窗
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        .setup(|app| {
            // 定时自动签到：独立后台线程，与进程同生命周期。
            // 只在应用运行期间生效——桌面端退出后没有守护进程可代为执行。
            scheduler::spawn(app.handle().clone());
            // 本地反代（按积分过期时间优先路由）+ 无感接管的装卸与心跳
            proxy::spawn(app.handle().clone());
            // 系统托盘：后台常驻入口
            tray::setup(app.handle())
                .expect("初始化系统托盘失败");
            Ok(())
        })
        // 关闭窗口 = 隐藏到托盘，进程常驻（签到/续签/反代不中断）。
        // 真正退出走托盘菜单「退出」（app.exit 触发 RunEvent::Exit 的清理逻辑）。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_accounts,
            commands::import_accounts,
            commands::import_accounts_file,
            commands::export_accounts,
            commands::remove_account,
            commands::checkin_one,
            commands::checkin_all,
            commands::refresh_all_credits,
            commands::discover_local_accounts,
            commands::oauth_start,
            commands::oauth_poll,
            commands::open_external,
            commands::get_settings,
            commands::save_settings,
            commands::apply_settings,
            commands::restart_workbuddy,
            commands::test_notify,
            commands::get_autostart,
            commands::set_autostart,
            commands::get_checkin_logs,
            commands::clear_checkin_logs,
            commands::app_version,
            // 网络急救：扫出「调试残留的全局服务端点」并一键清除（含关闭本地反代）
            netfix::net_diagnose,
            netfix::net_restore,
            netfix::reveal_path,
            // 无感接管：状态查询 / 立即停止 / 最近路由
            stealth::stealth_status,
            stealth::stealth_stop,
            stealth::takeover_events,
            proxy::proxy_routes,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application");

    // 退出时安全关闭接管。仅摘配置不够：WorkBuddy 的长驻 CLI host 会把旧值留在
    // process.env，必须在代理仍存活时让它退出，之后才能停止监听。
    app.run(|handle, event| {
        static CLEANED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        match &event {
            // macOS：窗口全隐藏后点 Dock 图标（或 finder 重新打开）→ 唤回主窗口。
            // Reopen 是 macOS 独有变体，Windows 编译时必须条件编译掉。
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => tray::show_main(handle),
            tauri::RunEvent::Exit | tauri::RunEvent::ExitRequested { .. } => {
                if CLEANED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                if let Ok(dir) = commands::try_data_dir(handle) {
                    let mut settings = accounts::load_settings(&dir);
                    if settings.proxy_enabled {
                        settings.proxy_enabled = false;
                        let _ = commands::apply_settings_inner(handle, settings);
                    }
                }
            }
            _ => {}
        }
    });
}
