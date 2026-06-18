/// `LayeredPageView` — a self-contained egui widget that renders one PDF page
/// as a stack of discrete layers:
///
/// 1. **Background** — pre-rendered PDF raster (`TextureHandle`).
/// 2. **TextMap highlights** — translucent search-result rectangles.
/// 3. **Annotation layer** — committed strokes, arrows, text boxes, notes.
/// 4. **Live stroke** — the `CurrentStroke` buffer being drawn right now.
///
/// All drawing is done through `egui::Painter`; no OpenGL/Vulkan calls are
/// made directly.  The widget captures pointer input and updates the
/// `CurrentStroke` buffer at the display refresh rate (typically 60 Hz).

use egui::{
    Color32, FontId, Id, Painter, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2,
    Align2, Shape,
};
use crate::annotations::{
    AnnotationLayer, AnnotationShape, AnnotKind, ArrowStyle, PenStroke,
    ShapeAnnot, ShapeKind, StrokePoint, TextBox, StickyNote, TextMap,
    arr_to_color32, color32_to_arr, draw_vector_arrow,
};
use crate::tab::NoteViewer;
use crate::ui::{Tool, ToolState};

// ── CurrentStroke — 60 Hz live-draw buffer ────────────────────────────────────

/// Accumulates pointer samples for the stroke being drawn right now.
/// When the drag ends the buffer is committed to the `AnnotationLayer`.
pub struct CurrentStroke {
    pub page: usize,
    pub points: Vec<StrokePoint>,
}

impl CurrentStroke {
    pub fn new(page: usize, first: Pos2, pressure: f32) -> Self {
        Self { page, points: vec![StrokePoint::new(first, pressure)] }
    }
    pub fn push(&mut self, pos: Pos2, pressure: f32) {
        self.points.push(StrokePoint::new(pos, pressure));
    }
}

// ── ActiveShape — preview while dragging a shape tool ─────────────────────────

pub struct ActiveShape {
    pub page: usize,
    pub start: Pos2,
    pub end: Pos2,
}

// ── Coordinate helpers ────────────────────────────────────────────────────────

fn screen_to_pdf(screen: Pos2, origin: Pos2, scale: f32) -> Pos2 {
    Pos2::new((screen.x - origin.x) / scale, (screen.y - origin.y) / scale)
}

fn pdf_to_screen(pdf: Pos2, origin: Pos2, scale: f32) -> Pos2 {
    Pos2::new(origin.x + pdf.x * scale, origin.y + pdf.y * scale)
}

// ── Ellipse helper ────────────────────────────────────────────────────────────

fn ellipse_points(center: Pos2, rx: f32, ry: f32, segments: usize) -> Vec<Pos2> {
    (0..segments).map(|i| {
        let a = i as f32 * std::f32::consts::TAU / segments as f32;
        Pos2::new(center.x + rx * a.cos(), center.y + ry * a.sin())
    }).collect()
}

// ── LayeredPageView ───────────────────────────────────────────────────────────

/// Builder / widget for one PDF page with full layer rendering.
pub struct LayeredPageView<'a> {
    page_idx: usize,
    scale: f32,
    tex: Option<&'a egui::TextureHandle>,
    tex_load_time: f64,
    annotations: &'a AnnotationLayer,
    text_map: &'a TextMap,
    search_rects: &'a [Rect],     // PDF-space rects to highlight
    active_shape: Option<&'a ActiveShape>,
    note_viewer: Option<&'a NoteViewer>,
    interact_id: Id,
}

impl<'a> LayeredPageView<'a> {
    pub fn new(
        page_idx: usize,
        scale: f32,
        tex: Option<&'a egui::TextureHandle>,
        tex_load_time: f64,
        annotations: &'a AnnotationLayer,
        text_map: &'a TextMap,
        search_rects: &'a [Rect],
        active_shape: Option<&'a ActiveShape>,
        note_viewer: Option<&'a NoteViewer>,
        interact_id: Id,
    ) -> Self {
        Self { page_idx, scale, tex, tex_load_time, annotations, text_map,
               search_rects, active_shape, note_viewer, interact_id }
    }

