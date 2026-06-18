//! `text_index.rs` — Independent text extraction and search index.
//!
//! The `TextIndex` manages the per-document, per-page `TextMap` lifecycle
//! **independently** from the raster render pipeline. Text extraction is
//! dispatched immediately on document open (via rayon), so word search is
//! available long before all pages finish rasterizing.
//!
//! ## Invariants
//! - `TextIndex` is **never** serialized.  `.ikra.json` sidecars only contain
//!   `AnnotationLayer` data; the text layer is always re-extracted from the
//!   live PDF on open.
//! - `TextMap` entries are stored in **egui / top-left-origin** coordinates,
//!   matching the coordinate system used by `LayeredPageView`.

use std::collections::HashMap;
use uuid::Uuid;
use crate::annotations::TextMap;

/// Per-document search index.
///
/// Populated asynchronously by `WorkerResponse::TextExtracted`.
/// Queries are safe to call before the index is ready — they return empty results.
pub struct TextIndex {
    maps: HashMap<Uuid, Vec<TextMap>>,
}

impl TextIndex {
    pub fn new() -> Self {
        Self { maps: HashMap::new() }
    }

    /// Store the freshly extracted text maps for a document.
    /// Replaces any previous data for the same `doc_id`.
    pub fn insert(&mut self, doc_id: Uuid, pages: Vec<TextMap>) {
        self.maps.insert(doc_id, pages);
    }

    /// Remove all index data for a closed document.
    pub fn remove(&mut self, doc_id: &Uuid) {
        self.maps.remove(doc_id);
    }

    /// Returns the `TextMap` for `page` within `doc_id`, if extraction has completed.
    pub fn get_page(&self, doc_id: &Uuid, page: usize) -> Option<&TextMap> {
        self.maps.get(doc_id)?.get(page)
    }

    /// Search all pages of `doc_id` for `query`.
    /// Returns `(page_index, rect_in_egui_pdf_space)` pairs.
    /// Returns an empty `Vec` if extraction has not yet completed.
    pub fn find_text(&self, doc_id: &Uuid, query: &str) -> Vec<(usize, egui::Rect)> {
        let Some(pages) = self.maps.get(doc_id) else {
            return Vec::new();
        };
        pages
            .iter()
            .enumerate()
            .flat_map(|(i, tm)| {
                tm.find_text(query).into_iter().map(move |r| (i, r))
            })
            .collect()
    }

    /// Returns `true` if text extraction has completed for `doc_id`.
    pub fn is_ready(&self, doc_id: &Uuid) -> bool {
        self.maps.contains_key(doc_id)
    }
}

impl Default for TextIndex {
    fn default() -> Self { Self::new() }
}
