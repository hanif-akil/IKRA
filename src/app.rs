use egui::*;
use std::collections::HashMap;

use crate::annotations::*;
use crate::pdf_engine::PdfEngine;
use crate::ui::{draw_toolbar, handle_shortcuts, Tool, ToolState, ToastMessage};

// Lazy-loading: keep at most this many full-res textures around the current page.
const PAGE_CACHE_RADIUS: usize = 2;
// Thumb cache: keep a smaller radius of thumbs for memory efficiency.
const THUMB_CACHE_RADIUS: usize = 5;
// Gap between the two pages in two-page view (pixels at scale=1).
const TWO_PAGE_GAP: f32 = 16.0;

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
    engine:            Option<PdfEngine>,
    page_textures:     HashMap<usize, PageCache>,
    thumb_textures:    HashMap<usize, TextureHandle>,
    scale:             f32,
    current_page:      usize,
    annots:            AnnotationState,
    active_stroke:     Option<ActiveStroke>,
    active_shape:      Option<ActiveShape>,
    tool_state:        ToolState,
    toast:             Option<ToastMessage>,
    note_modal:        NoteModal,
    text_modal:        TextModal,
    note_viewer:       Option<NoteViewer>,
    search_query:      String,
    search_results:    Vec<usize>,          // pages that contain the query
    search_match_count: usize,              // total match count across all pages
    search_current_idx: usize,             // which result we've navigated to
    /// Approximate index of the first thumb visible in the panel (for lazy thumb loading).
    thumb_viewport_top: usize,
    /// Whether two-page side-by-side view is active.
    two_page_view:     bool,
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
            search_match_count: 0,
            search_current_idx: 0,
            thumb_viewport_top: 0,
            two_page_view: false,
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
                    self.search_results.clear();
                    self.search_match_count = 0;
                    self.search_current_idx = 0;
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

    /// Returns the second page index shown in two-page view, or None.
    fn right_page(&self) -> Option<usize> {
        if !self.two_page_view { return None; }
        let next = self.current_page + 1;
        if next < self.page_count() { Some(next) } else { None }
    }

    /// Ensure the current page (and neighbours including two-page right) are rendered.
    fn manage_page_cache(&mut self, ctx: &Context) {
        let page  = self.current_page;
        let total = self.page_count();
        // In two-page view we need at least page+1 too
        let hi_extra = if self.two_page_view { PAGE_CACHE_RADIUS + 1 } else { PAGE_CACHE_RADIUS };
        let lo = page.saturating_sub(PAGE_CACHE_RADIUS);
        let hi = (page + hi_extra).min(total.saturating_sub(1));

        self.page_textures.retain(|k, _| *k >= lo && *k <= hi);

        if let Some(engine) = &self.engine {
            for p in lo..=hi {
                if self.page_textures.contains_key(&p) { continue; }
                if let Some(img) = engine.render_page(p, self.scale) {
                    let size = [img.width() as _, img.height() as _];
                    let rgba = img.into_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice());
                    drop(rgba);
                    let tex = ctx.load_texture(format!("page_{p}"), ci, TextureOptions::LINEAR);
                    self.page_textures.insert(p, PageCache { texture: tex, rendered_scale: self.scale });
                }
            }
        }
    }

    /// Ensure thumbnails near `center` are rendered; evict distant ones.
    fn manage_thumb_cache(&mut self, ctx: &Context, center: usize) {
        let total = self.page_count();
        if total == 0 { return; }

        let lo = center.saturating_sub(THUMB_CACHE_RADIUS);
        let hi = (center + THUMB_CACHE_RADIUS).min(total.saturating_sub(1));

        self.thumb_textures.retain(|k, _| *k >= lo && *k <= hi);

        if let Some(engine) = &self.engine {
            for p in lo..=hi {
                if self.thumb_textures.contains_key(&p) { continue; }
                if let Some(img) = engine.render_thumb(p) {
                    let size = [img.width() as _, img.height() as _];
                    let rgba = img.into_rgba8();
                    let ci = ColorImage::from_rgba_unmultiplied(size, rgba.as_flat_samples().as_slice());
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

    /// Draw a translucent yellow banner at the top of a page rect if it's a search result page.
    fn draw_search_highlight(&self, painter: &Painter, page_idx: usize, page_rect: Rect) {
        if self.search_query.trim().is_empty() { return; }
        if !self.search_results.contains(&page_idx) { return; }

        // Full-page translucent yellow tint
        painter.rect_filled(
            page_rect,
            0.0,
            Color32::from_rgba_unmultiplied(255, 230, 0, 28),
        );

        // Top banner label
        let banner = Rect::from_min_size(page_rect.min, Vec2::new(page_rect.width(), 22.0 * self.scale.sqrt()));
        painter.rect_filled(banner, 0.0, Color32::from_rgba_unmultiplied(255, 210, 0, 180));
        painter.text(
            banner.center(),
            Align2::CENTER_CENTER,
            "🔍 Match found on this page",
            FontId::proportional(11.0 * self.scale.sqrt().clamp(0.7, 1.4)),
            Color32::from_rgb(80, 50, 0),
        );
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

    /// Draw one PDF page in the scroll area. Returns the rect of the page image drawn.
    /// `interact_id` must be unique per page so responses don't collide.
    fn draw_page_canvas(
        &mut self,
        ui:           &mut Ui,
        page_idx:     usize,
        interact_id:  Id,
        ctx:          &Context,
    ) -> Option<Rect> {
        let cache = self.page_textures.get(&page_idx)?;
        let origin   = ui.cursor().min;
        let tex_size = cache.texture.size_vec2();
        let rect     = Rect::from_min_size(origin, tex_size);

        let (_, painter) = ui.allocate_painter(tex_size, Sense::click_and_drag());
        let response     = ui.interact(rect, interact_id, Sense::click_and_drag());

        // Page shadow
        let shadow_rect = rect.translate(Vec2::new(3.0, 3.0));
        painter.rect_filled(shadow_rect, 2.0, Color32::from_black_alpha(40));

        // PDF page image
        painter.image(cache.texture.id(), rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);

        // Search highlight overlay (behind annotations)
        self.draw_search_highlight(&painter, page_idx, rect);

        // Committed annotations
        self.draw_annotations(&painter, page_idx, origin);

        // Live shape preview (only on the page being actively drawn)
        if self.active_shape.as_ref().map(|s| s.page) == Some(page_idx) {
            self.draw_active_shape_preview(&painter, origin);
        }

        // ── Input ──────────────────────────────────────────────────────────
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
                        self.annots.erase_at(page_idx, pdf_pos, 20.0);
                    }
                    _ if is_free => {
                        self.active_stroke = Some(ActiveStroke {
                            page: page_idx,
                            points: vec![StrokePoint::new(pdf_pos, pressure)],
                        });
                    }
                    _ if is_shape => {
                        self.active_shape = Some(ActiveShape {
                            page: page_idx,
                            start: pdf_pos, end: pdf_pos,
                        });
                    }
                    _ => {}
                }
            } else if response.dragged() {
                if let Some(s) = &mut self.active_stroke {
                    if s.page == page_idx {
                        s.points.push(StrokePoint::new(pdf_pos, pressure));
                    }
                }
                if let Some(s) = &mut self.active_shape {
                    if s.page == page_idx { s.end = pdf_pos; }
                }
                if self.tool_state.tool == Tool::Eraser {
                    self.annots.erase_at(page_idx, pdf_pos, 20.0);
                }
            } else if response.drag_stopped() {
                // Commit freehand
                if let Some(stroke) = self.active_stroke.take() {
                    let kind = if self.tool_state.tool == Tool::Highlight {
                        AnnotKind::Highlight } else { AnnotKind::Pen };
                    let color = color32_to_arr(self.tool_state.color);
                    self.annots.add_annot(page_idx,
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
                    self.annots.add_annot(page_idx,
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
                            page: page_idx, pos: pdf_pos,
                        };
                    }
                    Tool::TextBox => {
                        self.text_modal = TextModal {
                            open: true, text: String::new(),
                            page: page_idx, pos: pdf_pos,
                            font_size: self.tool_state.brush_size * 5.0 + 8.0,
                        };
                    }
                    _ => {}
                }
            }

            // Note hover (cursor tool)
            if self.tool_state.tool == Tool::Cursor {
                let mut found = None;
                if page_idx < self.annots.pages.len() {
                    for (idx, annot) in self.annots.pages[page_idx].items.iter().enumerate() {
                        if let Annot::Note(n) = annot {
                            let ns = self.pdf_to_screen(Pos2::new(n.pos[0], n.pos[1]), origin);
                            if (pos - ns).length() < 16.0 {
                                found = Some((page_idx, idx));
                                break;
                            }
                        }
                    }
                }
                self.note_viewer = found.map(|(page, annot_index)| NoteViewer { page, annot_index });
            }
        }

        Some(rect)
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

        // ── TOP BAR ──────────────────────────────────────────────────────────────
        TopBottomPanel::top("top_bar")
            .min_height(56.0)
            .show(ctx, |ui| {
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.add_space(4.0);

                    // ── File ops ─────────────────────────────────────────────
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

                    // ── Page nav ─────────────────────────────────────────────
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

                    // ── Search ───────────────────────────────────────────────
                    ui.label("🔍");
                    let search_resp = ui.add(
                        TextEdit::singleline(&mut self.search_query)
                            .desired_width(130.0)
                            .hint_text("Search…")
                    );
                    // Search on Enter key too
                    let enter_pressed = search_resp.lost_focus()
                        && ctx.input(|i| i.key_pressed(Key::Enter));

                    let do_search = ui.add_sized([60.0, 28.0], Button::new("Search")).clicked()
                        || enter_pressed;

                    if do_search {
                        if let Some(engine) = &self.engine {
                            let (pages, total_matches) =
                                engine.search_text_with_count(&self.search_query);
                            self.search_results    = pages;
                            self.search_match_count = total_matches;
                            self.search_current_idx = 0;
                            if let Some(&first) = self.search_results.first() {
                                self.current_page = first;
                                self.show_toast(
                                    format!("Found {} match(es) on {} page(s)",
                                        self.search_match_count,
                                        self.search_results.len()),
                                    ctx);
                            } else {
                                self.show_toast("No results found", ctx);
                            }
                        }
                    }

                    // Prev / Next result navigation
                    if !self.search_results.is_empty() {
                        // Result counter badge
                        let badge_txt = format!(
                            "{}/{}",
                            self.search_current_idx + 1,
                            self.search_results.len()
                        );
                        ui.label(
                            RichText::new(badge_txt)
                                .size(11.0)
                                .color(Color32::from_rgb(49, 130, 206))
                                .strong(),
                        );

                        // Total match count
                        ui.label(
                            RichText::new(format!("({} matches)", self.search_match_count))
                                .size(10.0)
                                .color(Color32::GRAY),
                        );

                        if ui.add_sized([22.0, 24.0], Button::new("‹"))
                            .on_hover_text("Previous result page").clicked()
                        {
                            if self.search_current_idx == 0 {
                                self.search_current_idx = self.search_results.len() - 1;
                            } else {
                                self.search_current_idx -= 1;
                            }
                            self.current_page = self.search_results[self.search_current_idx];
                        }
                        if ui.add_sized([22.0, 24.0], Button::new("›"))
                            .on_hover_text("Next result page").clicked()
                        {
                            self.search_current_idx =
                                (self.search_current_idx + 1) % self.search_results.len();
                            self.current_page = self.search_results[self.search_current_idx];
                        }

                        // Clear search
                        if ui.add_sized([22.0, 24.0], Button::new("✕"))
                            .on_hover_text("Clear search").clicked()
                        {
                            self.search_query.clear();
                            self.search_results.clear();
                            self.search_match_count = 0;
                            self.search_current_idx = 0;
                        }
                    }

                    ui.separator();

                    // ── Zoom controls ─────────────────────────────────────────
                    if ui.add_sized([28.0, 28.0], Button::new("−"))
                        .on_hover_text("Zoom out  (Ctrl −scroll)").clicked()
                    { self.set_scale(self.scale / 1.2); }

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
                        let avail = ctx.screen_rect().width() - 70.0 - 145.0 - 40.0;
                        if let Some(engine) = &self.engine {
                            if let Some((pw, _)) = engine.page_size(self.current_page) {
                                // In two-page view, each page gets roughly half the width
                                let divisor = if self.two_page_view { 2.1 } else { 1.0 };
                                if pw > 0.0 { self.set_scale(avail / pw / divisor); }
                            }
                        }
                    }

                    // ── Right-side controls (right-to-left layout) ────────────
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

                        ui.add_space(4.0);

                        // ── Two-page view toggle ──────────────────────────────
                        let two_page_lbl = if self.two_page_view {
                            RichText::new("⬛⬛ 2-Page")
                                .size(11.5)
                                .color(Color32::WHITE)
                        } else {
                            RichText::new("⬛⬛ 2-Page").size(11.5)
                        };

                        let two_page_btn = if self.two_page_view {
                            // Active state: filled blue button
                            Button::new(two_page_lbl)
                                .fill(Color32::from_rgb(49, 130, 206))
                        } else {
                            Button::new(two_page_lbl)
                        };

                        if ui.add_sized([84.0, 28.0], two_page_btn)
                            .on_hover_text("Toggle two-page side-by-side view  [Ctrl+2]")
                            .clicked()
                        {
                            self.two_page_view = !self.two_page_view;
                            // Force even page as left page (0-indexed)
                            if self.two_page_view && self.current_page % 2 == 1 {
                                self.current_page = self.current_page.saturating_sub(1);
                            }
                            self.page_textures.clear();
                        }

                        ui.add_space(4.0);
                        ui.separator();
                        ui.add_space(4.0);

                        // File name (fills remaining space, centred)
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

        // ── Ctrl+2 shortcut for two-page toggle ─────────────────────────────────
        ctx.input(|i| {
            if i.modifiers.ctrl && i.key_pressed(Key::Num2) {
                self.two_page_view = !self.two_page_view;
                if self.two_page_view && self.current_page % 2 == 1 {
                    self.current_page = self.current_page.saturating_sub(1);
                }
                self.page_textures.clear();
            }
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

                let thumb_h = 140.0_f32;

                let scroll = ScrollArea::vertical()
                    .id_salt("thumb_scroll")
                    .show_rows(ui, thumb_h, total, |ui, range| {
                        let center = (range.start + range.end) / 2;
                        self.manage_thumb_cache(ctx, center);

                        for i in range {
                            // Highlight thumb if it's a search result
                            let is_search_hit = !self.search_query.trim().is_empty()
                                && self.search_results.contains(&i);
                            let is_active = i == self.current_page
                                || (self.two_page_view && Some(i) == self.right_page());

                            let frame_col = if is_active {
                                Color32::from_rgb(49, 130, 206)
                            } else if is_search_hit {
                                Color32::from_rgb(220, 170, 0)
                            } else {
                                Color32::TRANSPARENT
                            };

                            if let Some(tex) = self.thumb_textures.get(&i) {
                                Frame::none()
                                    .stroke(Stroke::new(2.0, frame_col))
                                    .inner_margin(2.0)
                                    .show(ui, |ui| {
                                        let r = ui.add(
                                            Image::new(tex)
                                                .max_width(110.0)
                                                .sense(Sense::click())
                                        );
                                        // Draw a small search badge overlay on thumb
                                        if is_search_hit {
                                            let tr = r.rect;
                                            let badge = Rect::from_min_size(
                                                tr.right_top() - Vec2::new(28.0, 0.0),
                                                Vec2::new(28.0, 16.0),
                                            );
                                            ui.painter().rect_filled(
                                                badge, 3.0,
                                                Color32::from_rgb(220, 160, 0));
                                            ui.painter().text(
                                                badge.center(),
                                                Align2::CENTER_CENTER,
                                                "🔍",
                                                FontId::proportional(10.0),
                                                Color32::WHITE,
                                            );
                                        }
                                        if r.clicked() { self.current_page = i; }
                                    });
                            } else {
                                let (rect, resp) = ui.allocate_exact_size(
                                    Vec2::new(110.0, 120.0), Sense::click());
                                ui.painter().rect_filled(rect, 2.0, Color32::from_gray(60));
                                if resp.clicked() { self.current_page = i; }
                            }

                            ui.label(RichText::new(format!("Pg {}", i + 1))
                                .size(10.0)
                                .color(if is_active {
                                    Color32::from_rgb(49, 130, 206)
                                } else if is_search_hit {
                                    Color32::from_rgb(200, 140, 0)
                                } else {
                                    Color32::GRAY
                                }));

                            ui.add_space(4.0);
                        }
                    });

                self.thumb_viewport_top = (scroll.state.offset.y / thumb_h) as usize;
            });

        // ── CENTRAL CANVAS ────────────────────────────────────────────────────
        CentralPanel::default().show(ctx, |ui| {

            // Keyboard navigation (in two-page mode, advance by 2)
            let page_count = self.page_count();
            let step = if self.two_page_view { 2 } else { 1 };
            ctx.input(|i| {
                if (i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::PageDown))
                    && self.current_page + step < page_count
                { self.current_page += step; }
                if (i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp))
                    && self.current_page >= step
                { self.current_page -= step; }
            });

            // Manage texture cache
            self.manage_page_cache(ctx);

            ScrollArea::both().id_salt("canvas_scroll").show(ui, |ui| {
                if self.page_textures.contains_key(&self.current_page) {

                    if self.two_page_view {
                        // ── Two-page side-by-side layout ──────────────────
                        let right_page = self.right_page();

                        // Use a horizontal layout with a gap between the two pages
                        ui.horizontal(|ui| {
                            ui.add_space(8.0);

                            // Left page (current_page)
                            let left_idx = self.current_page;
                            let left_id  = Id::new(("pdf_left", left_idx));
                            self.draw_page_canvas(ui, left_idx, left_id, ctx);

                            ui.add_space(TWO_PAGE_GAP);

                            // Right page (current_page + 1)
                            if let Some(right_idx) = right_page {
                                if self.page_textures.contains_key(&right_idx) {
                                    let right_id = Id::new(("pdf_right", right_idx));
                                    self.draw_page_canvas(ui, right_idx, right_id, ctx);
                                } else {
                                    // Placeholder while right page renders
                                    let ph_size = self.page_textures
                                        .get(&left_idx)
                                        .map(|c| c.texture.size_vec2())
                                        .unwrap_or(Vec2::new(400.0, 560.0));
                                    let (rect, _) = ui.allocate_exact_size(ph_size, Sense::hover());
                                    ui.painter().rect_filled(rect, 2.0,
                                        Color32::from_gray(if ctx.style().visuals.dark_mode { 40 } else { 220 }));
                                }
                            } else {
                                // Last odd-numbered page — show blank placeholder
                                let ph_size = self.page_textures
                                    .get(&left_idx)
                                    .map(|c| c.texture.size_vec2())
                                    .unwrap_or(Vec2::new(400.0, 560.0));
                                let (rect, _) = ui.allocate_exact_size(ph_size, Sense::hover());
                                ui.painter().rect_filled(rect, 2.0,
                                    Color32::from_gray(if ctx.style().visuals.dark_mode { 30 } else { 230 }));
                                ui.painter().text(
                                    rect.center(), Align2::CENTER_CENTER,
                                    "—",
                                    FontId::proportional(28.0),
                                    Color32::from_gray(120),
                                );
                            }

                            ui.add_space(8.0);
                        });

                        // Page label strip below the two pages
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_space(8.0);
                            let left_lbl = format!("Page {}", self.current_page + 1);
                            ui.add_sized([200.0, 16.0],
                                Label::new(RichText::new(left_lbl).size(11.0).color(Color32::GRAY)));
                            ui.add_space(TWO_PAGE_GAP);
                            if let Some(r) = right_page {
                                let right_lbl = format!("Page {}", r + 1);
                                ui.add_sized([200.0, 16.0],
                                    Label::new(RichText::new(right_lbl).size(11.0).color(Color32::GRAY)));
                            }
                        });

                    } else {
                        // ── Single-page layout ─────────────────────────────
                        let page_idx = self.current_page;
                        let page_id  = Id::new("pdf_canvas");
                        self.draw_page_canvas(ui, page_idx, page_id, ctx);
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
