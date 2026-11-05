use std::sync::Arc;

#[cfg(target_os = "macos")]
use macos_utils::window_server::ApplicationSpecifier;
use serde::Serialize;
use tao::dpi::Position;

use crate::EventLoopProxy;
use crate::utils::Rect;
use crate::webview::{FigIdMap, WindowId};

cfg_if::cfg_if! {
    if #[cfg(target_os = "linux")] {
        mod linux;
        pub use self::linux::*;
    } else if #[cfg(target_os = "macos")] {
        mod macos;
        pub use self::macos::*;
    } else if #[cfg(target_os = "windows")] {
        mod windows;
        pub use self::windows::*;
    } else {
        compile_error!("Unsupported platform");
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct PlatformWindow {
    pub rect: Rect,
    pub inner: PlatformWindowImpl,
    // TODO: add a platform specific impl of things like name, is_terminal(), etc
    // pub inner: ExternalPlatformWindowImpl
}

#[derive(Debug, Serialize)]
pub struct PlatformState(Arc<PlatformStateImpl>);

impl PlatformState {
    /// Create a new PlatformState instance
    pub fn new(proxy: EventLoopProxy) -> Self {
        Self(Arc::new(PlatformStateImpl::new(proxy)))
    }

    /// Handle a [`PlatformBoundEvent`]
    pub fn handle(self: &Arc<Self>, event: PlatformBoundEvent, window_map: &FigIdMap) -> anyhow::Result<()> {
        self.clone().0.handle(event, window_map)
    }

    /// Position the window at the given coordinates
    #[allow(dead_code)]
    pub fn position_window(&self, _window_id: &WindowId, position: Position) -> anyhow::Result<()> {
        self.0.position_window(_window_id, position)
    }

    /// Gets the current cursor position on the screen
    #[allow(dead_code)]
    pub fn get_cursor_position(&self) -> Option<Rect> {
        self.0.get_cursor_position()
    }

    /// Gets the currently active window on the platform
    pub fn get_active_window(&self) -> Option<PlatformWindow> {
        self.0.get_active_window()
    }

    /// Whether or not accessibility is enabled
    pub fn accessibility_is_enabled() -> Option<bool> {
        PlatformStateImpl::accessibility_is_enabled()
    }

    /// Returns the platform specific implementation.
    #[allow(dead_code)]
    pub fn inner(&self) -> Arc<PlatformStateImpl> {
        Arc::clone(&self.0)
    }
}

#[derive(Debug)]
pub enum PlatformBoundEvent {
    /// Early initialization before the event loop has started
    Initialize,
    /// Late initialization after the event loop has started
    InitializePostRun,
    EditBufferChanged,
    AccessibilityUpdated {
        enabled: bool,
    },
    AccessibilityUpdateRequested,
    #[cfg(target_os = "macos")]
    CaretPositionUpdateRequested {
        app: ApplicationSpecifier,
        window_id: u32,
    },
    WindowDestroyed {
        // TODO: dont use on other platforms than macos
        #[cfg(target_os = "macos")]
        app: ApplicationSpecifier,
    },
    /// Keyboard focus moved between elements of one window, e.g. from a VS Code terminal pane to
    /// the editor. Only raised on macOS, and only for the Electron terminals that need it.
    #[cfg(target_os = "macos")]
    FocusedElementChanged {
        element: macos_utils::window_server::UIElement,
        app: ApplicationSpecifier,
    },
    ExternalWindowFocusChanged {
        #[cfg(target_os = "macos")]
        app: ApplicationSpecifier,
        window: Option<PlatformWindowImpl>,
    },
    /// Re-applies the autocomplete overlay's window level from the currently focused terminal.
    ///
    /// The level is normally set by [`PlatformBoundEvent::ExternalWindowFocusChanged`], but the
    /// overlay's webview can be released and rebuilt between focus changes, which leaves the new
    /// native window at the default level until the user switches terminals again.
    AutocompleteWindowLevelUpdateRequested,
}
