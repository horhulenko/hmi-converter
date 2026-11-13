//! The one place that decides how the window looks.
//!
//! One font (Noto Sans, regular and bold), one accent colour, a few greys.
//! Every piece of text and every control in the program is built by a helper
//! from this module, so nothing drifts out of step.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke,
    TextStyle,
};

// -- palette ------------------------------------------------------------

/// Window background.
pub const BG: Color32 = Color32::from_rgb(0x11, 0x14, 0x18);
/// Quiet areas set off from the background: the file list, the timeline.
pub const SURFACE: Color32 = Color32::from_rgb(0x17, 0x1B, 0x21);
/// Buttons and input fields at rest.
pub const RAISED: Color32 = Color32::from_rgb(0x1F, 0x24, 0x2C);
pub const RAISED_HOVER: Color32 = Color32::from_rgb(0x29, 0x2F, 0x39);
/// Outlines and separators.
pub const EDGE: Color32 = Color32::from_rgb(0x2A, 0x30, 0x39);

/// Ordinary text.
pub const TEXT: Color32 = Color32::from_rgb(0xE9, 0xED, 0xF2);
/// Secondary text: units, paths, explanations.
pub const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA4, 0xB2);
/// Something switched off, or not there yet.
pub const DIM: Color32 = Color32::from_rgb(0x5D, 0x66, 0x73);

/// The one accent: the main button, ticks, the chosen time, links.
pub const ACCENT: Color32 = Color32::from_rgb(0x0B, 0x8D, 0xDC);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x2A, 0x9F, 0xE6);
/// The same blue, deep: what is selected or hovered sits on this.
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x08, 0x2C, 0x50);
/// Text on top of ACCENT.
pub const ON_ACCENT: Color32 = Color32::WHITE;

/// Kept only for what must not be missed.
pub const WARN: Color32 = Color32::from_rgb(0xE5, 0xA2, 0x3B);
pub const BAD: Color32 = Color32::from_rgb(0xE5, 0x48, 0x4D);

// -- measurements -------------------------------------------------------

/// Space between related things.
pub const GAP: f32 = 10.0;
/// Space between the steps of the screen.
pub const SECTION: f32 = 26.0;
/// Margin around the whole window.
pub const MARGIN: i8 = 28;

const RADIUS: u8 = 6;
const SIZE_HEADING: f32 = 24.0;
const SIZE_TITLE: f32 = 17.0;
const SIZE_BODY: f32 = 14.0;
const SIZE_SMALL: f32 = 12.5;

/// The bold face, for headings and the names that matter.
pub fn bold_family() -> FontFamily {
    FontFamily::Name("bold".into())
}

