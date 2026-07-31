// pdf-indexer — Inject a navigable table-of-contents outline into any PDF.
//
// Two modes:
//   --json SPEC    Read outline from a JSON specification file.
//   --toc          Auto-extract the TOC from the PDF's own contents pages
//                  via pdftotext (requires poppler-utils).
//
// The outline is written directly as PDF bookmark objects using lopdf;
// no Ghostscript dependency.
//
// This library crate holds the whole pipeline so both the CLI binary
// (`main.rs`) and the GUI (`gui.rs`) can call the same functions.

use anyhow::{Context, Result};
use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ═══════════════════════════════════════════════════════════════════════════════
//  1. Data model
// ═══════════════════════════════════════════════════════════════════════════════

/// The JSON outline specification format (also written by the GUI).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OutlineSpec {
    #[serde(default = "one")]
    pub arabic_start_logical: u32,
    #[serde(default = "one")]
    pub arabic_start_pdf: u32,
    #[serde(default = "one")]
    pub roman_start_logical: u32,
    #[serde(default = "one")]
    pub roman_start_pdf: u32,
    pub outline: Vec<OutlineEntry>,
}

/// A single TOC entry; children nest arbitrarily deep.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OutlineEntry {
    pub title: String,
    pub page: u32,
    #[serde(default)]
    pub roman: bool,
    #[serde(default)]
    pub children: Vec<OutlineEntry>,
}

const fn one() -> u32 {
    1
}

/// Page-number mapping between logical (printed) book pages and physical PDF pages.
#[derive(Clone)]
pub struct PageMapping {
    pub arabic_start_logical: u32,
    pub arabic_start_pdf: u32,
    pub roman_start_logical: u32,
    pub roman_start_pdf: u32,
    /// When set, maps logical page → PDF page for Arabic-numbered body pages.
    /// Handles blank-page discontinuities that the linear formula misses.
    pub page_map: PageNumberMap,
}

impl Default for PageMapping {
    fn default() -> Self {
        Self {
            arabic_start_logical: 1,
            arabic_start_pdf: 1,
            roman_start_logical: 1,
            roman_start_pdf: 1,
            page_map: PageNumberMap::empty(),
        }
    }
}

impl PageMapping {
    /// Rebuild the blank-page-aware map from raw page texts.
    ///
    /// Call this after editing `arabic_start_pdf` when the mapping came from
    /// `extract_toc`. Pass an empty slice to drop the map entirely (which is
    /// correct for JSON-loaded specs, where only the linear formula applies).
    pub fn rebuild_page_map(&mut self, pages: &[String]) {
        if pages.is_empty() {
            self.page_map = PageNumberMap::empty();
        } else {
            let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
            self.page_map = PageNumberMap::build(&refs, self.arabic_start_pdf);
        }
    }
}

/// Result of auto-extracting a TOC from a PDF's contents pages.
pub struct TocExtraction {
    pub entries: Vec<OutlineEntry>,
    pub mapping: PageMapping,
    /// Raw text of every PDF page (pdftotext `-layout` output, split on form
    /// feeds). Needed by the GUI to rebuild the page map after the user edits
    /// the page-number anchors.
    pub page_texts: Vec<String>,
}

// ═══════════════════════════════════════════════════════════════════════════════
//  2. Roman numerals
// ═══════════════════════════════════════════════════════════════════════════════

fn roman_to_int(s: &str) -> Option<u32> {
    if s.is_empty()
        || !s.chars().all(|c| {
            matches!(
                c.to_ascii_lowercase(),
                'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm'
            )
        })
    {
        return None;
    }

    let vals: HashMap<char, u32> = [
        ('i', 1),
        ('v', 5),
        ('x', 10),
        ('l', 50),
        ('c', 100),
        ('d', 500),
        ('m', 1000),
    ]
    .into_iter()
    .collect();

    let mut total = 0u32;
    let mut prev = 0u32;
    for ch in s.chars().rev() {
        let cur = vals[&ch.to_ascii_lowercase()];
        if cur >= prev {
            total += cur;
        } else {
            total = total.wrapping_sub(cur);
        }
        prev = cur;
    }
    Some(total)
}

// ═══════════════════════════════════════════════════════════════════════════════
//  3. Page mapping  (with blank-page-aware page-number map)
// ═══════════════════════════════════════════════════════════════════════════════

/// Map from logical (printed) page numbers to 1-based physical PDF pages.
///
/// Built by extracting printed page numbers from the pdftotext output.
/// For body pages, the first-number-on-first-line is the page number;
/// chapter-title pages (single small number like "5" or "6") are skipped
/// and interpolated via their neighbours at lookup time.
///
/// The linear `PageMapping` offsets are used as a fallback for pages that
/// fall outside the map's range (e.g. Roman front matter).
#[derive(Clone)]
pub struct PageNumberMap {
    /// Sorted list of `(logical_page, pdf_page)` for body pages.
    entries: Vec<(u32, u32)>,
}

