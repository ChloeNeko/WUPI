# Games Codex (placeholder — NOT WIRED)

This folder is the reserved home for the **Codex** panel that moved out of the
WUPI main menu (v0.6.3). The OS-level Codex surface (dock tile, home-grid tile,
`#codex` modal window, and the `codex()` JS module in `src/script.js`) was
removed; the authored-lore library is being relocated *into* the Games app so
Codex is strictly a games-app concept.

**Status: stub only.** Nothing here is imported or wired yet — the Games app
still needs work before the codex panel is built out. When that work happens,
this folder will hold the games-codex panel module (following the
`src/games/panels/` convention) and re-use the existing Rust `codex.rs` backend
+ its IPC (`codex_list` / `codex_save` / `codex_delete`), which were retained
unchanged.

> Note: `src/games/panels/codex.js` is a *different* thing — it renders the
> in-game **World Codex** (the read-only world-state reference the narrator
> reasons over: `WorldSchema.summary` + `recent_events` + tracked entities).
> That panel is unrelated to this authored-lore library and stays as-is.
