//! Checking a parsed line against the world before anything runs, and planning what will.
//!
//! Every word is expanded here — variables, `~`, globs, `%` — because expansion is where most of
//! the questions live: is that variable set, does that glob match, is that a directory. What comes
//! out is an [`Accepted`] the caller can act on — a directory change, an assignment, or a
//! [`Plan`] of stages with their argv already resolved — or a [`Refusal`] naming the word at
//! fault. Validation and execution read the same expansion, so what was checked is what runs.

use crate::glob::{self, PatChar};
use crate::lex::{Part, Span, Word};
use crate::parse::{Command, ListOp, Program, RedirectKind};
use crate::world::{normalize, PathKind, World};
use crate::Refusal;
use std::path::{Path, PathBuf};

/// Commands the shell answers itself, never looked up on `PATH`.
pub const BUILTINS: &[&str] = &["pwd", "type"];

/// What the validator needs of the shell's state.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub cwd: &'a Path,
    /// Where `-` goes.
    pub prev_cwd: Option<&'a Path>,
    /// What `%` names: the file most recently looked at, when there is one.
    pub current_file: Option<&'a Path>,
}

/// A line the shell will act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// Move the shell to this directory (absolute, lexically normalised).
    ChangeDir(PathBuf),
    /// Set these variables for the runs that follow.
    Assign(Vec<(String, String)>),
    /// Run these.
    Run(Plan),
}

/// What an accepted line runs: pipelines in order, each joined to the previous by its `op`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub items: Vec<PlannedItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedItem {
    pub op: Option<ListOp>,
    /// The pipeline's stages, first to last.
    pub stages: Vec<Stage>,
}

/// One command of a pipeline, resolved: what to execute, with what, and where its streams go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    pub exec: Exec,
    /// The full argument vector, `argv[0]` included — the name as typed and expanded.
    pub argv: Vec<String>,
    /// Prefix assignments, applied to this stage's environment only.
    pub env: Vec<(String, String)>,
    pub redirects: Vec<PlannedRedirect>,
    /// The command as typed, for a message.
    pub span: Span,
}

/// What a stage executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exec {
    Builtin(Builtin),
    /// An executable, found on `PATH` or named by a path — already checked to exist.
    Path(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Pwd,
    Type,
}

impl Builtin {
    pub fn named(name: &str) -> Option<Builtin> {
        match name {
            "pwd" => Some(Builtin::Pwd),
            "type" => Some(Builtin::Type),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Builtin::Pwd => "pwd",
            Builtin::Type => "type",
        }
    }
}

/// A redirection with its target resolved against the shell's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRedirect {
    pub kind: RedirectKind,
    pub path: PathBuf,
}

/// Validate `program` against the world, and plan it.
pub fn validate(
    program: &Program,
    ctx: Context<'_>,
    world: &impl World,
) -> Result<Accepted, Refusal> {
    let items = &program.items;
    if items.is_empty() {
        return Err(Refusal::new("nothing to run", Span::new(0, 0)));
    }
    let alone = items.len() == 1 && items[0].pipeline.commands.len() == 1;
    let mut planned = Vec::with_capacity(items.len());
    for item in items {
        let mut stages = Vec::with_capacity(item.pipeline.commands.len());
        for cmd in &item.pipeline.commands {
            match classify(cmd, ctx, world)? {
                Kind::Stage(stage) => stages.push(stage),
                Kind::ChangeDir(dir) if alone => return Ok(Accepted::ChangeDir(dir)),
                Kind::ChangeDir(_) => {
                    return Err(Refusal::new(
                        "a directory change must be a line of its own",
                        cmd.span,
                    ))
                }
                Kind::Assign(vars) if alone => return Ok(Accepted::Assign(vars)),
                Kind::Assign(_) => {
                    return Err(Refusal::new(
                        "an assignment must be a line of its own",
                        cmd.span,
                    ))
                }
            }
        }
        // A builtin answers from the shell itself, with nothing to pipe through and nowhere to
        // redirect from: on its own, or not at all.
        if let Some(b) = stages.iter().find(|s| matches!(s.exec, Exec::Builtin(_))) {
            let Exec::Builtin(builtin) = b.exec else {
                unreachable!()
            };
            if stages.len() > 1 {
                return Err(Refusal::new(
                    format!("`{}` can't be piped", builtin.name()),
                    b.span,
                ));
            }
            if !b.redirects.is_empty() {
                return Err(Refusal::new(
                    format!("`{}` takes no redirections", builtin.name()),
                    b.span,
                ));
            }
        }
        planned.push(PlannedItem {
            op: item.op,
            stages,
        });
    }
    Ok(Accepted::Run(Plan { items: planned }))
}

