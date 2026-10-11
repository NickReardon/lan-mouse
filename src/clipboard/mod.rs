//! Opt-in, on-switch text sharing (NickReardon/lan-mouse#1).
mod platform;
mod protocol;
mod tls;

use lan_mouse_ipc::{ClientConfig, ClientState};
use platform::ClipboardWorker;
use protocol::{PULL, PUSH, invalid, read_header, read_text, write_header, write_text};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    io,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use webrtc_dtls::crypto::Certificate;

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Mode {
    #[default]
    Off,
    OnSwitch,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub mode: Mode,
    /// Configured client hostname (or IP) -> expected certificate fingerprint.
    pub peers: HashMap<String, String>,
}

pub(crate) struct Clipboard {
    settings: Settings,
    cert: Certificate,
    epoch: Arc<AtomicU64>,
    worker: ClipboardWorker,
    session: [u8; 16],
    sequence: u64,
    listener: JoinHandle<()>,
    transfer: Option<JoinHandle<()>>,
}

impl Clipboard {
    pub async fn new(
        settings: Settings,
        port: u16,
        cert: Certificate,
        authorized: tls::Authorized,
    ) -> io::Result<Option<Self>> {
        if settings.mode == Mode::Off {
            return Ok(None);
        }
        let epoch = Arc::new(AtomicU64::new(0));
        let worker = ClipboardWorker::new(epoch.clone())?;
        let acceptor = TlsAcceptor::from(Arc::new(
            tls::server(&cert, authorized.clone()).map_err(io::Error::other)?,
        ));
        let socket = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).await?;
        let mut session = [0; 16];
        rustls::crypto::ring::default_provider()
            .secure_random
            .fill(&mut session)
            .map_err(|_| io::Error::other("failed to generate clipboard session ID"))?;
        let listener = tokio::task::spawn_local(serve(
            socket,
            acceptor,
            authorized,
            worker.clone(),
            epoch.clone(),
        ));
        log::info!("on-switch clipboard sharing listening on TCP port {port}");
        Ok(Some(Self {
            settings,
            cert,
            epoch,
            worker,
            session,
            sequence: 0,
            listener,
            transfer: None,
        }))
    }

    pub fn cancel(&mut self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(task) = self.transfer.take() {
            task.abort();
        }
    }

    pub fn on_switch(&mut self, config: &ClientConfig, state: &ClientState, returning: bool) {
        self.cancel();
        let Some(addr) = state.active_addr else {
            return;
        };
        let fingerprint = config
            .hostname
            .as_ref()
            .and_then(|name| self.settings.peers.get(name))
            .or_else(|| self.settings.peers.get(&addr.ip().to_string()));
        let Some(fingerprint) = fingerprint else {
            log::warn!(
                "clipboard sharing skipped for {addr}: configure its fingerprint in clipboard.peers"
            );
            return;
        };
        let config = match tls::client(&self.cert, fingerprint.clone()) {
            Ok(config) => config,
            Err(error) => {
                log::warn!("clipboard TLS configuration failed: {error}");
                return;
            }
        };
        let connector = TlsConnector::from(Arc::new(config));
        let worker = self.worker.clone();
        let epoch = self.epoch.clone();
        let expected = epoch.load(Ordering::SeqCst);
        self.sequence += 1;
        let sequence = self.sequence;
        let session = self.session;
        self.transfer = Some(tokio::task::spawn_local(async move {
            let transfer = exchange(
                TcpStream::connect(addr),
                connector,
                worker,
                epoch,
                expected,
                session,
                sequence,
                returning,
            );
            match timeout(TRANSFER_TIMEOUT, transfer).await {
                Ok(Ok(())) => log::debug!("clipboard switch transfer completed with {addr}"),
                Ok(Err(error)) => {
                    log::warn!("clipboard switch transfer with {addr} failed: {error}")
                }
                Err(_) => log::warn!("clipboard switch transfer with {addr} timed out"),
            }
        }));
    }

    pub async fn terminate(mut self) {
        self.cancel();
        self.listener.abort();
        let _ = (&mut self.listener).await;
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        self.cancel();
        self.listener.abort();
    }
}

// Sequence numbers order requests from one process; retired sessions prevent an
// older in-flight request from becoming current after the peer restarts.
#[derive(Default)]
struct UpdateOrder {
    current: Option<([u8; 16], u64)>,
    retired: VecDeque<[u8; 16]>,
}

impl UpdateOrder {
    fn accept(&mut self, session: [u8; 16], sequence: u64) -> bool {
        if self.retired.contains(&session) {
            return false;
        }
        if let Some((previous, last)) = self.current {
            if previous == session && sequence <= last {
                return false;
            }
            if previous != session {
                // Bound memory. Every request also has a five-second lifetime.
                if self.retired.len() == 64 {
                    self.retired.pop_front();
                }
                self.retired.push_back(previous);
            }
        }
        self.current = Some((session, sequence));
        true
    }
    fn is_current(&self, session: [u8; 16], sequence: u64) -> bool {
        self.current == Some((session, sequence))
    }
}

