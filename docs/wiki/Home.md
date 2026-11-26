# WizEverything Wiki

WizEverything is a Windows disk-space analyzer that reads an existing file index instead of
walking the disk. It is built for the question:

> “What is using this space, what is it, and what can I safely do about it?”

It is **not** a replacement for Everything or Census. Those provide the index; WizEverything
turns it into a visual, searchable analysis.

## Start here

- [Getting Started](Getting-Started)
- [Index Engines](Index-Engines)
- [Duplicates and Content Search](Duplicates-and-Content)
- [Snapshots and Offline Volumes](Snapshots-and-Offline)
- [Safety and Cleanup](Safety-and-Cleanup)
- [MCP Server](MCP)
- [Troubleshooting](Troubleshooting)
- [FAQ](FAQ)

## What it does

- Tree view, treemap, and Baobab-style rings.
- File-type breakdown, filters, real on-disk size, and CSV export.
- Quick and deep duplicate detection, including duplicate folders.
- Content search and OCR (with optional external addons).
- “Why is it full?”, “What grew?”, “Can I delete this?”, “Is it backed up?”.
- Snapshots, offline catalogs, and comparison between machines.
- Advanced storage engines: hardlinks, cloud/sparse audit, dev artifacts, magic-byte integrity,
  cold data and WOF compression.
- MCP server for AI agents.

## What it does not do

- It does not read the MFT itself. It uses Everything or Census.
- It is not a general file organizer. SortEverything owns plans, moves, copies, and undo.
- It does not replace Everything/Census as a resident indexer.
- It does not hydrate Google Drive or other cloud placeholders.
- It does not index `.eml`/`.msg` content.

## Core vs experimental

The app has grown a lot. Treat these as the stable core:

1. Disk usage views.
2. Duplicates.
3. Content search.
4. Explanations and recommendations.
5. Safe cleanup.
6. Snapshots and offline volumes.

Everything else is useful but optional. Experimental features are marked `(exp)` in the UI and
`(Experimental)` in hover help. See [FAQ](FAQ) for the scope discussion.

## Release

Current release: **v0.18.0**.

<https://github.com/luciomerlo/WizEverything/releases>
