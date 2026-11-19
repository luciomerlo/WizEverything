# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
