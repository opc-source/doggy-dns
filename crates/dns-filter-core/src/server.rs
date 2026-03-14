use crate::config::{DnsFilterConfig, HttpsConfig, TlsConfig};
use anyhow::{Context, Result};
use hickory_server::Server;
use hickory_server::server::RequestHandler;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, UdpSocket};

pub async fn build_server<H: RequestHandler>(
    config: &DnsFilterConfig,
    handler: H,
) -> Result<Server<H>> {
    let mut server = Server::new(handler);

    // UDP
    let addr: SocketAddr = format!("{}:{}", config.server.listen_addr, config.server.port)
        .parse()
        .context("invalid server address")?;
    let udp_socket = UdpSocket::bind(addr)
        .await
        .context("failed to bind UDP socket")?;
    server.register_socket(udp_socket);
    tracing::info!("listening on UDP {}", addr);

    // TCP
    let tcp_listener = TcpListener::bind(addr)
        .await
        .context("failed to bind TCP listener")?;
    let tcp_timeout = Duration::from_secs(config.server.tcp_timeout);
    server.register_listener(tcp_listener, tcp_timeout);
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
    config: &DnsFilterConfig,
    tls_config: &TlsConfig,
) -> Result<()> {
    let tls_addr: SocketAddr =
        format!("{}:{}", config.server.listen_addr, tls_config.port).parse()?;

    let cert_pem = fs::read(&tls_config.cert_path).context("failed to read TLS certificate")?;
    let key_pem = fs::read(&tls_config.key_path).context("failed to read TLS private key")?;

    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse TLS certificate")?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("failed to parse TLS private key")?
        .context("no private key found")?;

    let tls_server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("failed to build TLS server config")?;

    let listener = TcpListener::bind(tls_addr).await?;
    let timeout = Duration::from_secs(config.server.tcp_timeout);

    server.register_tls_listener_with_tls_config(listener, timeout, Arc::new(tls_server_config))?;

    tracing::info!("listening on TLS {}", tls_addr);
    Ok(())
}

async fn register_https<H: RequestHandler>(
    server: &mut Server<H>,
    config: &DnsFilterConfig,
    https_config: &HttpsConfig,
) -> Result<()> {
    let https_addr: SocketAddr =
        format!("{}:{}", config.server.listen_addr, https_config.port).parse()?;

    let cert_pem = fs::read(&https_config.cert_path).context("failed to read HTTPS certificate")?;
    let key_pem = fs::read(&https_config.key_path).context("failed to read HTTPS private key")?;

    let certs = rustls_pemfile::certs(&mut &cert_pem[..])
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse HTTPS certificate")?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])
        .context("failed to parse HTTPS private key")?
        .context("no private key found")?;

    let mut tls_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls_config.alpn_protocols = vec![b"h2".to_vec()];

    let listener = TcpListener::bind(https_addr).await?;
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