type Orders = Arc<Mutex<HashMap<String, UpdateOrder>>>;

async fn serve(
    socket: TcpListener,
    acceptor: TlsAcceptor,
    authorized: tls::Authorized,
    worker: ClipboardWorker,
    epoch: Arc<AtomicU64>,
) {
    let permits = Arc::new(Semaphore::new(8));
    let orders: Orders = Default::default();
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            connection = socket.accept() => {
                let (stream, addr) = match connection { Ok(connection) => connection, Err(error) => { log::warn!("clipboard listener failed: {error}"); break; } };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let (acceptor, authorized, worker, epoch, orders) = (acceptor.clone(), authorized.clone(), worker.clone(), epoch.clone(), orders.clone());
                let expected = epoch.load(Ordering::SeqCst);
                tasks.spawn_local(async move {
                    let _permit = permit;
                    let result = timeout(TRANSFER_TIMEOUT, receive(stream, acceptor, authorized, worker, epoch, orders, expected)).await;
                    if let Ok(Err(error)) = result { log::debug!("clipboard request from {addr} rejected: {error}"); }
                });
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
}

async fn receive(
    socket: impl AsyncRead + AsyncWrite + Unpin,
    acceptor: TlsAcceptor,
    authorized: tls::Authorized,
    worker: ClipboardWorker,
    epoch: Arc<AtomicU64>,
    orders: Orders,
    expected: u64,
) -> io::Result<()> {
    let mut stream = acceptor.accept(socket).await?;
    let cert = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|chain| chain.first())
        .ok_or_else(|| invalid("missing clipboard client certificate"))?
        .clone();
    let fingerprint = crate::crypto::generate_fingerprint(&cert);
    let (operation, session, sequence) = read_header(&mut stream).await?;
    if epoch.load(Ordering::SeqCst) != expected {
        return Err(invalid("clipboard switch was superseded"));
    }
    {
        let mut orders = orders.lock().await;
        if orders.len() >= 128 && !orders.contains_key(&fingerprint) {
            return Err(invalid("too many clipboard peers"));
        }
        if !orders
            .entry(fingerprint.clone())
            .or_default()
            .accept(session, sequence)
        {
            return Err(invalid("stale clipboard request"));
        }
    }
    if !tls::authorized(&authorized, &cert) {
        return Err(invalid("clipboard client authorization revoked"));
    }
    if operation == PULL {
        let snapshot = worker.read().await?;
        if !tls::authorized(&authorized, &cert) || epoch.load(Ordering::SeqCst) != expected {
            return Err(invalid("clipboard client authorization revoked"));
        }
        write_text(&mut stream, snapshot.text.as_deref()).await?;
    } else {
        let revision = worker.revision().await?;
        let text = read_text(&mut stream).await?;
        let orders = orders.lock().await;
        let mut applied = false;
        if orders
            .get(&fingerprint)
            .is_some_and(|order| order.is_current(session, sequence))
            && tls::authorized(&authorized, &cert)
        {
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                applied = worker.write(text, revision, expected).await?;
            }
        }
        stream.write_u8(u8::from(applied)).await?;
    }
    stream.flush().await?;
    stream.shutdown().await
}

