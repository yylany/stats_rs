use anyhow::Result;
use futures_util::TryFutureExt;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, error::UrlError, handshake::client::Response, Error as WsError,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// 连接到 ws url地址
pub async fn connect_ws<R>(
    url: R,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), WsError>
where
    R: IntoClientRequest + Unpin,
{
    let request = url.into_client_request()?;
    let domain = match request.uri().host() {
        Some(d) => d.to_string(),
        None => return Err(WsError::Url(UrlError::NoHostName)),
    };
    let port = request
        .uri()
        .port_u16()
        .or_else(|| match request.uri().scheme_str() {
            Some("wss") => Some(443),
            Some("ws") => Some(80),
            _ => None,
        })
        .ok_or(WsError::Url(UrlError::UnsupportedUrlScheme))?;

    let addr = format!("{}:{}", domain, port);

    connect_ws_with_addr(request, addr).await
}

//连接ws url地址，指定连接到ip
pub async fn connect_ws_with_addr<R>(
    request: R,
    addr: String,
) -> Result<(WebSocketStream<MaybeTlsStream<TcpStream>>, Response), WsError>
where
    R: IntoClientRequest + Unpin,
{
    let request = request.into_client_request()?;
    let inbound = TcpStream::connect(addr).await.map_err(WsError::Io)?;
    inbound.set_nodelay(true)?;
    tokio_tungstenite::client_async_tls(request, inbound).await
}

pub async fn connect_to_ws_with_timeout(
    url: &str,
    d: Duration,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let (socket, _) = tokio::time::timeout(d, connect_ws(url).map_err(anyhow::Error::from))
        .map_err(|_| anyhow::anyhow!("timeout"))
        .await
        .and_then(std::convert::identity)?;
    Ok(socket)
}
