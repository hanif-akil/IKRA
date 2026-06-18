use egui::{Color32, RichText, Ui, Vec2, Stroke, Rounding, Response, Sense, Button};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use crate::tab::PdfTab;
use crate::pdf_engine::WorkerRequest;

// ── Tool enum ─────────────────────────────────────────────────────────────────
#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Tool {
    Cursor, Pen, Highlight, Eraser, Rect, Ellipse, Arrow, Line, TextBox, Note,
}

impl Tool {
    pub fn label(&self) -> &'static str {
        match self {
            Tool::Cursor    => "SELECT",
            Tool::Pen       => "PEN",
            Tool::Highlight => "MARK",
            Tool::Eraser    => "ERASE",
            Tool::Note      => "NOTE",
            Tool::TextBox   => "TEXT",
            Tool::Rect      => "RECT",
            Tool::Ellipse   => "OVAL",
            Tool::Arrow     => "ARROW",
            Tool::Line      => "LINE",
        }
    }
    pub fn icon(&self) -> &'static str {
        match self {
            Tool::Cursor    => "↖",
            Tool::Pen       => "✏",
            Tool::Highlight => "▐",
            Tool::Eraser    => "◫",
            Tool::Note      => "▣",
            Tool::TextBox   => "T",
            Tool::Rect      => "□",
            Tool::Ellipse   => "○",
            Tool::Arrow     => "→",
            Tool::Line      => "╱",
        }
    }
    
    pub fn icon_image(&self) -> egui::ImageSource<'static> {
        match self {
            Tool::Cursor    => egui::include_image!("../assets/cursor.svg"),
            Tool::Pen       => egui::include_image!("../assets/pencil-simple.svg"),
            Tool::Highlight => egui::include_image!("../assets/highlighter.svg"),
            Tool::Eraser    => egui::include_image!("../assets/eraser.svg"),
            Tool::Note      => egui::include_image!("../assets/note.svg"),
            Tool::TextBox   => egui::include_image!("../assets/cursor-text.svg"),
            Tool::Rect      => egui::include_image!("../assets/rectangle.svg"),
            Tool::Ellipse   => egui::include_image!("../assets/rectangle-dashed.svg"), // fallback 
            Tool::Arrow     => egui::include_image!("../assets/arrow-line-right.svg"),
            Tool::Line      => egui::include_image!("../assets/minus.svg"),
        }
    }
    /// Keyboard shortcut hint shown in tooltip
    pub fn shortcut<'a>(&self, state: &'a ToolState) -> &'a str {
        let key = match self {
            Tool::Cursor    => "Select",
            Tool::Pen       => "Pen",
            Tool::Highlight => "Highlight",
            Tool::Eraser    => "Eraser",
            Tool::Note      => "Note",
            Tool::TextBox   => "TextBox",
            Tool::Rect      => "Rect",
            Tool::Ellipse   => "Ellipse",
            Tool::Arrow     => "Arrow",
            Tool::Line      => "Line",
        };
        state.shortcuts.get(key).map(|s| s.as_str()).unwrap_or("")
    }
}

// ── Colour palette ────────────────────────────────────────────────────────────
pub const PALETTE: &[Color32] = &[
    Color32::from_rgb(255, 200,  50),  // yellow
    Color32::from_rgb(255, 140,   0),  // orange
    Color32::from_rgb( 34, 197,  94),  // green
    Color32::from_rgb( 59, 130, 246),  // blue
    Color32::from_rgb(168,  85, 247),  // purple
    Color32::from_rgb(239,  68,  68),  // red
    Color32::from_rgb(255, 255, 255),  // white
    Color32::BLACK,
];

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Theme {
    Dark,
    Light,
    Glassmorphism,
    Skeuomorphism,
}

// ── ToolState ─────────────────────────────────────────────────────────────────
pub struct ToolState {
    pub tool:       Tool,
    pub color:      Color32,
    pub brush_size: f32,
    pub opacity:    f32,
    pub settings_open: bool,
    pub shortcuts: HashMap<String, String>,
    pub theme:      Theme,
    pub binding_key: Option<String>, // Tracks which shortcut is currently being bound
}

