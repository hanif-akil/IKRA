use egui::{Color32, RichText, Ui, Vec2, Stroke, Rounding, Response, Sense};

// ── Tool enum ─────────────────────────────────────────────────────────────────
#[derive(Clone, PartialEq, Debug)]
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
    /// Keyboard shortcut hint shown in tooltip
    pub fn shortcut(&self) -> &'static str {
        match self {
            Tool::Cursor    => "V",
            Tool::Pen       => "P",
            Tool::Highlight => "H",
            Tool::Eraser    => "E",
            Tool::Note      => "N",
            Tool::TextBox   => "T",
            Tool::Rect      => "R",
            Tool::Ellipse   => "O",
            Tool::Arrow     => "A",
            Tool::Line      => "L",
        }
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

// ── ToolState ─────────────────────────────────────────────────────────────────
pub struct ToolState {
    pub tool:       Tool,
    pub color:      Color32,
    pub brush_size: f32,
    pub opacity:    f32,
}

impl Default for ToolState {
    fn default() -> Self {
        Self {
            tool:       Tool::Pen,
            color:      Color32::from_rgb(239, 68, 68),
            brush_size: 3.0,
            opacity:    0.85,
        }
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

// ── Keyboard shortcut handler (call from app::update) ────────────────────────
pub fn handle_shortcuts(state: &mut ToolState, ctx: &egui::Context) -> Option<String> {
    // Don't steal keys if a text field is focused
    if ctx.memory(|m| m.focused().is_some()) { return None; }

    let mut action: Option<String> = None;

    ctx.input(|i| {
        // Tool shortcuts
        if i.key_pressed(egui::Key::V) && !i.modifiers.ctrl { state.tool = Tool::Cursor; }
        if i.key_pressed(egui::Key::P) && !i.modifiers.ctrl { state.tool = Tool::Pen; }
        if i.key_pressed(egui::Key::H) && !i.modifiers.ctrl { state.tool = Tool::Highlight; }
        if i.key_pressed(egui::Key::E) && !i.modifiers.ctrl { state.tool = Tool::Eraser; }
        if i.key_pressed(egui::Key::N) && !i.modifiers.ctrl { state.tool = Tool::Note; }
        if i.key_pressed(egui::Key::T) && !i.modifiers.ctrl { state.tool = Tool::TextBox; }
        if i.key_pressed(egui::Key::R) && !i.modifiers.ctrl { state.tool = Tool::Rect; }
        if i.key_pressed(egui::Key::O) && !i.modifiers.ctrl { state.tool = Tool::Ellipse; }
        if i.key_pressed(egui::Key::A) && !i.modifiers.ctrl { state.tool = Tool::Arrow; }
        if i.key_pressed(egui::Key::L) && !i.modifiers.ctrl { state.tool = Tool::Line; }

        // Ctrl+S → save (signal to app)
        if i.key_pressed(egui::Key::S) && i.modifiers.ctrl {
            action = Some("save".to_string());
        }

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
    let bg   = if dark { Color32::from_rgb(22, 27, 38) } else { Color32::from_rgb(240, 242, 248) };
    let sep  = if dark { Color32::from_rgb(45, 55, 72) } else { Color32::from_rgb(200, 210, 225) };

    // Fill background
    ui.painter().rect_filled(ui.max_rect(), 0.0, bg);

    ui.vertical(|ui| {
        ui.set_min_width(68.0);
        ui.set_max_width(68.0);
        ui.add_space(8.0);

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
                r.clone().on_hover_text(format!("{} [{}]", t.label(), t.shortcut()));
            });
            ui.add_space(2.0);
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

        // Size slider
        let lbl_col = if dark { Color32::from_gray(160) } else { Color32::from_gray(90) };
        ui.label(RichText::new("SIZE").size(9.0).color(lbl_col));
        ui.add(
            egui::Slider::new(&mut state.brush_size, 0.5..=40.0)
                .vertical()
                .show_value(false)
        );
        ui.label(RichText::new(format!("{:.0}", state.brush_size)).size(10.0).color(lbl_col));

        ui.add_space(8.0);

        // Opacity slider
        ui.label(RichText::new("ALPHA").size(9.0).color(lbl_col));
        ui.add(
            egui::Slider::new(&mut state.opacity, 0.05..=1.0)
                .vertical()
                .show_value(false)
        );
        ui.label(RichText::new(format!("{:.0}%", state.opacity * 100.0)).size(10.0).color(lbl_col));
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
    let btn_size = Vec2::new(60.0, 44.0);
    let (rect, resp) = ui.allocate_exact_size(btn_size, Sense::click());

    let painter = ui.painter();

    // Background
    let bg = if active {
        Color32::from_rgb(49, 130, 206)
    } else if resp.hovered() {
        if dark { Color32::from_rgb(35, 45, 65) } else { Color32::from_rgb(220, 228, 240) }
    } else {
        Color32::TRANSPARENT
    };
    painter.rect_filled(rect, Rounding::same(6.0), bg);

    // Left accent bar when active
    if active {
        let bar = egui::Rect::from_min_size(
            rect.min,
            Vec2::new(3.0, rect.height()),
        );
        painter.rect_filled(bar, Rounding::same(2.0), Color32::WHITE);
    }

    // Icon
    let icon_col = if active {
        Color32::WHITE
    } else if dark {
        Color32::from_gray(190)
    } else {
        Color32::from_gray(60)
    };
    painter.text(
        rect.center() - Vec2::new(0.0, 7.0),
        egui::Align2::CENTER_CENTER,
        tool.icon(),
        egui::FontId::proportional(18.0),
        icon_col,
    );

    // Label
    let lbl_col = if active {
        Color32::from_white_alpha(200)
    } else if dark {
        Color32::from_gray(120)
    } else {
        Color32::from_gray(100)
    };
    painter.text(
        rect.center() + Vec2::new(0.0, 12.0),
        egui::Align2::CENTER_CENTER,
        tool.label(),
        egui::FontId::proportional(8.5),
        lbl_col,
    );

    Some(resp)
}

pub fn draw_thumb(_ui: &mut Ui, _tex: &egui::TextureHandle, _page_num: usize, _active: bool) {
    // Handled inline in app.rs thumbnail panel
}