    /// Show the widget.  Returns the screen rect occupied by the page.
    ///
    /// Caller must provide:
    /// - `tool_state` — current tool/colour/size
    /// - `current_stroke` — mutable ref to the live-draw buffer
    /// - `page_size_pts` — (width, height) in PDF points (used for placeholder sizing)
    ///
    /// After this call the caller should inspect `current_stroke` and
    /// `completed_stroke`/`completed_shape` outputs (returned via the
    /// `WidgetOutput` struct) to commit finished annotations.
    pub fn show(
        &self,
        ui: &mut Ui,
        ctx: &egui::Context,
        tool_state: &ToolState,
        page_size_pts: (f32, f32),
        // mutable outputs
        current_stroke: &mut Option<CurrentStroke>,
        current_shape: &mut Option<ActiveShape>,
        on_click: &mut impl FnMut(Pos2),   // called on single click (Note / TextBox)
    ) -> Rect {
        // ── Size / placeholder logic ──────────────────────────────────────────
        let layout_size = Vec2::new(page_size_pts.0 * self.scale, page_size_pts.1 * self.scale);
        
        let (alloc_resp, painter) = ui.allocate_painter(layout_size, Sense::hover());
        let rect = alloc_resp.rect;
        let origin = rect.min;

        let response = ui.interact(rect, self.interact_id, Sense::click_and_drag());

        // ── Layer 0: shadow ───────────────────────────────────────────────────
        let shadow = rect.translate(Vec2::new(3.0, 3.0));
        painter.rect_filled(shadow, 2.0, Color32::from_black_alpha(40));

        // ── Layer 1: PDF background ───────────────────────────────────────────
        if let Some(tex) = self.tex {
            let elapsed = ctx.input(|i| i.time) - self.tex_load_time;
            let alpha   = (elapsed * 5.0).min(1.0) as f32;
            let tint    = Color32::from_white_alpha((alpha * 255.0) as u8);
            painter.image(tex.id(), rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
            if alpha < 1.0 { ctx.request_repaint(); }
        } else {
            painter.rect_filled(rect, 0.0, Color32::WHITE);
            painter.text(rect.center(), Align2::CENTER_CENTER, "Rendering…",
                FontId::proportional(20.0), Color32::GRAY);
        }

        // ── Layer 2: TextMap search highlights ────────────────────────────────
        for pdf_rect in self.search_rects {
            let tl = pdf_to_screen(pdf_rect.min, origin, self.scale);
            let br = pdf_to_screen(pdf_rect.max, origin, self.scale);
            let screen_rect = Rect::from_min_max(tl, br);
            painter.rect_filled(screen_rect, 0.0,
                Color32::from_rgba_unmultiplied(255, 230, 0, 100));
        }

        // ── Layer 3: committed annotations ────────────────────────────────────
        self.draw_annotation_layer(&painter, self.annotations, origin, tool_state);

        // ── Layer 4: active-shape preview ────────────────────────────────────
        if let Some(shape) = self.active_shape {
            if shape.page == self.page_idx {
                self.draw_shape_preview(&painter, shape, origin, tool_state);
            }
        }

        // ── Layer 5: live stroke (CurrentStroke buffer) ───────────────────────
        if let Some(cs) = current_stroke.as_ref() {
            if cs.page == self.page_idx && cs.points.len() >= 2 {
                let pts: Vec<Pos2> = cs.points.iter()
                    .map(|p| pdf_to_screen(p.to_pos2(), origin, self.scale))
                    .collect();
                let color = tool_state.color;
                match tool_state.tool {
                    Tool::Highlight => {
                        let hc = Color32::from_rgba_unmultiplied(
                            color.r(), color.g(), color.b(),
                            (tool_state.opacity * 255.0) as u8);
                        painter.add(Shape::Path(egui::epaint::PathShape {
                            points: pts, closed: false, fill: Color32::TRANSPARENT,
                            stroke: egui::epaint::PathStroke::new(
                                tool_state.brush_size * self.scale * 8.0, hc),
                        }));
                    }
                    _ => {
                        let stroke = Stroke::new(tool_state.brush_size * self.scale, color);
                        for i in 1..pts.len() {
                            painter.line_segment([pts[i-1], pts[i]], stroke);
                        }
                    }
                }
            }
        }

        // ── Input handling ────────────────────────────────────────────────────
        let is_free  = matches!(tool_state.tool, Tool::Pen | Tool::Highlight);
        let is_shape = matches!(tool_state.tool, Tool::Rect | Tool::Ellipse | Tool::Arrow | Tool::Line);

        if let Some(pos) = response.hover_pos() {
            if tool_state.tool == Tool::Eraser {
                let radius = 20.0 * self.scale;
                painter.circle_filled(pos, radius, Color32::from_black_alpha(20));
                painter.circle_stroke(pos, radius, Stroke::new(1.0, Color32::GRAY));
            }


            let pdf_pos  = screen_to_pdf(pos, origin, self.scale);
            let pressure = ctx.input(|i| read_pressure(i));

            if response.drag_started() {
                match tool_state.tool {
                    Tool::Eraser => { /* handled by caller via erase_at */ }
                    _ if is_free => {
                        *current_stroke = Some(CurrentStroke::new(self.page_idx, pdf_pos, pressure));
                    }
                    _ if is_shape => {
                        *current_shape = Some(ActiveShape {
                            page: self.page_idx, start: pdf_pos, end: pdf_pos,
                        });
                    }
                    _ => {}
                }
            } else if response.dragged() {
                if let Some(cs) = current_stroke.as_mut() {
                    if cs.page == self.page_idx { cs.push(pdf_pos, pressure); }
                }
                if let Some(s) = current_shape.as_mut() {
                    if s.page == self.page_idx { s.end = pdf_pos; }
                }
            }

            if response.clicked() {
                on_click(pdf_pos);
            }
        }

        rect
    }

    // ── Private drawing helpers ───────────────────────────────────────────────

    fn draw_annotation_layer(
        &self,
        painter: &Painter,
        layer: &AnnotationLayer,
        origin: Pos2,
        tool_state: &ToolState,
    ) {
        for (idx, annot) in layer.shapes.iter().enumerate() {
            match annot {
                AnnotationShape::Pen(s) => {
                    if s.points.len() < 2 { continue; }
                    let pts: Vec<Pos2> = s.points.iter()
                        .map(|p| pdf_to_screen(p.to_pos2(), origin, self.scale))
                        .collect();
                    match s.kind {
                        AnnotKind::Highlight => {
                            let c = arr_to_color32(s.color);
                            let hc = Color32::from_rgba_unmultiplied(
                                c.r(), c.g(), c.b(), (tool_state.opacity * 255.0) as u8);
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

                AnnotationShape::Shape(s) => {
                    let p1 = pdf_to_screen(Pos2::new(s.start[0], s.start[1]), origin, self.scale);
                    let p2 = pdf_to_screen(Pos2::new(s.end[0],   s.end[1]),   origin, self.scale);
                    let color  = arr_to_color32(s.color);
                    let stroke = Stroke::new(s.width * self.scale, color);
                    match s.kind {
                        ShapeKind::Rect => {
                            painter.rect_stroke(Rect::from_two_pos(p1, p2), 0.0, stroke);
                        }
                        ShapeKind::Ellipse => {
                            let c = Pos2::new((p1.x+p2.x)/2.0, (p1.y+p2.y)/2.0);
                            let rx = (p2.x-p1.x).abs()/2.0;
                            let ry = (p2.y-p1.y).abs()/2.0;
                            painter.add(Shape::Path(egui::epaint::PathShape {
                                points: ellipse_points(c, rx, ry, 48),
                                closed: true, fill: Color32::TRANSPARENT,
                                stroke: egui::epaint::PathStroke::new(s.width * self.scale, color),
                            }));
                        }
                        ShapeKind::Arrow => {
                            let arrow_style = ArrowStyle {
                                width: s.width * self.scale,
                                color,
                                head_length_px: 14.0 * self.scale,
                                head_half_width_px: 7.0 * self.scale,
                            };
                            draw_vector_arrow(painter, p1, p2, &arrow_style);
                        }
                        ShapeKind::Line => { painter.line_segment([p1, p2], stroke); }
                    }
                }

                AnnotationShape::TextBox(t) => {
                    let pos   = pdf_to_screen(Pos2::new(t.pos[0], t.pos[1]), origin, self.scale);
                    let color = arr_to_color32(t.color);
                    painter.text(pos, Align2::LEFT_TOP, &t.text,
                        FontId::proportional(t.font_size * self.scale), color);
                }

                AnnotationShape::Note(n) => {
                    let pos = pdf_to_screen(Pos2::new(n.pos[0], n.pos[1]), origin, self.scale);
                    painter.circle_filled(pos, 10.0 * self.scale.sqrt(),
                        Color32::from_rgb(255, 220, 50));
                    painter.text(pos, Align2::CENTER_CENTER, "📝",
                        FontId::proportional(14.0), Color32::BLACK);
                    if let Some(v) = self.note_viewer {
                        if v.page == self.page_idx && v.annot_index == idx {
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

    fn draw_shape_preview(
        &self,
        painter: &Painter,
        shape: &ActiveShape,
        origin: Pos2,
        tool_state: &ToolState,
    ) {
        let p1     = pdf_to_screen(shape.start, origin, self.scale);
        let p2     = pdf_to_screen(shape.end,   origin, self.scale);
        let color  = tool_state.color;
        let stroke = Stroke::new(tool_state.brush_size * self.scale, color);

        match tool_state.tool {
            Tool::Rect    => { painter.rect_stroke(Rect::from_two_pos(p1, p2), 0.0, stroke); }
            Tool::Ellipse => {
                let c  = Pos2::new((p1.x+p2.x)/2.0, (p1.y+p2.y)/2.0);
                let rx = (p2.x-p1.x).abs()/2.0;
                let ry = (p2.y-p1.y).abs()/2.0;
                painter.add(Shape::Path(egui::epaint::PathShape {
                    points: ellipse_points(c, rx, ry, 48), closed: true,
                    fill: Color32::TRANSPARENT,
                    stroke: egui::epaint::PathStroke::new(tool_state.brush_size * self.scale, color),
                }));
            }
            Tool::Arrow => {
                let arrow_style = ArrowStyle {
                    width: tool_state.brush_size * self.scale,
                    color,
                    head_length_px: 14.0 * self.scale,
                    head_half_width_px: 7.0 * self.scale,
                };
                draw_vector_arrow(painter, p1, p2, &arrow_style);
            }
            Tool::Line => { painter.line_segment([p1, p2], stroke); }
            _ => {}
        }
    }
}

// ── Pressure helper ───────────────────────────────────────────────────────────

fn read_pressure(input: &egui::InputState) -> f32 {
    for event in &input.events {
        if let egui::Event::Touch { force: Some(f), .. } = event {
            return f.clamp(0.01, 1.0);
        }
    }
    0.5
}