/// Set fonts, sizes and colours once, for every widget in the program.
pub fn apply(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "noto".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSans-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "noto-bold".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSans-Bold.ttf"
        ))),
    );
    // The built-in fonts stay behind as a fallback for any odd character.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let regular: Vec<String> = std::iter::once("noto".to_string())
        .chain(fallback.iter().cloned())
        .collect();
    let bold: Vec<String> = std::iter::once("noto-bold".to_string())
        .chain(regular.iter().cloned())
        .collect();
    fonts.families.insert(FontFamily::Proportional, regular);
    fonts.families.insert(bold_family(), bold);
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(SIZE_HEADING, bold_family())),
            (TextStyle::Body, FontId::new(SIZE_BODY, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(SIZE_BODY, FontFamily::Proportional)),
            (TextStyle::Small, FontId::new(SIZE_SMALL, FontFamily::Proportional)),
            (TextStyle::Monospace, FontId::new(SIZE_BODY, FontFamily::Proportional)),
            (TextStyle::Name("Title".into()), FontId::new(SIZE_TITLE, bold_family())),
        ]
        .into();

        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(14.0, 6.0);
        s.spacing.interact_size.y = 30.0;
        s.spacing.icon_width = 18.0;
        s.spacing.icon_width_inner = 10.0;
        s.spacing.icon_spacing = 8.0;
        // A scroll bar with room of its own, so it never lies over the
        // right-hand edge of a panel.
        s.spacing.scroll = egui::style::ScrollStyle::solid();
        s.spacing.scroll.bar_width = 8.0;
        s.spacing.scroll.bar_outer_margin = 4.0;

        let v = &mut s.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = SURFACE;
        v.window_stroke = Stroke::new(1.0, EDGE);
        v.window_corner_radius = CornerRadius::same(10);
        v.extreme_bg_color = SURFACE; // text input
        v.faint_bg_color = SURFACE;
        v.hyperlink_color = ACCENT_HOVER;
        v.selection.bg_fill = ACCENT_DEEP;
        v.selection.stroke = Stroke::new(1.0, ACCENT);
        v.popup_shadow = egui::Shadow {
            offset: [0, 6],
            blur: 20,
            spread: 0,
            color: Color32::from_black_alpha(110),
        };

        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.noninteractive.bg_fill = SURFACE;
        v.widgets.noninteractive.weak_bg_fill = SURFACE;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, EDGE);
        v.widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS);

        // Buttons: flat, no outline -- the fill says it can be pressed.
        for w in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
            &mut v.widgets.open,
        ] {
            w.corner_radius = CornerRadius::same(RADIUS);
            w.fg_stroke = Stroke::new(1.0, TEXT);
            w.expansion = 0.0;
        }
        v.widgets.inactive.bg_fill = RAISED;
        v.widgets.inactive.weak_bg_fill = RAISED;
        v.widgets.inactive.bg_stroke = Stroke::NONE;
        v.widgets.hovered.bg_fill = RAISED_HOVER;
        v.widgets.hovered.weak_bg_fill = RAISED_HOVER;
        v.widgets.hovered.bg_stroke = Stroke::NONE;
        v.widgets.active.bg_fill = ACCENT_DEEP;
        v.widgets.active.weak_bg_fill = ACCENT_DEEP;
        v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
        v.widgets.open.bg_fill = RAISED_HOVER;
        v.widgets.open.weak_bg_fill = RAISED_HOVER;
        v.widgets.open.bg_stroke = Stroke::new(1.0, EDGE);
    });
}

// -- text ---------------------------------------------------------------

/// The one heading of a screen.
pub fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text).heading().color(TEXT)
}

/// The title of a step.
pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .text_style(TextStyle::Name("Title".into()))
        .color(TEXT)
}

/// Ordinary text.
pub fn body(text: impl Into<String>) -> RichText {
    RichText::new(text).color(TEXT)
}

/// Ordinary text in the bold face: file names, the numbers that matter.
pub fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .font(FontId::new(SIZE_BODY, bold_family()))
        .color(TEXT)
}

/// Secondary text: counts, paths, hints.
pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).small().color(MUTED)
}

/// Secondary text at the size of the body, for a hint that is read, not
/// skimmed.
pub fn hint(text: impl Into<String>) -> RichText {
    RichText::new(text).color(MUTED)
}

// -- shapes -------------------------------------------------------------

/// A quiet area set off from the background, with no outline: the file
/// list, the side bar of the chart.
pub fn panel() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
}

// -- controls -----------------------------------------------------------

/// The button that does the job of the screen. There is one per screen.
pub fn primary(ui: &mut egui::Ui, text: &str, enabled: bool, size: egui::Vec2) -> egui::Response {
    let label = RichText::new(text)
        .font(FontId::new(15.0, bold_family()))
        .color(if enabled { ON_ACCENT } else { DIM });
    let fill = if enabled { ACCENT } else { RAISED };
    let button = egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow()))
        .fill(fill)
        .min_size(size);
    let response = ui.add_enabled(enabled, button);
    if enabled && response.hovered() {
        // A touch lighter under the pointer, painted over the button.
        ui.painter().rect_filled(
            response.rect,
            CornerRadius::same(RADIUS),
            Color32::from_white_alpha(14),
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Any other button with a frame.
pub fn secondary(ui: &mut egui::Ui, text: &str, enabled: bool, size: egui::Vec2) -> egui::Response {
    let label = RichText::new(text).color(if enabled { TEXT } else { DIM });
    let button = egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow())).min_size(size);
    ui.add_enabled(enabled, button)
}

/// A button that is only its words, for the small things beside a title.
pub fn quiet(ui: &mut egui::Ui, text: &str) -> egui::Response {
    quiet_enabled(ui, text, true)
}

