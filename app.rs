use egui::*;
use std::collections::HashMap;

use crate::annotations::*;
use crate::pdf_engine::PdfEngine;
use crate::ui::{draw_toolbar, handle_shortcuts, Tool, ToolState, ToastMessage};

// Lazy-loading: keep at most this many full-res textures around the current page.
const PAGE_CACHE_RADIUS: usize = 1;
// Thumb cache: keep a smaller radius of thumbs for memory efficiency.
const THUMB_CACHE_RADIUS: usize = 5;

fn read_pressure(input: &egui::InputState) -> f32 {
    for event in &input.events {
        if let Event::Touch { force: Some(f), .. } = event {
            return f.clamp(0.01, 1.0);
        }
    }
    0.5
}

fn ellipse_points(center: Pos2, rx: f32, ry: f32, segments: usize) -> Vec<Pos2> {
    (0..segments).map(|i| {
        let a = i as f32 * std::f32::consts::TAU / segments as f32;
        Pos2::new(center.x + rx * a.cos(), center.y + ry * a.sin())
    }).collect()
}

struct PageCache { texture: TextureHandle, rendered_scale: f32 }
struct ActiveStroke { page: usize, points: Vec<StrokePoint> }
struct ActiveShape  { page: usize, start: Pos2, end: Pos2 }

pub struct NoteModal  { pub open: bool, pub text: String, pub page: usize, pub pos: Pos2 }
pub struct TextModal  { pub open: bool, pub text: String, pub page: usize, pub pos: Pos2, pub font_size: f32 }
struct NoteViewer     { page: usize, annot_index: usize }

pub struct IkraApp {
    engine:        Option<PdfEngine>,
    page_textures: HashMap<usize, PageCache>,
    thumb_textures: HashMap<usize, TextureHandle>,
    scale:         f32,
    current_page:  usize,
    annots:        AnnotationState,
    active_stroke: Option<ActiveStroke>,
    active_shape:  Option<ActiveShape>,
    tool_state:    ToolState,
    toast:         Option<ToastMessage>,
    note_modal:    NoteModal,
    text_modal:    TextModal,
    note_viewer:   Option<NoteViewer>,
    search_query:  String,
    search_results: Vec<usize>,
    /// Approximate index of the first thumb visible in the panel (for lazy thumb loading).
    thumb_viewport_top: usize,
}

