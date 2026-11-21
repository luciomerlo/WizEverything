# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.17.0] - 2026-11-20

### Added
- **Diálogo de arranque sin motor**: si no detecta Everything ni Census, ofrece instalar Everything
  con winget o abrir la descarga oficial, y permite indicar la ruta de `census.exe` e iniciar
  `census serve` con elevación. Nada se instala ni se eleva sin que el usuario lo pida.
- **Census como motor de índice alternativo a Everything.** En Opciones → Motor de índice se elige
  **Auto / Everything / Census**. WizEverything habla el mismo IPC `QUERY2W` (`WM_COPYDATA`) y detecta
  la clase `CENSUS_IPC`; no hace falta DLL ni una integración distinta. Auto prefiere Everything si
  está listo y cae a Census cuando no. También aplica al servidor MCP y a `--ask`.
  Census se inicia con `census serve` (requiere elevación para leer la MFT); la app no lo lanza sola.
- **Tema oscuro legible**: texto casi blanco, texto secundario más claro, paneles más separados y
  bordes/acentos de mayor contraste. Aplica a Oscuro y a Sistema cuando detecta oscuro.
- **FTS5 en el índice de contenido compartido** y limpieza de entradas de archivos que ya no existen.

### Changed
- `refresh` del MCP devuelve qué motor respondió (`engine`).
- El tiempo de espera inicial del índice es de 45 s con un error claro, en vez de esperar para siempre.

## [0.16.0] - 2026-11-20

### Added
- Formatos de documento, presentación y planilla además de PDF/texto, y el índice compartido con
  SortEverything (ver 0.15.0 para el detalle de contenido).

## [0.15.0] - 2026-11-20

### Added
- **Buscar dentro de archivos (exp)**: texto/código y PDFs vía `pdftotext` en modo rápido; OCR de
  imágenes y PDFs escaneados vía `tesseract` en modo profundo, con estimación previa de archivos y
  tiempo. Nuevos tools MCP `content_search` y `content_estimate` y ventana/panel en la app.
  Corrige la detección de `pdftotext` (usa `-v`, no `--version`), limita la lectura en memoria y
  protege los fragmentos con Unicode.
- **Documentos, presentaciones y planillas**: `.docx/.odt/.rtf/.epub`, `.pptx/.odp` y `.xlsx/.ods`
  además de PDF y texto/código, extrayendo el XML/ZIP. Los formatos binarios antiguos
  (`.doc/.ppt/.xls`) se detectan y se avisa que hay que convertirlos.
- **Índice de contenido compartido con SortEverything** en
  `%LOCALAPPDATA%\WizeShared\content-index.sqlite3`: esquema idempotente, WAL y un *lease* con
  latido para que una sola app indexe a la vez; la otra lee el mismo índice. Contrato y API en
  `docs/SHARED-CONTENT-INDEX.md`, replicado en ambos repos.

## [0.14.0] - 2026-11-20

### Added
- **Addons externos**: detección de rclone y gphotos-sync para Google Photos, y de Tesseract/pdftotext
  para OCR/PDF, con pasos de instalación y comandos en la app (Opciones → Addons) y en el MCP (`addons`).
  La app no pide credenciales ni sube/borra nada.
- **Discos guardados (offline)**: registro de unidades que se conectan y desaparecen, con marca,
  modelo, número de serie del disco, serial de volumen, bus, tamaño y fecha de última actualización.
  Dos modos de retención: **Completa** (rutas, tamaños, fechas y huella; navegable) y **Resumen**
  (totales, carpetas, tipos y archivos grandes). Los discos habilitados aparecen como una unidad más;
  se pueden borrar los datos guardados sin tocar el disco real.
- **Semihash de muestra**: 8 KiB (rápido) o 48 KiB (profundo) por archivo, con el tamaño incluido.
  No es un hash completo; sirve para reconocer copias renombradas y buscar duplicados sin leer el
  disco entero. Cobertura y presupuesto declarados.
- **Cuarentena navegable**: listar, restaurar a la ruta original (sin sobrescribir) y borrar la copia
  guardada, con auditoría. El MCP expone `quarantine_list` de sólo lectura.
- **Procedencia**: zona de Internet/URL desde `Zone.Identifier`, aplicación instalada por Registro
  (Uninstall 64/32/HKCU), bibliotecas de Steam y `run_count` en modo profundo. MCP `provenance`.
- **Línea de tiempo**: lista de snapshots y comparación de cualquier intervalo con slider; reusa el
  informe de "¿Qué creció?". MCP `timeline`.
- **Velocidad y ETA**: detecta NVMe/SSD/HDD/USB por bus y penalidad de seek, mide lectura (y escritura
  opcional), detecta Google Drive streaming/clone y el porcentaje ya local/fijado, mide la descarga
  real leyendo un placeholder (profundo, lo hidrata) y estima la tarea con velocidad efectiva y costo
  por archivo. MCP `speed_test`.

## [0.13.0] - 2026-11-20

