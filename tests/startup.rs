//! Explicitly run with `cargo test --test startup -- --ignored --nocapture`.
//! Windows uses the real local IPC port, so refuse to run if it is occupied.
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
type Stream = std::net::TcpStream;
#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn await_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "child timed out");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "uses the local IPC endpoint; run explicitly when no Lan Mouse is running"]
fn daemon_identity_targeted_shutdown_and_unattended_conflict() {
    #[cfg(windows)]
    drop(
        std::net::TcpListener::bind("127.0.0.1:5252")
            .expect("IPC occupied: do not disturb the existing instance"),
    );
    let directory = std::env::temp_dir().join(format!(
        "lan-mouse-startup-integration-{}",
        std::process::id()
    ));
    fs::create_dir_all(directory.join("Library/Caches")).unwrap();
    let config = directory.join("config.toml");
    fs::write(&config, "").unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_lan-mouse"));
    command
        .args([
            "--capture-backend",
            "dummy",
            "--emulation-backend",
            "dummy",
            "--port",
            "0",
            "--config",
        ])
        .arg(&config)
        .arg("--cert-path")
        .arg(directory.join("test.pem"))
        .arg("daemon")
        .stdin(Stdio::null())
        .env("XDG_RUNTIME_DIR", &directory)
        .env("HOME", &directory);
    let mut daemon = OwnedChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        #[cfg(windows)]
        let result = Stream::connect("127.0.0.1:5252");
        #[cfg(all(unix, not(target_os = "macos")))]
        let result = Stream::connect(directory.join("lan-mouse-socket.sock"));
        #[cfg(target_os = "macos")]
        let result = Stream::connect(directory.join("Library/Caches/lan-mouse-socket.sock"));
        if let Ok(stream) = result {
            break stream;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited during startup"
        );
        assert!(Instant::now() < deadline, "daemon readiness timed out");
        thread::sleep(Duration::from_millis(50));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    writer.write_all(b"\"Identify\"\n").unwrap();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        if let Some(info) = event.get("InstanceInfo") {
            assert_eq!(info["pid"].as_u64(), Some(daemon.0.id() as u64));
            break;
        }
    }
    // An incorrect target must never shut down a newer/replacement daemon.
    writeln!(writer, "{{\"Shutdown\":{{\"pid\":0}}}}").unwrap();
    thread::sleep(Duration::from_millis(150));
    assert!(daemon.0.try_wait().unwrap().is_none());

    let mut duplicate = OwnedChild(command.stderr(Stdio::piped()).spawn().unwrap());
    assert!(!await_exit(&mut duplicate.0).success());
    let output = std::io::read_to_string(duplicate.0.stderr.take().unwrap()).unwrap();
    assert!(
        output.contains("unattended launch cannot confirm replacement"),
        "{output}"
    );
    assert!(daemon.0.try_wait().unwrap().is_none());

    writeln!(writer, "{{\"Shutdown\":{{\"pid\":{}}}}}", daemon.0.id()).unwrap();
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "shutdown notification missing"
        );
        if line.trim() == "\"ShuttingDown\"" {
            break;
        }
    }
    assert!(await_exit(&mut daemon.0).success());
    drop(writer);
    drop(reader);
    drop(daemon);
    drop(duplicate);
    // Only files created by this test, under its unique temporary directory.
    fs::remove_file(directory.join("test.pem")).unwrap();
    fs::remove_file(config).unwrap();
    #[cfg(unix)]
    fs::remove_file(directory.join("lan-mouse-startup.lock"))
        .or_else(|_| fs::remove_file(directory.join("Library/Caches/lan-mouse-startup.lock")))
        .unwrap();
    fs::remove_dir(directory.join("Library/Caches")).unwrap();
    fs::remove_dir(directory.join("Library")).unwrap();
    fs::remove_dir(directory).unwrap();
}
