# FLEETD-DBMETA-001 — acceptance evidence (2026-11-28)

DB schema versioning: `meta` table + upgrade-only migration framework +
`fleetd db info`, main **ac4ef50** (base 37ba2dc).

## 1. Unit tests (task asks ≥6, delivered 7)

`crates/store/src/ledger.rs` `mod migration_tests`:

1. `fresh_db_reaches_latest_version` — empty db → v4, every table present,
   gate/executor/note columns in place.
2. `legacy_metaless_db_migrates_without_duplicate_column_errors` — full
   current schema but NO meta table (the real pre-versioning shape):
   presence-guarded replay stamps v4 without duplicate-column failures,
   legacy rows preserved and backfilled to done.
3. `legacy_baseline_db_gains_missing_columns_and_backfills` — v1-only
   shape: v2..v4 add all 9 columns; finished row → done, in-flight row
   keeps active.
4. `failed_migration_rolls_back_version_and_ddl` — synthetic list, v2
   fails mid-step after a CREATE TABLE: version stays 1, rolled-back DDL
   absent, v1 committed; fixed step re-runs to completion.
5. `downgrade_is_refused` — schema_version > SCHEMA_VERSION → error
   naming "downgrade not supported", stamp and history untouched.
6. `migrate_is_idempotent` — second migrate is a no-op (version and
   history byte-identical).
7. `history_records_every_migration` — history length/order/descriptions
   mirror MIGRATIONS, versions ascend without gaps.

`cargo test --workspace` — green, re-run 3× (see §4 for the scheduler
flake found and fixed on the way).

## 2. Real ledger.db migration (lossless)

The production `~/.fleetd/ledger.db` (pre-versioning, live data) migrated
to v4 via the new-binary `ledger::open()` path:

- `schema_version` = 4; `schema_history` = complete 4-entry JSON array
  (v1 baseline / v2 seat gate+executor / v3 dispatch gate+backfill /
  v4 events note).
- `PRAGMA integrity_check` = ok; row counts preserved: 16 dispatches /
  106 events / 1 seat / 26 memory notes.
- Behavior equivalence with the old try-ALTER path: column migrations are
  presence-guarded (`PRAGMA table_info` check), so replaying v2..v4 over
  the already-ALTERed production db is a no-op instead of a duplicate-
  column error — same end state, no failure.

## 3. `fleetd db info` (reads without migrating)

- Up-to-date db: version + full history with UTC timestamps.
- Legacy db (meta dropped): "legacy (no meta table; migrates to 4 on next
  open)".
- Too-new db (schema_version forced to 5): "5 — NEWER THAN BINARY: this
  fleetd refuses to start (downgrade not supported)".
- Live refusal proof: `FLEETD_DB=<too-new copy> fleetd job list` errors
  with the downgrade message; `FLEETD_DB=<legacy copy> fleetd job list`
  migrates the copy in place (stamps v4) and serves data.
- Deliberately a plain `Connection::open` — the point of the face is
  reporting exactly the state `open()` would refuse.

## 4. Pre-existing scheduler flake: root cause + fix

While gating, `scheduler` lib tests flaked 3/5 red **at HEAD 37ba2dc**
(verified by stash) — not introduced by this task:

- Three tests share the process-global embedded CPython
  (`python_runs_and_hooks_fire`, `seat_list_round_trips`,
  `py_job_runs_embedded`) with three pieces of mutable global state: the
  API OnceLock, the sys.modules registration, run_source's sys.stdout
  swap.
- Failure modes moved between runs ("No module named 'fleetd'", empty
  seats, hooks not counted) — classic start-order luck.
- Fix (test-only, no production change): all interpreter-touching tests
  take `pyhost::PY_TEST_LOCK` BEFORE `pyhost::test_init()` (the first
  `Python::attach` boots CPython — a boot racing another attach leaves
  sys.modules half-initialized), and all three register the SAME shared
  fixture (`SharedTestApi`) so API routing is order-independent.
- Post-fix: scheduler suite 15/15 green; workspace suite 3× green.

## 5. Deployment face

- `fleetd --version` = `0.2.0+gac4ef50`; serve restarted on the new
  binary (systemd unit), `GET /v1/fleet` = 200 with live seat data.
- Note: the release binary must be built AFTER the commit or the baked
  build hash lags HEAD (hit and corrected during this task).
