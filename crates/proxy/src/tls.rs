//! The proxy's certificate authority: a CA made at start and one key for every leaf, so a browser trusts
//! all of them through one SPKI hash.
use crate::ProxyError;
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    ca: CertificateDer<'static>,
    leaf_key: KeyPair,
    servers: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn cert_err(e: rcgen::Error) -> ProxyError {
    ProxyError::Certificate(e.to_string())
}

impl Authority {
    /// A new authority, or the one `keys` (from [`Authority::keys`]) names: a browser started with the
    /// earlier proxy trusts the same leaf key.
    pub fn new(keys: Option<&crate::AuthorityKeys>) -> Result<Authority, ProxyError> {
        let key = match keys {
            Some(k) => KeyPair::from_pem(&k.ca_key_pem).map_err(cert_err)?,
            None => KeyPair::generate().map_err(cert_err)?,
        };
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(cert_err)?;
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, "frontcap generation authority");
        params.distinguished_name = name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
        let ca = params.self_signed(&key).map_err(cert_err)?.der().clone();
        let leaf_key = match keys {
            Some(k) => KeyPair::from_pem(&k.leaf_key_pem).map_err(cert_err)?,
            None => KeyPair::generate().map_err(cert_err)?,
        };
        Ok(Authority { issuer: Issuer::new(params, key), ca, leaf_key, servers: Mutex::new(HashMap::new()) })
    }

    pub fn keys(&self) -> crate::AuthorityKeys {
        crate::AuthorityKeys { ca_key_pem: self.issuer.key().serialize_pem(), leaf_key_pem: self.leaf_key.serialize_pem() }
    }

    pub fn ca_der(&self) -> CertificateDer<'static> {
        self.ca.clone()
    }

    pub fn spki_sha256_base64(&self) -> String {
        use rcgen::PublicKeyData;
        frontcap_core::encode::base64(&Sha256::digest(self.leaf_key.subject_public_key_info()))
    }

    /// The server side of a TLS connection for `host` (a name or an IP address).
    pub fn server(&self, host: &str) -> Result<Arc<rustls::ServerConfig>, ProxyError> {
        if let Some(c) = self.servers.lock().unwrap().get(host) {
            return Ok(c.clone());
        }
        let mut params = CertificateParams::new(vec![host.to_string()]).map_err(cert_err)?;
        params.distinguished_name.push(DnType::CommonName, host);
        let leaf = params.signed_by(&self.leaf_key, &self.issuer).map_err(cert_err)?.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf_key.serialize_der()));
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| ProxyError::Tls(e.to_string()))?
            .with_no_client_auth()
            .with_single_cert(vec![leaf, self.ca.clone()], key)
            .map_err(|e| ProxyError::Tls(e.to_string()))?;
        // HTTP/1.1 only: the proxy reads requests one at a time.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        self.servers.lock().unwrap().insert(host.to_string(), config.clone());
        Ok(config)
    }
}

/// Upstream TLS: the system's roots and `extra`.
pub fn client_config(extra: Vec<CertificateDer<'static>>) -> Result<rustls::ClientConfig, ProxyError> {
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    for c in extra {
        roots.add(c).map_err(|e| ProxyError::Tls(e.to_string()))?;
    }
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| ProxyError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}
