# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.11.0] - 2026-11-20

### Added
- "Similar files" analysis (MCP `similar_images`, `similar_audio` + Space analysis menu):
  - Images: perceptual hashes (dHash + aHash) with LSH banding to find the same photo resized,
    recompressed or exported twice, and a quality score that suggests which copy to keep.
  - Audio: groups the same track across formats/bitrates by artist + title (or filename) and
    duration, splits live/remix versions, and ranks lossless over lossy.
  - Both are read-only and return per-group keep/candidates reasons plus reversible alternatives
    (quarantine, keep both edits, archive, hardlink when byte-identical).

## [0.10.0] - 2026-11-20

### Added
- The remaining storage-analysis engines are now usable from the MCP server — nothing is internal-only anymore:
  - `cloud_audit` — sparse/offline/cloud files and reparse points, logical vs real on-disk size.
  - `hardlink_plan` — dry-run plan of exact duplicates mergeable into NTFS hardlinks on the same volume (read-only; merging stays in the app).
  - `cold_files` — coldest files by modified date, last access and run count (uses Everything's full metadata).
  - `compression_candidates` — cold files worth WOF LZX compression (excludes already-compressed formats).
  - `snapshot_save` / `snapshot_diff` — compact index snapshots and an O(N) delta.
  - `catalog_save` / `catalog_check` — portable `.ecat` catalogs and cross-checking local files against a disconnected volume.
- `snapshot_save`/`catalog_save` write only into `%LOCALAPPDATA%\WizEverything`; every other tool is read-only.

## [0.9.1] - 2026-11-20

### Added
- MCP: two new read-only tools backed by the new engines:
  - `dev_artifacts` — finds rebuildable dev artifacts (`node_modules`, `.venv`, `target`, `__pycache__`, `.gradle`, `.next`, `.terraform`, …) in abandoned projects, with reclaimable size (index only, reads no files).
  - `scan_integrity` — checks structured files (pdf, zip, 7z, rar, mp4, mkv, jpg, png, sqlite, exe, dll…) for corruption: zero-filled files, HTML error pages saved as binaries, or signature mismatches (reads the first 32 bytes of each file).

## [0.9.0] - 2026-11-20

### Added
- Advanced storage-analysis engines (branch `spec/7modules`), all backed by Everything's index and Win32 metadata,
  with unit tests. The in-app panels that drive them are still being wired up.
  - **Deduplication**: 3-stage exact match (size → NTFS file id → partial BLAKE3 of 3 blocks → full BLAKE3) and atomic
    same-volume hardlink merging that keeps every path; dry-run plans by default.
  - **Cloud/sparse audit**: logical vs physical size, sparse/offline/reparse-point classification, and
    junction/symlink resolution via `FSCTL_GET_REPARSE_POINT`.
  - **Dev-artifact sweeper**: detects `node_modules`, `.venv`, `target`, `__pycache__`, `.gradle`, `.next`, `.terraform`, …
    and flags abandoned projects with an inactivity heuristic.
  - **Snapshot delta diff**: compact binary snapshots and an O(N) diff (added/removed/grown/shrunk, net bytes per folder).
  - **Cold data + WOF LZX**: cold score from dates and run count, plus transparent `FSCTL_SET_EXTERNAL_BACKING` compression.
  - **Magic-byte integrity validator**: zero-filled files, HTML error pages saved as binaries, and signature mismatches.
  - **Portable offline catalogs (`.ecat`)**: a Zstd-compressed inventory of external volumes, cross-referenced against local files.
  - Extended Everything adapter (`DATE_ACCESSED`, `ATTRIBUTES`, `RUN_COUNT`) with a unit-tested reply parser.
  - Transactional executor: dry-run by default, JSONL audit log before any mutation.

## [0.7.0] - 2026-11-19

### Added
- Natural-language questions (Groq): question bar under the toolbar, answers tab, chosen model and its remaining quota next to Search. The model uses the read-only MCP tools on the loaded index, with automatic failover across every available model (best first); models known to be out of quota are tried last. Key entered in its own dialog, masked, stored with DPAPI.
- `--ask "question"` command-line mode.
- Duplicate exclusions: suggested defaults on first run, Clear and Suggested buttons.
- Starts maximized; the native title bar turns black in dark mode.
- Tree view: folders first, loose files after (unless sorting by the Folder column).

### Fixed
- Clipboard: paste and Copy path did nothing (eframe clipboard feature was off); links open through the Windows shell.
- MCP: all-drives guard bypassed by an empty path; standalone server ignored excluded drives; assistant duplicate searches default to quick mode.
- Hash cache: files read to the end are cached even when unique; the cache shortcut no longer forces full reads of uncached files.

## [0.6.0] - 2026-11-19

### Added
- Duplicates: excluded folders, kept between sessions (`dup_exclude=` lines in settings.ini). Edit them as a list (one path per line), tick them in the tree with "Pick in tree", or right-click a folder → Exclude from duplicates / Include again. Excluded subtrees are never walked, which also speeds up the search. Paths not found in the index are flagged.
- MCP `find_duplicates` applies the app's saved exclusions and accepts an `exclude` list.

## [0.5.0] - 2026-11-19

### Added
- Persistent hash cache for the advanced duplicate search (`%LOCALAPPDATA%\WizEverything\hash_cache.bin`): files keep their full-content hash while size and modification time are unchanged. Measured on C:\Program Files: 345 MB read on the first search, 11 MB on the next (same 26 groups). Full hashes are computed during the normal block comparison at no extra cost; stale entries are dropped after 90 days; "Clear cache" button.
- Duplicate progress shows bytes actually read from disk and files served from the cache (also reported by the MCP tool).
- MCP accepts paths with forward slashes (C:/Users/...).

### Changed
- File and folder comparison share one routine (cache, hard links, cloud placeholders, early exit).

## [0.4.0] - 2026-11-19

### Added
- MCP server mode (--mcp) for AI assistants: drives, folder_sizes, largest_files, file_types, find_duplicates, refresh.
- Duplicates quick mode (name + size from the index, no disk reads) vs advanced mode (content hashes); unverified groups marked with ≈ and warned about before deleting.
- Draggable split between duplicate files and folders (10%–90%).

### Fixed
- Cancel in duplicates stops immediately and keeps results; changing options restarts a running search; minimum size filters results live.
- Memory growth when syncing many new files into large folders (geometric child slots).
- Network paths (\\server\share) no longer create phantom drives on sync.
- Nested duplicate folders are verified instead of dropped; hidden only when covered by a duplicate parent.
- Hard links no longer reported as duplicate folders; duplicate progress reaches 100%.
- Google Drive detection with truncated volume labels.

## [0.3.0] - 2026-11-19

### Added
- File types grouped into categories (Video, Audio, Images, Documents, Spreadsheets, Presentations, Mail, Archives, Disk images & VMs, Programs & system, Code, Databases, Fonts, Design & 3D, Logs & temporary, No extension, Others); every column sorts, Others included.
- Clicking a category or an extension paints the treemap / rings with only those bytes.
- Flat, two-level treemap (group colors, soft separators, group tags, name (size) labels); double-click to go deeper.
- Ring chart uses the treemap palette; clicking a drive in the tree shows only that drive.
- Duplicate folders pane; results stream live; stopping keeps results.

### Fixed
- Right-click menu now opens (selectable labels swallowed the click); simplified to Open / Copy full path / Copy name.
- Columns can no longer be resized out of the panel.
- Stale ids after a reload (highlight, hover, menu, real-size queue) could crash.
- Keep oldest/newest could mark every copy of a file inside a duplicated folder.
- Recycle Bin errors left the list stale; errors now fade; free-space query cached and underflow-safe.
- App shown by a second instance now keeps syncing.

## [0.2.0] - 2026-11-19

### Added
- **Duplicate finder** tab: name+size or size-only candidates from the index, progressive block comparison
  (xxh3) on all cores, biggest savings first, live streaming results, sortable columns.
- **Duplicate folders** (right pane): index-only content signatures, verified on disk, nested matches skipped,
  mismatching copies split off instead of discarding the group.
- Mark keep oldest/newest, send to Recycle Bin with confirmation, CSV export of duplicates.
- **Baobab-style ring chart** as an alternative to the treemap (click to zoom in, center to go up).
- **Incremental sync** of changed files every 30 s (Everything `rc:` query) and full reload every 30 min.
- **Real on-disk size** (`GetCompressedFileSizeW`) for the current folder, in parallel.
- Drive names from volume labels; Google Drive shown as `(stream)` or `(clone)`.
- Exclude drives from "All"; breadcrumbs; keyboard navigation; CSV export of the current view.
- Options menu: language, theme, start with Windows, **close to tray**, included drives.
- Right-click menu everywhere: open, open/show in Explorer, copy full path, copy name.
- Tricolor magnifying-glass icon (tilted right, like Everything's).
- MIT license; unit tests (33, including live tests against Everything).
- `build.ps1` picks MSVC when available, GNU + MinGW otherwise.

### Changed
- Compact tree: ~56 bytes per node, shared name buffer and flat children array (≈450 MB → ≈210 MB for 2.1M items).
- Parallel work: index pages fetched while parsing, children sorted in parallel, charts rasterized in parallel bands.
- Panel toggle hides the chart (treemap or rings), not the tree view.

### Fixed
- Sorting the tree by "Folder" now redraws immediately (also at the "This PC" level).
- Sync of a change inside an excluded drive no longer creates a duplicate drive.
- Unsupported emoji glyphs removed; double-encoded characters repaired.

## [0.1.0] - 2026-11-19

### Added
- Everything IPC client (`WM_COPYDATA`, `QUERY2`) without `Everything64.dll`.
- Folder tree with aggregated sizes, tree / file / file types views, squarified cushion treemap.
- Sortable columns, panel toggles, context menu, system tray, single instance, start with Windows.
- Light/dark theme and English/Spanish UI.
