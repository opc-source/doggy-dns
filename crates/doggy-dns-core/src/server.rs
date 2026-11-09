use crate::config::net::socket_addr;
use crate::config::{DoggyDnsConfig, HttpsConfig, TlsConfig};
use crate::reuseport;
use anyhow::{Context, Result};
use hickory_server::Server;
use hickory_server::server::RequestHandler;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

pub async fn build_server<H: RequestHandler>(
    config: &DoggyDnsConfig,
    handler: H,
) -> Result<Server<H>> {
    let mut server = Server::new(handler);

    let addr = socket_addr(&config.server.listen_addr, config.server.port)
        .context("invalid server listen address")?;

    let udp_count = config.server.reuseport_listen_num as usize;

    // UDP — `reuseport_listen_num` sockets, every one of them armed with
    // `SO_REUSEPORT`. The option is what lets a restart overlap its predecessor
    // on this address; the count only decides how many receive paths the kernel
    // spreads incoming datagrams across.
    for _ in 0..udp_count {
        let udp_socket = reuseport::bind_udp(addr).await?;
        server.register_socket(udp_socket);
    }
    tracing::info!(
        count = udp_count,
        reuseport = reuseport::DNS_PORT_OVERLAP_SUPPORTED,
        "listening on UDP {}",
        addr
    );

    // TCP — one listener, armed the same way: an overlap needs every socket on
    // the address in the group, and a lone plain listener would hold the port
    // against its successor just as firmly as against any other process.
    let tcp_listener = reuseport::bind_tcp(addr).await?;
    let tcp_timeout = Duration::from_secs(config.server.tcp_timeout);
    server.register_listener(tcp_listener, tcp_timeout, 32);
    tracing::info!("listening on TCP {}", addr);

    // TLS (DoT)
    if let Some(tls_config) = &config.server.tls
        && tls_config.enabled
    {
        register_tls(&mut server, config, tls_config).await?;
    }

    // HTTPS (DoH)
    if let Some(https_config) = &config.server.https
        && https_config.enabled
    {
        register_https(&mut server, config, https_config).await?;
    }

    Ok(server)
}

async fn register_tls<H: RequestHandler>(
    server: &mut Server<H>,
    config: &DoggyDnsConfig,
    tls_config: &TlsConfig,
) -> Result<()> {
    let tls_addr = socket_addr(&config.server.listen_addr, tls_config.port)
        .context("invalid TLS listen address")?;

    let tls_server_config = load_rustls_config(&tls_config.cert_path, &tls_config.key_path, "TLS")?;

    let listener = reuseport::bind_tcp(tls_addr).await?;
    let timeout = Duration::from_secs(config.server.tcp_timeout);

    server.register_tls_listener_with_tls_config(listener, timeout, Arc::new(tls_server_config))?;

    tracing::info!("listening on TLS {}", tls_addr);
    Ok(())
}

async fn register_https<H: RequestHandler>(
    server: &mut Server<H>,
    config: &DoggyDnsConfig,
    https_config: &HttpsConfig,
) -> Result<()> {
    let https_addr = socket_addr(&config.server.listen_addr, https_config.port)
        .context("invalid HTTPS listen address")?;

    let mut tls_config =
        load_rustls_config(&https_config.cert_path, &https_config.key_path, "HTTPS")?;
    tls_config.alpn_protocols = vec![b"h2".to_vec()];

    let listener = reuseport::bind_tcp(https_addr).await?;
    let timeout = Duration::from_secs(config.server.tcp_timeout);

    server.register_https_listener_with_tls_config(
        listener,
        timeout,
        Arc::new(tls_config),
        None,
        "/dns-query".to_string(),
    )?;

    tracing::info!("listening on HTTPS {}", https_addr);
    Ok(())
}

/// Load a rustls server config from a PEM certificate/key pair.
///
/// `label` only names the listener in error messages ("TLS" or "HTTPS"), so a
/// bad certificate points at the config section that supplied it.
fn load_rustls_config(
    cert_path: &str,
    key_path: &str,
    label: &str,
) -> Result<rustls::ServerConfig> {
    let cert_pem =
        fs::read(cert_path).with_context(|| format!("failed to read {label} certificate"))?;
    let key_pem =
        fs::read(key_path).with_context(|| format!("failed to read {label} private key"))?;

    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse {label} certificate"))?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .with_context(|| format!("failed to parse {label} private key"))?
        .context("no private key found")?;

    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .with_context(|| format!("failed to build {label} server config"))
}
