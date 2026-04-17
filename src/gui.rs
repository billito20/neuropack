use eframe::egui;
use rfd::FileDialog;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::decompression::{ListEntry, PackageReader};
use crate::progress::ProgressToken;

// ── Entry point ────────────────────────────────────────────────────────────

pub fn launch() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_min_inner_size([700.0, 480.0])
            .with_title("NeuroPack"),
        ..Default::default()
    };
    eframe::run_native("NeuroPack", options, Box::new(|_cc| Ok(Box::new(App::default()))))
}

// ── Shared per-operation state ─────────────────────────────────────────────

struct OpState {
    running: bool,
    result:  Option<Result<String, String>>,
    /// Token for progress/cancel on operations that support it.
    token:   Option<Arc<ProgressToken>>,
}

impl Default for OpState {
    fn default() -> Self {
        Self { running: false, result: None, token: None }
    }
}

type SharedOp = Arc<Mutex<OpState>>;
fn new_op() -> SharedOp { Arc::new(Mutex::new(OpState::default())) }

/// Type alias for the async list result channel.
type ListSlot = Arc<Mutex<Option<anyhow::Result<(Vec<ListEntry>, u64, u64)>>>>;

// ── Application state ──────────────────────────────────────────────────────

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab { Compress, Extract, List, Verify, Analyze }

struct App {
    tab: Tab,

    // Compress
    compress_src: String,
    compress_out: String,
    compress_op:  SharedOp,

    // Extract
    extract_pkg: String,
    extract_out: String,
    extract_op:  SharedOp,

    // List
    list_pkg:          String,
    list_entries:      Vec<ListEntry>,
    list_total_unc:    u64,
    list_total_cmp:    u64,
    list_sort_by_size: bool,
    list_search:       String,
    list_selected:     Option<usize>,
    list_extract_out:  String,
    list_op:           SharedOp,
    list_slot:         Option<ListSlot>,
    list_extract_op:   SharedOp,

    // Verify
    verify_pkg: String,
    verify_op:  SharedOp,

    // Analyze
    analyze_src: String,
    analyze_out: String,
    analyze_op:  SharedOp,
}

impl Default for App {
    fn default() -> Self {
        Self {
            tab: Tab::Compress,
            compress_src: String::new(), compress_out: String::new(), compress_op: new_op(),
            extract_pkg:  String::new(), extract_out:  String::new(), extract_op:  new_op(),
            list_pkg: String::new(), list_entries: Vec::new(),
            list_total_unc: 0, list_total_cmp: 0,
            list_sort_by_size: false, list_search: String::new(),
            list_selected: None, list_extract_out: String::new(),
            list_op: new_op(), list_slot: None, list_extract_op: new_op(),
            verify_pkg: String::new(), verify_op: new_op(),
            analyze_src: String::new(), analyze_out: String::new(), analyze_op: new_op(),
        }
    }
}

// ── eframe::App ────────────────────────────────────────────────────────────

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_list_slot();

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("NeuroPack");
                ui.separator();
                for (label, t) in [
                    ("⬇ Compress", Tab::Compress),
                    ("⬆ Extract",  Tab::Extract),
                    ("☰ List",     Tab::List),
                    ("✔ Verify",   Tab::Verify),
                    ("⚙ Analyze", Tab::Analyze),
                ] {
                    ui.selectable_value(&mut self.tab, t, label);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Compress => self.ui_compress(ui, ctx),
            Tab::Extract  => self.ui_extract(ui, ctx),
            Tab::List     => self.ui_list(ui, ctx),
            Tab::Verify   => self.ui_verify(ui, ctx),
            Tab::Analyze  => self.ui_analyze(ui, ctx),
        });
    }
}

// ── Shared helpers ─────────────────────────────────────────────────────────

fn path_row(ui: &mut egui::Ui, label: &str, value: &mut String, pick_file: bool) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        if ui.add(egui::TextEdit::singleline(value).desired_width(420.0)).changed() {
            changed = true;
        }
        if ui.button("Browse…").clicked() {
            let p = if pick_file {
                FileDialog::new().add_filter("NeuroPack", &["neuropack"]).pick_file()
            } else {
                FileDialog::new().pick_folder()
            };
            if let Some(p) = p { *value = p.display().to_string(); changed = true; }
        }
    });
    changed
}

