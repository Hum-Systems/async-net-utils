use anyhow::{anyhow, bail};
use async_io::Async;
use async_rustls::client::TlsStream;
use async_rustls::rustls::ClientConfig;
use async_rustls::webpki::DNSNameRef;
use async_rustls::TlsConnector;
use async_std::future::timeout;
use async_std::io;
use async_std::net::TcpStream;
use async_std::net::{SocketAddr, ToSocketAddrs};
use async_std::os::unix::io::{FromRawFd, IntoRawFd};
use async_std::sync::Arc;
use async_tungstenite::tungstenite::protocol::WebSocketConfig;
use async_tungstenite::{client_async_with_config, WebSocketStream};
use http_types::{Request, Response};
use io::ErrorKind;
use libc::EINPROGRESS;
use socket2::{Domain, Protocol, Socket, Type};
use std::time::Duration;

const MAX_WAIT: Duration = Duration::from_secs(10);

pub async fn connect_https(
    config: Arc<ClientConfig>,
    interface: Option<&[u8]>,
    host: &str,
    port: u16,
    request: Request,
) -> anyhow::Result<Response> {
    let tls_stream = connect_tls(config, interface, host, port).await?;
    let response = match async_h1::connect(tls_stream, request).await {
        Ok(response) => response,
        Err(err) => bail!("http error: {:?}", err),
    };
    Ok(response)
}

pub async fn connect_wss(
    config: Arc<ClientConfig>,
    interface: Option<&[u8]>,
    host: &str,
    port: u16,
) -> anyhow::Result<WebSocketStream<TlsStream<TcpStream>>> {
    let tls_stream = connect_tls(config, interface, host, port).await?;
    let mut cfg = WebSocketConfig::default();
    cfg.max_send_queue = Some(1);
    let ws_url = format!("wss://{}:{}", host, port);
    let (ws, _) = client_async_with_config(&ws_url, tls_stream, Some(cfg)).await?;
    Ok(ws)
}

pub async fn connect_tls(
    config: Arc<ClientConfig>,
    interface: Option<&[u8]>,
    host: &str,
    port: u16,
) -> anyhow::Result<TlsStream<TcpStream>> {
    let tcp_stream = connect_tcp(interface, host, port).await?;
    let dns_name_ref = DNSNameRef::try_from_ascii_str(host)?;
    let tls_connector = TlsConnector::from(config);
    let tls_stream = tls_connector.connect(dns_name_ref, tcp_stream).await?;
    Ok(tls_stream)
}

pub async fn connect_tcp(
    interface: Option<&[u8]>,
    host: &str,
    port: u16,
) -> anyhow::Result<TcpStream> {
    let addrs_fut = (host, port).to_socket_addrs();
    let addrs: std::vec::IntoIter<std::net::SocketAddr> = timeout(MAX_WAIT, addrs_fut).await??;

    let mut last_err: Option<anyhow::Error> = None;
    for addr in addrs {
        match timeout(MAX_WAIT, connect_tcp_addr(interface, addr)).await {
            Ok(result) => match result {
                Ok(stream) => return Ok(stream),
                Err(err) => last_err = Some(err.into()),
            },
            Err(err) => last_err = Some(err.into()),
        }
    }
    Err(last_err.unwrap_or(anyhow!("could not resolve to any address")))
}

async fn connect_tcp_addr(interface: Option<&[u8]>, addr: SocketAddr) -> io::Result<TcpStream> {
    let domain = Domain::for_address(addr);

    let tcp_socket = Socket::new(domain, Type::STREAM.nonblocking(), Some(Protocol::TCP))?;
    tcp_socket.bind_device(interface)?;

    match tcp_socket.connect(&addr.into()) {
        Ok(_) => {}
        Err(err) if err.raw_os_error() == Some(EINPROGRESS) => {}
        Err(err) if err.kind() == ErrorKind::WouldBlock => {}
        Err(err) => return Err(err),
    }
    let tcp_stream = Async::new(std::net::TcpStream::from(tcp_socket))?;
    tcp_stream.writable().await?;
    if let Some(err) = tcp_stream.get_ref().take_error()? {
        return Err(err);
    }

    let tcp_fd = tcp_stream.into_inner()?.into_raw_fd();
    let tcp_stream: TcpStream;
    unsafe {
        tcp_stream = TcpStream::from_raw_fd(tcp_fd);
    };
    Ok(tcp_stream)
}
