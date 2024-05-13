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
use tokio::sync::{broadcast, mpsc, oneshot};
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

#[derive(Deserialize, Debug, Clone)]
pub struct Version {
    pub version: String,
    #[serde(rename = "enabledFeatures")]
    pub enabled_features: Vec<String>,
}

impl Display for Version {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            r"Version:
  version: {}
  enabled features: {:?}",
            self.version, self.enabled_features
        )
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct Status {
    pub gid: String,
    /// active, waiting, paused, error, complete, removed
    pub status: String,
    /// Total length of the download in bytes.
    #[serde(rename = "totalLength")]
    pub total_length: String,
    /// Completed length of the download in bytes.
    #[serde(rename = "completedLength")]
    pub completed_length: String,
    /// Uploaded length of the download in bytes.
    #[serde(rename = "uploadLength")]
    pub upload_length: String,
    /// Hexadecimal representation of the download progress.
    pub bitfield: String,
    /// Download speed of this download measured in bytes/sec.
    #[serde(rename = "downloadSpeed")]
    pub download_speed: String,
    /// Upload speed of this download measured in bytes/sec.
    #[serde(rename = "uploadSpeed")]
    pub upload_speed: String,
    /// InfoHash. BitTorrent only.
    #[serde(rename = "infoHash")]
    pub info_hash: String,
}

impl Display for Status {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            r"Status:
  gid: {}
  status: {}
  total length: {}bytes
  completed length: {}bytes
  upload length: {}bytes
  bitfield: {}
  download speed: {}bytes/sec
  upload speed: {}bytes/sec
  info_hash: {}",
            self.gid,
            self.status,
            self.total_length,
            self.completed_length,
            self.upload_length,
            self.bitfield,
            self.download_speed,
            self.upload_speed,
            self.info_hash,
        )
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct GlobalStat {
    /// Overall download speed (byte/sec).
    #[serde(rename = "downloadSpeed")]
    pub download_speed: String,
    /// Overall upload speed(byte/sec).
    #[serde(rename = "uploadSpeed")]
    pub upload_speed: String,
    /// The number of active downloads.
    #[serde(rename = "numActive")]
    pub num_active: String,
    /// The number of waiting downloads.
    #[serde(rename = "numWaiting")]
    pub num_waiting: String,
    /// The number of stopped downloads in the current session.
    /// This value is capped by the --max-download-result option.
    #[serde(rename = "numStopped")]
    pub num_stopped: String,
    /// The number of stopped downloads in the current session
    /// and not capped by the --max-download-result option.
    #[serde(rename = "numStoppedTotal")]
    pub num_stopped_total: String,
}

impl Display for GlobalStat {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            r"Global Stat:
  download speed: {}bytes/sec
  upload speed: {}/bytes/sec
  num active: {}
  num waiting: {}
  num stopped: {}
  num stopped total: {}
",
            self.download_speed,
            self.upload_speed,
            self.num_active,
            self.num_waiting,
            self.num_stopped,
            self.num_stopped_total,
        )
    }
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

impl Display for ResponseObject {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "result: {:?}\nerror: {:?}\njsonrpc: {:?}\nid: {:?}\nmethod: {:?}\nparams: {:?}",
            &self.result, &self.error, &self.jsonrpc, &self.id, &self.method, &self.params
        )
    }
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

#[derive(Clone)]
pub struct Client {
    secret: Option<String>,
    next_id: Arc<AtomicU64>,
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
    notify_tx: broadcast::Sender<ResponseObject>,
    message_tx: mpsc::Sender<(RequestObject, oneshot::Sender<ResponseObject>)>,
}

impl Client {
    pub async fn new(url: &str, secret: Option<&str>) -> Self {
        // 连接 ws 服务
        let (ws_stream, resp) = connect_async(url).await.unwrap();
        debug!("connect async response header: {:?}", resp.headers());

        // split stream
        let (write, read) = ws_stream.split();
        // id -> 消息回调通知
        let id_map = Arc::new(Mutex::new(
            HashMap::<String, oneshot::Sender<ResponseObject>>::new(),
        ));

        // notification channel
        let (notify_tx, _) = broadcast::channel(100);
        // write/read message channel
        let (message_tx, message_rx) = mpsc::channel(100);
        // read_message 在收到 notification 后会发送到 notify_tx
        tokio::spawn(read_message(read, id_map.clone(), notify_tx.clone()));
        // write_message 从 rx 中接收要发送的 RequestObject
        tokio::spawn(write_message(write, id_map.clone(), message_rx));

        Client {
            secret: secret.map(|s| s.to_string()),
            next_id: Arc::new(AtomicU64::new(1)),
            id_map,
            notify_tx,
            message_tx,
        }
    }
}

async fn read_message(
    mut reader: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
    notify_tx: broadcast::Sender<ResponseObject>,
) {
    info!("start read message");

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

    info!("end read message");
}

async fn write_message(
    mut write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    id_map: Arc<Mutex<HashMap<String, oneshot::Sender<ResponseObject>>>>,
    mut message_rx: mpsc::Receiver<(RequestObject, oneshot::Sender<ResponseObject>)>,
) {
    info!("start write message");

    while let Some((req, res_tx)) = message_rx.recv().await {
        // 有 id, 说明是 rpc 请求
        // 先注册消息回调通知, 然后再发送消息
        if req.is_request() {
            let mut mg = id_map.lock().unwrap();
            mg.insert(req.id().to_string(), res_tx);
        }

        let data = serde_json::to_string(&req).unwrap();
        debug!("encode request object: {:?}", req);

        if let Err(e) = write.send(Message::Text(data)).await {
            // 发送失败删除消息回调通知
            if req.is_request() {
                let mut mg = id_map.lock().unwrap();
                mg.remove(req.id());
            }
            error!("write message err: {:?}, message: {:?}", e, req);
            break;
        }
    }

    info!("end write messages")
}