#[allow(clippy::too_many_arguments)]
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    socket: impl Future<Output = io::Result<S>>,
    connector: TlsConnector,
    worker: ClipboardWorker,
    epoch: Arc<AtomicU64>,
    expected: u64,
    session: [u8; 16],
    sequence: u64,
    returning: bool,
) -> io::Result<()> {
    let revision = worker.revision().await?;
    let snapshot = if returning {
        None
    } else {
        Some(worker.read().await?)
    };
    let socket = socket.await?;
    let name = rustls::pki_types::ServerName::try_from("ignored").expect("static server name");
    let mut stream = connector.connect(name, socket).await?;
    if stream.get_ref().1.alpn_protocol() != Some(b"lan-mouse-clipboard/1") {
        return Err(invalid("peer does not support clipboard protocol v1"));
    }
    write_header(
        &mut stream,
        if returning { PULL } else { PUSH },
        session,
        sequence,
    )
    .await?;
    if returning {
        stream.flush().await?;
        if let Some(text) = read_text(&mut stream)
            .await?
            .filter(|text| !text.is_empty())
        {
            worker.write(text, revision, expected).await?;
        }
    } else {
        let snapshot = snapshot.expect("outgoing clipboard snapshot");
        if epoch.load(Ordering::SeqCst) != expected || worker.revision().await? != snapshot.revision
        {
            return Ok(());
        }
        write_text(&mut stream, snapshot.text.as_deref()).await?;
        stream.flush().await?;
        let _applied = stream.read_u8().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connections() -> (TlsAcceptor, TlsConnector, tls::Authorized) {
        let server = Certificate::generate_self_signed(vec!["ignored".into()]).unwrap();
        let client = Certificate::generate_self_signed(vec!["ignored".into()]).unwrap();
        let keys = Arc::new(std::sync::RwLock::new(HashMap::from([(
            crate::crypto::certificate_fingerprint(&client),
            "controller".into(),
        )])));
        (
            TlsAcceptor::from(Arc::new(tls::server(&server, keys.clone()).unwrap())),
            TlsConnector::from(Arc::new(
                tls::client(&client, crate::crypto::certificate_fingerprint(&server)).unwrap(),
            )),
            keys,
        )
    }

    #[tokio::test]
    async fn encrypted_entry_and_return_share_unicode_in_both_directions() {
        let epoch = Arc::new(AtomicU64::new(0));
        let controller = ClipboardWorker::memory(Some("Windows → Mac 🐭"), epoch.clone());
        let remote = ClipboardWorker::memory(Some("old clipboard"), epoch.clone());
        let (acceptor, connector, keys) = connections();
        let orders: Orders = Default::default();
        for returning in [false, true] {
            if returning {
                let revision = remote.revision().await.unwrap();
                remote
                    .write("Mac → Windows 世界".into(), revision, 0)
                    .await
                    .unwrap();
            }
            let (client, server) = tokio::io::duplex(4096);
            let (received, sent) = tokio::join!(
                receive(
                    server,
                    acceptor.clone(),
                    keys.clone(),
                    remote.clone(),
                    epoch.clone(),
                    orders.clone(),
                    0
                ),
                exchange(
                    async { Ok(client) },
                    connector.clone(),
                    controller.clone(),
                    epoch.clone(),
                    0,
                    [1; 16],
                    if returning { 2 } else { 1 },
                    returning
                ),
            );
            received.unwrap();
            sent.unwrap();
            assert_eq!(
                controller.read().await.unwrap().text,
                remote.read().await.unwrap().text
            );
        }
    }

    #[tokio::test]
    async fn unsupported_source_does_not_clear_destination() {
        for text in [None, Some("")] {
            let epoch = Arc::new(AtomicU64::new(0));
            let controller = ClipboardWorker::memory(text, epoch.clone());
            let remote = ClipboardWorker::memory(Some("keep this"), epoch.clone());
            let (acceptor, connector, keys) = connections();
            let (client, server) = tokio::io::duplex(4096);
            let (received, sent) = tokio::join!(
                receive(
                    server,
                    acceptor,
                    keys,
                    remote.clone(),
                    epoch.clone(),
                    Default::default(),
                    0
                ),
                exchange(
                    async { Ok(client) },
                    connector,
                    controller,
                    epoch,
                    0,
                    [1; 16],
                    1,
                    false
                ),
            );
            received.unwrap();
            sent.unwrap();
            assert_eq!(
                remote.read().await.unwrap().text.as_deref(),
                Some("keep this")
            );
        }
    }

    #[tokio::test]
    async fn copy_during_return_transfer_wins_over_remote_text() {
        let epoch = Arc::new(AtomicU64::new(0));
        let controller = ClipboardWorker::memory(Some("original"), epoch.clone());
        let remote = ClipboardWorker::memory(Some("remote text"), epoch.clone());
        let (acceptor, connector, keys) = connections();
        let (client, server) = tokio::io::duplex(4096);
        let connecting = async {
            let revision = controller.revision().await?;
            controller
                .write("new local copy".into(), revision, 0)
                .await?;
            Ok(client)
        };
        let (received, sent) = tokio::join!(
            receive(
                server,
                acceptor,
                keys,
                remote,
                epoch.clone(),
                Default::default(),
                0
            ),
            exchange(
                connecting,
                connector,
                controller.clone(),
                epoch.clone(),
                0,
                [1; 16],
                1,
                true
            ),
        );
        received.unwrap();
        sent.unwrap();
        assert_eq!(
            controller.read().await.unwrap().text.as_deref(),
            Some("new local copy")
        );
    }
    #[test]
    fn old_duplicate_and_retired_updates_cannot_win() {
        let mut order = UpdateOrder::default();
        assert!(order.accept([1; 16], 3));
        assert!(!order.accept([1; 16], 2));
        assert!(!order.accept([1; 16], 3));
        assert!(order.accept([1; 16], 4));
        assert!(!order.is_current([1; 16], 3));
        assert!(order.accept([2; 16], 1));
        assert!(!order.accept([1; 16], 5));
    }
    #[test]
    fn only_supported_modes_are_accepted() {
        assert_eq!(
            toml::from_str::<Settings>("mode = 'on-switch'")
                .unwrap()
                .mode,
            Mode::OnSwitch
        );
        assert_eq!(Settings::default().mode, Mode::Off);
        assert!(toml::from_str::<Settings>("mode = 'continuous'").is_err());
    }
}
