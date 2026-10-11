//! Pin self-signed Lan Mouse identities while still verifying handshake signatures.
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use webrtc_dtls::crypto::Certificate;

pub(super) type Authorized = Arc<RwLock<HashMap<String, String>>>;
const ALPN: &[u8] = b"lan-mouse-clipboard/1";

#[derive(Debug)]
struct PinnedServer(String);
#[derive(Debug)]
struct TrustedClient(Authorized);

fn algorithms() -> rustls::crypto::WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

macro_rules! signature_verification {
    () => {
        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms())
        }
        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            algorithms().supported_schemes()
        }
    };
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if crate::crypto::generate_fingerprint(cert).eq_ignore_ascii_case(&self.0) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General(
                "clipboard peer fingerprint changed or does not match configuration".into(),
            ))
        }
    }
    signature_verification!();
}

impl ClientCertVerifier for TrustedClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        if authorized(&self.0, cert) {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(Error::General(
                "clipboard client fingerprint is not authorized".into(),
            ))
        }
    }
    signature_verification!();
}

pub(super) fn authorized(authorized: &Authorized, cert: &CertificateDer<'_>) -> bool {
    authorized
        .read()
        .is_ok_and(|keys| keys.contains_key(&crate::crypto::generate_fingerprint(cert)))
}

pub(super) fn server(
    cert: &Certificate,
    authorized: Authorized,
) -> Result<rustls::ServerConfig, Error> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_client_cert_verifier(Arc::new(TrustedClient(authorized)))
    .with_single_cert(
        cert.certificate.clone(),
        PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone()).into(),
    )?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}

pub(super) fn client(
    cert: &Certificate,
    fingerprint: String,
) -> Result<rustls::ClientConfig, Error> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(PinnedServer(fingerprint)))
    .with_client_auth_cert(
        cert.certificate.clone(),
        PrivatePkcs8KeyDer::from(cert.private_key.serialized_der.clone()).into(),
    )?;
    config.alpn_protocols = vec![ALPN.to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    async fn handshake(trust_client: bool, correct_pin: bool) -> (bool, bool) {
        let server_cert = Certificate::generate_self_signed(vec!["ignored".into()]).unwrap();
        let client_cert = Certificate::generate_self_signed(vec!["ignored".into()]).unwrap();
        let mut keys = HashMap::new();
        if trust_client {
            keys.insert(
                crate::crypto::certificate_fingerprint(&client_cert),
                "client".into(),
            );
        }
        let acceptor = TlsAcceptor::from(Arc::new(
            server(&server_cert, Arc::new(RwLock::new(keys))).unwrap(),
        ));
        let pin = if correct_pin {
            crate::crypto::certificate_fingerprint(&server_cert)
        } else {
            "changed".into()
        };
        let connector = TlsConnector::from(Arc::new(client(&client_cert, pin).unwrap()));
        let (client, server) = tokio::io::duplex(4096);
        let (accepted, connected) = tokio::join!(
            acceptor.accept(server),
            connector.connect(ServerName::try_from("ignored").unwrap(), client),
        );
        (accepted.is_ok(), connected.is_ok())
    }

    #[tokio::test]
    async fn mutually_trusted_peers_connect() {
        assert_eq!(handshake(true, true).await, (true, true));
    }
    #[tokio::test]
    async fn unauthorized_client_is_rejected() {
        assert!(!handshake(false, true).await.0);
    }
    #[tokio::test]
    async fn unexpected_server_is_rejected() {
        assert!(!handshake(true, false).await.1);
    }
}
