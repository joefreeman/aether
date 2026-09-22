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

    /// Whether this method changes the text of the buffer it names.
    ///
    /// Declared here, beside the method's name, because it's a property of the method rather than
    /// of any one call site: the client checks it in its single request funnel and declines
    /// against a read-only buffer without paying a round trip to be refused, so holding a key
    /// down stays quiet instead of streaming errors back. The server refuses these for real —
    /// see `ServerState::editable_doc` — and remains the authority.
    ///
    /// "The buffer it names" is the load-bearing part. `git/apply_hunk` is `false` despite
    /// writing text: invoked on a patch view it stages *into a different buffer*, which is the
    /// one thing a read-only buffer is legitimately the subject of.
    const MUTATES_TEXT: bool = false;

    /// Whether `Ctrl-r` (repeat last change) may re-issue this method against the current
    /// selection.
    ///
    /// True for the cursor-relative edits — the `element/*` input methods and `buffer/cut` — whose
    /// params carry no position, so the same request means "do that here" wherever the cursor now
    /// is. False for everything wholesale (save, reload, format, git) and for history navigation
    /// (undo, redo): those change text too, but repeating them at a new cursor is never what a
    /// repeat key means. Declared on the method, like [`RpcMethod::MUTATES_TEXT`], so the client's
    /// change recorder learns it in the same request funnel and a method added later is classified
    /// where it is defined — the default is the safe one (not repeated).
    ///
    /// Implies `MUTATES_TEXT`; a mutating method that is *not* replayable aborts an insert-session
    /// recording when it lands inside one.
    const REPLAYABLE: bool = false;
}

/// One-way server→client notifications. No response.
pub trait NotificationMethod {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
}
