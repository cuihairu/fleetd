# Fleet vocabulary (2026-11-27, 用户钦定术语)

The project speaks one consistent naval model. Use these words everywhere
(code identifiers, docs, tray menu, commit messages):

| Word | Meaning | Notes |
|---|---|---|
| **fleet** | The whole deployment | project name itself |
| **flagship** | The main/control machine (formerly "hub") | `flagship serve`, `fleetd agent --flagship <url>`; `--hub` stays a legacy alias |
| **ship** | Any enrolled machine, flagship included | `agent_seen` rows, `fleet overview` rows |
| **admiral** | The human running the fleet (the user) | UI copy addresses the admiral; never call them "user" in fleet-facing surfaces |
| **seat** | A work slot on a ship where tasks run | unchanged |
| **dispatch** | One task firing | unchanged |

Rules:
- Never reintroduce "hub" in new code or docs. The only allowed occurrences
  are the `--hub` CLI alias and the serde `alias = "hub"` on the persisted
  agent.toml key (both for backward compatibility).
- Old `agent.toml` files written with `hub = "..."` keep loading (serde alias);
  new saves write `flagship = "..."`.
- `Topology::is_flagship()` is the check (renamed from `is_hub`).
