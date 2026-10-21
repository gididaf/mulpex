//! Which coding agent an instance runs: Claude Code or OpenAI Codex CLI.
//!
//! Everything agent-specific lives behind a function that takes an [`Agent`] —
//! never an inline `if codex` — so the Claude path stays byte-identical and the
//! Codex differences are listed in one place each. See `docs/codex.md`.
//!
//! The helper (hooks, MCP server, listener) learns its agent from
//! `MULPEX_AGENT`; a missing variable means Claude, so every claude that was
//! spawned before this existed keeps behaving exactly as it did.

/// The env var the app sets on every spawned agent (and on Codex's MCP server).
pub const AGENT_ENV: &str = "MULPEX_AGENT";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum Agent {
    #[default]
    Claude,
    Codex,
}

impl Agent {
    /// The word used in addresses (`codex#3`), labels and on the wire.
    pub fn word(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// The name people read ("Claude", "Codex").
    pub fn title(self) -> &'static str {
        match self {
            Agent::Claude => "Claude",
            Agent::Codex => "Codex",
        }
    }

    pub fn parse(s: &str) -> Option<Agent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }

    pub fn other(self) -> Agent {
        match self {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        }
    }

    /// The agent this helper process serves. Missing or unknown → Claude.
    pub fn from_env() -> Agent {
        std::env::var(AGENT_ENV)
            .ok()
            .and_then(|v| Agent::parse(&v))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip_and_default_is_claude() {
        for a in [Agent::Claude, Agent::Codex] {
            assert_eq!(Agent::parse(a.word()), Some(a));
            assert_eq!(a.other().other(), a);
        }
        assert_eq!(Agent::parse("CODEX"), Some(Agent::Codex));
        assert_eq!(Agent::parse("gpt"), None);
        assert_eq!(Agent::default(), Agent::Claude);
    }
}
