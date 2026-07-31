# pdf-indexer

Inject a navigable table-of-contents outline into any PDF — Rust port.

Reads a hierarchical outline spec (JSON) **or** auto-extracts the TOC
from the PDF's own contents pages via `pdftotext`, maps logical book
page numbers to physical PDF pages, and writes the outline tree as
PDF bookmark objects.

## Build

```bash
cargo build --release
```

The binary lands at `target/release/pdf-indexer`.

## Usage

```bash
# Graphical TOC editor (Linux / Windows / macOS)
pdf-indexer --gui

# From a JSON specification
pdf-indexer book.pdf --json outline.json -o book_indexed.pdf

# Auto-extract TOC from the PDF's own Contents/Inhalt pages
pdf-indexer book.pdf --toc -o book_indexed.pdf

# Override page offsets from the CLI
pdf-indexer book.pdf --json outline.json --arabic-start 1:21 --roman-start vii:5
```

When running `--toc`, the tool first looks for an existing outline JSON next
to the PDF (`<stem>-outline.json`, `<stem>.json`, or `<stem>_outline.json`,
case-insensitively) and uses it instead of auto-extraction when found.

## GUI (`--gui`)

`pdf-indexer --gui` opens a native window (via `eframe`/egui — a single binary
on Linux, Windows, and macOS):

1. **Open PDF…** pick the input file; the output path defaults to
   `<input>_indexed.pdf` and can be edited in the UI.
2. If an outline JSON already exists next to the PDF (`SICP-outline.json`
   next to `SICP.pdf`, etc.), it is loaded automatically.
3. **Auto-extract TOC** runs the same `pdftotext`-based extraction as `--toc`
   and shows every detected entry as a collapsible, editable tree.
4. Tick the chapters to include (or use **Select all / Select none**), edit
   titles, logical page numbers, and the Arabic/Roman page anchors live.
5. **Export JSON…** writes the current selection in the JSON format below —
   useful for later CLI runs. **Index PDF…** runs the bookmark injection
   directly from the GUI.

Extraction and indexing run on background threads, so the window stays
responsive while the PDF is processed. The GUI can also **Load JSON…** an
existing spec to review or modify it before indexing.

## JSON outline format

```json
{
  "arabic_start_logical": 1,
  "arabic_start_pdf": 21,
  "roman_start_logical": 7,
  "roman_start_pdf": 5,
  "outline": [
    {"title": "Preface", "page": 7, "roman": true},
    {"title": "1 Introduction", "page": 1, "children": [
      {"title": "1.1 Background", "page": 2}
    ]}
  ]
}
```

- `arabic_start_logical` / `arabic_start_pdf` — page mapping for Arabic-numbered body (e.g. book page "1" = PDF page 21)
- `roman_start_logical` / `roman_start_pdf` — same for Roman-numeral front matter (e.g. "vii" = PDF page 5)
- `"roman": true` — marks front-matter pages; absent means Arabic numbering
- `"children"` — nested sections, any depth

## Features

### `--toc` auto-extraction

Automatically locates the table-of-contents page and extracts all entries.
Supports both English ("Contents", "Table of Contents") and German
("Inhalt", "Inhaltsverzeichnis") headings. The tool:

1. Runs `pdftotext` (requires **poppler-utils**) and scans for the TOC page
2. Detects page-number mapping offsets: Roman front-matter via Preface/Vorwort,
   Arabic body via Chapter 1 / first numbered section
3. Builds a blank-page-aware `PageNumberMap` from printed page numbers,
   **sanitized** to discard cross-reference running headers that some
   scholarly editions place alongside real page numbers
   (e.g. "97 | 99  Zweytes Kapitel ...  381" correctly resolves to page 381)
4. Recursively nests entries by section numbering (dot-depth and section
   boundaries)

### Page-number extraction robustness

The `PageNumberMap` handles:

- **Blank pages** — absent from the extracted text, skipped automatically
- **Cross-reference headers** — scholarly editions that print
  GW21-style references ("6 | VII f.") alongside the real page number
  are handled with median-outlier filtering
- **Section-title pages** — chapter openings that show only "5" or "VI"
  are treated as chapter breaks rather than page number entries

### Page-mapping offsets

Books often re-start page numbering in the body (Arabic) after Roman-numeral
front matter.  Set the anchor once in the JSON:

```json
{"arabic_start_logical": 11, "arabic_start_pdf": 12}
```

means "book page 11 starts on physical PDF page 12".  Or use `--arabic-start`
/ `--roman-start` on the command line to override the JSON values.

## How it works

1. Loads the PDF via `lopdf`, finds all page objects (`/Type /Page`)
2. Recursively builds outline items — each gets a `/Title`, `/Dest`
   (page + XYZ), `/Parent`, `/Prev`, `/Next`, and `/Count`
   (−N for collapsed children)
3. Creates the `/Outlines` root object, updates the catalog entry
4. `doc.save()` rebuilds the cross-reference table and writes the output

No Ghostscript dependency.

## Dependencies

| Crate | Role |
|-------|------|
| `lopdf` | PDF read, object manipulation, write (xref rebuild) |
| `clap` | CLI argument parsing |
| `serde` / `serde_json` | JSON outline spec deserialization |
| `eframe` / `egui` | Cross-platform GUI (glow backend) |
| `rfd` | Native open/save file dialogs |

**Runtime requirement:** `pdftotext` (from `poppler-utils`) for `--toc` mode.