fn save_row(ui: &mut egui::Ui, label: &str, value: &mut String, ext: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::TextEdit::singleline(value).desired_width(420.0));
        if ui.button("Browse…").clicked() {
            if let Some(p) = FileDialog::new().add_filter("NeuroPack", &[ext]).save_file() {
                *value = p.display().to_string();
            }
        }
    });
}

/// Render spinner/progress-bar + result banner + optional cancel button.
fn op_status(ui: &mut egui::Ui, op: &SharedOp) {
    let state = op.lock().unwrap();
    if state.running {
        if let Some(ref tok) = state.token {
            let (done, total) = tok.snapshot();
            let frac = if total == 0 { 0.0f32 } else { (done as f32 / total as f32).min(1.0) };
            ui.add(
                egui::ProgressBar::new(frac)
                    .show_percentage()
                    .desired_width(200.0),
            );
            ui.label(format!("{}/{}", done, total));
            let tok_clone = tok.clone();
            drop(state); // release lock before button click
            if ui.button("Cancel").clicked() {
                tok_clone.cancel();
            }
        } else {
            drop(state);
            ui.spinner();
            ui.label("Running…");
        }
    } else {
        if let Some(ref r) = state.result {
            match r {
                Ok(msg)  => { ui.colored_label(egui::Color32::from_rgb(100, 220, 100), format!("✔  {}", msg)); }
                Err(msg) => { ui.colored_label(egui::Color32::from_rgb(255, 100, 100), format!("✘  {}", msg)); }
            }
        }
    }
}

fn fmt_mb(bytes: u64) -> String {
    if bytes == 0 { return "—".into(); }
    if bytes < 1_048_576 { return format!("{} B", bytes); }
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

// ── Tab: Compress ──────────────────────────────────────────────────────────

impl App {
    fn ui_compress(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.heading("Compress a folder into a .neuropack package");
        ui.add_space(8.0);

        egui::Grid::new("cmp_grid").num_columns(1).show(ui, |ui| {
            path_row(ui, "Source folder:", &mut self.compress_src, false); ui.end_row();
            save_row(ui, "Output file:  ", &mut self.compress_out, "neuropack"); ui.end_row();
        });

        ui.add_space(12.0);
        let busy  = self.compress_op.lock().unwrap().running;
        let ready = !self.compress_src.is_empty() && !self.compress_out.is_empty() && !busy;
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("▶  Compress")).clicked() {
                self.start_compress(ctx.clone());
            }
            op_status(ui, &self.compress_op);
        });

        ui.add_space(12.0);
        ui.separator();
        ui.small("Tip: place a neuropack.toml next to the source folder to customise compression levels.");
    }

    fn start_compress(&self, ctx: egui::Context) {
        let src = PathBuf::from(&self.compress_src);
        let out = PathBuf::from(&self.compress_out);
        let op  = self.compress_op.clone();

        // Create a progress token and store it so op_status can read it.
        let token = ProgressToken::new(0);
        {
            let mut s = op.lock().unwrap();
            s.running = true;
            s.result  = None;
            s.token   = Some(token.clone());
        }

        std::thread::spawn(move || {
            let cfg = crate::config::load_config(None).unwrap_or_default();
            let pipeline = crate::compression::Pipeline::from_config(cfg.compression.as_ref());
            let r = pipeline.compress_folder_with_progress(&src, &out, Some(token));
            let mut s = op.lock().unwrap();
            s.running = false;
            s.token   = None;
            s.result  = Some(match r {
                Ok(()) => {
                    let mb = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                    Ok(format!("Done — {:.1} MB → {}", mb as f64 / 1_048_576.0, out.display()))
                }
                Err(e) => Err(e.to_string()),
            });
            ctx.request_repaint();
        });
    }
}

// ── Tab: Extract ───────────────────────────────────────────────────────────

