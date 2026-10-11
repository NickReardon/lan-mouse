//! Startup coordination is synchronous and runs before the Tokio/GTK event loops.
use fs2::FileExt;
use lan_mouse_ipc::{FrontendEvent, FrontendRequest};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use thiserror::Error;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const INTERNAL_PARENT: &str = "LAN_MOUSE_STARTUP_PARENT";

#[derive(Debug, Error)]
pub enum StartupError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{0}")]
    Conflict(String),
}

/// Held only until the new daemon is ready, never for the frontend's lifetime.
pub struct StartupGuard(File);

pub struct Startup {
    pub guard: Option<StartupGuard>,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Instance {
    pid: Pid,
    started: u64,
    executable: PathBuf,
    user: sysinfo::Uid,
    daemon: bool,
}

fn refresh(system: &mut System) {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
}

fn matches_identity(instance: &Instance, process: &Process) -> bool {
    process.pid() == instance.pid
        && process.start_time() == instance.started
        && process.exe() == Some(instance.executable.as_path())
        && process.user_id() == Some(&instance.user)
}

fn is_control_command(args: &[std::ffi::OsString]) -> bool {
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some(
                "-p"
                | "--port"
                | "-c"
                | "--config"
                | "--capture-backend"
                | "--emulation-backend"
                | "--cert-path",
            ) => {
                args.next();
            }
            Some("cli" | "test-capture" | "test-emulation") => return true,
            Some("daemon") => return false,
            _ => {}
        }
    }
    false
}

// Name alone is insufficient: check the on-disk executable for multiple
// application-specific markers as well as OS-reported path, user, and lifetime.
// This is identification of local builds, not a signature/trust mechanism.
fn is_lan_mouse_binary(path: &Path) -> bool {
    const MARKERS: [&[u8]; 2] = [b"service already running!", b"terminating service ..."];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut found = [false; 2];
    let mut buffer = vec![0; 64 * 1024];
    let mut tail = Vec::new();
    loop {
        let Ok(len) = file.read(&mut buffer) else {
            return false;
        };
        if len == 0 {
            return found.iter().all(|v| *v);
        }
        tail.extend_from_slice(&buffer[..len]);
        for (marker, present) in MARKERS.iter().zip(found.iter_mut()) {
            *present |= tail.windows(marker.len()).any(|window| window == *marker);
        }
        if found.iter().all(|v| *v) {
            return true;
        }
        tail.drain(..tail.len().saturating_sub(64));
    }
}

fn instances(system: &System, current: Pid) -> Result<Vec<Instance>, StartupError> {
    let user = system
        .process(current)
        .and_then(Process::user_id)
        .ok_or_else(|| {
            StartupError::Conflict("Could not verify the current process owner.".into())
        })?;
    let mut binaries = HashMap::new();
    let mut result = Vec::new();
    for process in system.processes().values() {
        if process.pid() == current || process.user_id() != Some(user) {
            continue;
        }
        let Some(executable) = process.exe() else {
            continue;
        };
        if executable.file_stem().and_then(|s| s.to_str()) != Some("lan-mouse")
            || is_control_command(process.cmd())
        {
            continue;
        }
        if !*binaries
            .entry(executable.to_owned())
            .or_insert_with(|| is_lan_mouse_binary(executable))
        {
            continue;
        }
        result.push(Instance {
            pid: process.pid(),
            started: process.start_time(),
            executable: executable.to_owned(),
            user: user.clone(),
            daemon: process.cmd().iter().any(|arg| arg == "daemon"),
        });
    }
    result.sort_by_key(|instance| (!instance.daemon, instance.pid));
    Ok(result)
}

fn lock_startup() -> Result<StartupGuard, StartupError> {
    #[cfg(unix)]
    let directory = lan_mouse_ipc::default_socket_path()
        .map_err(|e| StartupError::Conflict(e.to_string()))?
        .parent()
        .expect("IPC path has a parent")
        .to_owned();
    #[cfg(windows)]
    let directory = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| StartupError::Conflict("LOCALAPPDATA is unavailable.".into()))?
        .join("Lan Mouse");
    fs::create_dir_all(&directory)?;
    // Do not unlink the lock: concurrent launchers must always lock the same inode.
    lock_file(&directory.join("lan-mouse-startup.lock"))
}

fn lock_file(path: &Path) -> Result<StartupGuard, StartupError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock_exclusive().map_err(|e| {
        StartupError::Conflict(format!(
            "Another Lan Mouse launch is in progress ({e}). Try again when it finishes."
        ))
    })?;
    Ok(StartupGuard(file))
}

#[cfg(feature = "gtk")]
pub fn mark_internal_child(command: &mut std::process::Command) {
    command.env(INTERNAL_PARENT, std::process::id().to_string());
}

