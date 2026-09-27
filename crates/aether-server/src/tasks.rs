//! Tasks: the named commands a project's runner files define — justfile recipes, Makefile
//! targets, `package.json` scripts and mise tasks — found so the tasks picker can offer them.
//!
//! A task is a shortcut for starting a shell. What this module produces is a *command line* and a
//! directory to run it in; the shell view does everything after that, so nothing here runs a task.
//!
//! Three of the four formats are read directly. They are small, regular files, reading them runs
//! nothing, and reading them ourselves is what gives each row the line its definition sits on.
//! mise is *asked* instead (`mise tasks ls --json`): its task list is a merge of every ancestor's
//! config, file tasks in several directories and the user's global config, and mise is the only
//! thing that knows that merge. Asking it runs a program, so the caller only does it for a
//! workspace the user configured — the rule that keeps language servers out of a temporary one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One task, ready to be a picker row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    /// The line a shell opened for this task runs — the runner and the name, in the editor's own
    /// shell language.
    pub command: String,
    /// Where the command runs: the directory the runner would look for the task from.
    pub dir: PathBuf,
    /// The file that defines it.
    pub path: PathBuf,
    /// 0-based line of the definition in `path`.
    pub line: u32,
    pub description: String,
}

/// A definition found in a file, before it is given a command and a place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Found {
    name: String,
    line: u32,
    description: String,
}

/// The formats read directly, in the order a directory's tasks are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Just,
    Make,
    Package,
}

/// How long mise gets to list its tasks before the picker opens without them.
const MISE_TIMEOUT: Duration = Duration::from_secs(5);

// ---- where the tasks are -----------------------------------------------------------------------

/// The runner files in `dir` this module reads directly, each once, in listing order.
///
/// One of each runner at most, chosen as the runner would choose: `just` takes any casing of
/// `justfile` (or `.justfile`), and `make` the first of `GNUmakefile`, `makefile`, `Makefile`.
fn runner_files(dir: &Path) -> Vec<(Format, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    let mut out = Vec::new();
    let mut just: Vec<&String> = names
        .iter()
        .filter(|n| matches!(n.to_ascii_lowercase().as_str(), "justfile" | ".justfile"))
        .collect();
    // Deterministic when a directory holds more than one spelling, which `just` itself refuses.
    just.sort();
    if let Some(name) = just.first() {
        out.push((Format::Just, dir.join(name)));
    }
    if let Some(name) = ["GNUmakefile", "makefile", "Makefile"]
        .into_iter()
        .find(|m| names.iter().any(|n| n == m))
    {
        out.push((Format::Make, dir.join(name)));
    }
    if names.iter().any(|n| n == "package.json") {
        out.push((Format::Package, dir.join("package.json")));
    }
    out
}

/// Whether a file of this name is one [`runner_files`] would pick up.
fn is_runner_file(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "justfile" | ".justfile")
        || matches!(
            name,
            "GNUmakefile" | "makefile" | "Makefile" | "package.json"
        )
}

/// Whether a workspace file is mise configuration, or one of mise's file tasks — anything whose
/// directory is worth asking mise about.
fn is_mise_file(relative_path: &str) -> bool {
    let name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let config =
        (name.starts_with("mise.") || name.starts_with(".mise.")) && name.ends_with(".toml");
    config
        || relative_path
            .split('/')
            .any(|c| matches!(c, ".mise" | "mise-tasks" | ".mise-tasks"))
        || relative_path.contains(".config/mise")
}

/// Whether `dir` holds mise configuration or file tasks of its own — the directories worth asking
/// mise about. Without one anywhere in reach, mise would answer only with the user's global tasks,
/// which are not this project's and would otherwise be in every list.
fn has_mise_config(dir: &Path) -> bool {
    [
        "mise.toml",
        ".mise.toml",
        "mise.local.toml",
        ".mise.local.toml",
        ".mise",
        "mise",
        ".config/mise.toml",
        ".config/mise",
        "mise-tasks",
        ".mise-tasks",
    ]
    .iter()
    .any(|name| dir.join(name).exists())
}

