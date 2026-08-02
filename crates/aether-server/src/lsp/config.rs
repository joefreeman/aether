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
    FileContaining {
        file: &'static str,
        needle: &'static str,
    },
}

/// The workspace-root rule for `language`. Most languages have none (nearest-marker is right);
/// rust-analyzer and gopls analyze a whole workspace, so they resolve to the Cargo `[workspace]` /
/// `go.work` root rather than each crate/module — otherwise a workspace spins up N redundant
/// servers.
pub fn workspace_marker(language: &str) -> WorkspaceMarker {
    match language {
        "rust" => WorkspaceMarker::FileContaining {
            file: "Cargo.toml",
            needle: "[workspace]",
        },
        "go" => WorkspaceMarker::File("go.work"),
        _ => WorkspaceMarker::None,
    }
}

/// Every language [`server_spec`] answers for. Kept in sync with its match arms by
/// `all_languages_have_specs`; used by the marker-table drift test and by project resolution.
pub const SERVER_LANGUAGES: &[&str] = &[
    "rust",
    "toml",
    "python",
    "go",
    "typescript",
    "javascript",
    "tsx",
    "json",
    "html",
    "css",
    "yaml",
    "bash",
    "markdown",
    "elixir",
    "erlang",
    "quiver",
    "sql",
    "terraform",
    "dockerfile",
];

/// The language a server is *keyed* under, collapsing languages that share one [`LspServerSpec`].
///
/// `typescript`, `javascript` and `tsx` are one `typescript-language-server` invocation with one set
/// of root markers, but [`super::manager::LspServerKey`] carries a language — so without this a
/// `.ts` and a `.js` buffer in the same root would key differently and spawn two identical servers,
/// neither reusing the other's document sync. Every key construction and every key *lookup* must
/// canonicalize, or they'll miss each other.
///
/// Identity for everything else; a language with no server is returned unchanged (it never reaches
/// a key).
pub fn canonical_language(language: &str) -> &str {
    match language {
        "javascript" | "tsx" => "typescript",
        other => other,
    }
}

/// The language a project marker file identifies — the reverse of [`LspServerSpec::root_markers`],
/// matched on the marker's *file name*.
///
/// Used to resolve a declared project (`docs/projects.md`) to the server it should pin: the path's
/// parent is the root, and its file name selects the server. Markers shared by several languages
/// resolve to the [`canonical_language`] of the group (`package.json` → `typescript`), which is the
/// key those servers all live under anyway.
///
/// Two entries deserve a note. `config.yml` is a generic name that means `sqls` only because a user
/// deliberately declared it — this is exactly why projects are declared rather than discovered by
/// scanning. `.terraform` is a directory, not a file, but it's a marker like any other and
/// `discover_root` already treats it that way.
///
/// `None` for a name no language claims — including the languages with no markers at all (json,
/// html, css, yaml, bash, quiver, dockerfile), which can't be expressed as a project and don't need
/// to be: none of them has meaningful workspace symbols.
pub fn language_for_marker(file_name: &str) -> Option<&'static str> {
    Some(match file_name {
        "Cargo.toml" => "rust",
        "taplo.toml" | ".taplo.toml" => "toml",
        "pyproject.toml" | "pyworkspace.toml" | "setup.py" | "setup.cfg" | "requirements.txt" => {
            "python"
        }
        "go.mod" | "go.work" => "go",
        "tsconfig.json" | "jsconfig.json" | "package.json" => "typescript",
        ".marksman.toml" => "markdown",
        "mix.exs" => "elixir",
        "rebar.config" | "rebar.lock" => "erlang",
        "config.yml" => "sql",
        ".terraform.lock.hcl" | ".terraform" => "terraform",
        _ => return None,
    })
}

