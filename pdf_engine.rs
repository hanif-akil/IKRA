use pdfium_render::prelude::*;
use image::DynamicImage;
use crate::annotations::*;

pub struct PdfEngine {
    pdfium: Pdfium,
    pub doc:          Option<PdfDocument<'static>>,
    pub current_path: Option<String>,
    pub page_count:   usize,
    pub has_text:     Vec<bool>,
    pub file_size:    u64,
}

impl PdfEngine {
    pub fn new() -> Result<Self, String> {
        let bindings = Self::find_bindings()?;
        Ok(Self { pdfium: Pdfium::new(bindings), doc: None,
            current_path: None, page_count: 0, has_text: Vec::new(), file_size: 0 })
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

    pub fn open(&mut self, path: &str) -> Result<(), String> {
        self.doc = None;
        let doc_nat = self.pdfium.load_pdf_from_file(path, None)
            .map_err(|e| e.to_string())?;
        let doc: PdfDocument<'static> = unsafe { std::mem::transmute(doc_nat) };
        self.page_count   = doc.pages().len() as usize;
        self.has_text     = vec![true; self.page_count];
        self.doc          = Some(doc);
        self.current_path = Some(path.to_string());
        self.file_size    = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(())
    }

    pub fn render_page(&self, page_index: usize, scale: f32) -> Option<DynamicImage> {
        let doc  = self.doc.as_ref()?;
        let page = doc.pages().get(page_index as u16).ok()?;
        
        // Capping resolution to prevent excessive memory usage.
        // 3000px is generally plenty for editing and prevents RAM explosion.
        let mut final_scale = scale;
        let w = page.width().value;
        let h = page.height().value;
        if w > 0.0 && h > 0.0 {
            let max_dim = w.max(h) * scale;
            if max_dim > 3000.0 {
                final_scale = 3000.0 / w.max(h);
            }
        }

        let cfg  = PdfRenderConfig::new()
            .set_clear_color(PdfColor::WHITE)
            .scale_page_by_factor(final_scale);
        
        // Resolve the bitmap before page drops
        let bitmap = page.render_with_config(&cfg).ok()?;
        let img = bitmap.as_image();
        drop(bitmap); // explicitly drop to free pdfium resources ASAP
        Some(img)
    }

    pub fn render_thumb(&self, page_index: usize) -> Option<DynamicImage> {
        self.render_page(page_index, 0.2)
    }

    pub fn page_size(&self, page_index: usize) -> Option<(f32, f32)> {
        let doc  = self.doc.as_ref()?;
        let page = doc.pages().get(page_index as u16).ok()?;
        Some((page.width().value, page.height().value))
    }

    pub fn search_text(&self, query: &str) -> Vec<usize> {
        let mut results = Vec::new();
        if query.trim().is_empty() { return results; }
        let q = query.to_lowercase();
        if let Some(doc) = &self.doc {
            for (i, page) in doc.pages().iter().enumerate() {
                if let Ok(text) = page.text() {
                    if text.all().to_lowercase().contains(&q) { results.push(i); }
                }
            }
        }
        results
    }

    pub fn add_blank_page(&mut self, after_index: usize, width: f32, height: f32)
        -> Result<(), String>
    {
        let doc = self.doc.as_mut().ok_or("No document open")?;
        let insert_at = (after_index + 1).min(doc.pages().len() as usize) as u16;
        let size = PdfPagePaperSize::Custom(PdfPoints::new(width), PdfPoints::new(height));
        doc.pages_mut().create_page_at_index(size, insert_at)
            .map_err(|e| e.to_string())?;
        self.page_count = doc.pages().len() as usize;
        self.has_text.insert(insert_at as usize, false);
        Ok(())
    }

    /// Burn all egui annotations into the PDF as native objects, then save.
    pub fn save_with_annotations(
        &mut self,
        path:   &str,
        annots: &[PageAnnotations],
    ) -> Result<(), String> {
        {
            let doc = self.doc.as_mut().ok_or("No document open")?;
            Self::burn_annotations(doc, annots)?;
        }
        let doc = self.doc.as_ref().ok_or("No document open")?;
        doc.save_to_file(path).map_err(|e| e.to_string())
    }

    /// Draw each annotation onto its PDF page using pdfium-render 0.8 API.
    fn burn_annotations(
        doc:    &mut PdfDocument<'static>,
        annots: &[PageAnnotations],
    ) -> Result<(), String> {
        // Pre-fetch helvetica token — it's Copy/Clone-able
        let helvetica = doc.fonts_mut().helvetica();

        for (page_idx, page_annots) in annots.iter().enumerate() {
            if page_annots.items.is_empty() { continue; }
            let pi = page_idx as u16;

            let page_height = {
                let page = match doc.pages().get(pi) { Ok(p) => p, Err(_) => continue };
                page.height().value
            };
            let flip = |y: f32| page_height - y;

            let mut page = match doc.pages_mut().get(pi) { Ok(p) => p, Err(_) => continue };

            for annot in &page_annots.items {
                match annot {
                    // ── Pen / Highlight stroke ─────────────────────────────
                    Annot::Pen(s) if s.points.len() >= 2 => {
                        let c = arr_to_color32(s.color);
                        let (stroke_color, stroke_w) = match s.kind {
                            AnnotKind::Highlight =>
                                (PdfColor::new(c.r(), c.g(), c.b(), 100), s.width * 8.0),
                            _ =>
                                (PdfColor::new(c.r(), c.g(), c.b(), c.a()), s.width),
                        };

                        // We build the stroke as a series of line segments
                        let pts = &s.points;
                        for i in 1..pts.len() {
                            let x1 = pts[i-1].pos[0]; let y1 = flip(pts[i-1].pos[1]);
                            let x2 = pts[i  ].pos[0]; let y2 = flip(pts[i  ].pos[1]);
                            let _ = page.objects_mut().create_path_object_line(
                                PdfPoints::new(x1), PdfPoints::new(y1),
                                PdfPoints::new(x2), PdfPoints::new(y2),
                                stroke_color,
                                PdfPoints::new(stroke_w),
                            );
                        }
                    }

                    // ── Shapes ─────────────────────────────────────────────
                    Annot::Shape(s) => {
                        let c  = arr_to_color32(s.color);
                        let pc = PdfColor::new(c.r(), c.g(), c.b(), c.a());
                        let x1 = s.start[0]; let y1 = flip(s.start[1]);
                        let x2 = s.end[0];   let y2 = flip(s.end[1]);
                        let sw = PdfPoints::new(s.width);

                        match s.kind {
                            ShapeKind::Rect => {
                                let (l, r) = (x1.min(x2), x1.max(x2));
                                let (b, t) = (y1.min(y2), y1.max(y2));
                                // Four sides
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(l), PdfPoints::new(b),
                                    PdfPoints::new(r), PdfPoints::new(b), pc, sw);
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(r), PdfPoints::new(b),
                                    PdfPoints::new(r), PdfPoints::new(t), pc, sw);
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(r), PdfPoints::new(t),
                                    PdfPoints::new(l), PdfPoints::new(t), pc, sw);
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(l), PdfPoints::new(t),
                                    PdfPoints::new(l), PdfPoints::new(b), pc, sw);
                            }
                            ShapeKind::Ellipse => {
                                let cx = (x1+x2)/2.0; let cy = (y1+y2)/2.0;
                                let rx = (x2-x1).abs()/2.0; let ry = (y2-y1).abs()/2.0;
                                let n = 32usize;
                                for i in 0..n {
                                    let a1 = i as f32       * std::f32::consts::TAU / n as f32;
                                    let a2 = (i+1) as f32   * std::f32::consts::TAU / n as f32;
                                    let _ = page.objects_mut().create_path_object_line(
                                        PdfPoints::new(cx + rx*a1.cos()),
                                        PdfPoints::new(cy + ry*a1.sin()),
                                        PdfPoints::new(cx + rx*a2.cos()),
                                        PdfPoints::new(cy + ry*a2.sin()),
                                        pc, sw);
                                }
                            }
                            ShapeKind::Arrow => {
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(x1), PdfPoints::new(y1),
                                    PdfPoints::new(x2), PdfPoints::new(y2), pc, sw);
                                // Arrowhead
                                let dx = x2-x1; let dy = y2-y1;
                                let len = (dx*dx+dy*dy).sqrt().max(0.001);
                                let (nx,ny) = (dx/len, dy/len);
                                let (px,py) = (-ny, nx);
                                let (hl,hw) = (10.0_f32, 5.0_f32);
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(x2), PdfPoints::new(y2),
                                    PdfPoints::new(x2-nx*hl+px*hw), PdfPoints::new(y2-ny*hl+py*hw),
                                    pc, sw);
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(x2), PdfPoints::new(y2),
                                    PdfPoints::new(x2-nx*hl-px*hw), PdfPoints::new(y2-ny*hl-py*hw),
                                    pc, sw);
                            }
                            ShapeKind::Line => {
                                let _ = page.objects_mut().create_path_object_line(
                                    PdfPoints::new(x1), PdfPoints::new(y1),
                                    PdfPoints::new(x2), PdfPoints::new(y2), pc, sw);
                            }
                        }
                    }

                    // ── Text box ───────────────────────────────────────────
                    Annot::TextBox(t) => {
                        let c = arr_to_color32(t.color);
                        let _ = page.objects_mut().create_text_object(
                            PdfPoints::new(t.pos[0]),
                            PdfPoints::new(flip(t.pos[1])),
                            &t.text,
                            helvetica,
                            PdfPoints::new(t.font_size),
                        ).map(|mut obj| {
                            let _ = obj.set_fill_color(PdfColor::new(c.r(), c.g(), c.b(), c.a()));
                        });
                    }

                    // ── Sticky note ────────────────────────────────────────
                    Annot::Note(n) => {
                        let x = n.pos[0]; let y = flip(n.pos[1]);
                        let yellow = PdfColor::new(255, 220, 50, 255);
                        let border = PdfColor::new(160, 120, 0, 255);
                        let bw = PdfPoints::new(0.8);
                        // Draw note icon as a small filled square (4 border lines)
                        let s = 12.0_f32;
                        let _ = page.objects_mut().create_path_object_line(
                            PdfPoints::new(x),   PdfPoints::new(y),
                            PdfPoints::new(x+s), PdfPoints::new(y),   yellow, bw);
                        let _ = page.objects_mut().create_path_object_line(
                            PdfPoints::new(x+s), PdfPoints::new(y),
                            PdfPoints::new(x+s), PdfPoints::new(y-s), yellow, bw);
                        let _ = page.objects_mut().create_path_object_line(
                            PdfPoints::new(x+s), PdfPoints::new(y-s),
                            PdfPoints::new(x),   PdfPoints::new(y-s), yellow, bw);
                        let _ = page.objects_mut().create_path_object_line(
                            PdfPoints::new(x),   PdfPoints::new(y-s),
                            PdfPoints::new(x),   PdfPoints::new(y),   yellow, bw);
                        // Note text
                        if !n.text.is_empty() {
                            let preview: String = n.text.chars().take(60).collect();
                            let _ = page.objects_mut().create_text_object(
                                PdfPoints::new(x + 14.0),
                                PdfPoints::new(y - 4.0),
                                &preview,
                                helvetica,
                                PdfPoints::new(8.0),
                            ).map(|mut obj| {
                                let _ = obj.set_fill_color(border);
                            });
                        }
                    }

                    _ => {}
                }
            }
        }
        Ok(())
    }
}
