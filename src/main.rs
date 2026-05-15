use clap::Parser;
use lopdf::{Dictionary, Document, Object, ObjectId, StringFormat};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pdf-indexer", about = "Inject a navigable outline into a PDF")]
struct Cli {
    pdf: PathBuf,
    #[arg(long)] json: PathBuf,
    #[arg(short = 'o', long)] output: Option<PathBuf>,
    #[arg(long, default_value = "1:1")] arabic_start: String,
    #[arg(long, default_value = "1:1")] roman_start: String,
}

#[derive(Deserialize)]
struct OutlineSpec {
    #[serde(default = "default_one")] arabic_start_logical: u32,
    #[serde(default = "default_one")] arabic_start_pdf: u32,
    #[serde(default = "default_one")] roman_start_logical: u32,
    #[serde(default = "default_one")] roman_start_pdf: u32,
    outline: Vec<OutlineEntry>,
}
fn default_one() -> u32 { 1 }

#[derive(Deserialize)]
struct OutlineEntry {
    title: String, page: u32,
    #[serde(default)] roman: bool,
    #[serde(default)] children: Vec<OutlineEntry>,
}

struct PageMapping { arabic_start_logical: u32, arabic_start_pdf: u32, roman_start_logical: u32, roman_start_pdf: u32 }

fn compute_pdf_page(logical: u32, is_roman: bool, m: &PageMapping) -> u32 {
    if is_roman { logical - m.roman_start_logical + m.roman_start_pdf }
    else { logical - m.arabic_start_logical + m.arabic_start_pdf }
}

fn parse_offset(s: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 { return Err(format!("Invalid offset: {}", s)); }
    let logical = roman_to_int(parts[0]).unwrap_or_else(|| parts[0].parse().unwrap());
    let pdf = parts[1].parse().map_err(|_| format!("Bad PDF page: {}", parts[1]))?;
    Ok((logical, pdf))
}

fn roman_to_int(s: &str) -> Option<u32> {
    let sl = s.to_lowercase();
    if !sl.chars().all(|c| matches!(c,'i'|'v'|'x'|'l'|'c'|'d'|'m')) { return None; }
    let vals = [('i',1),('v',5),('x',10),('l',50),('c',100),('d',500),('m',1000)].iter().cloned().collect::<BTreeMap<_,_>>();
    let (mut t, mut p) = (0u32, 0u32);
    for ch in sl.chars().rev() { let c = vals[&ch]; t += if c>=p {c} else {-c}; p = c; }
    Some(t)
}

fn pdf_string(s: &str) -> Vec<u8> {
    if s.chars().all(|c| (c as u32) <= 0xFF) { s.as_bytes().to_vec() }
    else { let mut v = vec![0xFE, 0xFF]; for cu in s.encode_utf16() { v.extend(cu.to_be_bytes()); } v }
}

fn collect_page_ids(doc: &Document) -> Vec<ObjectId> {
    doc.objects.iter().filter_map(|(id, obj)| {
        obj.as_dict().ok().and_then(|d| d.get(b"Type").ok().and_then(|t| t.as_name().ok()))
            .and_then(|n| if n == b"Page" { Some(*id) } else { None })
    }).collect()
}

fn inject_outline(doc: &mut Document, entries: &[OutlineEntry], mapping: &PageMapping) -> Result<ObjectId, String> {
    let page_ids = collect_page_ids(doc);
    let mut max_id = doc.max_id;
    let (first, last, total, _) = build_items(entries, mapping, &page_ids, doc, &mut max_id, None, 0)?;
    max_id += 1;
    let root_id = ObjectId(max_id, 0);
    let mut root = Dictionary::new();
    root.set(b"Type", Object::Name(b"Outlines".to_vec()));
    root.set(b"First", first);
    root.set(b"Last", last);
    eprintln!("DEBUG root total={}", total);
    root.set(b"Count", Object::Integer(total));
    doc.objects.insert(root_id, Object::Dictionary(root));

    let catalog_id = doc.trailer.get(b"Root").and_then(|r| r.as_reference().ok()).ok_or("no catalog")?;
    let cat_obj = doc.get_object_mut(catalog_id).map_err(|_| "cannot get catalog")?;
    if let Object::Dictionary(ref mut cat_dict) = cat_obj {
        cat_dict.set(b"Outlines", Object::Reference(root_id));
    }
    doc.max_id = max_id;
    Ok(root_id)
}