/// The environment mise runs in: the one a shell in `root` would get, so the `mise` on its `PATH`
/// is the user's.
async fn mise_environment(root: &Path) -> HashMap<String, String> {
    crate::shell::environment(root).await
}

/// The tasks of the runner files in `dir`, in listing order. `stop` bounds the upward search for
/// a `package.json`'s lockfile.
fn dir_tasks(dir: &Path, stop: &Path) -> Vec<Task> {
    let mut out = Vec::new();
    for (format, path) in runner_files(dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (runner, found) = match format {
            Format::Just => ("just".to_string(), parse_justfile(&text)),
            Format::Make => ("make".to_string(), parse_makefile(&text)),
            Format::Package => (
                format!("{} run", package_runner(dir, stop)),
                parse_package_json(&text),
            ),
        };
        out.extend(found.into_iter().map(|f| Task {
            command: format!("{runner} {}", aether_shell::quote(&f.name)),
            name: f.name,
            dir: dir.to_path_buf(),
            path: path.clone(),
            line: f.line,
            description: f.description,
        }));
    }
    out
}

/// The package manager a `package.json` in `dir` belongs to, by the lockfile nearest it — up to
/// `stop`, since a workspace member's lockfile lives at the monorepo's top. `npm` without one.
fn package_runner(dir: &Path, stop: &Path) -> &'static str {
    for d in dir.ancestors() {
        for (lockfile, runner) in [
            ("pnpm-lock.yaml", "pnpm"),
            ("yarn.lock", "yarn"),
            ("bun.lock", "bun"),
            ("bun.lockb", "bun"),
            ("package-lock.json", "npm"),
        ] {
            if d.join(lockfile).is_file() {
                return runner;
            }
        }
        if d == stop || !d.starts_with(stop) {
            break;
        }
    }
    "npm"
}

/// The tasks runnable from `start`: its directory's and each ancestor's up to `root`, nearest
/// first, then mise's. A `start` outside `root` is a directory of its own.
///
/// mise is asked only when `trusted` and one of those directories has mise configuration — and
/// then in `start` itself, which is how mise resolves what runs from there.
pub async fn discover_here(start: PathBuf, root: PathBuf, trusted: bool) -> Vec<Task> {
    let mut dirs: Vec<PathBuf> = start
        .ancestors()
        .take_while(|d| d.starts_with(&root))
        .map(Path::to_path_buf)
        .collect();
    if dirs.is_empty() {
        dirs.push(start.clone());
    }
    let (mut tasks, ask_mise) = {
        let (dirs, root) = (dirs.clone(), root.clone());
        tokio::task::spawn_blocking(move || {
            let tasks: Vec<Task> = dirs.iter().flat_map(|d| dir_tasks(d, &root)).collect();
            let ask_mise = trusted && dirs.iter().any(|d| has_mise_config(d));
            (tasks, ask_mise)
        })
        .await
        .unwrap_or_default()
    };
    if ask_mise {
        let env = mise_environment(&root).await;
        tasks.extend(mise_tasks(&start, &env).await);
    }
    order(tasks, &dirs)
}

