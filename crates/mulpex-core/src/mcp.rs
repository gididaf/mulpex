//! The `mulpex mcp` subcommand — the inner **coordination hub**. Mulpex registers
//! this same binary (via `--mcp-config`) as a stdio MCP server on every `claude`
//! instance, so the instances can see what the others are doing and message each
//! other. Like `hook.rs`, identity comes from the inherited env
//! (`MULPEX_INSTANCE_ID` / `MULPEX_STATE_DIR` / `MULPEX_PROJECT_DIR`) and all
//! cross-instance "shared memory" is plain files under `state_dir` — no network.
//!
//! Transport is the MCP **stdio** protocol: newline-delimited JSON-RPC 2.0. We
//! implement the minimum a client needs — `initialize`, `tools/list`,
//! `tools/call`, plus `ping` — and ignore notifications (no `id`). Every handler
//! **fails soft**: a bad request is skipped and a tool error is returned as text,
//! never a crash, so the hub can't wedge a Claude turn.
//!
//! Tools (namespaced `mcp__mulpex__*` in Claude):
//! - `hub_instances` — every instance's id / status / task / held files (+ my unread count)
//! - `hub_set_focus` — publish *my* current task (refines the auto-captured prompt)
//! - `hub_set_name` — label *my own* sidebar row (a real, persisted rename)
//! - `hub_file_owner` — who holds a given path, and what they're working on
//! - `hub_send` — leave a message for another instance (or `all`)
//! - `hub_inbox` — read (and clear) the messages addressed to me
//! - `hub_spawn` — start new instances, each seeded with a task, and get their ids
//! - `hub_terminal_open` / `_send` / `_read` / `_close` — create and drive plain
//!   shell terminals in this project, and read their output incrementally

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::hook::{canonical_target, now, read_field, Ctx};
use crate::remote;
use crate::persist::{fnv1a, new_uuid};
use crate::termlog;

/// Entry point for `mulpex mcp`. Runs the stdio JSON-RPC loop until stdin closes
/// (the parent `claude` exiting). Always returns `Ok`.
///
/// Each `tools/call` is handled on its own thread, with only the response write
/// serialized. Claude Code batches independent tool calls into one message
/// routinely, and some tools here genuinely block — `hub_terminal_read` can wait
/// half a minute for a build to say something, and `hub_spawn` waits several
/// seconds for its children. Handling calls one at a time would park the whole
/// batch behind whichever one is waiting. JSON-RPC responses are matched by `id`,
/// so replying out of order is legal.
pub fn run(_args: &[String]) -> anyhow::Result<()> {
    let Some(ctx) = Ctx::from_env() else {
        return Ok(()); // no coordination context → a no-op server
    };
    let ctx = Arc::new(ctx);
    let stdin = std::io::stdin();
    // One writer, so two concurrent replies can't interleave mid-line.
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue; // unparseable → skip
        };
        let method = req.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let id = req.get("id").cloned();

        // No id → a notification (e.g. notifications/initialized); never reply.
        let Some(id) = id else { continue };

        // Everything but a tool call is a cheap, purely local answer; handling
        // those inline keeps the common path allocation-free.
        let response = match method {
            "initialize" => {
                let pv = req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("2025-06-18");
                ok(&id, json!({
                    "protocolVersion": pv,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mulpex-hub", "version": env!("CARGO_PKG_VERSION") },
                }))
            }
            "ping" => ok(&id, json!({})),
            "tools/list" => ok(&id, json!({ "tools": tool_defs() })),
            "tools/call" => {
                let ctx = Arc::clone(&ctx);
                let out = Arc::clone(&out);
                let params = req.get("params").cloned();
                workers.retain(|h| !h.is_finished());
                workers.push(std::thread::spawn(move || {
                    let response = match call_tool(&ctx, params.as_ref()) {
                        Ok(text) => {
                            ok(&id, json!({ "content": [ { "type": "text", "text": text } ] }))
                        }
                        Err(text) => ok(
                            &id,
                            json!({ "content": [ { "type": "text", "text": text } ], "isError": true }),
                        ),
                    };
                    if let Ok(mut w) = out.lock() {
                        let _ = writeln!(w, "{response}");
                        let _ = w.flush();
                    }
                }));
                continue;
            }
            _ => err(&id, -32601, "method not found"),
        };
        if let Ok(mut w) = out.lock() {
            let _ = writeln!(w, "{response}");
            let _ = w.flush();
        }
    }
    Ok(())
}

fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Most instances one `hub_spawn` call may create — a batch is fine, a flood is
/// not (each new instance is a full `claude` process sharing this working tree).
const MAX_SPAWN_PER_CALL: usize = 8;

/// The hub tools, as MCP tool definitions.
fn tool_defs() -> Value {
    let empty = json!({ "type": "object", "properties": {} });
    json!([
        {
            "name": "hub_instances",
            "description": "List every parallel Claude instance Mulpex is running here, with each one's status (working/waiting/needs), current task, and the files it currently holds a lock on. Also reports how many unread hub messages you have, and every shell terminal open in this project (whether opened by you, by another instance, or by the user) with how much output is waiting for you to read. Call this to coordinate before starting overlapping work.",
            "inputSchema": empty,
        },
        {
            "name": "hub_set_focus",
            "description": "Publish what YOU are currently working on so the other instances can see it (shown in Mulpex and via hub_instances). Refines the task auto-captured from your prompt.",
            "inputSchema": {
                "type": "object",
                "properties": { "task": { "type": "string", "description": "Short description of your current task/intent." } },
                "required": ["task"],
            },
        },
        {
            "name": "hub_set_name",
            "description": "Give YOUR OWN instance a short label for the Mulpex sidebar, so the user can tell the parallel instances apart at a glance. Name it after the work, not after yourself: 2-5 words, in the same language the user writes to you in (a Hebrew prompt gets a Hebrew name). Call this once, early, as soon as you know what this session is about — and again later only if the work genuinely changes to something else. If the user has named this instance themselves, their name wins and yours is ignored.",
            "inputSchema": {
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Short label for this instance, e.g. \"vtgrid soft-wrap fix\"." } },
                "required": ["name"],
            },
        },
        {
            "name": "hub_file_owner",
            "description": "Check whether a file is currently locked by another instance (because it's being edited), and if so which instance and what they're working on. Use before editing a shared file, or after an edit is denied.",
            "inputSchema": {
                "type": "object",
                "properties": { "path": { "type": "string", "description": "File path (absolute, or relative to the project root)." } },
                "required": ["path"],
            },
        },
        {
            "name": "hub_send",
            "description": "Leave a message for another instance (e.g. 'I'm refactoring auth, hold off on session.rs'), or broadcast to every other instance at once with to: \"all\". It appears in each recipient's hub_inbox and is surfaced at the start of its next turn. Note that a message is mandatory reading for whoever receives it — an instance cannot finish a turn holding unread mail — so broadcast only what genuinely concerns everyone, and name a single recipient otherwise.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Recipient instance number (e.g. \"2\"), or \"all\" to broadcast to every other instance." },
                    "message": { "type": "string", "description": "The message body." },
                },
                "required": ["to", "message"],
            },
        },
        {
            "name": "hub_inbox",
            "description": "Read and clear the messages other instances have sent you. Returns each message's sender and body.",
            "inputSchema": empty,
        },
        {
            "name": "hub_spawn",
            "description": "Start one or more NEW Claude instances in this same project, each seeded with its own task that it begins working on immediately and autonomously. Use this to fan work out — e.g. fetch a list of tickets/items and spawn one instance per item to handle it in parallel. Each new instance is a full sibling on the coordination hub; it is told you are its spawner and will hub_send its result back to you when done. Returns the new instances' ids so you can track them (hub_instances) or message them (hub_send). Max 8 per call — for more, call again in batches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "One task per new instance to create. Each string is the full assignment that instance will start on (e.g. one ticket's title + details).",
                    },
                },
                "required": ["tasks"],
            },
        },
        {
            "name": "hub_remote_open",
            "description": "Start a Claude Code instance on ANOTHER MACHINE over ssh, in a Mulpex terminal, and coordinate with it. Opens its own terminal by default; pass terminal_id to use one that already exists, including one the user ssh'd in on themselves. Use this when work has to happen on a remote server (a deploy, a staging box, anything that must run there rather than here). The remote instance is told it is being driven by you, works autonomously, and SIGNALS you when it finishes, gets blocked, or needs an answer — you are woken by a hub message, so do NOT sit polling it. Talk to it with hub_terminal_send and read it with hub_terminal_read, using the terminal id this returns. It is a terminal, NOT a hub instance: hub_send can never reach it. Requires working ssh key access to the target, and claude installed there.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ssh_target": { "type": "string", "description": "Where to ssh, e.g. \"root@10.0.0.5\" or an ~/.ssh/config alias. Key-based auth must already work — there is nowhere to type a password. Optional ONLY when 'terminal_id' names a terminal that is already logged in to the remote machine." },
                    "terminal_id": { "type": "integer", "description": "Use an EXISTING terminal instead of opening a new one. Two uses: a terminal sitting at a local shell (give ssh_target too and it will ssh from there), or one the user has ALREADY ssh'd in on (omit ssh_target — only claude is started, on the far side). Useful when the login needed a password, a VPN or a jump host. Refused if that terminal is busy or already running a claude." },
                    "cwd": { "type": "string", "description": "Directory ON THE REMOTE machine to start in (its project dir). Defaults to the login directory." },
                    "task": { "type": "string", "description": "What the remote should do, sent once its prompt is ready. Include everything it needs: it cannot see your conversation, your files, or the user." },
                },
                "required": ["ssh_target"],
            },
        },
        {
            "name": "hub_terminal_open",
            "description": "Open a NEW shell terminal in this project, shown in Mulpex's sidebar next to the instances, and optionally start a command in it. Unlike your Bash tool this is a real, persistent interactive shell: it keeps running after the command finishes, you can type into it again later (hub_terminal_send) and read its output at any time (hub_terminal_read). Use it for anything long-running or interactive — a dev server, a watcher, `tail -f`, a REPL or database shell, or a long build you want to keep an eye on while you do other work. For a quick command that returns promptly, just use Bash instead. Returns the new terminal's id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Optional command line to run as soon as the shell is ready (e.g. \"npm run dev\"). Sent verbatim — whitespace and layout are preserved. The shell stays open afterwards." },
                    "name": { "type": "string", "description": "Optional short label for the sidebar row. Defaults to the command." },
                },
            },
        },
        {
            "name": "hub_terminal_send",
            "description": "Type into a terminal: run a command in it, answer a question a running command asked, or send a control key. Provide `input` for text (submitted with Enter unless submit=false), or `control` for a control key such as \"c\" to interrupt a running process. When you submit a single-line command at a shell prompt, Mulpex tracks it so hub_terminal_read can tell you exactly when it finished and with what exit code; the reply's `tracking_completion` says whether it did. MULTI-LINE input is never tracked — the completion marker would land on the last line and break a heredoc terminator — so read the output to see when it finishes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Terminal id (from hub_terminal_open or hub_instances)." },
                    "input": { "type": "string", "description": "Text to type in. A command line, or an answer to a prompt." },
                    "submit": { "type": "boolean", "description": "Press Enter after the text. Default true. Set false to leave the text sitting at the prompt." },
                    "control": { "type": "string", "description": "A control key instead of text: a single letter for Ctrl-<letter> (\"c\" interrupts, \"d\" sends EOF), or \"enter\", \"escape\", \"tab\", \"up\", \"down\". Sent after `input` if both are given." },
                },
                "required": ["id"],
            },
        },
        {
            "name": "hub_terminal_read",
            "description": "Read a terminal's output. By default returns only what is NEW since YOUR last read of that terminal, so you can follow a long-running command by calling this repeatedly without re-reading everything. Also returns the terminal's current on-screen content (which may not have scrolled into the history yet — a dev server sitting at a steady screen produces no new history at all), how long it has been idle, whether it is still running, and — if you submitted a command with hub_terminal_send — whether that command has finished and its exit code. Use `wait_ms` to block until there is something new instead of polling.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Terminal id." },
                    "wait_ms": { "type": "integer", "description": "Block up to this many milliseconds (max 30000) instead of returning immediately. If a command you submitted is still running, this waits for it to FINISH; otherwise it waits for any new output. Either way it returns early if the terminal exits, and the reply's `waited_for` says which of the two it was waiting on. Works with `full` too." },
                    "lines": { "type": "integer", "description": "Cap on how many lines to return, most recent kept. Default 200." },
                    "full": { "type": "boolean", "description": "Ignore your read position and return the whole retained history instead. Use when you need the beginning of a run you have already partly read." },
                },
                "required": ["id"],
            },
        },
        {
            "name": "hub_terminal_close",
            "description": "Close a terminal and remove it from the sidebar. Only close terminals you opened for your own work — a terminal the user opened themselves, or one another instance is using, is not yours to close unless you were asked to.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Terminal id." },
                },
                "required": ["id"],
            },
        },
    ])
}

