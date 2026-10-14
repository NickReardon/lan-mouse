//! On-switch text sharing (NickReardon/lan-mouse#1), enabled unless
//! `[clipboard] mode = "off"`. Destinations are authenticated by the
//! certificate pinned for the client (#36), sources by
//! `[authorized_fingerprints]`.
mod platform;
mod protocol;
mod tls;

use lan_mouse_ipc::{ClientConfig, ClientState};
use platform::ClipboardWorker;
use protocol::{
    DEFAULT_MAX_TEXT, MIB, PULL, PUSH, invalid, read_header, read_text, write_header, write_text,
};
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
    Off,
    #[default]
    OnSwitch,
}

/// Largest allowed `max_size_mib`: a transfer must finish within
/// `TRANSFER_TIMEOUT`.
const MAX_SIZE_MIB: u32 = 16;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Settings {
    pub mode: Mode,
    /// largest text sent or accepted, in MiB (1 to 16); peers need the
    /// same value, a receiver refuses anything above its own limit
    pub max_size_mib: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: Mode::default(),
            max_size_mib: (DEFAULT_MAX_TEXT / MIB) as u32,
        }
    }
}

impl Settings {
    /// the size limit in bytes, with `max_size_mib` kept within 1 to 16
    fn max_text(&self) -> usize {
        self.max_size_mib.clamp(1, MAX_SIZE_MIB) as usize * MIB
    }
}

/// Why no clipboard transfer is made for a client.
#[derive(Debug, PartialEq, Eq)]
enum Skip {
    /// sharing is turned off for this client
    Disabled,
    /// no certificate pinned yet, so the destination cannot be authenticated
    Unpinned,
}

/// The certificate fingerprint a transfer to this client must present.
fn destination(config: &ClientConfig) -> Result<&str, Skip> {
    if !config.clipboard {
        return Err(Skip::Disabled);
    }
    config.fingerprint.as_deref().ok_or(Skip::Unpinned)
}

