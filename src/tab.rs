use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use egui::{TextureHandle, Pos2};
use uuid::Uuid;

use crate::annotations::{DocumentState, AnnotationLayer, StrokePoint, AnnotationSidecar};
use crate::pdf_engine::{WorkerRequest, RenderKind, LOWRES_SCALE};
use crate::layered_view::{ActiveShape, CurrentStroke};
use crate::bookmarks::PdfOutlineEntry;

pub const PREFETCH_LOOKAHEAD: usize = 4;
pub const MAX_TEXTURE_CACHE_LIMIT: usize = 10;  // Reduced from 50
pub const THUMB_CACHE_RADIUS: usize = 5;
pub const TWO_PAGE_GAP: f32 = 16.0;

pub struct PageCache {
    pub texture: TextureHandle,
    pub load_time: f64,
    pub last_accessed: f64,
    /// `true` = low-res preview; `false` = full-quality HighRes.
    /// HighRes must never be downgraded to LowRes.
    pub is_preview: bool,
}



pub struct NoteViewer {
    pub page: usize,
    pub annot_index: usize,
}

pub struct PdfTab {
    pub id: Uuid,
    pub file_path: String,
    pub page_sizes: Vec<(f32, f32)>,
    pub page_textures: HashMap<usize, PageCache>,
    pub thumb_textures: HashMap<usize, TextureHandle>,
    pub garbage_textures: Vec<TextureHandle>,

    pub pending_renders: HashSet<(usize, RenderKind)>,

    pub scale: f32,
    pub current_page: usize,

    /// New layered document state — replaces the old `annots: AnnotationState`.
    pub doc: DocumentState,

    /// Live-draw buffer (freehand pen / highlight).
    pub current_stroke: Option<CurrentStroke>,
    /// Live-draw buffer (shape tools).
    pub active_shape: Option<ActiveShape>,

    pub needs_undo_checkpoint: bool,

    pub search_query: String,
    /// Pages (indices) that contain at least one text match.
    pub search_results: Vec<usize>,
    /// Rects in PDF space that matched the last search, per page.
    pub search_rects: Vec<Vec<egui::Rect>>,
    pub search_match_count: usize,
    pub search_current_idx: usize,

    pub two_page_view: bool,
    pub note_viewer: Option<NoteViewer>,

    /// Outline (bookmark tree) extracted from the PDF itself on load.
    /// Empty if the PDF has no outline.
    pub pdf_outline: Vec<PdfOutlineEntry>,
}

impl PdfTab {
    pub fn new(id: Uuid, file_path: String, page_sizes: Vec<(f32, f32)>) -> Self {
        let page_count = page_sizes.len();
        Self {
            id,
            file_path,
            page_sizes,
            page_textures: HashMap::new(),
            thumb_textures: HashMap::new(),
            garbage_textures: Vec::new(),
            pending_renders: HashSet::new(),
            scale: 1.4,
            current_page: 0,
            doc: DocumentState::new(page_count),
            current_stroke: None,
            active_shape: None,
            needs_undo_checkpoint: false,
            search_query: String::new(),
            search_results: Vec::new(),
            search_rects: Vec::new(),
            search_match_count: 0,
            search_current_idx: 0,
            two_page_view: false,
            note_viewer: None,
            pdf_outline: Vec::new(),
        }
    }