enum Kind {
    Stage(Stage),
    ChangeDir(PathBuf),
    Assign(Vec<(String, String)>),
}

fn classify(cmd: &Command, ctx: Context<'_>, world: &impl World) -> Result<Kind, Refusal> {
    let mut assigned = Vec::new();
    for a in &cmd.assignments {
        assigned.push((a.name.clone(), expand_one(&a.value, ctx, world)?));
    }
    if cmd.words.is_empty() {
        return Ok(Kind::Assign(assigned));
    }
    // Every word, so that an unset variable or an empty glob anywhere on the line is caught.
    let mut argv: Vec<String> = Vec::new();
    let mut expanded = Vec::with_capacity(cmd.words.len());
    for w in &cmd.words {
        let words = expand(w, ctx, world, true)?;
        argv.extend(words.iter().cloned());
        expanded.push(words);
    }
    let mut redirects = Vec::with_capacity(cmd.redirects.len());
    for r in &cmd.redirects {
        let target = expand_one(&r.target, ctx, world)?;
        let path = normalize(&ctx.cwd.join(&target));
        if r.kind == RedirectKind::In
            && !matches!(world.path_kind(&path), Some(PathKind::File { .. }))
        {
            return Err(Refusal::new(
                format!("no such file `{target}`"),
                r.target.span,
            ));
        }
        redirects.push(PlannedRedirect { kind: r.kind, path });
    }

    let first = &cmd.words[0];
    let names = &expanded[0];
    if names.len() != 1 {
        return Err(Refusal::new(
            format!("`{}` matches {} things", first.source_hint(), names.len()),
            first.span,
        ));
    }
    let name = &names[0];
    let stage = |exec: Exec| {
        Kind::Stage(Stage {
            exec,
            argv: argv.clone(),
            env: assigned.clone(),
            redirects: redirects.clone(),
            span: cmd.span,
        })
    };

    if is_path_shaped(first) {
        let dir = if first.is_bare("-") {
            match ctx.prev_cwd {
                Some(p) => p.to_path_buf(),
                None => return Err(Refusal::new("no previous directory", first.span)),
            }
        } else {
            normalize(&ctx.cwd.join(name))
        };
        return match world.path_kind(&dir) {
            Some(PathKind::Dir) => {
                if cmd.words.len() > 1 {
                    Err(Refusal::new(
                        format!("`{name}` is a directory — a directory change takes no arguments"),
                        first.span,
                    ))
                } else if !cmd.assignments.is_empty() {
                    Err(Refusal::new(
                        "a directory change takes no assignments",
                        cmd.assignments[0].span,
                    ))
                } else if !cmd.redirects.is_empty() {
                    Err(Refusal::new(
                        "a directory change takes no redirections",
                        cmd.redirects[0].span,
                    ))
                } else {
                    Ok(Kind::ChangeDir(dir))
                }
            }
            Some(PathKind::File { executable: true }) => Ok(stage(Exec::Path(dir))),
            Some(PathKind::File { executable: false }) => Err(Refusal::new(
                format!("`{name}` is not executable"),
                first.span,
            )),
            Some(PathKind::Other) | None => Err(Refusal::new(
                format!("no such file or directory `{name}`"),
                first.span,
            )),
        };
    }

    if let Some(builtin) = Builtin::named(name) {
        return Ok(stage(Exec::Builtin(builtin)));
    }
    if let Some(path) = find_executable(name, world) {
        return Ok(stage(Exec::Path(path)));
    }
    let mut message = format!("unknown command `{name}`");
    if cmd.words.get(1).is_some_and(|w| w.is_bare("=")) {
        message.push_str(&format!(" — to set a variable, write `{name}=…`"));
    } else if name.contains('/') {
        message.push_str(" — a path starts with `./`, `../`, `/` or `~`");
    }
    Err(Refusal::new(message, first.span))
}