/// Every task in the workspace: the runner files among `files` (the workspace index, so ignored
/// directories stay out), and — when `trusted` — mise's for each directory holding mise
/// configuration.
pub async fn discover_workspace(
    files: &[crate::workspace_index::CachedFile],
    roots: &[PathBuf],
    trusted: bool,
) -> Vec<Task> {
    let dir_of = |f: &crate::workspace_index::CachedFile| {
        Path::new(&f.abs)
            .parent()
            .map_or_else(|| PathBuf::from(&f.abs), Path::to_path_buf)
    };
    let mut dirs: Vec<(u32, String, PathBuf)> = Vec::new();
    let mut mise_dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for f in files {
        let name = f
            .relative_path
            .rsplit('/')
            .next()
            .unwrap_or(&f.relative_path);
        if is_runner_file(name) {
            let dir = dir_of(f);
            if seen.insert(dir.clone()) {
                let rel = f
                    .relative_path
                    .rsplit_once('/')
                    .map_or("", |(d, _)| d)
                    .to_string();
                dirs.push((f.path_index, rel, dir));
            }
        }
        if trusted && is_mise_file(&f.relative_path) {
            let dir = dir_of(f);
            if !mise_dirs.contains(&dir) {
                mise_dirs.push(dir);
            }
        }
    }
    // Roots first, then down the tree — a directory's tasks before its subdirectories'.
    dirs.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let dirs: Vec<PathBuf> = dirs.into_iter().map(|(_, _, d)| d).collect();

    let read = {
        let (dirs, roots) = (dirs.clone(), roots.to_vec());
        tokio::task::spawn_blocking(move || {
            dirs.iter()
                .flat_map(|d| {
                    let stop = roots
                        .iter()
                        .find(|r| d.starts_with(r))
                        .cloned()
                        .unwrap_or_else(|| d.clone());
                    dir_tasks(d, &stop)
                })
                .collect::<Vec<_>>()
        })
    };
    let mise = async {
        let Some(root) = roots.first().filter(|_| !mise_dirs.is_empty()) else {
            return Vec::new();
        };
        let env = mise_environment(root).await;
        futures_util::future::join_all(mise_dirs.iter().map(|d| mise_tasks(d, &env)))
            .await
            .into_iter()
            .flatten()
            .collect()
    };
    let (read, mise): (_, Vec<Task>) = tokio::join!(read, mise);
    let mut tasks = read.unwrap_or_default();
    tasks.extend(mise);
    // mise answers from every directory with the tasks it inherits too, so the same task comes back
    // once per directory below its config.
    let mut unique = HashSet::new();
    tasks.retain(|t| unique.insert((t.path.clone(), t.name.clone())));
    let mut all_dirs = dirs;
    all_dirs.extend(mise_dirs);
    order(tasks, &all_dirs)
}

/// Sort `tasks` by where they are defined: under the entry of `dirs` nearest the definition (the
/// longest one containing it), in `dirs` order, and the rest — a global mise config — last. Stable,
/// so a directory keeps its files' order and each file its definitions'.
fn order(mut tasks: Vec<Task>, dirs: &[PathBuf]) -> Vec<Task> {
    let rank = |t: &Task| {
        dirs.iter()
            .enumerate()
            .filter(|(_, d)| t.path.starts_with(d))
            .max_by_key(|(_, d)| d.components().count())
            .map_or(usize::MAX, |(i, _)| i)
    };
    tasks.sort_by_cached_key(rank);
    tasks
}

// ---- mise --------------------------------------------------------------------------------------

/// The fields of `mise tasks ls --json` this reads.
#[derive(Debug, serde::Deserialize)]
struct MiseTask {
    name: String,
    #[serde(default)]
    description: String,
    /// The file that defines it: a TOML config, or the script of a file task.
    #[serde(default)]
    source: String,
    /// Where it runs.
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    hide: bool,
}

