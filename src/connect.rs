use crate::client::{ClientManager, ConnectTarget};
use crate::config::local_commit;
use crate::crypto;
use lan_mouse_ipc::ClientHandle;
use lan_mouse_proto::{MAX_EVENT_SIZE, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, Mutex as SyncMutex},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
    #[error(transparent)]
    PeerIdentity(#[from] PeerIdentityError),
    #[error("client changed while connecting")]
    TargetChanged,
}

/// The peer presented a certificate that does not establish the
/// identity expected for this client.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(crate) enum PeerIdentityError {
    #[error("peer presented certificate `{received}`, but `{expected}` is pinned for this client")]
    Mismatch { expected: String, received: String },
    #[error("peer presented {0} certificates, expected exactly one")]
    CertificateCount(usize),
}

/// Check the certificates the peer presented during the handshake
/// against the pinned fingerprint, if any. Returns the fingerprint of
/// the peer's certificate.
///
/// The handshake has already verified the peer's key-exchange signature
/// against this certificate, so a matching fingerprint proves the peer
/// holds the pinned key. `insecure_skip_verify` only disables CA chain
/// validation, which self-signed certificates cannot pass anyway.
fn verify_peer(certs: &[Vec<u8>], expected: Option<&str>) -> Result<String, PeerIdentityError> {
    let [cert] = certs else {
        return Err(PeerIdentityError::CertificateCount(certs.len()));
    };
    let received = crypto::generate_fingerprint(cert);
    match expected {
        Some(expected) if expected != received => Err(PeerIdentityError::Mismatch {
            expected: expected.to_string(),
            received,
        }),
        _ => Ok(received),
    }
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// An established connection together with the fingerprint of the
/// certificate the peer presented.
type Connected = (Arc<dyn Conn + Sync + Send>, SocketAddr, String);

/// An open connection and the fingerprint of the peer certificate it
/// was established with.
#[derive(Clone)]
struct PeerConn {
    conn: Arc<dyn Conn + Send + Sync>,
    fingerprint: String,
}

type Conns = Rc<Mutex<HashMap<SocketAddr, PeerConn>>>;

/// Peer identity outcomes of outgoing connections, reported to the service.
#[derive(Debug)]
pub(crate) enum PeerIdentityEvent {
    /// the client had no pin and now pins the certificate it connected with
    Pinned(ClientHandle),
    /// the peer's certificate does not match the client's pin
    Rejected {
        handle: ClientHandle,
        expected: String,
        received: String,
    },
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
    expected: Option<String>,
) -> Result<Connected, (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    // result of the peer certificate check, written by the handshake
    let peer_check: Arc<SyncMutex<Option<Result<String, PeerIdentityError>>>> = Default::default();
    let verify_peer_certificate = {
        let peer_check = peer_check.clone();
        Arc::new(move |certs: &[Vec<u8>], _: &[CertificateDer<'static>]| {
            let result = verify_peer(certs, expected.as_deref());
            let verified = result.is_ok();
            peer_check.lock().expect("lock").replace(result);
            if verified {
                Ok(())
            } else {
                Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
            }
        })
    };
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        verify_peer_certificate: Some(verify_peer_certificate),
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    let result = tokio::select! {
        _ = timeout => return Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => result,
    };
    let peer_check = peer_check.lock().expect("lock").take();
    let dtls_conn = match result {
        Ok(dtls_conn) => dtls_conn,
        Err(e) => {
            let e = match peer_check {
                Some(Err(identity)) => identity.into(),
                _ => e.into(),
            };
            return Err((addr, e));
        }
    };
    match peer_check {
        Some(Ok(fingerprint)) => Ok((Arc::new(dtls_conn), addr, fingerprint)),
        // fail closed: the handshake completed without a certificate check
        Some(Err(e)) => {
            let _ = dtls_conn.close().await;
            Err((addr, e.into()))
        }
        None => {
            let _ = dtls_conn.close().await;
            Err((addr, PeerIdentityError::CertificateCount(0).into()))
        }
    }
}

/// Connect to the first address that completes a handshake with the
/// expected peer. If none does, a peer identity failure takes precedence
/// over other errors so that it can be reported.
async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
    expected: Option<String>,
) -> Result<Connected, LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone(), expected.clone()));
    }
    let mut identity_error = None;
    loop {
        match joinset.join_next().await {
            None => {
                return Err(identity_error
                    .map(LanMouseConnectionError::PeerIdentity)
                    .unwrap_or(LanMouseConnectionError::NotConnected));
            }
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, LanMouseConnectionError::PeerIdentity(e))) => {
                    log::warn!("rejected peer at {a}: {e}");
                    identity_error.get_or_insert(e);
                }
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Conns,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    identity_tx: Sender<PeerIdentityEvent>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        identity_tx: Sender<PeerIdentityEvent>,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
            identity_tx,
            ping_response: Default::default(),
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, ProtoEvent) {
        self.recv_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let peer = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            // the pin was changed while connected: drop the connection and
            // reconnect below so the new pin is enforced by a handshake
            let peer = match peer {
                Some(peer)
                    if !self
                        .client_manager
                        .fingerprint_matches(handle, &peer.fingerprint) =>
                {
                    log::info!("client {handle}: pinned certificate changed, reconnecting");
                    let _ = peer.conn.close().await;
                    disconnect(&self.client_manager, handle, addr, &self.conns).await;
                    None
                }
                peer => peer,
            };
            if let Some(PeerConn { conn, .. }) = peer {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(&self.client_manager, handle, addr, &self.conns).await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            let connect = connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
            );
            let identity_tx = self.identity_tx.clone();
            spawn_local(async move {
                let event = match connect.await {
                    Ok(true) => Some(PeerIdentityEvent::Pinned(handle)),
                    Err(LanMouseConnectionError::PeerIdentity(PeerIdentityError::Mismatch {
                        expected,
                        received,
                    })) => Some(PeerIdentityEvent::Rejected {
                        handle,
                        expected,
                        received,
                    }),
                    Ok(false) | Err(_) => None,
                };
                if let Some(event) = event {
                    if identity_tx.send(event).is_err() {
                        log::debug!("service gone, dropping peer identity event");
                    }
                }
            });
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

/// Apply a completed handshake to the client it was made for. Returns
/// whether the peer's certificate was pinned on first use.
fn accept_handshake(
    client_manager: &ClientManager,
    handle: ClientHandle,
    target: &ConnectTarget,
    fingerprint: &str,
) -> Result<bool, LanMouseConnectionError> {
    // the client may have been edited, or removed and its handle reused,
    // while the handshake was in flight: never apply the result to it
    if client_manager.connect_target(handle).as_ref() != Some(target) {
        return Err(LanMouseConnectionError::TargetChanged);
    }
    // a pinned client was already verified during the handshake
    if target.fingerprint.is_some() {
        return Ok(false);
    }
    // KNOWN LIMIT: trust on first use. An unpinned client accepts and pins
    // whichever peer completes the first handshake, including the fastest
    // of several addresses. Upgrade path: confirm before pinning.
    Ok(client_manager.pin_fingerprint(handle, fingerprint))
}

/// Connect to the client and start its receive loop. Returns whether the
/// client pinned the peer's certificate on first use.
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Conns,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) -> Result<bool, LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(target) = client_manager.connect_target(handle) {
        let addrs = target
            .ips
            .iter()
            .map(|&ip| SocketAddr::new(ip, target.port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert, target.fingerprint.clone()).await;
        let (conn, addr, fingerprint) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        let pinned_on_first_use =
            match accept_handshake(&client_manager, handle, &target, &fingerprint) {
                Ok(pinned) => pinned,
                Err(e) => {
                    let _ = conn.close().await;
                    connecting.lock().await.remove(&handle);
                    return Err(e);
                }
            };
        if pinned_on_first_use {
            log::info!("client ({handle}) pinned certificate {fingerprint} on first use");
        }
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(
            addr,
            PeerConn {
                conn: conn.clone(),
                fingerprint,
            },
        );
        connecting.lock().await.remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let (buf, len) = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            conns,
            tx,
            ping_response.clone(),
        ));
        return Ok(pinned_on_first_use);
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    conns: Conns,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    let mut buf = [0u8; MAX_EVENT_SIZE];
    loop {
        buf.fill(0);
        if conn.recv(&mut buf).await.is_err() {
            break;
        }
        match buf.try_into() {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    event => tx.send((handle, event)).expect("channel closed"),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    disconnect(&client_manager, handle, addr, &conns).await;
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, PeerConn>>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    conns.lock().await.remove(&addr);
    client_manager.set_active_addr(handle, None);
    client_manager.set_peer_commit(handle, None);
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use webrtc_dtls::{config::ClientAuthType::RequireAnyClientCert, listener::listen};
    use webrtc_util::conn::Listener;

    fn certificate() -> Certificate {
        Certificate::generate_self_signed(["ignored".to_owned()]).expect("certificate")
    }

    /// Start a DTLS server on localhost that presents `cert` and accepts
    /// any client certificate, mirroring the listener's handshake config.
    async fn serve(cert: Certificate) -> SocketAddr {
        let config = Config {
            certificates: vec![cert],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            ..Default::default()
        };
        let listener = listen("127.0.0.1:0", config).await.expect("listen");
        let addr = listener.addr().await.expect("listener address");
        tokio::spawn(async move {
            let mut conns = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                conns.push(conn);
            }
        });
        addr
    }

    async fn connect_to(
        server: &Certificate,
        expected: Option<String>,
    ) -> Result<String, LanMouseConnectionError> {
        let addr = serve(server.clone()).await;
        match connect(addr, certificate(), expected).await {
            Ok((conn, _, fingerprint)) => {
                let _ = conn.close().await;
                Ok(fingerprint)
            }
            Err((_, e)) => Err(e),
        }
    }

    #[tokio::test]
    async fn unpinned_peer_is_accepted_and_identified() {
        let server = certificate();
        let fingerprint = connect_to(&server, None).await.expect("handshake");
        assert_eq!(fingerprint, crypto::certificate_fingerprint(&server));
    }

    #[tokio::test]
    async fn pinned_peer_is_accepted() {
        let server = certificate();
        let pinned = crypto::certificate_fingerprint(&server);
        let fingerprint = connect_to(&server, Some(pinned.clone()))
            .await
            .expect("handshake");
        assert_eq!(fingerprint, pinned);
    }

    #[tokio::test]
    async fn untrusted_peer_is_rejected() {
        let server = certificate();
        let pinned = crypto::certificate_fingerprint(&certificate());
        let err = connect_to(&server, Some(pinned.clone())).await.unwrap_err();
        let LanMouseConnectionError::PeerIdentity(PeerIdentityError::Mismatch {
            expected,
            received,
        }) = err
        else {
            panic!("expected an identity mismatch, got `{err}`");
        };
        assert_eq!(expected, pinned);
        assert_eq!(received, crypto::certificate_fingerprint(&server));
    }

    #[tokio::test]
    async fn changed_peer_certificate_is_rejected() {
        let original = certificate();
        let pinned = connect_to(&original, None).await.expect("first handshake");
        // the peer regenerates its certificate, e.g. after lan-mouse.pem was deleted
        let regenerated = certificate();
        let err = connect_to(&regenerated, Some(pinned)).await.unwrap_err();
        assert!(
            matches!(
                err,
                LanMouseConnectionError::PeerIdentity(PeerIdentityError::Mismatch { .. })
            ),
            "expected an identity mismatch, got `{err}`"
        );
    }

    #[tokio::test]
    async fn pin_is_reported_and_enforced_on_existing_connection() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let server = certificate();
                let addr = serve(server.clone()).await;
                let client_manager = ClientManager::default();
                let handle = client_manager.add_client();
                client_manager.set_fix_ips(handle, vec![addr.ip()]);
                client_manager.set_port(handle, addr.port());
                let (identity_tx, mut identity_rx) = channel();
                let conn =
                    LanMouseConnection::new(certificate(), client_manager.clone(), identity_tx);
                let next_event = async |rx: &mut Receiver<PeerIdentityEvent>| {
                    tokio::time::timeout(Duration::from_secs(5), rx.recv())
                        .await
                        .expect("identity event")
                        .expect("channel open")
                };

                // first contact pins the peer's certificate
                assert!(conn.send(ProtoEvent::Ping, handle).await.is_err());
                let event = next_event(&mut identity_rx).await;
                assert!(matches!(event, PeerIdentityEvent::Pinned(h) if h == handle));
                let server_fingerprint = crypto::certificate_fingerprint(&server);
                assert_eq!(
                    client_manager
                        .connect_target(handle)
                        .and_then(|t| t.fingerprint)
                        .as_deref(),
                    Some(server_fingerprint.as_str())
                );

                // pinning a different certificate drops the open connection
                // and the reconnect is rejected
                let other = crypto::certificate_fingerprint(&certificate());
                client_manager.set_fingerprint(handle, Some(other.clone()));
                assert!(conn.send(ProtoEvent::Ping, handle).await.is_err());
                let PeerIdentityEvent::Rejected {
                    handle: h,
                    expected,
                    received,
                } = next_event(&mut identity_rx).await
                else {
                    panic!("expected a rejection");
                };
                assert_eq!(h, handle);
                assert_eq!(expected, other);
                assert_eq!(received, server_fingerprint);
            })
            .await;
    }

    fn unpinned_client(client_manager: &ClientManager, port: u16) -> ClientHandle {
        let handle = client_manager.add_client();
        client_manager.set_fix_ips(handle, vec!["127.0.0.1".parse().expect("ip")]);
        client_manager.set_port(handle, port);
        handle
    }

    fn pin_of(client_manager: &ClientManager, handle: ClientHandle) -> Option<String> {
        client_manager
            .connect_target(handle)
            .and_then(|target| target.fingerprint)
    }

    #[test]
    fn handshake_result_pins_unchanged_client() {
        let client_manager = ClientManager::default();
        let handle = unpinned_client(&client_manager, 4242);
        let target = client_manager.connect_target(handle).expect("client");
        let fingerprint = crypto::certificate_fingerprint(&certificate());
        assert!(matches!(
            accept_handshake(&client_manager, handle, &target, &fingerprint),
            Ok(true)
        ));
        assert_eq!(pin_of(&client_manager, handle), Some(fingerprint));
    }

    /// An edit made while a handshake is in flight; returns the handle the
    /// handshake result would be applied to.
    type InFlightChange = fn(&ClientManager, ClientHandle) -> ClientHandle;

    #[test]
    fn handshake_result_is_discarded_when_client_changed_in_flight() {
        let fingerprint = crypto::certificate_fingerprint(&certificate());
        let changes: [(&str, InFlightChange); 3] = [
            ("hostname edited", |m, h| {
                m.set_hostname(h, Some("other-host".into()));
                h
            }),
            ("pin set", |m, h| {
                let other = crypto::certificate_fingerprint(&certificate());
                m.set_fingerprint(h, Some(other));
                h
            }),
            ("removed and handle reused", |m, h| {
                m.remove_client(h);
                let reused = unpinned_client(m, 4343);
                assert_eq!(reused, h, "slab reuses the freed handle");
                reused
            }),
        ];
        for (change, apply) in changes {
            let client_manager = ClientManager::default();
            let handle = unpinned_client(&client_manager, 4242);
            let target = client_manager.connect_target(handle).expect("client");
            let pin_before = {
                let handle = apply(&client_manager, handle);
                pin_of(&client_manager, handle)
            };
            assert!(
                matches!(
                    accept_handshake(&client_manager, handle, &target, &fingerprint),
                    Err(LanMouseConnectionError::TargetChanged)
                ),
                "{change}: result must be discarded"
            );
            assert_eq!(
                pin_of(&client_manager, handle),
                pin_before,
                "{change}: pin must be untouched"
            );
        }
    }

    #[test]
    fn certificate_count_other_than_one_is_rejected() {
        assert_eq!(
            verify_peer(&[], None),
            Err(PeerIdentityError::CertificateCount(0))
        );
        let cert = vec![0u8; 8];
        assert_eq!(
            verify_peer(&[cert.clone(), cert], None),
            Err(PeerIdentityError::CertificateCount(2))
        );
    }
}
