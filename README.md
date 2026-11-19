# WizEverything

A **WizTree-style disk space visualizer** for Windows that never scans your disk.
Instead, it reads the file index that **[Everything](https://www.voidtools.com/)** (voidtools)
already keeps in memory, so a full view of ~2 million files is ready in a few seconds,
without administrator rights.

Written in Rust with [egui](https://github.com/emilk/egui).

## Features

- **Tree view**: % of parent bar, size, allocated size, items, files, folders, last modified.
- **File view**: the 1000 largest files under the current view, with a name filter.
- **File types panel**: per-extension totals with WizTree-like colors; click one to highlight it in the treemap.
- **Treemap**: squarified layout with cushion shading, hover tooltips, click to select, and zoom into folders.
- **Sortable columns** in every table: click to sort, click again to reverse, a third click clears the sort.
- **Panel toggles**: hide the tree or the file types panel to give the treemap more room.
- **Context menu**: open, show in Explorer, copy path, zoom into folder.
- **Light / dark / system theme** and **English / Spanish** UI (defaults to the Windows UI language).
- **System tray** icon, single instance, and an optional **Start with Windows** entry
  (starts hidden with `--background`, so the index is preloaded when you open it).
- Refreshes automatically when reopened from the tray if the data is older than one minute (or press **F5**).

## How it works

Everything exposes its index through a documented IPC protocol (`WM_COPYDATA`, the SDK's
`QUERY2` request). WizEverything implements that protocol directly, so it does **not** need
`Everything64.dll`. It requests name, path, size and date modified for every item,
in pages of 250k, and builds an in-memory tree with aggregated folder sizes.

## Requirements

- Windows 10/11
- [Everything](https://www.voidtools.com/) 1.4 or later, running, with file sizes indexed (the default).

## Building

The project is pinned to the GNU Rust toolchain (`rust-toolchain.toml`) and needs MinGW-w64
for `dlltool`/`as`:

```powershell
winget install BrechtSanders.WinLibs.POSIX.UCRT
.\build.ps1
```

The binary is written to `target\release\wizeverything.exe`.

## Settings

Stored in `%APPDATA%\WizEverything\settings.ini` (language, theme, visible panels).
"Start with Windows" writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\WizEverything`.

## Limitations

- **Allocated** is an approximation (size rounded up to the drive's cluster size).
  Everything does not expose the real on-disk size, so compressed, sparse and MFT-resident
  files are not accounted for exactly like WizTree does.
- Hard links are counted once per path.
- Only what Everything indexes is shown (excluded folders or drives won't appear).

## License

Not yet specified.