/// Ask mise which tasks it would run from `dir`. Nothing when mise is not installed, refuses (an
/// untrusted config makes it refuse outright), or takes too long.
async fn mise_tasks(dir: &Path, env: &HashMap<String, String>) -> Vec<Task> {
    let mut cmd = tokio::process::Command::new("mise");
    cmd.args(["tasks", "ls", "--json"])
        .current_dir(dir)
        .env_clear()
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(MISE_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => out.stdout,
        _ => return Vec::new(),
    };
    let listed = parse_mise_json(&String::from_utf8_lossy(&output));
    tokio::task::spawn_blocking(move || {
        // One read per config, however many tasks it defines.
        let mut configs: HashMap<PathBuf, Option<String>> = HashMap::new();
        listed
            .into_iter()
            .map(|t| {
                let path = PathBuf::from(&t.source);
                let line = if path.extension().is_some_and(|e| e == "toml") {
                    configs
                        .entry(path.clone())
                        .or_insert_with(|| std::fs::read_to_string(&path).ok())
                        .as_deref()
                        .and_then(|text| toml_task_line(text, &t.name))
                        .unwrap_or(0)
                } else {
                    0
                };
                let dir = t
                    .dir
                    .map(PathBuf::from)
                    .filter(|d| d.is_dir())
                    .or_else(|| path.parent().map(Path::to_path_buf))
                    .unwrap_or_default();
                Task {
                    command: format!("mise run {}", aether_shell::quote(&t.name)),
                    name: t.name,
                    dir,
                    path,
                    line,
                    description: t.description,
                }
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

fn parse_mise_json(json: &str) -> Vec<MiseTask> {
    serde_json::from_str::<Vec<MiseTask>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| !t.hide && !t.source.is_empty())
        .collect()
}

/// The 0-based line defining task `name` in a mise TOML config: a `[tasks.name]` table, a
/// `name = …` key under `[tasks]`, or a top-level dotted `tasks.name = …`.
fn toml_task_line(text: &str, name: &str) -> Option<u32> {
    let spellings = [name.to_string(), format!("\"{name}\""), format!("'{name}'")];
    let mut table: Option<String> = None;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            let header: String = t.chars().filter(|c| !c.is_whitespace()).collect();
            if spellings.iter().any(|s| header == format!("[tasks.{s}]")) {
                return Some(i as u32);
            }
            table = Some(header);
            continue;
        }
        let key_here = |prefix: &str| {
            spellings.iter().any(|s| {
                t.strip_prefix(prefix)
                    .and_then(|r| r.strip_prefix(s.as_str()))
                    .is_some_and(|r| r.trim_start().starts_with(['=', '.']))
            })
        };
        let hit = match table.as_deref() {
            Some("[tasks]") => key_here(""),
            None => key_here("tasks."),
            _ => false,
        };
        if hit {
            return Some(i as u32);
        }
    }
    None
}

// ---- the formats read directly -----------------------------------------------------------------

/// A justfile's public recipes. A recipe is a header at the start of a line — a name, maybe
/// parameters, then a `:` that is not `:=`. Private ones (a leading `_`, or `[private]`) are left
/// out, as `just --list` leaves them out. The description is a `[doc(…)]` attribute, else the
/// comment immediately above.
///
/// Imported files and modules are not followed.
fn parse_justfile(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut comment: Option<String> = None;
    let mut doc_attr: Option<String> = None;
    let mut private = false;
    for (i, line) in text.lines().enumerate() {
        let reset = |comment: &mut Option<String>, doc: &mut Option<String>, p: &mut bool| {
            *comment = None;
            *doc = None;
            *p = false;
        };
        // Indented: a recipe body or a continuation, never a definition.
        if line.starts_with([' ', '\t']) {
            reset(&mut comment, &mut doc_attr, &mut private);
            continue;
        }
        let t = line.trim_end();
        if t.is_empty() {
            reset(&mut comment, &mut doc_attr, &mut private);
            continue;
        }
        if let Some(c) = t.strip_prefix('#') {
            comment = (!t.starts_with("#!")).then(|| c.trim().to_string());
            continue;
        }
        // Attributes sit between a recipe's comment and its header, and keep both.
        if let Some(inner) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            for attr in split_attributes(inner) {
                let attr = attr.trim();
                if attr == "private" {
                    private = true;
                } else if let Some(arg) = attr
                    .strip_prefix("doc")
                    .map(str::trim_start)
                    .and_then(|r| r.strip_prefix('('))
                    .and_then(|r| r.strip_suffix(')'))
                {
                    doc_attr = Some(unquote(arg.trim()));
                }
            }
            continue;
        }
        let first = t.split_whitespace().next().unwrap_or("");
        if matches!(
            first,
            "set" | "alias" | "export" | "unexport" | "import" | "import?" | "mod" | "mod?"
        ) && !t[first.len()..].trim_start().starts_with(':')
        {
            reset(&mut comment, &mut doc_attr, &mut private);
            continue;
        }
        let header = t.strip_prefix('@').unwrap_or(t);
        let name_len = header
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .unwrap_or(header.len());
        let name = &header[..name_len];
        let rest = &header[name_len..];
        let is_recipe = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && !rest.trim_start().starts_with(":=")
            && has_header_colon(rest);
        if is_recipe && !private && !name.starts_with('_') {
            out.push(Found {
                name: name.to_string(),
                line: i as u32,
                description: doc_attr.take().or(comment.take()).unwrap_or_default(),
            });
        }
        reset(&mut comment, &mut doc_attr, &mut private);
    }
    out
}

/// Split `[a, b("x, y")]`'s inside on the commas that separate attributes, not those in arguments.
fn split_attributes(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None::<char>, 0usize);
    for (i, c) in inner.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&inner[start..]);
    out
}

