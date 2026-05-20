//! Error codes used in JSON-RPC error responses.
//!
//! Reserved JSON-RPC 2.0 codes (`-32700`, `-32600`, `-32601`, `-32602`, `-32603`) coexist with
//! application-specific codes in the implementation-defined `-32000` to `-32099` range.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorCode(pub i32);

impl ErrorCode {
    // JSON-RPC reserved
    pub const PARSE_ERROR: Self = Self(-32700);
    pub const INVALID_REQUEST: Self = Self(-32600);
    pub const METHOD_NOT_FOUND: Self = Self(-32601);
    pub const INVALID_PARAMS: Self = Self(-32602);
    pub const INTERNAL_ERROR: Self = Self(-32603);

    // Aether application errors
    pub const INVALID_TOKEN: Self = Self(-32001);
    pub const INVALID_PATH: Self = Self(-32010);
    pub const BUFFER_NOT_FOUND: Self = Self(-32011);
    pub const VIEWPORT_NOT_FOUND: Self = Self(-32012);
    pub const INVALID_POSITION: Self = Self(-32013);
    pub const STALE_REVISION: Self = Self(-32014);
    pub const BUFFER_HAS_NO_PATH: Self = Self(-32015);
    pub const FILE_IO: Self = Self(-32020);
    pub const LANGUAGE_NOT_FOUND: Self = Self(-32030);

    pub fn code(self) -> i32 {
        self.0
    }
}

impl From<ErrorCode> for i32 {
    fn from(c: ErrorCode) -> i32 {
        c.0
    }
}