impl App {
    fn ui_extract(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.heading("Extract a .neuropack package");
        ui.add_space(8.0);

        egui::Grid::new("ext_grid").num_columns(1).show(ui, |ui| {
            path_row(ui, "Package file:  ", &mut self.extract_pkg, true); ui.end_row();
            path_row(ui, "Output folder: ", &mut self.extract_out, false); ui.end_row();
        });

        ui.add_space(12.0);
        let busy  = self.extract_op.lock().unwrap().running;
        let ready = !self.extract_pkg.is_empty() && !self.extract_out.is_empty() && !busy;
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("▶  Extract")).clicked() {
                self.start_extract(ctx.clone());
            }
            op_status(ui, &self.extract_op);
        });
    }

    fn start_extract(&self, ctx: egui::Context) {
        let pkg = PathBuf::from(&self.extract_pkg);
        let out = PathBuf::from(&self.extract_out);
        let op  = self.extract_op.clone();
        { let mut s = op.lock().unwrap(); s.running = true; s.result = None; s.token = None; }

        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<String> {
                let reader = PackageReader::open(&pkg)?;
                let total  = reader.index.iter().filter(|e| e.duplicate_of.is_none()).count();
                std::fs::create_dir_all(&out)?;
                let failures = reader.extract_all(&out)?;
                if failures.is_empty() {
                    Ok(format!("Extracted {} files to {}", total, out.display()))
                } else {
                    Ok(format!(
                        "Extracted {} files to {} ({} warning(s))",
                        total,
                        out.display(),
                        failures.len()
                    ))
                }
            })();
            let mut s = op.lock().unwrap();
            s.running = false;
            s.result  = Some(r.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }
}

// ── Tab: List ──────────────────────────────────────────────────────────────

impl App {
    fn poll_list_slot(&mut self) {
        if let Some(slot) = &self.list_slot {
            let mut guard = slot.lock().unwrap();
            if let Some(res) = guard.take() {
                drop(guard);
                self.list_slot = None;
                match res {
                    Ok((entries, tu, tc)) => {
                        self.list_entries   = entries;
                        self.list_total_unc = tu;
                        self.list_total_cmp = tc;
                        self.list_selected  = None;
                    }
                    Err(e) => {
                        self.list_op.lock().unwrap().result = Some(Err(e.to_string()));
                    }
                }
            }
        }
    }

    fn ui_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.heading("Inspect package contents");
        ui.add_space(8.0);

        path_row(ui, "Package:", &mut self.list_pkg, true);

