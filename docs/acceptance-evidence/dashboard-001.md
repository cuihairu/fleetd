# FLEETD-DASHBOARD-001 acceptance evidence

Date: 2026-11-28
Head at evidence: `4b159b9` (binary `fleetd 0.2.0+g4b159b9`, deployed via
`fleetd.service` restart)
Task: web board for the fleet control plane — any ZT-net device browser
opens `http://<flagship>:3790/` and sees machines / seats / dispatches /
event tail. Read-only: no write route is reachable from the web face.

## Deliverables

- `crates/scheduler/src/dashboard.html` — single-file board (zero
  framework, dark theme, 3s polling, GET-only JS), embedded via
  `include_str!`, four blocks: machines / seat grid / dispatches
  (in-flight + review highlighted) / event tail (latest 20).
- Read faces opened per `read_face_peer` (always-true policy; exposure
  decided by bind): `/v1/fleet`, `/v1/seats`, `/v1/resources`, static `/`,
  plus two NEW faces — `GET /v1/dispatches?limit=`, `GET /v1/events?limit=`
  (backed by `dispatch_list` / `events_list` in `store::ledger`).
- Bind configurability: `[serve] bind = "host:port"` in `~/.hermes/fleet.toml`
  (default unchanged: `127.0.0.1:3790`); CLI `--port/--public/--bind` still
  override. Deployed config: `bind = "0.0.0.0:3790"`.
- Write faces stay loopback-gated: `POST /v1/events` (ingest),
  `PUT/GET /v1/memory`, `/v1/status`, review POST, dispatch POST.

## Acceptance results (live, 2026-11-28 20:5x CST)

| # | check | result |
|---|-------|--------|
| 1 | loopback `GET /` | 200 `text/html; charset=utf-8`, four blocks poll live data |
| 2 | work-runner → `http://10.147.19.130:3790/` (ZT) | 200 `text/html; charset=utf-8`; `/v1/fleet` renders real seats |
| 3 | ZT-source `POST /v1/events` | 403 `loopback only` |
| 4 | ZT-source `PUT /v1/memory` | 403 `loopback only` |
| 5 | ZT-source `POST /v1/agent/heartbeat` (no token) | 403 `forbidden` (fail-closed fix `4b159b9`) |
| 6 | loopback `POST /v1/agent/heartbeat` | 200 (local agent path intact; probe row cleaned after) |
| 7 | loopback `PUT /v1/memory` (truthful note) | 200 `{"ok":true}` |
| 8 | `cargo test --workspace` | 25 test binaries all ok, 0 failed |
| 9 | phone (ZT net) opens board | pending user (user acceptance item) |

## Security posture change (red-line compliance)

3790 is a control plane; the task forbids web exposure of writes. Three
tightenings landed with this task:

1. `POST /v1/events` (ledger ingest) had **no gate at all** — now
   `loopback_peer` only. Safe historically: the server was loopback-bound
   before, so no remote writer ever existed. tmux hooks (the only writers)
   run on the flagship; remote agents use `/v1/agent/heartbeat`, untouched.
   The `request_line.contains("/v1/events")` bypass in `token_ok` removed.
2. Agent heartbeat (`POST /v1/agent/heartbeat`) handed pending pull
   dispatches (task text) to any peer claiming a seat name whenever no
   `FLEETD_RELAY_TOKEN` was set — irrelevant while loopback-only, a real
   hole once the board bound 0.0.0.0. Found during acceptance (item 3
   returned 200 on the first attempt), fixed fail-closed in `4b159b9`:
   admission = real token match OR loopback (`heartbeat_admit`). Startup
   warns when binding non-loopback without a token.
3. Interim commit `e8ad9c3` modeled the gate on `token_ok`, which is TRUE
   for everyone when no token is configured (read-face default) — the live
   matrix caught it; `4b159b9` splits `auth_header_ok` into the two
   semantics (`token_ok` read face vs `heartbeat_tokened` pull channel).

Probe pollution from the acceptance curls (fake `agent_seen` seat `x`)
was deleted after each run; `agent_seen` holds only the pre-existing
`cuihaitao` row.

## Deployment notes

- `~/.hermes/fleet.toml` gained `[serve] bind = "0.0.0.0:3790"`.
- Service: `systemctl --user restart fleetd.service`; binary path is the
  build output (`target/release/fleetd`), version string matches HEAD
  (build.rs hash fix from `b30af5b`).
- When the `cuihaitao` agent machine returns it will get 403 until
  `FLEETD_RELAY_TOKEN` is set on both sides — flagged to the user.
- Cloudflare tunnel / public exposure (P1) deliberately NOT done in this
  task; if ever added, the page stays read-only and auth lands in front
  (cloudflare access or token), per the task red line.