/// A just string literal's text: the quotes off, and nothing else — escapes are rare in a doc and
/// showing one raw costs less than getting one wrong.
fn unquote(s: &str) -> String {
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner.to_string();
        }
    }
    s.to_string()
}

/// Whether what follows a recipe name reaches the header's `:` — one outside the parameters'
/// quoted defaults, and not the start of `:=`.
fn has_header_colon(rest: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, ':') => return chars.peek() != Some(&'='),
            // A `#` outside quotes starts a comment; a header's colon is never after one.
            (None, '#') => return false,
            _ => {}
        }
    }
    false
}

/// A Makefile's named targets: rule lines at the start of a line whose targets are plain names.
/// Pattern rules, file-shaped targets (with a `.` or `/`), special targets (`.PHONY`), variable
/// assignments and target-specific variables are left out. The description is a trailing
/// `## …` on the rule line — the self-documenting-Makefile convention — else the comment
/// immediately above.
///
/// `include`d files are not followed.
fn parse_makefile(text: &str) -> Vec<Found> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut comment: Option<String> = None;
    let mut in_define = false;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_end();
        if in_define {
            in_define = t.trim_start() != "endef" && !t.trim_start().starts_with("endef ");
            continue;
        }
        if line.starts_with(['\t', ' ']) || t.is_empty() {
            comment = None;
            continue;
        }
        if let Some(c) = t.strip_prefix('#') {
            comment = Some(c.trim_start_matches('#').trim().to_string());
            continue;
        }
        let first = t.split_whitespace().next().unwrap_or("");
        if first == "define" {
            in_define = true;
            comment = None;
            continue;
        }
        let Some(colon) = t.find(':') else {
            comment = None;
            continue;
        };
        let (targets, after) = (&t[..colon], &t[colon + 1..]);
        let after = after.strip_prefix(':').unwrap_or(after);
        let (prereqs, trailing) = match after.split_once("##") {
            Some((p, d)) => (p, Some(d.trim())),
            None => (after.split('#').next().unwrap_or(after), None),
        };
        // `X := y`, `X ::= y`, `X = a:b`, and `target: VAR = value`.
        if targets.contains('=') || after.starts_with('=') || prereqs.contains('=') {
            comment = None;
            continue;
        }
        let names: Vec<&str> = targets.split_whitespace().collect();
        // A special target (`.PHONY: build`) sits between a comment and its rule and keeps it.
        if !names.is_empty() && names.iter().all(|n| n.starts_with('.')) {
            continue;
        }
        let description = trailing
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .or(comment.take())
            .unwrap_or_default();
        for name in names {
            let plain = !name.contains(['.', '/', '%', '$', '(', ')', '\\', '*']);
            if plain && seen.insert(name.to_string()) {
                out.push(Found {
                    name: name.to_string(),
                    line: i as u32,
                    description: description.clone(),
                });
            }
        }
        comment = None;
    }
    out
}

/// A `package.json`'s scripts, in the order the file lists them. npm has no descriptions, so the
/// script itself stands in for one — it is what you would want to know before running it.
fn parse_package_json(text: &str) -> Vec<Found> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(scripts) = value.get("scripts").and_then(|s| s.as_object()) else {
        return Vec::new();
    };
    let scripts_at = text.find("\"scripts\"").unwrap_or(0);
    let mut out: Vec<Found> = scripts
        .iter()
        .map(|(name, body)| {
            let at =
                json_key_offset(&text[scripts_at..], name).map_or(scripts_at, |o| o + scripts_at);
            Found {
                name: name.clone(),
                line: text[..at].matches('\n').count() as u32,
                description: body.as_str().unwrap_or_default().to_string(),
            }
        })
        .collect();
    out.sort_by_key(|f| f.line);
    out
}