/// `./x`, `../x`, `/x`, `~`, `~/x`, and exactly `.`, `..` or `-`: words that name a place.
fn is_path_shaped(w: &Word) -> bool {
    if w.is_bare(".") || w.is_bare("..") || w.is_bare("-") {
        return true;
    }
    match w.leading_bare() {
        Some(s) => {
            s.starts_with("./") || s.starts_with("../") || s.starts_with('/') || s.starts_with('~')
        }
        None => false,
    }
}

/// Find `name` on the world's `PATH`.
pub fn find_executable(name: &str, world: &impl World) -> Option<PathBuf> {
    if name.contains('/') {
        return None;
    }
    let path = world.variable("PATH")?;
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| {
            matches!(
                world.path_kind(candidate),
                Some(PathKind::File { executable: true })
            )
        })
}

/// Expand a word that must come out as exactly one string: no globbing.
fn expand_one(w: &Word, ctx: Context<'_>, world: &impl World) -> Result<String, Refusal> {
    let mut v = expand(w, ctx, world, false)?;
    Ok(v.pop().unwrap_or_default())
}

/// Expand a word: `~`, variables, `%`, and — when `globbing` — globs against the directory.
fn expand(
    w: &Word,
    ctx: Context<'_>,
    world: &impl World,
    globbing: bool,
) -> Result<Vec<String>, Refusal> {
    if w.is_bare("%") {
        return match ctx.current_file {
            Some(file) => Ok(vec![file.to_string_lossy().into_owned()]),
            None => Err(Refusal::new("`%` has no file to point at", w.span)),
        };
    }
    let mut pat: Vec<PatChar> = Vec::new();
    for (i, part) in w.parts.iter().enumerate() {
        match part {
            Part::Bare(text) => {
                let mut text = text.as_str();
                if i == 0 && text.starts_with('~') {
                    let rest = &text[1..];
                    if !(rest.is_empty() || rest.starts_with('/')) {
                        return Err(Refusal::new("`~user` isn't supported", w.span));
                    }
                    let home = world
                        .home()
                        .ok_or_else(|| Refusal::new("`~` needs `$HOME` to be set", w.span))?;
                    pat.extend(
                        home.to_string_lossy()
                            .chars()
                            .map(|c| PatChar { c, live: false }),
                    );
                    text = rest;
                }
                pat.extend(text.chars().map(|c| PatChar { c, live: true }));
            }
            Part::Quoted(text) => pat.extend(text.chars().map(|c| PatChar { c, live: false })),
            Part::Var { name, span } => {
                let value = world
                    .variable(name)
                    .ok_or_else(|| Refusal::new(format!("`${name}` is not set"), *span))?;
                pat.extend(value.chars().map(|c| PatChar { c, live: false }));
            }
        }
    }
    if globbing && glob::has_glob(&pat) {
        let matches = glob::expand(&pat, ctx.cwd, world);
        if matches.is_empty() {
            return Err(Refusal::new(
                format!("nothing matches `{}`", w.source_hint()),
                w.span,
            ));
        }
        return Ok(matches
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect());
    }
    Ok(vec![pat.iter().map(|p| p.c).collect()])
}