impl PageNumberMap {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Build from pdftotext pages, starting at `body_start` (1-based PDF page
    /// where Chapter 1 begins).
    pub fn build(pages: &[&str], body_start: u32) -> Self {
        let mut entries = Vec::new();
        for (i, page_text) in pages.iter().enumerate() {
            let pdf_page = i as u32 + 1;
            if pdf_page < body_start {
                continue;
            }
            if let Some(logical) = extract_page_number(page_text) {
                entries.push((logical, pdf_page));
            }
        }
        // Sort by logical page (should already be in order, but be safe)
        entries.sort_by_key(|(l, _)| *l);
        // Deduplicate — keep the first occurrence (earliest PDF page)
        entries.dedup_by_key(|(l, _)| *l);

        // Sanitize: remove entries where |logical - pdf_page| is an outlier vs the
        // median offset.  Cross-reference running headers (e.g. "97 | 99 ... 381"
        // or "100 ... 5 | VI") extract bogus numbers; the real page is always the
        // one whose offset from the PDF page is consistent with its neighbours.
        if entries.len() >= 4 {
            let mut offsets: Vec<i64> =
                entries.iter().map(|(l, p)| *p as i64 - *l as i64).collect();
            offsets.sort();
            let median = offsets[offsets.len() / 2];
            entries.retain(|(l, p)| {
                let offset = *p as i64 - *l as i64;
                (offset - median).unsigned_abs() <= 50
            });
        }

        Self { entries }
    }

    /// Return the 1-based PDF page for a logical page number.
    ///
    /// Finds the nearest entry with `logical >= requested` and walks backward
    /// from it.  Because blank pages are *not* in the PDF, the forward-looking
    /// offset from the reference entry already accounts for any blank-page
    /// discontinuities between the reference and the requested page.
    pub fn get_pdf_page(&self, logical: u32) -> Option<u32> {
        if self.entries.is_empty() {
            return None;
        }
        // Binary search for first entry where entry.logical >= logical
        let idx = match self.entries.binary_search_by_key(&logical, |(l, _)| *l) {
            Ok(idx) => idx,
            Err(idx) => idx,
        };

        if idx < self.entries.len() {
            let (l_ref, p_ref) = self.entries[idx];
            // Walk backward from the reference: every logical unit = one PDF page
            let offset = l_ref as i64 - logical as i64;
            let pdf = p_ref as i64 - offset;
            if pdf > 0 {
                return Some(pdf as u32);
            }
        }

        // Fallback: extend forward from the last known entry
        let (l_last, p_last) = self.entries[self.entries.len() - 1];
        if logical >= l_last {
            return Some(p_last + (logical - l_last));
        }

        None
    }
}

/// Extract the printed page number from a page's text content.
///
/// Returns `None` for pages where the page number cannot be reliably
/// determined (e.g. chapter-title pages that show only the chapter number).
fn extract_page_number(page_text: &str) -> Option<u32> {
    let first_line = page_text.trim().lines().next()?.trim();
    if first_line.is_empty() {
        return None;
    }

    let words: Vec<&str> = first_line.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }

    let first = words[0];
    let first_has_dot = first.contains('.') && first.chars().any(|c| c.is_ascii_digit());

    // Collect all plain numeric tokens (no dots, no other chars)
    let numbers: Vec<u32> = words.iter().filter_map(|w| w.parse::<u32>().ok()).collect();

    if numbers.is_empty() {
        return None;
    }

    if first_has_dot {
        // Section header like "1.2. Subgraphs  3" — last number is page
        return numbers.last().copied();
    }

    if let Ok(n) = first.parse::<u32>() {
        // Standalone small number like "5" or "6" → chapter-title page, skip
        if words.len() == 1 && n <= 25 {
            return None;
        }
        // When the line has a page-reference pipe "|" (e.g. "97 | 99 ... 381"),
        // the first number is a cross-reference, not the page number.
        // Use the LAST number instead (the actual printed page).
        if first_line.contains('|') && numbers.len() > 1 {
            return numbers.last().copied();
        }
        // For lines without cross-ref pipes: a small first number with other
        // numbers on the line is likely a section/cross-ref, not the page.
        //   e.g. "6 | VII f.  Vorrede ...  101" → page 101 (caught above by |)
        //   e.g. "17 | XXVIII f. ...  137"        → page 137
        if n <= 25 && numbers.len() > 1 {
            return numbers.last().copied();
        }
        // Otherwise first number is page: "12  Vorwort" or "78  5. Generalized …"
        return Some(n);
    }