/// How to launch and root a language server.
#[derive(Debug, Clone, Copy)]
pub struct LspServerSpec {
    /// Executable name (resolved on `PATH`).
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Filenames whose nearest ancestor directory is the workspace root. The first marker found
    /// walking up from the file wins; if none is found, the workspace root is used.
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
            // taplo is happy per-file; fall back to the workspace root when no taplo config exists.
            root_markers: &["taplo.toml", ".taplo.toml"],
            init_options: None,
        },
        "python" => LspServerSpec {
            command: "pyright-langserver",
            args: &["--stdio"],
            root_markers: &[
                "pyproject.toml",
                "pyworkspace.toml",
                "setup.py",
                "setup.cfg",
                "requirements.txt",
            ],
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
        "quiver" => LspServerSpec {
            // `quiver-lsp` speaks LSP over stdio with no arguments. Bundles the standard
            // library, so no project marker is needed yet — fall back to the project root.
            command: "quiver-lsp",
            args: &[],
            root_markers: &[],
            init_options: None,
        },
        "sql" => LspServerSpec {
            // `sqls` speaks LSP over stdio by default. Its DB-aware features need a `config.yml`,
            // but it initializes fine without one (keyword completion / formatting) — fall back to
            // the project root when no config is present.
            command: "sqls",
            args: &[],
            root_markers: &["config.yml", ".sqls/config.yml"],
            init_options: None,
        },
        "terraform" => LspServerSpec {
            command: "terraform-ls",
            args: &["serve"],
            root_markers: &[".terraform.lock.hcl", ".terraform"],
            init_options: None,
        },
        "dockerfile" => LspServerSpec {
            // `docker-langserver` ships in the `dockerfile-language-server-nodejs` npm package. It
            // analyses a single file (lint + completion + hover), so there's no project marker to
            // look for — fall back to the workspace root.
            command: "docker-langserver",
            args: &["--stdio"],
            root_markers: &[],
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
        assert_eq!(
            server_spec("json").unwrap().command,
            "vscode-json-language-server"
        );
        // The vscode servers opt into their formatter; others send no init options.
        for lang in ["json", "css", "html"] {
            assert!(
                server_spec(lang)
                    .unwrap()
                    .init_options
                    .unwrap()
                    .contains("provideFormatter"),
                "{lang} should opt into its formatter",
            );
        }
        assert!(server_spec("rust").unwrap().init_options.is_none());
        assert_eq!(
            server_spec("html").unwrap().command,
            "vscode-html-language-server"
        );
        assert_eq!(
            server_spec("css").unwrap().command,
            "vscode-css-language-server"
        );
        assert_eq!(server_spec("yaml").unwrap().command, "yaml-language-server");
        assert_eq!(server_spec("bash").unwrap().command, "bash-language-server");
        assert_eq!(server_spec("markdown").unwrap().command, "marksman");
        assert_eq!(server_spec("elixir").unwrap().command, "elixir-ls");
        assert_eq!(server_spec("erlang").unwrap().command, "elp");
        assert_eq!(server_spec("quiver").unwrap().command, "quiver-lsp");
        assert_eq!(server_spec("sql").unwrap().command, "sqls");
        assert_eq!(server_spec("terraform").unwrap().command, "terraform-ls");
        assert_eq!(server_spec("terraform").unwrap().args, &["serve"]);
        assert_eq!(
            server_spec("dockerfile").unwrap().command,
            "docker-langserver"
        );
        // Workspace-aware languages resolve to the workspace root, not per crate/module.
        assert_eq!(
            workspace_marker("rust"),
            WorkspaceMarker::FileContaining {
                file: "Cargo.toml",
                needle: "[workspace]"
            }
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

    #[test]
    fn all_languages_have_specs() {
        for lang in SERVER_LANGUAGES {
            assert!(
                server_spec(lang).is_some(),
                "{lang} is listed in SERVER_LANGUAGES but has no spec",
            );
        }
    }

    /// Languages sharing one spec collapse to a single key; everything else is identity.
    #[test]
    fn shared_specs_canonicalize_to_one_language() {
        for lang in ["typescript", "javascript", "tsx"] {
            assert_eq!(canonical_language(lang), "typescript");
        }
        for lang in ["rust", "go", "python", "elixir"] {
            assert_eq!(canonical_language(lang), lang);
        }
        // A language with no server at all still passes through untouched.
        assert_eq!(canonical_language("brainfuck"), "brainfuck");
    }

    /// The marker table is the reverse of `root_markers`; this catches the two drifting apart when
    /// a marker is added to a spec but not to [`language_for_marker`].
    #[test]
    fn every_root_marker_reverses_to_its_language() {
        for lang in SERVER_LANGUAGES {
            for marker in server_spec(lang).unwrap().root_markers {
                // Markers can be relative paths (`.sqls/config.yml`); the reverse map keys on the
                // file name, which is what a declared project path also yields.
                let file_name = marker.rsplit('/').next().unwrap();
                let found = language_for_marker(file_name).unwrap_or_else(|| {
                    panic!("marker {marker} of {lang} has no language_for_marker entry")
                });
                assert_eq!(
                    canonical_language(found),
                    canonical_language(lang),
                    "marker {marker} of {lang} reverses to {found}",
                );
            }
        }
    }

    #[test]
    fn markerless_languages_are_not_projects() {
        // These have no root markers, so nothing can declare them as a project.
        for lang in [
            "json",
            "html",
            "css",
            "yaml",
            "bash",
            "quiver",
            "dockerfile",
        ] {
            assert!(server_spec(lang).unwrap().root_markers.is_empty());
        }
        assert!(language_for_marker("some-random-file.txt").is_none());
    }
}
