//! System tray icon and the native window icon (taskbar and title bar).
use anyhow::Result;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// Icon resource ID embedded from `Assets/favicon.ico` by `build.rs` (winres default).
pub const APP_ICON_RESOURCE: u16 = 1;

pub enum TrayAction {
    Show,
    Quit,
}

pub struct Tray {
    _icon: TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

impl Tray {
    pub fn new() -> Result<Self> {
        let show = MenuItem::new("Show PigeonMail", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append(&show)?;
        menu.append(&quit)?;
        let icon = Icon::from_resource(APP_ICON_RESOURCE, Some((16, 16)))?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("PigeonMail")
            .with_icon(icon)
            .build()?;
        Ok(Self {
            _icon: tray,
            show_id: show.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    /// Returns the next tray action, if the user clicked the icon or picked a menu entry.
    pub fn poll(&self) -> Option<TrayAction> {
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                return Some(TrayAction::Show);
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == self.show_id {
                return Some(TrayAction::Show);
            }
            if event.id == self.quit_id {
                return Some(TrayAction::Quit);
            }
        }
        None
    }
}

/// Sets the taskbar (`ICON_BIG`) and title bar (`ICON_SMALL`) icons of a winit window.
///
/// winit does not set a window icon on Windows, so the taskbar would otherwise show the default.
#[cfg(windows)]
pub fn apply_window_icon(window: &slint::winit_030::winit::window::Window) -> bool {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{LPARAM, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, WM_SETICON,
    };

    let Ok(handle) = window.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(win) = handle.as_raw() else {
        return false;
    };
    let hwnd = win.hwnd.get() as *mut core::ffi::c_void;

    // SAFETY: `hwnd` is the live window handle of this winit window; the icon resource is loaded
    // from the current executable, whose handle `GetModuleHandleW(null)` returns.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let resource = APP_ICON_RESOURCE as usize as *const u16;
        let load = |size: i32| {
            LoadImageW(instance, resource, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) as isize
        };
        let big = load(GetSystemMetrics(SM_CXICON));
        let small = load(GetSystemMetrics(SM_CXSMICON));
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as WPARAM, big as LPARAM);
        SendMessageW(hwnd, WM_SETICON, ICON_SMALL as WPARAM, small as LPARAM);
    }
    true
}