/// Dispatch a `tools/call`. Returns `Ok(text)` on success or `Err(text)` for a
/// tool-level error (still delivered to the model, just flagged `isError`).
fn call_tool(ctx: &Ctx, params: Option<&Value>) -> Result<String, String> {
    let params = params.ok_or("missing params")?;
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "hub_instances" => Ok(hub_instances(ctx)),
        "hub_set_focus" => hub_set_focus(ctx, &args),
        "hub_set_name" => hub_set_name(ctx, &args),
        "hub_file_owner" => Ok(hub_file_owner(ctx, &args)),
        "hub_send" => hub_send(ctx, &args),
        "hub_inbox" => Ok(hub_inbox(ctx)),
        "hub_spawn" => hub_spawn(ctx, &args),
        "hub_remote_open" => hub_remote_open(ctx, &args),
        "hub_terminal_open" => hub_terminal_open(ctx, &args),
        "hub_terminal_send" => hub_terminal_send(ctx, &args),
        "hub_terminal_read" => hub_terminal_read(ctx, &args),
        "hub_terminal_close" => hub_terminal_close(ctx, &args),
        other => Err(format!("unknown tool: {other}")),
    }
}

// ---- tool implementations -------------------------------------------------

fn hub_instances(ctx: &Ctx) -> String {
    let holds = locks_by_holder(ctx);
    let list: Vec<Value> = live_ids(ctx)
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "is_me": id == ctx.instance,
                "status": status_of(ctx, id),
                "task": task_of(ctx, id),
                "holds": holds.get(&id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    json!({
        "instances": list,
        "your_unread_messages": unread_for(ctx, ctx.instance),
        // Terminals ride along here rather than needing their own list call —
        // seeing "there is a dev server running in terminal #4" is exactly the
        // context an instance wants at the same moment it asks who else is here.
        "terminals": terminal_list(ctx),
    })
    .to_string()
}

/// The project's terminals, from the manifest Mulpex maintains, each with how
/// much output is waiting for *this* reader.
fn terminal_list(ctx: &Ctx) -> Vec<Value> {
    let Ok(index) = std::fs::read_to_string(ctx.state_dir.join("terminals").join("index")) else {
        return Vec::new();
    };
    index
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let id: usize = parts.next()?.trim().parse().ok()?;
            let state = parts.next().unwrap_or("running");
            let label = parts.next().unwrap_or("").trim();
            let unread = LogView::open(ctx, id)
                .map(|v| v.total.saturating_sub(cursor_of(ctx, id).unwrap_or(0)))
                .unwrap_or(0);
            Some(json!({
                "id": id,
                "running": state == "running",
                "name": if label.is_empty() { Value::Null } else { json!(label) },
                "new_output_bytes": unread,
            }))
        })
        .collect()
}

fn hub_set_focus(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let task = args
        .get("task")
        .and_then(|v| v.as_str())
        .ok_or("missing 'task'")?;
    let task = summarize(task);
    std::fs::write(ctx.tasks_dir.join(ctx.id_str()), &task)
        .map_err(|e| format!("could not save focus: {e}"))?;
    Ok(json!({ "ok": true, "task": task }).to_string())
}

/// Name this instance's own sidebar row. The label goes to Mulpex as a
/// `namereq/<id>` file which the poll loop turns into a real (persisted) rename,
/// exactly as if the user had pressed ⌘R.
///
/// **Fire-and-forget, unlike the terminal ops**, which wait for a `<token>.done`
/// reply: nothing here depends on the outcome, and the one case where the request
/// is *refused* — the user named this instance themselves, so their name wins —
/// is not something the model should react to. Blocking a turn on a cosmetic
/// rename would cost more than the answer is worth.
///
/// The `named/<id>` flag is written **here**, by the caller, rather than by
/// Mulpex when it applies the rename: it records that this instance has had its
/// say, which is what stops `AUTO_NAME_NUDGE` re-asking every turn. That has to
/// stop on a refusal too, so it cannot be keyed off the rename landing.
fn hub_set_name(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let raw = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("missing 'name'")?;
    // One line, capped — the same shape a ⌘R name or a spawned child's
    // task-derived label has, since all three land in the same sidebar row.
    let name = flatten_label(raw);
    if name.is_empty() {
        return Err("'name' is empty — give a short label for this instance.".to_string());
    }
    // Keyed by instance id, so a second call supersedes a still-pending first
    // one instead of queueing a rename the model has already thought better of.
    let path = crate::name_request_path(&ctx.state_dir, ctx.instance);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &name)
        .map_err(|e| format!("could not request the rename: {e}"))?;
    mark_named(ctx);
    Ok(json!({
        "ok": true,
        "name": name,
        "note": "Your sidebar row is renamed. If the user has named this instance themselves, \
                 their name stays.",
    })
    .to_string())
}

/// Record that this instance has named itself, so the hook stops nudging it to.
/// Mirrors the `armed/<id>` flag the hub listener's Monitor touches.
fn mark_named(ctx: &Ctx) {
    let path = crate::named_flag_path(&ctx.state_dir, ctx.instance);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, "");
}

fn hub_file_owner(ctx: &Ctx, args: &Value) -> String {
    let Some(raw) = args.get("path").and_then(|v| v.as_str()) else {
        return json!({ "error": "missing 'path'" }).to_string();
    };
    let Some(path) = canonical_target(ctx, raw) else {
        return json!({ "locked": false, "note": "could not resolve path" }).to_string();
    };
    let key = format!("{:016x}", fnv1a(path.to_string_lossy().as_bytes()));
    let lock_file = ctx.locks_dir.join(&key);
    match read_field(&lock_file, "instance").and_then(|s| s.parse::<usize>().ok()) {
        Some(holder) => json!({
            "locked": true,
            "holder": holder,
            "holder_is_me": holder == ctx.instance,
            "holder_task": task_of(ctx, holder),
            "path": path.display().to_string(),
        })
        .to_string(),
        None => json!({ "locked": false, "path": path.display().to_string() }).to_string(),
    }
}

fn hub_send(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let message = args
        .get("message")
        .and_then(|v| v.as_str())
        .ok_or("missing 'message'")?;
    // `to` may arrive as a number or a string ("2" / "all").
    let to_raw = match args.get("to") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => return Err("missing 'to' (instance number or \"all\")".into()),
    };

    let recipients: Vec<usize> = if to_raw.eq_ignore_ascii_case("all") {
        peer_ids(ctx)
    } else {
        let id: usize = to_raw.parse().map_err(|_| "'to' must be a number or \"all\"")?;
        // Refuse to "deliver" to an instance that isn't running: it has closed, so
        // the message would rot in an inbox no live instance reads (Mulpex reaps a
        // dead recipient's inbox). Tell the sender plainly instead of faking
        // success — this is what stops instances messaging a peer that's gone.
        if !live_ids(ctx).contains(&id) {
            return Err(format!(
                "claude #{id} is not a running instance — it has closed, so it can't receive \
                 messages and nothing was sent. Call mcp__mulpex__hub_instances to see who is \
                 still active."
            ));
        }
        vec![id]
    };
    if recipients.is_empty() {
        return Ok(json!({ "ok": false, "note": "no other instances are running right now" }).to_string());
    }

    let mut delivered = Vec::new();
    for to in recipients {
        let dir = ctx.inbox_dir.join(to.to_string());
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let body = json!({ "from": ctx.instance, "ts": now(), "body": message });
        if std::fs::write(dir.join(format!("{}.json", new_uuid())), body.to_string()).is_ok() {
            delivered.push(to);
        }
    }
    if !delivered.is_empty() {
        log_message(ctx, &to_raw, message);
    }
    Ok(json!({ "ok": !delivered.is_empty(), "delivered_to": delivered }).to_string())
}

