use egui::*;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread;
use std::process::Command;
use uuid::Uuid;

use crate::annotations::{AnnotationSidecar, pdf_fingerprint};
use crate::annotations::*;
use crate::pdf_engine::{PdfEngine, WorkerRequest, WorkerResponse};
use crate::tab::{PdfTab, NoteViewer};
use crate::ui::{draw_toolbar, handle_shortcuts, Tool, ToolState, ToastMessage};
use crate::layered_view::{ActiveShape, CurrentStroke, LayeredPageView};
use crate::text_index::TextIndex;
use crate::bookmarks::{BookmarkManager, Bookmark, LinkData, FolderData, PdfOutlineEntry, PdfNativeBookmark};

enum BookmarkAction {
    /// Load the file at `url`. If `page` is Some, jump there after the file loads.
    Load(String, Option<usize>),
    Delete(String),
    GoToPage(usize),
    None,
}

// ── Top-bar icon+label action button ─────────────────────────────────────────

/// Renders a vertically-stacked icon + label button for the top toolbar.
/// Returns true if the button was clicked.
fn toolbar_action_button(
    ui: &mut Ui,
    icon: egui::ImageSource<'static>,
    label: &str,
    icon_tint: Color32,
    lbl_color: Color32,
) -> bool {
    let btn_size = Vec2::new(56.0, 48.0);
    let (rect, resp) = ui.allocate_exact_size(btn_size, Sense::click());

    let dark = ui.visuals().dark_mode;
    let bg = if resp.hovered() {
        if dark { Color32::from_rgb(35, 45, 65) } else { Color32::from_rgb(220, 228, 240) }
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, Rounding::same(6.0), bg);

    // Icon centred in upper portion
    let icon_rect = egui::Rect::from_center_size(
        rect.center() - Vec2::new(0.0, 8.0),
        Vec2::splat(20.0),
    );
    ui.put(icon_rect, egui::Image::new(icon).tint(icon_tint));

    // Label below icon
    ui.painter().text(
        rect.center() + Vec2::new(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(10.0),
        lbl_color,
    );

    resp.clicked()
}

// ── Modals ────────────────────────────────────────────────────────────────────

pub struct NoteModal  { pub open: bool, pub text: String, pub tab_id: Option<Uuid>, pub page: usize, pub pos: Pos2 }
pub struct TextModal  { pub open: bool, pub text: String, pub tab_id: Option<Uuid>, pub page: usize, pub pos: Pos2, pub font_size: f32 }

#[derive(Clone, PartialEq, Eq)]
pub enum ConversionState {
    None,
    Converting(String),
}

// ── App struct ────────────────────────────────────────────────────────────────

pub struct IkraApp {
    tabs: Vec<PdfTab>,
    active_tab_id: Option<Uuid>,
    tool_state: ToolState,
    toast: Option<ToastMessage>,
    note_modal: NoteModal,
    text_modal: TextModal,

    conversion_state: ConversionState,
    conversion_rx: Option<Receiver<Result<String, String>>>,
    last_known_conversion_path: String,

    /// Independent text search index (never serialised).
    /// Populated by `WorkerResponse::TextExtracted` on every document open.
    text_index: TextIndex,

    worker_tx: Sender<WorkerRequest>,
    worker_rx: Receiver<WorkerResponse>,

    bookmarks: BookmarkManager,
    bookmarks_path: std::path::PathBuf,
    pub show_bookmarks: bool,
    settings_path: std::path::PathBuf,
    pub pending_file: Option<String>,

    /// When a bookmark with a page number triggers a Load, we remember
    /// `(canonical_path, 0-based page)` here so we can jump to it once
    /// `WorkerResponse::Loaded` arrives for that file.
    pending_page_jump: Option<(String, usize)>,


}

// ── Per-page canvas (now delegates to LayeredPageView) ────────────────────────

fn draw_tab_canvas(
    tab: &mut PdfTab,
    tool_state: &mut ToolState,
    note_modal: &mut NoteModal,
    text_modal: &mut TextModal,
    ui: &mut Ui,
    page_idx: usize,
    interact_id: Id,
    ctx: &Context,
) -> Option<Rect> {
    // ── Update last_accessed stamp & gather immutable data ────────────────────
    if let Some(c) = tab.page_textures.get_mut(&page_idx) {
        c.last_accessed = ctx.input(|i| i.time);
    }
    let cache = tab.page_textures.get(&page_idx);
    let (tex_handle, load_time) = if let Some(c) = cache {
        (Some(&c.texture), c.load_time)
    } else {
        (None, 0.0)
    };

    let page_size_pts = tab.page_sizes.get(page_idx).copied().unwrap_or((595.0, 842.0));

    let annotations = tab.doc.pages.get(page_idx)
        .map(|p| p.annotations.clone())
        .unwrap_or_default();

    let text_map = tab.doc.pages.get(page_idx)
        .map(|p| p.text_map.clone())
        .unwrap_or_default();

    let search_rects: Vec<egui::Rect> = if !tab.search_query.trim().is_empty()
        && page_idx < tab.search_rects.len()
    {
        tab.search_rects[page_idx].clone()
    } else {
        Vec::new()
    };

    let scale  = tab.scale;
    let tab_id = tab.id;

    // ── Temporarily take mutable live-draw buffers out of tab ─────────────────
    let mut current_stroke_tmp = tab.current_stroke.take();
    let mut active_shape_tmp   = tab.active_shape.take();

    // Copy NoteViewer so we don't hold a &tab borrow into the widget block
    let note_viewer_copy: Option<NoteViewer> = tab.note_viewer.as_ref()
        .map(|nv| NoteViewer { page: nv.page, annot_index: nv.annot_index });

    // Copy the active shape data for the widget display (owned, so no borrow conflict).
    let active_shape_copy: Option<ActiveShape> = active_shape_tmp
        .as_ref()
        .and_then(|s| if s.page == page_idx {
            Some(ActiveShape { page: s.page, start: s.start, end: s.end })
        } else { None });

    let nv_ref = note_viewer_copy.as_ref();

    let widget = LayeredPageView::new(
        page_idx, scale,
        tex_handle, load_time,
        &annotations, &text_map, &search_rects,
        active_shape_copy.as_ref(), nv_ref,
        interact_id,
    );

    let mut click_pos: Option<Pos2> = None;
    let rect = widget.show(
        ui, ctx, tool_state, page_size_pts,
        &mut current_stroke_tmp,
        &mut active_shape_tmp,
        &mut |pdf_pos| { click_pos = Some(pdf_pos); },
    );


    // ── Restore live-draw buffers ─────────────────────────────────────────────
    tab.current_stroke = current_stroke_tmp;
    tab.active_shape   = active_shape_tmp;

    // ── Handle click → open modals ────────────────────────────────────────────
    if let Some(pdf_pos) = click_pos {
        match tool_state.tool {
            Tool::Note => {
                *note_modal = NoteModal {
                    open: true, text: String::new(),
                    tab_id: Some(tab_id),
                    page: page_idx, pos: pdf_pos,
                };
            }
            Tool::TextBox => {
                *text_modal = TextModal {
                    open: true, text: String::new(),
                    tab_id: Some(tab_id),
                    page: page_idx, pos: pdf_pos,
                    font_size: tool_state.brush_size * 5.0 + 8.0,
                };
            }
            _ => {}
        }
    }

    // ── Drag-stopped: commit strokes/shapes to annotation layer ──────────────
    let response = ui.interact(rect, interact_id, Sense::click_and_drag());

    if response.drag_stopped() {
        if tab.current_stroke.is_some() || tab.active_shape.is_some() {
            if !tab.needs_undo_checkpoint {
                tab.doc.push_undo_checkpoint();
                tab.needs_undo_checkpoint = true;
            }
        }
        if let Some(stroke) = tab.current_stroke.take() {
            if stroke.page == page_idx {
                let kind = if tool_state.tool == Tool::Highlight {
                    AnnotKind::Highlight
                } else {
                    AnnotKind::Pen
                };
                let color = color32_to_arr(tool_state.color);
                tab.doc.add_annot(page_idx, AnnotationShape::Pen(PenStroke {
                    kind, points: stroke.points, color,
                    width: tool_state.brush_size,
                }));
            }
        }
        if let Some(shape) = tab.active_shape.take() {
            if shape.page == page_idx {
                let kind = match tool_state.tool {
                    Tool::Rect    => ShapeKind::Rect,
                    Tool::Ellipse => ShapeKind::Ellipse,
                    Tool::Arrow   => ShapeKind::Arrow,
                    _             => ShapeKind::Line,
                };
                let color = color32_to_arr(tool_state.color);
                tab.doc.add_annot(page_idx, AnnotationShape::Shape(ShapeAnnot {
                    kind,
                    start: [shape.start.x, shape.start.y],
                    end:   [shape.end.x,   shape.end.y],
                    color, width: tool_state.brush_size,
                    fill: None, dashed: false,
                }));
            }
        }
    }

    // ── Eraser ────────────────────────────────────────────────────────────────
    if tool_state.tool == Tool::Eraser {
        if let Some(pos) = response.hover_pos() {
            if response.dragged() || response.drag_started() {
                if response.drag_started() && !tab.needs_undo_checkpoint {
                    tab.doc.push_undo_checkpoint();
                    tab.needs_undo_checkpoint = true;
                }
                let pdf_pos = Pos2::new(
                    (pos.x - rect.min.x) / scale,
                    (pos.y - rect.min.y) / scale,
                );
                tab.doc.erase_at(page_idx, pdf_pos, 20.0);
            }
        }
    }

    // ── Note viewer hover ─────────────────────────────────────────────────────
    if tool_state.tool == Tool::Cursor {
        if let Some(hover) = response.hover_pos() {
            let mut found = None;
            if let Some(page_layer) = tab.doc.pages.get(page_idx) {
                for (idx, annot) in page_layer.annotations.shapes.iter().enumerate() {
                    if let AnnotationShape::Note(n) = annot {
                        let ns = Pos2::new(
                            rect.min.x + n.pos[0] * scale,
                            rect.min.y + n.pos[1] * scale,
                        );
                        if (hover - ns).length() < 16.0 {
                            found = Some((page_idx, idx));
                            break;
                        }
                    }
                }
            }
            tab.note_viewer = found.map(|(page, annot_index)| NoteViewer { page, annot_index });
        }
    }

    Some(rect)
}



// ── App impl ──────────────────────────────────────────────────────────────────

impl IkraApp {
    pub fn new(cc: &eframe::CreationContext<'_>, initial_file: Option<String>) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        
        let mut visuals = egui::Visuals::dark();
        visuals.window_shadow = egui::epaint::Shadow::NONE;
        visuals.popup_shadow = egui::epaint::Shadow::NONE;
        cc.egui_ctx.set_visuals(visuals);

        let (wt_tx, wr_rx) = channel();
        let (wr_tx, wt_rx) = channel();

        PdfEngine::start_worker(wr_rx, wr_tx);

        let mut bpath = std::env::current_dir().unwrap_or_default();
        bpath.push("bookmarks.json");

        let mut settings_path = std::env::current_dir().unwrap_or_default();
        settings_path.push("settings.json");
        let tool_state = ToolState::load_from_disk(&settings_path).unwrap_or_default();

        Self {
            tabs: Vec::new(),
            active_tab_id: None,
            tool_state,
            toast: None,
            note_modal: NoteModal { open: false, text: String::new(), tab_id: None, page: 0, pos: Pos2::ZERO },
            text_modal: TextModal { open: false, text: String::new(), tab_id: None, page: 0, pos: Pos2::ZERO, font_size: 16.0 },
            conversion_state: ConversionState::None,
            conversion_rx: None,
            last_known_conversion_path: String::new(),
            text_index: TextIndex::new(),
            worker_tx: wt_tx,
            worker_rx: wt_rx,
            bookmarks: BookmarkManager::load_from_disk(&bpath).unwrap_or_else(|_| BookmarkManager::new()),
            bookmarks_path: bpath,
            show_bookmarks: false,
            settings_path,
            pending_file: initial_file,
            pending_page_jump: None,

        }
    }

    fn show_toast(&mut self, text: impl Into<String>, ctx: &Context) {
        self.toast = Some(ToastMessage::new(text, ctx));
    }

    fn active_tab(&mut self) -> Option<&mut PdfTab> {
        let id = self.active_tab_id?;
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_tab_ref(&self) -> Option<&PdfTab> {
        let id = self.active_tab_id?;
        self.tabs.iter().find(|t| t.id == id)
    }

    fn load_file(&mut self, path: &str, ctx: &Context) {
        if crate::pdf_engine::is_pdf_file(path) {
            let id = Uuid::new_v4();
            self.show_toast(format!("Loading {}", path), ctx);
            let _ = self.worker_tx.send(WorkerRequest::Load(id, path.to_string()));
        } else {
            self.start_conversion(path.to_string(), ctx);
        }
    }

    fn start_conversion(&mut self, path: String, ctx: &Context) {
        let (tx, rx) = channel();
        self.conversion_rx = Some(rx);
        self.conversion_state = ConversionState::Converting(path.clone());
        self.last_known_conversion_path = path.clone();
        let ctx = ctx.clone();

        thread::spawn(move || {
            let temp_dir = std::env::temp_dir();
            let output = Command::new("soffice")
                .args(["--headless", "--convert-to", "pdf", "--outdir", temp_dir.to_str().unwrap(), &path])
                .output();

            let result = match output {
                Ok(out) if out.status.success() => {
                    let file_name = std::path::Path::new(&path)
                        .file_stem().unwrap_or_default().to_string_lossy();
                    let pdf_path = temp_dir.join(format!("{}.pdf", file_name));
                    Ok(pdf_path.to_string_lossy().into_owned())
                }
                Ok(out) => Err(String::from_utf8_lossy(&out.stderr).into_owned()),
                Err(e)  => Err(format!("Failed to run soffice. Make sure LibreOffice is installed. Error: {}", e)),
            };
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    // ── Save annotations to versioned JSON sidecar ────────────────────────────

    fn save_annotations_json(&self, tab: &PdfTab) {
        let sidecar = std::path::Path::new(&tab.file_path).with_extension("ikra.json");
        if let Ok(json) = tab.annotations_to_sidecar_json() {
            let _ = std::fs::write(&sidecar, json);
        }
    }

    fn load_annotations_json(&mut self, tab_id: Uuid, ctx: &Context) {
        // We need the file path but also a mutable tab reference later.
        // Split into two borrows to satisfy the borrow checker.
        let file_path = match self.tabs.iter().find(|t| t.id == tab_id) {
            Some(t) => t.file_path.clone(),
            None    => return,
        };
        let sidecar = std::path::Path::new(&file_path).with_extension("ikra.json");
        if let Ok(data) = std::fs::read_to_string(&sidecar) {
            match AnnotationSidecar::from_json(&data) {
                Ok(loaded) => {
                    // Check PDF integrity fingerprint
                    let fp_mismatch = match (&loaded.pdf_fingerprint, pdf_fingerprint(&file_path)) {
                        (Some(saved), Some(current)) => saved != &current,
                        _ => false,
                    };
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        tab.annotations_from_sidecar(&loaded);
                    }
                    if fp_mismatch {
                        self.show_toast(
                            "⚠ PDF changed since last save — annotations may be misaligned",
                            ctx,
                        );
                    }
                }
                Err(e) => {
                    self.show_toast(format!("⚠ Could not load annotations: {}", e), ctx);
                }
            }
        }
    }

    fn draw_bookmark_tree(ui: &mut Ui, nodes: &[Bookmark]) -> BookmarkAction {
        let mut action = BookmarkAction::None;
        for node in nodes {
            match node {
                Bookmark::Folder(folder) => {
                    egui::collapsing_header::CollapsingState::load_with_default_open(
                        ui.ctx(),
                        Id::new(&folder.id),
                        false,
                    )
                    .show_header(ui, |ui| {
                        ui.label(format!("📁 {}", folder.title));
                        if ui.button("🗑").on_hover_text("Delete Folder").clicked() {
                            action = BookmarkAction::Delete(folder.id.clone());
                        }
                    })
                    .body(|ui| {
                        let a = Self::draw_bookmark_tree(ui, &folder.children);
                        if !matches!(a, BookmarkAction::None) {
                            action = a;
                        }
                    });
                }
                Bookmark::Link(link) => {
                    ui.horizontal(|ui| {
                        // Main button: load the file (and carry along the saved page).
                        let btn_label = format!("📄 {}", link.title);
                        if ui.button(&btn_label)
                            .on_hover_text(&link.url)
                            .clicked()
                        {
                            action = BookmarkAction::Load(link.url.clone(), link.page);
                        }

                        // If this bookmark has a saved page, show a small badge
                        // button that navigates directly to that page in the
                        // currently-active tab (useful when the file is already open).
                        if let Some(pg) = link.page {
                            let badge_label = format!("p.{}", pg + 1);
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(&badge_label)
                                            .size(10.0)
                                            .color(Color32::from_rgb(49, 130, 206)),
                                    )
                                    .small()
                                    .frame(true),
                                )
                                .on_hover_text(format!("Jump to page {} in the active tab", pg + 1))
                                .clicked()
                            {
                                action = BookmarkAction::GoToPage(pg);
                            }
                        }

                        if ui.button("🗑").on_hover_text("Delete Bookmark").clicked() {
                            action = BookmarkAction::Delete(link.id.clone());
                        }
                    });
                }
            }
        }
        action
    }

    /// Collect all user bookmarks that target `file_path` and have a saved page,
    /// converting them into native PDF outline entries for burn-in on save.
    /// Folder hierarchy is preserved: a Bookmark::Folder becomes a parent node
    /// whose target_page is the first page-bearing descendant (so it has a valid
    /// jump target), and its children are mapped recursively.
    fn native_bookmarks_for_file(bms: &[Bookmark], file_path: &str) -> Vec<PdfNativeBookmark> {
        let mut out = Vec::new();
        for bm in bms {
            match bm {
                Bookmark::Link(l) if l.url == file_path => {
                    if let Some(pg) = l.page {
                        out.push(PdfNativeBookmark {
                            title:       l.title.clone(),
                            target_page: pg,
                            children:    Vec::new(),
                        });
                    }
                }
                Bookmark::Folder(f) => {
                    // Build children recursively first so we can derive a target page.
                    let children = Self::native_bookmarks_for_file(&f.children, file_path);
                    if children.is_empty() {
                        // No page-bearing descendants for this file — skip the folder.
                        continue;
                    }
                    // Use the first child's target_page as this folder node's jump target
                    // (required: every lopdf::Bookmark must have a page ObjectId).
                    let target_page = children[0].target_page;
                    out.push(PdfNativeBookmark {
                        title: f.title.clone(),
                        target_page,
                        children,
                    });
                }
                _ => {}
            }
        }
        out
    }

    /// Each entry with children becomes a collapsible section; clicking
    /// an entry navigates to its destination page.
    fn draw_pdf_outline(
        ui: &mut Ui,
        entries: &[PdfOutlineEntry],
        goto_page: &mut Option<usize>,
        depth: usize,
    ) {
        for (i, entry) in entries.iter().enumerate() {
            let has_children = !entry.children.is_empty();
            let page_label = entry.page.map(|p| format!("p.{}", p + 1)).unwrap_or_default();

            if has_children {
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    Id::new(format!("pdf_outline_{}_{}_{}", depth, i, &entry.title)),
                    depth == 0,  // top-level sections open by default
                )
                .show_header(ui, |ui| {
                    let btn_text = if page_label.is_empty() {
                        format!("📂 {}", entry.title)
                    } else {
                        format!("📂 {} ({})", entry.title, page_label)
                    };
                    if ui.button(RichText::new(btn_text).size(13.0)).clicked() {
                        if let Some(p) = entry.page {
                            *goto_page = Some(p);
                        }
                    }
                })
                .body(|ui| {
                    Self::draw_pdf_outline(ui, &entry.children, goto_page, depth + 1);
                });
            } else {
                ui.horizontal(|ui| {
                    let btn_text = if page_label.is_empty() {
                        format!("  📄 {}", entry.title)
                    } else {
                        format!("  📄 {} ({})", entry.title, page_label)
                    };
                    if ui.button(RichText::new(btn_text).size(12.5)).clicked() {
                        if let Some(p) = entry.page {
                            *goto_page = Some(p);
                        }
                    }
                });
            }
        }
    }
}

// ── eframe App ────────────────────────────────────────────────────────────────

impl eframe::App for IkraApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        if let Some(path) = self.pending_file.take() {
            self.load_file(&path, ctx);
        }

        // ── Cross-Platform Drag and Drop Handler ─────────────────────────────
        let dropped_file = ctx.input(|i| {
            i.raw.dropped_files.first()
                .and_then(|f| f.path.as_ref())
                .and_then(|p| p.to_str())
                .map(|s| {
                    // 1. Strip file:// protocol strings safely across systems
                    let mut path_str = s.to_string();
                    if path_str.starts_with("file://") {
                        path_str = path_str.replacen("file://", "", 1);
                        
                        // On Windows, file:///C:/path becomes /C:/path. 
                        // Strip the leading slash if a drive letter follows it.
                        if path_str.starts_with('/') && path_str.chars().nth(2) == Some(':') {
                            path_str.remove(0);
                        }
                    }

                    // 2. In-place Percent/URL Decoding (handles spaces %20, symbols, and accents)
                    let mut decoded = String::new();
                    let mut chars = path_str.chars();
                    while let Some(ch) = chars.next() {
                        if ch == '%' {
                            if let (Some(c1), Some(c2)) = (chars.next(), chars.next()) {
                                if let Ok(byte) = u8::from_str_radix(&format!("{}{}", c1, c2), 16) {
                                    decoded.push(byte as char);
                                    continue;
                                }
                                decoded.push('%');
                                decoded.push(c1);
                                decoded.push(c2);
                            } else {
                                decoded.push('%');
                            }
                        } else {
                            decoded.push(ch);
                        }
                    }
                    decoded
                })
        });

        if let Some(path) = dropped_file {
            self.load_file(&path, ctx);
            ctx.request_repaint(); // Force immediate UI switch to the new tab
        }

        // Apply themes globally
        match self.tool_state.theme {
            crate::ui::Theme::Dark => {
                let mut visuals = Visuals::dark();
                visuals.window_fill = Color32::from_rgb(35, 38, 41);  // KDE Breeze Dark base
                visuals.panel_fill  = Color32::from_rgb(42, 46, 50);  // KDE Breeze Dark panel
                
                // Breeze Blue accent
                let breeze_blue = Color32::from_rgb(61, 174, 233);
                visuals.selection.bg_fill = breeze_blue;
                visuals.hyperlink_color = breeze_blue;
                
                // Breeze-like widget styling: 3px rounding, crisp borders
                let rounding = Rounding::same(8.0);
                visuals.widgets.noninteractive.rounding = rounding;
                visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(49, 54, 59));
                
                visuals.widgets.inactive.rounding = rounding;
                visuals.widgets.inactive.bg_fill = Color32::from_rgb(49, 54, 59);
                visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(61, 67, 73));
                
                visuals.widgets.hovered.rounding = rounding;
                visuals.widgets.hovered.bg_fill = Color32::from_rgb(61, 67, 73);
                visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, breeze_blue);
                
                visuals.widgets.active.rounding = rounding;
                visuals.widgets.active.bg_fill = Color32::from_rgb(42, 46, 50);
                visuals.widgets.active.bg_stroke = Stroke::new(1.0, breeze_blue);
                
                ctx.set_visuals(visuals);
            }
            crate::ui::Theme::Light => {
                let mut visuals = Visuals::light();
                visuals.window_fill = Color32::from_rgb(239, 240, 241); // KDE Breeze Light base
                visuals.panel_fill  = Color32::from_rgb(252, 252, 252); // KDE Breeze Light panel
                
                let breeze_blue = Color32::from_rgb(61, 174, 233);
                visuals.selection.bg_fill = breeze_blue;
                visuals.hyperlink_color = breeze_blue;
                
                let rounding = Rounding::same(8.0);
                visuals.widgets.noninteractive.rounding = rounding;
                visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(200, 204, 207));
                
                visuals.widgets.inactive.rounding = rounding;
                visuals.widgets.inactive.bg_fill = Color32::from_rgb(220, 224, 227);
                visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(189, 195, 199));
                
                visuals.widgets.hovered.rounding = rounding;
                visuals.widgets.hovered.bg_fill = Color32::from_rgb(235, 238, 240);
                visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, breeze_blue);
                
                visuals.widgets.active.rounding = rounding;
                visuals.widgets.active.bg_fill = Color32::from_rgb(252, 252, 252);
                visuals.widgets.active.bg_stroke = Stroke::new(1.0, breeze_blue);
                
                ctx.set_visuals(visuals);
            }
            crate::ui::Theme::Glassmorphism => {
                let mut visuals = Visuals::dark();
                // Overriding backgrounds to be mostly transparent so actual design elements can pop
                visuals.window_fill = Color32::from_rgba_premultiplied(30, 30, 30, 200);
                visuals.panel_fill  = Color32::from_rgb(18, 18, 18);
                // White glints
                visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_white_alpha(30));
                ctx.set_visuals(visuals);
            }
            crate::ui::Theme::Skeuomorphism => {
                let mut visuals = Visuals::dark();
                visuals.window_fill = Color32::from_rgb(40, 40, 45);
                visuals.panel_fill  = Color32::from_rgb(30, 32, 40);
                visuals.widgets.noninteractive.rounding = Rounding::same(8.0);
                visuals.widgets.inactive.rounding = Rounding::same(8.0);
                ctx.set_visuals(visuals);
            }
        }

        let mut style = (*ctx.style()).clone();
        style.visuals.window_shadow = egui::epaint::Shadow::NONE;
        style.visuals.popup_shadow = egui::epaint::Shadow::NONE;
        style.visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
        style.visuals.widgets.hovered.rounding  = egui::Rounding::same(8.0);
        style.visuals.widgets.active.rounding   = egui::Rounding::same(8.0);
        style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
        ctx.set_style(style);
        // Flush GPU garbage from last frame — explicitly free textures
        for tab in &mut self.tabs {
            for tex in tab.garbage_textures.drain(..) {
                drop(tex);  // Force immediate drop
            }
        }

        // ── Worker responses ──────────────────────────────────────────────────
        while let Ok(res) = self.worker_rx.try_recv() {
            match res {
                WorkerResponse::Loaded(id, path, _count, sizes, Ok(())) => {
                    // Check before moving `path` into the tab whether we have a
                    // pending page-jump for this exact file.
                    let jump_page = self.pending_page_jump
                        .as_ref()
                        .filter(|(p, _)| p == &path)
                        .map(|(_, pg)| *pg);
                    if jump_page.is_some() {
                        self.pending_page_jump = None;
                    }

                    let mut tab = PdfTab::new(id, path, sizes);
                    // Apply the saved page jump immediately if the tab already
                    // knows its page count; otherwise it defaults to page 0 which
                    // is fine — the outline scroll will show the right page once
                    // the first render fires.
                    if let Some(pg) = jump_page {
                        if pg < tab.page_count() {
                            tab.current_page = pg;
                        }
                    }
                    let tab_id = tab.id;
                    self.tabs.push(tab);
                    self.active_tab_id = Some(tab_id);
                    // Load any existing annotation sidecar (with fingerprint check)
                    self.load_annotations_json(tab_id, ctx);
                    self.show_toast("PDF loaded — extracting text index…", ctx);
                }
                WorkerResponse::Loaded(_id, _path, _count, _sizes, Err(e)) => {
                    self.toast = Some(ToastMessage::new(format!("Failed to load: {}", e), ctx));
                }
                // ── Text extraction complete ──────────────────────────────────
                WorkerResponse::TextExtracted(id, text_maps) => {
                    // 1. Store in the independent TextIndex for direct search access.
                    self.text_index.insert(id, text_maps.clone());
                    // 2. Populate PageLayer.text_map so existing find_text() calls work.
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        for (i, tm) in text_maps.into_iter().enumerate() {
                            if i < tab.doc.pages.len() {
                                tab.doc.pages[i].text_map = tm;
                            }
                        }
                    }
                }
                // ── Outline (PDF bookmarks) extracted ─────────────────────────
                WorkerResponse::OutlineExtracted(id, outline) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.pdf_outline = outline;
                    }
                }
                WorkerResponse::Rendered(id, page, scale, kind, img) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.process_render_response(id, page, scale, kind, img, ctx);
                    }
                }
                WorkerResponse::Saved(_id, Ok(())) => {
                    self.show_toast("Saved!", ctx);
                }
                WorkerResponse::Saved(_id, Err(e)) => {
                    self.show_toast(format!("Save error: {}", e), ctx);
                }
                WorkerResponse::PageAdded(id, Ok((new_count, w, h))) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.page_sizes.push((w, h));
                        tab.doc.pages.push(crate::annotations::PageLayer::default());
                        tab.current_page = new_count - 1;
                        for (_, cache) in tab.page_textures.drain() {
                            tab.garbage_textures.push(cache.texture);
                        }
                        tab.pending_renders.clear();
                    }
                    self.show_toast("Blank page added!", ctx);
                }
                WorkerResponse::PageAdded(_id, Err(e)) => {
                    self.show_toast(format!("Add page failed: {}", e), ctx);
                }
            }
        }

        // ── Conversion polling ────────────────────────────────────────────────
        if let Some(rx) = &self.conversion_rx {
            if let Ok(res) = rx.try_recv() {
                self.conversion_state = ConversionState::None;
                self.conversion_rx = None;
                match res {
                    Ok(pdf_path) => {
                        self.load_file(&pdf_path, ctx);
                        self.show_toast("Conversion successful!", ctx);
                    }
                    Err(e) => {
                        self.show_toast(format!("Conversion failed: {}", e), ctx);
                    }
                }
            }
        }

        // ── Drag and Drop ─────────────────────────────────────────────────────
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for file in dropped {
            if let Some(path) = file.path {
                self.load_file(&path.to_string_lossy(), ctx);
            }
        }

        // ── Ctrl+Scroll zoom ──────────────────────────────────────────────────
        let (scroll_delta, ctrl) = ctx.input(|i| (i.raw_scroll_delta, i.modifiers.ctrl));
        if ctrl && scroll_delta.y.abs() > 0.5 {
            let dy = scroll_delta.y;
            let tx = self.worker_tx.clone();
            if let Some(tab) = self.active_tab() {
                let new_scale = if dy > 0.0 { tab.scale * 1.1 } else { tab.scale / 1.1 };
                tab.set_scale(new_scale, Some(&tx));
            }
        }

        // ── Handle drag & drop files ──────────────────────────────────────────────
        ctx.input(|i| {
            for file in &i.raw.dropped_files {
                if let Some(path) = &file.path {
                    let path_str = path.to_string_lossy().to_string();
                    self.load_file(&path_str, ctx);
                }
            }
        });

        // ── Keyboard shortcuts ────────────────────────────────────────────────
        if let Some(action) = handle_shortcuts(&mut self.tool_state, ctx) {
            if action == "save" {
                if let Some(tab) = self.active_tab_ref() {
                    let tab_id = tab.id;
                    let path   = tab.file_path.clone();
                    let annots = tab.annotation_layers_cloned();
                    self.save_annotations_json(tab);
                    let native_bms = Self::native_bookmarks_for_file(&self.bookmarks.root, &path);
                    let _ = self.worker_tx.send(WorkerRequest::Save(tab_id, path, annots, native_bms));
                    self.show_toast("Saving…", ctx);
                }
            } else if action == "next_tab" {
                if !self.tabs.is_empty() {
                    let idx = self.tabs.iter().position(|t| Some(t.id) == self.active_tab_id).unwrap_or(0);
                    let next_idx = (idx + 1) % self.tabs.len();
                    self.active_tab_id = Some(self.tabs[next_idx].id);
                }
            } else if action == "close_tab" {
                if let Some(id) = self.active_tab_id {
                    let _ = self.worker_tx.send(WorkerRequest::Close(id));
                    
                    // Explicitly drain and drop all textures BEFORE removing tab
                    if let Some(tab_pos) = self.tabs.iter().position(|t| t.id == id) {
                        let mut tab = self.tabs.remove(tab_pos);
                        // Force immediate GPU memory release
                        for tex in tab.page_textures.values_mut() {
                            // Move texture to garbage queue and let egui free it
                            tab.garbage_textures.push(tex.texture.clone());
                        }
                        tab.page_textures.clear();
                        for tex in tab.thumb_textures.values_mut() {
                            tab.garbage_textures.push(tex.clone());
                        }
                        tab.thumb_textures.clear();
                        // Drain garbage immediately
                        for tex in tab.garbage_textures.drain(..) {
                            drop(tex);
                        }
                    }
                    
                    if self.active_tab_id == Some(id) {
                        self.active_tab_id = self.tabs.last().map(|t| t.id);
                    }
                }
            } else if action == "undo" {
                if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                    if tab.doc.undo() { self.show_toast("Undo", ctx); }
                }
            } else if action == "redo" {
                if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                    if tab.doc.redo() { self.show_toast("Redo", ctx); }
                }
            }
        }

        // ── Top bar ───────────────────────────────────────────────────────────
        TopBottomPanel::top("top_bar")
            .frame(egui::Frame::none().fill(ctx.style().visuals.panel_fill).stroke(egui::Stroke::NONE))
            .min_height(80.0)
            .show(ctx, |ui| {
                // Tab bar (no separator at the bottom)
                if let Some(path) = crate::ui::draw_tab_bar(ui, &mut self.tabs, &mut self.active_tab_id, &self.worker_tx) {
                    self.load_file(&path, ctx);
                }

                let dark = ui.visuals().dark_mode;
                // Icon tint: white on dark theme so icons are visible
                let icon_tint = if dark { Color32::WHITE } else { Color32::from_gray(30) };
                let lbl_color = if dark { Color32::from_gray(210) } else { Color32::from_gray(50) };

                ui.add_space(4.0);
                egui::ScrollArea::horizontal()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.add_space(12.0);

                    // ── Open ──────────────────────────────────────────────────
                    let open_clicked = toolbar_action_button(
                        ui, egui::include_image!("../assets/folder-open.svg"),
                        "Open", icon_tint, lbl_color,
                    );
                    if open_clicked {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("All Supported", &["pdf", "docx", "doc", "odt", "rtf", "ppt", "pptx", "odp"])
                            .pick_file()
                        {
                            self.load_file(&path.to_string_lossy(), ctx);
                        }
                    }

                    // ── Save ──────────────────────────────────────────────────
                    let save_clicked = toolbar_action_button(
                        ui, egui::include_image!("../assets/floppy-disk.svg"),
                        "Save", icon_tint, lbl_color,
                    );
                    if save_clicked {
                        if let Some(save_path) = rfd::FileDialog::new()
                            .add_filter("PDF", &["pdf"]).save_file()
                        {
                            if let Some(tab) = self.active_tab_ref() {
                                let tab_id = tab.id;
                                let annots = tab.annotation_layers_cloned();
                                self.save_annotations_json(tab);
                                let save_path_str = save_path.to_string_lossy().to_string();
                                let native_bms = Self::native_bookmarks_for_file(
                                    &self.bookmarks.root, &tab.file_path,
                                );
                                let _ = self.worker_tx.send(WorkerRequest::Save(
                                    tab_id,
                                    save_path_str,
                                    annots,
                                    native_bms,
                                ));
                                self.show_toast("Saving…", ctx);
                            }
                        }
                    }

                    // ── New Page ──────────────────────────────────────────────
                    let new_page_clicked = toolbar_action_button(
                        ui, egui::include_image!("../assets/file-plus.svg"),
                        "New Page", icon_tint, lbl_color,
                    );
                    if new_page_clicked {
                        if let Some(tab_id) = self.active_tab_id {
                            let _ = self.worker_tx.send(WorkerRequest::AddBlankPage(tab_id));
                            self.show_toast("Adding blank page…", ctx);
                        }
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // ── Page navigation ───────────────────────────────────────
                    if let Some(tab) = self.active_tab() {
                        let total = tab.page_count();
                        if total > 0 {
                            if ui.add_sized([28.0, 36.0], Button::new(
                                RichText::new("◀").color(lbl_color)
                            )).on_hover_text("Previous page").clicked() {
                                if tab.current_page > 0 { tab.current_page -= 1; }
                            }
                            let input_id = ui.make_persistent_id("page_input");
                            let mut page_str = ui.ctx().data(|d| d.get_temp::<String>(input_id)).unwrap_or_else(|| format!("{}", tab.current_page + 1));
                            
                            if !ui.ctx().memory(|m| m.has_focus(input_id)) {
                                page_str = format!("{}", tab.current_page + 1);
                            }
                            
                            let text_edit = TextEdit::singleline(&mut page_str)
                                .id(input_id)
                                .vertical_align(egui::Align::Center)
                                .horizontal_align(egui::Align::Center)
                                .margin(egui::vec2(6.0, 4.0));
                            
                            if ui.add_sized([35.0, 36.0], text_edit).changed() {
                                ui.ctx().data_mut(|d| d.insert_temp(input_id, page_str.clone()));
                                if let Ok(parsed) = page_str.parse::<usize>() {
                                    if parsed > 0 && parsed <= total {
                                        tab.current_page = parsed - 1;
                                    }
                                }
                            }
                            
                            ui.add_sized([30.0, 36.0], Label::new(RichText::new(
                                format!(" / {}", total)
                            ).size(14.0).color(lbl_color)));
                            if ui.add_sized([28.0, 36.0], Button::new(
                                RichText::new("▶").color(lbl_color)
                            )).on_hover_text("Next page").clicked() {
                                if tab.current_page + 1 < total { tab.current_page += 1; }
                            }
                        }
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    // ── Search ────────────────────────────────────────────────
                    ui.label(RichText::new("🔍").color(lbl_color));
                    let mut do_search       = false;
                    let mut clear_search    = false;
                    let mut result_nav_prev = false;
                    let mut result_nav_next = false;

                    if let Some(tab) = self.active_tab() {
                        let search_resp = ui.add_sized([130.0, 32.0],
                            TextEdit::singleline(&mut tab.search_query)
                                .hint_text("Search…")
                                .vertical_align(egui::Align::Center)
                                .margin(egui::vec2(8.0, 4.0))
                        );
                        let enter = search_resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter));
                        do_search = ui.add_sized([60.0, 32.0], Button::new("Search")).clicked() || enter;

                        if !tab.search_results.is_empty() {
                            let badge = format!("{}/{}", tab.search_current_idx + 1, tab.search_results.len());
                            ui.label(RichText::new(badge).size(11.0).color(Color32::from_rgb(49, 130, 206)).strong());
                            ui.label(RichText::new(format!("({} matches)", tab.search_match_count)).size(10.0).color(Color32::GRAY));
                            if ui.add_sized([22.0, 28.0], Button::new("‹")).on_hover_text("Previous").clicked() { result_nav_prev = true; }
                            if ui.add_sized([22.0, 28.0], Button::new("›")).on_hover_text("Next").clicked()     { result_nav_next = true; }
                            if ui.add_sized([22.0, 28.0], Button::new("✕")).on_hover_text("Clear").clicked()    { clear_search = true; }
                        }
                    }

                    if do_search {
                        if let Some(tab) = self.active_tab() {
                            let q = tab.search_query.clone();
                            let hits = tab.doc.find_text(&q);
                            tab.search_rects = vec![Vec::new(); tab.page_count()];
                            tab.search_results.clear();
                            for (page, rect) in hits {
                                if page < tab.search_rects.len() {
                                    if !tab.search_results.contains(&page) {
                                        tab.search_results.push(page);
                                    }
                                    tab.search_rects[page].push(rect);
                                }
                            }
                            tab.search_results.sort_unstable();
                            tab.search_match_count = tab.search_rects.iter().map(|v| v.len()).sum();
                            tab.search_current_idx = 0;
                            if let Some(&first_page) = tab.search_results.first() {
                                tab.current_page = first_page;
                            }
                        }
                    }
                    if result_nav_prev {
                        if let Some(tab) = self.active_tab() {
                            if tab.search_current_idx == 0 { tab.search_current_idx = tab.search_results.len().saturating_sub(1); }
                            else { tab.search_current_idx -= 1; }
                            tab.current_page = tab.search_results[tab.search_current_idx];
                        }
                    }
                    if result_nav_next {
                        if let Some(tab) = self.active_tab() {
                            tab.search_current_idx = (tab.search_current_idx + 1) % tab.search_results.len();
                            tab.current_page = tab.search_results[tab.search_current_idx];
                        }
                    }
                    if clear_search {
                        if let Some(tab) = self.active_tab() {
                            tab.search_query.clear();
                            tab.search_results.clear();
                            tab.search_rects.clear();
                            tab.search_match_count = 0;
                            tab.search_current_idx = 0;
                        }
                    }

                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(8.0);

                    let tx = self.worker_tx.clone();
                    // ── Zoom ──────────────────────────────────────────────────
                    if ui.add_sized([28.0, 32.0], Button::new(RichText::new("−").color(lbl_color))).on_hover_text("Zoom out").clicked() {
                        if let Some(tab) = self.active_tab() { let s = tab.scale; tab.set_scale(s / 1.2, Some(&tx)); }
                    }
                    let scale_val = self.active_tab_ref().map(|t| t.scale).unwrap_or(1.0);
                    if ui.add_sized([52.0, 32.0], Button::new(RichText::new(format!("{:.0}%", scale_val * 100.0)).size(13.0).color(lbl_color)))
                        .on_hover_text("Click to reset zoom").clicked()
                    {
                        if let Some(tab) = self.active_tab() { tab.set_scale(1.0, Some(&tx)); }
                    }
                    if ui.add_sized([28.0, 32.0], Button::new(RichText::new("+").color(lbl_color))).on_hover_text("Zoom in").clicked() {
                        if let Some(tab) = self.active_tab() { let s = tab.scale; tab.set_scale(s * 1.2, Some(&tx)); }
                    }
                    if ui.add_sized([46.0, 32.0], Button::new(RichText::new("⊡ Fit").color(lbl_color))).on_hover_text("Fit page").clicked() {
                        let avail = ctx.screen_rect().width() - 70.0 - 145.0 - 40.0;
                        let tx = self.worker_tx.clone();
                        if let Some(tab) = self.active_tab() {
                            let curr = tab.current_page;
                            if curr < tab.page_sizes.len() {
                                let (pw, _) = tab.page_sizes[curr];
                                let divisor = if tab.two_page_view { 2.1 } else { 1.0 };
                                if pw > 0.0 { tab.set_scale(avail / pw / divisor, Some(&tx)); }
                            }
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);

                        // Settings gear
                        let settings_clicked = toolbar_action_button(
                            ui, egui::include_image!("../assets/gear-six.svg"),
                            "Settings", icon_tint, lbl_color,
                        );
                        if settings_clicked {
                            self.tool_state.settings_open = !self.tool_state.settings_open;
                        }

                        ui.add_space(8.0);

                        // Bookmarks toggle
                        let b_btn = if self.show_bookmarks {
                            Button::new(RichText::new("🔖 Bookmarks").color(Color32::WHITE))
                                .fill(Color32::from_rgb(49, 130, 206))
                        } else {
                            Button::new(RichText::new("🔖 Bookmarks").color(lbl_color))
                        };
                        if ui.add_sized([110.0, 36.0], b_btn).on_hover_text("Toggle Bookmarks").clicked() {
                            self.show_bookmarks = !self.show_bookmarks;
                        }

                        ui.add_space(8.0);

                        // 2-Page toggle
                        let two_page_active = self.active_tab_ref().map(|t| t.two_page_view).unwrap_or(false);
                        let btn = if two_page_active {
                            Button::new(RichText::new("📖 2-Page").color(Color32::WHITE))
                                .fill(Color32::from_rgb(49, 130, 206))
                        } else {
                            Button::new(RichText::new("📖 2-Page").color(lbl_color))
                        };
                        if ui.add_sized([100.0, 36.0], btn).on_hover_text("Toggle 2-Page").clicked() {
                            if let Some(tab) = self.active_tab() {
                                tab.two_page_view = !tab.two_page_view;
                                if tab.two_page_view && tab.current_page % 2 == 1 {
                                    tab.current_page = tab.current_page.saturating_sub(1);
                                }
                                for (_, cache) in tab.page_textures.drain() {
                                    tab.garbage_textures.push(cache.texture);
                                }
                                tab.pending_renders.clear();
                            }
                        }
                        
                        ui.add_space(12.0);


                    });
                });
                });
                ui.add_space(4.0);
            });

        // ── Arrow-key / Ctrl+2 shortcuts (when no text field focused) ─────────
        if !ctx.memory(|m| m.focused().is_some()) {
            ctx.input(|i| {
                if i.modifiers.ctrl && i.key_pressed(Key::Num2) {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                        tab.two_page_view = !tab.two_page_view;
                        if tab.two_page_view && tab.current_page % 2 == 1 {
                            tab.current_page = tab.current_page.saturating_sub(1);
                        }
                        for (_, cache) in tab.page_textures.drain() {
                            tab.garbage_textures.push(cache.texture);
                        }
                        tab.pending_renders.clear();
                    }
                }
                if i.key_pressed(Key::ArrowLeft) {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                        if tab.current_page > 0 { tab.current_page -= 1; }
                    }
                }
                if i.key_pressed(Key::ArrowRight) {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                        if tab.current_page + 1 < tab.page_count() { tab.current_page += 1; }
                    }
                }
            });
        }

        // ── Bookmarks Panel ───────────────────────────────────────────────────
        if self.show_bookmarks {
            SidePanel::left("bookmarks_panel")
                .frame(egui::Frame::none().fill(ctx.style().visuals.panel_fill).stroke(egui::Stroke::NONE))
                .resizable(true)
                .min_width(180.0)
                .max_width(350.0)
                .show(ctx, |ui| {
                    // ── PDF internal outline (TOC) ────────────────────────────
                    ui.label(RichText::new("📑 Document Outline").strong().size(16.0));
                    ui.separator();

                    let outline = self.active_tab_ref()
                        .map(|t| t.pdf_outline.clone())
                        .unwrap_or_default();

                    if outline.is_empty() {
                        ui.allocate_ui(egui::vec2(ui.available_width(), ui.available_height() * 0.55), |ui| {
                            ui.centered_and_justified(|ui| {
                                ui.label(RichText::new("No document outline found").color(Color32::from_gray(120)));
                            });
                        });
                    } else {
                        let mut goto_page: Option<usize> = None;
                        ScrollArea::vertical()
                            .id_salt("pdf_outline_scroll")
                            .max_height(ui.available_height() * 0.55)
                            .show(ui, |ui| {
                                Self::draw_pdf_outline(ui, &outline, &mut goto_page, 0);
                            });

                        if let Some(page) = goto_page {
                            if let Some(tab) = self.active_tab() {
                                if page < tab.page_count() {
                                    tab.current_page = page;
                                }
                            }
                        }
                    }

                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);

                    // ── User bookmarks ────────────────────────────────────────
                    ui.label(RichText::new("🔖 Bookmarks").strong().size(16.0));
                    ui.separator();

                    ui.horizontal(|ui| {
                        // ── "Add Current Document" — bookmarks the file only ──
                        if ui.button("➕ Add Document")
                            .on_hover_text("Bookmark this document (no page saved)")
                            .clicked()
                        {
                            if let Some(tab) = self.active_tab_ref() {
                                let title = std::path::Path::new(&tab.file_path)
                                    .file_name().unwrap_or_default()
                                    .to_string_lossy().to_string();
                                let b = Bookmark::Link(LinkData {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    title,
                                    url: tab.file_path.clone(),
                                    page: None,
                                    icon: None,
                                    tags: Vec::new(),
                                    date_added: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap().as_secs(),
                                });
                                self.bookmarks.add_bookmark(None, b);
                                let _ = self.bookmarks.save_to_disk(&self.bookmarks_path);
                            }
                        }

                        // ── "Bookmark Page" — bookmarks file + current page ───
                        if ui.button("📌 Bookmark Page")
                            .on_hover_text("Bookmark this document at the current page")
                            .clicked()
                        {
                            if let Some(tab) = self.active_tab_ref() {
                                let current_page = tab.current_page;
                                let title = format!(
                                    "{} (p.{})",
                                    std::path::Path::new(&tab.file_path)
                                        .file_name().unwrap_or_default()
                                        .to_string_lossy(),
                                    current_page + 1,
                                );
                                let b = Bookmark::Link(LinkData {
                                    id: uuid::Uuid::new_v4().to_string(),
                                    title,
                                    url: tab.file_path.clone(),
                                    page: Some(current_page),
                                    icon: None,
                                    tags: Vec::new(),
                                    date_added: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap().as_secs(),
                                });
                                self.bookmarks.add_bookmark(None, b);
                                let _ = self.bookmarks.save_to_disk(&self.bookmarks_path);
                            }
                        }
                    });

                    ui.separator();

                    let mut action = BookmarkAction::None;
                    let bm_root = self.bookmarks.root.clone();
                    ScrollArea::vertical()
                        .id_salt("user_bookmarks_scroll")
                        .show(ui, |ui| {
                            action = Self::draw_bookmark_tree(ui, &bm_root);
                        });

                    match action {
                        BookmarkAction::Load(url, maybe_page) => {
                            // If the file is already open in a tab, just switch to it
                            // (and jump to the page if requested).
                            let already_open = self.tabs
                                .iter()
                                .find(|t| t.file_path == url)
                                .map(|t| t.id);

                            if let Some(tid) = already_open {
                                self.active_tab_id = Some(tid);
                                if let Some(pg) = maybe_page {
                                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tid) {
                                        if pg < tab.page_count() {
                                            tab.current_page = pg;
                                        }
                                    }
                                }
                            } else {
                                // Record the desired jump so WorkerResponse::Loaded can apply it.
                                if let Some(pg) = maybe_page {
                                    self.pending_page_jump = Some((url.clone(), pg));
                                }
                                self.load_file(&url, ctx);
                            }
                        }
                        BookmarkAction::GoToPage(page) => {
                            if let Some(tab) = self.active_tab() {
                                if page < tab.page_count() {
                                    tab.current_page = page;
                                }
                            }
                        }
                        BookmarkAction::Delete(id) => {
                            self.bookmarks.delete_bookmark(&id);
                            let _ = self.bookmarks.save_to_disk(&self.bookmarks_path);
                        }
                        BookmarkAction::None => {}
                    }
                });
        }

        // ── Left toolbar ──────────────────────────────────────────────────────
        SidePanel::left("toolbar")
            .frame(egui::Frame::none().fill(ctx.style().visuals.panel_fill).stroke(egui::Stroke::NONE))
            .exact_width(72.0)
            .show(ctx, |ui| {
            draw_toolbar(ui, &mut self.tool_state);
        });

        // ── Right thumbnails ──────────────────────────────────────────────────
        SidePanel::right("thumbnails")
            .frame(egui::Frame::none().fill(ctx.style().visuals.panel_fill).stroke(egui::Stroke::NONE))
            .resizable(true)
            .min_width(130.0)
            .max_width(200.0)
            .show(ctx, |ui| {
                if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                    let total = tab.page_count();
                    if total == 0 {
                        ui.centered_and_justified(|ui| { ui.label("No pages"); });
                    } else {
                        let thumb_h = 140.0_f32;
                        ScrollArea::vertical()
                            .id_salt("thumb_scroll")
                            .show_rows(ui, thumb_h, total, |ui, range| {
                                let center = (range.start + range.end) / 2;
                                tab.manage_thumb_cache(center, &self.worker_tx);

                                for i in range {
                                    let is_search_hit = !tab.search_query.trim().is_empty()
                                        && tab.search_results.contains(&i);
                                    let is_active = i == tab.current_page
                                        || (tab.two_page_view && Some(i) == tab.right_page());

                                    let frame_col = if is_active {
                                        Color32::from_rgb(49, 130, 206)
                                    } else if is_search_hit {
                                        Color32::from_rgb(220, 170, 0)
                                    } else {
                                        Color32::TRANSPARENT
                                    };

                                    if let Some(tex) = tab.thumb_textures.get(&i) {
                                        Frame::none()
                                            .stroke(Stroke::new(2.0, frame_col))
                                            .inner_margin(2.0)
                                            .show(ui, |ui| {
                                                let r = ui.add(Image::new(tex).max_width(110.0).sense(Sense::click()));
                                                if is_search_hit {
                                                    let br = Rect::from_center_size(
                                                        r.rect.right_top() + Vec2::new(-8.0, 8.0),
                                                        Vec2::splat(16.0));
                                                    ui.painter().circle_filled(br.center(), 8.0, Color32::from_rgb(220, 170, 0));
                                                    ui.painter().text(br.center(), Align2::CENTER_CENTER, "🔍",
                                                        FontId::proportional(10.0), Color32::WHITE);
                                                }
                                                if r.clicked() {
                                                    tab.current_page = if tab.two_page_view { i & !1 } else { i };
                                                }
                                            });
                                    } else {
                                        let dummy = Rect::from_min_size(ui.cursor().min, Vec2::new(110.0, 140.0));
                                        ui.allocate_rect(dummy, Sense::hover());
                                        ui.painter().rect_filled(dummy, 2.0,
                                            Color32::from_gray(if ui.visuals().dark_mode { 40 } else { 220 }));
                                        ui.painter().text(dummy.center(), Align2::CENTER_CENTER, "Loading…",
                                            FontId::proportional(12.0), Color32::GRAY);
                                    }
                                    ui.add_space(8.0);
                                }
                            });
                    }
                } else {
                    ui.centered_and_justified(|ui| { ui.label("No active tab"); });
                }
            });

        // ── Central canvas ────────────────────────────────────────────────────
        CentralPanel::default()
            .frame(egui::Frame::none().fill(ctx.style().visuals.panel_fill))
            .show(ctx, |ui| {
            if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                tab.manage_page_cache(&self.worker_tx);
            }

            if self.active_tab_id.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("No document open").size(24.0).color(Color32::DARK_GRAY));
                });
                return;
            }

            let mut commit_note = None;
            let mut close_note  = false;
            let mut commit_text = None;
            let mut close_text  = false;

            ScrollArea::both()
                .auto_shrink([false, false])
                .id_salt("main_scroll")
                .show(ui, |ui| {
                    let mut total_w = 0.0;
                    if let Some(tab) = self.active_tab_ref() {
                        let cur   = tab.current_page;
                        let right = tab.right_page();
                        let gap   = crate::tab::TWO_PAGE_GAP * tab.scale;
                        if cur < tab.page_sizes.len() {
                            let (w, _) = tab.page_sizes[cur];
                            total_w += w * tab.scale;
                        }
                        if let Some(r) = right {
                            if r < tab.page_sizes.len() {
                                let (w, _) = tab.page_sizes[r];
                                total_w += w * tab.scale + gap;
                            }
                        }
                    }

                    let avail_w = ui.available_width();
                    let padding = if avail_w > total_w { (avail_w - total_w) / 2.0 } else { 0.0 };

                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if padding > 0.0 { ui.add_space(padding); }

                            let (cur, right, gap) = if let Some(tab) = self.active_tab_ref() {
                                (tab.current_page, tab.right_page(), crate::tab::TWO_PAGE_GAP * tab.scale)
                            } else {
                                (0, None, 0.0)
                            };

                            if right.is_some() {
                                if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                                    draw_tab_canvas(tab, &mut self.tool_state, &mut self.note_modal, &mut self.text_modal, ui, cur, Id::new(cur), ctx);
                                }
                                ui.add_space(gap);
                                if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                                    draw_tab_canvas(tab, &mut self.tool_state, &mut self.note_modal, &mut self.text_modal, ui, cur + 1, Id::new(cur + 1), ctx);
                                }
                            } else if let Some(tab) = self.tabs.iter_mut().find(|t| Some(t.id) == self.active_tab_id) {
                                draw_tab_canvas(tab, &mut self.tool_state, &mut self.note_modal, &mut self.text_modal, ui, cur, Id::new(cur), ctx);
                            }
                        });
                        ui.add_space(8.0);
                    });

                    // Modals
                    if self.note_modal.open {
                        egui::Window::new("Enter Note").collapsible(false).resizable(false)
                            .show(ctx, |ui| {
                                ui.text_edit_multiline(&mut self.note_modal.text);
                                ui.horizontal(|ui| {
                                    if ui.button("Save").clicked()   { commit_note = Some(self.note_modal.text.clone()); close_note = true; }
                                    if ui.button("Cancel").clicked() { close_note = true; }
                                });
                            });
                    }
                    if self.text_modal.open {
                        egui::Window::new("Text Options").collapsible(false).resizable(false)
                            .show(ctx, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label("Size:");
                                    ui.add(egui::Slider::new(&mut self.text_modal.font_size, 8.0..=72.0));
                                });
                                let res = ui.text_edit_multiline(&mut self.text_modal.text);
                                res.request_focus();
                                ui.horizontal(|ui| {
                                    if ui.button("Place Text").clicked() { commit_text = Some((self.text_modal.text.clone(), self.text_modal.font_size)); close_text = true; }
                                    if ui.button("Cancel").clicked()     { close_text = true; }
                                });
                            });
                    }
                });

            if close_note { self.note_modal.open = false; }
            if close_text { self.text_modal.open = false; }

            // Commit note
            if let Some(text) = commit_note {
                let tab_id = self.note_modal.tab_id;
                if let Some(id) = tab_id {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.doc.add_annot(self.note_modal.page,
                            AnnotationShape::Note(StickyNote {
                                pos: [self.note_modal.pos.x, self.note_modal.pos.y],
                                text,
                            }));
                    }
                }
                self.note_modal.text.clear();
            }

            // Commit text box
            if let Some((text, font_size)) = commit_text {
                if !text.trim().is_empty() {
                    let tab_id = self.text_modal.tab_id;
                    if let Some(id) = tab_id {
                        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                            tab.doc.add_annot(self.text_modal.page,
                                AnnotationShape::TextBox(TextBox {
                                    pos: [self.text_modal.pos.x, self.text_modal.pos.y],
                                    text, font_size,
                                    color: color32_to_arr(self.tool_state.color),
                                }));
                        }
                    }
                }
                self.text_modal.text.clear();
            }
        });

        // Reset undo checkpoint flag at end of frame
        for tab in &mut self.tabs {
            tab.needs_undo_checkpoint = false;
        }

        // ── Toast ─────────────────────────────────────────────────────────────
        if let Some(toast) = &self.toast {
            if toast.is_alive(ctx) {
                egui::Window::new("Toast")
                    .title_bar(false)
                    .resizable(false)
                    .anchor(Align2::CENTER_BOTTOM, [0.0, -40.0])
                    .show(ctx, |ui| {
                        Frame::none()
                            .fill(Color32::from_black_alpha(200))
                            .rounding(4.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.label(RichText::new(&toast.text).color(Color32::WHITE));
                            });
                    });
            } else {
                self.toast = None;
            }
        }

        // ── Conversion modal ──────────────────────────────────────────────────
        if let ConversionState::Converting(path) = &self.conversion_state {
            egui::Window::new("Document Conversion")
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .collapsible(false)
                .resizable(false)
                .auto_sized()
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(10.0);
                        ui.add(egui::widgets::Spinner::new().size(32.0));
                        ui.add_space(16.0);
                        ui.label(RichText::new("Converting to PDF…").strong().size(16.0));
                        ui.add_space(4.0);
                        ui.label(RichText::new(format!("File: {}", path)).weak());
                        ui.add_space(10.0);
                        ui.label("This may take a few seconds.");
                        ui.add_space(10.0);
                    });
                });
        }



        crate::ui::draw_shortcut_settings(ctx, &mut self.tool_state);
    }
}

impl Drop for IkraApp {
    fn drop(&mut self) {
        let _ = self.tool_state.save_to_disk(&self.settings_path);
    }
}