impl Default for ToolState {
    fn default() -> Self {
        let mut shortcuts = HashMap::new();
        shortcuts.insert("Select".to_string(), "V".to_string());
        shortcuts.insert("Pen".to_string(), "P".to_string());
        shortcuts.insert("Highlight".to_string(), "H".to_string());
        shortcuts.insert("Eraser".to_string(), "E".to_string());
        shortcuts.insert("Note".to_string(), "N".to_string());
        shortcuts.insert("TextBox".to_string(), "T".to_string());
        shortcuts.insert("Rect".to_string(), "R".to_string());
        shortcuts.insert("Ellipse".to_string(), "O".to_string());
        shortcuts.insert("Arrow".to_string(), "A".to_string());
        shortcuts.insert("Line".to_string(), "L".to_string());
        shortcuts.insert("Save".to_string(), "Ctrl+S".to_string());
        shortcuts.insert("Next Tab".to_string(), "Ctrl+Tab".to_string());
        shortcuts.insert("Close Tab".to_string(), "Ctrl+W".to_string());
        shortcuts.insert("Brush Size".to_string(), "Shift+Scroll".to_string());
        shortcuts.insert("Undo".to_string(), "Ctrl+Z".to_string());
        shortcuts.insert("Redo".to_string(), "Ctrl+Shift+Z".to_string());

        Self {
            tool:       Tool::Pen,
            color:      Color32::from_rgb(239, 68, 68),
            brush_size: 3.0,
            opacity:    0.85,
            settings_open: false,
            shortcuts,
            theme:      Theme::Dark,
            binding_key: None,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct SerializableToolState {
    pub tool: Tool,
    pub color: [u8; 4],
    pub brush_size: f32,
    pub opacity: f32,
    pub shortcuts: HashMap<String, String>,
    pub theme: Theme,
}

impl ToolState {
    pub fn save_to_disk(&self, path: &std::path::Path) -> Result<(), String> {
        let serializable = SerializableToolState {
            tool: self.tool.clone(),
            color: [self.color.r(), self.color.g(), self.color.b(), self.color.a()],
            brush_size: self.brush_size,
            opacity: self.opacity,
            shortcuts: self.shortcuts.clone(),
            theme: self.theme,
        };
        let data = serde_json::to_string_pretty(&serializable).map_err(|e| e.to_string())?;
        std::fs::write(path, data).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn load_from_disk(path: &std::path::Path) -> Option<Self> {
        let data = std::fs::read_to_string(path).ok()?;
        let serializable: SerializableToolState = serde_json::from_str(&data).ok()?;
        
        let c = serializable.color;
        
        let mut default_shortcuts = Self::default().shortcuts;
        for (k, v) in serializable.shortcuts {
            default_shortcuts.insert(k, v);
        }

        Some(Self {
            tool: serializable.tool,
            color: Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]),
            brush_size: serializable.brush_size,
            opacity: serializable.opacity,
            settings_open: false,
            shortcuts: default_shortcuts,
            theme: serializable.theme,
            binding_key: None,
        })
    }
}

// ── Toast ─────────────────────────────────────────────────────────────────────
pub struct ToastMessage {
    pub text:       String,
    pub spawn_time: f64,
}
impl ToastMessage {
    pub fn new(text: impl Into<String>, ctx: &egui::Context) -> Self {
        Self { text: text.into(), spawn_time: ctx.input(|i| i.time) }
    }
    pub fn is_alive(&self, ctx: &egui::Context) -> bool {
        ctx.input(|i| i.time) - self.spawn_time < 3.0
    }
}

fn check_shortcut(i: &egui::InputState, shortcut: &str) -> bool {
    if shortcut.trim().is_empty() { return false; }
    let parts: Vec<&str> = shortcut.split('+').map(|s| s.trim()).collect();
    let mut needs_ctrl = false;
    let mut needs_alt = false;
    let mut needs_shift = false;
    let mut key_str = "";

    for p in parts {
        match p.to_lowercase().as_str() {
            "ctrl" => needs_ctrl = true,
            "alt" => needs_alt = true,
            "shift" => needs_shift = true,
            _ => key_str = p,
        }
    }

    if i.modifiers.ctrl != needs_ctrl || i.modifiers.alt != needs_alt || i.modifiers.shift != needs_shift {
        return false;
    }

    let key = match key_str.to_uppercase().as_str() {
        "A" => egui::Key::A, "B" => egui::Key::B, "C" => egui::Key::C, "D" => egui::Key::D,
        "E" => egui::Key::E, "F" => egui::Key::F, "G" => egui::Key::G, "H" => egui::Key::H,
        "I" => egui::Key::I, "J" => egui::Key::J, "K" => egui::Key::K, "L" => egui::Key::L,
        "M" => egui::Key::M, "N" => egui::Key::N, "O" => egui::Key::O, "P" => egui::Key::P,
        "Q" => egui::Key::Q, "R" => egui::Key::R, "S" => egui::Key::S, "T" => egui::Key::T,
        "U" => egui::Key::U, "V" => egui::Key::V, "W" => egui::Key::W, "X" => egui::Key::X,
        "Y" => egui::Key::Y, "Z" => egui::Key::Z,
        "TAB" => egui::Key::Tab,
        "ENTER" => egui::Key::Enter, "SPACE" => egui::Key::Space, "ESCAPE" => egui::Key::Escape,
        _ => return false,
    };

    i.key_pressed(key)
}

// ── Keyboard shortcut handler (call from app::update) ────────────────────────
pub fn handle_shortcuts(state: &mut ToolState, ctx: &egui::Context) -> Option<String> {
    let mut action: Option<String> = None;

    // 1. Global shortcuts (work even if a widget has focus)
    ctx.input(|i| {
        if let Some(s) = state.shortcuts.get("Next Tab") { if check_shortcut(i, s) { action = Some("next_tab".to_string()); } }
        if let Some(s) = state.shortcuts.get("Close Tab") { if check_shortcut(i, s) { action = Some("close_tab".to_string()); } }
        if let Some(s) = state.shortcuts.get("Undo") { if check_shortcut(i, s) { action = Some("undo".to_string()); } }
        if let Some(s) = state.shortcuts.get("Redo") { if check_shortcut(i, s) { action = Some("redo".to_string()); } }
    });

    if action.is_some() {
        return action;
    }

    // 2. Local shortcuts (don't steal keys if a text field is focused)
    if ctx.memory(|m| m.focused().is_some()) { return None; }

    ctx.input(|i| {
        // Tool shortcuts
        if let Some(s) = state.shortcuts.get("Select") { if check_shortcut(i, s) { state.tool = Tool::Cursor; } }
        if let Some(s) = state.shortcuts.get("Pen") { if check_shortcut(i, s) { state.tool = Tool::Pen; } }
        if let Some(s) = state.shortcuts.get("Highlight") { if check_shortcut(i, s) { state.tool = Tool::Highlight; } }
        if let Some(s) = state.shortcuts.get("Eraser") { if check_shortcut(i, s) { state.tool = Tool::Eraser; } }
        if let Some(s) = state.shortcuts.get("Note") { if check_shortcut(i, s) { state.tool = Tool::Note; } }
        if let Some(s) = state.shortcuts.get("TextBox") { if check_shortcut(i, s) { state.tool = Tool::TextBox; } }
        if let Some(s) = state.shortcuts.get("Rect") { if check_shortcut(i, s) { state.tool = Tool::Rect; } }
        if let Some(s) = state.shortcuts.get("Ellipse") { if check_shortcut(i, s) { state.tool = Tool::Ellipse; } }
        if let Some(s) = state.shortcuts.get("Arrow") { if check_shortcut(i, s) { state.tool = Tool::Arrow; } }
        if let Some(s) = state.shortcuts.get("Line") { if check_shortcut(i, s) { state.tool = Tool::Line; } }

        // Actions
        if let Some(s) = state.shortcuts.get("Save") { if check_shortcut(i, s) { action = Some("save".to_string()); } }

        // Alt+Scroll → opacity
        if i.modifiers.alt && !i.modifiers.ctrl {
            let dy = i.raw_scroll_delta.y;
            if dy.abs() > 0.5 {
                state.opacity = (state.opacity + dy * 0.005).clamp(0.05, 1.0);
            }
        }

        // Shift+Scroll → brush size
        if i.modifiers.shift && !i.modifiers.ctrl {
            let dy = i.raw_scroll_delta.y;
            if dy.abs() > 0.5 {
                state.brush_size = (state.brush_size + dy * 0.05).clamp(0.5, 40.0);
            }
        }
    });

    action
}

// ── Toolbar renderer ──────────────────────────────────────────────────────────
pub fn draw_toolbar(ui: &mut Ui, state: &mut ToolState) {
    let dark = ui.visuals().dark_mode;
    let sep  = if dark { Color32::from_rgb(45, 55, 72) } else { Color32::from_rgb(200, 210, 225) };

    ui.scope(|ui| {
        // Sidebar breathing room and vertical item spacing
        ui.spacing_mut().window_margin = egui::Margin::symmetric(12.0, 15.0);
        ui.spacing_mut().item_spacing = egui::vec2(0.0, 4.0);
        // Soften active/hovered/inactive selection rounding
        ui.visuals_mut().widgets.active.rounding   = egui::Rounding::same(6.0);
        ui.visuals_mut().widgets.hovered.rounding  = egui::Rounding::same(6.0);
        ui.visuals_mut().widgets.inactive.rounding = egui::Rounding::same(6.0);

        ui.vertical(|ui| {
            ui.set_min_width(72.0);
            ui.set_max_width(72.0);
            ui.add_space(10.0);

            let tools = [
                Tool::Cursor,
                Tool::Pen,
                Tool::Highlight,
                Tool::Eraser,
                Tool::Note,
                Tool::TextBox,
                Tool::Rect,
                Tool::Ellipse,
                Tool::Arrow,
                Tool::Line,
            ];

            for t in &tools {
                let active = &state.tool == t;
                tool_button(ui, t, active, dark, |_| {}).inspect(|r| {
                    if r.clicked() { state.tool = t.clone(); }
                    r.clone().on_hover_text(format!("{} [{}]", t.label(), t.shortcut(state)));
                });
                // item_spacing=(0,10) already provides vertical gap — no extra add_space needed
            }

            // Divider
            ui.add_space(6.0);
            ui.painter().hline(
                ui.max_rect().x_range(),
                ui.cursor().min.y,
                Stroke::new(1.0, sep),
            );
            ui.add_space(8.0);

            // Colour palette — 2 columns of 4
            ui.horizontal_wrapped(|ui| {
                ui.set_max_width(64.0);
                for &col in PALETTE {
                    let selected = state.color == col;
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
                    let painter = ui.painter();
                    // shadow/glow ring for selected
                    if selected {
                        painter.rect_filled(rect.expand(3.0), Rounding::same(5.0),
                            Color32::from_white_alpha(60));
                    }
                    painter.rect_filled(rect, Rounding::same(4.0), col);
                    painter.rect_stroke(rect, Rounding::same(4.0),
                        Stroke::new(if selected { 2.0 } else { 1.0 },
                            if selected { Color32::WHITE } else { Color32::from_gray(120) }));
                    if resp.clicked() { state.color = col; }
                    ui.add_space(2.0);
                }
            });

            // Divider
            ui.add_space(8.0);
            ui.painter().hline(
                ui.max_rect().x_range(),
                ui.cursor().min.y,
                Stroke::new(1.0, sep),
            );
            ui.add_space(8.0);

            // Size and Opacity sliders (side-by-side)
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0; // Tighten the gap between the two sliders
                
                // Left Slider: Size
                ui.add_sized([20.0, 60.0], egui::Slider::new(&mut state.brush_size, 1.0..=50.0).vertical().show_value(false))
                    .on_hover_text("Brush Size");

                // Right Slider: Alpha
                ui.add_sized([20.0, 60.0], egui::Slider::new(&mut state.opacity, 0.0..=1.0).vertical().show_value(false))
                    .on_hover_text("Opacity");
            });
        });
    });
}