### Added
- Explicit **fast / deep** answers everywhere a thorough pass exists. Fast is the default: it uses a
  budget or cap, always returns an answer and marks it `partial` with `elapsed_ms` and percentages when
  it had to stop early. Deep removes the budget and analyses every candidate.
  - MCP: `mode: "fast" | "deep"` (or `deep: true`) on `find_duplicates`, `scan_integrity`,
    `similar_images`, `similar_audio`, `github_mode`, `growth`, `advice` and `backup_check`; every
    result repeats the `mode` that produced it. `find_duplicates` no longer hashes by default (fast =
    name + size; deep = content), and `github_mode` fast compares against the local `origin` refs with a
    note instead of fetching.
  - GUI: paired **rápido / profundo** buttons for similar images, similar audio, "what grew", backup
    comparison and the "can I delete this?" context menu; reports show the mode they used. After a fast
    run, the deep button's hover shows an estimate built from that measurement (audio ×6 for the longer
    window, images by the unfinished fraction, growth by the unattributed share, backup ×5 when
    truncated), e.g. `Última corrida rápida: 340 ms → profundo ≈ 2,0 s`.
  - Deep audio raises the measured window from 20 s to 120 s per copy and the wall-clock budget to
    60 s; deep images allow the full 120 s budget; deep growth resolves every changed entry (fast caps
    at the 2,000 largest); deep backup compares up to 250,000 files (fast 50,000); deep integrity scans
    50,000 files (fast 3,000).

### Changed
- `growth` and `advice` now accept `deep`; `advice` deep checks git tracking, fast explains that it did
  not and lowers `confidence` accordingly.

## [0.12.0] - 2026-11-20

### Added
- "Why is it full?" — top folders/files with percentages, free space and the chain of biggest
  children down to the leaf that holds the space.
- "What grew?" — diffs the two newest snapshots (`.wzsnap`) and attributes each new/grown file to
  its parent folder, returning a ranked folder list plus the biggest new/grown files. Exposed as the
  `growth` MCP tool; the GUI reuses `Guardar snapshot` and warns that the index must be refreshed first.
- "Can I delete this?" — per-file/folder verdict (safe / caution / keep) with reasons: regenerable
  build output/artifacts, temp/cache, git-tracked, hard links, cloud placeholders, system folders,
  personal data, age and size; reports the real reclaimable bytes and stages the path in the basket.
- "Is it backed up?" — compares a folder against another copy by relative path and size: coverage %,
  missing bytes, same-name-different-size and extras only in the backup.
- Audio A/B comparison (experimental): decodes both copies to the same rate, plays them from the
  same frame position and alternates which one is audible (0.5–4 s or manual A/B), with blind mode,
  side swap and a position slider. Up to two minutes per file.
- Image zoom comparison (experimental): shows the same normalised region of both copies side by side
  at 1×–16×; moving the mouse pans both, so resized/recompressed copies reveal their real detail.
- Four new read-only MCP tools: `why_full`, `growth`, `advice`, `backup_check`.
- Visual result panels instead of raw JSON:
  - Similar files: groups with the copy to keep, candidates, quality score, reasons, AI markers and
    thumbnails for images ("CONSERVAR" / "CANDIDATO").
  - GitHub mode: colored status headline, disk vs tracked vs delta per folder, collapsible commit
    and changed-file lists, notes and alternatives.
- Type filter sent to the left explorer from the baobab/rings view (↙ button): the tree rescales to
  the type's bytes, the files list keeps only that type, and a chip in the tab bar removes it.
- Similar audio now judges the actual sound, not only tags: up to two minutes per copy are decoded to
  compare noise floor, SNR, digital silence, high-frequency content and spectral stability. Denoised
  copies are matched with the original (markers such as `(denoised)` or `noise reduced` are ignored for
  grouping) and the result explains which copy is cleaner, which kept more air and which shows denoise
  artifacts. Bounded to 8 copies per group; WMA/APE/WV fall back to metadata only.
- Action basket (experimental) with dry-run, explicit confirmation and JSONL audit: stages hardlink
  merges (content verified), WOF LZX compression and reversible quarantine (moved to
  `%LOCALAPPDATA%\WizEverything\quarantine` with a manifest). Failures stay in the basket and are listed.

## [0.11.0] - 2026-11-20

### Added
- "Similar files" analysis (experimental) (MCP `similar_images`, `similar_audio` + Space analysis menu):
  - Images: perceptual hashes (dHash + aHash) with LSH banding to find the same photo resized,
    recompressed or exported twice, and a quality score that suggests which copy to keep.
  - Audio: groups the same track across formats/bitrates by artist + title (or filename) and
    duration, splits live/remix versions, and ranks lossless over lossy.
  - Both are read-only and return per-group keep/candidates reasons plus reversible alternatives
    (quarantine, keep both edits, archive, hardlink when byte-identical).
- GitHub mode (`github_mode` + Space analysis menu, experimental) for any chosen folder: up to date / local newer /
  behind / diverged headline, plus commits, changed files, size and per-folder gaps. Read-only;
  `git fetch` is optional.

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
