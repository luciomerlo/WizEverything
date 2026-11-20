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
- **Excluded folders** (kept between sessions): type them one per line, tick them in the tree, or right-click a folder.
- **Hash cache**: the advanced mode remembers each file's content hash (valid while size and date are unchanged), so repeated searches read almost nothing from disk.
- Results **stream in live** in two panes — files on the left, folders on the right — with sortable columns.
- Stopping a search keeps what was found. Hard links are collapsed and cloud-only files are never downloaded.
- Mark "keep oldest / keep newest", send to the **Recycle Bin** with confirmation (never all copies of a group),
  export to CSV.

### App
- Light / dark / system theme, English / Spanish UI (defaults to the Windows language).
- System tray, single instance, **close to tray** option, **Start with Windows** (`--background`, index preloaded).

## Advanced storage analysis (in the app and via MCP)

A suite of analyzers that lean entirely on Everything's in-memory index (no directory walking)
and on Win32 metadata, minimizing physical disk I/O. Every engine is implemented, unit-tested and
exposed both in the desktop app and through the MCP server.

- **Deduplication & hardlinks** — 3-stage exact match (size → NTFS file id → partial BLAKE3 of 3 blocks → full
  BLAKE3), plus atomic same-volume hardlink merging that keeps every path. Plans are dry-run by default.
- **Cloud / sparse audit** — separates logical size from real on-disk allocation (`GetCompressedFileSizeW`),
  flags sparse/offline/pinned placeholders, and resolves junctions and symlinks (`FSCTL_GET_REPARSE_POINT`).
- **Dev-artifact sweeper** — spots `node_modules`, `.venv`, `target`, `__pycache__`, `.gradle`, `.next`, `.terraform`, …
  and flags them when the surrounding project looks abandoned.
- **Snapshot delta diff** — compact binary snapshots and an O(N) diff (added / removed / grown / shrunk, net bytes per folder).
- **Cold data & WOF LZX** — a cold score from modified/accessed dates and run count, with transparent
  WOF (LZX/XPRESS) compression of cold files that are not already compressed.
- **Integrity check (magic bytes)** — reads the first 32 bytes to catch truncated or zero-filled files and
  HTML error pages saved with binary extensions.
- **Portable offline catalogs (`.ecat`)** — a Zstd-compressed inventory of external/offline volumes,
  cross-referenced against local files.
- **GitHub mode** (experimental) — for any chosen folder: is the local copy up to date with the web, newer than it
  (unpushed commits), behind or diverged? Then explains commits, changed files, size and per-folder gaps.
  Read-only; `git fetch` is optional (`fetch=false` stays offline).
- **Similar files (images & audio)** (experimental) — finds near-duplicates that are not byte-identical: perceptual
  hashes (dHash + aHash, LSH-banded) for photos resized/recompressed/re-exported, and tag + duration
  matching for the same track across FLAC/MP3/M4A. Each group suggests which copy to keep by quality
  and offers reversible alternatives (quarantine, keep both edits, archive).
  For copies that differ in processing (e.g. `Song.wav` vs `Song (denoised).wav`), it decodes a bounded
  20 s window per candidate (with a wall-clock budget) and measures the real noise floor, SNR, digital silence, high-frequency content
  and spectral stability: it tells a clean denoise from an over-filtered one (dull or "musical noise")
  and explains the difference in plain language. Denoise markers are ignored when matching names. Results open in a visual
  panel with thumbnails, the copy to keep and the candidates, not as raw JSON.
  It also includes **audio A/B**: both copies play from the same frame position (up to two minutes)
  and the app alternates which one is audible every 0.5–4 s, with blind mode, manual A/B, side swap
  and a position slider. For images, **zoom comparison**: the same normalised area of both side by
  side at 1×–16×, panned by the mouse, so a lower-resolution copy shows its blur immediately.
- **Visual GitHub panel** — the repo status becomes a colored headline plus a size-per-folder table
  (disk vs tracked vs delta) and collapsible commit/file lists, instead of JSON text.