/// Render one tool button. Returns the Response so caller can attach .clicked() / .on_hover_text().
fn tool_button<F>(
    ui: &mut Ui,
    tool: &Tool,
    active: bool,
    dark: bool,
    _extra: F,
) -> Option<Response>
where F: FnOnce(&Response)
{
    // Fill the sidebar width without bleeding past the panel edge.
    // Clamp to 64px so the highlight rect stays inside the 68px panel with
    // 2px of breathing room on each side.
    let available_w = ui.available_width().clamp(50.0, 64.0);
    let btn_size = Vec2::new(available_w, 48.0);
    let (rect, resp) = ui.allocate_exact_size(btn_size, Sense::click());

    let breeze_blue = Color32::from_rgb(61, 174, 233);

    // ── Background & accent bar ───────────────────────────────────────────────
    {
        let painter = ui.painter();

        let (bg, stroke) = if active {
            if dark {
                (Color32::from_rgba_unmultiplied(61, 174, 233, 45), Stroke::new(1.0, breeze_blue))
            } else {
                (Color32::from_rgba_unmultiplied(61, 174, 233, 30), Stroke::new(1.0, breeze_blue))
            }
        } else if resp.hovered() {
            if dark {
                (Color32::from_rgb(40, 46, 55), Stroke::new(1.0, Color32::from_rgb(55, 62, 72)))
            } else {
                (Color32::from_rgb(225, 230, 238), Stroke::new(1.0, Color32::from_rgb(190, 198, 210)))
            }
        } else {
            (Color32::TRANSPARENT, Stroke::NONE)
        };

        painter.rect_filled(rect, Rounding::same(6.0), bg);
        if stroke != Stroke::NONE {
            painter.rect_stroke(rect, Rounding::same(6.0), stroke);
        }

        // Left accent bar for active state
        if active {
            let bar = egui::Rect::from_min_size(
                rect.min + Vec2::new(0.0, 4.0),
                Vec2::new(3.0, rect.height() - 8.0),
            );
            painter.rect_filled(bar, Rounding::same(2.0), breeze_blue);
        }
    }

    // ── Icon tint ─────────────────────────────────────────────────────────────
    // SVG source pixels are white (#FFFFFF). egui's tint() multiplies each
    // channel: white * tint = tint, so the tint colour shows directly.
    //   • dark_mode  → pure WHITE  (max brightness, visible on dark bg)
    //   • light_mode → charcoal    (dark glyph on light bg)
    //   • active     → breeze blue accent regardless of theme
    let icon_tint = if active {
        breeze_blue
    } else if dark {
        // Pure white so the icon is fully visible on any dark background
        Color32::WHITE
    } else {
        // Dark charcoal for light theme
        Color32::from_rgb(40, 40, 40)
    };

    let icon_rect = egui::Rect::from_center_size(
        rect.center() - Vec2::new(0.0, 6.0),
        Vec2::splat(20.0),
    );
    egui::Image::new(tool.icon_image()).tint(icon_tint).paint_at(ui, icon_rect);

    // ── Label ─────────────────────────────────────────────────────────────────
    let lbl_col = if active {
        breeze_blue
    } else if dark {
        Color32::from_gray(130)
    } else {
        Color32::from_gray(95)
    };
    ui.painter().text(
        rect.center() + Vec2::new(0.0, 15.0),
        egui::Align2::CENTER_CENTER,
        tool.label(),
        egui::FontId::proportional(8.5),
        lbl_col,
    );

    Some(resp)
}



