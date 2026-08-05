// pdf-toc-adder — graphical TOC editor.
//
// An egui/eframe front end over the library pipeline:
//
//   1. Open a PDF (or load an existing JSON outline)
//   2. Auto-extract the TOC and show it as a checkbox tree
//   3. Edit titles / pages / anchors, tick the chapters to include
//   4. Add new entries with “+” (globally, or per chapter) and save/rewrite
//      the JSON spec, or run the indexing directly
//
// Extraction and indexing run on background threads so the UI stays
// responsive while pdftotext or lopdf is working.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use pdf_toc_adder::{
    count_all, default_output, extract_toc, find_outline_json, index_pdf, load_json, OutlineEntry,
    OutlineSpec, PageMapping, TocExtraction,
};

/// Start the GUI event loop. Returns once the window is closed.
pub fn run(pdf: Option<PathBuf>, output: Option<PathBuf>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 720.0])
            .with_min_inner_size([700.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "pdf-toc-adder — TOC editor",
        options,
        Box::new(move |_cc| Ok(Box::new(IndexerApp::new(pdf, output)))),
    )
    .map_err(|e| anyhow::anyhow!("failed to start GUI: {e}"))
}

struct IndexerApp {
    pdf_path: Option<PathBuf>,
    output_path: String,
    tree: Vec<GuiNode>,
    mapping: PageMapping,
    /// The JSON file the current outline came from (if any), so “Save JSON”
    /// can rewrite it in place.
    json_path: Option<PathBuf>,
    /// Raw per-page text from `pdftotext`, kept so the blank-page-aware page
    /// map can be rebuilt whenever the user edits the page-number anchors.
    page_texts: Vec<String>,
    status: String,
    status_is_error: bool,
    busy: bool,
    /// Where the “Add entry” dialog should insert the new node.
    pending_add: Option<AddTarget>,
    add_title: String,
    add_page: u32,
    add_roman: bool,
    job_tx: Sender<JobResult>,
    job_rx: Receiver<JobResult>,
}

/// Target for a new entry: a top-level line, or a child of the node at a path
/// (sequence of child indices) into the tree.
#[derive(Clone, Debug)]
enum AddTarget {
    Root,
    Node(Vec<usize>),
}

enum JobResult {
    Extracted(Result<TocExtraction, String>),
    Indexed(Result<(), String>),
}

/// Editable mirror of an [`OutlineEntry`] plus a "include in TOC" checkbox.
struct GuiNode {
    included: bool,
    title: String,
    page: u32,
    roman: bool,
    children: Vec<GuiNode>,
}

impl GuiNode {
    fn new(title: impl Into<String>, page: u32, roman: bool) -> Self {
        Self {
            included: true,
            title: title.into(),
            page,
            roman,
            children: Vec::new(),
        }
    }

    fn from_entries(entries: &[OutlineEntry]) -> Vec<GuiNode> {
        entries
            .iter()
            .map(|e| GuiNode {
                included: true,
                title: e.title.clone(),
                page: e.page,
                roman: e.roman,
                children: GuiNode::from_entries(&e.children),
            })
            .collect()
    }
}

/// Convert the tree back to outline entries, keeping only checked nodes.
fn gui_to_entries(nodes: &[GuiNode]) -> Vec<OutlineEntry> {
    nodes
        .iter()
        .filter(|n| n.included)
        .map(|n| OutlineEntry {
            title: n.title.clone(),
            page: n.page,
            roman: n.roman,
            children: gui_to_entries(&n.children),
        })
        .collect()
}

fn set_included(nodes: &mut [GuiNode], included: bool) {
    for node in nodes {
        node.included = included;
        set_included(&mut node.children, included);
    }
}

/// `(selected_count, total_count)` across the whole tree.
fn selection_counts(nodes: &[GuiNode]) -> (usize, usize) {
    nodes.iter().fold((0, 0), |(selected, total), node| {
        let (child_sel, child_total) = selection_counts(&node.children);
        (
            selected + child_sel + usize::from(node.included),
            total + child_total + 1,
        )
    })
}

/// Mutable reference to the node at `path` (a sequence of child indices).
fn node_at_mut<'a>(nodes: &'a mut [GuiNode], path: &[usize]) -> Option<&'a mut GuiNode> {
    let mut current = nodes;
    for (i, &idx) in path.iter().enumerate() {
        let node = current.get_mut(idx)?;
        if i + 1 == path.len() {
            return Some(node);
        }
        current = &mut node.children;
    }
    None
}

