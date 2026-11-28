# FLEETD-TRAY-001 acceptance evidence

Dispatch: 2026-11-28, seat trayfix, adversarial acceptance.
Branch: `fix/tray-windows-build`, head `2fce52e` (pushed to origin).

## Defects fixed (crates/tray)

1. `uds::UnixStream` (tray.rs L167) — undefined symbol, no `uds` crate in deps.
   Replaced with loopback TCP HTTP (`std::net::TcpStream` → `GET /v1/status` on
   `127.0.0.1:3790`, same face as flagship `serve_on`). No AF_UNIX dependency
   left in the tray. `FLEETD_STATUS_SOCK` override removed, `FLEETD_STATUS_URL`
   kept (tests only).
2. `fleetd_core::ffi::c_void` (tray.rs L218) — undefined crate. Replaced with
   `core::ffi::c_void` raw pointer (matches the windows-sys CreateDIBSection
   signature). Tray still has zero fleetd-internal dependencies.
3. `tray_lib::spawn` (main.rs) — undefined crate name (lib is
   `fleetd_tray_lib`); every Windows build failed here too, beyond the two
   dangling refs listed in the dispatch.
4. main.rs dropped the `StopHandle` on the floor, so `StopHandle::drop`
   (G_EXIT=true) fired immediately and would have killed the tray right after
   spawn. Now bound for the process lifetime (`let _stop`).
5. Dead code + clippy lints cleaned so the CI gate can run `-D warnings`
   (MENU_STATUS const, unused flagship_base, manual char comparison,
   chunks_exact → as_chunks). All new comments in English.

## Verification

- Local cross build: `cargo build --release -p tray --target x86_64-pc-windows-gnu`
  → 0 errors, exe produced:
  `target/x86_64-pc-windows-gnu/release/tray.exe` = 1,652,287 bytes (< 5 MB).
- `cargo clippy -p tray --target x86_64-pc-windows-gnu -- -D warnings` → clean.
- Host (Linux) `cargo check -p tray` → clean (windows cfg'd out path).
- CI: run 46120602803 on windows-latest (native msvc), all steps green:
  clippy -D warnings / build / test / upload artifact `tray-windows-msvc`.
  https://github.com/cuihairu/fleetd/actions/runs/46120602803

## Not done here (per dispatch)

- Deployment to the imac: deferred to the orchestrator after SERVE-001.
  Steps documented in `docs/getting-started.md` (Windows 托盘部署手动节):
  scp → `C:\Users\cui\.fleetd\fleetd-tray.exe` → `schtasks /SC ONLOGON` →
  systray gear visible.
- Docs reconciled with the transport change: `docs/architecture.md` serve↔tray
  line and tray-fallback row updated from AF_UNIX to loopback TCP HTTP.