#[derive(Clone, Copy, Debug)]
struct DragState {
    id: Uuid,
    src_idx: usize,
}

fn draw_tab_ui(
    ui: &mut Ui,
    rect: egui::Rect,
    name: &str,
    is_active: bool,
    is_hovered: bool,
    is_floating: bool,
    text_col: Color32,
    bg_color: Color32,
    tab_id: Uuid,
) -> bool {
    let painter = ui.painter();
    
    // Draw background frame
    let rounding = Rounding { nw: 3.0, ne: 3.0, sw: 0.0, se: 0.0 };
    
    let frame_bg = if is_floating {
        bg_color.linear_multiply(0.9)
    } else if is_hovered && !is_active {
        if ui.visuals().dark_mode {
            Color32::from_rgb(45, 49, 54)
        } else {
            Color32::from_rgb(235, 238, 240)
        }
    } else {
        bg_color
    };
    
    // Draw shadow if floating
    if is_floating {
        painter.rect_filled(rect.translate(Vec2::new(2.0, 2.0)), rounding, Color32::from_black_alpha(50));
    }
    
    painter.rect_filled(rect, rounding, frame_bg);
    
    // Subtle border for inactive tabs
    if !is_active {
        let border_color = if ui.visuals().dark_mode {
            Color32::from_rgb(45, 49, 54)
        } else {
            Color32::from_rgb(200, 204, 207)
        };
        painter.line_segment([rect.left_bottom(), rect.left_top()], Stroke::new(1.0, border_color));
        painter.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, border_color));
        painter.line_segment([rect.right_top(), rect.right_bottom()], Stroke::new(1.0, border_color));
    } else {
        // Active accent top stripe (Breeze style)
        let accent_rect = egui::Rect::from_min_max(
            rect.left_top(),
            rect.right_top() + Vec2::new(0.0, 2.5)
        );
        painter.rect_filled(accent_rect, 0.0, Color32::from_rgb(61, 174, 233));
    }
    
    // Display name
    let display_name = if name.len() > 18 {
        format!("{}...", &name[..15])
    } else {
        name.to_string()
    };
    
    // Draw text (centered vertically)
    let text_pos = rect.left_center() + Vec2::new(10.0, 0.0);
    painter.text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        display_name,
        egui::FontId::proportional(12.5),
        text_col,
    );
    
    // Close button
    let close_btn_rect = egui::Rect::from_center_size(
        rect.right_center() - Vec2::new(14.0, 0.0),
        Vec2::splat(14.0)
    );
    
    let close_id = ui.make_persistent_id((tab_id, if is_floating { "close_float" } else { "close" }));
    let close_resp = ui.interact(close_btn_rect, close_id, Sense::click());
    
    let mut close_clicked = false;
    let hover_close = close_resp.hovered();
    
    if hover_close {
        painter.circle_filled(close_btn_rect.center(), 7.0, Color32::from_rgb(239, 68, 68));
        painter.text(
            close_btn_rect.center() + Vec2::new(0.0, -0.5),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(12.0),
            Color32::WHITE,
        );
    } else {
        painter.text(
            close_btn_rect.center() + Vec2::new(0.0, -0.5),
            egui::Align2::CENTER_CENTER,
            "×",
            egui::FontId::proportional(12.0),
            text_col.linear_multiply(0.6),
        );
    }
    
    if close_resp.clicked() {
        close_clicked = true;
    }
    
    close_clicked
}

