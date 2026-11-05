//! The native menu bar belongs to GPUI's NSApplication. Muda is used only by
//! the tray; installing a Muda main menu leaves GPUI with no menu action tags.

use gpui::actions;

actions!(fastab_edit, [Cut, Copy, Paste, SelectAll, Undo, Redo]);

#[cfg(target_os = "macos")]
mod macos {
    use fastab_util::consts::PRODUCT_NAME;
    use fastab_util::consts::url::{ISSUE_TRACKER, RELEASE_NOTES, USER_MANUAL};
    use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, actions};
    use tao::event_loop::ControlFlow;

    use super::{Copy, Cut, Paste, Redo, SelectAll, Undo};
    use crate::event::{Event, WindowEvent};
    use crate::{DASHBOARD_ID, EventLoopProxy};

    actions!(
        fastab_menu,
        [
            About,
            CheckForUpdates,
            OpenGithub,
            OpenReleaseNotes,
            ReportIssue,
            CloseWindow,
            Hide,
            HideOthers,
            ShowAll,
            Quit,
            Minimize,
            Zoom,
            BringAllToFront,
        ]
    );

    fn send_settings_event(proxy: &EventLoopProxy, window_event: WindowEvent) {
        let _ = proxy.send_event(Event::WindowEvent {
            window_id: DASHBOARD_ID,
            window_event,
        });
    }

    fn open_link(url: &str) {
        if let Err(err) = fastab_util::open_url(url) {
            tracing::error!(%err, url, "Failed to open menu link");
        }
    }

    #[allow(unexpected_cfgs)]
    fn bring_all_to_front(_: &BringAllToFront, _: &mut App) {
        // AppKit's Window menu command preserves the order and visibility of
        // GPUI's parked overlay window.
        unsafe {
            use objc::{class, msg_send, sel, sel_impl};
            let app: cocoa::base::id = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, arrangeInFront: cocoa::base::nil];
        }
    }

    #[allow(unexpected_cfgs)]
    fn finish_native_menu() {
        // Keep Undo/Redo native for system text fields;
        // our GPUI inputs have no handlers and remain unavailable.
        unsafe {
            use cocoa::base::id;
            use objc::{class, msg_send, sel, sel_impl};

            let app: id = msg_send![class!(NSApplication), sharedApplication];
            let main_menu: id = msg_send![app, mainMenu];
            let edit_item: id = msg_send![main_menu, itemAtIndex: 2isize];
            let edit_menu: id = msg_send![edit_item, submenu];
            let undo_item: id = msg_send![edit_menu, itemAtIndex: 0isize];
            let redo_item: id = msg_send![edit_menu, itemAtIndex: 1isize];
            let _: () = msg_send![undo_item, setAction: sel!(undo:)];
            let _: () = msg_send![redo_item, setAction: sel!(redo:)];

            let window_item: id = msg_send![main_menu, itemAtIndex: 3isize];
            let window_menu: id = msg_send![window_item, submenu];
            let minimize_item: id = msg_send![window_menu, itemAtIndex: 0isize];
            let zoom_item: id = msg_send![window_menu, itemAtIndex: 1isize];
            let _: () = msg_send![minimize_item, setAction: sel!(performMiniaturize:)];
            let _: () = msg_send![zoom_item, setAction: sel!(performZoom:)];
        }
    }

    pub(super) fn install_actions(cx: &mut App, proxy: EventLoopProxy) {
        let about_proxy = proxy.clone();
        cx.on_action(move |_: &About, _| {
            send_settings_event(
                &about_proxy,
                WindowEvent::Batch(vec![
                    WindowEvent::NavigateRelative { path: "/about".into() },
                    WindowEvent::Show,
                ]),
            );
        });
        let updates_proxy = proxy.clone();
        cx.on_action(move |_: &CheckForUpdates, _| {
            updates_proxy.send_event(Event::CheckForUpdates).ok();
        });
        cx.on_action(|_: &OpenGithub, _| open_link(USER_MANUAL));
        cx.on_action(|_: &OpenReleaseNotes, _| open_link(RELEASE_NOTES));
        cx.on_action(|_: &ReportIssue, _| open_link(ISSUE_TRACKER));
        let close_proxy = proxy.clone();
        cx.on_action(move |_: &CloseWindow, _| send_settings_event(&close_proxy, WindowEvent::Close));
        cx.on_action(|_: &Hide, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(move |_: &Quit, _| {
            proxy.send_event(Event::ControlFlow(ControlFlow::Exit)).ok();
        });
        cx.on_action(bring_all_to_front);

        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-h", Hide, None),
            KeyBinding::new("cmd-alt-h", HideOthers, None),
            KeyBinding::new("cmd-m", Minimize, None),
            KeyBinding::new("cmd-a", SelectAll, Some("SettingsInput")),
            KeyBinding::new("cmd-z", Undo, Some("SettingsInput")),
            KeyBinding::new("cmd-shift-z", Redo, Some("SettingsInput")),
            KeyBinding::new("cmd-x", Cut, Some("SettingsInput")),
            KeyBinding::new("cmd-c", Copy, Some("SettingsInput")),
            KeyBinding::new("cmd-v", Paste, Some("SettingsInput")),
        ]);
    }

    pub(super) fn install_native(cx: &App) {
        cx.set_menus(vec![
            Menu {
                name: PRODUCT_NAME.into(),
                items: vec![
                    MenuItem::action(format!("About {PRODUCT_NAME}"), About),
                    MenuItem::action("Check for Updates…", CheckForUpdates),
                    MenuItem::separator(),
                    MenuItem::os_submenu("Services", SystemMenuType::Services),
                    MenuItem::separator(),
                    MenuItem::action(format!("Hide {PRODUCT_NAME}"), Hide),
                    MenuItem::action("Hide Others", HideOthers),
                    MenuItem::action("Show All", ShowAll),
                    MenuItem::separator(),
                    MenuItem::action(format!("Quit {PRODUCT_NAME}"), Quit),
                ],
            },
            Menu {
                name: "File".into(),
                items: vec![MenuItem::action("Close Window", CloseWindow)],
            },
            Menu {
                name: "Edit".into(),
                items: vec![
                    // No handlers or bindings exist for Undo/Redo yet, so GPUI
                    // validates these menu items as unavailable.
                    MenuItem::os_action("Undo", Undo, OsAction::Undo),
                    MenuItem::os_action("Redo", Redo, OsAction::Redo),
                    MenuItem::separator(),
                    MenuItem::os_action("Cut", Cut, OsAction::Cut),
                    MenuItem::os_action("Copy", Copy, OsAction::Copy),
                    MenuItem::os_action("Paste", Paste, OsAction::Paste),
                    MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
                ],
            },
            Menu {
                name: "Window".into(),
                items: vec![
                    MenuItem::action("Minimize", Minimize),
                    MenuItem::action("Zoom", Zoom),
                    MenuItem::separator(),
                    MenuItem::action("Bring All to Front", BringAllToFront),
                ],
            },
            Menu {
                name: "Help".into(),
                items: vec![
                    MenuItem::action(format!("{PRODUCT_NAME} on GitHub"), OpenGithub),
                    MenuItem::action("Release Notes", OpenReleaseNotes),
                    MenuItem::action("Report an Issue", ReportIssue),
                ],
            },
        ]);
        finish_native_menu();
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::{Minimize, Zoom};

#[cfg(target_os = "macos")]
pub fn install_actions(cx: &mut gpui::App, proxy: crate::EventLoopProxy) {
    macos::install_actions(cx, proxy);
}

#[cfg(target_os = "macos")]
pub fn install_native(cx: &gpui::App) {
    macos::install_native(cx);
}
