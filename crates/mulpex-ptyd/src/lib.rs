//! `mulpex-ptyd`: the process that owns every claude's and terminal's PTY, so
//! they keep running while the Mulpex app restarts (an update, a crash) and the
//! new app can attach to them again. See `docs/ptyd.md`.
//!
//! The daemon is deliberately dumb: the app builds every argv and environment,
//! keeps all hub logic, and talks to it over one unix socket (`proto`). What
//! lives here is only what has to outlive the app — the PTY masters, the child
//! processes (and their teardown), each session's output ring, a shell's
//! transcript recorder (`vtgrid`), and the Live Token proxy the claudes call.
//!
//! An app update that ships a new daemon does NOT replace a running one: the
//! new app keeps talking to the old daemon as long as its protocol is in range
//! (`proto::PROTO_MIN..=proto::PROTO`), and the new daemon starts after the next
//! real quit. Anything changed here therefore reaches users late — keep it small.

pub mod client;
pub mod daemon;
pub mod procs;
pub mod proto;
pub mod vtgrid;
