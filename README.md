# pdf-indexer

Inject a navigable table-of-contents outline into any PDF — Rust port.

Reads a hierarchical outline spec (JSON), maps logical book page numbers to
physical PDF pages, and writes the outline tree as PDF bookmark objects.

## Build

```bash
cargo build --release
```

The binary lands at `target/release/pdf-indexer`.

## Usage

```bash
# From a JSON specification
pdf-indexer book.pdf --json outline.json -o book_indexed.pdf

# Override page offsets from the CLI
pdf-indexer book.pdf --json outline.json --arabic-start 1:21 --roman-start vii:5
```

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

## How it works

1. Loads the PDF via `lopdf`, finds all page objects (`/Type /Page`)
2. Recursively builds outline items — each gets a `/Title`, `/Dest` (page + XYZ), `/Parent`, `/Prev`, `/Next`, and `/Count` (−N for collapsed children)
3. Creates the `/Outlines` root object, updates the catalog entry
4. `doc.save()` rebuilds the cross-reference table and writes the output

## Dependencies

| Crate | Role |
|-------|------|
| `lopdf` | PDF read, object manipulation, write (xref rebuild) |
| `clap` | CLI argument parsing |
| `serde` / `serde_json` | JSON outline spec deserialization |
| `pdf_oxide` | reserved for future validation / parsing (WIP; no write API as of 0.1) |