    // First word is text ("References", "Preface", "Contents") → last number
    numbers.last().copied()
}

fn compute_pdf_page(logical: u32, is_roman: bool, m: &PageMapping) -> u32 {
    if is_roman {
        let offset_logical = m.roman_start_logical as i64;
        let offset_pdf = m.roman_start_pdf as i64;
        return (logical as i64 - offset_logical + offset_pdf).max(0) as u32;
    }

    // Try the page-number map first (handles blank-page discontinuities)
    if let Some(pdf) = m.page_map.get_pdf_page(logical) {
        return pdf;
    }

    // Fallback: linear formula
    let offset_logical = m.arabic_start_logical as i64;
    let offset_pdf = m.arabic_start_pdf as i64;
    (logical as i64 - offset_logical + offset_pdf).max(0) as u32
}

fn parse_offset(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<&str> = s.splitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!(
            "invalid offset '{}': expected 'logical:pdf' (e.g. '1:21')",
            s
        );
    }

    let logical = roman_to_int(parts[0])
        .or_else(|| parts[0].parse().ok())
        .with_context(|| format!("invalid logical page: '{}'", parts[0]))?;

    let pdf = parts[1]
        .parse::<u32>()
        .with_context(|| format!("invalid PDF page: '{}'", parts[1]))?;

    Ok((logical, pdf))
}

// ═══════════════════════════════════════════════════════════════════════════════
//  4. PDF outline injection  (lopdf — no Ghostscript)
// ═══════════════════════════════════════════════════════════════════════════════

const NULL_ID: ObjectId = (0, 0);

/// Encode a title string for a PDF `/Title` entry.
///
/// - Pure ASCII → literal string `(title)` with `StringFormat::Literal`
/// - Non-ASCII  → hex string `<FEFF…>` (UTF-16BE + BOM) with `StringFormat::Hexadecimal`
fn pdf_string(s: &str) -> (Vec<u8>, StringFormat) {
    if s.is_ascii() {
        (s.as_bytes().to_vec(), StringFormat::Literal)
    } else {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        let mut bytes = vec![0xFE, 0xFF]; // Unicode BOM
        for cu in &utf16 {
            bytes.extend_from_slice(&cu.to_be_bytes());
        }
        (bytes, StringFormat::Hexadecimal)
    }
}

/// Walk the page tree from the catalog to collect page ObjectIds in document order.
///
/// This is critical: `doc.objects` iteration order is arbitrary (BTreeMap key order).
/// The page tree rooted at `/Pages` is the only source of document-order page sequence.
fn collect_page_ids(doc: &Document) -> Result<Vec<ObjectId>> {
    let catalog_id = doc
        .trailer
        .get(b"Root")
        .and_then(|r| r.as_reference())
        .context("PDF trailer missing /Root reference")?;

    let catalog = doc
        .get_object(catalog_id)
        .context("cannot read catalog object")?;
    let catalog_dict = catalog.as_dict().context("catalog is not a dictionary")?;
    let pages_id = catalog_dict
        .get(b"Pages")
        .context("catalog missing /Pages entry")?
        .as_reference()
        .context("/Pages is not an indirect reference")?;

    let mut ids = Vec::new();
    walk_page_tree(doc, pages_id, &mut ids)?;
    Ok(ids)
}

fn walk_page_tree(doc: &Document, node_id: ObjectId, ids: &mut Vec<ObjectId>) -> Result<()> {
    let node = doc
        .get_object(node_id)
        .with_context(|| format!("cannot read page tree node {:?}", node_id))?;
    let dict = node
        .as_dict()
        .with_context(|| format!("page tree node {:?} is not a dictionary", node_id))?;

    let type_name = dict
        .get(b"Type")
        .ok()
        .and_then(|t| t.as_name().ok())
        .map(|n| n.to_vec());

    match type_name.as_deref() {
        Some(b"Pages") => {
            let kids = dict
                .get(b"Kids")
                .with_context(|| format!("Pages node {:?} missing /Kids", node_id))?
                .as_array()
                .with_context(|| format!("/Kids of node {:?} is not an array", node_id))?;
            for kid in kids {
                let kid_ref = kid
                    .as_reference()
                    .with_context(|| format!("kid in node {:?} is not a reference", node_id))?;
                walk_page_tree(doc, kid_ref, ids)?;
            }
        }
        Some(b"Page") | None => {
            ids.push(node_id);
        }
        Some(other) => {
            anyhow::bail!(
                "unexpected page tree node type: {}",
                String::from_utf8_lossy(other)
            );
        }
    }
    Ok(())
}

