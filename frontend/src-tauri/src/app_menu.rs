//! The macOS menu bar. Mac apps are expected to have one (the app menu with
//! Settings… and Quit, Edit for copy and paste, Window); without it the app
//! gets Tauri's bare default. Windows apps usually have no menu bar, so the
//! same commands are keyboard shortcuts there (see useAppShortcuts).

use tauri::menu::{AboutMetadata, Menu, MenuEvent, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder};
use tauri::{AppHandle, Emitter, Runtime};

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<Menu<R>> {
    let name = app.package_info().name.clone();
    let app_menu = SubmenuBuilder::new(app, &name)
        .item(&PredefinedMenuItem::about(app, None, Some(AboutMetadata::default()))?)
        .separator()
        .item(&MenuItemBuilder::with_id("menu_settings", "Settings…").accelerator("CmdOrCtrl+,").build(app)?)
        .separator()
        .item(&PredefinedMenuItem::services(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::hide(app, None)?)
        .item(&PredefinedMenuItem::hide_others(app, None)?)
        .item(&PredefinedMenuItem::show_all(app, None)?)
        .separator()
        .item(&PredefinedMenuItem::quit(app, None)?)
        .build()?;
    let file = SubmenuBuilder::new(app, "File")
        .item(
            &MenuItemBuilder::with_id("menu_toggle_recording", "Start or Stop Recording")
                .accelerator("CmdOrCtrl+Shift+R")
                .build(app)?,
        )
        .item(&MenuItemBuilder::with_id("menu_import_audio", "Import Audio…").accelerator("CmdOrCtrl+O").build(app)?)
        .separator()
        .item(&PredefinedMenuItem::close_window(app, None)?)
        .build()?;
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    let view = SubmenuBuilder::new(app, "View").item(&PredefinedMenuItem::fullscreen(app, None)?).build()?;
    let window = SubmenuBuilder::new(app, "Window")
        .item(&PredefinedMenuItem::minimize(app, None)?)
        .item(&PredefinedMenuItem::maximize(app, None)?)
        .build()?;
    Menu::with_items(app, &[&app_menu, &file, &edit, &view, &window])
}

pub fn handle_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    // Tray clicks reach this handler too, so the menu bar's ids are its own:
    // reusing the tray's would run a tray click twice.
    match event.id().as_ref() {
        "menu_import_audio" => {
            crate::tray::focus_main_window(app);
            if let Err(e) = app.emit_to("main", "open-import-dialog", ()) {
                log::error!("Menu: failed to emit open-import-dialog: {}", e);
            }
        }
        // Settings and recording do exactly what the tray items do.
        "menu_settings" => crate::tray::handle_menu_event(app, "settings"),
        "menu_toggle_recording" => crate::tray::handle_menu_event(app, "toggle_recording"),
        _ => {}
    }
}
