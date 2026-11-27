#!/usr/bin/env bash
# auto_dispatch.sh — scan every workspace repo's todo.md, dispatch the first
# non-blocked unchecked item to its tmux seat when that seat is idle.
# Rules (user order 2026-11-27): if todo.md exists, has unchecked non-blocked
# items, and no task is running on that seat -> dispatch. Seats are tmux
# sessions named after the repo. BLOCKED detectors: markers in item text.
set -u
WS=/home/cui/workspaces
MARKERS='BLOCKED|暂缓|待用户|待授权|待拍板|不实施|缺.*凭据|缺.*账号|需.*真人|待.*档期|留后续|阻塞=|待测试|资金密钥|Jenkins 节点'

idle() {  # seat idle = tail shows empty composer / bypass hint, no spinner
  local t; t=$(tmux capture-pane -p -t "$1" 2>/dev/null | grep -v '^$' | tail -1)
  echo "$t" | grep -qE 'bypass permissions|new task|^❯ *$|^$'
}

for repo in $(ls "$WS"); do
  d="$WS/$repo"; f="$d/todo.md"; [ -f "$f" ] || f="$d/TODO.md"; [ -f "$f" ] || continue
  tmux has-session -t "$repo" 2>/dev/null || continue        # seat must exist
  idle "$repo" || continue                                    # running -> skip
  task=$(grep '^\s*- \[ \]' "$f" | grep -viE "$MARKERS" | head -1 | sed 's/^\s*- \[ \] //')
  [ -z "$task" ] && continue
  # skip meta-items (the auto-dispatch loop itself / docs-only audits)
  echo "$task" | grep -qiE '巡检|auto.?dispatch' && continue
  echo "[DISPATCH] $repo: ${task:0:80}"
  tmux send-keys -t "$repo" "继续任务（todo.md 首个可做项）：$task" Enter
  sleep 2
done
