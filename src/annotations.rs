use egui::{Color32, Pos2, Rect, Vec2};
use serde::{Deserialize, Serialize};

// ── Low-level primitives ──────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrokePoint {
    pub pos: [f32; 2],
    pub pressure: f32,
}

impl StrokePoint {
    pub fn new(pos: Pos2, pressure: f32) -> Self {
        Self { pos: [pos.x, pos.y], pressure }
    }
    pub fn to_pos2(&self) -> Pos2 {
        Pos2::new(self.pos[0], self.pos[1])
    }
}

// ── Annotation kinds ──────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AnnotKind {
    Pen,
    Highlight,
    Eraser,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PenStroke {
    pub kind: AnnotKind,
    pub points: Vec<StrokePoint>,
    pub color: [u8; 4],
    pub width: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ShapeKind {
    Rect,
    Ellipse,
    Arrow,
    Line,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShapeAnnot {
    pub kind: ShapeKind,
    pub start: [f32; 2],
    pub end: [f32; 2],
    pub color: [u8; 4],
    pub width: f32,
    pub fill: Option<[u8; 4]>,
    pub dashed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextBox {
    pub pos: [f32; 2],
    pub text: String,
    pub color: [u8; 4],
    pub font_size: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StickyNote {
    pub pos: [f32; 2],
    pub text: String,
}

/// A single annotation shape — the fundamental element of an AnnotationLayer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AnnotationShape {
    Pen(PenStroke),
    Shape(ShapeAnnot),
    TextBox(TextBox),
    Note(StickyNote),
}

// Keep the old `Annot` alias so existing call sites in app.rs compile unchanged.
pub type Annot = AnnotationShape;

// ── AnnotationLayer — serialisable per-page overlay ───────────────────────────

/// The annotation overlay for a single page.
/// This is the unit that gets serialised to JSON independently of the PDF binary.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AnnotationLayer {
    pub shapes: Vec<AnnotationShape>,
}

impl AnnotationLayer {
    pub fn is_empty(&self) -> bool { self.shapes.is_empty() }
}

// Keep the old `PageAnnotations` name as an alias so pdf_engine.rs still compiles.
pub type PageAnnotations = AnnotationLayer;

// ── TextMap — invisible hit-boxes for PDF text ────────────────────────────────

/// Lightweight text-hitbox map: each entry is `(bounding_rect_in_pdf_pts, text_content)`.
/// Stored in PDF-space coordinates (unscaled, origin top-left).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TextMap {
    pub entries: Vec<([f32; 4], String)>,   // [min_x, min_y, max_x, max_y], text
}

impl TextMap {
    pub fn insert(&mut self, rect: Rect, text: String) {
        self.entries.push(([rect.min.x, rect.min.y, rect.max.x, rect.max.y], text));
    }

    /// Return all bounding rects (in PDF space) whose text contains `query`.
    /// Used by the search/highlight system.
    pub fn find_text(&self, query: &str) -> Vec<Rect> {
        if query.trim().is_empty() { return Vec::new(); }
        let q = query.to_lowercase();
        self.entries.iter()
            .filter(|(_, t)| t.to_lowercase().contains(&q))
            .map(|([x0, y0, x1, y1], _)| Rect::from_min_max(
                Pos2::new(*x0, *y0),
                Pos2::new(*x1, *y1),
            ))
            .collect()
    }
}

// ── PageLayer — one complete page ────────────────────────────────────────────

/// All data for a single rendered page.
/// The `background` texture is managed by the GPU-upload pipeline and is *not*
/// serialised; only `annotations` and `text_map` are persisted.
#[derive(Clone)]
pub struct PageLayer {
    /// GPU texture for the pre-rendered PDF raster background (may be absent while loading).
    pub background: Option<egui::TextureHandle>,
    /// Invisible text hit-boxes extracted from the PDF (populated after load).
    pub text_map: TextMap,
    /// User-created annotations — serialisable to JSON independently of the PDF.
    pub annotations: AnnotationLayer,
}

impl PageLayer {
    pub fn new() -> Self {
        Self { background: None, text_map: TextMap::default(), annotations: AnnotationLayer::default() }
    }
}

impl Default for PageLayer {
    fn default() -> Self { Self::new() }
}

// ── Intersection helper ──────────────────────────────────────────────────────

pub fn shape_intersects_circle(shape: &AnnotationShape, center: Pos2, radius: f32) -> bool {
    match shape {
        AnnotationShape::Pen(s) => {
            s.points.iter().any(|p| {
                let dx = p.pos[0] - center.x;
                let dy = p.pos[1] - center.y;
                (dx * dx + dy * dy).sqrt() < radius
            })
        }
        AnnotationShape::TextBox(t) => {
            let dx = t.pos[0] - center.x;
            let dy = t.pos[1] - center.y;
            (dx * dx + dy * dy).sqrt() < radius
        }
        AnnotationShape::Note(n) => {
            let dx = n.pos[0] - center.x;
            let dy = n.pos[1] - center.y;
            (dx * dx + dy * dy).sqrt() < radius
        }
        AnnotationShape::Shape(s) => {
            let p1 = Pos2::new(s.start[0], s.start[1]);
            let p2 = Pos2::new(s.end[0], s.end[1]);
            
            match s.kind {
                ShapeKind::Line | ShapeKind::Arrow => {
                    let l2 = (p1.x - p2.x) * (p1.x - p2.x) + (p1.y - p2.y) * (p1.y - p2.y);
                    if l2 == 0.0 {
                        let dx = p1.x - center.x;
                        let dy = p1.y - center.y;
                        return (dx * dx + dy * dy).sqrt() < radius;
                    }
                    let t = ((center.x - p1.x) * (p2.x - p1.x) + (center.y - p1.y) * (p2.y - p1.y)) / l2;
                    let t = t.clamp(0.0, 1.0);
                    let px = p1.x + t * (p2.x - p1.x);
                    let py = p1.y + t * (p2.y - p1.y);
                    let dx = center.x - px;
                    let dy = center.y - py;
                    (dx * dx + dy * dy).sqrt() < radius
                }
                ShapeKind::Rect | ShapeKind::Ellipse => {
                    let min_x = p1.x.min(p2.x);
                    let max_x = p1.x.max(p2.x);
                    let min_y = p1.y.min(p2.y);
                    let max_y = p1.y.max(p2.y);
                    
                    let closest_x = center.x.clamp(min_x, max_x);
                    let closest_y = center.y.clamp(min_y, max_y);
                    
                    let dx = center.x - closest_x;
                    let dy = center.y - closest_y;
                    
                    (dx * dx + dy * dy).sqrt() < radius
                }
            }
        }
    }
}

// ── DocumentState — top-level model ──────────────────────────────────────────

/// The complete state for one open document.  All annotation logic goes through
/// this struct — the old `AnnotationState` is replaced by this.
pub struct DocumentState {
    pub pages: Vec<PageLayer>,
    pub undo_stack: Vec<Vec<AnnotationLayer>>,
    pub redo_stack: Vec<Vec<AnnotationLayer>>,
}

impl DocumentState {
    pub fn new(page_count: usize) -> Self {
        Self { 
            pages: vec![PageLayer::default(); page_count],
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn insert_page(&mut self, at: usize) {
        self.pages.insert(at, PageLayer::default());
    }

    pub fn page_count(&self) -> usize { self.pages.len() }

    // ── Annotation helpers ────────────────────────────────────────────────────

    pub fn add_annot(&mut self, page: usize, annot: AnnotationShape) {
        if page < self.pages.len() {
            self.pages[page].annotations.shapes.push(annot);
        }
    }

    pub fn erase_at(&mut self, page: usize, pos: Pos2, radius: f32) {
        if page >= self.pages.len() { return; }
        self.pages[page].annotations.shapes.retain(|annot| {
            !shape_intersects_circle(annot, pos, radius)
        });
    }

    pub fn push_undo_checkpoint(&mut self) {
        self.undo_stack.push(self.annotation_layers_cloned());
        self.redo_stack.clear();
    }

    pub fn undo(&mut self) -> bool {
        if let Some(state) = self.undo_stack.pop() {
            self.redo_stack.push(self.annotation_layers_cloned());
            for (i, layer) in state.into_iter().enumerate() {
                if i < self.pages.len() {
                    self.pages[i].annotations = layer;
                }
            }
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(state) = self.redo_stack.pop() {
            self.undo_stack.push(self.annotation_layers_cloned());
            for (i, layer) in state.into_iter().enumerate() {
                if i < self.pages.len() {
                    self.pages[i].annotations = layer;
                }
            }
            true
        } else {
            false
        }
    }

    // ── Text search ───────────────────────────────────────────────────────────

    /// Returns `(page_index, rect_in_pdf_space)` pairs for all text matches.
    pub fn find_text(&self, query: &str) -> Vec<(usize, Rect)> {
        self.pages.iter().enumerate().flat_map(|(i, page)| {
            page.text_map.find_text(query).into_iter().map(move |r| (i, r))
        }).collect()
    }

    // ── Serialisation helpers ─────────────────────────────────────────────────

    /// Returns the annotation layers (one per page) for JSON serialisation.
    /// This is independent from the PDF binary.
    pub fn annotation_layers(&self) -> Vec<&AnnotationLayer> {
        self.pages.iter().map(|p| &p.annotations).collect()
    }

    /// Clones annotation layers for sending over a channel.
    pub fn annotation_layers_cloned(&self) -> Vec<AnnotationLayer> {
        self.pages.iter().map(|p| p.annotations.clone()).collect()
    }
}

// ── Old AnnotationState shim ─────────────────────────────────────────────────
// Keeps tab.rs compiling with zero changes until it is updated in the next step.

pub struct AnnotationState {
    pub pages: Vec<PageAnnotations>,
}

#[allow(dead_code)]
impl AnnotationState {
    pub fn new(page_count: usize) -> Self {
        Self { pages: vec![PageAnnotations::default(); page_count] }
    }

    pub fn insert_page(&mut self, at: usize) {
        self.pages.insert(at, PageAnnotations::default());
    }

    pub fn add_annot(&mut self, page: usize, annot: Annot) {
        if page < self.pages.len() {
            self.pages[page].shapes.push(annot);
        }
    }

    pub fn erase_at(&mut self, page: usize, pos: Pos2, radius: f32) {
        if page >= self.pages.len() { return; }
        self.pages[page].shapes.retain(|annot| {
            !shape_intersects_circle(annot, pos, radius)
        });
    }
}

// ── Vector arrow drawing helper ───────────────────────────────────────────────

/// Style parameters for a vector arrow.
pub struct ArrowStyle {
    /// Stroke width in screen pixels.
    pub width: f32,
    /// Arrow colour.
    pub color: Color32,
    /// Head length as a fraction of the total arrow length (clamped to screen pixels).
    pub head_length_px: f32,
    /// Head half-width in screen pixels.
    pub head_half_width_px: f32,
}

impl ArrowStyle {
    pub fn new(width: f32, color: Color32) -> Self {
        Self { width, color, head_length_px: 14.0, head_half_width_px: 7.0 }
    }
}

/// Draw a vector arrow from `start` to `end` using `painter`.
///
/// The arrowhead is a filled triangle whose **tip** sits exactly on `end`,
/// calculated using trigonometry so it remains proportional at any angle.
pub fn draw_vector_arrow(painter: &egui::Painter, start: Pos2, end: Pos2, style: &ArrowStyle) {
    let delta = end - start;
    let length = delta.length();

    if length < 1.0 { return; }          // zero-length arrow — nothing to draw

    // Unit direction vector and its perpendicular
    let dir  = delta / length;
    let perp = Vec2::new(-dir.y, dir.x);

    let hl = style.head_length_px.min(length * 0.5);   // head length (clamped)
    let hw = style.head_half_width_px;                  // head half-width

    // Shaft ends just before the arrowhead base
    let shaft_end = end - dir * hl;
    let stroke = egui::Stroke::new(style.width, style.color);

    // Draw shaft
    painter.line_segment([start, shaft_end], stroke);

    // Arrowhead triangle — tip exactly on `end`
    let tip      = end;
    let base_l   = shaft_end + perp * hw;
    let base_r   = shaft_end - perp * hw;

    painter.add(egui::Shape::convex_polygon(
        vec![tip, base_l, base_r],
        style.color,
        egui::Stroke::NONE,
    ));
}

// ── Colour helpers ────────────────────────────────────────────────────────────

pub fn color32_to_arr(c: Color32) -> [u8; 4] {
    [c.r(), c.g(), c.b(), c.a()]
}

pub fn arr_to_color32(arr: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(arr[0], arr[1], arr[2], arr[3])
}

// ── Versioned annotation sidecar ──────────────────────────────────────────────

/// Top-level sidecar container written to `.ikra.json` alongside each PDF.
///
/// ## Schema versioning
/// - **v1 (legacy)**: bare `Vec<AnnotationLayer>` JSON array — no wrapper object.
/// - **v2 (current)**: JSON object `{ "schema_version": 2, "pdf_fingerprint": "…", "layers": […] }`.
///
/// `from_json` auto-detects v1 (array) vs v2 (object) and migrates transparently.
///
/// ## PDF integrity check
/// `pdf_fingerprint` is a lightweight identity string (file size + mtime).
/// If it doesn't match on load, the UI should warn the user that annotations
/// may be misaligned because the source PDF has changed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnnotationSidecar {
    /// Incremented on breaking schema changes.
    pub schema_version:  u32,
    /// `"sz{bytes}ts{unix_secs}"` fingerprint of the source PDF at save time.
    pub pdf_fingerprint: Option<String>,
    /// Per-page annotation layers — must **not** include `TextMap` data.
    pub layers: Vec<AnnotationLayer>,
}

impl AnnotationSidecar {
    pub const CURRENT_VERSION: u32 = 2;

    pub fn new(layers: Vec<AnnotationLayer>, pdf_fingerprint: Option<String>) -> Self {
        Self { schema_version: Self::CURRENT_VERSION, pdf_fingerprint, layers }
    }

    /// Deserialise from JSON, migrating v1 legacy format automatically.
    pub fn from_json(json: &str) -> Result<Self, String> {
        // Try current versioned format (object)
        if let Ok(s) = serde_json::from_str::<AnnotationSidecar>(json) {
            return Ok(s);
        }
        // Fall back to legacy v1: bare Vec<AnnotationLayer> (JSON array)
        match serde_json::from_str::<Vec<AnnotationLayer>>(json) {
            Ok(layers) => Ok(Self {
                schema_version:  Self::CURRENT_VERSION,
                pdf_fingerprint: None,
                layers,
            }),
            Err(e) => Err(format!("Cannot parse annotation sidecar: {}", e)),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

// ── PDF identity helper ───────────────────────────────────────────────────────

/// Returns a lightweight fingerprint string for a PDF file:
/// `"sz{byte_count}ts{mtime_unix_secs}"`.
///
/// This is not a cryptographic hash — it detects the most common case where
/// the PDF is re-exported or edited externally, which changes both size and
/// modification time.  No extra dependencies required.
pub fn pdf_fingerprint(path: &str) -> Option<String> {
    let meta    = std::fs::metadata(path).ok()?;
    let size    = meta.len();
    let secs    = meta.modified().ok()?
        .duration_since(std::time::UNIX_EPOCH).ok()?
        .as_secs();
    Some(format!("sz{}ts{}", size, secs))
}