- **Type filter in the left explorer** — the baobab/rings view can send the highlighted type to the
  explorer with one click (↙): the tree rescales to that type's bytes and the files list keeps only
  that type, with a removable chip in the tab bar.
- **"Why is it full?"** — top folders/files with percentages, free space and the chain of biggest
  children down to the leaf that actually holds the space. Read-only.
- **"What grew?"** — diffs the two newest snapshots and attributes every new/grown file to its parent
  folder, so growth arrives as a ranked folder list. Snapshots see what the index sees: refresh (F5)
  before saving if files were just created.
- **"Can I delete this?"** — second opinion for any file/folder: safe / caution / keep, with reasons
  (regenerable build output, git-tracked, hard links, cloud placeholder, system folder, personal data,
  age), the real reclaimable bytes and the action to stage it in the basket.
- **"Is it backed up?"** — compares a folder against another copy by relative path and size: coverage %,
  missing bytes, same-name-different-size and extras only in the backup. Both folders must be indexed.
- **Action basket** (experimental) — stages hardlink merges, LZX compression and reversible quarantine,
  shows a dry-run estimate, and only executes after an explicit "I understand this modifies files"
  checkbox. Writes one JSONL line per operation to `%LOCALAPPDATA%\WizEverythingudit.jsonl` and keeps
  a manifest so quarantined files can be restored.

Audio A/B and the image zoom comparison are local only: audio is decoded and played through the
default output device (`cpal`); images never leave the machine.

Every mutating operation runs as a dry-run first and writes a JSONL audit log before touching the disk.

## Fast and deep

Every analyzer that can afford a thorough pass answers in two explicit modes:

- **Fast (default)** — a budget or a cap instead of a stall. It always returns an answer and marks the
  result `partial` when the budget cut it short, with `elapsed_ms` and percentages where exactness is
  expensive. Git comparisons use the last known `origin` refs without touching the network.
- **Deep** — no budget: every candidate is analysed (`mode: "deep"` in MCP, or the **profundo** button in
  the app). Slower by design, for when the answer matters more than the wait.

Both modes are listed in the MCP schema (`mode: "fast" | "deep"`, and `deep: true` as an alias) and the
result repeats which one produced it. In the app, after a fast run the **profundo** button's hover shows
how long the fast one took and the estimated deep time, so the slower choice is informed.

## Fast by design

- Everything comes from Everything's in-memory index: no directory walking, no re-reading files for sizes.
- Anything that would need per-file work has a budget and degrades gracefully: audio noise analysis decodes
  at most 20 s per copy, stops after ~1.2 s of wall clock or 120 files, and reports `elapsed_ms`, how many
  groups were measured and whether the result is partial. Deep mode raises that to 120 s per copy and 60 s
  of wall clock, so it can measure the whole library when you ask for it.
- When the exact answer would be slow, the app gives a **percentage** instead of stalling: perceptual
  image similarity, acoustic similarity, backup coverage, growth attribution, deletion confidence.
- Expensive detail is opt-in: A/B playback decodes up to two minutes only when you press play; the zoom
  comparison loads full images only when you open it.

## Use it from an AI (MCP server)

`wizeverything.exe --mcp` runs a [Model Context Protocol](https://modelcontextprotocol.io) server on stdio,
using the same Everything index. It can run alongside the desktop app.

Tools (all read-only except `snapshot_save`/`catalog_save`, which write only into `%LOCALAPPDATA%\WizEverything`):
`drives`, `folder_sizes`, `largest_files`, `file_types`, `find_duplicates`, `dev_artifacts`,
`scan_integrity`, `cloud_audit`, `hardlink_plan`, `cold_files`, `cold_folders`, `compression_candidates`,
`similar_images`, `similar_audio`, `github_mode`, `why_full`, `growth`, `advice`, `backup_check`,
`snapshot_save`, `snapshot_diff`, `catalog_save`, `catalog_check`, `volumes`, `volume_duplicates`,
`volume_lookup`, `provenance`, `timeline`, `quarantine_list`, `speed_test`, `content_search`,
`content_estimate`, `addons`, `refresh`. Tools with a
thorough variant accept `mode: "fast" | "deep"` (or `deep: true`).

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
