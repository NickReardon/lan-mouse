mod authorization_window;
mod client_object;
mod client_row;
mod fingerprint_window;
mod key_object;
mod key_row;
#[cfg(target_os = "macos")]
mod macos_privacy;
#[cfg(target_os = "macos")]
mod macos_status_item;
mod window;

use std::{env, process, str, sync::OnceLock};

use gtk::CssProvider;
use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`lan_mouse_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

/// Convenience: returns the local commit as an 8-char ASCII string,
/// or a placeholder if unset (which would indicate a programmer
/// error since [`run`] always sets it).
pub(crate) fn local_commit_str() -> String {
    LOCAL_COMMIT
        .get()
        .and_then(|c| std::str::from_utf8(c).ok())
        .unwrap_or("????????")
        .to_string()
}

use lan_mouse_ipc::FrontendEvent;

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
    #[error("could not initialize GTK: {0}")]
    Initialization(#[from] glib::BoolError),
}

/// Called on the same thread that will run GTK, before registering GApplication.
pub fn confirm_replacement(force: bool, details: &str) -> Result<bool, GtkError> {
    #[cfg(target_os = "macos")]
    configure_macos_bundle_environment();
    gtk::init()?;
    let dialog = gtk::MessageDialog::builder()
        .modal(true)
        .message_type(if force {
            gtk::MessageType::Warning
        } else {
            gtk::MessageType::Question
        })
        .text(if force {
            "Force close existing Lan Mouse?"
        } else {
            "Lan Mouse is already running"
        })
        .secondary_text(details)
        .build();
    dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    dialog.add_button(
        if force {
            "Force close and start"
        } else {
            "Close existing and start this version"
        },
        gtk::ResponseType::Accept,
    );
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let response = glib::MainContext::default().block_on(dialog.run_future());
    dialog.close();
    Ok(response == gtk::ResponseType::Accept)
}

pub fn show_startup_error(message: &str) {
    #[cfg(target_os = "macos")]
    configure_macos_bundle_environment();
    if gtk::init().is_err() {
        return;
    }
    let dialog = gtk::MessageDialog::builder()
        .modal(true)
        .message_type(gtk::MessageType::Error)
        .text("Lan Mouse could not start")
        .secondary_text(message)
        .buttons(gtk::ButtonsType::Close)
        .build();
    glib::MainContext::default().block_on(dialog.run_future());
    dialog.close();
}

pub fn run(
    local_commit: [u8; 8],
    connection: (
        lan_mouse_ipc::FrontendEventReader,
        lan_mouse_ipc::FrontendRequestWriter,
    ),
) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    LOCAL_COMMIT
        .set(local_commit)
        .expect("local_commit set once");

    // The launcher provides the larger Windows stack for both preflight and GTK.
    let ret = gtk_main(connection);

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

fn gtk_main(
    connection: (
        lan_mouse_ipc::FrontendEventReader,
        lan_mouse_ipc::FrontendRequestWriter,
    ),
) -> glib::ExitCode {
    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle_environment();
        install_macos_gtk_log_filter();
    }

    gio::resources_register_include!("lan-mouse.gresource").expect("Failed to register resources.");

    let app = Application::builder()
        .application_id("de.feschber.LanMouse")
        .build();

    app.connect_startup(|app| {
        load_css();
        load_icons();
        setup_actions(app);
        setup_menu(app);
    });
    let connection = std::cell::RefCell::new(Some(connection));
    app.connect_activate(move |app| {
        if let Some(window) = app.windows().first() {
            window.present();
            return;
        }
        if let Some(connection) = connection.borrow_mut().take() {
            build_ui(app, connection);
        }
    });

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

#[cfg(target_os = "macos")]
fn install_macos_gtk_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && is_gtk_theme_parser_warning(fields) {
            return glib::LogWriterOutput::Handled;
        }

        glib::log_writer_default(level, fields)
    });
}

#[cfg(target_os = "macos")]
fn is_gtk_theme_parser_warning(fields: &[glib::LogField<'_>]) -> bool {
    let mut domain = None;
    let mut message = None;

    for field in fields {
        match field.key() {
            "GLIB_DOMAIN" => domain = field.value_str(),
            "MESSAGE" => message = field.value_str(),
            _ => {}
        }
    }

    domain == Some("Gtk")
        && message.is_some_and(|message| message.starts_with("Theme parser warning: gtk.css:"))
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(contents) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(std::path::Path::to_owned)
    else {
        return;
    };

    let share = contents.join("Resources").join("share");
    if !share.exists() {
        return;
    }

    let schemas = share.join("glib-2.0").join("schemas");
    if schemas.exists() {
        env::set_var("GSETTINGS_SCHEMA_DIR", schemas);
    }

    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var(
        "GTK_DATA_PREFIX",
        contents.join("Resources").to_string_lossy().as_ref(),
    );
}

fn load_css() {
    let provider = CssProvider::default();
    provider.load_from_resource("de/feschber/LanMouse/style.css");
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/de/feschber/LanMouse/icons");
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            app.quit();
        }
    });
    app.add_action(&quit_action);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    menu.append_submenu(Some("_File"), &file_menu);

    app.set_menubar(Some(&menu))
}

fn build_ui(
    app: &Application,
    (mut frontend_rx, frontend_tx): (
        lan_mouse_ipc::FrontendEventReader,
        lan_mouse_ipc::FrontendRequestWriter,
    ),
) {
    // If a window already exists (re-activation), just present it
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }

    log::debug!("connected to lan-mouse-socket");

    let (sender, receiver) = async_channel::bounded(10);

    gio::spawn_blocking(move || {
        while let Some(e) = frontend_rx.next_event() {
            match e {
                Ok(e) => {
                    if sender.send_blocking(e).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    log::error!("{e}");
                    break;
                }
            }
        }
    });

    let window = Window::new(app, frontend_tx);
    #[cfg(target_os = "macos")]
    {
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        macos_status_item::setup(app, &window);
        // First-launch TCC prompts. No-op when already granted.
        macos_privacy::fire_initial_prompts();
        // Watch the Accessibility grant continuously for the lifetime
        // of the process. On a grant, swap the warning row into its
        // "relaunch required" state (the daemon subprocess already
        // bailed and can't recover without a restart). On a REVOKE,
        // quit immediately — an active CGEventTap at
        // HeadInsertEventTap can wedge system input if the process
        // lingers after losing AX, and forcing the process to exit is
        // the only bulletproof way to guarantee the kernel tears the
        // tap down.
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        macos_privacy::watch_accessibility_state(move |change| match change {
            macos_privacy::AccessibilityChange::Granted => {
                if let Some(window) = window_weak.upgrade() {
                    window.present();
                    window.refresh_capture_emulation_status();
                }
            }
            macos_privacy::AccessibilityChange::Revoked => {
                log::warn!("Accessibility revoked — quitting to avoid wedging system input");
                if let Some(app) = app_weak.upgrade() {
                    app.quit();
                }
            }
        });
    }

    glib::spawn_future_local(clone!(
        #[weak]
        window,
        async move {
            loop {
                let notify = receiver.recv().await.unwrap_or_else(|_| process::exit(1));
                match notify {
                    FrontendEvent::InstanceInfo { .. } => {}
                    FrontendEvent::ShuttingDown => {
                        if let Some(app) = window.application() {
                            app.quit();
                        }
                        break;
                    }
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => window.set_capture(s.into()),
                    FrontendEvent::EmulationStatus(s) => window.set_emulation(s.into()),
                    FrontendEvent::AuthorizedUpdated(keys) => window.set_authorized_keys(keys),
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        window.request_authorization(&fingerprint);
                    }
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        window.show_toast(format!("device connected: {addr}").as_str());
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos,
                    } => {
                        window.show_toast(format!("device entered: {addr} ({pos})").as_str());
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        window.show_toast(format!("{addr} disconnected").as_str());
                    }
                }
            }
        }
    ));

    #[cfg(not(target_os = "macos"))]
    window.present();

    // On macOS, default to presenting the main window on every launch
    // so the user gets a visible confirmation that the app is running
    // — including the post-grant relaunch and normal Dock/Finder/`open`
    // launches. Opt out by setting `LAN_MOUSE_HIDDEN=1` in the
    // environment (useful for a LaunchAgent / login-item configuration
    // where the user wants the app to come up quietly into the menu
    // bar only, with no window on boot).
    #[cfg(target_os = "macos")]
    if env::var_os("LAN_MOUSE_HIDDEN").is_none() {
        window.present();
    }
}