pub fn draw_tab_bar(
    ui: &mut Ui,
    tabs: &mut Vec<PdfTab>,
    active_tab_id: &mut Option<Uuid>,
    worker_tx: &Sender<WorkerRequest>,
) -> Option<String> {
    let mut load_file_path = None;
    
    // Fetch persistent drag state
    let drag_state_id = ui.make_persistent_id("tab_drag_state");
    let mut drag_state: Option<DragState> = ui.ctx().data(|d| d.get_temp(drag_state_id));
    
    // If the mouse is released, stop dragging
    if ui.input(|i| i.pointer.any_released()) {
        ui.ctx().data_mut(|d| d.insert_temp::<Option<DragState>>(drag_state_id, None));
        drag_state = None;
    }

    let mut close_tab_id = None;
    let mut active_rect = None;

    ui.horizontal(|ui| {
        ui.style_mut().spacing.item_spacing.x = 2.0;
        
        let mut tab_rects = Vec::new();
        let mut drag_src = None;
        let mut drag_dst = None;

        for (idx, tab) in tabs.iter().enumerate() {
            let is_active = Some(tab.id) == *active_tab_id;
            let is_being_dragged = drag_state.map_or(false, |s| s.id == tab.id);
            
            let (bg_color, text_col) = if is_active {
                if ui.visuals().dark_mode {
                    (Color32::from_rgb(42, 46, 50), Color32::WHITE)
                } else {
                    (Color32::from_rgb(252, 252, 252), Color32::from_rgb(49, 54, 59))
                }
            } else {
                if ui.visuals().dark_mode {
                    (Color32::from_rgb(31, 34, 37), Color32::from_gray(140))
                } else {
                    (Color32::from_rgb(220, 224, 227), Color32::from_gray(100))
                }
            };

            let name = tab.name();
            
            // Get tab width from previous frame or layout the text to get exact width
            let width_id = ui.make_persistent_id((tab.id, "width"));
            let tab_width = ui.ctx().data(|d| d.get_temp::<f32>(width_id)).unwrap_or_else(|| {
                let display_name = if name.len() > 18 {
                    format!("{}...", &name[..15])
                } else {
                    name.clone()
                };
                let text_w = ui.fonts(|f| {
                    f.layout_no_wrap(
                        display_name,
                        egui::FontId::proportional(12.5),
                        Color32::WHITE
                    ).rect.width()
                });
                text_w + 38.0
            });

            if is_being_dragged {
                // Allocate placeholder slot
                let (rect, _resp) = ui.allocate_exact_size(Vec2::new(tab_width, 28.0), Sense::click_and_drag());
                tab_rects.push((idx, rect));

                // Draw dashed outline for placeholder
                let stroke = Stroke::new(1.0, if ui.visuals().dark_mode { Color32::from_gray(80) } else { Color32::from_gray(170) });
                ui.painter().rect_stroke(rect, Rounding::same(3.0), stroke);
                
                drag_src = Some(idx);
            } else {
                // Allocate space for normal tab
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(tab_width, 28.0), Sense::click_and_drag());
                tab_rects.push((idx, rect));

                // Save width for next frame
                ui.ctx().data_mut(|d| d.insert_temp(width_id, rect.width()));

                if is_active {
                    active_rect = Some(rect);
                }

                let is_hovered = resp.hovered();

                let close_clicked = draw_tab_ui(
                    ui,
                    rect,
                    &name,
                    is_active,
                    is_hovered,
                    false,
                    text_col,
                    bg_color,
                    tab.id,
                );

                if close_clicked {
                    close_tab_id = Some(tab.id);
                } else if resp.clicked() || resp.drag_started() {
                    *active_tab_id = Some(tab.id);
                }

                if resp.drag_started() {
                    if let Some(mouse_pos) = ui.input(|i| i.pointer.press_origin()) {
                        let offset = mouse_pos - rect.left_top();
                        ui.ctx().data_mut(|d| d.insert_temp(ui.make_persistent_id((tab.id, "drag_offset")), offset));
                    }
                    let state = DragState { id: tab.id, src_idx: idx };
                    ui.ctx().data_mut(|d| d.insert_temp(drag_state_id, Some(state)));
                    drag_state = Some(state);
                }
            }
        }

        // Handle Drag & Drop rearrangement
        if let Some(state) = drag_state {
            let src_idx = state.src_idx;
            if let Some(mouse_pos) = ui.input(|i| i.pointer.hover_pos()) {
                for &(idx, rect) in &tab_rects {
                    if idx == src_idx { continue; }
                    if mouse_pos.x < rect.center().x && src_idx > idx {
                        drag_dst = Some(idx);
                        break;
                    }
                    if mouse_pos.x > rect.center().x && src_idx < idx {
                        drag_dst = Some(idx);
                    }
                }
            }
        }

        if let (Some(src), Some(dst)) = (drag_src, drag_dst) {
            let tab = tabs.remove(src);
            tabs.insert(dst, tab);
            if let Some(ref mut state) = drag_state {
                state.src_idx = dst;
                ui.ctx().data_mut(|d| d.insert_temp(drag_state_id, Some(*state)));
            }
        }

        // Add button
        if ui.add(Button::new(RichText::new("+").size(16.0)).frame(false))
            .on_hover_text("Open new document")
            .clicked() 
        {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("All Supported", &["pdf", "docx", "doc", "odt", "rtf", "ppt", "pptx", "odp"])
                .pick_file()
            {
                load_file_path = Some(path.to_string_lossy().to_string());
            }
        }
    });

    if let Some(id) = close_tab_id {
        let _ = worker_tx.send(WorkerRequest::Close(id));
        tabs.retain(|t| t.id != id);
        if *active_tab_id == Some(id) {
            *active_tab_id = tabs.last().map(|t| t.id);
        }
    }

    // Draw bottom border line for the tab bar to match KDE Breeze style
    let bottom_y = ui.cursor().min.y;
    let line_color = if ui.visuals().dark_mode {
        Color32::from_rgb(49, 54, 59)
    } else {
        Color32::from_rgb(200, 204, 207)
    };
    
    let left_x = ui.max_rect().left();
    let right_x = ui.max_rect().right();
    
    // Draw segment by segment to skip under active tab
    if let Some(a_rect) = active_rect {
        ui.painter().line_segment([egui::Pos2::new(left_x, bottom_y), egui::Pos2::new(a_rect.left(), bottom_y)], Stroke::new(1.0, line_color));
        ui.painter().line_segment([egui::Pos2::new(a_rect.right(), bottom_y), egui::Pos2::new(right_x, bottom_y)], Stroke::new(1.0, line_color));
    } else {
        ui.painter().line_segment([egui::Pos2::new(left_x, bottom_y), egui::Pos2::new(right_x, bottom_y)], Stroke::new(1.0, line_color));
    }

    // Draw floating tab if being dragged
    if let Some(state) = drag_state {
        if let Some(tab) = tabs.iter().find(|t| t.id == state.id) {
            if let Some(mouse_pos) = ui.input(|i| i.pointer.hover_pos()) {
                let drag_offset = ui.ctx().data(|d| d.get_temp::<Vec2>(ui.make_persistent_id((tab.id, "drag_offset")))).unwrap_or(Vec2::ZERO);
                let tab_pos = mouse_pos - drag_offset;
                
                let width_id = ui.make_persistent_id((tab.id, "width"));
                let tab_width = ui.ctx().data(|d| d.get_temp::<f32>(width_id)).unwrap_or(120.0);
                
                let rect = egui::Rect::from_min_size(tab_pos, Vec2::new(tab_width, 28.0));
                
                egui::Area::new(ui.make_persistent_id((tab.id, "floating")))
                    .fixed_pos(tab_pos)
                    .order(egui::Order::Tooltip)
                    .show(ui.ctx(), |ui| {
                        ui.allocate_exact_size(Vec2::new(tab_width, 28.0), Sense::hover());
                        
                        let (bg_color, text_col) = if Some(tab.id) == *active_tab_id {
                            if ui.visuals().dark_mode {
                                (Color32::from_rgb(42, 46, 50), Color32::WHITE)
                            } else {
                                (Color32::from_rgb(252, 252, 252), Color32::from_rgb(49, 54, 59))
                            }
                        } else {
                            if ui.visuals().dark_mode {
                                (Color32::from_rgb(31, 34, 37), Color32::from_gray(140))
                            } else {
                                (Color32::from_rgb(220, 224, 227), Color32::from_gray(100))
                            }
                        };
                        
                        draw_tab_ui(
                            ui,
                            rect,
                            &tab.name(),
                            Some(tab.id) == *active_tab_id,
                            false,
                            true,
                            text_col,
                            bg_color,
                            tab.id,
                        );
                    });
            }
        }
    }
    
    load_file_path
}

