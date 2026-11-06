use crate::identity::{self, Identity};
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{ring::default_provider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, Error, RootCertStore,
    ServerConfig, SignatureScheme,
};
use std::{net::SocketAddr, sync::Arc, time::Duration};

pub fn normalize_fingerprint(value: &str) -> Result<String, String> {
    let value = value.trim();
    let hex = if value.len() == 95 {
        let parts = value.split(':').collect::<Vec<_>>();
        if parts.len() != 32 || parts.iter().any(|part| part.len() != 2) {
            return Err("请填写完整的 SHA-256 证书指纹。".into());
        }
        parts.concat()
    } else {
        value.to_owned()
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("请填写完整的 64 位十六进制 SHA-256 证书指纹。".into());
    }
    Ok(hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|part| String::from_utf8_lossy(part).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":"))
}

fn leaf_roots(
    certificate: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
) -> Result<Arc<RootCertStore>, Error> {
    if certificate.len() > 8192 || !intermediates.is_empty() {
        return Err(Error::InvalidCertificate(CertificateError::BadEncoding));
    }
    let mut roots = RootCertStore::empty();
    roots.add(certificate.clone().into_owned())?;
    Ok(Arc::new(roots))
}

#[derive(Debug)]
struct PinnedServer {
    fingerprint: String,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        certificate: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        if identity::fingerprint(certificate.as_ref()) != self.fingerprint {
            return Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        rustls::client::WebPkiServerVerifier::builder_with_provider(
            leaf_roots(certificate, intermediates)?,
            Arc::new(default_provider()),
        )
        .build()
        .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?
        .verify_server_cert(certificate, intermediates, server_name, ocsp, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// TLS proves key possession; Engine separately requires persisted trust for every file route.
#[derive(Debug)]
struct PairingClientVerifier;

impl ClientCertVerifier for PairingClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        certificate: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        rustls::server::WebPkiClientVerifier::builder_with_provider(
            leaf_roots(certificate, intermediates)?,
            Arc::new(default_provider()),
        )
        .build()
        .map_err(|_| Error::InvalidCertificate(CertificateError::BadEncoding))?
        .verify_client_cert(certificate, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn server(identity: &Identity) -> Result<ServerConfig, String> {
    let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS 协议初始化失败：{error}"))?
        .with_client_cert_verifier(Arc::new(PairingClientVerifier))
        .with_single_cert(vec![identity.certificate.clone()], identity.private_key())
        .map_err(|_| "本机 TLS 身份不可用。")?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    Ok(config)
}

pub fn client(
    identity: &Identity,
    fingerprint: &str,
    address: SocketAddr,
) -> Result<reqwest::Client, String> {
    let mut config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("TLS 协议初始化失败：{error}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer {
            fingerprint: normalize_fingerprint(fingerprint)?,
        }))
        .with_client_auth_cert(vec![identity.certificate.clone()], identity.private_key())
        .map_err(|_| "无法加载本机客户端身份证书。")?;
    config.resumption = rustls::client::Resumption::disabled();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    // The out-of-band fingerprint authenticates the endpoint; localhost is the pinned cert's SAN.
    reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .https_only(true)
        .http1_only()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve("localhost", address)
        .connect_timeout(Duration::from_secs(3))
        // A response-header timeout would also cap uploads; Engine bounds control and idle IO.
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|error| format!("无法创建固定身份的 TLS 客户端：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_requires_all_32_bytes() {
        let identity = identity::generate().unwrap();
        assert_eq!(
            normalize_fingerprint(&identity.fingerprint.to_lowercase()).unwrap(),
            identity.fingerprint
        );
        assert_eq!(
            normalize_fingerprint(&identity.fingerprint.replace(':', "")).unwrap(),
            identity.fingerprint
        );
        for value in [
            "",
            "123456",
            &"ab".repeat(31),
            &"gg".repeat(32),
            &"a:".repeat(32),
        ] {
            assert!(normalize_fingerprint(value).is_err());
        }
    }

    #[test]
    fn pin_rejects_identity_change_and_extra_chain() {
        let first = identity::generate().unwrap();
        let second = identity::generate().unwrap();
        let verifier = PinnedServer {
            fingerprint: first.fingerprint,
        };
        let name = ServerName::try_from("localhost").unwrap();
        assert!(verifier
            .verify_server_cert(&first.certificate, &[], &name, &[], UnixTime::now())
            .is_ok());
        assert!(verifier
            .verify_server_cert(&second.certificate, &[], &name, &[], UnixTime::now())
            .is_err());
        assert!(verifier
            .verify_server_cert(
                &first.certificate,
                &[],
                &ServerName::try_from("wrong-name.invalid").unwrap(),
                &[],
                UnixTime::now()
            )
            .is_err());
        assert!(verifier
            .verify_server_cert(
                &first.certificate,
                &[second.certificate],
                &name,
                &[],
                UnixTime::now()
            )
            .is_err());
        assert!(PairingClientVerifier
            .verify_client_cert(&CertificateDer::from(vec![0; 8193]), &[], UnixTime::now())
            .is_err());
    }
}
