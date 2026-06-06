//! LSP base-protocol framing: `Content-Length`-delimited JSON over a byte stream.
//!
//! Each message is `Content-Length: <n>\r\n\r\n<n bytes of UTF-8 JSON>`. Headers are ASCII; any
//! header other than `Content-Length` (e.g. the optional `Content-Type`) is ignored. This is the
//! whole of the LSP "base protocol" — everything above it is ordinary JSON-RPC, which we handle in
//! [`super::client`].
//!
//! The functions are generic over the stream so they can run over a child process's pipes in
//! production and over in-memory [`tokio::io::duplex`] pipes in tests — identical code paths.

use std::io::{Error, ErrorKind};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Read one framed message, returning its raw JSON bytes. `Ok(None)` is a clean EOF (the peer
/// closed the stream between messages).
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut content_length: Option<usize> = None;
    let mut saw_header = false;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF. Clean only if it lands on a message boundary (no half-read headers).
            return if saw_header {
                Err(Error::new(ErrorKind::UnexpectedEof, "EOF mid-header"))
            } else {
                Ok(None)
            };
        }
        saw_header = true;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // blank line terminates the header block
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            content_length = val.trim().parse().ok();
        }
        // Any other header (Content-Type, ...) is ignored.
    }
    let len =
        content_length.ok_or_else(|| Error::new(ErrorKind::InvalidData, "missing Content-Length"))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(Some(body))
}

/// Write one framed message around the given JSON `body`.
pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, body: &[u8]) -> std::io::Result<()> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn round_trips_a_message() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, _aw) = tokio::io::split(a);
        let (_br, mut bw) = tokio::io::split(b);
        write_frame(&mut bw, br#"{"jsonrpc":"2.0","id":1}"#).await.unwrap();
        let mut reader = BufReader::new(ar);
        let got = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(got, br#"{"jsonrpc":"2.0","id":1}"#);
    }

    #[tokio::test]
    async fn reads_back_to_back_messages() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, _aw) = tokio::io::split(a);
        let (_br, mut bw) = tokio::io::split(b);
        write_frame(&mut bw, b"first").await.unwrap();
        write_frame(&mut bw, b"second").await.unwrap();
        let mut reader = BufReader::new(ar);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"first");
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"second");
    }

    #[tokio::test]
    async fn clean_eof_at_boundary_is_none() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, _aw) = tokio::io::split(a);
        drop(b); // close the writer end
        let mut reader = BufReader::new(ar);
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ignores_extra_headers() {
        let (a, b) = tokio::io::duplex(8192);
        let (ar, _aw) = tokio::io::split(a);
        let (_br, mut bw) = tokio::io::split(b);
        bw.write_all(b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\nhi")
            .await
            .unwrap();
        bw.flush().await.unwrap();
        let mut reader = BufReader::new(ar);
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"hi");
    }
}
