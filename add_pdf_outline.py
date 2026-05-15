#!/usr/bin/env python3
"""
add_pdf_outline.py — Inject a navigable table-of-contents outline into a PDF.

Two modes:
  --json SPEC     Read outline from a JSON specification file.
  --toc           Attempt to auto-extract the TOC from the PDF's own contents pages.

Dependencies: Ghostscript ('gs') must be on PATH.

JSON spec example (see also planar_ising_outline.json in this directory):
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

arabic_start_logical / arabic_start_pdf:
    The logical (book-printed) page number and corresponding physical PDF page
    number for the first Arabic-numbered page (usually Chapter 1 page 1).
roman_start_logical / roman_start_pdf:
    Same for Roman-numeral front-matter pages (e.g., Preface = vii → PDF page 5).

All other pages are linearly interpolated from these anchors.
"""

import argparse, json, os, re, subprocess, sys, tempfile
from typing import Optional


# ---------------------------------------------------------------------------
# 1.  Outline data model
# ---------------------------------------------------------------------------

class OutlineNode:
    """A single node in the outline tree."""
    def __init__(self, title: str, logical_page: int, is_roman: bool = False,
                 children: Optional[list["OutlineNode"]] = None):
        self.title = title
        self.logical_page = logical_page
        self.is_roman = is_roman
        self.children = children or []


# ---------------------------------------------------------------------------
# 2.  Page mapping helpers
# ---------------------------------------------------------------------------

def compute_pdf_page(logical: int, is_roman: bool,
                     arabic_start_logical: int, arabic_start_pdf: int,
                     roman_start_logical: int, roman_start_pdf: int) -> int:
    """Convert a logical (book-printed) page to a 1-based PDF physical page."""
    if is_roman:
        return logical - roman_start_logical + roman_start_pdf
    else:
        return logical - arabic_start_logical + arabic_start_pdf


# ---------------------------------------------------------------------------
# 3.  PostScript string encoding
# ---------------------------------------------------------------------------

def ps_string(s: str) -> str:
    """Encode a Python string as a PostScript string literal (UTF-16BE when needed)."""
    try:
        s.encode("ascii")
        escaped = s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
        return f"({escaped})"
    except UnicodeEncodeError:
        utf16 = s.encode("utf-16-be")
        hex_str = "".join(f"{b:02X}" for b in utf16)
        return f"<FEFF{hex_str}>"


# ---------------------------------------------------------------------------
# 4.  pdfmark generator
# ---------------------------------------------------------------------------

def generate_pdfmark_flat(nodes: list[OutlineNode], args) -> str:
    """Generate a complete pdfmark PostScript snippet for a list of top-level nodes."""
    lines: list[str] = []

    def walk(node: OutlineNode):
        pg = compute_pdf_page(node.logical_page, node.is_roman,
                              args.arabic_start_logical, args.arabic_start_pdf,
                              args.roman_start_logical, args.roman_start_pdf)
        title = ps_string(node.title)
        n_children = len(node.children)
        if n_children > 0:
            lines.append(f"[/Page {pg} /Title {title} /Count {n_children} /OUT pdfmark")
        else:
            lines.append(f"[/Page {pg} /Title {title} /OUT pdfmark")
        for child in node.children:
            walk(child)

    for node in nodes:
        walk(node)
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# 5.  JSON specification loader
# ---------------------------------------------------------------------------

def parse_json_spec(path: str) -> tuple[list[OutlineNode], argparse.Namespace]:
    """Load outline from a JSON file, returning (nodes, args_namespace)."""
    with open(path, "r", encoding="utf-8") as f:
        spec = json.load(f)

    class Args:
        pass
    args = Args()
    args.arabic_start_logical = spec.get("arabic_start_logical", 1)
    args.arabic_start_pdf     = spec.get("arabic_start_pdf", 1)
    args.roman_start_logical  = spec.get("roman_start_logical", 1)
    args.roman_start_pdf      = spec.get("roman_start_pdf", 1)

    def build(node_dict):
        return OutlineNode(
            title=node_dict["title"],
            logical_page=node_dict["page"],
            is_roman=node_dict.get("roman", False),
            children=[build(c) for c in node_dict.get("children", [])]
        )

    nodes = [build(n) for n in spec["outline"]]
    return nodes, args


# ---------------------------------------------------------------------------
# 6.  Auto-TOC extraction
# ---------------------------------------------------------------------------

