# FLEETD-PRESENCE-001 — acceptance evidence (2026-11-28)

Seat-presence face: `presence` ledger table + pure tail classifier + 60 s
reconciler + `GET /v1/fleet` + `fleetd fleet` CLI, parity-verified against the
orchestrator's reference script `~/.hermes/scripts/seat_state.py`.

## 1. Unit tests

`cargo test --release --workspace` — all suites green (24 `test result: ok`,
0 failures). New coverage in this task:

- `crates/core/src/presence.rs` — 17 pure-classifier tests: the four states
  (active / stuck / idle / gone), hysteresis band, user-reply cancels stuck,
  stale tool_use window, mtime fallback for empty/garbled tails, tail
  keeps only the last 200 lines on a 300-line file, spinner/menu pane
  patterns, veto promotion, wire names.
- `crates/scheduler/src/presence.rs` — 2 tests (gone seat without
  transcript, missing todo counts).
- `crates/store/src/ledger.rs` — presence upsert + `?state=` filter
  roundtrip.

Regression test added during bring-up:
`classify_transcript_uses_file_mtime_age`. The first live build classified
every seat through a `classify_transcript` that computed the mtime EPOCH
(`now - elapsed`) instead of the AGE, so `mtime_age < 240` never passed and
STUCK could never fire on any live path (CLI + reconciler). Deterministic
replay of the frozen transcript against `seat_state.py::transcript_state`
showed byte-identical verdicts (both STUCK, `pending-tool-use:Bash`,
ages 279.4 / 279.368), which isolated the bug to the mtime path. Fixed and
covered by the regression test.

## 2. Live parity vs seat_state.py

`fleetd fleet` (release binary) vs `python3 ~/.hermes/scripts/seat_state.py`
on the full seat set, three verification rounds:

```
round 4 common: 81 diff: 0
round 6 common: 81 diff: 0
final capture  common: 81 diff: 0     exit code: 0
```

Zero divergence across all 81 common seats, including every active and idle
seat (the acceptance hard rule). Sample of the final run
(`/tmp/fleet_final.txt`, 82 seats printed; lanes/non-repo dirs filtered):

```
seat                     state     open  blocked  last       signal
BigWorld                 idle      1     0        614s       stale-transcript
Playsmith                idle      0     0        137602s    stale-transcript
apollo                   active    29    29       85s        fresh-event
...
soar                     stuck     3     1        375s       pending-tool-use:Bash
```

Exit-code parity confirmed: 0 when some IDLE seat has open todos
(fire candidates exist), 1 otherwise.

Note on single-seat transient flips observed during earlier rounds
(stuck/draining/active flipping between the two sequential reads on 1-2
seats): both tools read the transcript files at different moments while
agents are appending to them; a tool_result landing or an agent waking
between the reads legitimately changes the verdict of the SECOND reader.
Deterministic replay on frozen files (identical bytes, identical synthetic
`now`) produced identical verdicts, and transcript selection plus tail
bytes were fingerprint-verified identical between the two implementations.
Live rounds after the mtime fix went fully clean (rounds 4/6 above).

## 3. HTTP face

`fleetd serve --http --no-tray --no-scheduler` (loopback TCP :3790), after
two reconciliation rounds:

```
presence: reconciled 59 seat(s)
presence: reconciled 59 seat(s)
```

```
$ curl -s http://127.0.0.1:3790/v1/fleet
rows: 59
states: ['active', 'gone', 'idle', 'stuck']
sample: {'blocked': 0, 'last_commit': None, 'last_event_ts': 1795825525,
         'open': 1, 'seat': 'BigWorld', 'state': 'idle'}

$ curl -s 'http://127.0.0.1:3790/v1/fleet?state=stuck'
stuck rows: 3 ['beacon', 'kode', 'soar']
```

Loopback gate active (same as `/v1/status`); no `--public` face was
chartered for this endpoint.

## 4. Presence table vs tmux sessions

After two 60 s reconciliation rounds:

```
sqlite> select count(*) from presence;         -> 59
sqlite> select state, count(*) group by state; -> active|4, gone|1,
                                                  idle|51, stuck|3
tmux ls | wc -l                                -> 59
```

Presence rows (59) >= tmux session count (59). One `gone` row is a seat
whose tmux session died between enumeration and transcript read; it
self-heals on the next round.
