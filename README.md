# pdf-indexer

Inject a navigable table-of-contents outline into any PDF.

## Usage

```bash
python3 add_pdf_outline.py book.pdf --json outline.json -o book_indexed.pdf
python3 add_pdf_outline.py book.pdf --toc -o book_indexed.pdf
python3 add_pdf_outline.py book.pdf --json outline.json --arabic-start 1:21 --roman-start vii:5
```

Dependencies: **Ghostscript** (`gs`) and **pdftotext** (for `--toc` only).

## JSON spec

```json
{
  "arabic_start_logical": 1,  "arabic_start_pdf": 21,
  "roman_start_logical": 7,   "roman_start_pdf": 5,
  "outline": [
    {"title": "Preface", "page": 7, "roman": true},
    {"title": "1 Introduction", "page": 1, "children": [
      {"title": "1.1 Background", "page": 2}
    ]}
  ]
}
```

- `arabic_start_logical` / `arabic_start_pdf` — page mapping for Arabic-numbered body
- `roman_start_logical` / `roman_start_pdf` — same for Roman-numeral front matter
- `"roman": true` — marks front-matter pages
- `"children"` — nested sections, any depth