def auto_extract_toc(pdf_path: str) -> tuple[list[OutlineNode], argparse.Namespace]:
    """
    Attempt to auto-extract the table of contents from the PDF's own contents pages.
    This is best-effort and may need manual correction for complex layouts.
    """
    result = subprocess.run(
        ["pdftotext", "-layout", pdf_path, "-"],
        capture_output=True, text=True
    )
    if result.returncode != 0:
        sys.exit(f"pdftotext failed: {result.stderr}")

    pages = result.stdout.split("\f")

    # Look for the Contents/TOC page
    toc_start = None
    for i, page_text in enumerate(pages):
        for line in page_text.strip().split("\n"):
            if line.strip().lower() in ("contents", "table of contents"):
                toc_start = i
                break
        if toc_start is not None:
            break

    if toc_start is None:
        sys.exit("Could not find a 'Contents' page in the PDF.")

    # Find where TOC ends (blank page or start of chapter 1)
    toc_end = None
    for i in range(toc_start + 1, min(toc_start + 5, len(pages))):
        text = pages[i].strip()
        if not text or re.search(r'(?m)^1\s+\w', text):
            toc_end = i
            break
    if toc_end is None:
        toc_end = toc_start + 3

    # Collect TOC text
    toc_text = "\n".join(pages[toc_start:toc_end])

    # Parse section entries
    entries = []
    for line in toc_text.split("\n"):
        line = line.strip()
        if not line:
            continue
        m = re.match(
            r"^((?:[A-F]|\d+)(?:\.\d+)*\s+)?(.+?)\s+(\d{1,4}|[ivxlcdm]+)\s*$",
            line, re.IGNORECASE
        )
        if m:
            num = m.group(1) or ""
            title = m.group(2).rstrip(" .")
            page_str = m.group(3)

            if not num:
                level = 1
            else:
                level = num.count(".") + 1

            is_roman = bool(re.match(r"^[ivxlcdm]+$", page_str, re.IGNORECASE))
            if is_roman:
                page = _roman_to_int(page_str)
            else:
                page = int(page_str)

            full_title = f"{num} {title}".strip()
            entries.append((level, full_title, page, is_roman))

    if not entries:
        sys.exit("Could not parse any TOC entries.")

    tree = _build_tree(entries)
    offsets = _guess_offsets(pages, toc_start)

    # --- Prepend a "Contents" node ------------------------------------------
    # Most scientific books have a Contents page in the front matter.  Derive
    # its logical (Roman) page number from the offset anchors, then try to
    # confirm by finding a matching Roman numeral on the actual page.
    toc_pdf = toc_start + 1  # 1-based PDF page
    toc_logical = toc_pdf - offsets.roman_start_pdf + offsets.roman_start_logical

    # Try to find a Roman numeral on the Contents page itself
    toc_page_text = pages[toc_start]
    found_numeral = None
    for m in re.finditer(
        r'\b(x{0,3}(?:ix|iv|v?i{0,3}))\b', toc_page_text, re.IGNORECASE
    ):
        found_numeral = m.group(1)
        # If it matches our computed logical, good; otherwise prefer computed
        break

    if found_numeral is not None:
        toc_logical = max(toc_logical, _roman_to_int(found_numeral))

    tree.insert(0, OutlineNode("Contents", toc_logical, is_roman=True))

    return tree, offsets


def _roman_to_int(s: str) -> int:
    vals = {"i": 1, "v": 5, "x": 10, "l": 50, "c": 100, "d": 500, "m": 1000}
    s = s.lower()
    total = 0
    prev = 0
    for ch in reversed(s):
        cur = vals[ch]
        if cur >= prev:
            total += cur
        else:
            total -= cur
        prev = cur
    return total


def _build_tree(entries: list[tuple]) -> list[OutlineNode]:
    root = []
    stack: list[OutlineNode] = []

    for level, title, page, is_roman in entries:
        node = OutlineNode(title, page, is_roman)
        while stack and len(stack) >= level:
            stack.pop()
        if not stack:
            root.append(node)
        else:
            stack[-1].children.append(node)
        stack.append(node)

    return root


def _guess_offsets(pages: list[str], toc_page_idx: int) -> argparse.Namespace:
    class Args:
        pass
    args = Args()
    args.arabic_start_logical = 1
    args.arabic_start_pdf = 1
    args.roman_start_logical = 1
    args.roman_start_pdf = 1

    # Find first page with "1  Title" pattern (likely Chapter 1)
    for i in range(toc_page_idx + 1, len(pages)):
        text = pages[i].strip()
        if re.search(r'(?m)^1\s+\w', text) or re.search(r'(?m)^Chapter\s+1', text):
            args.arabic_start_pdf = i + 1
            break

    # Find Roman-numeral start (Preface page)
    for i in range(0, toc_page_idx):
        text = pages[i].strip()
        if "Preface" in text:
            args.roman_start_pdf = i + 1
            rmatch = re.search(
                r'\b(x{0,3}(?:ix|iv|v?i{0,3}))\b', text, re.IGNORECASE
            )
            if rmatch:
                args.roman_start_logical = _roman_to_int(rmatch.group(1))
            else:
                # No numeral on the Preface page — check adjacent pages
                args.roman_start_logical = 7
                for j in (i + 1, i - 1):
                    if 0 <= j < toc_page_idx:
                        rmatch2 = re.search(
                            r'\b(x{0,3}(?:ix|iv|v?i{0,3}))\b',
                            pages[j].strip(), re.IGNORECASE
                        )
                        if rmatch2:
                            adj_numeral = _roman_to_int(rmatch2.group(1))
                            # Adjust logical to match the page offset
                            args.roman_start_logical = adj_numeral - (j - i)
                            break
            break

    return args