impl Client {
    async fn call(&self, req: RequestObject) -> Result<Option<ResponseObject>, Box<dyn Error>> {
        let is_request = req.is_request();
        let id = req.id.clone();
        let (tx, rx) = oneshot::channel();

        self.message_tx.send((req, tx)).await?;

        // 发送成功, 如果是 rpc 请求, 那么需要等待响应
        if is_request {
            match time::timeout(Duration::from_secs(10), rx).await {
                Err(e) => {
                    // 超时, 删除消息回调通知
                    let mut mg = self.id_map.lock().unwrap();
                    mg.remove(&id.unwrap());
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

    fn next_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    pub async fn get_version(&self) -> Result<Version, Box<dyn Error>> {
        let params_array = new_params(self.secret.as_deref());
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

    pub async fn add_uri(&self, uri: &str) -> Result<String, Box<dyn Error>> {
        // 添加 uris, 目前每次调用只添加一个 uri
        let mut uris = Vec::new();
        uris.push(JsonValue::String(uri.to_string()));
        // 添加 token
        let mut params_array = new_params(self.secret.as_deref());
        params_array.push(JsonValue::Array(uris));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::AddUri, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        // 解析响应结果
        unwrap_result_for_string(res)
    }

    pub async fn remove(&self, gid: &str) -> Result<String, Box<dyn Error>> {
        let mut params_array = new_params(self.secret.as_deref());
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::Remove, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        unwrap_result_for_string(res)
    }

    pub async fn pause(&self, gid: &str) -> Result<String, Box<dyn Error>> {
        let mut params_array = new_params(self.secret.as_deref());
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::Pause, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        unwrap_result_for_string(res)
    }

    pub async fn unpause(&self, gid: &str) -> Result<String, Box<dyn Error>> {
        let mut params_array = new_params(self.secret.as_deref());
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::Unpause, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        unwrap_result_for_string(res)
    }

    pub async fn tell_status(&self, gid: &str) -> Result<Status, Box<dyn Error>> {
        let mut params_array = new_params(self.secret.as_deref());
        params_array.push(JsonValue::String(gid.to_string()));
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::TellStatus, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        if let Some(result) = res.result {
            let status = serde_json::from_value::<Status>(result)?;
            Ok(status)
        } else if let Some(err) = res.error {
            Err(Box::new(err))
        } else {
            panic!("{}", MUST_RESULT)
        }
    }

    pub async fn get_global_stat(&self) -> Result<GlobalStat, Box<dyn Error>> {
        let params_array = new_params(self.secret.as_deref());
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::GetGlobalStat, params, Some(self.next_id()));
        let res = self.call(req).await?.expect(MUST_RESPONSE);

        if let Some(result) = res.result {
            let stat = serde_json::from_value::<GlobalStat>(result)?;
            Ok(stat)
        } else if let Some(err) = res.error {
            Err(Box::new(err))
        } else {
            panic!("{}", MUST_RESULT)
        }
    }

    pub async fn purge_download_result(&self) -> Result<String, Box<dyn Error>> {
        let params_array = new_params(self.secret.as_deref());
        let params = JsonValue::Array(params_array);

        let req = RequestObject::new(Method::PurgeDownloadResult, params, Some(self.next_id()));
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
    use tokio::time::sleep;

    fn setup() {
        // env::set_var("RUST_LOG", "trace");
        env::set_var("RUST_LOG", "aria2_api=trace");
        pretty_env_logger::init()
    }

    async fn new_client() -> Client {
        let secret = env::var("ARIA2_SECRET").unwrap();
        Client::new("ws://172.18.2.11:6800/jsonrpc", Some(&secret)).await
    }

    #[test]
    fn test_enum() {
        println!("{:?}", <Method as Into<&str>>::into(Method::AddUri))
    }

    #[tokio::test]
    async fn test_get_version() {
        setup();

        let client = new_client().await;

        let res = client.get_version().await;
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

        let client = new_client().await;

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
        let uri = "https://github.com/sxyazi/yazi/releases/download/v0.2.5/yazi-x86_64-unknown-linux-gnu.zip";
        let res = client.add_uri(uri).await;
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

        let client = new_client().await;

        let res = client.remove("2089b05ecca3d829").await;
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
    async fn test_get_global_stat() {
        setup();

        let client = new_client().await;

        let res = client.get_global_stat().await;
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
    async fn test_purge_download_result() {
        setup();

        let client = new_client().await;

        let res = client.purge_download_result().await;
        match res {
            Ok(result) => {
                info!("result: {:?}", result);
            }
            Err(e) => {
                error!("err: {:?}", e)
            }
        }

        sleep(Duration::from_secs(500)).await;
    }

    #[test]
    fn test_format() {
        let v = Status {
            gid: "abc".to_string(),
            status: "def".to_string(),
            total_length: "1".to_string(),
            completed_length: "2".to_string(),
            upload_length: "3".to_string(),
            bitfield: "ghi".to_string(),
            download_speed: "4".to_string(),
            upload_speed: "5".to_string(),
            info_hash: "jkl".to_string(),
        };
        println!("{}", v);

        let mut ef = Vec::new();
        ef.push("abc".to_string());

        let v = Version {
            version: "v0.1.0".to_string(),
            enabled_features: ef,
        };
        println!("{}", v);

        let v = GlobalStat {
            download_speed: "1".to_string(),
            upload_speed: "2".to_string(),
            num_active: "3".to_string(),
            num_waiting: "4".to_string(),
            num_stopped: "5".to_string(),
            num_stopped_total: "6".to_string(),
        };
        println!("{}", v);
    }
}