impl IkraApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            engine: PdfEngine::new().ok(),
            page_textures: HashMap::new(),
            thumb_textures: HashMap::new(),
            scale: 1.4,
            current_page: 0,
            annots: AnnotationState::new(0),
            active_stroke: None,
            active_shape: None,
            tool_state: ToolState::default(),
            toast: None,
            note_modal: NoteModal { open: false, text: String::new(), page: 0, pos: Pos2::ZERO },
            text_modal: TextModal { open: false, text: String::new(), page: 0, pos: Pos2::ZERO, font_size: 16.0 },
            note_viewer: None,
            search_query: String::new(),
            search_results: Vec::new(),
            thumb_viewport_top: 0,
        }
    }

    fn show_toast(&mut self, text: impl Into<String>, ctx: &Context) {
        self.toast = Some(ToastMessage::new(text, ctx));
    }

    fn page_count(&self) -> usize {
        self.engine.as_ref().map(|e| e.page_count).unwrap_or(0)
    }

    fn pdf_name(&self) -> String {
        self.engine.as_ref()
            .and_then(|e| e.current_path.as_deref())
            .and_then(|p| std::path::Path::new(p).file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("No file open")
            .to_string()
    }

    fn load_file(&mut self, path: &str, ctx: &Context) {
        if let Some(engine) = &mut self.engine {
            match engine.open(path) {
                Err(e) => self.show_toast(format!("Failed to open: {}", e), ctx),
                Ok(_) => {
                    self.page_textures.clear();
                    self.thumb_textures.clear();
                    self.annots = AnnotationState::new(engine.page_count);
                    self.current_page = 0;
                    self.thumb_viewport_top = 0;
                    self.show_toast("PDF loaded!", ctx);
                }
            }
        }
    }

    /// Change zoom scale, clear stale textures for full-res pages only.
    fn set_scale(&mut self, new_scale: f32) {
        let clamped = new_scale.clamp(0.1, 8.0);
        if (clamped - self.scale).abs() > 0.005 {
            self.scale = clamped;
            self.page_textures.clear(); // must re-render at new scale
        }
    }

    fn screen_to_pdf(&self, screen: Pos2, origin: Pos2) -> Pos2 {
        Pos2::new((screen.x - origin.x) / self.scale, (screen.y - origin.y) / self.scale)
    }

    fn pdf_to_screen(&self, pdf: Pos2, origin: Pos2) -> Pos2 {
        Pos2::new(origin.x + pdf.x * self.scale, origin.y + pdf.y * self.scale)
    }

    /// Ensure the current page (and ±radius neighbours) are rendered; evict the rest.
    fn manage_page_cache(&mut self, ctx: &Context) {
        let page      = self.current_page;
        let total     = self.page_count();
        let lo        = page.saturating_sub(PAGE_CACHE_RADIUS);
        let hi        = (page + PAGE_CACHE_RADIUS).min(total.saturating_sub(1));

        // Evict pages outside window
        self.page_textures.retain(|k, _| *k >= lo && *k <= hi);

        // Render missing pages in window (only current page is strictly needed; neighbours optional)
        if let Some(engine) = &self.engine {
            for p in lo..=hi {
                if self.page_textures.contains_key(&p) { continue; }
                if let Some(img) = engine.render_page(p, self.scale) {
                    let size = [img.width() as _, img.height() as _];
                    let rgba = img.into_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice());
                    drop(rgba); // free buffer immediately
                    let tex = ctx.load_texture(format!("page_{p}"), ci, TextureOptions::LINEAR);
                    self.page_textures.insert(p, PageCache { texture: tex, rendered_scale: self.scale });
                }
            }
        }
    }

    /// Ensure thumbnails near `center` are rendered; evict distant ones.
    /// Called once per frame to maintain the thumbnail sliding window.
    fn manage_thumb_cache(&mut self, ctx: &Context, center: usize) {
        let total = self.page_count();
        if total == 0 { return; }

        let lo = center.saturating_sub(THUMB_CACHE_RADIUS);
        let hi = (center + THUMB_CACHE_RADIUS).min(total.saturating_sub(1));

        // Evict thumbnails that are far from the current visible range
        self.thumb_textures.retain(|k, _| *k >= lo && *k <= hi);

        if let Some(engine) = &self.engine {
            // Only render missing ones. We limit rendering to a few per frame if needed,
            // but for now, we render whatever is in the radius.
            for p in lo..=hi {
                if self.thumb_textures.contains_key(&p) {
                    continue;
                }
                if let Some(img) = engine.render_thumb(p) {
                    let size = [img.width() as _, img.height() as _];
                    let rgba = img.into_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice());
                    // Drop rgba early to save some peaks
                    drop(rgba);
                    let tex = ctx.load_texture(format!("thumb_{p}"), ci, TextureOptions::LINEAR);
                    self.thumb_textures.insert(p, tex);
                }
            }
        }
    }

    fn draw_annotations(&self, painter: &Painter, page: usize, origin: Pos2) {
        if page >= self.annots.pages.len() { return; }
        for (idx, annot) in self.annots.pages[page].items.iter().enumerate() {
            match annot {
                Annot::Pen(s) => {
                    if s.points.len() < 2 { continue; }
                    let pts: Vec<Pos2> = s.points.iter()
                        .map(|p| self.pdf_to_screen(p.to_pos2(), origin)).collect();
                    match s.kind {
                        AnnotKind::Highlight => {
                            let c = arr_to_color32(s.color);
                            let hc = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(),
                                (self.tool_state.opacity * 255.0) as u8);
                            painter.add(Shape::Path(egui::epaint::PathShape {
                                points: pts, closed: false, fill: Color32::TRANSPARENT,
                                stroke: egui::epaint::PathStroke::new(s.width * self.scale * 8.0, hc),
                            }));
                        }
                        AnnotKind::Pen => {
                            let color = arr_to_color32(s.color);
                            for i in 1..pts.len() {
                                painter.line_segment([pts[i-1], pts[i]],
                                    Stroke::new(s.width * self.scale, color));
                            }
                        }
                        _ => {}
                    }
                }
                Annot::Shape(s) => {
                    let p1 = self.pdf_to_screen(Pos2::new(s.start[0], s.start[1]), origin);
                    let p2 = self.pdf_to_screen(Pos2::new(s.end[0],   s.end[1]),   origin);
                    let color  = arr_to_color32(s.color);
                    let stroke = Stroke::new(s.width * self.scale, color);
                    match s.kind {
                        ShapeKind::Rect => {
                            painter.rect_stroke(Rect::from_two_pos(p1, p2), 0.0, stroke);
                        }
                        ShapeKind::Ellipse => {
                            let c  = Pos2::new((p1.x+p2.x)/2.0, (p1.y+p2.y)/2.0);
                            let rx = (p2.x-p1.x).abs()/2.0;
                            let ry = (p2.y-p1.y).abs()/2.0;
                            painter.add(Shape::Path(egui::epaint::PathShape {
                                points: ellipse_points(c, rx, ry, 48),
                                closed: true, fill: Color32::TRANSPARENT,
                                stroke: egui::epaint::PathStroke::new(s.width*self.scale, color),
                            }));
                        }
                        ShapeKind::Arrow => {
                            painter.line_segment([p1, p2], stroke);
                            let dir  = (p2-p1).normalized();
                            let perp = Vec2::new(-dir.y, dir.x);
                            let hl = 12.0*self.scale; let hw = 6.0*self.scale;
                            painter.line_segment([p2, p2 - dir*hl + perp*hw], stroke);
                            painter.line_segment([p2, p2 - dir*hl - perp*hw], stroke);
                        }
                        ShapeKind::Line => { painter.line_segment([p1, p2], stroke); }
                    }
                }
                Annot::TextBox(t) => {
                    let pos   = self.pdf_to_screen(Pos2::new(t.pos[0], t.pos[1]), origin);
                    let color = arr_to_color32(t.color);
                    painter.text(pos, Align2::LEFT_TOP, &t.text,
                        FontId::proportional(t.font_size * self.scale), color);
                }
                Annot::Note(n) => {
                    let pos = self.pdf_to_screen(Pos2::new(n.pos[0], n.pos[1]), origin);
                    painter.circle_filled(pos, 10.0 * self.scale.sqrt(),
                        Color32::from_rgb(255, 220, 50));
                    painter.text(pos, Align2::CENTER_CENTER, "📝",
                        FontId::proportional(14.0), Color32::BLACK);
                    if let Some(v) = &self.note_viewer {
                        if v.page == page && v.annot_index == idx {
                            let pp = pos + Vec2::new(15.0, -10.0);
                            let tr = Rect::from_min_size(pp, Vec2::new(200.0, 80.0));
                            painter.rect_filled(tr, 4.0, Color32::from_rgb(255, 250, 200));
                            painter.rect_stroke(tr, 4.0,
                                Stroke::new(1.0, Color32::from_rgb(180, 160, 0)));
                            painter.text(pp + Vec2::new(6.0, 6.0), Align2::LEFT_TOP,
                                &n.text, FontId::proportional(12.0), Color32::BLACK);
                        }
                    }
                }
            }
        }
    }

    fn draw_active_shape_preview(&self, painter: &Painter, origin: Pos2) {
        let Some(shape) = &self.active_shape else { return };
        let p1     = self.pdf_to_screen(shape.start, origin);
        let p2     = self.pdf_to_screen(shape.end,   origin);
        let color  = self.tool_state.color;
        let stroke = Stroke::new(self.tool_state.brush_size * self.scale, color);
        match self.tool_state.tool {
            Tool::Rect    => { painter.rect_stroke(Rect::from_two_pos(p1, p2), 0.0, stroke); }
            Tool::Ellipse => {
                let c  = Pos2::new((p1.x+p2.x)/2.0, (p1.y+p2.y)/2.0);
                let rx = (p2.x-p1.x).abs()/2.0; let ry = (p2.y-p1.y).abs()/2.0;
                painter.add(Shape::Path(egui::epaint::PathShape {
                    points: ellipse_points(c, rx, ry, 48), closed: true,
                    fill: Color32::TRANSPARENT,
                    stroke: egui::epaint::PathStroke::new(self.tool_state.brush_size*self.scale, color),
                }));
            }
            Tool::Arrow => {
                painter.line_segment([p1, p2], stroke);
                let dir  = (p2-p1).normalized();
                let perp = Vec2::new(-dir.y, dir.x);
                let hl = 12.0*self.scale; let hw = 6.0*self.scale;
                painter.line_segment([p2, p2 - dir*hl + perp*hw], stroke);
                painter.line_segment([p2, p2 - dir*hl - perp*hw], stroke);
            }
            Tool::Line => { painter.line_segment([p1, p2], stroke); }
            _ => {}
        }
    }
}