# ---------------------------------------------------------------------------
# 7.  Ghostscript merge
# ---------------------------------------------------------------------------

def merge_outline(pdf_path: str, pdfmark_path: str, output_path: str):
    cmd = [
        "gs", "-q", "-dBATCH", "-dNOPAUSE",
        "-sDEVICE=pdfwrite",
        "-dPDFSETTINGS=/prepress",
        f"-sOutputFile={output_path}",
        pdfmark_path,
        pdf_path,
    ]
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        sys.exit(f"Ghostscript failed:\n{result.stderr}")


# ---------------------------------------------------------------------------
# 8.  CLI
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="Inject a navigable outline into a PDF.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  add_pdf_outline.py book.pdf --json outline.json -o book_indexed.pdf
  add_pdf_outline.py book.pdf --toc -o book_indexed.pdf
  add_pdf_outline.py book.pdf --toc --arabic-start 1:21 --roman-start vii:5
        """,
    )
    parser.add_argument("pdf", help="Input PDF file")
    parser.add_argument("--json", help="JSON outline specification file")
    parser.add_argument("--toc", action="store_true",
                        help="Auto-extract TOC from PDF Contents page")
    parser.add_argument("-o", "--output", default=None,
                        help="Output PDF path (default: <input>_indexed.pdf)")
    parser.add_argument("--arabic-start", default="1:1",
                        help="Arabic logical_page:pdf_page mapping (e.g. 1:21)")
    parser.add_argument("--roman-start", default="1:1",
                        help="Roman logical_page:pdf_page mapping (e.g. vii:5)")

    opts = parser.parse_args()

    if not opts.json and not opts.toc:
        parser.error("Either --json or --toc is required.")
    if opts.json and opts.toc:
        parser.error("Use --json or --toc, not both.")

    def _parse_offset(s: str):
        parts = s.split(":")
        if len(parts) != 2:
            sys.exit(f"Invalid offset '{s}': expected 'logical:pdf' (e.g., '1:21')")
        logical_str, pdf_str = parts
        # Handle Roman numerals in logical part
        if re.match(r"^[ivxlcdm]+$", logical_str, re.IGNORECASE):
            logical = _roman_to_int(logical_str)
        else:
            logical = int(logical_str)
        return logical, int(pdf_str)

    class Args:
        pass
    args = Args()
    args.arabic_start_logical, args.arabic_start_pdf = _parse_offset(opts.arabic_start)
    args.roman_start_logical, args.roman_start_pdf = _parse_offset(opts.roman_start)

    # Load or extract outline
    if opts.json:
        nodes, spec_args = parse_json_spec(opts.json)
        # JSON spec overrides CLI offsets when explicitly set
        if spec_args.arabic_start_logical != 1 or spec_args.arabic_start_pdf != 1:
            args.arabic_start_logical, args.arabic_start_pdf = (
                spec_args.arabic_start_logical, spec_args.arabic_start_pdf)
        if spec_args.roman_start_logical != 1 or spec_args.roman_start_pdf != 1:
            args.roman_start_logical, args.roman_start_pdf = (
                spec_args.roman_start_logical, spec_args.roman_start_pdf)
    else:
        nodes, auto_args = auto_extract_toc(opts.pdf)
        args.arabic_start_logical = auto_args.arabic_start_logical
        args.arabic_start_pdf = auto_args.arabic_start_pdf
        args.roman_start_logical = auto_args.roman_start_logical
        args.roman_start_pdf = auto_args.roman_start_pdf
        print(f"Guessed offsets: Arabic {args.arabic_start_logical}→"
              f"{args.arabic_start_pdf}, "
              f"Roman {args.roman_start_logical}→{args.roman_start_pdf}")

    pdfmark = generate_pdfmark_flat(nodes, args)

    with tempfile.NamedTemporaryFile(mode="w", suffix=".ps", delete=False) as f:
        f.write(pdfmark)
        pdfmark_path = f.name

    output = opts.output or _default_output(opts.pdf)
    node_count = sum(1 + _count_children(n) for n in nodes)
    print(f"Writing {node_count} outline entries → {output}")
    merge_outline(opts.pdf, pdfmark_path, output)
    os.unlink(pdfmark_path)
    print("Done.")


def _count_children(node: OutlineNode) -> int:
    return len(node.children) + sum(_count_children(c) for c in node.children)


def _default_output(path: str) -> str:
    base, ext = os.path.splitext(path)
    return f"{base}_indexed{ext}"


if __name__ == "__main__":
    main()