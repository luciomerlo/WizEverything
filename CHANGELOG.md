# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Light / dark / system theme selector.
- English and Spanish UI, defaulting to the Windows UI language; tray menu labels follow the selected language.
- Locale-aware number and date formatting.
- Persisted settings (`%APPDATA%\WizEverything\settings.ini`): language, theme, visible panels.
- README and CHANGELOG.

### Changed
- The panel toggle now hides the **treemap** (the tree view stays); setting renamed to `show_treemap`.
- "Items" column is left empty for files instead of showing 0.

## [0.1.0] - 2026-11-19

### Added
- Everything IPC client (`WM_COPYDATA`, `QUERY2`) without `Everything64.dll`; reads ~2M items in about 2 s.
- In-memory folder tree with aggregated size, approximate allocated size, item/file/folder counts and dates.
- Tree view, file view (1000 largest files with filter) and file types panel.
- Squarified cushion treemap rendered on the CPU, with hover, selection, zoom and per-extension highlight.
- Sortable columns (sort, reverse, clear) in all tables.
- Toggle buttons to hide the tree and file types panels.
- Context menu: open, show in Explorer, copy path, zoom into folder.
- System tray icon, single instance, Start with Windows (`--background`), F5 refresh.
- Segoe UI font with Segoe UI Symbol fallback; drawn icons for folders, files, drives and expanders.