fn build_items(entries: &[OutlineEntry], mapping: &PageMapping, page_ids: &[ObjectId],
    doc: &mut Document, max_id: &mut u32, parent: Option<ObjectId>, _depth: usize)
    -> Result<(ObjectId, ObjectId, i64, ObjectId), String>
{
    if entries.is_empty() { return Ok((ObjectId(0, 0), ObjectId(0, 0), 0, ObjectId(0, 0))); }
    let mut first = ObjectId(0, 0);
    let mut last = ObjectId(0, 0);
    let mut prev = ObjectId(0, 0);
    let mut total_desc = 0i64;
    for (i, entry) in entries.iter().enumerate() {
        *max_id += 1;
        let this = ObjectId(*max_id, 0);
        let pdf_pg = compute_pdf_page(entry.page, entry.roman, mapping);
        let pg_ref = page_ids.get(pdf_pg as usize - 1).copied().unwrap_or(page_ids[0]);

        let (cf, cl, cc, _) = if entry.children.is_empty() {
            (ObjectId(0, 0), ObjectId(0, 0), 0i64, ObjectId(0, 0))
        } else {
            build_items(&entry.children, mapping, page_ids, doc, max_id, Some(this), _depth+1)?
        };

        let next = if i < entries.len() - 1 { ObjectId(*max_id + 1, 0) } else { ObjectId(0, 0) };

        let title_bytes = pdf_string(&entry.title);
        let mut dict = Dictionary::new();
        dict.set(b"Title", Object::String(title_bytes, StringFormat::Literal));
        dict.set(b"Dest", Object::Array(vec![
            Object::Reference(pg_ref), Object::Name(b"XYZ".to_vec()),
            Object::Null, Object::Null, Object::Integer(0),
        ]));
        if let Some(p) = parent { dict.set(b"Parent", Object::Reference(p)); }
        if prev != ObjectId(0, 0) { dict.set(b"Prev", Object::Reference(prev)); }
        if next != ObjectId(0, 0) { dict.set(b"Next", Object::Reference(next)); }
        if cc != 0 { dict.set(b"First", Object::Reference(cf)); dict.set(b"Last", Object::Reference(cl)); dict.set(b"Count", Object::Integer(-cc)); }

        doc.objects.insert(this, Object::Dictionary(dict));
        if first == ObjectId(0, 0) { first = this; }
        prev = this; last = this;
        total_desc += 1 + cc;
    }
    let after = ObjectId(*max_id + 1, 0, 0);
    Ok((first, last, total_desc, after))
}

fn count_all(e: &OutlineEntry) -> i64 { 1 + e.children.iter().map(count_all).sum::<i64>() }

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let spec: OutlineSpec = serde_json::from_str(&std::fs::read_to_string(&cli.json)?)?;
    let (al, ap) = parse_offset(&cli.arabic_start)?;
    let (rl, rp) = parse_offset(&cli.roman_start)?;
    let mapping = PageMapping {
        arabic_start_logical: if cli.arabic_start != "1:1" { al } else { spec.arabic_start_logical },
        arabic_start_pdf: if cli.arabic_start != "1:1" { ap } else { spec.arabic_start_pdf },
        roman_start_logical: if cli.roman_start != "1:1" { rl } else { spec.roman_start_logical },
        roman_start_pdf: if cli.roman_start != "1:1" { rp } else { spec.roman_start_pdf },
    };
    let total = spec.outline.iter().map(count_all).sum::<i64>();
    eprintln!("{} outline entries", total);
    let mut doc = Document::load(&cli.pdf)?;
    inject_outline(&mut doc, &spec.outline, &mapping)?;
    let output = cli.output.unwrap_or_else(|| {
        let stem = cli.pdf.file_stem().unwrap().to_string_lossy();
        cli.pdf.with_file_name(format!("{}_indexed.pdf", stem))
    });
    println!("Writing -> {}", output.display());
    doc.save(&output)?;
    println!("Done.");
    Ok(())
}