impl IndexerApp {
    fn new(pdf: Option<PathBuf>, output: Option<PathBuf>) -> Self {
        let output_path = output
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| {
                pdf.as_deref()
                    .map(|p| default_output(p).to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "indexed.pdf".to_string());
        let (job_tx, job_rx) = channel();
        let mut app = Self {
            pdf_path: pdf,
            output_path,
            tree: Vec::new(),
            mapping: PageMapping::default(),
            json_path: None,
            page_texts: Vec::new(),
            status: "Open a PDF and run “Auto-extract TOC”, or load an existing JSON outline."
                .to_string(),
            status_is_error: false,
            busy: false,
            pending_add: None,
            add_title: String::new(),
            add_page: 1,
            add_roman: false,
            job_tx,
            job_rx,
        };
        // When the GUI is started with a PDF argument, look for an outline
        // JSON next to it right away.
        if app.pdf_path.is_some() {
            app.auto_load_json_next_to_pdf();
        }
        app
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if ui.button("Open PDF…").clicked() {
                self.open_pdf_dialog();
            }
            if ui.button("Load JSON…").clicked() {
                self.load_json_dialog();
            }
            if ui
                .add_enabled(!self.busy, egui::Button::new("+ Add entry"))
                .on_hover_text("Add a new top-level entry to the outline")
                .clicked()
            {
                self.begin_add(AddTarget::Root);
            }
            ui.separator();
            let can_extract = self.pdf_path.is_some() && !self.busy;
            if ui
                .add_enabled(can_extract, egui::Button::new("Auto-extract TOC"))
                .clicked()
            {
                self.start_extract();
            }
            let has_tree = !self.tree.is_empty();
            if ui
                .add_enabled(has_tree, egui::Button::new("Select all"))
                .clicked()
            {
                set_included(&mut self.tree, true);
            }
            if ui
                .add_enabled(has_tree, egui::Button::new("Select none"))
                .clicked()
            {
                set_included(&mut self.tree, false);
            }
            ui.separator();
            let can_export = has_tree && !self.busy;
            if ui
                .add_enabled(can_export, egui::Button::new("Save JSON"))
                .on_hover_text("Rewrite the loaded JSON (or ask for a location)")
                .clicked()
            {
                self.save_json();
            }
            if ui
                .add_enabled(can_export, egui::Button::new("Export JSON…"))
                .clicked()
            {
                self.export_json();
            }
            let can_index = self.pdf_path.is_some() && has_tree && !self.busy;
            if ui
                .add_enabled(can_index, egui::Button::new("Index PDF…"))
                .clicked()
            {
                self.start_index();
            }
        });
        ui.add_space(2.0);
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            if self.busy {
                ui.spinner();
            }
            let color = if self.status_is_error {
                ui.visuals().error_fg_color
            } else {
                ui.visuals().text_color()
            };
            ui.colored_label(color, &self.status);
        });
        ui.add_space(2.0);
    }

    fn content(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Input:");
            match &self.pdf_path {
                Some(path) => {
                    ui.monospace(path.display().to_string());
                }
                None => {
                    ui.weak("no PDF selected");
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Output:");
            ui.add(egui::TextEdit::singleline(&mut self.output_path).desired_width(480.0));
        });
        ui.add_space(2.0);

        ui.horizontal(|ui| {
            ui.label("Arabic start:");
            let arabic_logical = ui.add(
                egui::DragValue::new(&mut self.mapping.arabic_start_logical)
                    .range(1..=100_000)
                    .prefix("logical "),
            );
            let arabic_pdf = ui.add(
                egui::DragValue::new(&mut self.mapping.arabic_start_pdf)
                    .range(1..=100_000)
                    .prefix("pdf "),
            );
            if arabic_logical.changed() || arabic_pdf.changed() {
                self.mapping.rebuild_page_map(&self.page_texts);
            }
            ui.separator();
            ui.label("Roman start:");
            ui.add(
                egui::DragValue::new(&mut self.mapping.roman_start_logical)
                    .range(1..=100_000)
                    .prefix("logical "),
            );
            ui.add(
                egui::DragValue::new(&mut self.mapping.roman_start_pdf)
                    .range(1..=100_000)
                    .prefix("pdf "),
            );
        });
        ui.add_space(2.0);

        if self.tree.is_empty() {
            ui.add_space(8.0);
            ui.weak(
                "No outline loaded yet — open a PDF and click “Auto-extract TOC”, \
                 or load an existing JSON outline.",
            );
            ui.add_space(8.0);
            if ui.button("＋ Add first entry").clicked() {
                self.begin_add(AddTarget::Root);
            }
        } else {
            let (selected, total) = selection_counts(&self.tree);
            ui.label(format!(
                "{selected} of {total} entries selected — only checked entries are \
                 indexed and exported"
            ));
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut path = Vec::new();
                    for (i, node) in self.tree.iter_mut().enumerate() {
                        path.push(i);
                        show_node(ui, node, &mut path, &mut self.pending_add);
                        path.pop();
                    }
                });
        }
    }

    fn open_pdf_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PDF", &["pdf"])
            .pick_file()
        else {
            return;
        };
        self.pdf_path = Some(path.clone());
        self.output_path = default_output(&path).to_string_lossy().into_owned();
        self.tree.clear();
        self.json_path = None;
        self.page_texts.clear();
        self.mapping = PageMapping::default();
        self.auto_load_json_next_to_pdf();
    }

    /// Look for an outline JSON next to the selected PDF and load it if found.
    fn auto_load_json_next_to_pdf(&mut self) {
        let Some(pdf) = &self.pdf_path else {
            return;
        };
        match find_outline_json(pdf) {
            Some(json_path) => match load_json(&json_path) {
                Ok((entries, mapping)) => {
                    self.tree = GuiNode::from_entries(&entries);
                    self.mapping = mapping;
                    self.json_path = Some(json_path.clone());
                    self.page_texts.clear();
                    let (_, total) = selection_counts(&self.tree);
                    self.status_ok(format!(
                        "Loaded {total} entries from {} (found next to the PDF)",
                        json_path.display()
                    ));
                }
                Err(e) => self.status_err(format!(
                    "Found JSON next to the PDF but could not load it: {e:#}"
                )),
            },
            None => self.status_ok(
                "PDF selected — no outline JSON found next to it; click \
                 “Auto-extract TOC” or “Load JSON…”.",
            ),
        }
    }

    fn load_json_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .pick_file()
        else {
            return;
        };
        match load_json(&path) {
            Ok((entries, mapping)) => {
                self.tree = GuiNode::from_entries(&entries);
                self.mapping = mapping;
                self.json_path = Some(path.clone());
                self.page_texts.clear();
                let (_, total) = selection_counts(&self.tree);
                self.status_ok(format!("Loaded {total} entries from {}", path.display()));
            }
            Err(e) => self.status_err(format!("Cannot load JSON: {e:#}")),
        }
    }

    fn start_extract(&mut self) {
        let Some(pdf) = self.pdf_path.clone() else {
            return;
        };
        self.busy = true;
        self.status_ok("Extracting TOC…");
        let tx = self.job_tx.clone();
        std::thread::spawn(move || {
            let result = extract_toc(&pdf).map_err(|e| format!("{e:#}"));
            let _ = tx.send(JobResult::Extracted(result));
        });
    }

    fn start_index(&mut self) {
        let Some(pdf) = self.pdf_path.clone() else {
            return;
        };
        let output = self.output_path.trim().to_string();
        if output.is_empty() {
            self.status_err("Output path is empty.");
            return;
        }
        let entries = gui_to_entries(&self.tree);
        if entries.is_empty() {
            self.status_err("No entries selected — tick at least one chapter.");
            return;
        }
        self.mapping.rebuild_page_map(&self.page_texts);
        let mapping = self.mapping.clone();
        let total = count_all(&entries);
        self.busy = true;
        self.status_ok(format!("Indexing {total} entries → {output}…"));
        let tx = self.job_tx.clone();
        std::thread::spawn(move || {
            let result = index_pdf(&pdf, &entries, &mapping, Path::new(&output))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(JobResult::Indexed(result));
        });
    }

    /// Serialize the currently selected entries into the JSON spec string.
    fn build_json(&mut self) -> Option<String> {
        let entries = gui_to_entries(&self.tree);
        if entries.is_empty() {
            return None;
        }
        let spec = OutlineSpec {
            arabic_start_logical: self.mapping.arabic_start_logical,
            arabic_start_pdf: self.mapping.arabic_start_pdf,
            roman_start_logical: self.mapping.roman_start_logical,
            roman_start_pdf: self.mapping.roman_start_pdf,
            outline: entries,
        };
        match serde_json::to_string_pretty(&spec) {
            Ok(json) => Some(json),
            Err(e) => {
                self.status_err(format!("Failed to serialize JSON: {e}"));
                None
            }
        }
    }

    fn default_json_name(&self) -> String {
        self.pdf_path
            .as_deref()
            .map(|p| {
                let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                format!("{stem}-outline.json")
            })
            .unwrap_or_else(|| "outline.json".to_string())
    }

    /// Write the JSON and remember the path for later in-place rewrites.
    fn write_json_file(&mut self, path: &Path, json: String) {
        match std::fs::write(path, json) {
            Ok(()) => {
                self.json_path = Some(path.to_path_buf());
                self.status_ok(format!("Wrote {}", path.display()));
            }
            Err(e) => self.status_err(format!("Cannot write {}: {e}", path.display())),
        }
    }

    /// Rewrite the loaded JSON file, or ask for a location if there is none.
    fn save_json(&mut self) {
        let Some(json) = self.build_json() else {
            self.status_err("No entries selected — tick at least one chapter.");
            return;
        };
        if let Some(path) = self.json_path.clone() {
            self.write_json_file(&path, json);
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(self.default_json_name())
            .save_file()
        else {
            return;
        };
        self.write_json_file(&path, json);
    }

    /// Save-as: always ask for the target location.
    fn export_json(&mut self) {
        let Some(json) = self.build_json() else {
            self.status_err("No entries selected — tick at least one chapter.");
            return;
        };
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(self.default_json_name())
            .save_file()
        else {
            return;
        };
        self.write_json_file(&path, json);
    }

    /// Open the “Add entry” dialog for the given target.
    fn begin_add(&mut self, target: AddTarget) {
        self.add_title.clear();
        self.add_page = 1;
        self.add_roman = false;
        self.pending_add = Some(target);
    }

    /// Insert the new entry at the requested target.
    fn add_entry(&mut self, target: AddTarget, title: String, page: u32, roman: bool) {
        let title = if title.trim().is_empty() {
            "New entry".to_string()
        } else {
            title.trim().to_string()
        };
        match target {
            AddTarget::Root => self.tree.push(GuiNode::new(title, page, roman)),
            AddTarget::Node(path) => match node_at_mut(&mut self.tree, &path) {
                Some(node) => node.children.push(GuiNode::new(title, page, roman)),
                None => self.tree.push(GuiNode::new(title, page, roman)),
            },
        }
        self.add_title.clear();
        self.add_page = 1;
        self.status_ok(
            "Entry added — edit its title/page in the tree, then click “Save JSON” \
             to rewrite the outline file.",
        );
    }

    /// The “Add entry” dialog: title + page, inserted on confirmation.
    fn show_add_entry_window(&mut self, ctx: &egui::Context) {
        if self.pending_add.is_none() {
            return;
        }
        let is_root = matches!(self.pending_add, Some(AddTarget::Root));
        egui::Window::new(if is_root {
            "Add top-level entry"
        } else {
            "Add sub-section"
        })
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Title:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.add_title)
                        .desired_width(280.0)
                        .hint_text("e.g. 1.3 Results"),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Page:");
                ui.add(egui::DragValue::new(&mut self.add_page).range(1..=100_000));
                ui.checkbox(&mut self.add_roman, "Roman page");
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Add").clicked() {
                    let target = self.pending_add.take().expect("checked above");
                    let title = std::mem::take(&mut self.add_title);
                    let page = self.add_page;
                    let roman = self.add_roman;
                    self.add_entry(target, title, page, roman);
                    self.add_roman = false;
                }
                if ui.button("Cancel").clicked() {
                    self.pending_add = None;
                }
            });
        });
    }

    fn poll_jobs(&mut self) {
        while let Ok(result) = self.job_rx.try_recv() {
            match result {
                JobResult::Extracted(Ok(extraction)) => {
                    self.tree = GuiNode::from_entries(&extraction.entries);
                    self.mapping = extraction.mapping;
                    self.page_texts = extraction.page_texts;
                    let (_, total) = selection_counts(&self.tree);
                    self.status_ok(format!(
                        "Extracted {total} entries — tick the chapters to include."
                    ));
                }
                JobResult::Extracted(Err(e)) => {
                    self.status_err(format!("Auto-extraction failed: {e}"));
                }
                JobResult::Indexed(Ok(())) => {
                    self.status_ok(format!("Done — wrote {}", self.output_path));
                }
                JobResult::Indexed(Err(e)) => {
                    self.status_err(format!("Indexing failed: {e}"));
                }
            }
            self.busy = false;
        }
    }

    fn status_ok(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_is_error = false;
    }

    fn status_err(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_is_error = true;
    }
}