/// Build a single outline item dictionary (without Prev / Next links).
fn make_outline_dict(
    title: &str,
    page_ref: ObjectId,
    parent_id: ObjectId,
    first_child: ObjectId,
    last_child: ObjectId,
    child_count: i64,
) -> Dictionary {
    let mut d = Dictionary::new();
    let (bytes, fmt) = pdf_string(title);

    d.set(b"Title", Object::String(bytes, fmt));
    d.set(b"Parent", Object::Reference(parent_id));
    d.set(
        b"Dest",
        Object::Array(vec![
            Object::Reference(page_ref),
            Object::Name(b"XYZ".to_vec()),
            Object::Null,
            Object::Null,
            Object::Null,
        ]),
    );

    if child_count > 0 {
        d.set(b"First", Object::Reference(first_child));
        d.set(b"Last", Object::Reference(last_child));
        d.set(b"Count", Object::Integer(-child_count));
    }

    d
}

/// Recursively build outline items.
///
/// Two-phase approach at each level:
///   1. Create all sibling dictionaries (recurse into children first).
///   2. Link Prev / Next between adjacent siblings.
///
/// Returns `(first_id, last_id, total_descendant_count)` for the subtree.
fn build_outline(
    entries: &[OutlineEntry],
    mapping: &PageMapping,
    page_ids: &[ObjectId],
    doc: &mut Document,
    next_id: &mut u32,
    parent_id: ObjectId,
) -> Result<(ObjectId, ObjectId, i64)> {
    if entries.is_empty() {
        return Ok((NULL_ID, NULL_ID, 0));
    }

    let mut item_ids: Vec<ObjectId> = Vec::with_capacity(entries.len());
    let mut total_descendants: i64 = 0;

    for entry in entries {
        *next_id += 1;
        let this_id: ObjectId = (*next_id, 0);

        let pdf_page = compute_pdf_page(entry.page, entry.roman, mapping);
        let Some(page_ref) = page_ids.get(pdf_page as usize - 1).copied() else {
            eprintln!(
                "warning: skipping entry '{}': logical page {} → PDF page {} \
                 is out of range (PDF has {} pages)",
                entry.title,
                entry.page,
                pdf_page,
                page_ids.len()
            );
            // Skip this entry and its children.  The object ID counter has
            // already bumped, but an unused ID is harmless in lopdf.
            continue;
        };

        let (child_first, child_last, child_count) = if entry.children.is_empty() {
            (NULL_ID, NULL_ID, 0)
        } else {
            build_outline(&entry.children, mapping, page_ids, doc, next_id, this_id)?
        };

        let dict = make_outline_dict(
            &entry.title,
            page_ref,
            parent_id,
            child_first,
            child_last,
            child_count,
        );

        doc.objects.insert(this_id, Object::Dictionary(dict));
        item_ids.push(this_id);
        total_descendants += 1 + child_count;
    }

    for i in 0..item_ids.len() {
        if let Some(Object::Dictionary(ref mut d)) = doc.objects.get_mut(&item_ids[i]) {
            if i > 0 {
                d.set(b"Prev", Object::Reference(item_ids[i - 1]));
            }
            if i + 1 < item_ids.len() {
                d.set(b"Next", Object::Reference(item_ids[i + 1]));
            }
        }
    }

    let first = item_ids[0];
    let last = item_ids[item_ids.len() - 1];
    Ok((first, last, total_descendants))
}

/// Point the catalog's `/Outlines` entry at the outline root object.
fn update_catalog(doc: &mut Document, outlines_id: ObjectId) -> Result<()> {
    let catalog_id = doc
        .trailer
        .get(b"Root")
        .and_then(|r| r.as_reference())
        .context("PDF trailer missing /Root reference")?;

    let cat_obj = doc
        .get_object_mut(catalog_id)
        .context("cannot read catalog object for update")?;

    if let Object::Dictionary(ref mut cat) = cat_obj {
        cat.set(b"Outlines", Object::Reference(outlines_id));
    } else {
        anyhow::bail!("catalog is not a dictionary");
    }
    Ok(())
}

/// Orchestrate outline injection: collect page IDs, build outline tree,
/// create /Outlines root, and update the catalog.
fn inject_outline(
    doc: &mut Document,
    entries: &[OutlineEntry],
    mapping: &PageMapping,
) -> Result<()> {
    let page_ids = collect_page_ids(doc)?;
    if page_ids.is_empty() {
        anyhow::bail!("PDF contains no pages");
    }

    let mut next_id = doc.max_id;
    next_id += 1;
    let outlines_id: ObjectId = (next_id, 0);

    let (first, last, total_count) = if entries.is_empty() {
        (NULL_ID, NULL_ID, 0)
    } else {
        build_outline(entries, mapping, &page_ids, doc, &mut next_id, outlines_id)?
    };

    let mut outlines_root = Dictionary::new();
    outlines_root.set(b"Type", Object::Name(b"Outlines".to_vec()));
    if total_count > 0 {
        outlines_root.set(b"First", Object::Reference(first));
        outlines_root.set(b"Last", Object::Reference(last));
    }
    outlines_root.set(b"Count", Object::Integer(total_count));
    doc.objects
        .insert(outlines_id, Object::Dictionary(outlines_root));

    update_catalog(doc, outlines_id)?;
    doc.max_id = next_id;
    Ok(())
}

