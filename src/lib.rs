use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use strum::IntoStaticStr;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[derive(IntoStaticStr)]
pub enum Method {
    #[strum(serialize = "aria2.addUri")]
    AddUri,
    #[strum(serialize = "aria2.getVersion")]
    GetVersion,
    #[strum(serialize = "aria2.saveSession")]
    SaveSession,
}

#[derive(Deserialize, Debug)]
pub struct Version {
    pub version: String,
    #[serde(rename = "enabledFeatures")]
    pub enabled_features: Vec<String>,
}

#[derive(Serialize, Debug)]
struct RequestObject {
    jsonrpc: &'static str,
    method: &'static str,
    params: JsonValue,
    id: Option<String>,
}

impl RequestObject {
    fn new(method: Method, params: JsonValue, id: Option<String>) -> RequestObject {
        RequestObject {
            jsonrpc: "2.0",
            method: method.into(),
            params,
            id,
        }
    }

    fn is_request(&self) -> bool {
        if let Some(id) = &self.id {
            if id.len() > 0 {
                return true;
            }
        }
        return false;
    }

    fn id(&self) -> &str {
        self.id.as_deref().unwrap()
    }
}

#[derive(Deserialize, Debug)]
pub struct ResponseObject {
    result: Option<JsonValue>,
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

impl Display for ErrorObject {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "code: {}, message: {:?}, data: {:?}",
            self.code, self.message, self.data
        )
    }
}

impl Error for ErrorObject {}

pub struct Client {
    next_id: AtomicU64,
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    // read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    // 定时清理, 否则内存泄漏?
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
    trace!("start read message");

    while let Some(message) = reader.next().await {
        // todo: 发生网络, 直接 panic 不太好?
        let message = message.unwrap();
        if let Message::Text(data) = message {
            // todo: 解析错误, 直接 panic 不太好?
            let result = serde_json::from_str::<ResponseObject>(&data).unwrap();
            // todo: 可能是通知, 没有 id
            let id = &result.id;
            match id {
                // 有值, 是 rpc 响应
                Some(id) => {
                    let tx = {
                        let mut mg = id_map.lock().unwrap();
                        mg.remove(id).expect("oneshot sender not found")
                    };
                    if let Err(e) = tx.send(result) {
                        error!("send ResponseObject err: {:?}", e);
                    }
                }
                // 服务端发来的通知
                _ => {
                    // todo: 发送到 channel
                    warn!("通知未发送到 channel: {:?}", result)
                }
            }
        }
    }
}

impl Client {
    async fn call(&mut self, req: RequestObject) -> Result<Option<ResponseObject>, Box<dyn Error>> {
        let (tx, rx) = oneshot::channel();
        // 有 id, 说明是 rpc 请求
        // 先注册消息回调通知, 然后再发送消息
        if req.is_request() {
            let mut mg = self.id_map.lock().unwrap();
            mg.insert(req.id().to_string(), tx);
        }

        let data = serde_json::to_string(&req)?;
        debug!("encode request object: {:?}", data);

        if let Err(e) = self.write.send(Message::Text(data)).await {
            // 删除消息回调通知
            if req.is_request() {
                let mut mg = self.id_map.lock().unwrap();
                mg.remove(req.id());
            }
            return Err(Box::new(e));
        }

        // 发送成功, 如果是 rpc 请求, 那么需要等待响应
        if req.is_request() {
            let res = rx.await?;
            Ok(Some(res))
        } else {
            // 是通知
            Ok(None)
        }
    }

    pub async fn get_version(&mut self, secret: Option<&str>) -> Result<Version, Box<dyn Error>> {
        let mut params_array = Vec::new();
        if let Some(sec) = secret {
            let token = "token:".to_string() + sec;
            params_array.push(token);
        }

        let params = serde_json::json!(params_array);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let req = RequestObject::new(Method::GetVersion, params, Some(id));
        let res = self.call(req).await?.expect("request must had response");

        // 解析响应结果
        if let Some(result) = res.result {
            let version = serde_json::from_value::<Version>(result)?;
            Ok(version)
        } else if let Some(err) = res.error {
            Err(Box::new(err))
        } else {
            panic!("neither result nor err")
        }
    }

    pub async fn add_uri(
        &mut self,
        secret: Option<&str>,
        uri: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut params_array = Vec::new();
        // 添加 token
        if let Some(sec) = secret {
            let token = "token:".to_string() + sec;
            params_array.push(serde_json::Value::String(token));
        }
        // 添加 uri, 目前每次只添加一个 uri
        let mut uris = Vec::new();
        uris.push(serde_json::Value::String(uri.to_string()));
        let uris = serde_json::Value::Array(uris);
        params_array.push(uris);

        let params = serde_json::json!(params_array);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let req = RequestObject::new(Method::AddUri, params, Some(id));
        let res = self.call(req).await?.expect("request must had response");

        // 解析响应结果
        if let Some(result) = res.result {
            let gid = serde_json::from_value::<String>(result)?;
            Ok(gid)
        } else if let Some(err) = res.error {
            Err(Box::new(err))
        } else {
            panic!("neither string nor err")
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use log::debug;
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
        println!("{:?}", <Method as Into<&str>>::into(Method::AddUri))
    }

    #[tokio::test]
    async fn test_new_client() {
        setup();

        let client = new_client().await;
        sleep(Duration::from_secs(10)).await;
    }

    fn new_request_object() -> RequestObject {
        let secret = env::var("ARIA2_SECRET").unwrap();
        let token = "token:".to_string() + &secret;
        let mut params_array = Vec::new();
        params_array.push(token);
        let params = serde_json::json!(params_array);

        let id = Some("1".to_string());

        RequestObject::new(Method::GetVersion, params, id)
    }

    #[tokio::test]
    async fn test_call() {
        setup();

        let mut client = new_client().await;
        let req = new_request_object();
        let res = client.call(req).await.unwrap().unwrap();

        let version = serde_json::from_value::<Version>(res.result.unwrap()).unwrap();
        println!("version: {:?}", version)
    }

    #[tokio::test]
    async fn test_get_version() {
        setup();

        let mut client = new_client().await;

        let secret = env::var("ARIA2_SECRET").unwrap();
        let res = client.get_version(Some(&secret)).await;
        match res {
            Ok(version) => {
                println!("version: {:?}", version)
            }
            Err(e) => {
                error!("err: {:?}", e)
            }
        }
    }

    #[tokio::test]
    async fn test_add_uri() {
        setup();

        let mut client = new_client().await;

        let secret = env::var("ARIA2_SECRET").unwrap();
        let uri = "https://github.com/sxyazi/yazi/releases/download/v0.2.5/yazi-x86_64-unknown-linux-gnu.zip";
        let res = client.add_uri(Some(&secret), uri).await;
        match res {
            Ok(gid) => {
                info!("gid: {:?}", gid);
            }
            Err(e) => {
                error!("err: {:?}", e)
            }
        }

        sleep(Duration::from_secs(500)).await;
    }
}
