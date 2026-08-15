//! Web-client share links — the `?workspace=&root=&file=` (+ 1-based `#L:C`) URL scheme the web
//! shell boots from and its picker links use (`web/src/shell.ts`: the boot parser and
//! `pickerItemUrl`/`fileQuery`). One builder shared by the core's copy-web-url gesture
//! (`Space Alt-z`) and the `ae --web` launcher, so every producer emits exactly what the boot
//! parses. Only the query + fragment live here: the base is the caller's — the CLI knows the
//! server's loopback address, the web shell its own origin (which may be a port-forward the
//! server address would misname).

use aether_protocol::BufferId;

/// What a link opens — mirrors the web shell's `pickerItemUrl` switch.
pub enum WebLinkTarget<'a> {
    /// A file: root index + workspace-relative path, optionally with a 0-based cursor
    /// `(line, col)` rendered as the 1-based `#L:C` fragment the boot jumps to.
    File {
        root: u32,
        path: &'a str,
        at: Option<(u32, u32)>,
    },
    /// A scratch buffer (`?buffer=<id>`). Ids are daemon-session-scoped; the web boot falls
    /// back to the workspace's MRU when the id has gone stale.
    Buffer(BufferId),
    /// Just the workspace — or the chooser, when `workspace` is `None` too.
    Workspace,
}

/// Build the query (+ fragment) for a target: `?workspace=aether&file=src/main.rs#42:10`,
/// `?workspace=aether&buffer=7`, `?workspace=aether`, or `""` for the bare chooser. `root` is
/// omitted when 0 and the fragment is 1-based, both matching the web shell's own links. Append
/// to a base ending in `/` (the served page).
pub fn web_link(workspace: Option<&str>, target: WebLinkTarget) -> String {
    use core::fmt::Write;
    let mut link = String::new();
    let mut sep = '?';
    let mut push = |link: &mut String, key: &str, value: &str| {
        let _ = write!(
            link,
            "{sep}{key}={}",
            percent_encoding::utf8_percent_encode(value, QUERY_VALUE)
        );
        sep = '&';
    };
    if let Some(ws) = workspace {
        push(&mut link, "workspace", ws);
    }
    match target {
        WebLinkTarget::File { root, path, at } => {
            if root != 0 {
                push(&mut link, "root", &root.to_string());
            }
            push(&mut link, "file", path);
            if let Some((line, col)) = at {
                let _ = write!(link, "#{}:{}", line + 1, col + 1);
            }
        }
        WebLinkTarget::Buffer(id) => push(&mut link, "buffer", &id.to_string()),
        WebLinkTarget::Workspace => {}
    }
    link
}

/// The HTTP base for a WebSocket dial address — `ws://…` → `http://…` (and `wss` → `https`):
/// the native shells' prefix for a [`web_link`], valid because the web client is served on the
/// very port they dialed (the server peek-routes HTTP and WS on one listener).
pub fn http_base(server_url: &str) -> String {
    match server_url.strip_prefix("ws") {
        Some(rest) => format!("http{rest}"),
        None => server_url.to_string(),
    }
}

/// Bytes escaped in query-string *values*. Beyond controls: the URL structure characters (`#`
/// ends the query, `&`/`=` delimit pairs, `?` re-opens one), `%` (the escape itself), `+`
/// (decoded as a space by `URLSearchParams`), and space/quotes/angle-brackets (unsafe raw).
const QUERY_VALUE: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'\'')
    .add(b'<')
    .add(b'>')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+')
    .add(b'=')
    .add(b'?');

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_link_matches_the_web_shells_boot_scheme() {
        // Bare chooser: no params at all.
        assert_eq!(web_link(None, WebLinkTarget::Workspace), "");
        assert_eq!(
            web_link(Some("aether"), WebLinkTarget::Workspace),
            "?workspace=aether"
        );
        // File in root 0: `root` omitted, matching the web shell's `fileQuery`; the cursor
        // becomes the 1-based `#L:C` fragment (0-based in).
        assert_eq!(
            web_link(
                Some("aether"),
                WebLinkTarget::File {
                    root: 0,
                    path: "src/main.rs",
                    at: Some((41, 9)),
                }
            ),
            "?workspace=aether&file=src/main.rs#42:10"
        );
        // Non-zero root is carried; no fragment without a cursor.
        assert_eq!(
            web_link(
                Some("aether"),
                WebLinkTarget::File {
                    root: 2,
                    path: "notes.md",
                    at: None,
                }
            ),
            "?workspace=aether&root=2&file=notes.md"
        );
        // Scratch buffers are `?buffer=` links.
        assert_eq!(
            web_link(Some("aether"), WebLinkTarget::Buffer(7)),
            "?workspace=aether&buffer=7"
        );
    }

    #[test]
    fn web_link_escapes_query_values() {
        // `#` would end the query early, `&` would split the pair, `+` would decode as a
        // space, spaces are unsafe raw; `%` itself must escape or a literal `%20` in a name
        // would decode as a space. `URLSearchParams` decodes `%XX` back to the original.
        assert_eq!(
            web_link(
                Some("my workspace"),
                WebLinkTarget::File {
                    root: 0,
                    path: "a&b/#1 + 100%.txt",
                    at: None,
                }
            ),
            "?workspace=my%20workspace&file=a%26b/%231%20%2B%20100%25.txt"
        );
    }

    #[test]
    fn http_base_maps_ws_schemes() {
        assert_eq!(http_base("ws://127.0.0.1:2385"), "http://127.0.0.1:2385");
        assert_eq!(http_base("wss://example:1"), "https://example:1");
    }
}