impl Word {
    /// The word as typed, for a message — without the source, which the validator never sees.
    pub fn source_hint(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            match p {
                Part::Bare(s) => out.push_str(s),
                Part::Quoted(s) => {
                    out.push('"');
                    out.push_str(s);
                    out.push('"');
                }
                Part::Var { name, .. } => {
                    out.push('$');
                    out.push_str(name);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glob::tests::Fake;
    use crate::parse::parse;

    const FILE: PathKind = PathKind::File { executable: false };
    const EXE: PathKind = PathKind::File { executable: true };

    fn world() -> Fake {
        let mut w = Fake::new(&[
            ("/usr/bin/ls", EXE),
            ("/usr/bin/cargo", EXE),
            ("/usr/bin/sh", EXE),
            ("/opt/tools/rg", EXE),
            ("/home/joe/proj/build.sh", EXE),
            ("/home/joe/proj/notes.txt", FILE),
            ("/home/joe/proj/src/main.rs", FILE),
            ("/home/joe/proj/src/lib.rs", FILE),
            ("/home/joe/proj/target/debug", PathKind::Dir),
            ("/home/joe/other", PathKind::Dir),
        ]);
        w.vars.insert("PATH".into(), "/usr/bin:/opt/tools".into());
        w.vars.insert("HOME".into(), "/home/joe".into());
        w.vars.insert("NAME".into(), "world".into());
        w.vars.insert("EMPTY".into(), String::new());
        w
    }

    fn check(src: &str) -> Result<Accepted, Refusal> {
        check_from(src, "/home/joe/proj", Some("/home/joe/other"), None)
    }

    fn check_from(
        src: &str,
        cwd: &str,
        prev: Option<&str>,
        current_file: Option<&str>,
    ) -> Result<Accepted, Refusal> {
        let program = parse(src)?;
        validate(
            &program,
            Context {
                cwd: Path::new(cwd),
                prev_cwd: prev.map(Path::new),
                current_file: current_file.map(Path::new),
            },
            &world(),
        )
    }

    fn refused(src: &str) -> Refusal {
        check(src).expect_err(src)
    }

    fn plan(src: &str) -> Plan {
        match check(src) {
            Ok(Accepted::Run(plan)) => plan,
            other => panic!("{src}: {other:?}"),
        }
    }

    fn argv(src: &str) -> Vec<Vec<String>> {
        plan(src)
            .items
            .into_iter()
            .flat_map(|i| i.stages.into_iter().map(|s| s.argv))
            .collect()
    }

    #[test]
    fn commands_are_found_on_path_or_are_builtins_or_are_executable_files() {
        let one = plan("ls -la");
        assert_eq!(
            one.items[0].stages[0].exec,
            Exec::Path("/usr/bin/ls".into())
        );
        assert_eq!(one.items[0].stages[0].argv, vec!["ls", "-la"]);
        let piped = plan("rg foo | ls");
        assert_eq!(piped.items[0].stages.len(), 2);
        assert_eq!(
            piped.items[0].stages[0].exec,
            Exec::Path("/opt/tools/rg".into())
        );
        let list = plan("pwd && type ls");
        assert_eq!(list.items[0].stages[0].exec, Exec::Builtin(Builtin::Pwd));
        assert_eq!(list.items[1].op, Some(ListOp::And));
        assert_eq!(list.items[1].stages[0].exec, Exec::Builtin(Builtin::Type));
        assert_eq!(
            plan("./build.sh --release").items[0].stages[0].exec,
            Exec::Path("/home/joe/proj/build.sh".into())
        );
        assert_eq!(
            plan("/usr/bin/ls").items[0].stages[0].exec,
            Exec::Path("/usr/bin/ls".into())
        );
        assert_eq!(
            plan("~/proj/build.sh").items[0].stages[0].exec,
            Exec::Path("/home/joe/proj/build.sh".into())
        );
    }

    #[test]
    fn a_stage_carries_its_prefix_environment_and_redirections() {
        let p = plan("RUST_LOG=debug cargo test > out.txt >> log.txt < notes.txt");
        let s = &p.items[0].stages[0];
        assert_eq!(s.env, vec![("RUST_LOG".to_string(), "debug".to_string())]);
        assert_eq!(s.argv, vec!["cargo", "test"]);
        let redirects: Vec<_> = s
            .redirects
            .iter()
            .map(|r| (r.kind, r.path.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            redirects,
            vec![
                (RedirectKind::Out, "/home/joe/proj/out.txt".to_string()),
                (RedirectKind::Append, "/home/joe/proj/log.txt".to_string()),
                (RedirectKind::In, "/home/joe/proj/notes.txt".to_string()),
            ]
        );
    }

    #[test]
    fn a_builtin_stands_alone() {
        assert_eq!(refused("pwd | ls").message, "`pwd` can't be piped");
        assert_eq!(
            refused("pwd > here.txt").message,
            "`pwd` takes no redirections"
        );
    }

    #[test]
    fn an_unknown_command_is_refused_at_its_word() {
        let r = refused("frobnicate --now");
        assert_eq!(r.message, "unknown command `frobnicate`");
        assert_eq!(r.span, Span::new(0, 10));
        // Wherever it sits.
        let r = refused("ls | frobnicate");
        assert_eq!(r.span, Span::new(5, 15));
        let r = refused("./notes.txt");
        assert_eq!(r.message, "`./notes.txt` is not executable");
        let r = refused("./missing");
        assert_eq!(r.message, "no such file or directory `./missing`");
        let r = refused("src/main.rs");
        assert!(r.message.contains("a path starts with"), "{}", r.message);
    }

    #[test]
    fn a_misspelt_assignment_gets_a_hint() {
        let r = refused("FOO = bar");
        assert_eq!(
            r.message,
            "unknown command `FOO` — to set a variable, write `FOO=…`"
        );
        assert_eq!(r.span, Span::new(0, 3));
    }

    #[test]
    fn a_lone_path_shaped_word_naming_a_directory_changes_directory() {
        assert_eq!(
            check("./src").unwrap(),
            Accepted::ChangeDir("/home/joe/proj/src".into())
        );
        assert_eq!(
            check("..").unwrap(),
            Accepted::ChangeDir("/home/joe".into())
        );
        assert_eq!(
            check("../other").unwrap(),
            Accepted::ChangeDir("/home/joe/other".into())
        );
        assert_eq!(check("~").unwrap(), Accepted::ChangeDir("/home/joe".into()));
        assert_eq!(
            check("/home/joe/proj/target/debug").unwrap(),
            Accepted::ChangeDir("/home/joe/proj/target/debug".into())
        );
        assert_eq!(
            check("-").unwrap(),
            Accepted::ChangeDir("/home/joe/other".into())
        );
        assert_eq!(
            check("./s*").unwrap(),
            Accepted::ChangeDir("/home/joe/proj/src".into()),
            "a glob that names exactly one directory"
        );
        assert_eq!(
            check(".").unwrap(),
            Accepted::ChangeDir("/home/joe/proj".into())
        );
    }

    #[test]
    fn a_directory_change_is_refused_when_it_cannot_be_one() {
        assert_eq!(
            refused("./src cargo build").message,
            "`./src` is a directory — a directory change takes no arguments"
        );
        assert_eq!(
            refused("./nowhere").message,
            "no such file or directory `./nowhere`"
        );
        assert_eq!(
            check_from("-", "/home/joe/proj", None, None)
                .unwrap_err()
                .message,
            "no previous directory"
        );
        assert_eq!(
            refused("./src && ls").message,
            "a directory change must be a line of its own"
        );
        assert_eq!(
            refused("./src | ls").message,
            "a directory change must be a line of its own"
        );
        assert_eq!(refused("~joe").message, "`~user` isn't supported");
    }

    #[test]
    fn assignments_alone_are_state_and_with_a_command_are_arguments() {
        assert_eq!(
            check("FOO=bar BAZ=\"two words\"").unwrap(),
            Accepted::Assign(vec![
                ("FOO".into(), "bar".into()),
                ("BAZ".into(), "two words".into())
            ])
        );
        assert_eq!(
            check("GREETING=\"hello $NAME\"").unwrap(),
            Accepted::Assign(vec![("GREETING".into(), "hello world".into())])
        );
        assert_eq!(
            check("DIR=~/proj").unwrap(),
            Accepted::Assign(vec![("DIR".into(), "/home/joe/proj".into())])
        );
        assert_eq!(
            check("GLOB=*.rs").unwrap(),
            Accepted::Assign(vec![("GLOB".into(), "*.rs".into())]),
            "no globbing in a value"
        );
        assert!(matches!(
            check("RUST_LOG=debug cargo test").unwrap(),
            Accepted::Run(_)
        ));
        assert_eq!(
            refused("FOO=bar; ls").message,
            "an assignment must be a line of its own"
        );
    }

    #[test]
    fn variables_must_be_set_and_never_split() {
        let r = refused("ls $NOPE");
        assert_eq!(r.message, "`$NOPE` is not set");
        assert_eq!(r.span, Span::new(3, 8), "the reference, not the whole word");
        assert_eq!(
            argv("ls $EMPTY"),
            vec![vec!["ls", ""]],
            "one empty argument"
        );
        assert_eq!(
            argv("ls \"$NAME\" ${NAME}x"),
            vec![vec!["ls", "world", "worldx"]]
        );
        let mut w = world();
        w.vars.insert("TWO".into(), "a b".into());
        let program = parse("ls $TWO").unwrap();
        let ctx = Context {
            cwd: Path::new("/home/joe/proj"),
            prev_cwd: None,
            current_file: None,
        };
        let Accepted::Run(p) = validate(&program, ctx, &w).unwrap() else {
            panic!()
        };
        assert_eq!(p.items[0].stages[0].argv, vec!["ls", "a b"], "never split");
    }

    #[test]
    fn globs_must_match_and_quoted_ones_are_literal() {
        assert_eq!(
            argv("ls src/*.rs"),
            vec![vec!["ls", "src/lib.rs", "src/main.rs"]]
        );
        let r = refused("ls *.md");
        assert_eq!(r.message, "nothing matches `*.md`");
        assert_eq!(r.span, Span::new(3, 7));
        // A quoted asterisk is an argument, not a pattern, so nothing is checked.
        assert_eq!(argv("ls \"*.md\""), vec![vec!["ls", "*.md"]]);
        assert_eq!(
            argv("ls **/*.rs"),
            vec![vec!["ls", "src/lib.rs", "src/main.rs"]]
        );
    }

    #[test]
    fn input_redirections_need_their_file() {
        assert!(matches!(check("ls < notes.txt").unwrap(), Accepted::Run(_)));
        assert!(matches!(
            check("ls > out.txt >> log.txt").unwrap(),
            Accepted::Run(_)
        ));
        let r = refused("ls < nope.txt");
        assert_eq!(r.message, "no such file `nope.txt`");
        assert_eq!(r.span, Span::new(5, 13));
    }

    #[test]
    fn the_empty_line() {
        assert_eq!(refused("").message, "nothing to run");
        assert_eq!(refused("# nothing").message, "nothing to run");
    }

    /// The real shell is one command away: `sh -c "…"`, with `\$` for a variable meant for it.
    #[test]
    fn the_users_shell_is_just_a_command() {
        let p = plan(r#"sh -c "for f in *; do echo \$f; done >&2""#);
        assert_eq!(
            p.items[0].stages[0].argv,
            vec!["sh", "-c", "for f in *; do echo $f; done >&2"]
        );
        // Without the backslash the variable is ours, and it is not set.
        assert_eq!(refused(r#"sh -c "echo $f""#).message, "`$f` is not set");
    }

    #[test]
    fn the_current_file_word_names_the_file_being_looked_at() {
        let accepted = check_from(
            "cargo test %",
            "/home/joe/proj",
            None,
            Some("/home/joe/proj/src/lib.rs"),
        )
        .unwrap();
        let Accepted::Run(p) = accepted else { panic!() };
        assert_eq!(
            p.items[0].stages[0].argv,
            vec!["cargo", "test", "/home/joe/proj/src/lib.rs"]
        );
        assert_eq!(
            refused("cargo test %").message,
            "`%` has no file to point at"
        );
    }
}
