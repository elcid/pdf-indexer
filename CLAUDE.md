# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

pdf-indexer — Inject navigable table-of-contents outline (PDF bookmarks) into a PDF. Rust port of `add_pdf_outline.py`, using `lopdf` for direct PDF object manipulation instead of Ghostscript pdfmarks.

Two modes:
- `--json SPEC` — read outline from a JSON specification file
- `--toc` — auto-extract TOC from the PDF's own contents pages via `pdftotext` (requires `poppler-utils`)

## Build / Test / Run

```bash
cargo build --release          # binary → target/release/pdf-indexer
cargo check                     # fast compile-check (no output)
cargo fmt                       # format source
cargo clippy                    # lint
cargo test                      # run tests (currently none)
cargo run -- book.pdf --json outline.json -o out.pdf
```

## Architecture

Single-file Rust binary — `src/main.rs` (~900 lines). No modules; all logic lives in one file with section-header comments.

### PDF manipulation: lopdf 0.40

The key data type is `ObjectId` = `(u32, u16)`. The "null" sentinel is `(0, 0)`.

**Critical: page ordering.** `doc.objects` is a `BTreeMap<ObjectId, Object>` — iterating it yields arbitrary order, NOT document order. Always walk the `/Pages` tree from the catalog (`/Root` → `/Pages` → `/Kids` …) to get pages in document order. See `collect_page_ids()` and `walk_page_tree()`. Leaf nodes may omit `/Type` (match both `Some(b"Page")` and `None`).

**PDF string encoding** (`pdf_string()`): pure ASCII → `(literal)` with `StringFormat::Literal`; non-ASCII → `<FEFF…>` (UTF-16BE + BOM) with `StringFormat::Hexadecimal`.

**Outline tree construction** (`build_outline()`): two-phase approach at each level:
1. Create all sibling dictionaries (recurse into children first to get their First/Last/Count).
2. Link Prev/Next between adjacent siblings.

This avoids the fragile "predict next ID" pattern from earlier versions.

New objects use `next_id += 1` with ID `(next_id, 0)`. `doc.max_id` must be updated before saving.

### Page mapping

Logical (book-printed) page numbers → physical 1-based PDF pages. Two numbering schemes:
- **Arabic** — body pages (Chapter 1 = page 1). Offset via `--arabic-start logical:pdf` (e.g. `1:21`).
- **Roman** — front matter (Preface = vii). Offset via `--roman-start logical:pdf` (e.g. `vii:5`).

Offsets can come from the JSON spec or CLI overrides. CLI overrides take precedence when not the default `"1:1"`.

### TOC auto-extraction (`--toc`)

Pipeline: `pdftotext -layout` → split pages on form-feed (`\x0c`) → find "Contents" page → parse entries → guess page offsets → build tree.

`guess_offsets()` extracts the Roman anchor from TOC page headers (e.g. "xiv Contents") rather than hunting for "Preface" in body text — much more reliable. The Arabic anchor is the first page after the TOC that looks like Chapter 1.

`build_toc_tree()` uses a flat Vec with parent-child indices, then converts to recursive `OutlineEntry`. This avoids a clone bug where the stack held disconnected copies.

### Dependencies

| Crate | Role |
|-------|------|
| `lopdf` 0.40 | PDF read, object manipulation, write (xref rebuild) |
| `clap` 4 | CLI argument parsing (derive mode) |
| `serde` / `serde_json` | JSON outline spec deserialization |
| `anyhow` | Error handling |

### Test PDFs

The PDFs in the repo root (`AGT.pdf`, `Algebraic Graph Theory - Chris Godsil.pdf`) are test inputs. They are git-ignored (`.gitignore` covers `*.pdf`).

### Python reference

`add_pdf_outline.py` is the original Python implementation that this Rust port replaces. It uses Ghostscript pdfmarks. The Rust port's TOC auto-extraction logic (`parse_toc_entries`, `build_toc_tree`, `guess_offsets`) is derived from the Python version but improved — notable difference: the Rust version extracts Roman offsets from TOC page headers rather than body text.
