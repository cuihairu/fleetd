# What fleetd should adopt from Orca (stablyai/orca)

Source: https://gitflagship.com/stablyai/orca — 139k stars, MIT, Electron + pnpm
monorepo. Researched 2026-11-26 against fleetd's actual situation
(multi-machine headless fleet, WeChat control plane, ZeroTier network).

## Fit verdict (what NOT to copy)

Orca is a **desktop app + mobile app**; fleetd is a **headless daemon**.
Do NOT copy its product, Electron shell, or cloud relay. Three of the user's
machines (work-runner = headless Linux, home-pve, etc.) cannot run Orca's
GUI runtime. Its official integrations are Slack/Linear/GitHub — no WeChat,
no device/resource matrix, no user-space cron, no ZeroTier coexistence.

Adopt the **design patterns**, not the product.

## Pattern 1: Bot lifecycle state machine (HIGHEST VALUE)

Orca's bots expose explicit states: running / waiting / needs-you / completed,
plus a PR as the delivery artifact. The person at the machine can see *why* a
bot is stalled and step in ("one tab away").

fleetd gap: `events.verdict` and `dispatches` exist but the state semantics are
implicit; a failed dispatch is indistinguishable from a quiet one unless a
human reads tmux.

Adopt:
- Enumerate dispatch states explicitly: `queued | running | needs_input |
  done | failed | cancelled`, persisted in `dispatches.state`.
- `needs_input` is first-class (like Orca's "Needs you"), not a silent stall —
  this directly fixes the user's "任务跑着就失败、无感知" complaint.
- Hook side effects (claude stop/hook, kode hook) feed state transitions via
  the existing `/v1/events` endpoint; no polling of tmux text.
- Surface in `fleetd status`: one line per seat with state + age + last head.

## Pattern 2: Session continuity across devices

Orca's headline: "move a conversation between Chat UI and the terminal
without losing context", and desktop↔mobile continuity via a cloud relay.

fleetd gap: sessions live in each machine's own `~/.hermes/state.db`
(5238 rows locally, no host column). Every machine has its own copy; a
dispatch to B starts with zero context from A's session.

Adopt (summary-only, NOT full transcripts — token cost + secret exposure):
- New table `session_notes`: `{id, ts, seat, host, task, summary, commit,
  todo_delta, context_refs}`.
- Each agent writes a compact note at dispatch close (via MCP tool or a
  fleetd hook); full transcript stays local.
- `fleetd context search <query>` queries notes + events + dispatches.
- On dispatch, inject the target task's notes into the new agent prompt:
  B starts with A's context instead of a blank slate.

This is the user's "分配任务时能快速获得上下文" requirement.

## Pattern 3: Host-local caller-surface predicate (cross-machine consistency)

From Orca's native-chat provisioning design: one **host-local predicate**
decides behavior regardless of which client (desktop/mobile/old/new) asked.
Transient inputs (`createdWithAgent`, `promisedAgentChat`) are NOT persisted
as workspace metadata; old clients talking to a new host still get correct
behavior. "No client-local SSH fallback" — one owner of the decision.

fleetd gap: behavior is split between scripts on each seat and the flagship;
old/new script versions cause divergent verdicts (the "双口径" problem in
the user's rules).

Adopt:
- Put dispatch decisions in the flagship daemon only; seats report observations,
  never decide their own state.
- Accept additive optional fields on events/notes from older agents;
  never require a synchronized upgrade of all seats.
- One source of truth (`ledger.db` on flagship), no per-seat "I think I'm done".

## Pattern 4: Managed skill bundles (capability distribution)

Orca ships skills via signed, versioned, resumable bundles to selected
machines, with atomic install, cancellation, journal + recovery after process
death, and per-provider roots (`~/.agents/skills` for Codex etc.).

fleetd gap: the user asked "是写技能还是怎么办 / 需要告诉 claude 这类 agent
不然又要瞎搞" — currently capabilities are hand-written per agent (SKILL.md
on each machine), no distribution/versioning.

Adopt (small scale):
- fleetd holds a canonical skill registry (git repo or directory on flagship).
- `fleetd skills sync <seat>` pushes/validates skill checksums to that seat.
- No need for signed GCS bundles; a checksum + source path is enough for the
  user's trust level. Keep data-out defaults off (user's hard rule).
- Report per-seat installed skill versions in `resources` table.

## Pattern 5: First-class remote hosts

Orca's AGENTS.md lists "Remote hosts — features should work locally and on
remote hosts" as a first-class citizen. fleetd is already agent-side and
headless, so this is a natural advantage — keep it, don't regress.

Concretely: every new fleetd command must work when invoked from the flagship
against any seat (local or remote), not only on the machine where it runs.

## Pattern 6: Engineering discipline (ratchets, changed-only gates)

Orca's AGENTS.md: "Tests are the exception" — prove changes by running them;
commit tests only where silent regressions are costly (parsers, security,
persistence, concurrency). CI uses per-path ratchets; changes are verified
with `check:changed` only on touched paths.

Note: the user's rule is "功能优先于测试，测试是门禁不是交付物" and coverage
gates were disabled 2026-11-26 — this aligns with Orca's anti-antipattern
stance, NOT with chasing 100% coverage. Adopt the "prove by running" part;
do not reintroduce coverage gates.

## Priority order for fleetd

| P | Pattern | Why |
|---|---------|-----|
| 1 | Lifecycle state machine | fixes "无感知" directly; small schema change |
| 2 | Session notes + context injection | fixes "跨机上下文"; medium work |
| 3 | Host-local decision ownership | fixes "双口径"; mostly discipline |
| 4 | Skill sync | medium; after 1-2 land |
| 5 | Ratchets/changed-gates | optional; user is token-sensitive, keep gates cheap |

## Explicit non-goals

- No Electron/GUI shell for headless seats (use the Windows tray already
  built for GUI visibility instead).
- No WeChat/Slack/Linear integrations inside fleetd — the control plane
  stays where the user talks to Hermes.
- No full transcript replication; summaries only.
- No new coverage gate.