/// Load a PDF, inject the outline, and save the result to `output`.
pub fn index_pdf(
    input: &Path,
    entries: &[OutlineEntry],
    mapping: &PageMapping,
    output: &Path,
) -> Result<()> {
    let mut doc =
        Document::load(input).with_context(|| format!("cannot load PDF '{}'", input.display()))?;

    inject_outline(&mut doc, entries, mapping).context("failed to inject outline into PDF")?;

    doc.save(output)
        .with_context(|| format!("failed to save output PDF '{}'", output.display()))?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
//  5. JSON outline loader
// ═══════════════════════════════════════════════════════════════════════════════

/// Read an outline spec from a JSON file and return its entries and mapping.
///
/// The returned mapping uses the anchors from the file only; CLI `--arabic-start`
/// / `--roman-start` overrides can be applied afterwards with
/// [`apply_anchor_overrides`].
pub fn load_json(path: &Path) -> Result<(Vec<OutlineEntry>, PageMapping)> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read JSON file '{}'", path.display()))?;
    let spec: OutlineSpec =
        serde_json::from_str(&json).context("failed to parse JSON outline spec")?;

    let mapping = PageMapping {
        arabic_start_logical: spec.arabic_start_logical,
        arabic_start_pdf: spec.arabic_start_pdf,
        roman_start_logical: spec.roman_start_logical,
        roman_start_pdf: spec.roman_start_pdf,
        page_map: PageNumberMap::empty(),
    };

    Ok((spec.outline, mapping))
}

/// Search the PDF's directory for an existing outline JSON spec.
///
/// Candidates, in priority order:
///   1. `<stem>-outline.json` — the file name the GUI exports
///   2. `<stem>.json`
///   3. `<stem>_outline.json`
///   4. case-insensitive variants of the same patterns (so `sicp-outline.json`
///      is found next to `SICP.pdf`)
///
/// Returns `None` when no matching JSON file exists.
pub fn find_outline_json(pdf_path: &Path) -> Option<PathBuf> {
    let stem = pdf_path.file_stem()?.to_string_lossy();
    let dir = pdf_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let exact_names = [
        format!("{stem}-outline.json"),
        format!("{stem}.json"),
        format!("{stem}_outline.json"),
    ];
    for name in exact_names {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    // Case-insensitive fallback: scan the directory for matching JSON files.
    let stem_lower = stem.to_lowercase();
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            if !path.is_file() {
                return false;
            }
            let Some(ext) = path.extension() else {
                return false;
            };
            if !ext.eq_ignore_ascii_case("json") {
                return false;
            }
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase();
            name == stem_lower
                || name == format!("{stem_lower}-outline")
                || name == format!("{stem_lower}_outline")
        })
        .collect();
    matches.sort();
    matches.into_iter().next()
}

