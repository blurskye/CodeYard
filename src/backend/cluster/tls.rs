//! TLS for node connections over TCP. Each side presents its node key as a raw ed25519
//! public key (RFC 7250) and proves that it holds the key with the TLS 1.3
//! CertificateVerify signature, which the provider checks. The TLS layer trusts no key
//! by itself: the dialer accepts only the key it pinned, and the listener accepts any
//! well-formed ed25519 key so that unpaired nodes can reach pairing. `cluster::tcp`
//! applies membership to everything after the handshake.
use super::{
    identity::NodeKey,
    transport::{ALPN_EVENTS, ALPN_PAIR, ALPN_RPC},
};
use anyhow::Result;
use iroh::PublicKey;
use std::sync::Arc;
use tokio_rustls::rustls::{
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        AlwaysResolvesClientRawPublicKeys, Resumption,
    },
    crypto::{aws_lc_rs, verify_tls13_signature_with_raw_key, CryptoProvider},
    pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer,
        UnixTime,
    },
    server::{
        danger::{ClientCertVerified, ClientCertVerifier},
        AlwaysResolvesServerRawPublicKeys, NoServerSessionStorage,
    },
    sign::CertifiedKey,
    version::TLS13,
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme,
};

/// The header of an ed25519 SubjectPublicKeyInfo (RFC 8410). The 32 key bytes follow it.
const ED25519_SPKI_HEADER: [u8; 12] = [
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];
/// The PKCS#8 wrapper of an ed25519 seed (RFC 8410). The 32 seed bytes follow it.
const ED25519_PKCS8_HEADER: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The protocol names a node connection may negotiate. Any other name fails the handshake.
pub const ALPNS: [&[u8]; 3] = [ALPN_PAIR, ALPN_RPC, ALPN_EVENTS];

/// The key a peer presented, when it is a well-formed ed25519 SubjectPublicKeyInfo.
pub(super) fn peer_key(presented: &[u8]) -> Result<PublicKey, Error> {
    let bytes = presented
        .strip_prefix(&ED25519_SPKI_HEADER)
        .and_then(|key| <&[u8; 32]>::try_from(key).ok())
        .ok_or_else(|| Error::General("the peer did not present an ed25519 node key".into()))?;
    PublicKey::from_bytes(bytes)
        .map_err(|_| Error::General("the peer presented an invalid ed25519 key".into()))
}

/// The node's TLS identity: its raw public key, and the signer for it.
fn identity(key: &NodeKey) -> Result<Arc<CertifiedKey>> {
    let mut pkcs8 = ED25519_PKCS8_HEADER.to_vec();
    pkcs8.extend_from_slice(&key.iroh_secret().to_bytes());
    let signer = aws_lc_rs::sign::any_supported_type(&PrivateKeyDer::Pkcs8(
        PrivatePkcs8KeyDer::from(pkcs8),
    ))?;
    let mut spki = ED25519_SPKI_HEADER.to_vec();
    spki.extend_from_slice(key.public_key().as_bytes());
    Ok(Arc::new(CertifiedKey::new(
        vec![CertificateDer::from(spki)],
        signer,
    )))
}

/// Checks the CertificateVerify signature against the presented raw key, with the
/// provider's own algorithms. Only ed25519 is accepted.
fn verify_signature(
    provider: &CryptoProvider,
    presented: &CertificateDer<'_>,
    message: &[u8],
    signature: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    peer_key(presented)?;
    if signature.scheme != SignatureScheme::ED25519 {
        return Err(Error::General(
            "node connections sign with ed25519 only".into(),
        ));
    }
    verify_tls13_signature_with_raw_key(
        message,
        &SubjectPublicKeyInfoDer::from(presented.as_ref()),
        signature,
        &provider.signature_verification_algorithms,
    )
}

/// The listener's view of a dialing peer. Any well-formed ed25519 key passes here, and
/// the peer must prove it holds the key. `cluster::tcp` decides what the key may do.
#[derive(Debug)]
struct AnyNodeKey {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyNodeKey {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        peer_key(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("node connections use TLS 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_signature(&self.provider, cert, message, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The dialer's view of the node it called. Only the key it pinned passes.
#[derive(Debug)]
struct PinnedNode {
    expected: PublicKey,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedNode {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if !intermediates.is_empty() || peer_key(end_entity)? != self.expected {
            return Err(Error::General(
                "the node presented a key other than the one pinned".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("node connections use TLS 1.3".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_signature(&self.provider, cert, message, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The listener's TLS settings: TLS 1.3 only, this node's key, a client key demanded,
/// the three protocol names, and no session tickets or early data.
pub fn server_config(key: &NodeKey) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let verifier = AnyNodeKey {
        provider: provider.clone(),
    };
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(identity(
            key,
        )?)));
    config.alpn_protocols = ALPNS.iter().map(|alpn| alpn.to_vec()).collect();
    config.send_tls13_tickets = 0;
    config.session_storage = Arc::new(NoServerSessionStorage {});
    config.max_early_data_size = 0;
    Ok(Arc::new(config))
}

/// The dialer's TLS settings: TLS 1.3 only, this node's key, the one protocol name asked
/// for, the pinned peer key, and no session resumption or early data.
pub fn client_config(key: &NodeKey, expected: PublicKey, alpn: &[u8]) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(aws_lc_rs::default_provider());
    let verifier = PinnedNode {
        expected,
        provider: provider.clone(),
    };
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(identity(
            key,
        )?)));
    config.alpn_protocols = vec![alpn.to_vec()];
    config.resumption = Resumption::disabled();
    config.enable_early_data = false;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_connections_take_no_early_data_and_no_session_tickets() {
        let key = NodeKey::generate();
        let server = server_config(&key).unwrap();
        assert_eq!(server.max_early_data_size, 0);
        assert_eq!(server.send_tls13_tickets, 0);
        let client = client_config(&key, key.public_key(), ALPN_RPC).unwrap();
        assert!(!client.enable_early_data);
    }
}