        ui.add_space(6.0);
        let busy  = self.list_op.lock().unwrap().running;
        let ready = !self.list_pkg.is_empty() && !busy;
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("Load")).clicked() {
                self.start_list(ctx.clone());
            }
            if busy { ui.spinner(); ui.label("Loading…"); }
        });

        if self.list_entries.is_empty() {
            op_status(ui, &self.list_op);
            return;
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Search:");
            ui.add(egui::TextEdit::singleline(&mut self.list_search).desired_width(220.0));
            ui.checkbox(&mut self.list_sort_by_size, "Sort by size");
            ui.separator();
            let ratio = if self.list_total_cmp == 0 { 0.0 }
                        else { self.list_total_unc as f64 / self.list_total_cmp as f64 };
            ui.label(format!(
                "{} entries  |  {}  →  {}  |  {:.2}×",
                self.list_entries.len(),
                fmt_mb(self.list_total_unc),
                fmt_mb(self.list_total_cmp),
                ratio,
            ));
        });
        ui.separator();

        let search = self.list_search.to_lowercase();
        let mut visible_indices: Vec<usize> = self.list_entries.iter()
            .enumerate()
            .filter(|(_, e)| search.is_empty() || e.path.to_lowercase().contains(&search))
            .map(|(i, _)| i)
            .collect();
        if self.list_sort_by_size {
            visible_indices.sort_by(|&a, &b| {
                self.list_entries[b].uncompressed_bytes.cmp(&self.list_entries[a].uncompressed_bytes)
            });
        }

        let row_height = 18.0;
        let count = visible_indices.len();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show_rows(
            ui,
            row_height,
            count,
            |ui, range| {
                egui::Grid::new("list_table")
                    .num_columns(6)
                    .striped(true)
                    .min_col_width(40.0)
                    .show(ui, |ui| {
                        if range.start == 0 {
                            for h in ["Path", "Type", "Uncompressed", "Compressed", "Ratio", "Dup"] {
                                ui.strong(h);
                            }
                            ui.end_row();
                        }
                        for vis_idx in &visible_indices[range] {
                            let entry = &self.list_entries[*vis_idx];
                            let selected = self.list_selected == Some(*vis_idx);
                            let display = if entry.path.len() > 58 {
                                format!("…{}", &entry.path[entry.path.len() - 57..])
                            } else {
                                entry.path.clone()
                            };

                            // Selectable row — click to select.
                            let row_resp = ui.selectable_label(selected, &display)
                                .on_hover_text(&entry.path);
                            if row_resp.clicked() {
                                self.list_selected = if selected { None } else { Some(*vis_idx) };
                            }

                            ui.label(&entry.asset_type);
                            ui.label(fmt_mb(entry.uncompressed_bytes));
                            if entry.is_duplicate {
                                ui.label("—");
                                ui.label("dup");
                            } else {
                                ui.label(fmt_mb(entry.compressed_bytes));
                                ui.label(format!("{:.2}×", entry.ratio));
                            }
                            ui.label(entry.duplicate_of.as_deref().unwrap_or(""));
                            ui.end_row();
                        }
                    });
            },
        );

        // ── Single-file extract panel ─────────────────────────────────────
        ui.separator();
        ui.horizontal(|ui| {
            let extract_ready = self.list_selected.is_some()
                && !self.list_extract_op.lock().unwrap().running;

            if ui.add_enabled(extract_ready, egui::Button::new("Extract selected file")).clicked() {
                let idx = self.list_selected.unwrap();
                self.start_extract_file(idx, ctx.clone());
            }

            ui.label("to:");
            ui.add(egui::TextEdit::singleline(&mut self.list_extract_out).desired_width(280.0));
            if ui.button("Browse…").clicked() {
                if let Some(p) = FileDialog::new().pick_folder() {
                    self.list_extract_out = p.display().to_string();
                }
            }

            op_status(ui, &self.list_extract_op);
        });
    }

    fn start_list(&mut self, ctx: egui::Context) {
        let pkg  = PathBuf::from(&self.list_pkg);
        let op   = self.list_op.clone();
        let slot: ListSlot = Arc::new(Mutex::new(None));
        self.list_slot    = Some(slot.clone());
        self.list_entries.clear();
        { let mut s = op.lock().unwrap(); s.running = true; s.result = None; s.token = None; }

        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(Vec<ListEntry>, u64, u64)> {
                let reader = PackageReader::open(&pkg)?;
                let entries = reader.list_entries();
                let tu: u64 = reader.index.iter().map(|e| e.uncompressed_length).sum();
                let tc: u64 = reader.index.iter()
                    .filter(|e| e.duplicate_of.is_none())
                    .map(|e| e.compressed_length).sum();
                Ok((entries, tu, tc))
            })();
            *slot.lock().unwrap() = Some(res);
            op.lock().unwrap().running = false;
            ctx.request_repaint();
        });
    }

    fn start_extract_file(&mut self, entry_idx: usize, ctx: egui::Context) {
        let pkg  = PathBuf::from(&self.list_pkg);
        let rel  = PathBuf::from(&self.list_entries[entry_idx].path);
        let out  = if self.list_extract_out.is_empty() {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        } else {
            PathBuf::from(&self.list_extract_out)
        };
        let op = self.list_extract_op.clone();
        { let mut s = op.lock().unwrap(); s.running = true; s.result = None; s.token = None; }

        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<String> {
                let reader = PackageReader::open(&pkg)?;
                reader.extract_file(&rel, &out)?;
                let file_name = rel.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                Ok(format!("Extracted {} to {}", file_name, out.display()))
            })();
            let mut s = op.lock().unwrap();
            s.running = false;
            s.result  = Some(r.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }
}

// ── Tab: Verify ────────────────────────────────────────────────────────────

