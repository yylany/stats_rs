// 推送爬虫统计信息
// 使用广播的版本；
use super::websocket::connect_to_ws_with_timeout;
use crate::GLOBAL_RUNTIME;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt, TryFutureExt};
use tokio::sync::broadcast::{channel, Receiver, Sender};
use tokio::{
    net::TcpStream,
    time::{Duration, Instant},
};
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{error, info};

// ping 间隔 ms
const PING_KEEP_ALIVE: u64 = 30_000;
// 重连 间隔 ms
const RECONNECTION_DELAY: u64 = 3_000;

pub fn load_broadcast_chan(push_target: Vec<String>) -> Sender<String> {
    println!("初始化消息转推");
    let (create_order_sender, _) = channel(10);

    if !push_target.is_empty() {
        let s = create_order_sender.clone();
        GLOBAL_RUNTIME.spawn(init_websocket(push_target, s));
    }

    create_order_sender
}

async fn init_websocket(push_targets: Vec<String>, msg_chan: Sender<String>) {
    for push_url in push_targets {
        let s = msg_chan.subscribe();
        tokio::spawn(push_loop(push_url, s));
    }
}

async fn push_loop(push_url: String, mut event_receiver: Receiver<String>) {
    loop {
        info!(url = &push_url, "准备连接到推送服务r");

        let socket = match connect_to_ws_with_timeout(&push_url, Duration::from_secs(2)).await {
            Ok(socket) => {
                info!(url = &push_url, "连接推送服务成功");
                socket
            }
            Err(err) => {
                error!(
                    url = &push_url,
                    error = %err,
                    "无法连接，正在重新连接"
                );

                tokio::time::sleep(Duration::from_millis(RECONNECTION_DELAY)).await;
                continue;
            }
        };

        // 开始处理事件
        match process_events(socket, &mut event_receiver).await {
            Ok(()) => {}
            Err(err) => {
                error!(
                    url = &push_url,
                    error = %err,
                    "数据发送异常，正在重新连接"
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn process_events(
    mut socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    event_receiver: &mut Receiver<String>,
) -> Result<()> {
    let now = Instant::now();
    let mut last_heartbeat = now;
    let mut heartbeat_interval =
        tokio::time::interval_at(now, Duration::from_millis(PING_KEEP_ALIVE));

    let heartbear_timeout = Duration::from_millis(2 * PING_KEEP_ALIVE);

    loop {
        tokio::select! {
            item = socket.next() => {
                match item.transpose()? {
                    Some(_) => {
                        last_heartbeat = Instant::now();
                    }
                    None => {
                        // 客户端主动断开连接
                        return Ok(());
                    }
                }
            }
            _ = heartbeat_interval.tick() => {
                // 检测心跳
                anyhow::ensure!(Instant::now() - last_heartbeat < heartbear_timeout, "heartbeat timeout");
                // let ping =  "ping".to_string();

                tokio::time::timeout(
                    Duration::from_secs(2),
                    socket.send(Message::Ping(Vec::from("ping"))).map_err(anyhow::Error::from),
                )
                .map_err(|_| anyhow::anyhow!("timeout"))
                .await
                .and_then(std::convert::identity)?;
                anyhow::ensure!(Instant::now() - last_heartbeat < heartbear_timeout, "heartbeat timeout");
            }
            res = event_receiver.recv() => {
                let pkg = res?;
                tokio::time::timeout(
                    Duration::from_secs(2),
                    socket.send(Message::Text(pkg)).map_err(anyhow::Error::from),
                )
                .map_err(|_| anyhow::anyhow!("timeout"))
                .await
                .and_then(std::convert::identity)?;
            }
        }
    }
}
