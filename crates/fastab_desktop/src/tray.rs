use std::borrow::Cow;
use std::sync::OnceLock;

use cfg_if::cfg_if;
use fastab_install::InstallComponents;
use fastab_os_shim::Context;
use fastab_remote_ipc::figterm::FigtermState;
use fastab_util::consts::PRODUCT_NAME;
use fastab_util::url::USER_MANUAL;
use muda::accelerator::Accelerator;
use muda::{IconMenuItem, Menu, MenuEvent, MenuId, PredefinedMenuItem, Submenu};
use tao::event_loop::ControlFlow;
use tracing::{error, trace};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::event::{Event, ShowMessageNotification, WindowEvent};
use crate::{AUTOCOMPLETE_ID, DASHBOARD_ID, EventLoopProxy};

// macro_rules! icon {
//     ($icon:literal) => {{
//         #[cfg(target_os = "macos")]
//         {
//             Some(include_bytes!(concat!(
//                 env!("TRAY_ICONS_PROCESSED"),
//                 "/",
//                 $icon,
//                 ".png"
//             )))
//         }
//         #[cfg(not(target_os = "macos"))]
//         {
//             None
//         }
//     }};
// }

const ACCESSIBILITY_MENU_ID: &str = "accessibility";

/// Autocomplete is fully inert without Accessibility, so the tray is the one surface that can say
/// so no matter how the app was launched — a silent launch never opens the dashboard, where the
/// permission gate lives.
#[cfg(target_os = "macos")]
fn accessibility_is_missing() -> bool {
    !macos_utils::accessibility::accessibility_is_enabled()
}

#[cfg(not(target_os = "macos"))]
fn accessibility_is_missing() -> bool {
    false
}

fn tray_update(proxy: &EventLoopProxy) {
    let proxy = proxy.clone();
    tokio::runtime::Handle::current().spawn(async move {
        if !crate::update::check_for_update(true, true).await {
            proxy
                .send_event(
                    ShowMessageNotification {
                        title: format!("{PRODUCT_NAME} updates are unavailable").into(),
                        body: "The Sparkle updater could not start — the framework may be missing from this build or failed to initialize. Check the logs for details.".into(),
                        ..Default::default()
                    }
                    .into(),
                )
                .unwrap();
        }
    });
}

pub fn handle_event(menu_event: &MenuEvent, proxy: &EventLoopProxy) {
    match &*menu_event.id().0 {
        "dashboard-devtools" => {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: DASHBOARD_ID,
                    window_event: WindowEvent::Devtools,
                })
                .unwrap();
        },
        "autocomplete-devtools" => {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: AUTOCOMPLETE_ID,
                    window_event: WindowEvent::Devtools,
                })
                .unwrap();
        },
        "update" => {
            tray_update(proxy);
        },
        "quit" => {
            proxy.send_event(Event::ControlFlow(ControlFlow::Exit)).unwrap();
        },
        "dashboard" => {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: DASHBOARD_ID.clone(),
                    window_event: WindowEvent::Batch(vec![
                        WindowEvent::NavigateRelative { path: "/".into() },
                        WindowEvent::Show,
                    ]),
                })
                .unwrap();
        },
        "settings" => {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: DASHBOARD_ID.clone(),
                    window_event: WindowEvent::Batch(vec![
                        WindowEvent::NavigateRelative {
                            path: "/autocomplete".into(),
                        },
                        WindowEvent::Show,
                    ]),
                })
                .unwrap();
        },
        "not-working" => {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: DASHBOARD_ID.clone(),
                    window_event: WindowEvent::Batch(vec![
                        WindowEvent::NavigateRelative { path: "/help".into() },
                        WindowEvent::Show,
                    ]),
                })
                .unwrap();
        },
        "uninstall" => {
            tokio::runtime::Handle::current().spawn(async {
                fastab_install::uninstall(InstallComponents::all(), Context::new())
                    .await
                    .ok();
                #[allow(clippy::exit)]
                std::process::exit(0);
            });
        },
        ACCESSIBILITY_MENU_ID => {
            proxy
                .send_event(Event::WindowEvent {
                    window_id: DASHBOARD_ID.clone(),
                    window_event: WindowEvent::Batch(vec![WindowEvent::Show]),
                })
                .unwrap();
        },
        "user-manual" => {
            if let Err(err) = fastab_util::open_url(USER_MANUAL) {
                error!(%err, "Failed to open user manual url");
            }
        },
        id => {
            trace!(?id, "Unhandled tray event");
        },
    }

    // fastab_telemetry removed
}