fn is_internal_child(system: &System, current: Pid) -> bool {
    let expected = std::env::var(INTERNAL_PARENT).ok();
    let process = system.process(current);
    let parent = process
        .and_then(Process::parent)
        .and_then(|pid| system.process(pid));
    let (Some(process), Some(parent)) = (process, parent) else {
        if expected.is_some() {
            log::warn!(
                "internal daemon check failed: own process found={} parent pid={:?} found={}",
                process.is_some(),
                process.and_then(Process::parent),
                parent.is_some()
            );
        }
        return false;
    };
    let verified = expected.as_deref() == Some(&parent.pid().to_string())
        && parent.exe().is_some()
        && parent.exe() == process.exe()
        && parent.user_id().is_some()
        && parent.user_id() == process.user_id();
    if expected.is_some() && !verified {
        log::warn!(
            "internal daemon check failed: expected parent {expected:?}, actual {}; exe {:?} vs parent {:?}; user {:?} vs parent {:?}",
            parent.pid(),
            process.exe(),
            parent.exe(),
            process.user_id(),
            parent.user_id()
        );
    }
    verified
}

pub fn confirm(gui: bool, force: bool, details: &str) -> Result<bool, StartupError> {
    #[cfg(feature = "gtk")]
    if gui {
        return lan_mouse_gtk::confirm_replacement(force, details)
            .map_err(|e| StartupError::Conflict(e.to_string()));
    }
    let _ = gui;
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(StartupError::Conflict(
            "Lan Mouse is already running. An unattended launch cannot confirm replacement; use the GUI or an interactive terminal.".into(),
        ));
    }
    let action = if force {
        "Force close the listed processes and start this version?"
    } else {
        "Close existing Lan Mouse and start this version?"
    };
    eprint!("{details}\n{action} [y/N] ");
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn describe(existing: &[Instance]) -> String {
    let mut details = format!(
        "New executable: {}\n\nExisting processes:\n",
        std::env::current_exe().unwrap_or_default().display()
    );
    for instance in existing {
        details.push_str(&format!(
            "{} (PID {}, {}): {}\n",
            if instance.daemon { "Daemon" } else { "App" },
            instance.pid,
            instance.started,
            instance.executable.display()
        ));
    }
    details.push_str("\nInput sharing will stop briefly during replacement.");
    details
}

async fn identify_async() -> Option<u32> {
    let (mut rx, mut tx) = lan_mouse_ipc::connect_async(Some(Duration::from_millis(300)))
        .await
        .ok()?;
    tx.request(FrontendRequest::Identify).await.ok()?;
    use futures::StreamExt;
    while let Some(Ok(event)) = rx.next().await {
        if let FrontendEvent::InstanceInfo { pid } = event {
            return Some(pid);
        }
    }
    None
}

fn identify() -> Option<u32> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(500), identify_async())
            .await
            .ok()
            .flatten()
    })
}

pub fn request_shutdown(pid: u32) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let shutdown = async {
            let (mut events, mut writer) =
                lan_mouse_ipc::connect_async(Some(Duration::from_millis(300))).await?;
            writer.request(FrontendRequest::Shutdown { pid }).await?;
            // Stay connected until the daemon confirms. On Windows, closing the
            // TCP connection right after writing (with broadcasts still unread)
            // can reset it before the daemon reads the request.
            use futures::StreamExt;
            while let Some(event) = events.next().await {
                if matches!(event?, FrontendEvent::ShuttingDown) {
                    break;
                }
            }
            Ok::<(), lan_mouse_ipc::IpcError>(())
        };
        match tokio::time::timeout(Duration::from_secs(1), shutdown).await {
            Ok(Ok(())) => {}
            error => log::warn!("could not request daemon shutdown: {error:?}"),
        }
    });
}

fn remaining(system: &mut System, original: &[Instance]) -> Vec<Instance> {
    refresh(system);
    original
        .iter()
        .filter(|instance| {
            system
                .process(instance.pid)
                .is_some_and(|p| matches_identity(instance, p))
        })
        .cloned()
        .collect()
}

