//! Which ACP agents this build knows how to launch.
//!
//! A table in code, not a settings surface — the same choice [`crate::lsp::config`] makes for the
//! thirteen language servers, and for the same reason: an agent is a program with a fixed
//! invocation, a new one is a code change, and a workspace file naming a command to spawn is a
//! security surface we have no reason to open.
//!
//! An agent is *offered* when its program resolves on `PATH`. Everything ships behind `npx`, which
//! is why the check is for the launcher rather than the package.

/// One launchable agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSpec {
    /// Stable across restarts: what a session file and `agent/open { agent }` name.
    pub id: &'static str,
    /// What the picker row and the input's box call it.
    pub name: &'static str,
    /// The program to run. Checked against `PATH` to decide whether to offer this agent at all.
    pub program: &'static str,
    /// Its arguments, in order.
    pub args: &'static [&'static str],
}

/// The agents we know, in the order they are offered. First that resolves wins when `agent/open`
/// names none.
///
/// Both entries mirror the constructors the ACP SDK ships (`AcpAgent::claude_agent()` and
/// `AcpAgent::codex()`), spelled out here rather than called so that the table is the one place
/// this build's agents are listed and `program` is a real thing to look for on `PATH`.
pub const KNOWN_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        id: "claude",
        name: "Claude Code",
        program: "npx",
        args: &["-y", "@agentclientprotocol/claude-agent-acp@latest"],
    },
    AgentSpec {
        id: "codex",
        name: "Codex",
        program: "npx",
        args: &["-y", "@agentclientprotocol/codex-acp@latest"],
    },
    AgentSpec {
        id: "gemini",
        name: "Gemini CLI",
        program: "gemini",
        args: &["--experimental-acp"],
    },
];

/// The row with this id, whatever `PATH` says. `None` for an id this build does not know — which
/// is what a session file written by a newer build looks like.
pub fn by_id(id: &str) -> Option<&'static AgentSpec> {
    KNOWN_AGENTS.iter().find(|a| a.id == id)
}

/// Every agent whose program is on `PATH`, in table order.
pub fn available() -> Vec<&'static AgentSpec> {
    KNOWN_AGENTS.iter().filter(|a| on_path(a.program)).collect()
}

/// The agent `agent/open` uses when it was given no id: the first available.
pub fn default_agent() -> Option<&'static AgentSpec> {
    available().into_iter().next()
}

/// Whether `program` resolves on `PATH`. A plain search rather than a spawn: launching an agent to
/// find out whether it exists would cost seconds and a subprocess per keystroke in the picker.
fn on_path(program: &str) -> bool {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return std::path::Path::new(program).is_file();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique() {
        // The id is what a session file records and what `agent/open` names; two rows sharing one
        // would make restore pick whichever happened to be first.
        let mut ids: Vec<_> = KNOWN_AGENTS.iter().map(|a| a.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate agent id in KNOWN_AGENTS");
    }

    #[test]
    fn by_id_finds_every_row_and_nothing_else() {
        for spec in KNOWN_AGENTS {
            assert_eq!(by_id(spec.id), Some(spec));
        }
        assert_eq!(by_id("no-such-agent"), None);
    }

    #[test]
    fn a_program_that_cannot_exist_is_not_on_path() {
        assert!(!on_path("aether-no-such-program-8f3a1c"));
    }

    #[test]
    fn an_absolute_program_is_checked_as_a_file() {
        // Covers the branch that skips the PATH walk. `/bin/sh` is a file on every unix we build
        // for; the negative case is the one that would silently offer a missing agent.
        assert!(on_path("/bin/sh"));
        assert!(!on_path("/bin/aether-no-such-program-8f3a1c"));
    }
}