impl eframe::App for IkraApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {

        // ── Handle Ctrl+Scroll zoom (read before panels consume scroll) ────────
        let (scroll_delta, ctrl) = ctx.input(|i| (i.raw_scroll_delta, i.modifiers.ctrl));
        if ctrl && scroll_delta.y.abs() > 0.5 {
            let new_scale = if scroll_delta.y > 0.0 {
                self.scale * 1.1
            } else {
                self.scale / 1.1
            };
            self.set_scale(new_scale);
        }

        // ── Keyboard shortcuts ────────────────────────────────────────────────────
        if let Some(action) = handle_shortcuts(&mut self.tool_state, ctx) {
            if action == "save" {
                // Ctrl+S quick-save to original path, or prompt if none
                let path = self.engine.as_ref()
                    .and_then(|e| e.current_path.clone());
                if let Some(p) = path {
                    let annots = self.annots.pages.clone();
                    if let Some(engine) = &mut self.engine {
                        match engine.save_with_annotations(&p, &annots) {
                            Ok(_)  => self.show_toast("Saved!", ctx),
                            Err(e) => self.show_toast(format!("Save error: {e}"), ctx),
                        }
                    }
                } else if let Some(path) = rfd::FileDialog::new()
                    .add_filter("PDF", &["pdf"]).save_file()
                {
                    let annots = self.annots.pages.clone();
                    if let Some(engine) = &mut self.engine {
                        match engine.save_with_annotations(&path.to_string_lossy(), &annots) {
                            Ok(_)  => self.show_toast("Saved!", ctx),
                            Err(e) => self.show_toast(format!("Save error: {e}"), ctx),
                        }
                    }
                }
            }
        }

