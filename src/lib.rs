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
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::time::{self, Duration};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

static MUST_RESPONSE: &'static str = "request must had response";
static MUST_RESULT: &'static str = "neither result nor err";

#[derive(IntoStaticStr)]
pub enum Method {
    #[strum(serialize = "aria2.addUri")]
    AddUri,
    #[strum(serialize = "aria2.remove")]
    Remove,
    #[strum(serialize = "aria2.pause")]
    Pause,
    #[strum(serialize = "aria2.unpause")]
    Unpause,

    #[strum(serialize = "aria2.tellStatus")]
    TellStatus,
    #[strum(serialize = "aria2.getGlobalStat")]
    GetGlobalStat,
    #[strum(serialize = "aria2.getVersion")]
    GetVersion,

    #[strum(serialize = "aria2.purgeDownloadResult")]
    PurgeDownloadResult,
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

#[derive(Deserialize, Debug, Clone)]
pub struct ResponseObject {
    result: Option<JsonValue>,
    error: Option<ErrorObject>,

    jsonrpc: String,
    id: Option<String>,
    method: Option<String>,
    params: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug, Clone)]
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
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
    notify_tx: broadcast::Sender<ResponseObject>,
}

impl Client {
    pub async fn new(url: &str) -> Self {
        // 连接 ws 服务
        let (ws_stream, resp) = connect_async(url).await.unwrap();
        trace!("websocket response header: {:?}", resp.headers());

        // split stream
        let (write, read) = ws_stream.split();
        let id_map = Arc::new(Mutex::new(
            HashMap::<String, oneshot::Sender<ResponseObject>>::new(),
        ));

        // notification channel
        let (notify_tx, _) = broadcast::channel(100);
        tokio::spawn(read_message(read, id_map.clone(), notify_tx.clone()));

        Client {
            next_id: AtomicU64::new(1),
            write,
            id_map,
            notify_tx,
        }
    }
}

async fn read_message(
    mut reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
    notify_tx: broadcast::Sender<ResponseObject>,
) {
    trace!("start read message");

    while let Some(message) = reader.next().await {
        // todo: 发生网络, 直接 panic 不太好?
        let message = message.unwrap();
        if let Message::Text(data) = message {
            let res = match serde_json::from_str::<ResponseObject>(&data) {
                Ok(v) => v,
                Err(e) => {
                    error!("parse ResponseObject err: {:?}, data: {:?}", e, data);
                    continue;
                }
            };
            let id = &res.id;
            match id {
                // 有值, 是 rpc 响应
                Some(id) => {
                    let tx = {
                        let mut mg = id_map.lock().unwrap();
                        mg.remove(id).expect("oneshot sender not found")
                    };
                    if let Err(e) = tx.send(res) {
                        error!("send ResponseObject err: {:?}", e);
                    }
                }
                // 没值, 是通知
                _ => {
                    // 服务端发来的通知发送到 notification
                    if let Err(e) = notify_tx.send(res) {
                        error!("try send notification err: {:?}", e)
                    }
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
            match time::timeout(Duration::from_secs(10), rx).await {
                Err(e) => {
                    // 超时, 删除消息回调通知
                    let mut mg = self.id_map.lock().unwrap();
                    mg.remove(req.id());
                    Err(Box::new(e))
                }
                Ok(result) => {
                    let res = result?;
                    Ok(Some(res))
                }
            }
        } else {
            // 是通知
            Ok(None)
        }
    }

    /// 如果想接收服务端的 notification 请尽早调用, 否则会丢失消息.
    pub fn notification_receiver(&self) -> broadcast::Receiver<ResponseObject> {
        self.notify_tx.subscribe()
    }

    fn next_id(&mut self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    pub async fn get_version(&mut self, secret: Option<&str>) -> Result<Version, Box<dyn Error>> {
        let params_array = new_params(secret);
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::GetVersion, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        // 解析响应结果
        if let Some(result) = res.result {
            let version = serde_json::from_value::<Version>(result)?;
            Ok(version)
        } else if let Some(err) = res.error {
            Err(Box::new(err))
        } else {
            panic!("{}", MUST_RESULT)
        }
    }

    pub async fn add_uri(
        &mut self,
        secret: Option<&str>,
        uri: &str,
    ) -> Result<String, Box<dyn Error>> {
        // 添加 uris, 目前每次调用只添加一个 uri
        let mut uris = Vec::new();
        uris.push(JsonValue::String(uri.to_string()));
        // 添加 token
        let mut params_array = new_params(secret);
        params_array.push(JsonValue::Array(uris));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::AddUri, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        // 解析响应结果
        unwrap_result_for_string(res)
    }

    pub async fn remove(
        &mut self,
        secret: Option<&str>,
        gid: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut params_array = new_params(secret);
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::Remove, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        unwrap_result_for_string(res)
    }

    pub async fn pause(
        &mut self,
        secret: Option<&str>,
        gid: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut params_array = new_params(secret);
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::Pause, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        unwrap_result_for_string(res)
    }

    pub async fn unpause(
        &mut self,
        secret: Option<&str>,
        gid: &str,
    ) -> Result<String, Box<dyn Error>> {
        let mut params_array = new_params(secret);
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::Unpause, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        unwrap_result_for_string(res)
    }
}

fn new_params(secret: Option<&str>) -> Vec<serde_json::Value> {
    let mut params_array = Vec::new();
    if let Some(sec) = secret {
        let token = "token:".to_string() + sec;
        params_array.push(JsonValue::String(token));
    }
    params_array
}

fn unwrap_result_for_string(res: ResponseObject) -> Result<String, Box<dyn Error>> {
    if let Some(result) = res.result {
        let gid = serde_json::from_value::<String>(result)?;
        Ok(gid)
    } else if let Some(err) = res.error {
        Err(Box::new(err))
    } else {
        panic!("{}", MUST_RESULT)
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

    fn setup() {
        // env::set_var("RUST_LOG", "trace");
        env::set_var("RUST_LOG", "aria2_api=trace");
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

        // 打印通知
        let mut notification = client.notification_receiver();
        tokio::spawn(async move {
            loop {
                match notification.recv().await {
                    Ok(res) => {
                        info!("response: {:?}", res)
                    }
                    Err(e) => {
                        error!("err: {:?}", e)
                    }
                }
            }
        });

        // 添加下载请求
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

    #[tokio::test]
    async fn test_remove() {
        setup();

        let mut client = new_client().await;

        let secret = env::var("ARIA2_SECRET").unwrap();
        let res = client.remove(Some(&secret), "2089b05ecca3d829").await;
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
