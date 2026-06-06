//! Which language server to launch for a given language, and how to find its workspace root.
//!
//! Kept here rather than on `syntax::LanguageConfig` for now: LSP is staged, and a separate table
//! avoids touching the syntax registry until the launch path is wired. If/when this stabilizes it
//! can fold into `LanguageConfig` so language detection and LSP launch share one source of truth
//! (see `docs/lsp.md` §2.3). Keys match `LanguageConfig::name`.

/// How a language's *workspace* root is recognized, for servers that analyze a whole workspace at
/// once. Preferred over the nearest [`LspServerSpec::root_markers`] match so a Cargo workspace (or
/// a `go.work`) resolves to a single server instead of one per crate/module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMarker {
    /// No workspace concept — use the nearest root marker.
    None,
    /// An ancestor directory containing this file is a workspace root (outermost wins), e.g.
    /// `go.work`.
    File(&'static str),
    /// An ancestor whose `file` contains `needle` (on some line) is a workspace root (outermost
    /// wins), e.g. a `Cargo.toml` with a `[workspace]` table.
    FileContaining { file: &'static str, needle: &'static str },
}

/// The workspace-root rule for `language`. Most languages have none (nearest-marker is right);
/// rust-analyzer and gopls analyze a whole workspace, so they resolve to the Cargo `[workspace]` /
/// `go.work` root rather than each crate/module — otherwise a workspace spins up N redundant
/// servers.
pub fn workspace_marker(language: &str) -> WorkspaceMarker {
    match language {
        "rust" => WorkspaceMarker::FileContaining { file: "Cargo.toml", needle: "[workspace]" },
        "go" => WorkspaceMarker::File("go.work"),
        _ => WorkspaceMarker::None,
    }
}

/// How to launch and root a language server.
#[derive(Debug, Clone, Copy)]
pub struct LspServerSpec {
    /// Executable name (resolved on `PATH`).
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Filenames whose nearest ancestor directory is the workspace root. The first marker found
    /// walking up from the file wins; if none is found, the project root is used.
    pub root_markers: &'static [&'static str],
    /// Server-specific `initializationOptions` (raw JSON), sent in the `initialize` handshake.
    /// `None` for servers that need none. Used to opt the vscode JSON/CSS/HTML servers into their
    /// formatter (`{"provideFormatter": true}`), which they otherwise advertise as off.
    pub init_options: Option<&'static str>,
}

/// The configured server for `language` (matching `syntax::LanguageConfig::name`), or `None` if no
/// server is wired for it.
pub fn server_spec(language: &str) -> Option<LspServerSpec> {
    // The vscode JSON/CSS/HTML servers gate their formatter behind this init option; without it
    // they report `documentFormattingProvider: false` and `lsp/format` would say "no formatter".
    const PROVIDE_FORMATTER: Option<&'static str> = Some(r#"{"provideFormatter": true}"#);
    let spec = match language {
        "rust" => LspServerSpec {
            command: "rust-analyzer",
            args: &[],
            root_markers: &["Cargo.toml"],
            init_options: None,
        },
        "toml" => LspServerSpec {
            command: "taplo",
            args: &["lsp", "stdio"],
            // taplo is happy per-file; fall back to the project root when no taplo config exists.
            root_markers: &["taplo.toml", ".taplo.toml"],
            init_options: None,
        },
        "python" => LspServerSpec {
            command: "pyright-langserver",
            args: &["--stdio"],
            root_markers: &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"],
            init_options: None,
        },
        "go" => LspServerSpec {
            command: "gopls",
            args: &[],
            root_markers: &["go.mod", "go.work"],
            init_options: None,
        },
        "typescript" | "javascript" | "tsx" => LspServerSpec {
            command: "typescript-language-server",
            args: &["--stdio"],
            root_markers: &["tsconfig.json", "jsconfig.json", "package.json"],
            init_options: None,
        },
        "json" => LspServerSpec {
            command: "vscode-json-language-server",
            args: &["--stdio"],
            root_markers: &[],
            init_options: PROVIDE_FORMATTER,
        },
        "html" => LspServerSpec {
            command: "vscode-html-language-server",
            args: &["--stdio"],
            root_markers: &[],
            init_options: PROVIDE_FORMATTER,
        },
        "css" => LspServerSpec {
            command: "vscode-css-language-server",
            args: &["--stdio"],
            root_markers: &[],
            init_options: PROVIDE_FORMATTER,
        },
        "yaml" => LspServerSpec {
            command: "yaml-language-server",
            args: &["--stdio"],
            root_markers: &[],
            init_options: None,
        },
        "bash" => LspServerSpec {
            // Diagnostics come from shellcheck, which bash-language-server runs if it's on PATH.
            command: "bash-language-server",
            args: &["start"],
            root_markers: &[],
            init_options: None,
        },
        "markdown" => LspServerSpec {
            command: "marksman",
            args: &["server"],
            root_markers: &[".marksman.toml"],
            init_options: None,
        },
        "elixir" => LspServerSpec {
            command: "elixir-ls",
            args: &[],
            root_markers: &["mix.exs"],
            init_options: None,
        },
        "erlang" => LspServerSpec {
            command: "elp",
            args: &["server"],
            root_markers: &["rebar.config", "rebar.lock"],
            init_options: None,
        },
        _ => return None,
    };
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_languages_have_servers() {
        assert_eq!(server_spec("rust").unwrap().command, "rust-analyzer");
        assert!(server_spec("python").unwrap().args.contains(&"--stdio"));
        assert_eq!(server_spec("go").unwrap().root_markers[0], "go.mod");
        assert_eq!(server_spec("toml").unwrap().command, "taplo");
        assert_eq!(server_spec("toml").unwrap().args, &["lsp", "stdio"]);
        // The gap languages added for broad coverage.
        assert_eq!(server_spec("json").unwrap().command, "vscode-json-language-server");
        // The vscode servers opt into their formatter; others send no init options.
        for lang in ["json", "css", "html"] {
            assert!(
                server_spec(lang).unwrap().init_options.unwrap().contains("provideFormatter"),
                "{lang} should opt into its formatter",
            );
        }
        assert!(server_spec("rust").unwrap().init_options.is_none());
        assert_eq!(server_spec("html").unwrap().command, "vscode-html-language-server");
        assert_eq!(server_spec("css").unwrap().command, "vscode-css-language-server");
        assert_eq!(server_spec("yaml").unwrap().command, "yaml-language-server");
        assert_eq!(server_spec("bash").unwrap().command, "bash-language-server");
        assert_eq!(server_spec("markdown").unwrap().command, "marksman");
        assert_eq!(server_spec("elixir").unwrap().command, "elixir-ls");
        assert_eq!(server_spec("erlang").unwrap().command, "elp");
        // Workspace-aware languages resolve to the workspace root, not per crate/module.
        assert_eq!(
            workspace_marker("rust"),
            WorkspaceMarker::FileContaining { file: "Cargo.toml", needle: "[workspace]" }
        );
        assert_eq!(workspace_marker("go"), WorkspaceMarker::File("go.work"));
        assert_eq!(workspace_marker("python"), WorkspaceMarker::None);
        // TS/JS/TSX share one server.
        for lang in ["typescript", "javascript", "tsx"] {
            assert_eq!(
                server_spec(lang).unwrap().command,
                "typescript-language-server"
            );
        }
    }

    #[test]
    fn unknown_language_has_no_server() {
        assert!(server_spec("brainfuck").is_none());
    }
}