        // ── TOP BAR (tall, branded) ────────────────────────────────────────────
        TopBottomPanel::top("top_bar")
            .min_height(56.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);

                // Row 1: File actions | PDF name (centred) | Theme
                ui.horizontal(|ui| {
                    ui.add_space(4.0);

                    // Left cluster — file ops
                    if ui.add_sized([80.0, 32.0], Button::new("📂 Open")).clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("PDF", &["pdf"]).pick_file()
                        {
                            self.load_file(&path.to_string_lossy(), ctx);
                        }
                    }
                    if ui.add_sized([80.0, 32.0], Button::new("💾 Save")).clicked() {
                        if let Some(path) = rfd::FileDialog::new()
                            .add_filter("PDF", &["pdf"]).save_file()
                        {
                            let annots = self.annots.pages.clone();
                            if let Some(engine) = &mut self.engine {
                                match engine.save_with_annotations(&path.to_string_lossy(), &annots) {
                                    Ok(_)  => self.show_toast("Saved with annotations!", ctx),
                                    Err(e) => self.show_toast(format!("Save error: {e}"), ctx),
                                }
                            }
                        }
                    }
                    if ui.add_sized([110.0, 32.0], Button::new("📄 Blank Page")).clicked() {
                        let cur = self.current_page;
                        if let Some(engine) = &mut self.engine {
                            if engine.add_blank_page(cur, 595.0, 842.0).is_ok() {
                                // Shift cached textures that are at indices > cur upward by one
                                let shifted: HashMap<usize, PageCache> = self.page_textures
                                    .drain()
                                    .map(|(k, v)| (if k > cur { k + 1 } else { k }, v))
                                    .collect();
                                self.page_textures = shifted;
                                let shifted_t: HashMap<usize, TextureHandle> = self.thumb_textures
                                    .drain()
                                    .map(|(k, v)| (if k > cur { k + 1 } else { k }, v))
                                    .collect();
                                self.thumb_textures = shifted_t;
                                self.annots.insert_page(cur + 1);
                                self.current_page = cur + 1;
                                self.show_toast("Blank page inserted after current page", ctx);
                            }
                        }
                    }

                    ui.separator();

                    // Page nav
                    let total = self.page_count();
                    if total > 0 {
                        if ui.add_sized([28.0, 28.0], Button::new("◀")).clicked()
                            && self.current_page > 0
                        { self.current_page -= 1; }

                        ui.add_sized([70.0, 28.0],
                            Label::new(RichText::new(
                                format!("{} / {}", self.current_page + 1, total)
                            ).size(14.0))
                        );

                        if ui.add_sized([28.0, 28.0], Button::new("▶")).clicked()
                            && self.current_page + 1 < total
                        { self.current_page += 1; }
                    }

                    ui.separator();

                    // Search
                    ui.label("🔍");
                    ui.add(TextEdit::singleline(&mut self.search_query)
                        .desired_width(130.0)
                        .hint_text("Search…"));
                    if ui.add_sized([60.0, 28.0], Button::new("Search")).clicked() {
                        if let Some(engine) = &self.engine {
                            self.search_results = engine.search_text(&self.search_query);
                            if let Some(&first) = self.search_results.first() {
                                self.current_page = first;
                                self.show_toast(
                                    format!("Found {} result(s)", self.search_results.len()), ctx);
                            } else {
                                self.show_toast("No results found", ctx);
                            }
                        }
                    }

                    ui.separator();

                    // Zoom controls
                    if ui.add_sized([28.0, 28.0], Button::new("−"))
                        .on_hover_text("Zoom out  (Ctrl −scroll)").clicked()
                    { self.set_scale(self.scale / 1.2); }

                    // Clicking the % label resets to 100 %
                    if ui.add_sized([52.0, 28.0],
                        Button::new(RichText::new(format!("{:.0}%", self.scale * 100.0)).size(13.0)))
                        .on_hover_text("Click to reset zoom to 100%").clicked()
                    { self.set_scale(1.0); }

                    if ui.add_sized([28.0, 28.0], Button::new("+"))
                        .on_hover_text("Zoom in  (Ctrl +scroll)").clicked()
                    { self.set_scale(self.scale * 1.2); }

                    if ui.add_sized([46.0, 28.0], Button::new("⊡ Fit"))
                        .on_hover_text("Fit page width to window").clicked()
                    {
                        // Use available panel width as approximation (toolbar ~70 + thumbs ~130)
                        let avail = ctx.screen_rect().width() - 70.0 - 145.0 - 40.0;
                        if let Some(engine) = &self.engine {
                            if let Some((pw, _)) = engine.page_size(self.current_page) {
                                if pw > 0.0 { self.set_scale(avail / pw); }
                            }
                        }
                    }

                    // Centred file name (fill remaining space)
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        // Theme toggle (rightmost)
                        let is_dark = ctx.style().visuals.dark_mode;
                        let lbl = if is_dark { "🌙" } else { "☀️" };
                        if ui.add_sized([36.0, 28.0], Button::new(lbl))
                            .on_hover_text("Toggle light / dark mode").clicked()
                        {
                            if is_dark {
                                ctx.set_visuals(Visuals::dark());
                            } else {
                                let mut v = Visuals::light();
                                v.panel_fill = Color32::from_rgb(240, 242, 245);
                                ctx.set_visuals(v);
                            }
                        }

                        // File name centred in remaining space
                        ui.centered_and_justified(|ui| {
                            ui.label(RichText::new(self.pdf_name())
                                .size(14.0).strong()
                                .color(if ctx.style().visuals.dark_mode {
                                    Color32::from_rgb(200, 210, 230)
                                } else {
                                    Color32::from_rgb(40, 60, 100)
                                })
                            );
                        });
                    });
                });

                ui.add_space(4.0);
            });

        // ── LEFT TOOLBAR ──────────────────────────────────────────────────────
        SidePanel::left("toolbar").exact_width(68.0).show(ctx, |ui| {
            draw_toolbar(ui, &mut self.tool_state);
        });

        // ── RIGHT THUMBNAIL PANEL (lazy) ──────────────────────────────────────
        SidePanel::right("thumbnails")
            .resizable(true)
            .min_width(130.0)
            .max_width(200.0)
            .show(ctx, |ui| {
                let total = self.page_count();
                if total == 0 {
                    ui.centered_and_justified(|ui| { ui.label("No pages"); });
                    return;
                }

                let thumb_h = 140.0_f32; // approx height per thumbnail item

                let scroll = ScrollArea::vertical()
                    .id_salt("thumb_scroll")
                    .show_rows(ui, thumb_h, total, |ui, range| {
                        // Call cache management once for the visible range
                        let center = (range.start + range.end) / 2;
                        self.manage_thumb_cache(ctx, center);

                        for i in range {
                            if let Some(tex) = self.thumb_textures.get(&i) {
                                let is_active = i == self.current_page;
                                let frame_col = if is_active {
                                    Color32::from_rgb(49, 130, 206)
                                } else {
                                    Color32::TRANSPARENT
                                };
                                Frame::none()
                                    .stroke(Stroke::new(2.0, frame_col))
                                    .inner_margin(2.0)
                                    .show(ui, |ui| {
                                        let r = ui.add(
                                            Image::new(tex)
                                                .max_width(110.0)
                                                .sense(Sense::click())
                                        );
                                        if r.clicked() { self.current_page = i; }
                                    });
                                ui.label(RichText::new(format!("Pg {}", i + 1))
                                    .size(10.0)
                                    .color(if is_active {
                                        Color32::from_rgb(49, 130, 206)
                                    } else {
                                        Color32::GRAY
                                    }));
                            } else {
                                // Placeholder rect for pages not yet rendered
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(110.0, 120.0), Sense::click());
                                ui.painter().rect_filled(
                                    rect, 2.0, Color32::from_gray(60));
                                ui.label(RichText::new(format!("Pg {}", i + 1)).size(10.0).color(Color32::GRAY));
                            }
                            ui.add_space(4.0);
                        }
                    });

                // Update viewport top estimate (though show_rows handles most of it now)
                self.thumb_viewport_top = (scroll.state.offset.y / thumb_h) as usize;
            });

        // ── CENTRAL CANVAS ────────────────────────────────────────────────────
        CentralPanel::default().show(ctx, |ui| {

            // Keyboard navigation
            let page_count = self.page_count();
            ctx.input(|i| {
                if (i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::PageDown))
                    && self.current_page + 1 < page_count
                { self.current_page += 1; }
                if (i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp))
                    && self.current_page > 0
                { self.current_page -= 1; }
            });

            // Manage texture cache around current page
            self.manage_page_cache(ctx);

            ScrollArea::both().id_salt("canvas_scroll").show(ui, |ui| {
                if let Some(cache) = self.page_textures.get(&self.current_page) {
                    let origin   = ui.cursor().min;
                    let tex_size = cache.texture.size_vec2();
                    let rect     = Rect::from_min_size(origin, tex_size);

                    let (_, painter) = ui.allocate_painter(tex_size, Sense::click_and_drag());
                    let response     = ui.interact(rect, Id::new("pdf_canvas"), Sense::click_and_drag());

                    // PDF page image
                    painter.image(cache.texture.id(), rect,
                        Rect::from_min_max(pos2(0.0,0.0), pos2(1.0,1.0)), Color32::WHITE);

                    // Committed annotations
                    self.draw_annotations(&painter, self.current_page, origin);

                    // Live shape preview
                    self.draw_active_shape_preview(&painter, origin);

                    // ── Input ──────────────────────────────────────────────
                    if let Some(pos) = response.hover_pos() {
                        let pdf_pos  = self.screen_to_pdf(pos, origin);
                        let pressure = ctx.input(|i| read_pressure(i));

                        let is_shape = matches!(self.tool_state.tool,
                            Tool::Rect | Tool::Ellipse | Tool::Arrow | Tool::Line);
                        let is_free  = matches!(self.tool_state.tool,
                            Tool::Pen | Tool::Highlight);

                        if response.drag_started() {
                            match self.tool_state.tool {
                                Tool::Eraser => {
                                    self.annots.erase_at(self.current_page, pdf_pos, 20.0);
                                }
                                _ if is_free => {
                                    self.active_stroke = Some(ActiveStroke {
                                        page: self.current_page,
                                        points: vec![StrokePoint::new(pdf_pos, pressure)],
                                    });
                                }
                                _ if is_shape => {
                                    self.active_shape = Some(ActiveShape {
                                        page: self.current_page,
                                        start: pdf_pos, end: pdf_pos,
                                    });
                                }
                                _ => {}
                            }
                        } else if response.dragged() {
                            if let Some(s) = &mut self.active_stroke {
                                s.points.push(StrokePoint::new(pdf_pos, pressure));
                            }
                            if let Some(s) = &mut self.active_shape { s.end = pdf_pos; }
                            if self.tool_state.tool == Tool::Eraser {
                                self.annots.erase_at(self.current_page, pdf_pos, 20.0);
                            }
                        } else if response.drag_stopped() {
                            // Commit freehand
                            if let Some(stroke) = self.active_stroke.take() {
                                let kind = if self.tool_state.tool == Tool::Highlight {
                                    AnnotKind::Highlight } else { AnnotKind::Pen };
                                let color = color32_to_arr(self.tool_state.color);
                                self.annots.add_annot(self.current_page,
                                    Annot::Pen(PenStroke {
                                        kind, points: stroke.points, color,
                                        width: self.tool_state.brush_size,
                                    }));
                            }
                            // Commit shape
                            if let Some(shape) = self.active_shape.take() {
                                let kind = match self.tool_state.tool {
                                    Tool::Rect    => ShapeKind::Rect,
                                    Tool::Ellipse => ShapeKind::Ellipse,
                                    Tool::Arrow   => ShapeKind::Arrow,
                                    _             => ShapeKind::Line,
                                };
                                let color = color32_to_arr(self.tool_state.color);
                                self.annots.add_annot(self.current_page,
                                    Annot::Shape(ShapeAnnot {
                                        kind,
                                        start: [shape.start.x, shape.start.y],
                                        end:   [shape.end.x,   shape.end.y],
                                        color, width: self.tool_state.brush_size,
                                        fill: None, dashed: false,
                                    }));
                            }
                        }

                        // Click-only tools
                        if response.clicked() {
                            match self.tool_state.tool {
                                Tool::Note => {
                                    self.note_modal = NoteModal {
                                        open: true, text: String::new(),
                                        page: self.current_page, pos: pdf_pos,
                                    };
                                }
                                Tool::TextBox => {
                                    self.text_modal = TextModal {
                                        open: true, text: String::new(),
                                        page: self.current_page, pos: pdf_pos,
                                        font_size: self.tool_state.brush_size * 5.0 + 8.0,
                                    };
                                }
                                _ => {}
                            }
                        }

                        // Note hover (cursor tool)
                        if self.tool_state.tool == Tool::Cursor {
                            let mut found = None;
                            if self.current_page < self.annots.pages.len() {
                                for (idx, annot) in self.annots.pages[self.current_page].items.iter().enumerate() {
                                    if let Annot::Note(n) = annot {
                                        let ns = self.pdf_to_screen(Pos2::new(n.pos[0], n.pos[1]), origin);
                                        if (pos - ns).length() < 16.0 {
                                            found = Some((self.current_page, idx));
                                            break;
                                        }
                                    }
                                }
                            }
                            self.note_viewer = found.map(|(page, annot_index)| NoteViewer { page, annot_index });
                        }
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label(RichText::new("📂  Open a PDF to get started").size(18.0));
                    });
                }
            });
        });

        // ── Note modal ─────────────────────────────────────────────────────────
        if self.note_modal.open {
            Window::new("📝 Add Note").collapsible(false).resizable(false)
                .show(ctx, |ui| {
                    ui.label("Note text:");
                    ui.text_edit_multiline(&mut self.note_modal.text);
                    ui.horizontal(|ui| {
                        if ui.button("Save Note").clicked() {
                            self.annots.add_annot(self.note_modal.page,
                                Annot::Note(StickyNote {
                                    pos: [self.note_modal.pos.x, self.note_modal.pos.y],
                                    text: self.note_modal.text.clone(),
                                }));
                            self.note_modal.open = false;
                            self.show_toast("Note added — hover with Cursor to view", ctx);
                        }
                        if ui.button("Cancel").clicked() { self.note_modal.open = false; }
                    });
                });
        }

        // ── Text modal ─────────────────────────────────────────────────────────
        if self.text_modal.open {
            Window::new("T  Add Text").collapsible(false).resizable(false)
                .show(ctx, |ui| {
                    ui.label("Text:");
                    ui.text_edit_multiline(&mut self.text_modal.text);
                    ui.horizontal(|ui| {
                        ui.label("Size:");
                        ui.add(DragValue::new(&mut self.text_modal.font_size).range(6.0..=72.0));
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Place").clicked() {
                            let color = color32_to_arr(self.tool_state.color);
                            self.annots.add_annot(self.text_modal.page,
                                Annot::TextBox(crate::annotations::TextBox {
                                    pos: [self.text_modal.pos.x, self.text_modal.pos.y],
                                    text: self.text_modal.text.clone(),
                                    color, font_size: self.text_modal.font_size,
                                }));
                            self.text_modal.open = false;
                            self.show_toast("Text placed!", ctx);
                        }
                        if ui.button("Cancel").clicked() { self.text_modal.open = false; }
                    });
                });
        }

        // ── Toast ──────────────────────────────────────────────────────────────
        if let Some(toast) = &self.toast {
            if toast.is_alive(ctx) {
                Area::new(Id::new("toast")).anchor(Align2::CENTER_BOTTOM, [0.0, -30.0])
                    .show(ctx, |ui| {
                        Frame::dark_canvas(ui.style()).inner_margin(10.0).show(ui, |ui| {
                            ui.label(RichText::new(&toast.text).size(14.0));
                        });
                    });
                ctx.request_repaint();
            } else {
                self.toast = None;
            }
        }
    }
}
