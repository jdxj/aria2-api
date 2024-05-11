use log::trace;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::error::Error;
use std::sync::atomic::AtomicU64;
use std::sync::{atomic, Arc, Mutex};
use strum::IntoStaticStr;
use tokio::net::TcpStream;
use tokio::sync::oneshot::Sender;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

#[derive(IntoStaticStr)]
pub enum Method {
    AddUri,
}

#[derive(Serialize, Debug)]
pub struct RequestObject<Params> {
    jsonrpc: &'static str,
    method: &'static str,
    params: Params,
    id: Option<String>,
}

impl<Params> RequestObject<Params> {
    pub fn new(method: Method, params: Params, id: Option<String>) -> Self {
        RequestObject {
            jsonrpc: "2.0",
            method: method.into(),
            params,
            id,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct ResponseObject<Res> {
    pub result: Res,
    pub error: Option<ErrorObject>,

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
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl Client {
    pub async fn new(url: &str) -> Self {
        let (ws_stream, resp) = connect_async(url).await.unwrap();
        trace!("websocket response header: {:?}", resp.headers());

        Client {
            next_id: AtomicU64::new(1),
            ws_stream,
        }
    }
}

impl Client {
    fn call<Params, Res>(
        &self,
        req: RequestObject<Params>,
    ) -> Result<ResponseObject<Res>, Box<dyn Error>> {
        panic!("not implement!")
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

    #[tokio::test]
    async fn test_new_client() {
        setup();

        let client = new_client().await;
        sleep(Duration::from_secs(10)).await;
    }
}