/// Where `key` is written as an object key in `text`: its quoted spelling followed by a `:`.
fn json_key_offset(text: &str, key: &str) -> Option<usize> {
    let quoted = serde_json::to_string(key).ok()?;
    let mut from = 0;
    while let Some(o) = text[from..].find(&quoted) {
        let at = from + o;
        if text[at + quoted.len()..].trim_start().starts_with(':') {
            return Some(at);
        }
        from = at + quoted.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(found: &[Found]) -> Vec<&str> {
        found.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn justfile_recipes_skip_assignments_settings_and_private_ones() {
        let text = "\
set shell := [\"bash\", \"-c\"]
version := \"1.0\"
alias b := build

# Build everything
build target='all': deps
    cargo build

[private]
helper:
    true

_hidden:
    true

[doc('Run the tests')]
[no-cd]
test *args:
    cargo test {{args}}

@quiet:
    echo hi

export FOO := \"bar\"
";
        let found = parse_justfile(text);
        assert_eq!(names(&found), ["build", "test", "quiet"]);
        assert_eq!(found[0].line, 5);
        assert_eq!(found[0].description, "Build everything");
        assert_eq!(found[1].description, "Run the tests");
        assert_eq!(found[2].description, "");
    }

    #[test]
    fn justfile_comment_only_describes_the_recipe_right_below_it() {
        let text = "# stray\n\nbuild:\n    true\n";
        assert_eq!(parse_justfile(text)[0].description, "");
    }

    #[test]
    fn justfile_colon_inside_a_quoted_default_is_not_the_header() {
        let text = "serve addr=':8080':\n    run\n";
        assert_eq!(names(&parse_justfile(text)), ["serve"]);
        // An assignment whose value holds a colon is still an assignment.
        assert!(parse_justfile("url := \"http://x\"\n").is_empty());
    }

    #[test]
    fn makefile_targets_are_plain_named_rules() {
        let text = "\
CC := gcc
FLAGS = -a:b

.PHONY: build test
# Build it
build: deps
\t$(CC) -o app

test: build ## Run the tests
\t./app --test

%.o: %.c
\t$(CC) -c $<

app.o: app.c
out/dir:
build: more

define RECIPE
fake: target
endef

debug: FLAGS += -g
install uninstall:
";
        let found = parse_makefile(text);
        assert_eq!(names(&found), ["build", "test", "install", "uninstall"]);
        assert_eq!(found[0].line, 5);
        assert_eq!(found[0].description, "Build it");
        assert_eq!(found[1].description, "Run the tests");
        assert_eq!(found[2].line, found[3].line);
    }

    #[test]
    fn package_scripts_keep_file_order_and_their_lines() {
        let text = r#"{
  "name": "web",
  "scripts": {
    "test": "vitest run",
    "build": "vite build",
    "dev": "vite"
  },
  "devDependencies": { "vite": "^5" }
}"#;
        let found = parse_package_json(text);
        assert_eq!(names(&found), ["test", "build", "dev"]);
        assert_eq!(found.iter().map(|f| f.line).collect::<Vec<_>>(), [3, 4, 5]);
        assert_eq!(found[1].description, "vite build");
        assert!(parse_package_json("{\"name\": \"x\"}").is_empty());
        assert!(parse_package_json("not json").is_empty());
    }

    #[test]
    fn a_script_named_like_another_scripts_body_finds_its_own_key() {
        let text = "{\n\"scripts\": {\n\"a\": \"b\",\n\"b\": \"x\"\n}\n}";
        let found = parse_package_json(text);
        assert_eq!(found[1].name, "b");
        assert_eq!(found[1].line, 3);
    }

    #[test]
    fn toml_task_lines_cover_tables_keys_and_dotted_keys() {
        let text = "\
tasks.lint = \"cargo clippy\"

[tools]
node = \"latest\"

[tasks]
fmt = \"cargo fmt\"
\"test:unit\" = \"cargo test\"

[tasks.build]
run = \"cargo build\"

[tasks.\"build:web\"]
run = \"npm run build\"
";
        assert_eq!(toml_task_line(text, "lint"), Some(0));
        assert_eq!(toml_task_line(text, "fmt"), Some(6));
        assert_eq!(toml_task_line(text, "test:unit"), Some(7));
        assert_eq!(toml_task_line(text, "build"), Some(9));
        assert_eq!(toml_task_line(text, "build:web"), Some(12));
        assert_eq!(toml_task_line(text, "node"), None);
    }

    #[test]
    fn mise_json_drops_hidden_tasks() {
        let json = r#"[
            {"name": "build", "description": "Build it", "source": "/p/mise.toml", "dir": "/p", "hide": false, "extra": 1},
            {"name": "secret", "source": "/p/mise.toml", "hide": true}
        ]"#;
        let listed = parse_mise_json(json);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "build");
        assert_eq!(listed[0].description, "Build it");
        assert!(parse_mise_json("mise ERROR").is_empty());
    }

    #[test]
    fn runner_files_pick_what_the_runner_would() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "Makefile",
            "GNUmakefile",
            "Justfile",
            "package.json",
            "README.md",
        ] {
            std::fs::write(dir.path().join(name), "").unwrap();
        }
        let files: Vec<(Format, String)> = runner_files(dir.path())
            .into_iter()
            .map(|(f, p)| (f, p.file_name().unwrap().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(
            files,
            [
                (Format::Just, "Justfile".to_string()),
                (Format::Make, "GNUmakefile".to_string()),
                (Format::Package, "package.json".to_string()),
            ]
        );
    }

    #[test]
    fn the_nearest_lockfile_names_the_package_manager() {
        let root = tempfile::tempdir().unwrap();
        let member = root.path().join("packages/web");
        std::fs::create_dir_all(&member).unwrap();
        assert_eq!(package_runner(&member, root.path()), "npm");
        std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(package_runner(&member, root.path()), "pnpm");
        std::fs::write(member.join("yarn.lock"), "").unwrap();
        assert_eq!(package_runner(&member, root.path()), "yarn");
    }

    #[tokio::test]
    async fn tasks_from_here_list_the_nearest_directory_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sub = root.join("crates/app");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("justfile"), "build:\n    true\n").unwrap();
        std::fs::write(sub.join("Makefile"), "run:\n\ttrue\n").unwrap();
        std::fs::write(
            sub.join("package.json"),
            r#"{"scripts": {"build:web": "vite build"}}"#,
        )
        .unwrap();

        let tasks = discover_here(sub.clone(), root.clone(), true).await;
        let rows: Vec<(&str, &str, &Path)> = tasks
            .iter()
            .map(|t| (t.name.as_str(), t.command.as_str(), t.dir.as_path()))
            .collect();
        assert_eq!(
            rows,
            [
                ("run", "make run", sub.as_path()),
                ("build:web", "npm run build:web", sub.as_path()),
                ("build", "just build", root.as_path()),
            ]
        );

        // From the root, the subdirectory's tasks are not "here".
        let tasks = discover_here(root.clone(), root.clone(), true).await;
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn order_puts_tasks_under_their_nearest_listed_directory() {
        let task = |path: &str| Task {
            name: path.into(),
            command: String::new(),
            dir: PathBuf::new(),
            path: PathBuf::from(path),
            line: 0,
            description: String::new(),
        };
        let dirs = [PathBuf::from("/w"), PathBuf::from("/w/a")];
        let ordered = order(
            vec![
                task("/home/.config/mise/config.toml"),
                task("/w/a/.mise/tasks/x"),
                task("/w/justfile"),
                task("/w/a/justfile"),
            ],
            &dirs,
        );
        let paths: Vec<&str> = ordered.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/w/justfile",
                "/w/a/.mise/tasks/x",
                "/w/a/justfile",
                "/home/.config/mise/config.toml",
            ]
        );
    }

    #[test]
    fn mise_files_are_recognised_wherever_mise_keeps_them() {
        for p in [
            "mise.toml",
            ".mise.toml",
            "sub/mise.local.toml",
            ".mise/tasks/build",
            "mise-tasks/lint",
            ".config/mise/config.toml",
            ".config/mise.toml",
        ] {
            assert!(is_mise_file(p), "{p}");
        }
        assert!(!is_mise_file("src/promise.toml"));
        assert!(!is_mise_file("Cargo.toml"));
    }
}