impl App {
    fn ui_verify(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.heading("Verify package integrity");
        ui.label("Checks every entry's XXH3 hash without writing anything to disk.");
        ui.add_space(8.0);

        path_row(ui, "Package:", &mut self.verify_pkg, true);

        ui.add_space(10.0);
        let busy  = self.verify_op.lock().unwrap().running;
        let ready = !self.verify_pkg.is_empty() && !busy;
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("▶  Verify")).clicked() {
                self.start_verify(ctx.clone());
            }
            op_status(ui, &self.verify_op);
        });
    }

    fn start_verify(&self, ctx: egui::Context) {
        let pkg = PathBuf::from(&self.verify_pkg);
        let op  = self.verify_op.clone();
        { let mut s = op.lock().unwrap(); s.running = true; s.result = None; s.token = None; }

        std::thread::spawn(move || {
            let r = (|| -> anyhow::Result<String> {
                let reader = PackageReader::open(&pkg)?;
                let report = reader.verify()?;
                if report.failed.is_empty() {
                    Ok(format!("{}/{} entries verified — all ok",
                        report.verified, report.total_entries))
                } else {
                    let msgs: Vec<String> = report.failed.iter()
                        .map(|f| format!("  {}: {}", f.path.display(), f.reason))
                        .collect();
                    Err(anyhow::anyhow!(
                        "{} failure(s):\n{}", report.failed.len(), msgs.join("\n")))
                }
            })();
            let mut s = op.lock().unwrap();
            s.running = false;
            s.result  = Some(r.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }
}

// ── Tab: Analyze ───────────────────────────────────────────────────────────

impl App {
    fn ui_analyze(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.add_space(8.0);
        ui.heading("Analyze a folder");
        ui.label("Reports asset breakdown, exact duplicates, and similar file groups.");
        ui.add_space(8.0);

        egui::Grid::new("ana_grid").num_columns(1).show(ui, |ui| {
            path_row(ui, "Source folder:", &mut self.analyze_src, false); ui.end_row();
            save_row(ui, "Report JSON:   ", &mut self.analyze_out, "json"); ui.end_row();
        });

        ui.add_space(10.0);
        let busy  = self.analyze_op.lock().unwrap().running;
        let ready = !self.analyze_src.is_empty() && !busy;
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("▶  Analyze")).clicked() {
                self.start_analyze(ctx.clone());
            }
            op_status(ui, &self.analyze_op);
        });

        let state = self.analyze_op.lock().unwrap();
        if let Some(Ok(ref msg)) = state.result {
            ui.add_space(8.0);
            ui.separator();
            for line in msg.lines() {
                ui.label(line);
            }
        }
    }

    fn start_analyze(&self, ctx: egui::Context) {
        let src     = PathBuf::from(&self.analyze_src);
        let out_opt = if self.analyze_out.is_empty() { None }
                      else { Some(PathBuf::from(&self.analyze_out)) };
        let op = self.analyze_op.clone();
        { let mut s = op.lock().unwrap(); s.running = true; s.result = None; s.token = None; }

        std::thread::spawn(move || {
            let r = analyze_folder(&src, out_opt);
            let mut s = op.lock().unwrap();
            s.running = false;
            s.result  = Some(r.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }
}

fn analyze_folder(src: &PathBuf, out: Option<PathBuf>) -> anyhow::Result<String> {
    use crate::asset_scanner::{AssetScanner, AssetType};
    use crate::duplicate::{ExactDuplicateCluster, find_similar_files};

    let assets = AssetScanner::default().scan(src)?;
    let total_bytes: u64 = assets.iter().map(|a| a.size).sum();

    let mb = |t: AssetType| -> f64 {
        assets.iter().filter(|a| a.asset_type == t).map(|a| a.size).sum::<u64>() as f64
            / 1_048_576.0
    };

    let clusters = ExactDuplicateCluster::find(&assets);
    let wasted: u64 = clusters.iter().map(|c| c.size * (c.paths.len() as u64 - 1)).sum();
    let similar = find_similar_files(&assets, 4096, 3);

    let summary = format!(
        "{} files  |  {:.1} MB total\n\
         Textures {:.1} MB  ·  Meshes {:.1} MB  ·  Audio {:.1} MB\n\
         {} duplicate cluster(s), {:.1} MB wasted\n\
         {} similar file group(s)",
        assets.len(),
        total_bytes as f64 / 1_048_576.0,
        mb(AssetType::Texture), mb(AssetType::Mesh), mb(AssetType::Audio),
        clusters.len(),
        wasted as f64 / 1_048_576.0,
        similar.len(),
    );

    if let Some(out_path) = out {
        let json = serde_json::json!({
            "total_files": assets.len(),
            "total_bytes": total_bytes,
            "duplicate_clusters": clusters.len(),
            "bytes_wasted_by_duplicates": wasted,
            "similar_groups": similar.len(),
        });
        std::fs::write(&out_path, serde_json::to_vec_pretty(&json)?)?;
    }

    Ok(summary)
}