impl eframe::App for IndexerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_jobs();
        if self.busy {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show_add_entry_window(ui.ctx());
        egui::Panel::top("tools").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        egui::CentralPanel::default().show(ui, |ui| self.content(ui));
    }
}

/// Render one outline node: checkbox + page number + collapsible editor plus
/// a “+” button that queues an "add sub-section" dialog for this node.
fn show_node(
    ui: &mut egui::Ui,
    node: &mut GuiNode,
    path: &mut Vec<usize>,
    add_request: &mut Option<AddTarget>,
) {
    ui.push_id(path.clone(), |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut node.included, "");
            ui.add(
                egui::DragValue::new(&mut node.page)
                    .range(1..=100_000)
                    .speed(1),
            );
            if node.roman {
                ui.label("roman");
            }
            if ui
                .small_button("+")
                .on_hover_text("Add sub-section under this entry")
                .clicked()
            {
                *add_request = Some(AddTarget::Node(path.clone()));
            }
            egui::CollapsingHeader::new(&node.title)
                .default_open(true)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Title:");
                        ui.add(
                            egui::TextEdit::singleline(&mut node.title)
                                .desired_width(f32::INFINITY),
                        );
                        ui.checkbox(&mut node.roman, "Roman page");
                    });
                    ui.indent("children", |ui| {
                        for (ci, child) in node.children.iter_mut().enumerate() {
                            path.push(ci);
                            show_node(ui, child, path, add_request);
                            path.pop();
                        }
                    });
                });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, page: u32) -> OutlineEntry {
        OutlineEntry {
            title: title.to_string(),
            page,
            roman: false,
            children: vec![],
        }
    }

    #[test]
    fn inclusion_filter_applies_recursively() {
        let entries = vec![
            OutlineEntry {
                title: "Preface".into(),
                page: 7,
                roman: true,
                children: vec![],
            },
            OutlineEntry {
                title: "1 Introduction".into(),
                page: 1,
                roman: false,
                children: vec![entry("1.1 Background", 2), entry("1.2 Related work", 4)],
            },
        ];
        let mut tree = GuiNode::from_entries(&entries);
        tree[0].included = false;
        tree[1].children[1].included = false;

        let filtered = gui_to_entries(&tree);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].children.len(), 1);
        assert_eq!(filtered[0].children[0].title, "1.1 Background");
        assert_eq!(selection_counts(&tree), (2, 4));
    }

    #[test]
    fn select_all_none_toggles_everything() {
        let mut tree = GuiNode::from_entries(&[
            entry("a", 1),
            OutlineEntry {
                title: "b".into(),
                page: 2,
                roman: false,
                children: vec![entry("b.1", 3)],
            },
        ]);
        set_included(&mut tree, false);
        assert!(gui_to_entries(&tree).is_empty());
        set_included(&mut tree, true);
        assert_eq!(count_all(&gui_to_entries(&tree)), 3);
    }

    #[test]
    fn auto_loads_json_found_next_to_pdf() {
        let dir = std::env::temp_dir().join(format!("pdf-toc-adder-gui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pdf = dir.join("SICP.pdf");
        let json = dir.join("SICP-outline.json");
        std::fs::write(&json, r#"{"outline": [{"title": "Chapter 1", "page": 1}]}"#).unwrap();

        let app = IndexerApp::new(Some(pdf), None);

        assert_eq!(app.tree.len(), 1);
        assert!(app.status.contains("found next to the PDF"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_entry_creates_root_and_children() {
        let mut app = IndexerApp::new(None, None);

        app.add_entry(AddTarget::Root, "Chapter 1".to_string(), 1, false);
        assert_eq!(app.tree.len(), 1);
        assert_eq!(app.tree[0].title, "Chapter 1");

        app.add_entry(
            AddTarget::Node(vec![0]),
            "1.1 Background".to_string(),
            2,
            false,
        );
        assert_eq!(app.tree[0].children.len(), 1);
        assert_eq!(app.tree[0].children[0].title, "1.1 Background");
        assert_eq!(app.tree[0].children[0].page, 2);

        // An empty title falls back to a placeholder.
        app.add_entry(AddTarget::Node(vec![0]), "   ".to_string(), 3, true);
        assert_eq!(app.tree[0].children[1].title, "New entry");
        assert!(app.tree[0].children[1].roman);

        // A stale path degrades to a root entry instead of panicking.
        app.add_entry(AddTarget::Node(vec![99, 0]), "Orphan".to_string(), 4, false);
        assert_eq!(app.tree[1].title, "Orphan");
    }

    #[test]
    fn node_at_mut_resolves_nested_paths() {
        let mut tree = GuiNode::from_entries(&[OutlineEntry {
            title: "1".into(),
            page: 1,
            roman: false,
            children: vec![OutlineEntry {
                title: "1.1".into(),
                page: 2,
                roman: false,
                children: vec![entry("1.1.1", 3)],
            }],
        }]);
        let leaf = node_at_mut(&mut tree, &[0, 0, 0]).expect("path should resolve");
        assert_eq!(leaf.title, "1.1.1");
        assert!(node_at_mut(&mut tree, &[5]).is_none());
    }
}
