# WizEverything

A **WizTree-style disk space visualizer and duplicate finder** for Windows that never scans
your disk. It reads the file index that **[Everything](https://www.voidtools.com/)** (voidtools)
already keeps in memory, so a full view of ~2 million files is ready in a few seconds,
without administrator rights.

Written in Rust with [egui](https://github.com/emilk/egui).

## Features

### Disk usage
- **Tree view**: % of parent bar, size, allocated size, items, files, folders, last modified.
- **File view**: the 1000 largest files under the current view, with a name filter.
- **File types panel**: per-extension totals with WizTree-like colors; click one to highlight it in the chart.
- **Two charts**: a squarified **treemap** with cushion shading, or **Baobab-style rings**
  (click a ring to zoom in, click the center to go up).
- **Real on-disk size**: computed automatically when you enter a folder (or on demand from the context menu).
- **Drive names**: volume labels, with Google Drive detected as `(stream)` or `(clone)` (mirror).
- **Exclude drives** from the "All" view (handy for cloud drives).
- **Breadcrumbs, keyboard navigation** (arrows, Enter to zoom, Backspace to go up, Home/End, PgUp/PgDn).
- **Sortable columns** everywhere: click to sort, click again to reverse, a third click clears the sort.
- **Right-click menu**: open, open/show in Explorer, copy full path, copy name, zoom, real size.
- **Export to CSV** (current view, folders and files).
- **Live updates**: changed files are synced from Everything every 30 s (deletions show on refresh / every 30 min).

### Duplicates
- **Quick mode**: name + size straight from the index, reads no file (safe with streamed Google Drive); results marked as unverified.
- **Advanced mode**: candidates by **same name + size** or **same size only** (finds renamed copies), confirmed by content.
- **Progressive block comparison** (64 KB → 8 MB blocks, xxh3): files are dropped as soon as they differ,
  so non-duplicates are rarely read to the end. Runs on every core, biggest savings first.
- **Duplicate folders**: whole folders with identical content, detected by an index-only signature
  (names + sizes of everything inside) and then verified; nested matches are not repeated.
- **Hash cache**: the advanced mode remembers each file's content hash (valid while size and date are unchanged), so repeated searches read almost nothing from disk.
- Results **stream in live** in two panes — files on the left, folders on the right — with sortable columns.
- Stopping a search keeps what was found. Hard links are collapsed and cloud-only files are never downloaded.
- Mark "keep oldest / keep newest", send to the **Recycle Bin** with confirmation (never all copies of a group),
  export to CSV.

### App
- Light / dark / system theme, English / Spanish UI (defaults to the Windows language).
- System tray, single instance, **close to tray** option, **Start with Windows** (`--background`, index preloaded).

## Use it from an AI (MCP server)

`wizeverything.exe --mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io) server on stdio,
using the same Everything index. It can run alongside the desktop app.

Tools: `drives`, `folder_sizes`, `largest_files`, `file_types`, `find_duplicates` (read-only;
`verify: false` = name + size only, reads no file), `refresh`.

Claude Code:

```powershell
claude mcp add wizeverything -- "C:\path\to\wizeverything.exe" --mcp
```

Claude Desktop (`claude_desktop_config.json`):

```json
{ "mcpServers": { "wizeverything": { "command": "C:\\path\\to\\wizeverything.exe", "args": ["--mcp"] } } }
```
## How it works

Everything exposes its index through a documented IPC protocol (`WM_COPYDATA`, the SDK's `QUERY2` request).
WizEverything implements it directly (no `Everything64.dll`), fetching pages of 250k items on one thread while
another builds a compact tree (~56 bytes per node; names in one buffer, children in one flat array).
Charts are rasterized on the CPU in parallel bands.

## Requirements

- Windows 10/11
- [Everything](https://www.voidtools.com/) 1.4 or later, running, with file sizes indexed (the default).

## Building

```powershell
.\build.ps1
```

Uses MSVC when Visual Studio Build Tools are installed; otherwise the GNU toolchain plus MinGW-w64
(`winget install BrechtSanders.WinLibs.POSIX.UCRT`). The binary is written to `target\release\wizeverything.exe`.

## Tests

```powershell
cargo test --release --bin wizeverything                    # unit tests
cargo test --release --bin wizeverything -- --include-ignored   # + live tests against a running Everything
```

## Settings

`%APPDATA%\WizEverything\settings.ini` (language, theme, panels, chart, close to tray, excluded drives).
"Start with Windows" writes `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\WizEverything`.

## Limitations

- **Allocated** is an estimate (cluster-rounded) until the real size is computed for a folder.
- Hard links are counted once per path in sizes.
- Only what Everything indexes is shown.

## License

[MIT](LICENSE)