pub fn quiet_enabled(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let font = FontId::new(SIZE_BODY, FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER);
    let pad = egui::vec2(8.0, 5.0);
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(galley.size() + pad * 2.0, sense);
    let colour = if !enabled {
        DIM
    } else if response.hovered() {
        TEXT
    } else {
        ACCENT_HOVER
    };
    if enabled && response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(RADIUS), RAISED);
    }
    ui.painter().galley(rect.min + pad, galley, colour);
    if enabled {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

/// A tick box with its words beside it: blue and white when on, an empty
/// box when off. True in `changed()` when it was clicked.
pub fn check(ui: &mut egui::Ui, on: &mut bool, text: &str) -> egui::Response {
    let galley = (!text.is_empty()).then(|| {
        ui.painter().layout_no_wrap(
            text.to_string(),
            FontId::new(SIZE_BODY, FontFamily::Proportional),
            Color32::PLACEHOLDER,
        )
    });
    let box_size = 18.0;
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x + 8.0);
    let height = ui.spacing().interact_size.y;
    let (rect, mut response) =
        ui.allocate_exact_size(egui::vec2(box_size + text_w, height), egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let lit = ui
        .ctx()
        .animate_bool_with_time(response.id.with("lit"), *on, 0.12);
    let square = egui::Rect::from_center_size(
        egui::pos2(rect.left() + box_size / 2.0, rect.center().y),
        egui::vec2(box_size, box_size),
    );
    tick_box(ui.painter(), square, lit, response.hovered());
    if let Some(g) = galley {
        let colour = if response.hovered() { TEXT } else { TEXT.gamma_multiply(0.92) };
        ui.painter().galley(
            egui::pos2(square.right() + 8.0, rect.center().y - g.size().y / 2.0),
            g,
            colour,
        );
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The box of a tick, drawn: `lit` runs from 0 (empty) to 1 (ticked).
pub fn tick_box(paint: &egui::Painter, square: egui::Rect, lit: f32, hovered: bool) {
    let rest = if hovered { RAISED_HOVER } else { RAISED };
    paint.rect(
        square,
        CornerRadius::same(4),
        rest.lerp_to_gamma(ACCENT, lit),
        Stroke::new(1.0, EDGE.lerp_to_gamma(ACCENT, lit)),
        egui::StrokeKind::Inside,
    );
    if lit > 0.05 {
        let c = square.center();
        let s = square.width() / 18.0;
        paint.line(
            vec![
                c + egui::vec2(-4.5, 0.2) * s,
                c + egui::vec2(-1.3, 3.4) * s,
                c + egui::vec2(4.6, -3.4) * s,
            ],
            Stroke::new(2.0, ON_ACCENT.gamma_multiply(lit)),
        );
    }
}

/// A row of choices of which one is picked, drawn as one pill.
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[(T, &str, &str)],
) -> bool {
    let mut changed = false;
    let font = FontId::new(SIZE_BODY, FontFamily::Proportional);
    let pad = egui::vec2(14.0, 6.0);
    let galleys: Vec<_> = options
        .iter()
        .map(|(_, text, _)| {
            ui.painter()
                .layout_no_wrap(text.to_string(), font.clone(), Color32::PLACEHOLDER)
        })
        .collect();
    let widths: Vec<f32> = galleys.iter().map(|g| g.size().x + pad.x * 2.0).collect();
    let height = ui.spacing().interact_size.y;
    let total = widths.iter().sum::<f32>() + 6.0;
    let (outer, _) = ui.allocate_exact_size(egui::vec2(total, height + 2.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(outer, CornerRadius::same(RADIUS + 2), SURFACE);
    let mut x = outer.left() + 3.0;
    for ((option, galley), w) in options.iter().zip(galleys).zip(widths) {
        let rect = egui::Rect::from_min_size(
            egui::pos2(x, outer.top() + 3.0),
            egui::vec2(w, outer.height() - 6.0),
        );
        x += w;
        let response = ui
            .interact(rect, ui.id().with(("seg", option.1)), egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        let response = if option.2.is_empty() {
            response
        } else {
            response.on_hover_text(option.2)
        };
        let picked = *value == option.0;
        if picked {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS), ACCENT);
        } else if response.hovered() {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(RADIUS), RAISED);
        }
        let colour = if picked {
            ON_ACCENT
        } else if response.hovered() {
            TEXT
        } else {
            MUTED
        };
        ui.painter().galley(
            rect.center() - galley.size() / 2.0,
            galley,
            colour,
        );
        if response.clicked() && !picked {
            *value = option.0;
            changed = true;
        }
    }
    changed
}

/// Where a step of the screen stands.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// What to do now.
    Current,
    /// Already done; can still be changed.
    Done,
    /// Comes later; nothing to do here yet.
    Later,
}

/// How far the content of a step is set in from the rail.
pub const RAIL: f32 = 48.0;

/// One step of the setup screen: a numbered disc on the rail at the left, a
/// line down to the next step, and the content set in beside it -- so the
/// screen reads as one path from top to bottom rather than loose blocks.
pub fn step<R>(
    ui: &mut egui::Ui,
    number: usize,
    state: Step,
    last: bool,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let left = ui.cursor().min.x;
    let width = ui.available_width();
    let spacing = ui.spacing().item_spacing;
    let out = ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(RAIL);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = spacing;
            ui.set_width(width - RAIL);
            // Where the title row really begins -- the spacing before the
            // step has been added by now -- so the disc sits level with it.
            let top = ui.cursor().min.y;
            (top, content(ui))
        })
        .inner
    });
    let (top, inner) = out.inner;
    let bottom = out.response.rect.bottom();

    let paint = ui.painter();
    let c = egui::pos2(left + 15.0, top + TITLE_ROW / 2.0);
    if !last {
        // Down to the next disc, which sits one section further on.
        paint.vline(
            c.x,
            (c.y + 20.0)..=(bottom + SECTION - 4.0),
            Stroke::new(
                2.0,
                if state == Step::Done {
                    ACCENT.gamma_multiply(0.45)
                } else {
                    EDGE
                },
            ),
        );
    }
    match state {
        Step::Current => {
            paint.circle_filled(c, 15.0, ACCENT);
        }
        Step::Done => {
            paint.circle_filled(c, 15.0, ACCENT_DEEP);
            paint.circle_stroke(c, 15.0, Stroke::new(1.5, ACCENT));
        }
        Step::Later => {
            paint.circle_filled(c, 15.0, BG);
            paint.circle_stroke(c, 14.5, Stroke::new(1.5, EDGE));
        }
    }
    if state == Step::Done {
        // A finished step shows a tick rather than its number.
        paint.line(
            vec![
                c + egui::vec2(-5.5, 0.5),
                c + egui::vec2(-1.5, 4.5),
                c + egui::vec2(6.0, -4.0),
            ],
            Stroke::new(2.2, ACCENT_HOVER),
        );
    } else {
        // A figure in Noto Sans sits half a pixel below the middle of its
        // line at this size, so the line is lifted by that much.
        paint.text(
            c - egui::vec2(0.0, 0.5),
            egui::Align2::CENTER_CENTER,
            number.to_string(),
            FontId::new(15.0, bold_family()),
            if state == Step::Current { ON_ACCENT } else { DIM },
        );
    }
    inner
}

