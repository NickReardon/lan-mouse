use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;
#[cfg(feature = "gtk")]
use lan_mouse_gtk::GtkError;
use lan_mouse_ipc::IpcError;
#[cfg(feature = "gtk")]
use std::process::Child;
use std::{future::Future, io, process};
use thiserror::Error;
use tokio::task::LocalSet;

mod instance;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Startup(#[from] instance::StartupError),
}

fn main() {
    // init logging
    let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    // Keep startup dialogs and GTK on the same thread, with GTK's required stack.
    #[cfg(windows)]
    let result = std::thread::Builder::new()
        .name("lan-mouse".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(run)
        .expect("could not start launcher thread")
        .join()
        .expect("launcher thread panicked");
    #[cfg(not(windows))]
    let result = run();
    if let Err(e) = result {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                let Some(startup) = instance::prepare(false, true)? else {
                    return Ok(());
                };
                run_async(run_service(config, startup.guard))?;
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "gtk")]
            {
                if let Err(error) = run_gui() {
                    lan_mouse_gtk::show_startup_error(&error.to_string());
                    return Err(error);
                }
            }
            #[cfg(not(feature = "gtk"))]
            {
                let Some(startup) = instance::prepare(false, true)? else {
                    return Ok(());
                };
                run_async(run_service(config, startup.guard))?;
            }
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

#[cfg(feature = "gtk")]
fn run_gui() -> Result<(), LanMouseError> {
    let Some(startup) = instance::prepare(true, false)? else {
        return Ok(());
    };
    let mut command = process::Command::new(std::env::current_exe()?);
    command.args(std::env::args_os().skip(1)).arg("daemon");
    instance::mark_internal_child(&mut command);
    let mut service = command.spawn()?;
    let ready = instance::wait_until_ready(&mut service);
    drop(startup);
    let connection = match ready {
        Ok(connection) => connection,
        Err(error) => {
            stop_child(&mut service)?;
            return Err(error.into());
        }
    };
    let result = lan_mouse_gtk::run(config::local_commit(), connection);
    stop_child(&mut service)?;
    result?;
    Ok(())
}

#[cfg(feature = "gtk")]
fn stop_child(child: &mut Child) -> Result<(), io::Error> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    instance::request_shutdown(child.id());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    log::warn!("new child daemon did not shut down; terminating owned child");
    child.kill()?;
    child.wait()?;
    Ok(())
}

async fn run_service(
    config: Config,
    guard: Option<instance::StartupGuard>,
) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    drop(guard);
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
