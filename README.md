# DeskFrame

Folders as frames on the Windows desktop. A small, single Rust program with no installer and no background services.

## Features

- **Folder frames:** each frame shows one folder's files and folders on the desktop, with the same icons Explorer uses. Frames update live when the folder changes.
- **Type groups:** items are grouped by type (Folders, Pictures, Videos, Audio, Documents, Archives, Programs and shortcuts, then one group per other extension), with names A–Z inside each group.
- **Previews:** pictures and videos show their thumbnail, as in Explorer.
- **Full name on hover:** a tooltip shows an item's full name with its extension.
- **Collapse and expand:** click a frame's title bar to roll it up to just the bar, and click again to open it.
- **Move, resize and lock:** drag the title bar to move a frame and drag its edges to resize it. Lock a frame to stop it moving.
- **Open items:** double-click or press Enter to open an item. Right-click an item for Open, Open containing folder, Copy path and Delete.
- **Delete:** the Delete key sends items to the Recycle Bin, and Shift+Delete deletes them permanently. Windows asks to confirm and can undo.
- **Drag and drop:** drag items out of a frame into Explorer or other apps. Drop files onto a frame, or onto a folder in it, to move or copy them there, following Explorer's rules.
- **Frame settings:** right-click a frame's title bar to set:
  - opacity
  - blur on or off
  - hide file extensions
  - lock
  - remove the frame
- **Glass look:** blurred, rounded frames that follow Windows' light or dark theme and accent colour.
- **Stays on the desktop:** frames survive Show Desktop (Win+D), Explorer restarts, sleep and resume, and display changes.
- **Every monitor and scale:** frames remember their position per monitor and keep the same look at any resolution or DPI.
- **Missing folders:** if a folder's drive or share goes away, its frame shows "Folder not available" and fills again when the folder returns.
- **Tray icon:**
  - Add frame…
  - Lock all
  - Start with Windows
  - Quit DeskFrame
- **Settings file:** settings live in `config.json` next to the program.

## Download

Get `deskframe.exe` from the [Releases](../../releases) page and put it in a folder of its own (its settings and log are kept beside it). Run it, then use the tray icon's **Add frame…** to pick a folder.

The program is not code-signed, so Windows SmartScreen may warn the first time you run it. Choose **More info**, then **Run anyway**.

## Requirements

Windows 11, build 22621 (22H2) or later, 64-bit.

To build: Rust (stable, MSVC toolchain) and the Visual Studio 2022 Build Tools with the "Desktop development with C++" workload.

## Build

```
cargo build --release
```

The program is `target/release/deskframe.exe`.

## Uninstall

1. In the tray menu, turn off **Start with Windows**. This removes the `DeskFrame Widget` entry the app adds under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
2. Choose **Quit DeskFrame**.
3. Delete the folder holding `deskframe.exe` (with its `config.json` and `deskframe.log`).

Nothing else is installed or written elsewhere.

## License

Licensed under the GNU General Public License, version 3 or (at your option) any later version (GPL-3.0-or-later). See [LICENSE](LICENSE).

The program includes Rust crates under the MIT license; see [THIRD-PARTY-NOTICES](THIRD-PARTY-NOTICES).
