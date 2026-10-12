use crate::client::ClientManager;
use crate::config::local_commit;
use crate::crypto;
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
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
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    recv_rx: Receiver<(ClientHandle, ProtoEvent)>,
    recv_tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
}

impl LanMouseConnection {
    pub(crate) fn new(cert: Certificate, client_manager: ClientManager) -> Self {
        let (recv_tx, recv_rx) = channel();
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            recv_rx,
            recv_tx,
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
            let conn = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(conn) = conn {
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
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.ping_response.clone(),
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, ProtoEvent)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let expected = client_manager.get_fingerprint(handle);
        let res = connect_any(&addrs, cert, expected.clone()).await;
        let (conn, addr, fingerprint) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        // KNOWN LIMIT: trust on first use. An unpinned client accepts and
        // pins whichever peer completes the first handshake, including the
        // fastest of several addresses. Upgrade path: confirm before pinning.
        if expected.is_none() && client_manager.pin_fingerprint(handle, &fingerprint) {
            log::info!("client ({handle}) pinned certificate {fingerprint} on first use");
        }
        // the pin may have been changed while the handshake was in flight
        if let Some(pinned) = client_manager
            .get_fingerprint(handle)
            .filter(|pinned| *pinned != fingerprint)
        {
            let _ = conn.close().await;
            connecting.lock().await.remove(&handle);
            return Err(PeerIdentityError::Mismatch {
                expected: pinned,
                received: fingerprint,
            }
            .into());
        }
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        conns.lock().await.insert(addr, conn.clone());
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
        return Ok(());
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
    conns: Rc<Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>>,
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
    conns: &Mutex<HashMap<SocketAddr, Arc<dyn Conn + Send + Sync>>>,
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
