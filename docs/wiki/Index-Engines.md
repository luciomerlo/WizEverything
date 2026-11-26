# Index Engines

WizEverything does not walk the whole disk to build its main tree. It asks an index engine for
the file list, then computes what it needs.

## Auto

`Auto` prefers Everything when it is installed and ready. If Everything is unavailable, it can
use Census. The engine is pinned for the current load, so a refresh does not mix results from two
different engines.

## Everything

- Reads Everything's index over IPC.
- Requires Everything to be running and indexing the volumes you want.
- If Everything is installed but a volume is not indexed, WizEverything may see zero files.
  Refresh Everything, or index that volume.

## Census

- Alternative engine with its own NTFS/MFT index.
- Uses the `QUERY2W` IPC protocol and the `CENSUS_IPC` window class.
- Runs read-only from WizEverything's point of view: WizEverything never edits `C.wcat` or the
  Census index.
- `Auto` never mixes Everything and Census rows.

## Google Drive and other cloud drives

Cloud drives are not local NTFS volumes. Do not expect MFT-style behavior.

- Quick duplicate search uses metadata and does not read file content.
- Deep analysis, hashing, OCR, archives, and cross-volume copies can force the cloud client to
  download (“hydrate”) files. This is slow and can consume disk and quota.
- WizEverything's rule is metadata-only for placeholders: do not hydrate automatically.
- If a cloud drive is slow, exclude it from the “All” view or analyze a local folder instead.

## If the engine fails

- **Everything installed but empty results**: index the volume in Everything and refresh.
- **Access denied / UIPI**: run Everything/Census at the same integrity level, or use Census's
  supported non-elevated client path.
- **Census not found**: set `census_path` in settings or in the startup dialog.
- **Stale tree**: refresh the engine, then refresh WizEverything.
