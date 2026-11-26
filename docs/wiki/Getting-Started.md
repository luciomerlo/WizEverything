# Getting Started

## Requirements

- Windows 10 or 11, x64.
- An index engine:
  - **Everything** (recommended for all local volumes), or
  - **Census** (alternative NTFS indexer), or
  - WizEverything's **Auto** mode, which picks whichever engine is available.
- Optional for OCR/content extraction: Tesseract and Poppler. WizEverything detects them as
  addons and shows setup instructions; it does not install them silently.

## Download

Download the latest release from:

<https://github.com/luciomerlo/WizEverything/releases>

The release provides:

- a self-contained `wizeverything.exe` (no Visual C++ runtime needed), and
- a ZIP with the executable and documentation.

## First run

1. Start Everything or Census.
2. Start WizEverything.
3. Choose a drive or “All”.
4. Pick a folder in the tree; the treemap/rings and type panel update.
5. Open **Options → Index engine** if you need to switch between Auto, Everything, and Census.

If no engine is found, the startup dialog offers to install Everything or point to
`census.exe`. Nothing is installed or elevated without an explicit click.

## Where settings live

`%APPDATA%\WizEverything\settings.ini`

Relevant keys:

- `lang`
- `theme` (`system`, `light`, `dark`, `contrast`)
- `engine` (`auto`, `everything`, `census`)
- `census_path`
- `show_treemap`, `show_exts`, `chart`
- `close_to_tray`
- `excluded_drives`
- `dup_exclude*`, `dup_prefer`
- `groq_key`

App data is under `%LOCALAPPDATA%\WizEverything\`:

- `volumes/`
- `quarantine/`
- `snapshots/`
- `reports/`
- `hash_cache.bin`
- `audit.jsonl`
- `tessdata/`

## Uninstall

WizEverything is portable. Delete the executable. To remove all state, delete the two folders
above. No system service is installed.