// ── Settings modal ────────────────────────────────────────────────────────────
pub fn draw_shortcut_settings(ctx: &egui::Context, state: &mut ToolState) {
    if !state.settings_open {
        state.binding_key = None;
        return;
    }

    let is_glass = state.theme == Theme::Glassmorphism;
    let base_frame = if is_glass {
        egui::Frame::none()
            .fill(egui::Color32::from_rgba_premultiplied(30, 30, 30, 180))
            .rounding(16.0)
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_white_alpha(30)))
            .inner_margin(20.0)
    } else {
        egui::Frame::window(&ctx.style()).inner_margin(20.0)
    };

    let title_color = if state.theme == Theme::Light { Color32::BLACK } else { Color32::WHITE };
    let bg_color_card = if state.theme == Theme::Light { Color32::from_gray(245) } else { Color32::from_rgb(24, 24, 24) };
    let card_frame = egui::Frame::none()
        .fill(bg_color_card)
        .rounding(8.0)
        .inner_margin(12.0);

    egui::Window::new("⚙ Settings")
        .frame(base_frame)
        .collapsible(false)
        .resizable(false)
        .title_bar(!is_glass)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            if is_glass {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("⚙ Settings").size(20.0).color(title_color).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("✕").clicked() {
                            state.settings_open = false;
                        }
                    });
                });
                ui.add_space(10.0);
            }

            ui.horizontal(|ui| {
                ui.label(RichText::new("Theme:").color(title_color));
                egui::ComboBox::from_id_salt("theme_selector")
                    .selected_text(format!("{:?}", state.theme))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut state.theme, Theme::Dark, "Dark");
                        ui.selectable_value(&mut state.theme, Theme::Light, "Light");
                        ui.selectable_value(&mut state.theme, Theme::Glassmorphism, "Glassmorphism");
                        ui.selectable_value(&mut state.theme, Theme::Skeuomorphism, "Skeuomorphism");
                    });
            });
            
            ui.add_space(12.0);

            // Interactive Binding Logic
            if let Some(target) = state.binding_key.clone() {
                // We are waiting for keypresses
                let mut pressed_modifiers = String::new();
                ctx.input(|i| {
                    if i.modifiers.ctrl { pressed_modifiers.push_str("Ctrl+"); }
                    if i.modifiers.alt { pressed_modifiers.push_str("Alt+"); }
                    if i.modifiers.shift { pressed_modifiers.push_str("Shift+"); }
                    
                    if let Some(key) = i.events.iter().find_map(|e| {
                        if let egui::Event::Key { key, pressed: true, .. } = e { Some(*key) } else { None }
                    }) {
                        // Skip if it's just a modifier key
                        match key {
                            egui::Key::Escape => {
                                state.binding_key = None; // Cancel binding
                            }
                            // Keys not to bind standalone if they are modifiers
                            _ => {
                                let key_name = format!("{:?}", key).to_uppercase();
                                state.shortcuts.insert(target.clone(), format!("{}{}", pressed_modifiers, key_name));
                                state.binding_key = None;
                            }
                        }
                    } else if i.pointer.any_pressed() {
                        // Cancel on mouse click
                        state.binding_key = None;
                    }
                });
            }

            egui::ScrollArea::vertical()
                .max_height(400.0)
                .show(ui, |ui| {
                    card_frame.show(ui, |ui| {
                        ui.label(RichText::new("Tools").size(14.0).strong().color(title_color));
                        ui.add_space(6.0);
                        let tools = vec!["Select", "Pen", "Highlight", "Eraser", "Note", "TextBox", "Rect", "Ellipse", "Arrow", "Line"];
                        draw_shortcut_rows(ui, state, &tools, title_color);
                        
                        ui.add_space(12.0);
                        ui.label(RichText::new("System").size(14.0).strong().color(title_color));
                        ui.add_space(6.0);
                        let system = vec!["Save", "Next Tab", "Close Tab", "Brush Size"];
                        draw_shortcut_rows(ui, state, &system, title_color);
                    });
                });

            if !is_glass {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Close").clicked() {
                        state.settings_open = false;
                    }
                });
            }
        });
}

