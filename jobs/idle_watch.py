#!/usr/bin/env python3
"""Seat gate + idle nudge, Python port of fleet-idle-watch.sh + fire_idle.sh.

Runs as a fleetd .py job: `fleetd job add idle-watch "*/10 * * * *" idle_watch.py`.
Uses the embedded interpreter's `fleetd` hooks so results land in the ledger
(no more parsing tmux text into bash exit codes).

Logic parity with the bash version (2026-11-22 v3/v4 rules):
- busy  = footer 'esc to interrupt · <number>' OR spinner 'N m Ns · ↓'
- idle  = bypass footer 'esc to interrupt · ←' without a number
- skip  = session missing, repo frozen-private, noauto-nudge flag,
          fleetd seat gate DISABLED, no open todos (lowercase todo.md only)
- nudge = paste 继续 via tmux, logged via fleetd.log
"""
import os
import subprocess
import time

FLEETD = "/home/cui/workspaces/fleetd/target/release/fleetd"
WORKSPACES = "/home/cui/workspaces"
NOAUTO = "/home/cui/.hermes/noauto-nudge"
FROZEN_PRIVATE = {"skt-miner", "skt-s12-scratch"}
SESSION_SKIP = ("oc-", "hermes-guard", "exec", "log-")


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def sh(cmd):
    return run(["bash", "-c", cmd]).stdout


def gate_disabled(seat):
    r = run([FLEETD, "seat", "gate-check", seat])
    return "DISABLED" in r.stdout


def is_busy(seat):
    tail = sh(f"tmux capture-pane -p -S -12 -t {seat}")
    if "esc to interrupt · " in tail and any(c.isdigit() for c in tail.split("esc to interrupt · ")[1][:4]):
        return True
    for line in tail.splitlines():
        if "esc to interrupt ·" in line and any(ch.isdigit() for ch in line):
            return True
        if "· ↓" in line:
            return True
    return False


def open_todos(seat):
    todo = os.path.join(WORKSPACES, seat, "todo.md")
    if not os.path.exists(todo):
        return 0
    with open(todo, encoding="utf-8", errors="ignore") as f:
        return sum(1 for line in f if line.lstrip().startswith("- [ ]"))


def main():
    import fleetd  # embedded hook module

    sessions = sh("tmux ls -F '#{session_name}'").splitlines()
    nudged, skipped = [], []
    for seat in sessions:
        if any(seat.startswith(p) or seat.endswith(p) for p in SESSION_SKIP):
            continue
        if seat in FROZEN_PRIVATE:
            continue
        flag = os.path.join(NOAUTO, "ALL")
        if os.path.exists(flag) or os.path.exists(os.path.join(NOAUTO, seat)):
            continue
        if gate_disabled(seat):
            skipped.append(f"{seat}=GATED")
            continue
        if not os.path.isdir(os.path.join(WORKSPACES, seat)):
            continue
        if is_busy(seat):
            continue
        open_n = open_todos(seat)
        if open_n == 0:
            continue
        # idle with open work → nudge
        subprocess.run(["tmux", "send-keys", "-t", seat, "继续按 todo.md 推进下一项", "Enter"])
        nudged.append(f"{seat}(open={open_n})")
        fleetd.log(f"idle-watch: nudged {seat} (open={open_n})")
        time.sleep(1)

    print(f"nudged: {len(nudged)} {nudged}")
    print(f"skipped-gated: {skipped}")


# fleetd runs job sources via exec with __name__="fleetd_job", so the
# classic __main__ guard never fires. Call main() unconditionally.
main()
