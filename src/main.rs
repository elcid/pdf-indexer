// pdf-indexer — CLI entry point.
//
// The pipeline itself lives in the library crate (`lib.rs`); this binary only
// parses CLI arguments and optionally starts the GUI (`gui.rs`).

mod gui;

use anyhow::{bail, Context, Result};
use clap::Parser;
use pdf_indexer::{
    apply_anchor_overrides, count_all, default_output, extract_toc, find_outline_json, index_pdf,
    load_json,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "pdf-indexer",
    about = "Inject a navigable outline into a PDF",
    after_help = "Examples:\n  \
                  pdf-indexer book.pdf --json outline.json -o indexed.pdf\n  \
                  pdf-indexer book.pdf --toc -o indexed.pdf\n  \
                  pdf-indexer --gui\n  \
                  pdf-indexer book.pdf --json outline.json --arabic-start 1:21 --roman-start vii:5"
)]
struct Cli {
    /// Input PDF file (optional — the GUI can pick one via dialog)
    pdf: Option<PathBuf>,

    /// JSON outline specification file
    #[arg(long, group = "source")]
    json: Option<PathBuf>,

    /// Auto-extract TOC from PDF's own Contents page (requires pdftotext)
    #[arg(long, group = "source")]
    toc: bool,

    /// Open the graphical TOC editor instead of running the CLI pipeline
    #[arg(long)]
    gui: bool,

    /// Output PDF path (default: <input>_indexed.pdf)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Arabic page mapping 'logical:pdf' (e.g. "1:21")
    #[arg(long, default_value = "1:1")]
    arabic_start: String,

    /// Roman page mapping 'logical:pdf' (e.g. "vii:5")
    #[arg(long, default_value = "1:1")]
    roman_start: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.gui {
        return gui::run(cli.pdf, cli.output);
    }

    let pdf = cli
        .pdf
        .context("input PDF file is required (or use --gui)")?;

    let (entries, mut mapping) = if let Some(ref json_path) = cli.json {
        load_json(json_path)?
    } else if cli.toc {
        if let Some(json_path) = find_outline_json(&pdf) {
            eprintln!(
                "Found outline JSON next to the PDF, using it instead of auto-extraction: {}",
                json_path.display()
            );
            load_json(&json_path)?
        } else {
            let extraction = extract_toc(&pdf)?;
            (extraction.entries, extraction.mapping)
        }
    } else {
        bail!("either --json or --toc is required (or use --gui)");
    };

    apply_anchor_overrides(&mut mapping, &cli.arabic_start, &cli.roman_start)?;

    if entries.is_empty() {
        eprintln!("warning: outline is empty; output PDF will have no bookmarks.");
    }

    let output = cli.output.unwrap_or_else(|| default_output(&pdf));
    eprintln!(
        "Writing {} outline entries → {}",
        count_all(&entries),
        output.display()
    );
    index_pdf(&pdf, &entries, &mapping, &output)?;
    eprintln!("Done.");

    Ok(())
}