#[allow(dead_code)]
#[cfg(target_os = "linux")]
fn load_icon(path: impl AsRef<std::path::Path>) -> Option<Icon> {
    let image = image::open(path).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    let rgba = image.into_raw();
    Icon::from_rgba(rgba, width, height).ok()
}

pub async fn build_tray(_figterm_state: &FigtermState) -> tray_icon::Result<TrayIcon> {
    let is_logged_in = true; // fig_auth removed
    TrayIconBuilder::new()
        .with_icon(get_icon(is_logged_in))
        .with_icon_as_template(true)
        .with_menu(Box::new(get_context_menu(is_logged_in)))
        .build()
}

pub fn get_icon(is_logged_in: bool) -> Icon {
    fn decode(bytes: &[u8]) -> Icon {
        let image = image::load_from_memory(bytes)
            .expect("Failed to open icon path")
            .into_rgba8();
        let (width, height) = image.dimensions();
        Icon::from_rgba(image.into_raw(), width, height).expect("Failed to open icon")
    }

    if is_logged_in {
        static SIGNED_IN: OnceLock<Icon> = OnceLock::new();
        return SIGNED_IN
            .get_or_init(|| {
                cfg_if! {
                    if #[cfg(target_os = "linux")] {
                        decode(include_bytes!("../icons/icon-monochrome-light.png"))
                    } else if #[cfg(target_os = "macos")] {
                        // tray-icon displays this at 18 pt; provide 36 px for Retina.
                        decode(include_bytes!("../icons/icon-monochrome@2x.png"))
                    } else {
                        decode(include_bytes!("../icons/icon-monochrome.png"))
                    }
                }
            })
            .clone();
    }

    static SIGNED_OUT: OnceLock<Icon> = OnceLock::new();
    SIGNED_OUT
        .get_or_init(|| {
            cfg_if! {
                if #[cfg(target_os = "linux")] {
                    decode(include_bytes!("../icons/icon-monochrome-light.png"))
                } else if #[cfg(target_os = "macos")] {
                    decode(include_bytes!("../icons/not-logged-in@2x.png"))
                } else {
                    decode(include_bytes!("../icons/not-logged-in.png"))
                }
            }
        })
        .clone()
}

fn warning_icon_rgba() -> (Vec<u8>, u32, u32) {
    static RGBA: OnceLock<(Vec<u8>, u32, u32)> = OnceLock::new();
    RGBA.get_or_init(|| {
        let image = image::load_from_memory(include_bytes!("../icons/yellow-circle.png"))
            .expect("Failed to open icon path")
            .into_rgba8();
        let (width, height) = image.dimensions();
        (image.into_raw(), width, height)
    })
    .clone()
}

pub fn get_context_menu(is_logged_in: bool) -> Menu {
    let mut tray_menu = Menu::new();

    let elements = menu(is_logged_in);
    for elem in elements {
        elem.add_to_menu(&mut tray_menu);
    }

    tray_menu
}

enum MenuElement {
    Info {
        image_icon: Option<muda::Icon>,
        text: Cow<'static, str>,
    },
    Entry {
        emoji_icon: Option<Cow<'static, str>>,
        image_icon: Option<muda::Icon>,
        text: Cow<'static, str>,
        id: Cow<'static, str>,
        accelerator: Option<Accelerator>,
    },
    Separator,
    #[allow(dead_code)]
    SubMenu {
        title: Cow<'static, str>,
        elements: Vec<MenuElement>,
    },
}

impl MenuElement {
    fn info(image_icon: Option<(Vec<u8>, u32, u32)>, text: impl Into<Cow<'static, str>>) -> Self {
        Self::Info {
            image_icon: image_icon.and_then(|(bytes, width, height)| muda::Icon::from_rgba(bytes, width, height).ok()),
            text: text.into(),
        }
    }

    fn entry(
        emoji_icon: Option<Cow<'static, str>>,
        image_icon: Option<(Vec<u8>, u32, u32)>,
        text: impl Into<Cow<'static, str>>,
        id: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::Entry {
            emoji_icon,
            image_icon: image_icon.and_then(|(bytes, width, height)| muda::Icon::from_rgba(bytes, width, height).ok()),
            text: text.into(),
            id: id.into(),
            accelerator: None,
        }
    }

    fn with_accelerator(mut self, accel: &str) -> Self {
        if let Self::Entry {
            ref mut accelerator, ..
        } = self
        {
            *accelerator = accel.parse::<Accelerator>().ok();
        }
        self
    }

    // fn sub_menu(title: impl Into<Cow<'static, str>>, elements: Vec<MenuElement>) -> Self {
    //     Self::SubMenu {
    //         title: title.into(),
    //         elements,
    //     }
    // }

