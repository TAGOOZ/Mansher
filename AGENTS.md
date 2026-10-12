# AGENTS.md — Mansher

Rust + GTK4 clothesline for screenshots. Crates: `mansher-core` (line model, layout, inbox watch — portable, test anywhere), `mansher-app` (GTK overlay, raw Wayland hover strip, tray, clipboard serve).

## Build / test
- `cargo test -p mansher-core` — fast, always run after core changes.
- `cargo build --release -p mansher-app` — ship this; clipboard offer stays alive ~909ms (debug ~1904ms, flakier pastes).
- Tests must set `MANSHER_STATE_DIR` to a temp dir (see `with_state_dir` in core tests) — never touch real `~/.local/state/mansher/`.

## Run the daemon
- Start detached: `setsid ./target/release/mansher-app >/tmp/mansher.log 2>&1 < /dev/null & disown`
- Toggle/reveal: `timeout 8 ./target/release/mansher-app --toggle` (blocks; always wrap in `timeout`).
- Never `pkill -f 'mansher-app'` — it matches your own shell. Use `pkill -f 'mansher-ap[p]'`.
- State: `~/.local/state/mansher/line.json` + `lastseed`. Logs: `/tmp/mansher.log`.

## Gotchas
- Niri supports layer-shell; Mutter does not — keep the non-layer-shell fallback working.
- Rope/card geometry lives in `mansher-core::layout`; the GTK draw code must respect the widget heights (clipped rope = invisible middle).
- Clipboard has no system store on Wayland: copies are served in-process (`serve_forever`), keep the thread alive ≥1s per paste.
- Hover comes from `strip_raw.rs` (raw Wayland 1920x6 strip), not GTK events. No `uinput` — can't self-test hover, ask the user to hold the cursor top-edge.