/// Append a sent message to the persistent cross-instance conversation log
/// (`state_dir/messages.log`), TSV `ts\tfrom\tto\tbody`. The body's backslashes,
/// tabs and newlines are escaped so each message stays on one line (the UI
/// decodes them). Unlike the inbox files (deleted when the recipient reads them)
/// this log persists, so Mulpex can show the full instance-to-instance
/// conversation. One `write_all` under `O_APPEND` is atomic across instances.
fn log_message(ctx: &Ctx, to: &str, body: &str) {
    let esc = body.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n");
    let line = format!("{}\t{}\t{}\t{}\n", now(), ctx.instance, to, esc);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ctx.state_dir.join("messages.log"))
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

fn hub_inbox(ctx: &Ctx) -> String {
    let mut msgs = take_inbox(ctx, ctx.instance);
    msgs.sort_by_key(|m| m.0); // by ts
    let out: Vec<Value> = msgs
        .into_iter()
        .map(|(ts, from, body)| json!({ "from": from, "ts": ts, "message": body }))
        .collect();
    json!({ "messages": out }).to_string()
}

/// Queue new task-seeded instances for Mulpex to spawn. The MCP helper can't
/// create sessions itself (Mulpex owns the PTYs), so it drops a request file in
/// `state_dir/spawn/` and waits briefly for the poll loop to spawn them and write
/// back the assigned ids. If the response doesn't arrive in time the spawn is
/// still queued — the caller is told to discover the new instances via
/// `hub_instances`.
fn hub_spawn(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let tasks: Vec<String> = match args.get("tasks") {
        // Normal shape: an array of task strings, one per new instance.
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|t| t.as_str())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        // Be forgiving if a single task arrives as a bare string.
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        _ => {
            return Err("missing 'tasks' — provide an array of task strings, one per new \
                        instance to create."
                .into())
        }
    };
    if tasks.is_empty() {
        return Err("'tasks' is empty — provide at least one non-empty task string.".into());
    }
    if tasks.len() > MAX_SPAWN_PER_CALL {
        return Err(format!(
            "too many instances requested at once: {} (max {MAX_SPAWN_PER_CALL} per call). \
             Spawn in smaller batches — call hub_spawn again for the rest once these are under way.",
            tasks.len()
        ));
    }

    let spawn_dir = ctx.state_dir.join("spawn");
    std::fs::create_dir_all(&spawn_dir).map_err(|e| format!("could not queue spawn: {e}"))?;
    let token = new_uuid();
    let body = json!({ "from": ctx.instance, "ts": now(), "tasks": tasks });
    std::fs::write(spawn_dir.join(format!("{token}.json")), body.to_string())
        .map_err(|e| format!("could not queue spawn: {e}"))?;

    // Poll for the poll-loop's response (assigned ids). ~6s cap; the loop ticks
    // every 200ms and writes the `.done` file in the tick it processes the request.
    let done = spawn_dir.join(format!("{token}.done"));
    for _ in 0..60 {
        if let Ok(content) = std::fs::read_to_string(&done) {
            let _ = std::fs::remove_file(&done);
            let ids: Vec<u64> = serde_json::from_str::<Value>(&content)
                .ok()
                .and_then(|v| {
                    Some(
                        v.get("ids")?
                            .as_array()?
                            .iter()
                            .filter_map(|n| n.as_u64())
                            .collect(),
                    )
                })
                .unwrap_or_default();
            return Ok(json!({
                "ok": !ids.is_empty(),
                "spawned_instances": ids,
                "note": "New instances are starting on their assigned tasks and will hub_send \
                         their results back to you when done. Use hub_instances to check on them."
            })
            .to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(json!({
        "ok": true,
        "note": "Spawn requested; the new instances are being created. Call hub_instances in a \
                 moment to see them and their ids."
    })
    .to_string())
}

// ---- terminals ------------------------------------------------------------
//
// Mulpex owns the PTYs, so creating a terminal or typing into one is a file
// handshake through the poll loop, exactly like `hub_spawn`. *Reading* one is
// not: the app writes each terminal's transcript to a file, which this process
// reads directly. That asymmetry is deliberate — it's what makes "read its
// output at any time" cheap enough to poll.

/// How long to wait for the poll loop to acknowledge a terminal request. The
/// loop ticks every 200 ms and these ops are all O(1) for it, so this is a
/// generous ceiling rather than an expected wait.
const TERM_REQ_TIMEOUT_MS: u64 = 5_000;

/// Ceiling on `hub_terminal_read`'s blocking wait. Claude Code's own per-tool
/// MCP timeout is on the order of a minute, so staying well inside it keeps a
/// long wait from surfacing as a tool failure.
const MAX_WAIT_MS: u64 = 30_000;

/// Default cap on how many lines one read returns.
const DEFAULT_READ_LINES: usize = 200;

/// Cap on a sidebar label derived from a command — a seeded script is sent to
/// the shell in full, but only its first line's worth belongs on a row.
const LABEL_MAX_CHARS: usize = 48;

/// Marks the end of a tracked command. `hub_terminal_send` appends a `printf` of
/// this form so a read can report completion and the exit code, instead of the
/// model having to guess from a lull in output — a linking build can be silent
/// for a minute mid-run.
const DONE_PREFIX: &str = "__MPX_DONE_";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn terminals_dir(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("terminals")
}

fn cursor_path(ctx: &Ctx, id: usize) -> PathBuf {
    terminals_dir(ctx)
        .join("cursors")
        .join(format!("{}.{}", id, ctx.instance))
}

fn mark_path(ctx: &Ctx, id: usize) -> PathBuf {
    terminals_dir(ctx).join(format!("{id}.mark"))
}

/// This instance's saved read position in terminal `id`, if it has read before.
fn cursor_of(ctx: &Ctx, id: usize) -> Option<u64> {
    std::fs::read_to_string(cursor_path(ctx, id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn set_cursor(ctx: &Ctx, id: usize, at: u64) {
    let path = cursor_path(ctx, id);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, at.to_string());
}

/// A consistent view of one terminal's transcript.
struct LogView {
    /// Logical offset of the first byte of `data`.
    base: u64,
    /// Logical offset one past the last byte — i.e. everything ever written.
    total: u64,
    data: String,
    idle_ms: u64,
    running: bool,
}

impl LogView {
    /// Read the log, retrying if a trim moved the data mid-read.
    ///
    /// The writer rewrites the header last, so a `base` that is the same before
    /// and after means the bytes in between belong to the offsets we think they
    /// do. Without this check a trim landing mid-read would silently return the
    /// wrong window of text — not an error, just a wrong answer.
    fn open(ctx: &Ctx, id: usize) -> Option<Self> {
        let path = terminals_dir(ctx).join(format!("{id}.log"));
        for _ in 0..4 {
            let before = termlog::parse_header(&read_prefix(&path, termlog::HEADER_LEN)?)?;
            let raw = std::fs::read(&path).ok()?;
            let after = termlog::parse_header(&read_prefix(&path, termlog::HEADER_LEN)?)?;
            if before.base != after.base || raw.len() < termlog::HEADER_LEN {
                continue;
            }
            let data = String::from_utf8_lossy(&raw[termlog::HEADER_LEN..]).into_owned();
            let total = before.base + (raw.len() - termlog::HEADER_LEN) as u64;
            return Some(Self {
                base: before.base,
                total,
                data,
                idle_ms: now_ms().saturating_sub(after.last_out_ms),
                running: !after.exited,
            });
        }
        None
    }
}

fn read_prefix(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// Post a request to Mulpex's poll loop and wait for its reply.
fn terminal_request(ctx: &Ctx, body: Value) -> Result<Value, String> {
    let dir = ctx.state_dir.join("termreq");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not reach Mulpex: {e}"))?;
    // The stamp leads the name so the poll loop's plain filename sort is time
    // order — two sends from one instance must arrive in the order they were
    // made (`cd somewhere` then `make` is not the same as the reverse).
    let token = format!(
        "{:020}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
        ctx.instance,
        new_uuid()
    );
    std::fs::write(dir.join(format!("{token}.json")), body.to_string())
        .map_err(|e| format!("could not reach Mulpex: {e}"))?;

    let done = dir.join(format!("{token}.done"));
    let deadline = now_ms() + TERM_REQ_TIMEOUT_MS;
    while now_ms() < deadline {
        if let Ok(content) = std::fs::read_to_string(&done) {
            let _ = std::fs::remove_file(&done);
            let v: Value = serde_json::from_str(&content)
                .map_err(|_| "Mulpex sent an unreadable reply".to_string())?;
            if v.get("ok").and_then(|x| x.as_bool()) == Some(true) {
                return Ok(v);
            }
            return Err(v
                .get("error")
                .and_then(|x| x.as_str())
                .unwrap_or("the request failed")
                .to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err("Mulpex did not respond — it may be shutting down.".to_string())
}

/// How long `hub_remote_open` will wait for the remote's TUI to appear before
/// handing the caller back an un-seeded terminal.
///
/// A cold `claude` over ssh took ~5 s in the reference capture; 30 s is the same
/// ceiling `hub_terminal_read`'s wait uses, chosen for the same reason (Claude
/// Code's own per-tool timeout). Timing out is not a failure — the terminal is
/// open and the caller is told to send the task itself.
const REMOTE_READY_TIMEOUT_MS: u64 = 30_000;

/// Open a terminal, ssh somewhere, and start a `claude` there that knows how to
/// call back.
///
/// The launch is the *only* moment the peer rules can be attached — they ride in
/// on `--append-system-prompt`, which is re-sent with every request and so
/// survives both a long conversation and compaction. There is deliberately no
/// way to adopt a remote claude someone started by hand: rules delivered as a
/// typed message would drift out of context, which is the failure this design
/// exists to avoid.
fn hub_remote_open(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let ssh_target = args
        .get("ssh_target")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let cwd = args
        .get("cwd")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let task = args
        .get("task")
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let existing = args
        .get("terminal_id")
        .and_then(|x| x.as_u64())
        .map(|n| n as usize);
    if existing.is_none() && ssh_target.is_none() {
        return Err("missing 'ssh_target' — e.g. \"root@10.0.0.5\" or an ~/.ssh/config alias. \
                    (It is only optional when you pass 'terminal_id' for a terminal that is \
                    already logged in to the remote machine.)"
            .into());
    }

    let token = remote::new_token(ctx.instance, now_ms());
    let rules_b64 = remote::b64(remote::peer_rules(&token).as_bytes());
    // With an ssh target, this terminal is on THIS machine and has to travel;
    // without one, the caller is telling us the terminal is already on the far
    // side, so only the `claude` half is launched. Both attach the rules at
    // launch, which is the only thing that actually matters.
    let command = match ssh_target {
        Some(target) => remote::ssh_command(target, cwd, &rules_b64),
        // No ssh hop means this terminal is the user's own remote session, so
        // the launch must NOT exec: their shell has to survive the claude.
        None => remote::remote_launch_command(cwd, &rules_b64, false),
    };

    // The command carries NO completion marker. `; printf …` marks the end of a
    // shell command, and this "command" is an interactive session that ends only
    // when the remote claude quits — tracking it would report completion at
    // exactly the wrong moment.
    let id = match existing {
        Some(id) => {
            launch_into_existing(ctx, id, &command)?;
            id
        }
        None => {
            let reply = terminal_request(
                ctx,
                json!({
                    "op": "open",
                    "from": ctx.instance,
                    "seed": command,
                    "label": format!("ssh {}", ssh_target.unwrap_or_default()),
                }),
            )?;
            let id = reply
                .get("id")
                .and_then(|x| x.as_u64())
                .ok_or("Mulpex did not report the new terminal's id")? as usize;
            // A terminal you opened is one whose entire life you should see.
            // A terminal the USER opened already has a history that is theirs,
            // so its cursor is left where it is.
            set_cursor(ctx, id, 0);
            id
        }
    };

    remote::RemoteMeta {
        token: token.clone(),
        ssh_target: ssh_target.unwrap_or_default().to_string(),
        opener: ctx.instance,
    }
    .write(&ctx.state_dir, id)
    .map_err(|e| format!("could not record the remote terminal: {e}"))?;

    clear_mark(ctx, id);

    // Wait for the remote's input box before typing the task in. This is the
    // same problem `pty.rs` solves for spawned local instances, and for the same
    // reason: nothing in the byte stream announces "the TUI is ready", and text
    // typed before it is simply dropped — leaving a remote with no task and no
    // way to be given one, since it never takes a first turn.
    let mut ready = false;
    if task.is_some() {
        let deadline = now_ms() + REMOTE_READY_TIMEOUT_MS;
        while now_ms() < deadline {
            let screen = std::fs::read_to_string(terminals_dir(ctx).join(format!("{id}.screen")))
                .unwrap_or_default();
            if remote::looks_like_claude_tui(&screen) {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if ready {
            ready = inject_task(ctx, id, task.unwrap_or_default())?;
        }
    }

    Ok(json!({
        "ok": true,
        "terminal_id": id,
        "ssh_target": ssh_target,
        "task_sent": task.is_some() && ready,
        "task_delivered": task.is_some() && ready,
        "note": match (task.is_some(), ready) {
            (true, true) =>
                "Remote claude started and your task was sent to it. It will signal when it is \
                 done, blocked, or has a question — you will be woken with a hub message, so you \
                 do NOT need to poll. To watch it anyway, use hub_terminal_read with wait_ms.",
            (true, false) =>
                "Terminal opened, but the task could not be confirmed as started — the remote's \
                 prompt may not have appeared in time, or ssh may be asking something. Call \
                 hub_terminal_read(id) to see what state it is in before re-sending.",
            _ =>
                "Remote claude is starting. Give it a task with hub_terminal_send; it will signal \
                 when done, blocked, or asking, and you will be woken by a hub message.",
        },
    })
    .to_string())
}

/// Launch into a terminal that already exists, refusing if it is not free.
///
/// Typing into a busy terminal is the same class of mistake as appending
/// `; printf …` to a heredoc terminator: the text is not a command line, it is
/// input to whatever is running, and it will be consumed as such. Three ways it
/// can be unfree, and each gets its own message because the fix differs:
/// something is running, a claude is already there, or the shell exited.
///
/// Prompt detection is a heuristic — no shell announces its prompt — so it is
/// paired with a genuine idleness check rather than trusted on its own.
fn launch_into_existing(ctx: &Ctx, id: usize, command: &str) -> Result<(), String> {
    let view = LogView::open(ctx, id)
        .ok_or_else(|| format!("no terminal #{id} — call hub_instances to see the live ones."))?;
    if !view.running {
        return Err(format!(
            "terminal #{id} has exited. Open a new one, or call hub_remote_open without \
             'terminal_id' and it will make its own."
        ));
    }
    let screen = std::fs::read_to_string(terminals_dir(ctx).join(format!("{id}.screen")))
        .unwrap_or_default();
    if remote::looks_like_claude_tui(&screen) {
        return Err(format!(
            "terminal #{id} already has a Claude Code session running in it. Starting another \
             inside it would type into that one's prompt. Use hub_terminal_send to talk to it, or \
             call hub_remote_open without 'terminal_id' for a fresh terminal."
        ));
    }
    // Two independent ways to be sure it is free, because prompt detection is a
    // heuristic and prompt themes are endless: it is producing no output, AND it
    // either looks like a prompt or has been quiet long enough that whatever ran
    // is plainly over. Without the second clause an unrecognised prompt theme
    // makes the tool permanently refuse a terminal that is perfectly idle.
    const SETTLED_MS: u64 = 750;
    const UNRECOGNISED_GRACE_MS: u64 = 3_000;
    let quiet = view.idle_ms >= SETTLED_MS;
    let ready = remote::at_shell_prompt(&screen) || view.idle_ms >= UNRECOGNISED_GRACE_MS;
    if !quiet || !ready {
        let last = screen
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("(blank)")
            .trim()
            .chars()
            .take(80)
            .collect::<String>();
        return Err(format!(
            "terminal #{id} does not look like it is sitting at a shell prompt — something may \
             still be running in it, and the launch command would be typed into that instead of \
             into a shell. Its last line is: {last:?}. Wait for it to finish (hub_terminal_read \
             with wait_ms), or call hub_remote_open without 'terminal_id'."
        ));
    }
    terminal_request(
        ctx,
        json!({ "op": "send", "from": ctx.instance, "id": id,
                "data": format!("{command}\r") }),
    )?;
    Ok(())
}

/// How many times to try getting a task into a remote claude's input box.
const INJECT_ATTEMPTS: usize = 3;

/// Type a task into a remote claude and confirm it actually started a turn.
///
/// Three things here are load-bearing, and all three were measured against the
/// real remote rather than assumed:
///
/// 1. **The `\r` must be a separate write.** Sent as the tail of the same burst
///    as the text, Claude Code treats it as *paste content* rather than as
///    Enter: the task lands in the input box, sits there fully typed, and is
///    never submitted. That is precisely what the first live run did — the
///    driver then waited on a remote that had been given a task it had not read.
///    `pty.rs` documents the same rule for locally spawned instances.
/// 2. **Submission is verified, not assumed.** A remote that is still finishing
///    its startup silently drops what it is given, so the reply is only honest
///    if something confirms the turn began. The spinner is that proof: it
///    animates continuously while a turn runs.
/// 3. **A retry clears the box first** (Ctrl-U), or a half-landed attempt
///    concatenates with the next one into gibberish.
fn inject_task(ctx: &Ctx, id: usize, task: &str) -> Result<bool, String> {
    for attempt in 0..INJECT_ATTEMPTS {
        if attempt > 0 {
            // Ctrl-U: discard whatever the previous attempt left behind.
            terminal_request(
                ctx,
                json!({ "op": "send", "from": ctx.instance, "id": id, "data": "\x15" }),
            )?;
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        terminal_request(
            ctx,
            json!({ "op": "send", "from": ctx.instance, "id": id, "data": task }),
        )?;
        std::thread::sleep(std::time::Duration::from_millis(400));
        terminal_request(
            ctx,
            json!({ "op": "send", "from": ctx.instance, "id": id, "data": "\r" }),
        )?;

        let deadline = now_ms() + 8_000;
        while now_ms() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(250));
            let screen = std::fs::read_to_string(terminals_dir(ctx).join(format!("{id}.screen")))
                .unwrap_or_default();
            // Either it is visibly working, or it already finished and said so —
            // a very short turn can be over before the first poll.
            let signalled = remote::RemoteMeta::read(&ctx.state_dir, id)
                .is_some_and(|m| !remote::find_signals(&screen, &m.token).is_empty());
            if remote::has_spinner(&screen) || signalled {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn hub_terminal_open(ctx: &Ctx, args: &Value) -> Result<String, String> {
    // The command goes to the shell **verbatim** — collapsing its whitespace
    // silently rewrites a script before it ever runs. Only the sidebar *label*
    // gets flattened, because that is the one place a newline is wrong.
    let command = args
        .get("command")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    let label = args
        .get("name")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .or_else(|| command.clone())
        .map(|s| flatten_label(&s));

    let (seed, mark) = match command.as_deref() {
        Some(c) => {
            let (seed, mark) = seed_and_mark(c, now_ms());
            (Some(seed), mark)
        }
        None => (None, None),
    };

    let reply = terminal_request(
        ctx,
        json!({ "op": "open", "from": ctx.instance, "seed": seed, "label": label }),
    )?;
    let id = reply
        .get("id")
        .and_then(|x| x.as_u64())
        .ok_or("Mulpex did not report the new terminal's id")? as usize;

    // Start this instance's cursor at the very beginning: a terminal you opened
    // yourself is one whose entire life you should see on your first read.
    set_cursor(ctx, id, 0);
    match mark {
        Some(mark) => {
            let _ = std::fs::write(mark_path(ctx, id), mark.to_string());
        }
        // Nothing to report completion for; make sure no earlier mark can be
        // mistaken for this terminal's command.
        None => clear_mark(ctx, id),
    }
    Ok(json!({
        "ok": true,
        "terminal_id": id,
        "note": if command.is_some() {
            "Terminal opened and the command is starting. Use hub_terminal_read (with wait_ms to \
             block until there is output) to follow it; the shell stays open when it finishes."
        } else {
            "Terminal opened at a shell prompt. Use hub_terminal_send to run something in it."
        },
    })
    .to_string())
}

/// What to type into a new terminal, and the token to track it by (if any).
///
/// The command itself is passed through **verbatim** — its whitespace is the
/// caller's, and collapsing it silently rewrites a script before it ever runs.
/// Tracking is skipped for multi-line input for the same reason as in
/// `hub_terminal_send`: the marker would land on a heredoc's terminator.
fn seed_and_mark(command: &str, now: u64) -> (String, Option<u64>) {
    if command.contains('\n') {
        (command.to_string(), None)
    } else {
        (with_completion_marker(command, now), Some(now))
    }
}

/// One line, for a sidebar row. The *command* is never put through this — see
/// `hub_terminal_open`.
fn flatten_label(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(LABEL_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &one[..cut]),
        None => one,
    }
}

/// Wrap a command so the shell announces its exit status on a line of its own.
fn with_completion_marker(command: &str, token: u64) -> String {
    format!("{command}; printf '\\n{DONE_PREFIX}{token}_%s__\\n' \"$?\"")
}

fn hub_terminal_send(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|x| x.as_u64())
        .ok_or("missing 'id' — the terminal to type into.")? as usize;
    let input = args.get("input").and_then(|x| x.as_str()).unwrap_or("");
    let submit = args
        .get("submit")
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    let control = args.get("control").and_then(|x| x.as_str()).unwrap_or("");
    if input.is_empty() && control.is_empty() {
        return Err("nothing to send — provide 'input' (text) or 'control' (a control key).".into());
    }

    // Only wrap the input in a completion marker when the terminal is genuinely
    // sitting at a shell prompt. If a command we're tracking is still running,
    // this text is an *answer* to that command, not a new command line, and
    // appending `; printf …` to it would feed the running program nonsense.
    let idle = LogView::open(ctx, id).map(|v| {
        let tracked = tracked_token(ctx, id);
        match tracked {
            Some(t) => find_completion(&v.data, t).is_some(),
            None => true,
        }
    });
    let is_remote = remote::RemoteMeta::read(&ctx.state_dir, id).is_some();
    let action = mark_action(submit, input, control, idle, is_remote);
    let track = action == Mark::Track;

    let mut data = String::new();
    let mark = now_ms();
    if !input.is_empty() {
        if track {
            data.push_str(&with_completion_marker(input, mark));
        } else {
            data.push_str(input);
        }
        if submit {
            data.push('\r');
        }
    }
    if !control.is_empty() {
        data.push_str(&control_bytes(control)?);
    }
    match action {
        Mark::Track => {
            let _ = std::fs::write(mark_path(ctx, id), mark.to_string());
        }
        Mark::Clear => clear_mark(ctx, id),
        Mark::Keep => {}
    }

    terminal_request(
        ctx,
        json!({ "op": "send", "from": ctx.instance, "id": id, "data": data }),
    )?;
    Ok(json!({
        "ok": true,
        "terminal_id": id,
        "tracking_completion": track,
        "note": if track {
            "Sent. Call hub_terminal_read (wait_ms is useful here) to see the output; it will tell \
             you when this command finishes and its exit code."
        } else {
            "Sent. Call hub_terminal_read to see what happened."
        },
    })
    .to_string())
}

/// Translate a named control key into the bytes a terminal expects.
fn control_bytes(name: &str) -> Result<String, String> {
    let n = name.trim().to_ascii_lowercase();
    Ok(match n.as_str() {
        "enter" | "return" | "cr" => "\r".to_string(),
        "escape" | "esc" => "\x1b".to_string(),
        "tab" => "\t".to_string(),
        "up" => "\x1b[A".to_string(),
        "down" => "\x1b[B".to_string(),
        "right" => "\x1b[C".to_string(),
        "left" => "\x1b[D".to_string(),
        _ => {
            let mut chars = n.chars();
            match (chars.next(), chars.next()) {
                // Ctrl-<letter> is the letter's position in the alphabet.
                (Some(c), None) if c.is_ascii_alphabetic() => {
                    ((c.to_ascii_uppercase() as u8 - b'A' + 1) as char).to_string()
                }
                _ => {
                    return Err(format!(
                        "unknown control key {name:?} — use a single letter for Ctrl-<letter> \
                         (e.g. \"c\"), or one of enter/escape/tab/up/down/left/right."
                    ))
                }
            }
        }
    })
}

/// What a send does about the terminal's tracked-command mark.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mark {
    /// Wrap this command and record it: a read may report its exit code.
    Track,
    /// Leave the existing mark: it names a command that is still running, and
    /// this input is an answer to *it*.
    Keep,
    /// Forget the mark, so no read can report a completion for it.
    Clear,
}

/// The rule, given whether the terminal is sitting at a prompt (`idle`).
///
/// Two of the three arms are bug fixes:
///
/// - **Multi-line is never tracked.** The marker is appended to the end of the
///   whole string, so on multi-line input it lands on the last line — and when
///   that line is a heredoc's terminator, `PY` becomes `PY; printf …`, which no
///   longer terminates it and leaves the shell stuck in `>` continuation until
///   someone sends Ctrl-C. The cost is no exit code for heredocs.
/// - **An untracked send at a prompt clears the mark.** Otherwise the mark still
///   names an *earlier*, already-completed command, and the next read answers
///   `command_finished: true` with that old exit code about the thing we just
///   sent — a wrong answer rather than an error, which a model will branch on.
///   Observed reporting exit 0 while a `cd … && ls && cat` was mid-flight.
///
/// An interrupt clears too: it aborts the command *and* the `; printf …` after
/// it, so the marker will never be printed, and a dangling mark would leave the
/// terminal reading as permanently not-idle — nothing sent afterwards could ever
/// be tracked again.
fn mark_action(submit: bool, input: &str, control: &str, idle: Option<bool>, remote: bool) -> Mark {
    // There is no shell on the other end of a remote terminal — there is a
    // claude. Wrapping input in `; printf …` would not measure anything; it
    // would append shell plumbing to the END OF THE PROMPT the remote claude
    // reads, so it receives the task plus a line of gibberish. Found in the
    // field: a driver's message arrived with the completion sentinel glued to
    // it. Nothing about a claude TUI distinguishes it from a shell prompt in
    // the transcript, so the terminal's own kind has to say so.
    if remote {
        return Mark::Clear;
    }
    if aborts_running_command(control) {
        return Mark::Clear;
    }
    let trackable = submit && !input.is_empty() && !input.contains('\n');
    match (trackable, idle) {
        (true, Some(true)) => Mark::Track,
        // Not idle, or unknown: a mark that exists belongs to something still
        // running, and is still worth reporting.
        (_, Some(true)) => Mark::Clear,
        _ => Mark::Keep,
    }
}

/// Ctrl-C / Ctrl-\ / Ctrl-Z / Ctrl-D: the running command goes away without
/// reaching the `printf` that would announce its exit status.
fn aborts_running_command(control: &str) -> bool {
    matches!(
        control.trim().to_ascii_lowercase().as_str(),
        "c" | "d" | "z" | "\\"
    )
}

/// Forget any tracked command, so no read can report a completion for it.
fn clear_mark(ctx: &Ctx, id: usize) {
    let _ = std::fs::remove_file(mark_path(ctx, id));
}

/// The token of the most recently tracked command in this terminal.
fn tracked_token(ctx: &Ctx, id: usize) -> Option<u64> {
    std::fs::read_to_string(mark_path(ctx, id))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Find `token`'s completion marker in `text`, returning its exit code.
fn find_completion(text: &str, token: u64) -> Option<i64> {
    let needle = format!("{DONE_PREFIX}{token}_");
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&needle) {
            if let Some(code) = rest.strip_suffix("__") {
                return code.trim().parse().ok();
            }
        }
    }
    None
}

/// Remove the completion-marker plumbing from text shown to the model: both the
/// marker line the shell printed and the `; printf …` tail on the echoed command
/// line. It's bookkeeping, not output.
fn strip_markers(text: &str) -> String {
    let without_tails = strip_echoed_tails(text);
    let mut out = String::with_capacity(without_tails.len());
    for line in without_tails.lines() {
        // The marker the shell *printed*. Always on a line of its own and only
        // ~25 characters, so it never wraps.
        if line.trim().starts_with(DONE_PREFIX) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Remove the `; printf '\n__MPX_DONE_…' "$?"` tail the shell echoes back after
/// the command.
///
/// Newline-tolerant on purpose: the tail is ~47 characters, so any command
/// longer than about half the terminal width makes the echoed line **wrap**, and
/// the grid turns that wrap into a real newline — landing anywhere inside the
/// tail, including in the middle of `__MPX_DONE_` itself. A plain per-line
/// `find` therefore misses it entirely on long commands and leaves half of it
/// behind on medium ones. Swallowing the wrap newline along with the span also
/// rejoins the echoed command into the single line it was before wrapping.
fn strip_echoed_tails(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ';' {
            if let Some(end) = echoed_tail_end(&chars, i) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// If a (possibly wrapped) marker tail starts at `i`, the index just past it.
fn echoed_tail_end(chars: &[char], i: usize) -> Option<usize> {
    /// Cap the search so a stray `; printf '` in real output can't run away.
    const MAX_TAIL: usize = 200;

    let mut j = match_ignoring_newlines(chars, i, "; printf '")?;
    let mut payload = String::new();
    while j < chars.len() && payload.len() < MAX_TAIL {
        if let Some(end) = match_ignoring_newlines(chars, j, "\"$?\"") {
            // Only ours: a command that genuinely contains `; printf '…"$?"` is
            // left exactly as the user wrote it.
            return payload.contains(DONE_PREFIX).then_some(end);
        }
        if chars[j] != '\n' {
            payload.push(chars[j]);
        }
        j += 1;
    }
    None
}

/// Match `pat` at `i`, tolerating newlines the terminal inserted mid-sequence.
fn match_ignoring_newlines(chars: &[char], mut i: usize, pat: &str) -> Option<usize> {
    for (n, pc) in pat.chars().enumerate() {
        // Only *inside* the pattern — a leading newline means this isn't a match
        // starting here, it's the next line.
        if n > 0 {
            while chars.get(i) == Some(&'\n') {
                i += 1;
            }
        }
        if chars.get(i) != Some(&pc) {
            return None;
        }
        i += 1;
    }
    Some(i)
}

/// Keep at most `max` lines, the most recent ones. Returns whether it cut.
fn tail_lines(text: &str, max: usize) -> (String, bool) {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return (text.to_string(), false);
    }
    let kept = lines[lines.len() - max..].join("\n");
    (format!("{kept}\n"), true)
}

fn hub_terminal_read(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|x| x.as_u64())
        .ok_or("missing 'id' — the terminal to read.")? as usize;
    let full = args.get("full").and_then(|x| x.as_bool()).unwrap_or(false);
    let max_lines = args
        .get("lines")
        .and_then(|x| x.as_u64())
        .map(|n| (n as usize).clamp(1, 5_000))
        .unwrap_or(DEFAULT_READ_LINES);
    let wait_ms = args
        .get("wait_ms")
        .and_then(|x| x.as_u64())
        .unwrap_or(0)
        .min(MAX_WAIT_MS);

    let mut view = LogView::open(ctx, id)
        .ok_or_else(|| format!("no terminal #{id} — call hub_instances to see the live ones."))?;
    let cursor = cursor_of(ctx, id);
    let first_read = cursor.is_none();
    let token = tracked_token(ctx, id);
    // A remote claude is watched differently from a shell command: there is no
    // exit code to wait for, and "finished" is something the remote says (or,
    // when it forgets to, something its silence implies).
    let remote_meta = remote::RemoteMeta::read(&ctx.state_dir, id);
    let screen_path = terminals_dir(ctx).join(format!("{id}.screen"));
    let read_screen = || std::fs::read_to_string(&screen_path).unwrap_or_default();

    // Blocking wait. What it waits *for* depends on whether a command of yours
    // is in flight: with one running, "something new" means that command
    // finishing — returning on its first byte of output is what made nearly
    // every wait in real use come back mid-command. With nothing tracked there
    // is no completion to wait for, so any new output ends the wait. `waited_for`
    // reports which, so a caller is never guessing. `full` no longer disables
    // this: what you read and how long you block are unrelated questions.
    // Where this read will start from. Recomputed as we wait, because a trim can
    // move `base` out from under a cursor.
    let effective_start = |v: &LogView| -> u64 {
        if full || first_read {
            v.base
        } else {
            cursor.unwrap_or(v.base).max(v.base)
        }
    };

    let mut timed_out = false;
    let mut waited_for = None;
    if wait_ms > 0 {
        let pending = token.filter(|&t| find_completion(&view.data, t).is_none());
        waited_for = Some(match (&remote_meta, pending.is_some()) {
            (Some(_), _) => "remote_signal",
            (None, true) => "completion",
            (None, false) => "output",
        });
        let deadline = now_ms() + wait_ms;
        while view.running {
            let satisfied = match (&remote_meta, pending) {
                // Wait for the remote to hand the turn back: either it signals,
                // or it falls silent, which for a claude means its spinner
                // stopped animating and the turn is over. Waiting merely for
                // *output* here would return on the first frame of its thinking
                // animation, which is never what the caller meant.
                (Some(m), _) => {
                    let start = effective_start(&view);
                    let slice = &view.data
                        [((start - view.base) as usize).min(view.data.len())..];
                    !remote::find_signals(slice, &m.token).is_empty()
                        || !remote::find_signals(&read_screen(), &m.token).is_empty()
                        || (view.total > start && view.idle_ms >= remote::IDLE_TURN_END_MS)
                }
                (None, Some(t)) => find_completion(&view.data, t).is_some(),
                // Anything this reader has not seen counts — including output
                // that arrived before the call. Waiting only for *further*
                // growth would sit on output already in hand.
                (None, None) => view.total > effective_start(&view),
            };
            if satisfied {
                break;
            }
            if now_ms() >= deadline {
                timed_out = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            // A vanished log means the terminal was closed while we waited.
            let Some(next) = LogView::open(ctx, id) else {
                return Err(format!("terminal #{id} was closed while you were waiting."));
            };
            view = next;
        }
    }

    let start = effective_start(&view);
    let dropped = match cursor {
        Some(c) if c < view.base => view.base - c,
        _ => 0,
    };
    let slice = &view.data[((start - view.base) as usize).min(view.data.len())..];
    let text = strip_markers(slice);
    // Take the signal from the log slice if it is there, else from the screen: a
    // remote whose output has not yet scrolled has its marker on screen and
    // nowhere else, and that is the common case for a short reply.
    let signal = remote_meta.as_ref().and_then(|m| {
        remote::find_signals(&text, &m.token)
            .into_iter()
            .last()
            .or_else(|| remote::find_signals(&read_screen(), &m.token).into_iter().last())
    });
    // The marker is Mulpex's wire protocol. Showing it to the reader invites it
    // to imitate it, and imitating it would let a local instance forge a wake.
    let text = match &remote_meta {
        Some(m) => remote::strip_signals(&text, &m.token),
        None => text,
    };
    // A first read of someone else's terminal returns the tail, not the whole
    // retained megabyte — but it says so, so the model knows it's mid-story.
    let (text, truncated) = tail_lines(&text, max_lines);

    // A remote terminal has no shell command to finish; `remote_signal` is how
    // its turn ends. Reporting `command_finished` there would be a wrong answer,
    // not a missing one.
    let finished = token
        .filter(|_| remote_meta.is_none())
        .and_then(|t| find_completion(&view.data, t));
    // The screen gets the same treatment as `new_output`: it is read straight
    // off disk, so without this every screen read carries the visible
    // `; printf '…__MPX_DONE_…' "$?"` plumbing. That matters more than it looks
    // — a caller working around history lag by prefixing `clear;` makes the
    // screen its primary channel.
    let screen = strip_markers(&read_screen());
    let screen = match &remote_meta {
        Some(m) => remote::strip_signals(&screen, &m.token),
        None => screen,
    };

    // A wait that timed out with nothing new must not consume anything: the
    // caller should be able to retry and still see it.
    if !(timed_out && text.trim().is_empty()) {
        set_cursor(ctx, id, view.total);
    }

    let mut out = json!({
        "ok": true,
        "terminal_id": id,
        "new_output": text,
        "current_screen": screen,
        "idle_ms": view.idle_ms,
        "running": view.running,
    });
    let map = out.as_object_mut().unwrap();
    if let Some(code) = finished {
        map.insert("command_finished".into(), json!(true));
        map.insert("exit_code".into(), json!(code));
    }
    if let Some(m) = &remote_meta {
        map.insert("remote_claude".into(), json!(true));
        map.insert("ssh_target".into(), json!(m.ssh_target));
        match &signal {
            Some(sig) => {
                map.insert("remote_signal".into(), json!(sig.kind.as_str()));
                map.insert("remote_summary".into(), json!(sig.summary));
                remote::take_if_new(&ctx.state_dir, id, &ctx.instance.to_string(), sig);
            }
            // Silence is the other half of the answer: a claude that has stopped
            // producing output has ended its turn, whether or not it remembered
            // to say so.
            None if view.idle_ms >= remote::IDLE_TURN_END_MS => {
                map.insert("remote_idle".into(), json!(true));
            }
            None => {
                map.insert("remote_working".into(), json!(true));
            }
        }
    }
    if first_read && !full {
        map.insert("first_read".into(), json!(true));
    }
    if truncated {
        map.insert("truncated".into(), json!(true));
        map.insert(
            "note".into(),
            json!(format!(
                "Only the last {max_lines} lines are shown. Raise 'lines' for more."
            )),
        );
    }
    if dropped > 0 {
        map.insert("dropped_bytes".into(), json!(dropped));
        map.insert(
            "dropped_note".into(),
            json!("Output produced before this scrolled out of the retained history."),
        );
    }
    if let Some(what) = waited_for {
        map.insert("waited_for".into(), json!(what));
    }
    if timed_out {
        map.insert("timed_out".into(), json!(true));
        if waited_for == Some("completion") {
            map.insert(
                "timed_out_note".into(),
                json!("The command you submitted is still running. Anything it printed is in \
                       new_output; call again to keep waiting."),
            );
        }
    }
    Ok(out.to_string())
}

fn hub_terminal_close(ctx: &Ctx, args: &Value) -> Result<String, String> {
    let id = args
        .get("id")
        .and_then(|x| x.as_u64())
        .ok_or("missing 'id' — the terminal to close.")? as usize;
    terminal_request(ctx, json!({ "op": "close", "from": ctx.instance, "id": id }))?;
    Ok(json!({ "ok": true, "terminal_id": id, "note": "Terminal closed." }).to_string())
}

// ---- shared readers (also used by the UserPromptSubmit hook) --------------

/// A compact, human-readable snapshot of the OTHER instances + my unread count,
/// injected into each turn by the `userpromptsubmit` hook. `None` when there's
/// nothing worth saying (no peers and no unread messages).
pub(crate) fn peers_context(ctx: &Ctx) -> Option<String> {
    let holds = locks_by_holder(ctx);
    let peers: Vec<usize> = live_ids(ctx).into_iter().filter(|&id| id != ctx.instance).collect();
    let unread = unread_for(ctx, ctx.instance);
    if peers.is_empty() && unread == 0 {
        return None;
    }

    let mut s = String::from("[Mulpex hub] You are one of several parallel Claude instances in this directory.");
    if !peers.is_empty() {
        s.push_str(" Other instances right now:");
        for id in peers {
            let task = task_of(ctx, id);
            let held = holds.get(&id).cloned().unwrap_or_default();
            s.push_str(&format!("\n  - claude #{id} [{}]", status_of(ctx, id)));
            if !task.is_empty() {
                s.push_str(&format!(" task: \"{task}\""));
            }
            if !held.is_empty() {
                s.push_str(&format!(" holds: {}", held.join(", ")));
            }
        }
    }
    if unread > 0 {
        s.push_str(&format!(
            "\nYou have {unread} unread hub message(s) — call mcp__mulpex__hub_inbox to read them."
        ));
    }
    Some(s)
}

/// Live *peer* ids — every live instance except this one. Used by `hub_send`
/// (the `all` fan-out and recipient validation) and by the hook's departed-peer
/// detection (`hook::departed_peers`).
pub(crate) fn peer_ids(ctx: &Ctx) -> Vec<usize> {
    live_ids(ctx).into_iter().filter(|&id| id != ctx.instance).collect()
}

/// Live instance ids. Authoritative source is `state_dir/instances` (written by
/// `App` as the instance set changes); falls back to scanning the integer-named
/// status files if that's missing.
fn live_ids(ctx: &Ctx) -> Vec<usize> {
    if let Ok(content) = std::fs::read_to_string(ctx.state_dir.join("instances")) {
        let mut ids: Vec<usize> = content.lines().filter_map(|l| l.trim().parse().ok()).collect();
        ids.sort_unstable();
        if !ids.is_empty() {
            return ids;
        }
    }
    let mut ids = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&ctx.state_dir) {
        for e in entries.flatten() {
            if let Some(id) = e.file_name().to_str().and_then(|n| n.parse::<usize>().ok()) {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    ids
}

fn status_of(ctx: &Ctx, id: usize) -> String {
    std::fs::read_to_string(ctx.state_dir.join(id.to_string()))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "waiting".to_string())
}

fn task_of(ctx: &Ctx, id: usize) -> String {
    std::fs::read_to_string(ctx.tasks_dir.join(id.to_string()))
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn unread_for(ctx: &Ctx, id: usize) -> usize {
    std::fs::read_dir(ctx.inbox_dir.join(id.to_string()))
        .map(|d| d.flatten().count())
        .unwrap_or(0)
}

/// `holder id → basenames of the files it currently locks`.
fn locks_by_holder(ctx: &Ctx) -> HashMap<usize, Vec<String>> {
    let mut map: HashMap<usize, Vec<String>> = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(&ctx.locks_dir) {
        for e in entries.flatten() {
            let file = e.path();
            let (Some(holder), Some(path)) = (
                read_field(&file, "instance").and_then(|s| s.parse::<usize>().ok()),
                read_field(&file, "path"),
            ) else {
                continue;
            };
            let name = Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or(path);
            map.entry(holder).or_default().push(name);
        }
    }
    for v in map.values_mut() {
        v.sort();
    }
    map
}

/// Read and remove every message addressed to `id`. Returns `(ts, from, body)`.
/// A message's sender, as the recipient should see it.
///
/// Almost always a peer instance. The exception is a **remote claude**, which
/// has no instance id at all — the app writes its wake on its behalf and tags it
/// with the terminal it came from, so the recipient is never told to reply to a
/// peer number that does not exist.
fn sender_label(v: &Value) -> String {
    match v.get("from_terminal").and_then(|x| x.as_u64()) {
        Some(t) => format!("terminal #{t} (remote claude)"),
        None => format!("#{}", v.get("from").and_then(|x| x.as_u64()).unwrap_or(0)),
    }
}

fn take_inbox(ctx: &Ctx, id: usize) -> Vec<(u64, String, String)> {
    let dir = ctx.inbox_dir.join(id.to_string());
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let file = e.path();
            if let Ok(content) = std::fs::read_to_string(&file) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    let ts = v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0);
                    let from = sender_label(&v);
                    let body = v.get("body").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    out.push((ts, from, body));
                }
            }
            let _ = std::fs::remove_file(&file);
        }
    }
    out
}

/// One-line task summary: collapse whitespace and cap the length.
pub(crate) fn summarize(prompt: &str) -> String {
    let one_line = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = one_line.chars().take(140).collect();
    if one_line.chars().count() > 140 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_marker_round_trips() {
        let cmd = with_completion_marker("cargo build", 1764000000123);
        assert!(cmd.starts_with("cargo build; printf "));
        // What the shell actually prints once the command exits.
        let output = "Compiling…\n__MPX_DONE_1764000000123_101__\n";
        assert_eq!(find_completion(output, 1764000000123), Some(101));
        // A marker from an earlier command must not be mistaken for this one's.
        assert_eq!(find_completion(output, 1764000000999), None);
    }

    #[test]
    fn markers_are_stripped_from_what_the_model_sees() {
        let raw = concat!(
            "$ cargo build; printf '\\n__MPX_DONE_7_%s__\\n' \"$?\"\n",
            "   Compiling mulpex v0.5.0\n",
            "__MPX_DONE_7_0__\n",
        );
        assert_eq!(strip_markers(raw), "$ cargo build\n   Compiling mulpex v0.5.0\n");
    }

    #[test]
    fn strip_markers_leaves_ordinary_output_alone() {
        let raw = "one\ntwo\nthree\n";
        assert_eq!(strip_markers(raw), raw);
    }

    /// The echoed tail is ~47 chars, so a command longer than about half the
    /// terminal width wraps — and the grid turns that wrap into a real newline,
    /// which can land anywhere inside the tail, `__MPX_DONE_` included.
    #[test]
    fn a_wrapped_echoed_tail_is_still_stripped_whole() {
        let cmd = "npm run build -- --mode production --outDir dist/deep/output/path";
        let tail = with_completion_marker(cmd, 7);
        let full = format!("{tail}\nsome output\n__MPX_DONE_7_0__\n");

        // Wrap at every possible position inside the tail, which is what a
        // narrower or wider terminal each amount to.
        for at in cmd.len()..tail.len() {
            let mut wrapped: String = full.clone();
            wrapped.insert(at, '\n');
            let got = strip_markers(&wrapped);
            assert!(
                !got.contains("printf") && !got.contains("$?") && !got.contains(DONE_PREFIX),
                "wrap at {at} left plumbing behind: {got:?}"
            );
            assert!(got.contains("some output"), "wrap at {at} ate real output: {got:?}");
        }
    }

    #[test]
    fn a_command_that_genuinely_uses_printf_is_left_alone() {
        let raw = "$ true; printf 'exit=%s\\n' \"$?\"\nexit=0\n";
        assert_eq!(strip_markers(raw), raw);
    }

    #[test]
    fn tail_lines_keeps_the_most_recent() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(tail_lines(text, 10), (text.to_string(), false));
        assert_eq!(tail_lines(text, 2), ("c\nd\n".to_string(), true));
    }

    #[test]
    fn control_keys_map_to_the_expected_bytes() {
        assert_eq!(control_bytes("c").unwrap(), "\u{3}"); // Ctrl-C
        assert_eq!(control_bytes("D").unwrap(), "\u{4}"); // Ctrl-D, case-insensitive
        assert_eq!(control_bytes("enter").unwrap(), "\r");
        assert_eq!(control_bytes("escape").unwrap(), "\u{1b}");
        assert_eq!(control_bytes("up").unwrap(), "\u{1b}[A");
        assert!(control_bytes("f13").is_err());
    }

    // -- the tracked-command mark ------------------------------------------

    /// The wrong-exit-code bug: an untracked send at a prompt must retire the
    /// previous command's mark, or the next read reports *its* completion as if
    /// it were this send's.
    #[test]
    fn an_untracked_send_at_a_prompt_retires_the_old_mark() {
        let idle = Some(true);
        // Text left sitting at the prompt (submit=false).
        assert_eq!(mark_action(false, "ls", "", idle, false), Mark::Clear);
        // A control key on its own.
        assert_eq!(mark_action(true, "", "enter", idle, false), Mark::Clear);
        // Multi-line, which is never tracked.
        assert_eq!(mark_action(true, "cat <<'PY'\nx\nPY", "", idle, false), Mark::Clear);
        // The ordinary case still tracks.
        assert_eq!(mark_action(true, "ls -la", "", idle, false), Mark::Track);
    }

    /// A mark for a command that is still *running* stays: the input is an
    /// answer to that command, and its completion is still worth reporting.
    #[test]
    fn answering_a_running_command_keeps_its_mark() {
        assert_eq!(mark_action(true, "y", "", Some(false), false), Mark::Keep);
        assert_eq!(mark_action(true, "y", "", None, false), Mark::Keep);
    }

    /// An interrupt kills the `; printf …` too, so the marker never arrives.
    /// Left in place, the mark would make the terminal read as permanently
    /// not-idle and nothing could ever be tracked again.
    #[test]
    fn an_interrupt_retires_the_mark() {
        assert_eq!(mark_action(true, "", "c", Some(false), false), Mark::Clear);
        assert_eq!(mark_action(true, "", "d", Some(false), false), Mark::Clear);
        // A cursor key is not an abort.
        assert_eq!(mark_action(true, "", "up", Some(false), false), Mark::Keep);
    }

    /// Multi-line input is passed through untouched: appending the marker to a
    /// heredoc turns its terminator into `PY; printf …`, which no longer
    /// terminates it and hangs the shell in `>` continuation.
    #[test]
    fn a_multi_line_seed_is_never_wrapped() {
        let script = "cat <<'PY'\nprint('hi')\nPY";
        let (seed, mark) = seed_and_mark(script, 7);
        assert_eq!(seed, script);
        assert_eq!(mark, None);
    }

    /// A seeded command reaches the shell exactly as written — collapsing its
    /// whitespace silently rewrites it before it ever runs.
    #[test]
    fn a_seeded_command_keeps_its_whitespace() {
        let cmd = "awk '{ print $1,   $3 }' file.txt";
        let (seed, mark) = seed_and_mark(cmd, 7);
        assert!(seed.starts_with(cmd), "command was rewritten: {seed:?}");
        assert_eq!(mark, Some(7));
    }

    /// `hub_set_name` leaves the two files Mulpex and the hook read: the rename
    /// request, and the flag that stops the naming nudge. The flag is written
    /// here rather than when the rename lands, because it must also stop the
    /// nudge for a request Mulpex *refuses* (the user renamed the row already).
    #[test]
    fn naming_myself_leaves_a_request_and_stops_the_nudge() {
        let dir = std::env::temp_dir().join(format!("mulpex-setname-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = test_ctx(&dir, 4);

        let reply = hub_set_name(&ctx, &json!({ "name": "  תיקון גלישת שורות\nב-vtgrid  " }))
            .expect("a valid name should be accepted");
        assert!(reply.contains("\"ok\":true"), "{reply}");

        // Whatever the model passed reaches the sidebar as one short line — the
        // user writes in Hebrew, so the label does too.
        let requested = std::fs::read_to_string(dir.join("namereq").join("4")).unwrap();
        assert_eq!(requested, "תיקון גלישת שורות ב-vtgrid");
        assert!(dir.join("named").join("4").exists(), "nudge flag not written");

        // A second call supersedes a still-pending first one rather than
        // queueing a rename the model has thought better of.
        hub_set_name(&ctx, &json!({ "name": "second thoughts" })).unwrap();
        assert_eq!(std::fs::read_dir(dir.join("namereq")).unwrap().count(), 1);

        assert!(hub_set_name(&ctx, &json!({ "name": "   " })).is_err());
        assert!(hub_set_name(&ctx, &json!({})).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only the sidebar label is flattened, and it stays short.
    #[test]
    fn labels_are_one_short_line() {
        assert_eq!(flatten_label("npm run   dev\n--host"), "npm run dev --host");
        let long = flatten_label(&"x".repeat(200));
        assert_eq!(long.chars().count(), LABEL_MAX_CHARS + 1); // + the ellipsis
        assert!(long.ends_with('…'));
    }

    // -- reading ------------------------------------------------------------

    fn test_ctx(dir: &Path, instance: usize) -> Ctx {
        let state_dir = dir.to_path_buf();
        Ctx {
            instance,
            project_dir: state_dir.clone(),
            locks_dir: state_dir.join("locks"),
            history_dir: state_dir.join("history"),
            tasks_dir: state_dir.join("tasks"),
            inbox_dir: state_dir.join("inbox"),
            waiting_dir: state_dir.join("waiting"),
            state_dir,
        }
    }

    /// Write a terminal log + screen the way `vtgrid::Recorder` would.
    fn fake_terminal(ctx: &Ctx, id: usize, log: &str, screen: &str) {
        let dir = terminals_dir(ctx);
        std::fs::create_dir_all(&dir).unwrap();
        let header = crate::termlog::format_header(&crate::termlog::Header {
            base: 0,
            last_out_ms: now_ms(),
            exited: false,
        });
        std::fs::write(dir.join(format!("{id}.log")), format!("{header}{log}")).unwrap();
        std::fs::write(dir.join(format!("{id}.screen")), screen).unwrap();
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mulpex-mcp-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -- remote peers -------------------------------------------------------

    /// A remote terminal hosts a claude, not a shell, so input must never be
    /// wrapped in the completion sentinel. In the field this appended
    /// `; printf '\n__MPX_DONE_…' "$?"` to the end of the task text the remote
    /// claude read as its prompt.
    #[test]
    fn input_to_a_remote_claude_is_never_wrapped_in_shell_plumbing() {
        // Exactly the shape that gets tracked for a shell: submitted, one line,
        // at an idle prompt.
        assert_eq!(
            mark_action(true, "fix the bug in auth.ts", "", Some(true), false),
            Mark::Track,
            "a shell command should still be tracked"
        );
        assert_eq!(
            mark_action(true, "fix the bug in auth.ts", "", Some(true), true),
            Mark::Clear,
            "a remote claude must never get the sentinel"
        );
        // And no combination brings it back.
        for submit in [true, false] {
            for idle in [Some(true), Some(false), None] {
                assert_eq!(
                    mark_action(submit, "anything", "", idle, true),
                    Mark::Clear,
                    "submit={submit} idle={idle:?}"
                );
            }
        }
    }


    /// Typing a launch command into a terminal that is mid-command feeds it to
    /// whatever is running, not to a shell — the same mistake as appending
    /// `; printf …` to a heredoc terminator.
    #[test]
    fn launching_into_a_busy_terminal_is_refused() {
        let dir = tmpdir("remote-busy");
        let ctx = test_ctx(&dir, 1);
        fake_terminal(&ctx, 3, "", "   Compiling mulpex v0.6.0\n   Compiling serde v1.0");

        let err = launch_into_existing(&ctx, 3, "ssh somewhere").unwrap_err();
        assert!(err.contains("shell prompt"), "unhelpful refusal: {err}");
        assert!(err.contains("Compiling"), "the refusal does not show what it saw: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Launching into a terminal that already holds a claude would type the
    /// command into *its* prompt — a whole conversation's worth of confusion.
    #[test]
    fn launching_into_a_terminal_that_already_runs_claude_is_refused() {
        let dir = tmpdir("remote-occupied");
        let ctx = test_ctx(&dir, 1);
        let tui = format!("{}\n❯ \n{}", "─".repeat(40), "─".repeat(40));
        fake_terminal(&ctx, 4, "", &tui);

        let err = launch_into_existing(&ctx, 4, "ssh somewhere").unwrap_err();
        assert!(
            err.contains("already has a Claude Code session"),
            "wrong refusal: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_or_exited_terminal_is_refused_by_name() {
        let dir = tmpdir("remote-gone");
        let ctx = test_ctx(&dir, 1);
        assert!(launch_into_existing(&ctx, 9, "x").unwrap_err().contains("no terminal #9"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read of a remote terminal reports the signal and hides the marker: the
    /// wire protocol is Mulpex's, and a model that saw it could forge a wake.
    #[test]
    fn reading_a_remote_reports_its_signal_and_hides_the_marker() {
        let dir = tmpdir("remote-read");
        let ctx = test_ctx(&dir, 1);
        let token = "beefcafe";
        crate::remote::RemoteMeta {
            token: token.into(),
            ssh_target: "root@vm".into(),
            opener: 1,
        }
        .write(&ctx.state_dir, 5)
        .unwrap();
        let marker = format!(
            "{} {token} question Which database should staging use?{}",
            crate::remote::SIG_OPEN,
            crate::remote::SIG_CLOSE
        );
        fake_terminal(&ctx, 5, &format!("I need a decision.\n{marker}\n"), &marker);

        let reply: Value =
            serde_json::from_str(&hub_terminal_read(&ctx, &json!({"id": 5})).unwrap()).unwrap();
        assert_eq!(reply["remote_claude"], json!(true));
        assert_eq!(reply["remote_signal"], json!("question"));
        assert_eq!(
            reply["remote_summary"],
            json!("Which database should staging use?")
        );
        assert_eq!(reply["ssh_target"], json!("root@vm"));
        for channel in ["new_output", "current_screen"] {
            let shown = reply[channel].as_str().unwrap();
            assert!(!shown.contains(token), "the token leaked into {channel}: {shown:?}");
            assert!(
                !shown.contains(crate::remote::SIG_OPEN),
                "the marker leaked into {channel}: {shown:?}"
            );
        }
        assert!(reply["new_output"].as_str().unwrap().contains("I need a decision."));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The screen is the primary channel for a caller that prefixes `clear;`, so
    /// it gets the same marker-stripping as the scrolled-off history.
    #[test]
    fn the_current_screen_has_no_plumbing_in_it() {
        let dir = tmpdir("screen");
        let ctx = test_ctx(&dir, 1);
        let screen = format!(
            "$ ls{}\nfile.txt\n__MPX_DONE_7_0__\n$ ",
            "; printf '\\n__MPX_DONE_7_%s__\\n' \"$?\""
        );
        fake_terminal(&ctx, 1, "", &screen);

        let reply: Value =
            serde_json::from_str(&hub_terminal_read(&ctx, &json!({"id": 1})).unwrap()).unwrap();
        let shown = reply["current_screen"].as_str().unwrap();
        assert!(!shown.contains("printf"), "plumbing on screen: {shown:?}");
        assert!(!shown.contains(DONE_PREFIX), "marker on screen: {shown:?}");
        assert!(shown.contains("file.txt"), "real output lost: {shown:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With a command in flight, `wait_ms` waits for it to FINISH — returning on
    /// its first byte of output is what made real reads come back mid-command.
    /// With nothing tracked there is no completion to wait for, so any new
    /// output ends the wait. The reply says which, so a caller never guesses.
    #[test]
    fn wait_ms_waits_for_the_command_when_one_is_tracked() {
        let dir = tmpdir("wait");
        let ctx = test_ctx(&dir, 1);
        fake_terminal(&ctx, 1, "building…\n", "");

        // Nothing tracked: the wait is for output, and there is already output
        // this reader has not seen — so it must return now, not sit out the
        // window waiting for *more*.
        let started = now_ms();
        let reply: Value = serde_json::from_str(
            &hub_terminal_read(&ctx, &json!({"id": 1, "wait_ms": 5000})).unwrap(),
        )
        .unwrap();
        assert_eq!(reply["waited_for"], "output");
        assert!(reply.get("timed_out").is_none());
        assert!(
            now_ms() - started < 500,
            "blocked despite having unread output in hand"
        );

        // Now track a command that has not printed its marker.
        std::fs::write(mark_path(&ctx, 1), "7").unwrap();
        let reply: Value = serde_json::from_str(
            &hub_terminal_read(&ctx, &json!({"id": 1, "wait_ms": 400})).unwrap(),
        )
        .unwrap();
        assert_eq!(reply["waited_for"], "completion");
        assert_eq!(reply["timed_out"], json!(true));
        assert!(reply.get("command_finished").is_none());
        assert!(now_ms() - started >= 400, "the wait did not actually block");

        // Once the marker lands, the same call returns the exit code.
        fake_terminal(&ctx, 1, "building…\n__MPX_DONE_7_2__\n", "");
        let reply: Value = serde_json::from_str(
            &hub_terminal_read(&ctx, &json!({"id": 1, "wait_ms": 1000})).unwrap(),
        )
        .unwrap();
        assert_eq!(reply["command_finished"], json!(true));
        assert_eq!(reply["exit_code"], json!(2));
        assert!(reply.get("timed_out").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `full` used to silently disable the wait, and the schema did not say so.
    #[test]
    fn full_reads_can_wait_too() {
        let dir = tmpdir("full");
        let ctx = test_ctx(&dir, 1);
        fake_terminal(&ctx, 1, "out\n", "");
        std::fs::write(mark_path(&ctx, 1), "7").unwrap();

        let started = now_ms();
        let reply: Value = serde_json::from_str(
            &hub_terminal_read(&ctx, &json!({"id": 1, "wait_ms": 400, "full": true})).unwrap(),
        )
        .unwrap();
        assert_eq!(reply["waited_for"], "completion");
        assert!(now_ms() - started >= 400, "full read did not wait");
        assert!(reply["new_output"].as_str().unwrap().contains("out"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
