//! Connection handshake messages — §4 of the protocol doc.

use crate::envelope::RpcMethod;
use crate::ClientId;
use serde::{Deserialize, Serialize};

pub struct ClientHello;
impl RpcMethod for ClientHello {
    const NAME: &'static str = "client/hello";
    type Params = ClientHelloParams;
    type Result = ClientHelloResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientHelloParams {
    pub token: String,
    pub client_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClientHelloResult {
    pub client_id: ClientId,
    pub server_version: String,
    pub project: ProjectInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub paths: Vec<String>,
}
