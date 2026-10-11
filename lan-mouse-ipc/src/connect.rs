use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    io::{self, BufReader, LineWriter, prelude::*},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::net::TcpStream;

pub struct FrontendEventReader {
    #[cfg(unix)]
    reader: BufReader<UnixStream>,
    #[cfg(windows)]
    reader: BufReader<TcpStream>,
}

pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    #[cfg(windows)]
    line_writer: LineWriter<TcpStream>,
}

impl FrontendEventReader {
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => None,
            Err(e) => Some(Err(e.into())),
            Ok(_) => Some(serde_json::from_str(&line).map_err(|e| e.into())),
        }
    }
}

impl FrontendRequestWriter {
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request).unwrap();
        log::debug!("requesting: {json}");
        json.push('\n');
        self.line_writer.write_all(json.as_bytes())?;
        Ok(())
    }
}

/// Try to connect to the service once without retrying.
pub fn try_connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    let rx = {
        let socket_path = crate::default_socket_path()?;
        UnixStream::connect(&socket_path)?
    };
    #[cfg(windows)]
    let rx = TcpStream::connect("127.0.0.1:5252")?;
    make_connection(rx)
}

/// Connect to a specific newly started daemon and retain that exact connection.
/// Call before entering an async runtime. Both connect and identity reads are bounded.
pub fn connect_verified(
    expected_pid: u32,
    timeout: Duration,
) -> Result<(FrontendEventReader, FrontendRequestWriter), IpcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let stream = runtime.block_on(async {
        #[cfg(unix)]
        let connect = tokio::net::UnixStream::connect(crate::default_socket_path()?);
        #[cfg(windows)]
        let connect = tokio::net::TcpStream::connect("127.0.0.1:5252");
        let stream = tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| ConnectionError::Timeout)??;
        Ok::<_, ConnectionError>(stream.into_std()?)
    })?;
    verify_stream(stream, expected_pid, timeout)
}

#[cfg(windows)]
type LocalStream = TcpStream;
#[cfg(unix)]
type LocalStream = UnixStream;

fn verify_stream(
    stream: LocalStream,
    expected_pid: u32,
    timeout: Duration,
) -> Result<(FrontendEventReader, FrontendRequestWriter), IpcError> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let (mut reader, mut writer) = make_connection(stream)?;
    writer.request(FrontendRequest::Identify)?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(ConnectionError::Timeout.into());
        }
        reader.reader.get_ref().set_read_timeout(Some(
            deadline.saturating_duration_since(std::time::Instant::now()),
        ))?;
        match reader.next_event().transpose()? {
            Some(FrontendEvent::InstanceInfo { pid }) if pid == expected_pid => break,
            Some(FrontendEvent::InstanceInfo { .. }) | None => {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "IPC endpoint does not belong to the newly launched daemon",
                )
                .into());
            }
            _ => {}
        }
    }
    reader.reader.get_ref().set_read_timeout(None)?;
    writer.line_writer.get_ref().set_write_timeout(None)?;
    // Identity verification may have consumed the initial snapshot.
    writer.request(FrontendRequest::Sync)?;
    Ok((reader, writer))
}

pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let rx = wait_for_service()?;
    make_connection(rx)
}

#[cfg(unix)]
fn make_connection(
    rx: UnixStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { reader: buf_reader };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

#[cfg(windows)]
fn make_connection(
    rx: TcpStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { reader: buf_reader };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// wait for the lan-mouse socket to come online
#[cfg(unix)]
fn wait_for_service() -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = UnixStream::connect(&socket_path) {
            break Ok(stream);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        thread::sleep(exponential_back_off(&mut duration));
    }
}

#[cfg(windows)]
fn wait_for_service() -> Result<TcpStream, ConnectionError> {
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = TcpStream::connect("127.0.0.1:5252") {
            break Ok(stream);
        }
        thread::sleep(exponential_back_off(&mut duration));
    }
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (LocalStream, LocalStream) {
        #[cfg(unix)]
        {
            UnixStream::pair().unwrap()
        }
        #[cfg(windows)]
        {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (server, _) = listener.accept().unwrap();
            (client, server)
        }
    }

    fn fake_daemon(mut stream: LocalStream, pid: u32) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "\"Identify\"");
        writeln!(stream, "{{\"InstanceInfo\":{{\"pid\":{pid}}}}}").unwrap();
    }

    #[test]
    fn verified_connection_rejects_another_daemon() {
        let (client, server) = pair();
        let worker = thread::spawn(move || fake_daemon(server, 101));
        assert!(verify_stream(client, 102, Duration::from_secs(1)).is_err());
        worker.join().unwrap();
    }

    #[test]
    fn verified_connection_retains_socket_and_requests_fresh_state() {
        let (client, mut server) = pair();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let worker = thread::spawn(move || {
            fake_daemon(server.try_clone().unwrap(), 101);
            let mut reader = BufReader::new(server.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "\"Sync\"");
            // Past the handshake deadline, ordinary event reads must still work.
            thread::sleep(Duration::from_millis(200));
            writeln!(server, "\"ShuttingDown\"").unwrap();
        });
        let (mut reader, _) = verify_stream(client, 101, Duration::from_millis(100)).unwrap();
        assert!(matches!(
            reader.next_event(),
            Some(Ok(FrontendEvent::ShuttingDown))
        ));
        worker.join().unwrap();
    }

    #[test]
    fn silent_endpoint_has_a_bounded_identity_wait() {
        let (client, _server) = pair();
        let started = std::time::Instant::now();
        assert!(verify_stream(client, 101, Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