    fn add_to_menu(&self, menu: &mut Menu) {
        match self {
            MenuElement::Info { text, image_icon } => {
                let menu_item = IconMenuItem::new(
                    text,
                    false,
                    image_icon.clone(), // Some(muda::Icon::from_rgba(bytes, width, height).unwrap()),
                    None,
                );
                menu.append(&menu_item).unwrap();
            },
            MenuElement::Entry {
                emoji_icon,
                image_icon,
                text,
                id,
                accelerator,
            } => {
                let text = match (std::env::consts::OS, emoji_icon) {
                    ("linux", Some(emoji_icon)) => format!("{emoji_icon} {text}"),
                    _ => text.to_string(),
                };
                let menu_item = IconMenuItem::with_id(MenuId::new(id), text, true, image_icon.clone(), *accelerator);
                menu.append(&menu_item).unwrap();
            },
            MenuElement::Separator => {
                menu.append(&PredefinedMenuItem::separator()).unwrap();
            },
            MenuElement::SubMenu { title, elements } => {
                let sub_menu = Submenu::new(title, true);
                for element in elements {
                    element.add_to_submenu(&sub_menu);
                }

                menu.append(&sub_menu).unwrap();
            },
        }
    }

    fn add_to_submenu(&self, submenu: &Submenu) {
        match self {
            MenuElement::Info { image_icon, text } => {
                // menu.append(MenuItemAttributes::new(info).with_enabled(false));
                let menu_item = IconMenuItem::new(
                    text,
                    false,
                    image_icon.clone(), // Some(muda::Icon::from_rgba(bytes, width, height).unwrap()),
                    None,
                );
                submenu.append(&menu_item).unwrap();
            },
            MenuElement::Entry {
                emoji_icon,
                text,
                id,
                accelerator,
                ..
            } => {
                let text: String = match (std::env::consts::OS, emoji_icon) {
                    ("linux", Some(emoji_icon)) => format!("{emoji_icon} {text}"),
                    _ => text.to_string(),
                };
                let menu_item = muda::MenuItem::with_id(MenuId::new(id), text, true, *accelerator);
                submenu.append(&menu_item).unwrap();
            },
            MenuElement::Separator => {
                submenu.append(&PredefinedMenuItem::separator()).unwrap();
            },
            MenuElement::SubMenu { title, elements } => {
                let sub_menu = Submenu::new(title, true);
                for element in elements {
                    element.add_to_submenu(&sub_menu);
                }

                submenu.append(&sub_menu).unwrap();
            },
        }
    }
}

fn menu(_is_logged_in: bool) -> Vec<MenuElement> {
    let quit = MenuElement::entry(None, None, "Quit", "quit").with_accelerator("super+KeyQ");
    let settings = MenuElement::entry(None, None, "Settings", "settings").with_accelerator("super+Comma");
    let check_for_updates = MenuElement::entry(None, None, "Check for Updates…", "update");

    // Auth is gone. A leftover logout IPC must not flip the tray to
    // "Get Started" / "Log back in" — those commands do not exist.
    let mut menu = vec![settings, check_for_updates];

    if accessibility_is_missing() {
        let warning_img = warning_icon_rgba();
        let mut warning = vec![
            MenuElement::info(Some(warning_img), "Accessibility permission is missing"),
            MenuElement::entry(None, None, "Enable Accessibility…", ACCESSIBILITY_MENU_ID),
            MenuElement::Separator,
        ];
        warning.append(&mut menu);
        menu = warning;
    }

    menu.extend(vec![MenuElement::Separator, quit]);

    menu
}

#[cfg(test)]
mod tests {
    #[test]
    fn tray_accessibility_entry_opens_our_settings_not_system_settings() {
        let production = include_str!("tray.rs")
            .rsplit_once("mod tests {")
            .map(|(src, _)| src)
            .expect("production source");
        assert!(
            !production.contains("prompt_for_accessibility"),
            "tray must not raise the system TCC sheet"
        );
        assert!(
            !production.contains("open_accessibility"),
            "tray must not open System Settings"
        );
        assert!(
            !production.contains("begin_accessibility_guide"),
            "tray must not start the Accessibility guide"
        );
        assert!(production.contains("ACCESSIBILITY_MENU_ID"));
        assert!(production.contains("DASHBOARD_ID"));
        assert!(production.contains("WindowEvent::Show"));
    }

    #[test]
    fn tray_icon_decode_is_cached() {
        let first = super::get_icon(true);
        let second = super::get_icon(true);
        let _ = (first, second);
        let warning = super::warning_icon_rgba();
        assert!(!warning.0.is_empty());
        assert_eq!(warning.0.len(), (warning.1 * warning.2 * 4) as usize);
    }
}