fn wait_for_exit(system: &mut System, original: &[Instance]) -> Vec<Instance> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    loop {
        let remaining = remaining(system, original);
        if remaining.is_empty() || Instant::now() >= deadline {
            return remaining;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn signal(system: &mut System, instance: &Instance, force: bool) -> Result<(), StartupError> {
    refresh(system);
    let Some(process) = system.process(instance.pid) else {
        return Ok(());
    };
    if !matches_identity(instance, process) {
        return Ok(());
    }
    let sent = if force {
        process.kill()
    } else {
        // Windows does not support sysinfo's non-force signals; ask separately
        // before using TerminateProcess on a legacy process.
        process
            .kill_with(sysinfo::Signal::Interrupt)
            .unwrap_or(false)
    };
    // Windows refuses to terminate a process that is already exiting, as the
    // app does once its daemon is closed. Only fail if it is still running.
    if force && !sent && !wait_for_exit(system, std::slice::from_ref(instance)).is_empty() {
        return Err(StartupError::Conflict(format!(
            "Could not close PID {}. Check process permissions.",
            instance.pid
        )));
    }
    Ok(())
}

/// None means the user cancelled. Internal children inherit the launch decision.
pub fn prepare(gui: bool, daemon: bool) -> Result<Option<Startup>, StartupError> {
    let current = Pid::from_u32(std::process::id());
    let mut system = System::new();
    refresh(&mut system);
    if daemon && is_internal_child(&system, current) {
        return Ok(Some(Startup { guard: None }));
    }
    let guard = lock_startup()?;
    refresh(&mut system);
    let existing = instances(&system, current)?;
    if existing.is_empty() {
        if lan_mouse_ipc::is_service_running() {
            return Err(StartupError::Conflict("The local IPC endpoint is occupied, but its owner could not be verified as Lan Mouse. Nothing was closed.".into()));
        }
        return Ok(Some(Startup { guard: Some(guard) }));
    }
    if !confirm(gui, false, &describe(&existing))? {
        return Ok(None);
    }
    // Only send lifecycle commands after the endpoint identifies a verified daemon.
    // A build without GTK can run the service without a `daemon` argument.
    let cooperative = identify().filter(|pid| existing.iter().any(|p| p.pid.as_u32() == *pid));
    if let Some(pid) = cooperative {
        request_shutdown(pid);
    }
    for instance in &existing {
        if cooperative.is_none() || instance.daemon && cooperative != Some(instance.pid.as_u32()) {
            signal(&mut system, instance, false)?;
        }
    }
    let lingering = wait_for_exit(&mut system, &existing);
    if !lingering.is_empty() {
        let details = format!(
            "{}\n\nThese processes did not close gracefully. Force closing may skip input cleanup.",
            describe(&lingering)
        );
        if !confirm(gui, true, &details)? {
            return Ok(None);
        }
        for instance in &lingering {
            signal(&mut system, instance, true)?;
        }
        if !wait_for_exit(&mut system, &lingering).is_empty() {
            return Err(StartupError::Conflict(
                "Timed out closing the existing processes. The new daemon was not started.".into(),
            ));
        }
    }
    if !instances(&system, current)?.is_empty() || lan_mouse_ipc::is_service_running() {
        return Err(StartupError::Conflict("Another instance appeared during replacement, possibly restarted by a service manager. The new daemon was not started. Stop or pause the supervising service before retrying.".into()));
    }
    Ok(Some(Startup { guard: Some(guard) }))
}

#[cfg(feature = "gtk")]
pub fn wait_until_ready(
    child: &mut std::process::Child,
) -> Result<
    (
        lan_mouse_ipc::FrontendEventReader,
        lan_mouse_ipc::FrontendRequestWriter,
    ),
    StartupError,
> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(StartupError::Conflict(format!(
                "The new daemon exited before becoming ready ({status})."
            )));
        }
        if let Ok(connection) =
            lan_mouse_ipc::connect_verified(child.id(), Duration::from_millis(500))
        {
            return Ok(connection);
        }
        if Instant::now() >= deadline {
            return Err(StartupError::Conflict("Timed out waiting for the new daemon. The frontend was not connected to another instance.".into()));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_commands_are_not_replacement_targets() {
        for command in ["cli", "test-capture", "test-emulation"] {
            assert!(is_control_command(&["lan-mouse".into(), command.into()]));
        }
        assert!(!is_control_command(&["lan-mouse".into(), "daemon".into()]));
        assert!(!is_control_command(&["lan-mouse".into()]));
        assert!(!is_control_command(&[
            "lan-mouse".into(),
            "--config".into(),
            "cli".into()
        ]));
    }

    #[test]
    fn process_identity_rejects_pid_reuse_or_changed_executable() {
        let mut system = System::new();
        refresh(&mut system);
        let process = system.process(Pid::from_u32(std::process::id())).unwrap();
        let mut instance = Instance {
            pid: process.pid(),
            started: process.start_time(),
            executable: process.exe().unwrap().to_owned(),
            user: process.user_id().unwrap().clone(),
            daemon: false,
        };
        assert!(matches_identity(&instance, process));
        instance.started += 1;
        assert!(!matches_identity(&instance, process));
        instance.started = process.start_time();
        instance.executable = PathBuf::from("unrelated");
        assert!(!matches_identity(&instance, process));
    }

    #[test]
    fn executable_name_does_not_verify_an_unrelated_binary() {
        assert!(!is_lan_mouse_binary(Path::new("Cargo.toml")));
    }

    #[test]
    fn startup_lock_serializes_launches_and_releases_on_drop() {
        let path = std::env::temp_dir().join(format!("lan-mouse-lock-test-{}", std::process::id()));
        let first = lock_file(&path).unwrap();
        assert!(lock_file(&path).is_err());
        drop(first);
        drop(lock_file(&path).unwrap());
        fs::remove_file(path).unwrap();
    }
}