pub(crate) struct Clipboard {
    cert: Certificate,
    epoch: Arc<AtomicU64>,
    worker: ClipboardWorker,
    session: [u8; 16],
    sequence: u64,
    synced: Synced,
    max_text: usize,
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
        // on by default, so an unsupported platform is not a warning
        if !cfg!(any(windows, target_os = "macos")) {
            log::info!("clipboard sharing is not supported on this platform");
            return Ok(None);
        }
        let max_text = settings.max_text();
        if max_text != settings.max_size_mib as usize * MIB {
            log::warn!(
                "clipboard max_size_mib = {} is outside 1 to {MAX_SIZE_MIB}, using {} MiB",
                settings.max_size_mib,
                max_text / MIB
            );
        }
        let epoch = Arc::new(AtomicU64::new(0));
        let worker = ClipboardWorker::new(epoch.clone())?;
        // startup counts as a switch: earlier copies are never sent
        let synced = Synced::new(worker.revision().await?);
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
            synced.clone(),
            max_text,
        ));
        log::info!("on-switch clipboard sharing listening on TCP port {port}");
        Ok(Some(Self {
            cert,
            epoch,
            worker,
            session,
            sequence: 0,
            synced,
            max_text,
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
        let fingerprint = match destination(config) {
            Ok(fingerprint) => fingerprint.to_owned(),
            Err(Skip::Disabled) => return,
            Err(Skip::Unpinned) => {
                log::info!("clipboard sharing skipped for {addr}: no certificate pinned yet");
                return;
            }
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
        let synced = self.synced.clone();
        let max_text = self.max_text;
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
                (&synced, &fingerprint),
                max_text,
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

/// This machine's clipboard revision at the last switch with each peer,
/// keyed by the peer's certificate fingerprint. Text goes to a peer, on
/// entry or in answer to its pull, only if something was copied here since,
/// so a switch never replaces the peer's newer clipboard (for example a
/// screenshot) with stale text. A peer without a switch yet is compared
/// with the revision at startup.
#[derive(Clone)]
struct Synced {
    startup: u64,
    peers: Arc<std::sync::Mutex<HashMap<String, u64>>>,
}

impl Synced {
    fn new(startup: u64) -> Self {
        Self {
            startup,
            peers: Default::default(),
        }
    }

    /// whether nothing was copied here since the last switch with `peer`
    fn unchanged(&self, peer: &str, revision: u64) -> bool {
        let peers = self.peers.lock().expect("lock");
        peers.get(peer).copied().unwrap_or(self.startup) == revision
    }

    fn record(&self, peer: &str, revision: u64) {
        let mut peers = self.peers.lock().expect("lock");
        // peers are pinned or authorized, but still bound memory
        if peers.len() < 128 || peers.contains_key(peer) {
            peers.insert(peer.to_owned(), revision);
        }
    }
}

async fn serve(
    socket: TcpListener,
    acceptor: TlsAcceptor,
    authorized: tls::Authorized,
    worker: ClipboardWorker,
    epoch: Arc<AtomicU64>,
    synced: Synced,
    max_text: usize,
) {
    let permits = Arc::new(Semaphore::new(8));
    let orders: Orders = Default::default();
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            connection = socket.accept() => {
                let (stream, addr) = match connection { Ok(connection) => connection, Err(error) => { log::warn!("clipboard listener failed: {error}"); break; } };
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue; };
                let (acceptor, authorized, worker, epoch, orders, synced) = (acceptor.clone(), authorized.clone(), worker.clone(), epoch.clone(), orders.clone(), synced.clone());
                let expected = epoch.load(Ordering::SeqCst);
                tasks.spawn_local(async move {
                    let _permit = permit;
                    let result = timeout(TRANSFER_TIMEOUT, receive(stream, acceptor, authorized, worker, epoch, orders, synced, expected, max_text)).await;
                    if let Ok(Err(error)) = result { log::debug!("clipboard request from {addr} rejected: {error}"); }
                });
            },
            _ = tasks.join_next(), if !tasks.is_empty() => {},
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive(
    socket: impl AsyncRead + AsyncWrite + Unpin,
    acceptor: TlsAcceptor,
    authorized: tls::Authorized,
    worker: ClipboardWorker,
    epoch: Arc<AtomicU64>,
    orders: Orders,
    synced: Synced,
    expected: u64,
    max_text: usize,
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
        // nothing copied here since the switch: keep the peer's clipboard
        let text = if synced.unchanged(&fingerprint, snapshot.revision) {
            None
        } else {
            snapshot.text.as_deref()
        };
        write_text(&mut stream, text, max_text).await?;
        synced.record(&fingerprint, snapshot.revision);
    } else {
        let revision = worker.revision().await?;
        let text = read_text(&mut stream, max_text).await?;
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
            // read after our own write, so it is not taken for a copy
            synced.record(&fingerprint, worker.revision().await?);
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
    (synced, peer): (&Synced, &str),
    max_text: usize,
) -> io::Result<()> {
    let revision = worker.revision().await?;
    let snapshot = if returning {
        None
    } else {
        Some(worker.read().await?)
    };
    if let Some(snapshot) = &snapshot {
        if synced.unchanged(peer, snapshot.revision) {
            log::debug!("nothing copied since the last switch, clipboard not sent");
            return Ok(());
        }
    }
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
        if let Some(text) = read_text(&mut stream, max_text)
            .await?
            .filter(|text| !text.is_empty())
        {
            if worker.write(text, revision, expected).await? {
                synced.record(peer, worker.revision().await?);
            }
        }
    } else {
        let snapshot = snapshot.expect("outgoing clipboard snapshot");
        if epoch.load(Ordering::SeqCst) != expected || worker.revision().await? != snapshot.revision
        {
            return Ok(());
        }
        write_text(&mut stream, snapshot.text.as_deref(), max_text).await?;
        stream.flush().await?;
        let _applied = stream.read_u8().await?;
        synced.record(peer, snapshot.revision);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// One side of a pair: its identity, clipboard and listener state.
    struct Machine {
        cert: Certificate,
        fingerprint: String,
        keys: tls::Authorized,
        worker: ClipboardWorker,
        epoch: Arc<AtomicU64>,
        synced: Synced,
        orders: Orders,
        session: [u8; 16],
        sequence: Cell<u64>,
    }

    impl Machine {
        async fn new(text: Option<&str>, session: u8) -> Self {
            let cert = Certificate::generate_self_signed(vec!["ignored".into()]).unwrap();
            let epoch = Arc::new(AtomicU64::new(0));
            let worker = ClipboardWorker::memory(text, epoch.clone());
            // as at startup: the current clipboard counts as already synced
            let synced = Synced::new(worker.revision().await.unwrap());
            Self {
                fingerprint: crate::crypto::certificate_fingerprint(&cert),
                cert,
                keys: Default::default(),
                worker,
                epoch,
                synced,
                orders: Default::default(),
                session: [session; 16],
                sequence: Cell::new(0),
            }
        }

        /// Treat the current clipboard as copied after startup.
        fn forget_startup(&mut self) {
            self.synced = Synced::new(u64::MAX);
        }

        async fn copy(&self, text: &str) {
            let revision = self.worker.revision().await.unwrap();
            assert!(self.worker.write(text.into(), revision, 0).await.unwrap());
        }

        async fn clipboard(&self) -> Option<String> {
            self.worker.read().await.unwrap().text
        }

        fn next_sequence(&self) -> u64 {
            self.sequence.set(self.sequence.get() + 1);
            self.sequence.get()
        }
    }

    /// Two machines that pin and authorize each other.
    async fn pair(a: Option<&str>, b: Option<&str>) -> (Machine, Machine) {
        let a = Machine::new(a, 1).await;
        let b = Machine::new(b, 2).await;
        a.keys
            .write()
            .unwrap()
            .insert(b.fingerprint.clone(), "b".into());
        b.keys
            .write()
            .unwrap()
            .insert(a.fingerprint.clone(), "a".into());
        (a, b)
    }

    fn link(from: &Machine, to: &Machine) -> (TlsConnector, TlsAcceptor) {
        (
            TlsConnector::from(Arc::new(
                tls::client(&from.cert, to.fingerprint.clone()).unwrap(),
            )),
            TlsAcceptor::from(Arc::new(tls::server(&to.cert, to.keys.clone()).unwrap())),
        )
    }

    /// `from` takes control of `to` (`returning == false`, a push), or gives
    /// control back to itself with the release bind (`returning == true`, a
    /// pull from `to`). A skipped push never connects, so `to` then only sees
    /// a closed stream.
    async fn switch(from: &Machine, to: &Machine, returning: bool) {
        let (connector, acceptor) = link(from, to);
        let (client, server) = tokio::io::duplex(4096);
        let (sent, _received) = tokio::join!(
            exchange(
                async { Ok(client) },
                connector,
                from.worker.clone(),
                from.epoch.clone(),
                0,
                from.session,
                from.next_sequence(),
                returning,
                (&from.synced, &to.fingerprint),
                DEFAULT_MAX_TEXT,
            ),
            receive(
                server,
                acceptor,
                to.keys.clone(),
                to.worker.clone(),
                to.epoch.clone(),
                to.orders.clone(),
                to.synced.clone(),
                0,
                DEFAULT_MAX_TEXT
            ),
        );
        sent.unwrap();
    }

    #[tokio::test]
    async fn encrypted_entry_and_return_share_unicode_in_both_directions() {
        let (windows, mac) = pair(Some("old"), Some("old clipboard")).await;
        windows.copy("Windows → Mac 🐭").await;
        switch(&windows, &mac, false).await;
        assert_eq!(mac.clipboard().await.as_deref(), Some("Windows → Mac 🐭"));
        mac.copy("Mac → Windows 世界").await;
        switch(&windows, &mac, true).await;
        assert_eq!(
            windows.clipboard().await.as_deref(),
            Some("Mac → Windows 世界")
        );
    }

    #[tokio::test]
    async fn screenshot_survives_a_return_by_screen_edge() {
        // a screenshot: no text on Windows, stale text on the Mac
        let (windows, mac) = pair(None, Some("stale mac text")).await;
        switch(&windows, &mac, false).await;
        // crossing back by the edge is the Mac taking control of Windows
        switch(&mac, &windows, false).await;
        assert_eq!(windows.clipboard().await, None);
    }

    #[tokio::test]
    async fn screenshot_survives_a_return_by_release_bind() {
        let (windows, mac) = pair(None, Some("stale mac text")).await;
        switch(&windows, &mac, false).await;
        switch(&windows, &mac, true).await;
        assert_eq!(windows.clipboard().await, None);
    }

    #[tokio::test]
    async fn text_copied_on_the_remote_comes_back_by_screen_edge() {
        let (windows, mac) = pair(None, Some("stale mac text")).await;
        switch(&windows, &mac, false).await;
        mac.copy("copied on the mac").await;
        switch(&mac, &windows, false).await;
        assert_eq!(
            windows.clipboard().await.as_deref(),
            Some("copied on the mac")
        );
        // going back does not send the same text again: a newer copy made
        // on the Mac in the meantime survives
        mac.copy("newer on the mac").await;
        switch(&windows, &mac, false).await;
        assert_eq!(mac.clipboard().await.as_deref(), Some("newer on the mac"));
    }

    #[tokio::test]
    async fn text_copied_on_the_remote_comes_back_by_release_bind() {
        let (windows, mac) = pair(None, Some("stale mac text")).await;
        switch(&windows, &mac, false).await;
        mac.copy("copied on the mac").await;
        switch(&windows, &mac, true).await;
        assert_eq!(
            windows.clipboard().await.as_deref(),
            Some("copied on the mac")
        );
    }

    #[tokio::test]
    async fn text_copied_before_startup_is_not_sent() {
        let (windows, mac) = pair(Some("copied before startup"), Some("mac text")).await;
        switch(&windows, &mac, false).await;
        assert_eq!(mac.clipboard().await.as_deref(), Some("mac text"));
    }

    #[tokio::test]
    async fn unsupported_source_does_not_clear_destination() {
        for text in [None, Some("")] {
            let (mut windows, mac) = pair(text, Some("keep this")).await;
            windows.forget_startup();
            switch(&windows, &mac, false).await;
            assert_eq!(mac.clipboard().await.as_deref(), Some("keep this"));
        }
    }

    #[tokio::test]
    async fn copy_during_return_transfer_wins_over_remote_text() {
        let (windows, mut mac) = pair(Some("original"), Some("remote text")).await;
        mac.forget_startup();
        let (connector, acceptor) = link(&windows, &mac);
        let (client, server) = tokio::io::duplex(4096);
        let connecting = async {
            windows.copy("new local copy").await;
            Ok(client)
        };
        let (sent, received) = tokio::join!(
            exchange(
                connecting,
                connector,
                windows.worker.clone(),
                windows.epoch.clone(),
                0,
                windows.session,
                1,
                true,
                (&windows.synced, &mac.fingerprint),
                DEFAULT_MAX_TEXT,
            ),
            receive(
                server,
                acceptor,
                mac.keys.clone(),
                mac.worker.clone(),
                mac.epoch.clone(),
                mac.orders.clone(),
                mac.synced.clone(),
                0,
                DEFAULT_MAX_TEXT
            ),
        );
        received.unwrap();
        sent.unwrap();
        assert_eq!(windows.clipboard().await.as_deref(), Some("new local copy"));
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
            toml::from_str::<Settings>("mode = 'off'").unwrap().mode,
            Mode::Off
        );
        assert_eq!(
            toml::from_str::<Settings>("mode = 'on-switch'")
                .unwrap()
                .mode,
            Mode::OnSwitch
        );
        assert!(toml::from_str::<Settings>("mode = 'continuous'").is_err());
    }

    #[test]
    fn sharing_is_on_unless_turned_off() {
        assert_eq!(Settings::default().mode, Mode::OnSwitch);
        assert_eq!(toml::from_str::<Settings>("").unwrap().mode, Mode::OnSwitch);
    }

    #[test]
    fn size_limit_defaults_to_one_mib_and_stays_in_range() {
        assert_eq!(Settings::default().max_text(), MIB);
        let settings = |toml: &str| toml::from_str::<Settings>(toml).unwrap();
        assert_eq!(settings("max_size_mib = 8").max_text(), 8 * MIB);
        assert_eq!(settings("max_size_mib = 0").max_text(), MIB);
        assert_eq!(settings("max_size_mib = 999").max_text(), 16 * MIB);
    }

    #[test]
    fn separate_peer_pins_are_rejected() {
        // destinations are authenticated by the client's pinned certificate
        assert!(toml::from_str::<Settings>("[peers]\nhost = 'aa'").is_err());
    }

    #[test]
    fn destination_is_the_pinned_certificate() {
        let mut config = ClientConfig::default();
        assert_eq!(destination(&config), Err(Skip::Unpinned));
        config.fingerprint = Some("ab".repeat(32));
        assert_eq!(destination(&config), Ok("ab".repeat(32).as_str()));
    }

    #[test]
    fn disabled_client_is_skipped_even_when_pinned() {
        let config = ClientConfig {
            fingerprint: Some("ab".repeat(32)),
            clipboard: false,
            ..Default::default()
        };
        assert_eq!(destination(&config), Err(Skip::Disabled));
    }
}
