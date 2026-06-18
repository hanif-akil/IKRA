use pdfium_render::prelude::*;
use image::DynamicImage;
use std::collections::{BinaryHeap, HashMap};
use std::cmp::Ordering;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use egui::{Pos2, Rect};
use uuid::Uuid;
use crate::annotations::{TextMap, arr_to_color32};
use crate::annotations::*;
use crate::bookmarks::{PdfNativeBookmark, PdfOutlineEntry};

// ── Two-pass scale constants ──────────────────────────────────────────────────

/// Scale factor for the instant low-res preview pass (~25 DPI for A4).
/// Renders in <50 ms per page; shows content immediately on open.
pub const LOWRES_SCALE: f32 = 0.35;

// ── Thread-safe document wrapper ─────────────────────────────────────────────

/// Newtype wrapper that makes PdfDocument usable across rayon threads.
/// SAFETY: pdfium-render is compiled with the `thread_safe` feature which
/// enables `FPDF_InitLibraryWithConfig` and all internal C-level locking.
struct SendableDoc(PdfDocument<'static>);
unsafe impl Send for SendableDoc {}
unsafe impl Sync for SendableDoc {} // only behind Mutex, never aliased

// ── Protocol ─────────────────────────────────────────────────────────────────

pub enum WorkerRequest {
    Load(Uuid, String),
    /// Enqueue a page render with explicit priority.
    /// `priority`: 0 = urgent (visible), 1 = prefetch, 2 = thumbnails
    Render {
        id:       Uuid,
        page:     usize,
        scale:    f32,
        kind:     RenderKind,
        priority: u8,
    },
    Save(Uuid, String, Vec<PageAnnotations>, Vec<PdfNativeBookmark>),
    Close(Uuid),
    AddBlankPage(Uuid),
    /// Cancel all queued LowRes/HighRes page renders for a document.
    /// Thumb renders are preserved.  Use after scale changes.
    CancelPageRenders(Uuid),
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum RenderKind {
    /// Quick low-resolution (LOWRES_SCALE) preview.
    LowRes,
    /// Full-quality render at the current viewport scale.
    HighRes,
    /// Fixed-scale thumbnail for the sidebar (0.2).
    Thumb,
}

pub enum WorkerResponse {
    Loaded(Uuid, String, usize, Vec<(f32, f32)>, Result<(), String>),
    /// Text bounding-boxes extracted from the PDF — arrives shortly after Loaded.
    /// The UI should populate both `TextIndex` and `DocumentState.pages[i].text_map`
    /// from this payload so search works immediately, independent of rasterization.
    TextExtracted(Uuid, Vec<TextMap>),
    /// Outline (internal PDF bookmarks/TOC) extracted from the PDF.
    /// Arrives shortly after Loaded via a separate rayon task.
    OutlineExtracted(Uuid, Vec<PdfOutlineEntry>),
    Rendered(Uuid, usize, f32, RenderKind, DynamicImage),
    Saved(Uuid, Result<(), String>),
    PageAdded(Uuid, Result<(usize, f32, f32), String>),
}

// ── Internal priority-queue job ───────────────────────────────────────────────

struct RenderJob {
    priority: u8,
    seq:      u64,
    id:       Uuid,
    page:     usize,
    scale:    f32,
    kind:     RenderKind,
    doc_arc:  Arc<Mutex<SendableDoc>>,
}

impl Eq for RenderJob {}
impl PartialEq for RenderJob {
    fn eq(&self, o: &Self) -> bool { self.priority == o.priority && self.seq == o.seq }
}
impl Ord for RenderJob {
    fn cmp(&self, o: &Self) -> Ordering {
        // BinaryHeap is a max-heap → lower priority number = higher weight
        o.priority.cmp(&self.priority)
            .then(o.seq.cmp(&self.seq)) // FIFO within same priority
    }
}
impl PartialOrd for RenderJob {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) }
}

// ── PdfEngine ─────────────────────────────────────────────────────────────────

pub struct PdfEngine {
    pdfium: Pdfium,
    pub docs: HashMap<Uuid, (Arc<Mutex<SendableDoc>>, String)>,
}

