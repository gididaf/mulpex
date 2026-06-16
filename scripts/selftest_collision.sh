#!/usr/bin/env bash
# Self-test: drive two real Claude instances into a same-file collision and
# report whether instance #2 resolves it CLEANLY (one quiet wait → success) or
# churns (staleness errors, shell bypass, asking the user). Fully automated:
# launches mulpex in tmux, sends prompts, then inspects the session transcripts.
#
# Usage: scripts/selftest_collision.sh
set -u
SCRATCH=~/Documents/Code/mulpex-scratch
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SESS=mpselftest
PROJ=~/.claude/projects/-Users-gididaf-Documents-Code-mulpex-scratch

echo "== (re)installing mulpex from $REPO =="
( cd "$REPO" && cargo install --path . --locked >/dev/null 2>&1 ) || { echo "install failed"; exit 1; }

echo "== resetting scratch =="
cd "$SCRATCH" || exit 1
git checkout -q . 2>/dev/null
rm -rf "$SCRATCH/.mulpex"
printf 'line 1\nline 2\nline 3\n' > NOTES.md
printf 'alpha\nbeta\ngamma\n'     > DATA.md
git add -A >/dev/null 2>&1; git commit -q -m reset >/dev/null 2>&1
# forget persisted sessions so we start with ONE fresh instance
for f in ~/.mulpex/sessions/*.txt; do [ -e "$f" ] && head -1 "$f" | grep -qF "$SCRATCH" && rm -f "$f"; done

echo "== launching mulpex in tmux =="
tmux kill-session -t "$SESS" 2>/dev/null
tmux new-session -d -s "$SESS" -x 200 -y 50
tmux send-keys -t "$SESS" "cd $SCRATCH && mulpex" Enter
sleep 8

# locate the main mulpex TUI process (NOT its `mulpex mcp`/`hook` children) + state dir
MAIN=$(for p in $(pgrep -x mulpex); do a=$(ps -o command= -p "$p"); case "$a" in *" mcp"*|*" hook"*) ;; *) echo "$p";; esac; done | head -1)
SD="${TMPDIR%/}/mulpex-$MAIN"
echo "   main pid=$MAIN state=$SD"

echo "== #1: exercise an MCP tool (hub_set_focus) + start a long single-file editing turn =="
tmux send-keys -t "$SESS" 'Use mcp__mulpex__hub_set_focus to set your task to "editing NOTES.md", then append 25 numbered lines to NOTES.md, one Edit per line, going slowly.' Enter
# GATE: wait until #1 actually acquires a lock (its first edit) — up to 60s
held=no
for _ in $(seq 1 120); do
  if ls "$SD"/locks/* >/dev/null 2>&1; then held=yes; break; fi
  sleep 0.5
done
echo "   #1 holds a lock: $held  ($(grep -h . "$SD"/locks/* 2>/dev/null | tr '\n' ' '))"
echo "   #1 MCP hub task published: [$(cat "$SD"/tasks/1 2>/dev/null)]"

echo "== #2: new instance, collide on NOTES.md WHILE #1 holds it =="
tmux send-keys -t "$SESS" C-t
sleep 2
tmux send-keys -t "$SESS" 'Add the line "hello from two" at the end of NOTES.md.' Enter
# observe whether #2's read/edit actually engages the ⏳ wait
engaged=no
for _ in $(seq 1 90); do
  if ls "$SD"/waiting/* >/dev/null 2>&1; then engaged=yes; break; fi
  sleep 0.5
done
echo "   #2 ⏳ wait engaged: $engaged  ($(cat "$SD"/waiting/* 2>/dev/null | tr '\n' ' '))"
if [ "$engaged" = yes ]; then
  echo "   --- live info-pane capture during the wait (look for 'Waiting'/⏳ + the task line) ---"
  tmux capture-pane -t "$SESS" -p | grep -iE 'Waiting|⏳|editing NOTES|Locks|NOTES.md' | sed 's/^/     | /' | head -12
fi

echo "== waiting for the scenario to play out (~230s, generous so #2 can finish after its wait) =="
sleep 230

echo "== quitting =="
tmux send-keys -t "$SESS" C-q; sleep 0.4; tmux send-keys -t "$SESS" C-q; sleep 2
tmux kill-session -t "$SESS" 2>/dev/null

echo
echo "================ ANALYSIS ================"
# the two most recent transcripts
mapfile -t FILES < <(ls -t "$PROJ"/*.jsonl 2>/dev/null | head -2)
for f in "${FILES[@]}"; do
  # which instance? find the first user prompt
  firstp=$(jq -rc 'select(.message.role=="user") | (.message.content // empty) |
            if type=="string" then . else (.[]?|select(.type=="text")|.text) end' "$f" 2>/dev/null | head -1)
  stale=$(grep -c "modified since read" "$f" 2>/dev/null)
  asks=$(jq -rc 'select(.message.role=="assistant") | (.message.content[]?|select(.type=="text")|.text)' "$f" 2>/dev/null \
          | grep -ciE 'would you like|want me to|how (would|do) you|should i|what.*line|hold off|let me know')
  bypass=$(jq -rc 'select(.message.role=="assistant") | (.message.content[]?|select(.type=="tool_use" and .name=="Bash")|.input.command)' "$f" 2>/dev/null \
          | grep -ciE '>>|printf|sed|tee|cat >')
  # longest stall between consecutive events = how long a tool (e.g. a gated
  # Read) blocked. A big stall on #2 is proof the ⏳ wait engaged.
  maxgap=$(jq -rc 'select(.timestamp!=null) | .timestamp' "$f" 2>/dev/null | python3 -c '
import sys,datetime
ts=[datetime.datetime.fromisoformat(l.strip().replace("Z","+00:00")) for l in sys.stdin if l.strip()]
g=max(((ts[i+1]-ts[i]).total_seconds() for i in range(len(ts)-1)), default=0)
print(f"{g:.0f}")')
  echo "--- $(basename "$f")"
  echo "    first prompt: ${firstp:0:70}"
  echo "    staleness errors : $stale"
  echo "    user-facing asks : $asks"
  echo "    shell-bypass tries: $bypass"
  echo "    longest stall (s): $maxgap   (big = a gated read/edit waited)"
  echo "    last 3 events:"
  jq -rc 'select(.message.content != null) | .timestamp as $t |
    (.message.content[]? |
      if .type=="text" then ($t+" TEXT "+(.text|gsub("\n";" ")|.[0:55]))
      elif .type=="tool_use" then ($t+" CALL "+.name)
      elif .type=="tool_result" then ($t+" RSLT "+((.content|if type=="array" then (map(.text//"")|join(" ")) else tostring end)|gsub("\n";" ")|.[0:35]))
      else empty end)' "$f" 2>/dev/null | tail -3 | sed 's/^/        /'
done
echo
echo "final NOTES.md tail:"; tail -3 "$SCRATCH/NOTES.md"
echo "=========================================="