/// Override the mapping's page-number anchors with CLI values.
///
/// A value of `"1:1"` (the CLI default) leaves that anchor untouched, so it
/// preserves the JSON file's settings unless the user explicitly overrides.
pub fn apply_anchor_overrides(
    mapping: &mut PageMapping,
    arabic_start: &str,
    roman_start: &str,
) -> Result<()> {
    if arabic_start != "1:1" {
        let (logical, pdf) = parse_offset(arabic_start)?;
        mapping.arabic_start_logical = logical;
        mapping.arabic_start_pdf = pdf;
    }
    if roman_start != "1:1" {
        let (logical, pdf) = parse_offset(roman_start)?;
        mapping.roman_start_logical = logical;
        mapping.roman_start_pdf = pdf;
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
//  6. TOC auto-extraction
// ═══════════════════════════════════════════════════════════════════════════════

/// Locate the TOC in a PDF, parse its entries, and guess the page mapping.
///
/// Requires `pdftotext` (poppler-utils) on `PATH`.
pub fn extract_toc(pdf_path: &Path) -> Result<TocExtraction> {
    // ── 1. Run pdftotext ──────────────────────────────────────────────────
    let output = Command::new("pdftotext")
        .args(["-layout", &pdf_path.to_string_lossy(), "-"])
        .output()
        .context("failed to run pdftotext — is poppler-utils installed?")?;

    if !output.status.success() {
        anyhow::bail!(
            "pdftotext failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let pages: Vec<&str> = text.split('\x0c').collect();

    // ── 2. Find the Contents page ─────────────────────────────────────────
    let toc_start = pages
        .iter()
        .position(|page| {
            page.lines().any(|line| {
                let t = line.trim().to_lowercase();
                let first = t.split_whitespace().next().unwrap_or("");
                // Match heading: "Contents" or "Inhalt" as first word, or
                // multi-word "Table of Contents" / "Inhaltsverzeichnis".
                // Use first-word check to avoid matching copyright CIP data
                // (e.g. "CONTENTS: 1. Programming  1").
                first == "contents"
                    || first == "inhalt"
                    || t.contains("table of contents")
                    || t.contains("inhaltsverzeichnis")
            })
        })
        .context("could not find a 'Contents', 'Inhalt', or 'Table of Contents' page")?;

    // ── 3. Find where the TOC ends ────────────────────────────────────────
    let toc_end = (toc_start + 1..pages.len().min(toc_start + 10))
        .find(|&i| {
            let text = pages[i].trim();
            text.is_empty()
                || text.lines().any(|l| {
                    let l = l.trim().to_lowercase();
                    l == "chapter 1"
                        || l == "1"
                        || (l.starts_with("1 ")
                            && l.chars().nth(2).is_some_and(|c| c.is_alphabetic()))
                })
        })
        .unwrap_or(toc_start + 8);

    let toc_text: String = pages[toc_start..toc_end.min(pages.len())].join("\n");

    // ── 4. Parse individual TOC entries ───────────────────────────────────
    let parsed = parse_toc_entries(&toc_text)?;
    if parsed.is_empty() {
        anyhow::bail!(
            "no TOC entries could be parsed from the Contents/Inhalt page\n  \
             hint: the auto-extractor needs page numbers at the end of each TOC \
             line;\n  \
             if your TOC has no page numbers, use --json instead"
        );
    }

    // ── 5. Guess page offsets ─────────────────────────────────────────────
    let mapping = guess_offsets(&pages, toc_start, toc_end);

    // ── 6. Build outline tree ─────────────────────────────────────────────
    let mut tree = build_toc_tree(&parsed);

    // ── 7. Prepend a "Contents" node ──────────────────────────────────────
    let toc_pdf = toc_start as u32 + 1;
    let toc_logical = toc_pdf.saturating_sub(mapping.roman_start_pdf) + mapping.roman_start_logical;
    tree.insert(
        0,
        OutlineEntry {
            title: "Contents".into(),
            page: toc_logical,
            roman: true,
            children: vec![],
        },
    );

    eprintln!(
        "Guessed offsets: Arabic {}→{}, Roman {}→{}",
        mapping.arabic_start_logical,
        mapping.arabic_start_pdf,
        mapping.roman_start_logical,
        mapping.roman_start_pdf
    );

    Ok(TocExtraction {
        entries: tree,
        mapping,
        page_texts: pages.iter().map(|s| s.to_string()).collect(),
    })
}

fn is_back_matter(word: &str) -> bool {
    let keywords = [
        "glossary",
        "index",
        "bibliography",
        "appendix",
        "notation",
        "symbols",
    ];
    keywords.contains(&word.to_lowercase().as_str())
}

struct TocLine {
    level: u32,
    title: String,
    page: u32,
    is_roman: bool,
}

/// Parse TOC entries from the concatenated text of the contents pages.
///
/// Each line:  `<section-num>  <title>   <page-num>`
///
/// The page number is the *last* recognisable number or Roman numeral on the
/// line. The section number (optional) determines nesting depth.
fn parse_toc_entries(toc_text: &str) -> Result<Vec<TocLine>> {
    let mut entries = Vec::new();
    let mut current_level: u32 = 1;

    for line in toc_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let words: Vec<&str> = line.split_whitespace().collect();
        if words.len() < 2 {
            continue;
        }

        let last_word = words[words.len() - 1];

        let (page, is_roman) = if let Ok(n) = last_word.parse::<u32>() {
            (n, false)
        } else if let Some(n) = roman_to_int(last_word) {
            (n, true)
        } else {
            continue;
        };

        let first_word = words[0];
        let is_numbered = first_word.ends_with('.')
            || first_word.chars().all(|c| c.is_ascii_digit() || c == '.')
            || roman_to_int(first_word).is_some();

        let level = if is_numbered {
            let lvl = if first_word.ends_with('.') {
                let stripped = first_word.trim_end_matches('.');
                stripped.matches('.').count() as u32 + 1
            } else if roman_to_int(first_word).is_some() {
                1
            } else {
                first_word.matches('.').count() as u32 + 1
            };
            current_level = lvl;
            lvl
        } else {
            if is_back_matter(first_word) {
                1
            } else {
                current_level
            }
        };

        let mut title = words[0..words.len() - 1].join(" ");

        if (title.to_lowercase().trim().contains("contents")
            || title.to_lowercase().trim().contains("inhalt"))
            && is_roman
        {
            continue;
        }

        title = title.trim_end_matches(&[' ', '.', '\t'][..]).to_string();
        if title.is_empty() {
            continue;
        }

        entries.push(TocLine {
            level,
            title,
            page,
            is_roman,
        });
    }

    if entries.is_empty() {
        anyhow::bail!("no parseable TOC entries found");
    }

    Ok(entries)
}

/// Build a hierarchical outline tree from flat TOC entries.
///
/// Uses a flat `Vec` with parent-child indices to avoid clone bugs where the
/// stack would hold disconnected copies. Converts to recursive `OutlineEntry`
/// at the end.
fn build_toc_tree(lines: &[TocLine]) -> Vec<OutlineEntry> {
    struct FlatEntry {
        title: String,
        page: u32,
        roman: bool,
        children: Vec<usize>,
    }

    let mut flat: Vec<FlatEntry> = Vec::with_capacity(lines.len());
    let mut root_indices: Vec<usize> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for line in lines {
        let idx = flat.len();
        flat.push(FlatEntry {
            title: line.title.clone(),
            page: line.page,
            roman: line.is_roman,
            children: vec![],
        });

        while stack.len() >= line.level as usize && !stack.is_empty() {
            stack.pop();
        }

        if stack.is_empty() {
            root_indices.push(idx);
        } else {
            let parent_idx = *stack.last().unwrap();
            flat[parent_idx].children.push(idx);
        }
        stack.push(idx);
    }

    fn convert(idx: usize, flat: &[FlatEntry]) -> OutlineEntry {
        let fe = &flat[idx];
        OutlineEntry {
            title: fe.title.clone(),
            page: fe.page,
            roman: fe.roman,
            children: fe.children.iter().map(|&ci| convert(ci, flat)).collect(),
        }
    }

    root_indices.iter().map(|&ri| convert(ri, &flat)).collect()
}

/// Guess the Arabic and Roman page offset anchors.
///
/// **Roman anchor** — found by locating the "Preface" page before the TOC and
/// extracting its Roman numeral.  Much more reliable than TOC page headers
/// because some books number TOC pages differently from front matter.
///
/// **Arabic anchor** — first page after the TOC that looks like Chapter 1.
fn guess_offsets(pages: &[&str], toc_start: usize, toc_end: usize) -> PageMapping {
    let mut mapping = PageMapping::default();

    // ── Roman anchor from Preface page ────────────────────────────────────
    // Search pages *before* the TOC for "Preface" and extract its Roman numeral.
    // Some books show the numeral on the Preface page itself (e.g.
    // "Preface vii"); others only show it on subsequent pages (e.g. "viii"
    // on the second page of the preface).  Check the first match and a few
    // following pages.
    for (i, text) in pages.iter().enumerate().take(toc_start) {
        if text.contains("Preface") {
            mapping.roman_start_pdf = i as u32 + 1;
            let mut found = false;
            for offset in 0..4usize {
                let j = i + offset;
                if j >= pages.len() || j >= toc_start {
                    break;
                }
                for word in pages[j].split_whitespace() {
                    if let Some(n) = roman_to_int(word) {
                        if (1..30).contains(&n) {
                            mapping.roman_start_logical = n.saturating_sub(offset as u32);
                            found = true;
                            break;
                        }
                    }
                }
                if found {
                    break;
                }
            }
            if !found {
                // Fallback: assume 1:1 for Roman pages (common in many books)
                mapping.roman_start_logical = mapping.roman_start_pdf;
            }
            break;
        }
    }

    // ── Arabic start: first page after TOC that looks like Chapter 1 ───────
    for (i, page) in pages.iter().enumerate().skip(toc_end) {
        let looks_like_chapter1 = page.trim().lines().any(|l| {
            let t = l.trim().to_lowercase();
            t == "chapter 1"
                || (t == "1"
                    && page
                        .trim()
                        .lines()
                        .nth(1)
                        .is_some_and(|l2| !l2.trim().is_empty()))
                || (t.starts_with("1 ") && t.chars().nth(2).is_some_and(|c| c.is_alphabetic()))
        });
        if looks_like_chapter1 {
            mapping.arabic_start_pdf = i as u32 + 1;
            break;
        }
    }

    // ── Build the page-number map for blank-page-aware lookups ─────────────
    mapping.page_map = PageNumberMap::build(pages, mapping.arabic_start_pdf);

    mapping
}

// ═══════════════════════════════════════════════════════════════════════════════
//  7. Shared helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Total number of entries including all descendants.
pub fn count_all(entries: &[OutlineEntry]) -> usize {
    entries.iter().map(|e| 1 + count_all(&e.children)).sum()
}

/// Default output path: `<input_stem>_indexed<ext>` next to the input file.
pub fn default_output(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{}_indexed{}", stem, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roman_numeral_parsing() {
        assert_eq!(roman_to_int("vii"), Some(7));
        assert_eq!(roman_to_int("XLVII"), Some(47));
        assert_eq!(roman_to_int("mcmxcix"), Some(1999));
        assert_eq!(roman_to_int(""), None);
        assert_eq!(roman_to_int("abc"), None);
    }

    #[test]
    fn outline_spec_json_round_trip() {
        let spec = OutlineSpec {
            arabic_start_logical: 1,
            arabic_start_pdf: 21,
            roman_start_logical: 7,
            roman_start_pdf: 5,
            outline: vec![OutlineEntry {
                title: "Preface".into(),
                page: 7,
                roman: true,
                children: vec![],
            }],
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: OutlineSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back.arabic_start_pdf, 21);
        assert_eq!(back.outline[0].title, "Preface");
        assert!(back.outline[0].roman);
    }

    #[test]
    fn toc_entries_nest_by_section_numbering() {
        let text = "1  Introduction  1\n1.1  Background  2\n1.2  Related work  4\n2  Methods  9\n";
        let lines = parse_toc_entries(text).unwrap();
        let tree = build_toc_tree(&lines);
        assert_eq!(tree.len(), 2);
        assert_eq!(tree[0].children.len(), 2);
        assert_eq!(tree[0].children[0].title, "1.1 Background");
        assert!(tree[1].children.is_empty());
    }

    #[test]
    fn anchor_overrides_apply_only_when_non_default() {
        let mut mapping = PageMapping {
            arabic_start_logical: 11,
            arabic_start_pdf: 12,
            roman_start_logical: 7,
            roman_start_pdf: 5,
            page_map: PageNumberMap::empty(),
        };
        apply_anchor_overrides(&mut mapping, "1:1", "vii:3").unwrap();
        assert_eq!(mapping.arabic_start_logical, 11);
        assert_eq!(mapping.arabic_start_pdf, 12);
        assert_eq!(mapping.roman_start_logical, 7);
        assert_eq!(mapping.roman_start_pdf, 3);
    }

    #[test]
    fn page_map_handles_blank_pages() {
        let pages: Vec<String> = (1..=35)
            .map(|i| {
                if (31..=32).contains(&i) || (34..=35).contains(&i) {
                    format!("{i}\nBody text")
                } else {
                    "Front matter".into()
                }
            })
            .collect();
        let mut mapping = PageMapping {
            arabic_start_logical: 1,
            arabic_start_pdf: 31,
            roman_start_logical: 1,
            roman_start_pdf: 1,
            page_map: PageNumberMap::empty(),
        };
        mapping.rebuild_page_map(&pages);
        assert_eq!(mapping.page_map.get_pdf_page(31), Some(31));
        // Page 33 is blank in the PDF text; it still resolves linearly.
        assert_eq!(mapping.page_map.get_pdf_page(33), Some(33));
    }

    fn with_temp_dir(name: &str, f: impl FnOnce(&Path)) {
        let dir =
            std::env::temp_dir().join(format!("pdf-indexer-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        f(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_outline_json_next_to_pdf() {
        with_temp_dir("find-json", |dir| {
            let pdf = dir.join("SICP.pdf");
            let json = dir.join("SICP-outline.json");
            std::fs::write(&json, "{}").unwrap();
            assert_eq!(find_outline_json(&pdf), Some(json));
        });
    }

    #[test]
    fn finds_outline_json_case_insensitively() {
        with_temp_dir("find-json-ci", |dir| {
            let pdf = dir.join("SICP.pdf");
            let json = dir.join("sicp-outline.json");
            std::fs::write(&json, "{}").unwrap();
            assert_eq!(find_outline_json(&pdf), Some(json));
        });
    }

    #[test]
    fn outline_json_priority_prefers_gui_name() {
        with_temp_dir("find-json-prio", |dir| {
            let pdf = dir.join("SICP.pdf");
            let gui_name = dir.join("SICP-outline.json");
            let plain_name = dir.join("SICP.json");
            std::fs::write(&gui_name, "{}").unwrap();
            std::fs::write(&plain_name, "{}").unwrap();
            assert_eq!(find_outline_json(&pdf), Some(gui_name));
        });
    }

    #[test]
    fn finds_nothing_when_no_json_present() {
        with_temp_dir("find-json-none", |dir| {
            let pdf = dir.join("book.pdf");
            assert_eq!(find_outline_json(&pdf), None);
        });
    }
}
