//! Cross-machine link test: one daemon per machine, dummy capture and
//! emulation, exercising connection setup, certificate authorization and
//! event delivery over the real network. See DOC.md for how to run it.
//!
//! The receiver approves every fingerprint that tries to connect. This is
//! only acceptable because it always runs with the dummy emulation backend.
use std::{
    env, fs,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

use lan_mouse_ipc::{FrontendEvent, FrontendRequest, FrontendRequestWriter};

/// Input events the receiver must see before it passes (about half a second
/// of the dummy capture's 1 kHz motion stream).
const REQUIRED_EVENTS: usize = 500;
/// How long the sender keeps sending after the receiver acknowledged.
const SEND_DURATION: Duration = Duration::from_secs(5);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_PORT: u16 = 4242;

/// Log line written by the dummy emulation backend for every input event.
const RECEIVED_MARKER: &str = "received event:";
/// Log line written by the capture side once the peer acknowledged `Enter`.
const ACK_MARKER: &str = "acknowledged the connection";

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

enum Msg {
    Log(String),
    Ipc(Box<FrontendEvent>),
}

enum Role {
    Send { peer: String },
    Receive,
}

#[test]
#[ignore = "needs a second machine; run with LAN_MOUSE_LINK_ROLE set on each side"]
fn cross_machine_link() {
    let Some(role) = role_from_env() else {
        eprintln!("LAN_MOUSE_LINK_ROLE not set; skipping cross-machine link test");
        return;
    };
    let port = env::var("LAN_MOUSE_LINK_PORT")
        .map(|p| {
            p.parse()
                .expect("LAN_MOUSE_LINK_PORT must be a port number")
        })
        .unwrap_or(DEFAULT_PORT);
    let timeout = env::var("LAN_MOUSE_LINK_TIMEOUT_SECS")
        .map(|s| Duration::from_secs(s.parse().expect("LAN_MOUSE_LINK_TIMEOUT_SECS")))
        .unwrap_or(DEFAULT_TIMEOUT);
    assert!(
        !lan_mouse_ipc::is_service_running(),
        "a Lan Mouse daemon is already running on this machine; stop it first"
    );

    let directory = env::temp_dir().join(format!("lan-mouse-link-{}", std::process::id()));
    fs::create_dir_all(&directory).unwrap();
    let config = directory.join("config.toml");
    fs::write(&config, config_toml(&role, port)).unwrap();

    let mut daemon = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_lan-mouse"))
            .args(["--capture-backend", "dummy", "--emulation-backend", "dummy"])
            .arg("--config")
            .arg(&config)
            .arg("--cert-path")
            .arg(directory.join("link-test.pem"))
            .arg("daemon")
            .env("LAN_MOUSE_LOG_LEVEL", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (tx, rx) = mpsc::channel();
    forward_logs(&mut daemon, tx.clone());
    let mut writer = connect_ipc(&mut daemon, tx);

    let result = match &role {
        Role::Receive => {
            println!("receiving on port {port}; start the sender within {timeout:?}");
            receive(&rx, &mut writer, timeout)
        }
        Role::Send { peer } => {
            println!("sending to {peer}:{port}; waiting up to {timeout:?} for the receiver");
            send(&rx, timeout)
        }
    };

    let pid = daemon.0.id();
    if writer.request(FrontendRequest::Shutdown { pid }).is_ok() {
        await_exit(&mut daemon.0, Duration::from_secs(15));
    }
    drop(daemon);
    // Only files created by this test, under its unique temporary directory.
    fs::remove_dir_all(&directory).unwrap();

    if let Err(e) = result {
        panic!("{e}");
    }
    println!("PASS");
}

fn role_from_env() -> Option<Role> {
    let role = env::var("LAN_MOUSE_LINK_ROLE").ok()?;
    match role.as_str() {
        "receive" => Some(Role::Receive),
        "send" => {
            let peer = env::var("LAN_MOUSE_LINK_PEER")
                .expect("LAN_MOUSE_LINK_PEER (receiver IP or hostname) is required to send");
            Some(Role::Send { peer })
        }
        _ => panic!("LAN_MOUSE_LINK_ROLE must be `send` or `receive`, got `{role}`"),
    }
}

/// The dummy capture leaves through the left edge, so the peer goes there.
fn config_toml(role: &Role, port: u16) -> String {
    let mut config = format!("port = {port}\n");
    if let Role::Send { peer } = role {
        let address = if peer.parse::<std::net::IpAddr>().is_ok() {
            format!("ips = [\"{peer}\"]")
        } else {
            format!("hostname = \"{peer}\"")
        };
        config.push_str(&format!(
            "\n[[clients]]\nposition = \"left\"\n{address}\nport = {port}\nactivate_on_startup = true\n"
        ));
    }
    config
}

fn forward_logs(daemon: &mut OwnedChild, tx: Sender<Msg>) {
    let stderr = daemon.0.stderr.take().unwrap();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(Msg::Log(line)).is_err() {
                break;
            }
        }
    });
}

