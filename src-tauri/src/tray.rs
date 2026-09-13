//! 系统托盘与后台常驻。
//!
//! 需求：关闭窗口 ≠ 退出进程。定时签到、token 续签、本地反代都依赖进程常驻，
//! 所以点红按钮只隐藏窗口，真正退出必须走托盘菜单「退出」。

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

/// 构建托盘图标 + 菜单（显示主窗口 / 退出）。
pub fn setup(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("WorkBuddy 助手")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.to_owned());
    }
    builder.build(app)?;
    Ok(())
}

/// 显示并聚焦主窗口（托盘菜单 / macOS Dock 重新激活共用）。
pub fn show_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}