/// The height of the title line of a step, which its disc is centred on.
const TITLE_ROW: f32 = 34.0;

/// The title of a step, and under it one line saying what to do there.
/// `right` sits at the far end of the title line.
pub fn step_title(
    ui: &mut egui::Ui,
    text: &str,
    subtitle: &str,
    state: Step,
    right: impl FnOnce(&mut egui::Ui),
) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal(|ui| {
            ui.set_min_height(TITLE_ROW);
            ui.label(title(text).color(if state == Step::Later { DIM } else { TEXT }));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
        });
        if !subtitle.is_empty() {
            ui.label(hint(subtitle).color(if state == Step::Later { DIM } else { MUTED }));
        }
    });
}

/// A hairline across the width of whatever it is in.
pub fn divider(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, EDGE));
}

/// A tinted strip for something that must be read: a warning, an error.
pub fn banner(ui: &mut egui::Ui, text: impl Into<String>, colour: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(colour.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, colour.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 18.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, colour);
                ui.add(egui::Label::new(body(text).color(TEXT)).wrap());
            });
        })
        .response
}

/// A short bar showing how much of a whole is kept.
pub fn meter(ui: &mut egui::Ui, fraction: f32, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), RAISED_HOVER);
    let f = fraction.clamp(0.0, 1.0);
    if f > 0.0 {
        let filled = egui::Rect::from_min_size(
            rect.min,
            egui::vec2((rect.width() * f).max(3.0), rect.height()),
        );
        ui.painter().rect_filled(filled, CornerRadius::same(3), ACCENT);
    }
}

