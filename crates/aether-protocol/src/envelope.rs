//! JSON-RPC 2.0 envelope and per-method binding traits.

use serde::de::DeserializeOwned;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

pub type RequestId = u64;

/// Phantom marker for the required `"jsonrpc": "2.0"` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JsonRpc;

impl Serialize for JsonRpc {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        "2.0".serialize(s)
    }
}

impl<'de> Deserialize<'de> for JsonRpc {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "2.0" {
            return Err(de::Error::custom("only JSON-RPC 2.0 is supported"));
        }
        Ok(JsonRpc)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: JsonRpc,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: JsonRpc,
    pub id: RequestId,
    pub result: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub jsonrpc: JsonRpc,
    pub id: RequestId,
    pub error: ErrorObject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: JsonRpc,
    pub method: String,
    pub params: serde_json::Value,
}

/// What the server can receive on its WebSocket: a request, full stop. (Clients send no
/// notifications in v1.)
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerInbound {
    Request(Request),
}

/// What the client can receive: response to one of its requests, an error response, or a
/// server-initiated notification.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientInbound {
    Response(Response),
    Error(ErrorResponse),
    Notification(Notification),
}

/// Binds a method name to its param and result types. Implemented by zero-sized marker structs in
/// the per-namespace modules.
pub trait RpcMethod {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
    type Result: Serialize + DeserializeOwned;
}

/// One-way server→client notifications. No response.
pub trait NotificationMethod {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
}
