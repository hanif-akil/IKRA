use egui::{Color32, Pos2};
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Annot {
    Pen(PenStroke),
    Shape(ShapeAnnot),
    TextBox(TextBox),
    Note(StickyNote),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PageAnnotations {
    pub items: Vec<Annot>,
}

pub struct AnnotationState {
    pub pages: Vec<PageAnnotations>,
    pub undo_stack: Vec<Vec<PageAnnotations>>,
    pub redo_stack: Vec<Vec<PageAnnotations>>,
}

impl AnnotationState {
    pub fn new(page_count: usize) -> Self {
        Self {
            pages: vec![PageAnnotations::default(); page_count],
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
        }
    }

    pub fn insert_page(&mut self, at: usize) {
        self.pages.insert(at, PageAnnotations::default());
    }

    pub fn add_annot(&mut self, page: usize, annot: Annot) {
        if page < self.pages.len() {
            self.pages[page].items.push(annot);
        }
    }

    pub fn erase_at(&mut self, page: usize, pos: Pos2, radius: f32) {
        if page >= self.pages.len() { return; }
        self.pages[page].items.retain(|annot| {
            match annot {
                Annot::Pen(s) => {
                    !s.points.iter().any(|p| {
                        let dx = p.pos[0] - pos.x;
                        let dy = p.pos[1] - pos.y;
                        (dx * dx + dy * dy).sqrt() < radius
                    })
                }
                _ => true,
            }
        });
    }
}

pub fn color32_to_arr(c: Color32) -> [u8; 4] {
    [c.r(), c.g(), c.b(), c.a()]
}

pub fn arr_to_color32(arr: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(arr[0], arr[1], arr[2], arr[3])
}