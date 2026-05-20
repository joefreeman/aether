//! Buffer lifecycle messages — §6 of the protocol doc.

use crate::envelope::{NotificationMethod, RpcMethod};
use crate::{BufferId, Revision};
use serde::{Deserialize, Serialize};

// ---- buffer/open --------------------------------------------------------------------------------

pub struct BufferOpen;
impl RpcMethod for BufferOpen {
    const NAME: &'static str = "buffer/open";
    type Params = BufferOpenParams;
    type Result = BufferOpenResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferOpenParams {
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferOpenResult {
    pub buffer_id: BufferId,
    pub language: Option<String>,
    pub line_count: u32,
    pub byte_count: u64,
    pub revision: Revision,
    pub dirty: bool,
}

// ---- buffer/save --------------------------------------------------------------------------------

pub struct BufferSave;
impl RpcMethod for BufferSave {
    const NAME: &'static str = "buffer/save";
    type Params = BufferSaveParams;
    type Result = BufferSaveResult;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferSaveParams {
    pub buffer_id: BufferId,
    pub path_index: Option<u32>,
    pub relative_path: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferSaveResult {
    pub saved_at_unix_ms: u64,
    pub revision: Revision,
}

// ---- buffer/close -------------------------------------------------------------------------------

pub struct BufferClose;
impl RpcMethod for BufferClose {
    const NAME: &'static str = "buffer/close";
    type Params = BufferCloseParams;
    type Result = ();
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferCloseParams {
    pub buffer_id: BufferId,
}

// ---- buffer/state (notification) ----------------------------------------------------------------

pub struct BufferState;
impl NotificationMethod for BufferState {
    const NAME: &'static str = "buffer/state";
    type Params = BufferStateParams;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BufferStateParams {
    pub buffer_id: BufferId,
    pub dirty: bool,
    pub revision: Revision,
    pub saved_at_unix_ms: Option<u64>,
}