    pub fn name(&self) -> String {
        std::path::Path::new(&self.file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled")
            .to_string()
    }

    pub fn page_count(&self) -> usize {
        self.page_sizes.len()
    }

    pub fn set_scale(&mut self, new_scale: f32, tx: Option<&Sender<WorkerRequest>>) {
        let clamped = new_scale.clamp(0.1, 8.0);
        if (clamped - self.scale).abs() > 0.005 {
            self.scale = clamped;
            // Drop all cached page textures (both LowRes previews and HighRes).
            // LowRes renders at the fixed LOWRES_SCALE so they are still valid;
            // however we clear them too so that the new HighRes renders fill in
            // consistently at the new scale without a quality mismatch flash.
            for (_, cache) in self.page_textures.drain() {
                self.garbage_textures.push(cache.texture);
            }
            // Force garbage collection immediately on scale change
            for tex in self.garbage_textures.drain(..) {
                drop(tex);  // Explicit immediate drop
            }
            // Cancel queued HighRes renders at the old scale.
            // LowRes renders (fixed scale) can stay unless already cancelled.
            self.pending_renders.retain(|&(_, ref kind)| *kind != RenderKind::HighRes);
            // Ask the worker to purge its heap too
            if let Some(tx) = tx {
                let _ = tx.send(WorkerRequest::CancelPageRenders(self.id));
            }
        }
    }

    pub fn screen_to_pdf(&self, screen: Pos2, origin: Pos2) -> Pos2 {
        Pos2::new((screen.x - origin.x) / self.scale, (screen.y - origin.y) / self.scale)
    }

    pub fn pdf_to_screen(&self, pdf: Pos2, origin: Pos2) -> Pos2 {
        Pos2::new(origin.x + pdf.x * self.scale, origin.y + pdf.y * self.scale)
    }

    pub fn right_page(&self) -> Option<usize> {
        if !self.two_page_view { return None; }
        let next = self.current_page + 1;
        if next < self.page_count() { Some(next) } else { None }
    }

    // ── Backward-compat helpers (used by app.rs) ──────────────────────────────

    /// Returns the annotation layer for `page` as a slice suitable for the old
    /// `draw_annotations` path (now delegated to `LayeredPageView`).
    pub fn annotation_layer(&self, page: usize) -> Option<&AnnotationLayer> {
        self.doc.pages.get(page).map(|p| &p.annotations)
    }

    /// Clones annotation layers for the save/serialise path.
    pub fn annotation_layers_cloned(&self) -> Vec<AnnotationLayer> {
        self.doc.annotation_layers_cloned()
    }

    // ── Thumbnail management ──────────────────────────────────────────────────

    pub fn manage_thumb_cache(&mut self, center_page: usize, tx: &Sender<WorkerRequest>) {
        if self.page_count() == 0 { return; }

        let lo = center_page.saturating_sub(THUMB_CACHE_RADIUS);
        let hi = (center_page + THUMB_CACHE_RADIUS).min(self.page_count().saturating_sub(1));

        let mut to_remove = Vec::new();
        for k in self.thumb_textures.keys() {
            if *k < lo || *k > hi { to_remove.push(*k); }
        }
        for k in to_remove {
            if let Some(tex) = self.thumb_textures.remove(&k) {
                self.garbage_textures.push(tex);
            }
        }

        for p in lo..=hi {
            if self.thumb_textures.contains_key(&p) { continue; }
            let req_key = (p, RenderKind::Thumb);
            if !self.pending_renders.contains(&req_key) {
                self.pending_renders.insert(req_key.clone());
                let _ = tx.send(WorkerRequest::Render {
                    id: self.id, page: p,
                    scale: 0.2, kind: RenderKind::Thumb, priority: 2,
                });
            }
        }
    }

    // ── Page-cache management ─────────────────────────────────────────────────

    pub fn manage_page_cache(&mut self, tx: &Sender<WorkerRequest>) {
        if self.page_count() == 0 { return; }

        let page     = self.current_page;
        let total    = self.page_count();
        let hi_extra = if self.two_page_view { PREFETCH_LOOKAHEAD + 1 } else { PREFETCH_LOOKAHEAD };
        let lo       = page.saturating_sub(PREFETCH_LOOKAHEAD);
        let hi       = (page + hi_extra).min(total.saturating_sub(1));

        // HighRes prefetch is narrower — only the visible window + 1
        let hi_lo    = page.saturating_sub(1);
        let hi_hi    = (page + 1 + if self.two_page_view { 1 } else { 0 }).min(total.saturating_sub(1));

        for p in lo..=hi {
            // ── Pass 1: LowRes quick preview ─────────────────────────────────
            let has_any = self.page_textures.contains_key(&p);
            if !has_any {
                let req_key = (p, RenderKind::LowRes);
                if !self.pending_renders.contains(&req_key) {
                    self.pending_renders.insert(req_key);
                    let priority: u8 = if p == page { 0 } else { 1 };
                    let _ = tx.send(WorkerRequest::Render {
                        id: self.id, page: p,
                        scale: LOWRES_SCALE, kind: RenderKind::LowRes, priority,
                    });
                }
            }

            // ── Pass 2: HighRes upgrade (visible window only) ─────────────────
            if p >= hi_lo && p <= hi_hi {
                let has_hires = self.page_textures.get(&p)
                    .map(|c| !c.is_preview)
                    .unwrap_or(false);
                if !has_hires {
                    let req_key = (p, RenderKind::HighRes);
                    if !self.pending_renders.contains(&req_key) {
                        self.pending_renders.insert(req_key);
                        let priority: u8 = if p == page { 1 } else { 2 };
                        let _ = tx.send(WorkerRequest::Render {
                            id: self.id, page: p,
                            scale: self.scale, kind: RenderKind::HighRes, priority,
                        });
                    }
                }
            }
        }

        if self.page_textures.len() > MAX_TEXTURE_CACHE_LIMIT {
            let mut keys: Vec<usize> = self.page_textures.keys().copied().collect();
            keys.retain(|k| *k < lo || *k > hi);
            keys.sort_by(|a, b| {
                let t_a = self.page_textures.get(a).map(|c| c.last_accessed).unwrap_or(0.0);
                let t_b = self.page_textures.get(b).map(|c| c.last_accessed).unwrap_or(0.0);
                t_a.partial_cmp(&t_b).unwrap_or(std::cmp::Ordering::Equal)
            });

            let to_remove_count = self.page_textures.len().saturating_sub(MAX_TEXTURE_CACHE_LIMIT);
            let mut removed = 0;
            for k in keys {
                if removed >= to_remove_count { break; }
                if let Some(cache) = self.page_textures.remove(&k) {
                    self.garbage_textures.push(cache.texture);
                    removed += 1;
                }
            }
        }
    }

    // ── Render response ───────────────────────────────────────────────────────

    pub fn process_render_response(
        &mut self,
        _id: Uuid,
        page: usize,
        res_scale: f32,
        kind: RenderKind,
        img: image::DynamicImage,
        ctx: &egui::Context,
    ) {
        let req_key = (page, kind.clone());
        self.pending_renders.remove(&req_key);

        if img.width() == 0 || img.height() == 0 {
            return;
        }

        match kind {
            // ── LowRes: instant preview ───────────────────────────────────────
            RenderKind::LowRes => {
                // Never downgrade an existing HighRes texture
                let already_hires = self.page_textures.get(&page)
                    .map(|c| !c.is_preview)
                    .unwrap_or(false);
                if !already_hires {
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [img.width() as _, img.height() as _],
                        img.into_rgba8().as_raw(),
                    );
                    let tex = ctx.load_texture(
                        format!("page_lr_{}_{}", self.id, page), ci,
                        egui::TextureOptions::LINEAR);
                    if page < self.doc.pages.len() {
                        // Defer the old background handle; do not drop it mid-frame.
                        let _old_bg = self.doc.pages[page].background.replace(tex.clone());
                        if let Some(old) = _old_bg {
                            self.garbage_textures.push(old);
                        }
                    }
                    let now = ctx.input(|i| i.time);
                    // Capture the evicted PageCache so its TextureHandle is deferred,
                    // not dropped immediately inside HashMap::insert.
                    if let Some(old_cache) = self.page_textures.insert(page, PageCache {
                        texture: tex, load_time: now, last_accessed: now, is_preview: true,
                    }) {
                        self.garbage_textures.push(old_cache.texture);
                    }
                    ctx.request_repaint();
                }
            }

            // ── HighRes: full-quality upgrade ─────────────────────────────────
            RenderKind::HighRes => {
                if (res_scale - self.scale).abs() < 0.01 {
                    let ci = egui::ColorImage::from_rgba_unmultiplied(
                        [img.width() as _, img.height() as _],
                        img.into_rgba8().as_raw(),
                    );
                    let tex = ctx.load_texture(
                        format!("page_hr_{}_{}", self.id, page), ci,
                        egui::TextureOptions::LINEAR);
                    if page < self.doc.pages.len() {
                        // Defer the old background handle; do not drop it mid-frame.
                        let _old_bg = self.doc.pages[page].background.replace(tex.clone());
                        if let Some(old) = _old_bg {
                            self.garbage_textures.push(old);
                        }
                    }
                    let now = ctx.input(|i| i.time);
                    // Capture the evicted PageCache so its TextureHandle is deferred,
                    // not dropped immediately inside HashMap::insert.
                    if let Some(old_cache) = self.page_textures.insert(page, PageCache {
                        texture: tex, load_time: now, last_accessed: now, is_preview: false,
                    }) {
                        self.garbage_textures.push(old_cache.texture);
                    }
                    ctx.request_repaint();
                }
                // Stale scale → silently drop
            }

            RenderKind::Thumb => {
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [img.width() as _, img.height() as _],
                    img.into_rgba8().as_raw(),
                );
                let tex = ctx.load_texture(
                    format!("thumb_{}_{}", self.id, page), ci,
                    egui::TextureOptions::LINEAR);
                // Defer the old thumbnail handle; do not drop it mid-frame.
                if let Some(old_tex) = self.thumb_textures.insert(page, tex) {
                    self.garbage_textures.push(old_tex);
                }
                ctx.request_repaint();
            }
        }
    }

    // ── Annotation persistence helpers ────────────────────────────────────────

    /// Serialise annotations to a versioned JSON sidecar string.
    pub fn annotations_to_sidecar_json(&self) -> Result<String, String> {
        let layers = self.doc.annotation_layers_cloned();
        let fp     = crate::annotations::pdf_fingerprint(&self.file_path);
        AnnotationSidecar::new(layers, fp)
            .to_json()
            .map_err(|e| e.to_string())
    }

    /// Load annotation layers from a versioned (or legacy) sidecar.
    pub fn annotations_from_sidecar(&mut self, sidecar: &AnnotationSidecar) {
        for (i, layer) in sidecar.layers.iter().enumerate() {
            if i < self.doc.pages.len() {
                self.doc.pages[i].annotations = layer.clone();
            }
        }
    }

    /// Convenience: returns the `is_preview` flag for a cached page texture.
    pub fn page_is_preview(&self, page: usize) -> bool {
        self.page_textures.get(&page).map(|c| c.is_preview).unwrap_or(true)
    }
}