pub fn is_pdf_file(path: &str) -> bool {
    use std::io::Read;
    let mut f = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return false };
    let mut h = [0u8; 5];
    if f.read_exact(&mut h).is_err() { return false; }
    h == *b"%PDF-"
}

impl PdfEngine {
    pub fn new() -> Result<Self, String> {
        let bindings = Self::find_bindings()?;
        Ok(Self { pdfium: Pdfium::new(bindings), docs: HashMap::new() })
    }

    // ── Worker thread ─────────────────────────────────────────────────────────

    pub fn start_worker(rx: Receiver<WorkerRequest>, tx: Sender<WorkerResponse>) {
        std::thread::spawn(move || {
            let mut engine = match PdfEngine::new() {
                Ok(e) => e,
                Err(e) => { eprintln!("Fatal: PDFium init failed: {}", e); return; }
            };

            let mut heap: BinaryHeap<RenderJob> = BinaryHeap::new();
            let mut seq:  u64 = 0;

            loop {
                // ── Phase 1: Drain all pending channel messages ───────────────
                loop {
                    match rx.try_recv() {
                        Ok(req) => engine.handle_request(req, &tx, &mut heap, &mut seq),
                        Err(TryRecvError::Empty)        => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }

                // ── Phase 2: Render a single page if available ────────────────
                if let Some(job) = heap.pop() {
                    if let Some(img) = PdfEngine::render_doc(
                        &job.doc_arc, job.page, job.scale, &job.kind,
                    ) {
                        let _ = tx.send(WorkerResponse::Rendered(
                            job.id, job.page, job.scale, job.kind, img,
                        ));
                    }
                }

                // ── Phase 3: Wait strategy ────────────────────────────────────
                if heap.is_empty() {
                    // Nothing queued — block efficiently until next message
                    match rx.recv() {
                        Ok(req) => engine.handle_request(req, &tx, &mut heap, &mut seq),
                        Err(_) => return,
                    }
                }
            }
        });
    }

    fn handle_request(
        &mut self,
        req:  WorkerRequest,
        tx:   &Sender<WorkerResponse>,
        heap: &mut BinaryHeap<RenderJob>,
        seq:  &mut u64,
    ) {
        match req {
            // ── Load ─────────────────────────────────────────────────────────
            WorkerRequest::Load(id, path) => {
                let resp = self.load_doc(id, &path);
                let _ = tx.send(resp);

                // Extract text maps on the worker thread.
                if let Some((doc_arc, _)) = self.docs.get(&id) {
                    let maps = PdfEngine::extract_text_maps(doc_arc);
                    let _ = tx.send(WorkerResponse::TextExtracted(id, maps));
                }

                // Extract PDF outline on the worker thread.
                if let Some((_, file_path)) = self.docs.get(&id) {
                    let outline = PdfEngine::extract_outline(&file_path);
                    let _ = tx.send(WorkerResponse::OutlineExtracted(id, outline));
                }
            }

            // ── Render ───────────────────────────────────────────────────────
            WorkerRequest::Render { id, page, scale, kind, priority } => {
                if let Some((doc_arc, _)) = self.docs.get(&id) {
                    *seq += 1;
                    heap.push(RenderJob {
                        priority, seq: *seq,
                        id, page, scale, kind,
                        doc_arc: Arc::clone(doc_arc),
                    });
                }
            }

            // ── Save ─────────────────────────────────────────────────────────
            WorkerRequest::Save(id, path, annots, native_bms) => {
                let res = self.save_doc(id, &path, &annots, &native_bms);
                let _ = tx.send(WorkerResponse::Saved(id, res));
            }

            // ── Close ────────────────────────────────────────────────────────
            WorkerRequest::Close(id) => {
                if let Some((doc_arc, _)) = self.docs.remove(&id) {
                    drop(doc_arc);  // Force Arc release
                }
                // Purge all pending jobs for this document from the priority heap
                let remaining: Vec<RenderJob> = heap.drain().collect();
                *heap = remaining.into_iter().filter(|j| j.id != id).collect();
            }

            // ── Add blank page ────────────────────────────────────────────────
            WorkerRequest::AddBlankPage(id) => {
                let res = self.add_blank_page(id);
                let _ = tx.send(res);
            }

            // ── Cancel page renders ───────────────────────────────────────────
            WorkerRequest::CancelPageRenders(id) => {
                // Keep Thumb renders (sidebar stale-ness is acceptable);
                // discard LowRes and HighRes (they'll be re-queued at new scale)
                let remaining: Vec<RenderJob> = heap.drain().collect();
                *heap = remaining
                    .into_iter()
                    .filter(|j| !(j.id == id && j.kind != RenderKind::Thumb))
                    .collect();
            }
        }
    }

    // ── Document operations ───────────────────────────────────────────────────

    fn load_doc(&mut self, id: Uuid, path: &str) -> WorkerResponse {
        match self.pdfium.load_pdf_from_file(path, None) {
            Ok(doc_nat) => {
                // SAFETY: the document is immediately wrapped in Arc<Mutex<SendableDoc>>.
                // pdfium-render + thread_safe feature guarantees C-level locking.
                let doc: PdfDocument<'static> = unsafe { std::mem::transmute(doc_nat) };
                let page_count = doc.pages().len() as usize;
                let mut sizes  = Vec::with_capacity(page_count);
                for i in 0..page_count {
                    match doc.pages().get(i as u16) {
                        Ok(p)  => sizes.push((p.width().value, p.height().value)),
                        Err(_) => sizes.push((595.0, 842.0)),
                    }
                }
                self.docs.insert(id, (Arc::new(Mutex::new(SendableDoc(doc))), path.to_string()));
                WorkerResponse::Loaded(id, path.to_string(), page_count, sizes, Ok(()))
            }
            Err(e) => WorkerResponse::Loaded(id, path.to_string(), 0, vec![], Err(e.to_string())),
        }
    }

    /// Render a single page.  Called from rayon worker threads.
    fn render_doc(
        doc_arc:    &Arc<Mutex<SendableDoc>>,
        page_index: usize,
        scale:      f32,
        kind:       &RenderKind,
    ) -> Option<DynamicImage> {
        let guard = doc_arc.lock().ok()?;
        let page  = guard.0.pages().get(page_index as u16).ok()?;

        let final_scale = match kind {
            RenderKind::Thumb   => 0.2_f32,
            RenderKind::LowRes  => scale.min(LOWRES_SCALE), // hard cap for speed

            RenderKind::HighRes => {
                // Cap at 3 000 px max to avoid OOM on huge pages
                let w = page.width().value;
                let h = page.height().value;
                if w > 0.0 && h > 0.0 {
                    let max_px = w.max(h) * scale;
                    if max_px > 3000.0 { 3000.0 / w.max(h) } else { scale }
                } else {
                    scale
                }
            }
        };

        let cfg    = PdfRenderConfig::new()
            .set_clear_color(PdfColor::WHITE)
            .scale_page_by_factor(final_scale);
        let bitmap = page.render_with_config(&cfg).ok()?;
        let img    = bitmap.as_image();
        drop(bitmap);
        Some(img)
    }

    /// Extract word-level text bounding boxes for every page.
    ///
    /// The Mutex is **released between pages** so concurrent page renders
    /// (LowRes first pass) are not serialized behind extraction.
    ///
    /// Coordinates are converted from PDF space (bottom-left origin) to
    /// egui space (top-left origin) during extraction.
    fn extract_text_maps(doc_arc: &Arc<Mutex<SendableDoc>>) -> Vec<TextMap> {
        // First pass: get page count without holding the lock
        let page_count = {
            let g = match doc_arc.lock() { Ok(g) => g, Err(_) => return Vec::new() };
            g.0.pages().len() as usize
        };

        let mut maps = Vec::with_capacity(page_count);

        for i in 0..page_count {
            let mut text_map = TextMap::default();

            // Acquire → extract one page → release
            if let Ok(guard) = doc_arc.lock() {
                if let Ok(page) = guard.0.pages().get(i as u16) {
                    let page_height = page.height().value;

                    if let Ok(text_page) = page.text() {
                        let chars = text_page.chars();

                        let mut current_word = String::new();
                        let mut w_left   = f32::MAX;
                        let mut w_bottom = f32::MAX;
                        let mut w_right  = f32::MIN;
                        let mut w_top    = f32::MIN;

                        for ch in chars.iter() {
                            let unicode = ch.unicode_char();
                            let is_ws   = unicode
                                .map(|c| c.is_whitespace() || c == '\n' || c == '\r')
                                .unwrap_or(true);

                            if is_ws {
                                // Flush current word
                                if !current_word.is_empty()
                                    && w_left < w_right
                                    && w_bottom < w_top
                                {
                                    // PDF bottom-left → egui top-left
                                    let rect = Rect::from_min_max(
                                        Pos2::new(w_left,  page_height - w_top),
                                        Pos2::new(w_right, page_height - w_bottom),
                                    );
                                    text_map.insert(rect, current_word.clone());
                                }
                                current_word.clear();
                                w_left   = f32::MAX; w_bottom = f32::MAX;
                                w_right  = f32::MIN; w_top    = f32::MIN;
                            } else {
                                if let Some(c) = unicode { current_word.push(c); }
                                if let Ok(b) = ch.loose_bounds() {
                                    w_left   = w_left.min(b.left.value);
                                    w_bottom = w_bottom.min(b.bottom.value);
                                    w_right  = w_right.max(b.right.value);
                                    w_top    = w_top.max(b.top.value);
                                }
                            }
                        }

                        // Flush last word
                        if !current_word.is_empty() && w_left < w_right && w_bottom < w_top {
                            let rect = Rect::from_min_max(
                                Pos2::new(w_left,  page_height - w_top),
                                Pos2::new(w_right, page_height - w_bottom),
                            );
                            text_map.insert(rect, current_word);
                        }
                    }
                }
            } // Mutex released here between pages

            maps.push(text_map);
        }
        maps
    }

    fn save_doc(
        &mut self,
        id:         Uuid,
        path:       &str,
        annots:     &[PageAnnotations],
        native_bms: &[PdfNativeBookmark],
    ) -> Result<(), String> {
        let (doc_arc, _) = self.docs.get_mut(&id).ok_or("Doc not found")?;
        let mut guard    = doc_arc.lock().map_err(|e| e.to_string())?;
        Self::burn_annotations(&mut guard.0, annots)?;
        guard.0.save_to_file(path).map_err(|e| e.to_string())?;
        drop(guard); // release pdfium lock before lopdf opens the same file

        if !native_bms.is_empty() {
            // ponytail: lopdf round-trip is ~ms on typical files; upgrade path
            // is async if files grow large enough to block the worker thread.
            if let Err(e) = Self::burn_outline(path, native_bms) {
                // Non-fatal: annotations are already saved; outline is cosmetic.
                eprintln!("burn_outline failed (non-fatal): {}", e);
            }
        }
        Ok(())
    }

    /// Bake `native_bms` into the PDF's /Outlines catalog using lopdf.
    /// Opens the file independently (read-then-write), so it never conflicts
    /// with the pdfium Mutex.  The existing outline (if any) is replaced.
    fn burn_outline(path: &str, native_bms: &[PdfNativeBookmark]) -> Result<(), String> {
        let mut doc = lopdf::Document::load(path).map_err(|e| e.to_string())?;

        // Build a page-number → lopdf ObjectId map (1-based page_num from get_pages)
        let page_id_map: HashMap<usize, lopdf::ObjectId> = doc
            .get_pages()
            .into_iter()
            .map(|(page_num, obj_id)| ((page_num as usize).saturating_sub(1), obj_id))
            .collect();

        // Recursively add lopdf::Bookmark nodes; returns the root bookmark id.
        fn add_recursive(
            doc:         &mut lopdf::Document,
            bms:         &[PdfNativeBookmark],
            page_id_map: &HashMap<usize, lopdf::ObjectId>,
            parent_id:   Option<u32>,
        ) {
            for bm in bms {
                let page_obj_id = match page_id_map.get(&bm.target_page) {
                    Some(id) => *id,
                    None     => continue, // skip if page index out of range
                };
                let lopdf_bm = lopdf::Bookmark::new(
                    bm.title.clone(),
                    [0.0, 0.0, 0.0],
                    0,
                    page_obj_id,
                );
                let bm_id = doc.add_bookmark(lopdf_bm, parent_id);
                add_recursive(doc, &bm.children, page_id_map, Some(bm_id));
            }
        }

        add_recursive(&mut doc, native_bms, &page_id_map, None);
        doc.adjust_zero_pages();

        if let Some(new_outline_id) = doc.build_outline() {
            let old_outline_id_opt = doc.catalog().ok()
                .and_then(|cat| cat.get(b"Outlines").ok())
                .and_then(|obj| obj.as_reference().ok());

            if let Some(old_outline_id) = old_outline_id_opt {
                let mut merged = false;
                if let Ok(new_dict) = doc.get_dictionary(new_outline_id).cloned() {
                    if let (Ok(lopdf::Object::Reference(new_first)), Ok(lopdf::Object::Reference(new_last))) = 
                        (new_dict.get(b"First"), new_dict.get(b"Last")) 
                    {
                        let new_first = *new_first;
                        let new_last = *new_last;
                        let new_count = new_dict.get(b"Count").ok().and_then(|c| c.as_i64().ok()).unwrap_or(0);
                        
                        let (old_last_opt, old_count) = if let Ok(old_dict) = doc.get_dictionary(old_outline_id) {
                            (old_dict.get(b"Last").ok().and_then(|obj| obj.as_reference().ok()),
                             old_dict.get(b"Count").ok().and_then(|c| c.as_i64().ok()).unwrap_or(0))
                        } else {
                            (None, 0)
                        };

                        if let Some(old_last) = old_last_opt {
                            if let Ok(old_last_dict) = doc.get_dictionary_mut(old_last) {
                                old_last_dict.set(b"Next", lopdf::Object::Reference(new_first));
                            }
                            if let Ok(new_first_dict) = doc.get_dictionary_mut(new_first) {
                                new_first_dict.set(b"Prev", lopdf::Object::Reference(old_last));
                            }
                            if let Ok(old_outlines_dict) = doc.get_dictionary_mut(old_outline_id) {
                                old_outlines_dict.set(b"Last", lopdf::Object::Reference(new_last));
                                if old_count != 0 || new_count != 0 {
                                    old_outlines_dict.set(b"Count", old_count + new_count);
                                }
                            }

                            // Update Parent for all top-level new items
                            let mut curr = Some(new_first);
                            while let Some(c) = curr {
                                if let Ok(dict) = doc.get_dictionary_mut(c) {
                                    dict.set(b"Parent", lopdf::Object::Reference(old_outline_id));
                                    curr = dict.get(b"Next").ok().and_then(|obj| obj.as_reference().ok());
                                } else {
                                    break;
                                }
                            }

                            let _ = doc.remove_object(&new_outline_id);
                            merged = true;
                        }
                    }
                }
                
                if !merged {
                    if let Ok(catalog) = doc.catalog_mut() {
                        catalog.set(b"Outlines", lopdf::Object::Reference(new_outline_id));
                    }
                }
            } else {
                if let Ok(catalog) = doc.catalog_mut() {
                    catalog.set(b"Outlines", lopdf::Object::Reference(new_outline_id));
                }
            }
        }

        doc.save(path).map(|_| ()).map_err(|e| e.to_string())
    }

    fn add_blank_page(&mut self, id: Uuid) -> WorkerResponse {
        const W: f32 = 595.28;
        const H: f32 = 841.89;
        match self.docs.get_mut(&id) {
            Some((doc_arc, _)) => match doc_arc.lock() {
                Ok(mut g) => match g.0.pages_mut().create_page_at_end(PdfPagePaperSize::a4()) {
                    Ok(_)  => WorkerResponse::PageAdded(id, Ok((g.0.pages().len() as usize, W, H))),
                    Err(e) => WorkerResponse::PageAdded(id, Err(e.to_string())),
                },
                Err(_) => WorkerResponse::PageAdded(id, Err("Lock error".into())),
            },
            None => WorkerResponse::PageAdded(id, Err("Document not found".into())),
        }
    }

    // ── PDF outline (bookmark/TOC) extraction via lopdf ─────────────────────

    /// Extract the PDF's internal outline tree using `lopdf`.
    /// Opens the file independently (read-only) so it never conflicts with
    /// the pdfium Mutex.  Returns an empty Vec if the PDF has no outlines.
    pub fn extract_outline(path: &str) -> Vec<PdfOutlineEntry> {
        let doc = match lopdf::Document::load(path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        // Build a mapping: page object_id → 0-based page index.
        let page_id_to_index: HashMap<lopdf::ObjectId, usize> = doc
            .get_pages()
            .into_iter()
            .map(|(page_num, obj_id)| (obj_id, (page_num as usize).saturating_sub(1)))
            .collect();

        // Find the /Outlines dictionary in the catalog.
        let catalog = match doc.catalog() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let outlines_ref = match catalog.get(b"Outlines") {
            Ok(o) => o,
            Err(_) => return Vec::new(),
        };

        let outlines_id = match outlines_ref.as_reference() {
            Ok(id) => id,
            Err(_) => return Vec::new(),
        };

        let outlines = match doc.get_object(outlines_id) {
            Ok(lopdf::Object::Dictionary(d)) => d,
            _ => return Vec::new(),
        };

        // The root /Outlines dict has a /First child.
        let first_id = match outlines.get(b"First") {
            Ok(o) => match o.as_reference() {
                Ok(id) => id,
                Err(_) => return Vec::new(),
            },
            Err(_) => return Vec::new(),
        };

        Self::walk_outline_siblings(&doc, first_id, &page_id_to_index)
    }

    /// Recursively walk a chain of outline item siblings (/Next links)
    /// and their children (/First).
    fn walk_outline_siblings(
        doc: &lopdf::Document,
        first_id: lopdf::ObjectId,
        page_map: &HashMap<lopdf::ObjectId, usize>,
    ) -> Vec<PdfOutlineEntry> {
        let mut entries = Vec::new();
        let mut current_id = Some(first_id);

        while let Some(obj_id) = current_id {
            let dict = match doc.get_object(obj_id) {
                Ok(lopdf::Object::Dictionary(d)) => d,
                _ => break,
            };

            // ── Title ────────────────────────────────────────────────────
            let title = dict
                .get(b"Title")
                .ok()
                .and_then(|o| match o {
                    lopdf::Object::String(bytes, _) => {
                        // Try UTF-16BE (BOM = FE FF), otherwise Latin-1
                        if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
                            let u16s: Vec<u16> = bytes[2..]
                                .chunks_exact(2)
                                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                                .collect();
                            Some(String::from_utf16_lossy(&u16s))
                        } else {
                            Some(String::from_utf8_lossy(bytes).into_owned())
                        }
                    }
                    _ => None,
                })
                .unwrap_or_default();

            // ── Destination page ──────────────────────────────────────────
            let page = Self::resolve_outline_dest(doc, &dict, page_map);

            // ── Children ─────────────────────────────────────────────────
            let children = dict
                .get(b"First")
                .ok()
                .and_then(|o| o.as_reference().ok())
                .map(|child_id| Self::walk_outline_siblings(doc, child_id, page_map))
                .unwrap_or_default();

            entries.push(PdfOutlineEntry { title, page, children });

            // ── Next sibling ─────────────────────────────────────────────
            current_id = dict
                .get(b"Next")
                .ok()
                .and_then(|o| o.as_reference().ok());
        }

        entries
    }

    /// Resolve the destination page index from an outline item's /Dest or /A entry.
    fn resolve_outline_dest(
        doc: &lopdf::Document,
        dict: &lopdf::Dictionary,
        page_map: &HashMap<lopdf::ObjectId, usize>,
    ) -> Option<usize> {
        // Try /Dest first (explicit destination)
        if let Ok(dest) = dict.get(b"Dest") {
            return Self::page_from_dest(doc, dest, page_map);
        }

        // Try /A (action dictionary with /D destination)
        if let Ok(action_ref) = dict.get(b"A") {
            let action_dict = match action_ref {
                lopdf::Object::Dictionary(d) => d,
                lopdf::Object::Reference(id) => match doc.get_object(*id) {
                    Ok(lopdf::Object::Dictionary(d)) => d,
                    _ => return None,
                },
                _ => return None,
            };
            if let Ok(dest) = action_dict.get(b"D") {
                return Self::page_from_dest(doc, dest, page_map);
            }
        }

        None
    }

    /// Given a /Dest value (array or name/string), extract the page index.
    fn page_from_dest(
        doc: &lopdf::Document,
        dest: &lopdf::Object,
        page_map: &HashMap<lopdf::ObjectId, usize>,
    ) -> Option<usize> {
        match dest {
            // [pageRef /Fit ...] or [pageRef /XYZ x y z]
            lopdf::Object::Array(arr) if !arr.is_empty() => {
                let page_ref = match &arr[0] {
                    lopdf::Object::Reference(id) => *id,
                    _ => return None,
                };
                page_map.get(&page_ref).copied()
            }
            // Named destination — look up in /Dests or /Names
            lopdf::Object::Name(name) | lopdf::Object::String(name, _) => {
                Self::resolve_named_dest(doc, name, page_map)
            }
            lopdf::Object::Reference(id) => {
                if let Ok(resolved) = doc.get_object(*id) {
                    Self::page_from_dest(doc, resolved, page_map)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Resolve a named destination to a page index.
    fn resolve_named_dest(
        doc: &lopdf::Document,
        name: &[u8],
        page_map: &HashMap<lopdf::ObjectId, usize>,
    ) -> Option<usize> {
        // Try catalog /Dests dictionary (PDF 1.1 style)
        if let Ok(catalog) = doc.catalog() {
            if let Ok(dests_ref) = catalog.get(b"Dests") {
                let dests_dict = match dests_ref {
                    lopdf::Object::Dictionary(d) => Some(d),
                    lopdf::Object::Reference(id) => match doc.get_object(*id) {
                        Ok(lopdf::Object::Dictionary(d)) => Some(d),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(dd) = dests_dict {
                    if let Ok(dest) = dd.get(name) {
                        return Self::page_from_dest(doc, dest, page_map);
                    }
                }
            }
        }
        None
    }

    fn find_bindings() -> Result<Box<dyn PdfiumLibraryBindings>, String> {
        if let Ok(b) = Pdfium::bind_to_system_library() { return Ok(b); }
        let exe_dir = std::env::current_exe().ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));
        let dirs: Vec<std::path::PathBuf> = [
            Some(std::path::PathBuf::from(".")),
            Some(std::path::PathBuf::from("..")),
            exe_dir,
        ].into_iter().flatten().collect();
        for dir in &dirs {
            let name = Pdfium::pdfium_platform_library_name_at_path(dir.to_str().unwrap_or("."));
            if let Ok(b) = Pdfium::bind_to_library(&name) { return Ok(b); }
        }
        Err("pdfium library not found. Place it next to the executable.".into())
    }

    // ── Burn annotations into the PDF binary ─────────────────────────────────

    fn burn_annotations(
        doc:    &mut PdfDocument<'static>,
        annots: &[PageAnnotations],
    ) -> Result<(), String> {
        let helvetica = doc.fonts_mut().helvetica();

        for (page_idx, page_annots) in annots.iter().enumerate() {
            if page_annots.shapes.is_empty() { continue; }
            let pi = page_idx as u16;

            let page_height = {
                let page = match doc.pages().get(pi) { Ok(p) => p, Err(_) => continue };
                page.height().value
            };
            let flip = |y: f32| page_height - y;

            let mut page = match doc.pages_mut().get(pi) { Ok(p) => p, Err(_) => continue };

            for annot in &page_annots.shapes {
                match annot {
                    AnnotationShape::Pen(s) if s.points.len() >= 2 => {
                        let c = arr_to_color32(s.color);
                        let (sc, sw) = match s.kind {
                            AnnotKind::Highlight =>
                                (PdfColor::new(c.r(), c.g(), c.b(), 100), s.width * 8.0),
                            _ =>
                                (PdfColor::new(c.r(), c.g(), c.b(), c.a()), s.width),
                        };
                        for i in 1..s.points.len() {
                            let (x1, y1) = (s.points[i-1].pos[0], flip(s.points[i-1].pos[1]));
                            let (x2, y2) = (s.points[i  ].pos[0], flip(s.points[i  ].pos[1]));
                            let _ = page.objects_mut().create_path_object_line(
                                PdfPoints::new(x1), PdfPoints::new(y1),
                                PdfPoints::new(x2), PdfPoints::new(y2),
                                sc, PdfPoints::new(sw),
                            );
                        }
                    }

                    AnnotationShape::Shape(s) => {
                        let c  = arr_to_color32(s.color);
                        let pc = PdfColor::new(c.r(), c.g(), c.b(), c.a());
                        let x1 = s.start[0]; let y1 = flip(s.start[1]);
                        let x2 = s.end[0];   let y2 = flip(s.end[1]);
                        let sw = PdfPoints::new(s.width);
                        match s.kind {
                            ShapeKind::Rect => {
                                let (l, r) = (x1.min(x2), x1.max(x2));
                                let (b, t) = (y1.min(y2), y1.max(y2));
                                let _ = page.objects_mut().create_path_object_line(PdfPoints::new(l), PdfPoints::new(b), PdfPoints::new(r), PdfPoints::new(b), pc, sw);
                                let _ = page.objects_mut().create_path_object_line(PdfPoints::new(r), PdfPoints::new(b), PdfPoints::new(r), PdfPoints::new(t), pc, sw);
                                let _ = page.objects_mut().create_path_object_line(PdfPoints::new(r), PdfPoints::new(t), PdfPoints::new(l), PdfPoints::new(t), pc, sw);
                                let _ = page.objects_mut().create_path_object_line(PdfPoints::new(l), PdfPoints::new(t), PdfPoints::new(l), PdfPoints::new(b), pc, sw);
                            }
                            ShapeKind::Ellipse => {
                                let (cx, cy) = ((x1+x2)/2.0, (y1+y2)/2.0);
                                let (rx, ry) = ((x2-x1).abs()/2.0, (y2-y1).abs()/2.0);
                                for i in 0..32usize {
                                    let a1 = i       as f32 * std::f32::consts::TAU / 32.0;
                                    let a2 = (i + 1) as f32 * std::f32::consts::TAU / 32.0;
                                    let _ = page.objects_mut().create_path_object_line(
                                        PdfPoints::new(cx + rx*a1.cos()), PdfPoints::new(cy + ry*a1.sin()),
                                        PdfPoints::new(cx + rx*a2.cos()), PdfPoints::new(cy + ry*a2.sin()),
                                        pc, sw);
                                }
                            }
                            ShapeKind::Arrow | ShapeKind::Line => {
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(x1), PdfPoints::new(y1),
                                    PdfPoints::new(x2), PdfPoints::new(y2), pc, sw);
                            }
                        }
                    }

                    AnnotationShape::TextBox(t) => {
                        let c = arr_to_color32(t.color);
                        let _ = page.objects_mut().create_text_object(
                            PdfPoints::new(t.pos[0]), PdfPoints::new(flip(t.pos[1])),
                            &t.text, helvetica, PdfPoints::new(t.font_size),
                        ).map(|mut obj| {
                            let _ = obj.set_fill_color(PdfColor::new(c.r(), c.g(), c.b(), c.a()));
                        });
                    }

                    AnnotationShape::Note(n) => {
                        let (x, y) = (n.pos[0], flip(n.pos[1]));
                        let yellow = PdfColor::new(255, 220, 50, 255);
                        let border = PdfColor::new(160, 120,  0, 255);
                        let bw = PdfPoints::new(0.8);
                        let s  = 12.0_f32;
                        let _ = page.objects_mut().create_path_object_line(PdfPoints::new(x),   PdfPoints::new(y),   PdfPoints::new(x+s), PdfPoints::new(y),   yellow, bw);
                        let _ = page.objects_mut().create_path_object_line(PdfPoints::new(x+s), PdfPoints::new(y),   PdfPoints::new(x+s), PdfPoints::new(y-s), yellow, bw);
                        let _ = page.objects_mut().create_path_object_line(PdfPoints::new(x+s), PdfPoints::new(y-s), PdfPoints::new(x),   PdfPoints::new(y-s), yellow, bw);
                        let _ = page.objects_mut().create_path_object_line(PdfPoints::new(x),   PdfPoints::new(y-s), PdfPoints::new(x),   PdfPoints::new(y),   yellow, bw);
                        if !n.text.is_empty() {
                            let preview: String = n.text.chars().take(60).collect();
                            let _ = page.objects_mut().create_text_object(
                                PdfPoints::new(x + 14.0), PdfPoints::new(y - 4.0),
                                &preview, helvetica, PdfPoints::new(8.0),
                            ).map(|mut obj| { let _ = obj.set_fill_color(border); });
                        }
                    }

                    _ => {}
                }
            }
        }
        Ok(())
    }
}
