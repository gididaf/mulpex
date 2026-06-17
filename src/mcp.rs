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
//! - `hub_file_owner` — who holds a given path, and what they're working on
//! - `hub_send` — leave a message for another instance (or `all`)
//! - `hub_inbox` — read (and clear) the messages addressed to me

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::hook::{canonical_target, now, read_field, Ctx};
use crate::persist::{fnv1a, new_uuid};

/// Entry point for `mulpex mcp`. Runs the stdio JSON-RPC loop until stdin closes
/// (the parent `claude` exiting). Always returns `Ok`.
pub fn run(_args: &[String]) -> anyhow::Result<()> {
    let Some(ctx) = Ctx::from_env() else {
        return Ok(()); // no coordination context → a no-op server
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

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
            "tools/call" => match call_tool(&ctx, req.get("params")) {
                Ok(text) => ok(&id, json!({ "content": [ { "type": "text", "text": text } ] })),
                Err(text) => ok(
                    &id,
                    json!({ "content": [ { "type": "text", "text": text } ], "isError": true }),
                ),
            },
            _ => err(&id, -32601, "method not found"),
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
    Ok(())
}

fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The five hub tools, as MCP tool definitions.
fn tool_defs() -> Value {
    let empty = json!({ "type": "object", "properties": {} });
    json!([
        {
            "name": "hub_instances",
            "description": "List every parallel Claude instance Mulpex is running here, with each one's status (working/waiting/needs), current task, and the files it currently holds a lock on. Also reports how many unread hub messages you have. Call this to coordinate before starting overlapping work.",
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
            "description": "Leave a message for another instance (e.g. 'I'm refactoring auth, hold off on session.rs'). It appears in that instance's hub_inbox and is surfaced at the start of its next turn.",
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
        "hub_file_owner" => Ok(hub_file_owner(ctx, &args)),
        "hub_send" => hub_send(ctx, &args),
        "hub_inbox" => Ok(hub_inbox(ctx)),
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
    json!({ "instances": list, "your_unread_messages": unread_for(ctx, ctx.instance) })
        .to_string()
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
        live_ids(ctx).into_iter().filter(|&id| id != ctx.instance).collect()
    } else {
        let id: usize = to_raw.parse().map_err(|_| "'to' must be a number or \"all\"")?;
        vec![id]
    };

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
fn take_inbox(ctx: &Ctx, id: usize) -> Vec<(u64, usize, String)> {
    let dir = ctx.inbox_dir.join(id.to_string());
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let file = e.path();
            if let Ok(content) = std::fs::read_to_string(&file) {
                if let Ok(v) = serde_json::from_str::<Value>(&content) {
                    let ts = v.get("ts").and_then(|x| x.as_u64()).unwrap_or(0);
                    let from = v.get("from").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
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