fn draw_shortcut_rows(ui: &mut Ui, state: &mut ToolState, keys: &[&str], _title_color: Color32) {
    for &name in keys {
        ui.horizontal(|ui| {
            ui.set_min_height(32.0);
            let action_color = if state.theme == Theme::Light { Color32::from_gray(80) } else { Color32::from_gray(180) };
            ui.allocate_ui_with_layout(Vec2::new(120.0, 32.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label(RichText::new(name).color(action_color));
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let current_val = state.shortcuts.get(name).cloned().unwrap_or_default();
                let is_binding = state.binding_key.as_deref() == Some(name);
                
                let btn_text = if is_binding { "Press any key..." } else { &current_val };
                let bg_color = if is_binding { 
                    Color32::from_rgb(50, 130, 250) 
                } else if state.theme == Theme::Light { 
                    Color32::from_gray(220) 
                } else { 
                    Color32::from_gray(40) 
                };

                let text_color = if is_binding { Color32::WHITE } else if state.theme == Theme::Light { Color32::BLACK } else { Color32::WHITE };

                // Neumorphic / Skeuomorphic Button Rendering
                let (rect, response) = ui.allocate_exact_size(Vec2::new(120.0, 28.0), Sense::click());
                
                if response.hovered() && !is_binding {
                    ui.painter().rect_filled(rect, 6.0, bg_color.linear_multiply(1.2));
                } else {
                    if state.theme == Theme::Skeuomorphism || state.theme == Theme::Glassmorphism {
                        let offset = if response.is_pointer_button_down_on() { 1.0 } else { 3.0 };
                        // Shadow
                        ui.painter().rect_filled(rect.translate(Vec2::new(0.0, offset)), 6.0, Color32::from_black_alpha(100));
                        // Main
                        ui.painter().rect_filled(rect, 6.0, bg_color);
                        // Highlight
                        ui.painter().rect_stroke(rect, 6.0, Stroke::new(1.0, Color32::from_white_alpha(50)));
                    } else {
                        ui.painter().rect_filled(rect, 4.0, bg_color);
                    }
                }

                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    btn_text,
                    egui::FontId::proportional(13.0),
                    text_color
                );

                if response.clicked() {
                    if is_binding {
                        state.binding_key = None;
                    } else {
                        state.binding_key = Some(name.to_string());
                    }
                }
            });
        });
    }
}

