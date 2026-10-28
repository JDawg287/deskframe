# DeskFrame

Rust Win32 app that shows folders as frames on the Windows desktop. Spec: `docs/Desktop Folder Widget — Build Plan.md`.

## Resume here

Work follows `docs/superpowers/plans/2026-10-26-deskframe.md`. Find the first unchecked box and continue from it; tick boxes in the same edit as the work. Gate results go in `docs/notes/`.

## Hard rules

- Never parent a frame to `Progman`/`WorkerW` (no `SetParent` into Explorer). Frames are unowned top-level `WS_EX_TOOLWINDOW` windows.
- No Rainmeter code: it is GPL. Reimplement the approach only.
- Dependencies: runtime `windows`, `serde`, `serde_json`; build-only `embed-resource`. Nothing else. Extra `windows` crate features are fine.
- Save a frame's position only when a user drag/resize ends (`WM_EXITSIZEMOVE`), never when Windows moves it.
- No right-click shell context menu (shell extensions) in v1.
- Anything installed for this project gets an entry in `docs/dependencies.json` (`"for_project": "yes"`).

## Commands

- Build: `cargo build --release` → `target/release/deskframe.exe`
- Test: `cargo test` (pure modules: `desktop`, `grid`, `groups`, `folder`, `config`, `placement`)
- Lint: `cargo fmt` and `cargo clippy --all-targets -- -D warnings` (a PostToolUse hook runs both after every `.rs` edit)
- Gate: `powershell -File tools/gate.ps1` (add `-RestartExplorer` only with the user's OK; it restarts Explorer)
- Logs: `target/release/deskframe.log` (also visible in DebugView)

## Conventions

- `dlog!(...)` for every state change worth seeing in a gate run (z-order, Show Desktop, display, power, Explorer restart).
- Pure logic stays Win32-free and unit-tested; Win32 glue stays thin.
- Window classes `DeskFrame.App` (hidden) and `DeskFrame.Frame`.
