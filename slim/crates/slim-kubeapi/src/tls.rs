//! Self-signed CA + serving cert for the apiserver, and a TLS serve loop.
//! In-pod operators (client-go in-cluster config) only speak HTTPS and trust
//! the CA we project into their pod's ServiceAccount dir, so the cert doesn't
//! need a public chain — it needs the right SANs and a CA we hand to pods.

use crate::server::ApiServer;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::net::TcpListener;
use std::sync::Arc;

pub struct TlsIdentity {
    /// PEM of the CA cert — projected into pods as ca.crt.
    pub ca_pem: String,
    config: Arc<ServerConfig>,
}

/// Generate a CA and a serving cert covering the in-cluster apiserver names
/// plus any extra SAN (the bridge gateway IP, the ClusterIP).
pub fn generate(
    extra_dns: &[String],
    extra_ips: &[std::net::IpAddr],
) -> Result<TlsIdentity, String> {
    let ca_key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "nebula-slim-ca");
    let ca_cert = ca_params.self_signed(&ca_key).map_err(|e| e.to_string())?;

    let mut san: Vec<SanType> = vec![
        SanType::DnsName("kubernetes".try_into().map_err(|_| "san")?),
        SanType::DnsName("kubernetes.default".try_into().map_err(|_| "san")?),
        SanType::DnsName("kubernetes.default.svc".try_into().map_err(|_| "san")?),
        SanType::DnsName(
            "kubernetes.default.svc.cluster.local"
                .try_into()
                .map_err(|_| "san")?,
        ),
        SanType::DnsName("localhost".try_into().map_err(|_| "san")?),
    ];
    for d in extra_dns {
        if let Ok(n) = d.as_str().try_into() {
            san.push(SanType::DnsName(n));
        }
    }
    san.push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
    for ip in extra_ips {
        san.push(SanType::IpAddress(*ip));
    }

    let srv_key = KeyPair::generate().map_err(|e| e.to_string())?;
    let mut srv_params = CertificateParams::new(Vec::<String>::new()).map_err(|e| e.to_string())?;
    srv_params.subject_alt_names = san;
    srv_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "kube-apiserver");
    let srv_cert = srv_params
        .signed_by(&srv_key, &ca_cert, &ca_key)
        .map_err(|e| e.to_string())?;

    let cert_chain = vec![
        CertificateDer::from(srv_cert.der().to_vec()),
        CertificateDer::from(ca_cert.der().to_vec()),
    ];
    let key = PrivateKeyDer::try_from(srv_key.serialize_der()).map_err(|e| e.to_string())?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)
        .map_err(|e| e.to_string())?;

    Ok(TlsIdentity {
        ca_pem: ca_cert.pem(),
        config: Arc::new(config),
    })
}

impl TlsIdentity {
    /// Serve the API over TLS (blocking). One thread per connection.
    pub fn serve(&self, api: Arc<ApiServer>, addr: &str) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr)?;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let cfg = self.config.clone();
            let api = api.clone();
            crate::server::spawn_conn("kubeapi-tls", move || {
                match rustls::ServerConnection::new(cfg) {
                    Ok(sc) => {
                        let tls = rustls::StreamOwned::new(sc, conn);
                        let _ = api.handle_conn(tls);
                    }
                    Err(e) => eprintln!("slim-kubeapi tls: {e}"),
                }
            });
        }
        Ok(())
    }
}
