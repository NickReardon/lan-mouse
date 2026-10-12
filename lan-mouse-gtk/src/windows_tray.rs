//! Windows notification-area (tray) icon, the counterpart of the macOS menu
//! bar item. It stays while Lan Mouse runs, so closing the window only hides
//! it and the daemon keeps sharing. See NickReardon/lan-mouse#23.

use std::cell::RefCell;

use adw::prelude::*;
use gtk::glib;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON, SM_CYSMICON};

use crate::window::Window;

/// Icon resource ordinal from build-aux/windows/lan-mouse.rc.
const ICON_RESOURCE: u16 = 1;
const OPEN: &str = "open";
const QUIT: &str = "quit";

enum Event {
    Open,
    Quit,
}

/// Creates the tray icon and makes closing the window hide it. On error the
/// window keeps its normal close behavior so Lan Mouse cannot become
/// unreachable.
pub fn setup(app: &adw::Application, window: &Window) -> Result<(), Box<dyn std::error::Error>> {
    let menu = Menu::new();
    menu.append_items(&[
        &MenuItem::with_id(OPEN, "Open Lan Mouse", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id(QUIT, "Quit Lan Mouse", true, None),
    ])?;

    // GTK pumps Win32 messages on this thread for every window, including the
    // tray's hidden native window. Do not create the icon on a worker thread.
    let tray = TrayIconBuilder::new()
        .with_tooltip("Lan Mouse")
        .with_icon(icon()?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;

    // Install close interception only after the icon exists.
    window.connect_close_request(|window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });

    // The hold keeps the application, and the daemon the launcher owns, alive
    // without a visible window. Quit ends the GTK loop and the launcher then
    // stops that daemon.
    let (sender, receiver) = async_channel::unbounded();
    let shutdown_receiver = receiver.clone();
    let lifetime = RefCell::new(Some((tray, app.hold())));
    app.connect_shutdown(move |_| {
        // The native event handlers are process-wide and cannot be removed.
        // Close their channel so late callbacks cannot enqueue more UI work.
        shutdown_receiver.close();
        lifetime.borrow_mut().take();
    });

    // Forward native callbacks to GTK asynchronously so GTK is not re-entered
    // while Windows is dispatching a tray message.
    let menu_sender = sender.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let event = match event.id.as_ref() {
            OPEN => Event::Open,
            QUIT => Event::Quit,
            _ => return,
        };
        let _ = menu_sender.try_send(event);
    }));
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = sender.try_send(Event::Open);
        }
    }));
    glib::spawn_future_local(glib::clone!(
        #[weak]
        app,
        #[weak]
        window,
        async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    Event::Open => window.present(),
                    Event::Quit => app.quit(),
                }
            }
        }
    ));
    Ok(())
}

/// The app icon embedded in lan-mouse.exe, at the small-icon size for the
/// current DPI so Windows does not downscale the 32px default.
fn icon() -> Result<Icon, tray_icon::BadIcon> {
    let (width, height) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    let size = (width > 0 && height > 0).then_some((width as u32, height as u32));
    Icon::from_resource(ICON_RESOURCE, size)
}