/// A row of a settings form: the grey name in a column of its own, the
/// controls beside it, all on one baseline.
pub fn form_row(ui: &mut egui::Ui, label: &str, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.set_min_height(44.0);
        ui.allocate_ui_with_layout(
            egui::vec2(width, 32.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.set_width(width);
                ui.set_min_height(32.0);
                ui.label(hint(label));
            },
        );
        add(ui);
    });
}

/// Content of `width` at most, centred in what there is: for the screens
/// that report a result, which read best as one column.
pub fn column<R>(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let w = width.min(ui.available_width());
    let side = ((ui.available_width() - w) / 2.0).max(0.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(side);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            ui.set_width(w);
            add(ui)
        })
        .inner
    })
    .inner
}

/// A small tag for what a file holds. Drawn rather than written, so it stays
/// the same height whatever the text.
pub fn chip(ui: &mut egui::Ui, text: &str, strong: bool) {
    let font = FontId::new(SIZE_SMALL, FontFamily::Proportional);
    let colour = if strong { ACCENT_HOVER } else { MUTED };
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, colour);
    let pad = egui::vec2(8.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(10),
        if strong { ACCENT_DEEP } else { RAISED },
    );
    ui.painter().galley(rect.min + pad, galley, colour);
}

/// A thin bar of progress, from 0 to 1; `None` sweeps back and forth while
/// the end is not yet known.
pub fn progress(ui: &mut egui::Ui, fraction: Option<f32>, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 8.0), egui::Sense::hover());
    let paint = ui.painter();
    paint.rect_filled(rect, CornerRadius::same(4), RAISED);
    let filled = match fraction {
        Some(f) => egui::Rect::from_min_size(
            rect.min,
            egui::vec2(rect.width() * f.clamp(0.0, 1.0), rect.height()),
        ),
        None => {
            let t = ui.input(|i| i.time) as f32;
            let w = rect.width() * 0.25;
            let x = (t * 0.8).sin() * 0.5 + 0.5;
            ui.ctx().request_repaint();
            egui::Rect::from_min_size(
                egui::pos2(rect.left() + (rect.width() - w) * x, rect.top()),
                egui::vec2(w, rect.height()),
            )
        }
    };
    if filled.width() > 0.5 {
        paint.rect_filled(filled, CornerRadius::same(4), ACCENT);
    }
}

/// A round mark with a tick in it, for a finished job.
pub fn done_mark(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(34.0, 34.0), egui::Sense::hover());
    let c = rect.center();
    ui.painter().circle_filled(c, 16.0, ACCENT);
    ui.painter().line(
        vec![
            c + egui::vec2(-7.0, 0.5),
            c + egui::vec2(-2.0, 5.5),
            c + egui::vec2(7.5, -5.0),
        ],
        Stroke::new(2.6, ON_ACCENT),
    );
}

/// A small x, drawn rather than written, to take something off a list.
pub fn cross(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), RAISED_HOVER);
    }
    let colour = if response.hovered() { TEXT } else { DIM };
    let r = 4.0;
    let c = rect.center();
    let stroke = Stroke::new(1.5, colour);
    ui.painter()
        .line_segment([c - egui::vec2(r, r), c + egui::vec2(r, r)], stroke);
    ui.painter()
        .line_segment([c + egui::vec2(r, -r), c + egui::vec2(-r, r)], stroke);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A line of text in a warning colour, with a small mark in front.
pub fn warning(ui: &mut egui::Ui, text: impl Into<String>, colour: Color32) -> egui::Response {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 18.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, colour);
        ui.add(egui::Label::new(body(text).color(colour)).wrap())
    })
    .inner
}
