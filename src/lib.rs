use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{future, pin_mut, SinkExt, StreamExt};
use log::{error, info, trace};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::AtomicU64;
use std::sync::{atomic, Arc, Mutex};
use strum::IntoStaticStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[derive(IntoStaticStr)]
pub enum Method {
    #[strum(serialize = "aria2.addUri")]
    Aria2AddUri,
    #[strum(serialize = "aria2.getVersion")]
    Aria2GetVersion,
    #[strum(serialize = "aria2.saveSession")]
    Aria2SaveSession,
}

#[derive(Serialize, Debug)]
struct RequestObject<'a> {
    jsonrpc: &'static str,
    method: &'static str,
    params: &'a Vec<u8>,
    id: Option<String>,
}

impl<'a> RequestObject<'a> {
    fn new(method: Method, params: &'a Vec<u8>, id: Option<String>) -> RequestObject<'a> {
        RequestObject {
            jsonrpc: "2.0",
            method: method.into(),
            params,
            id,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct ResponseObject {
    result: Vec<u8>,
    error: Option<ErrorObject>,

    jsonrpc: String,
    id: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ErrorObject {
    pub code: i16,
    pub message: Option<String>,
    pub data: Option<String>,
}

pub struct Client {
    next_id: AtomicU64,
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    // read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    // 定时清理, 否则内存泄漏
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
}

impl Client {
    pub async fn new(url: &str) -> Self {
        // 连接 ws 服务
        let (ws_stream, resp) = connect_async(url).await.unwrap();
        trace!("websocket response header: {:?}", resp.headers());

        // split
        let (write, read) = ws_stream.split();
        let id_map = Arc::new(Mutex::new(
            HashMap::<String, oneshot::Sender<ResponseObject>>::new(),
        ));
        tokio::spawn(read_message(read, id_map.clone()));

        Client {
            next_id: AtomicU64::new(1),
            write,
            id_map,
        }
    }
}

async fn read_message(
    mut reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
) {
    while let Some(message) = reader.next().await {
        let message = message.unwrap();
        if let Message::Text(data) = message {
            let result = serde_json::from_str::<ResponseObject>(&data).unwrap();
            // todo: 可能是通知, 没有 id
            let id = &result.id;
            match id {
                // 有值, 是 rpc 响应
                Some(id) => {
                    let tx = {
                        let mut mg = id_map.lock().unwrap();
                        mg.remove(id).unwrap()
                    };
                    if let Err(e) = tx.send(result) {
                        error!("send err: {:?}", e);
                    }
                }
                // 服务端发来的通知
                _ => {
                    // todo: 发送到 channel
                    info!("通知未发送到 channel: {:?}", result)
                }
            }
        }
    }
}

impl Client {
    async fn call<'a>(
        &mut self,
        req: RequestObject<'a>,
    ) -> Result<Option<ResponseObject>, Box<dyn Error>> {
        let (tx, rx) = oneshot::channel();
        // 有 id, 说明是 rpc 请求
        // 先注册消息回调通知, 然后再发送消息
        if let Some(id) = &req.id {
            let mut mg = self.id_map.lock().unwrap();
            mg.insert(id.to_string(), tx);
        }

        let data = serde_json::to_string(&req)?;
        if let Err(e) = self.write.send(Message::Text(data)).await {
            error!("send message err: {:?}", e);

            // 删除消息回调通知
            if let Some(id) = &req.id {
                let mut mg = self.id_map.lock().unwrap();
                mg.remove(id);
            }
        }

        // 发送成功, 如果是 rpc 请求, 那么需要等待响应
        if let Some(id) = &req.id {
            let res = rx.await?;
            Ok(Some(res))
        } else {
            // 是通知
            Ok(None)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::env;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use tokio::time::sleep;
    use tokio_tungstenite::tungstenite::client;

    fn setup() {
        env::set_var("RUST_LOG", "trace");
        pretty_env_logger::init()
    }

    async fn new_client() -> Client {
        Client::new("ws://172.18.2.11:6800/jsonrpc").await
    }

    #[test]
    fn test_enum() {
        println!("{:?}", <Method as Into<&str>>::into(Method::Aria2AddUri))
    }

    #[test]
    fn test_oneshot() {
        let (tx, rx) = tokio::sync::oneshot::channel();
    }

    #[tokio::test]
    async fn test_new_client() {
        setup();

        let client = new_client().await;
        sleep(Duration::from_secs(10)).await;
    }
}