fn connect_ipc(daemon: &mut OwnedChild, tx: Sender<Msg>) -> FrontendRequestWriter {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut reader, writer) = loop {
        match lan_mouse_ipc::connect_verified(daemon.0.id(), Duration::from_millis(500)) {
            Ok(connection) => break connection,
            Err(e) => {
                if let Some(status) = daemon.0.try_wait().unwrap() {
                    panic!("daemon exited during startup ({status})");
                }
                assert!(Instant::now() < deadline, "daemon readiness timed out: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    };
    thread::spawn(move || {
        while let Some(Ok(event)) = reader.next_event() {
            if tx.send(Msg::Ipc(Box::new(event))).is_err() {
                break;
            }
        }
    });
    writer
}

fn receive(
    rx: &Receiver<Msg>,
    writer: &mut FrontendRequestWriter,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut received = 0;
    while received < REQUIRED_EVENTS {
        let msg = next(rx, deadline)
            .map_err(|e| format!("{e} after {received} of {REQUIRED_EVENTS} input events"))?;
        match msg {
            Msg::Log(line) if line.contains(RECEIVED_MARKER) => {
                if received == 0 {
                    println!("first input event arrived");
                }
                received += 1;
            }
            Msg::Log(line) => println!("[daemon] {line}"),
            Msg::Ipc(event) => {
                if let FrontendEvent::ConnectionAttempt { fingerprint } = *event {
                    println!("authorizing peer {fingerprint}");
                    writer
                        .request(FrontendRequest::AuthorizeKey(
                            "link test peer".into(),
                            fingerprint,
                        ))
                        .map_err(|e| format!("IPC request failed: {e}"))?;
                }
            }
        }
    }
    println!("received {received} input events");
    Ok(())
}

fn send(rx: &Receiver<Msg>, timeout: Duration) -> Result<(), String> {
    let mut deadline = Instant::now() + timeout;
    let mut acknowledged = false;
    loop {
        match next(rx, deadline) {
            Ok(Msg::Log(line)) => {
                if !acknowledged && line.contains(ACK_MARKER) {
                    acknowledged = true;
                    deadline = Instant::now() + SEND_DURATION;
                    println!("receiver acknowledged; sending for {SEND_DURATION:?}");
                }
                println!("[daemon] {line}");
            }
            Ok(Msg::Ipc(_)) => {}
            Err(_) if acknowledged => return Ok(()),
            Err(e) => return Err(format!("{e} before the receiver acknowledged")),
        }
    }
}

fn next(rx: &Receiver<Msg>, deadline: Instant) -> Result<Msg, String> {
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(msg) => Ok(msg),
        Err(RecvTimeoutError::Timeout) => Err("timed out".into()),
        Err(RecvTimeoutError::Disconnected) => Err("daemon exited".into()),
    }
}

fn await_exit(child: &mut Child, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
}
