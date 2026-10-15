//! The one place that decides how the window looks.
//!
//! Four text sizes, one font, one palette. Every piece of text in the program
//! is built by a helper from this module, so nothing drifts out of step.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, RichText, Stroke, TextStyle};

// -- palette ------------------------------------------------------------

/// Window background.
pub const BG: Color32 = Color32::from_rgb(0x15, 0x18, 0x1D);
/// Cards sitting on the background.
pub const CARD: Color32 = Color32::from_rgb(0x1D, 0x21, 0x28);
/// Their outline, and separators.
pub const EDGE: Color32 = Color32::from_rgb(0x2F, 0x35, 0x3F);
/// Buttons and input fields at rest.
pub const FIELD: Color32 = Color32::from_rgb(0x28, 0x2D, 0x36);

/// Ordinary text.
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xEB, 0xF0);
/// Secondary text: units, paths, explanations.
pub const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA3, 0xB2);
/// Something not filled in yet.
pub const DIM: Color32 = Color32::from_rgb(0x5A, 0x62, 0x70);

/// Links and the loaded file names.
pub const ACCENT: Color32 = Color32::from_rgb(0x6E, 0xA8, 0xFF);
/// The fill of the main buttons. Dark enough for white text to stand out.
pub const ACCENT_FILL: Color32 = Color32::from_rgb(0x1F, 0x5F, 0xCC);
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0x2B, 0x72, 0xE6);
/// Text on top of ACCENT_FILL.
pub const ON_ACCENT: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);

pub const OK: Color32 = Color32::from_rgb(0x56, 0xD1, 0x94);
pub const WARN: Color32 = Color32::from_rgb(0xF0, 0xBE, 0x4B);
pub const BAD: Color32 = Color32::from_rgb(0xF2, 0x7B, 0x7B);

// -- measurements -------------------------------------------------------

/// The single vertical rhythm used between and inside cards.
pub const GAP: f32 = 8.0;

/// Minimum height of the two cards that sit side by side, so they end at the
/// same line however much each has to say.
pub const ROW_BOTTOM: f32 = 104.0;

const RADIUS: u8 = 6;
const SIZE_HEADING: f32 = 20.0;
const SIZE_LEAD: f32 = 16.0;
const SIZE_BODY: f32 = 14.0;
const SIZE_SMALL: f32 = 12.0;

/// Set fonts, sizes and colors once, for every widget in the program.
pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(SIZE_HEADING, FontFamily::Proportional),
            ),
            (
                TextStyle::Body,
                FontId::new(SIZE_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Button,
                FontId::new(SIZE_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(SIZE_SMALL, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(SIZE_BODY, FontFamily::Proportional),
            ),
            (
                TextStyle::Name("Lead".into()),
                FontId::new(SIZE_LEAD, FontFamily::Proportional),
            ),
        ]
        .into();

        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;

        let v = &mut s.visuals;
        v.dark_mode = true;
        v.panel_fill = BG;
        v.window_fill = BG;
        v.extreme_bg_color = Color32::from_rgb(0x11, 0x14, 0x18); // text input
        v.hyperlink_color = ACCENT;
        v.selection.bg_fill = ACCENT_FILL.gamma_multiply(0.55);
        v.selection.stroke = Stroke::new(1.0, ON_ACCENT);

        // Plain text and card outlines.
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        v.widgets.noninteractive.bg_fill = CARD;
        v.widgets.noninteractive.weak_bg_fill = CARD;
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, EDGE);
        v.widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS);

        // Buttons and checkboxes.
        for w in [
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
        ] {
            w.corner_radius = CornerRadius::same(RADIUS);
            w.fg_stroke = Stroke::new(1.0, TEXT);
        }
        v.widgets.inactive.bg_fill = FIELD;
        v.widgets.inactive.weak_bg_fill = FIELD;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, EDGE);
        v.widgets.hovered.bg_fill = FIELD.gamma_multiply(1.35);
        v.widgets.hovered.weak_bg_fill = FIELD.gamma_multiply(1.35);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
        v.widgets.active.bg_fill = ACCENT_HOVER;
        v.widgets.active.weak_bg_fill = ACCENT_HOVER;
        v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);

        // A disabled button must read as unavailable, not as unreadable text
        // on a coloured fill.
        v.widgets.noninteractive.fg_stroke.color = TEXT;
        v.widgets.open.corner_radius = CornerRadius::same(RADIUS);
    });
}

// -- text ---------------------------------------------------------------

/// The one heading of a screen.
pub fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text).heading().color(TEXT)
}

/// A step or a card title that needs more weight than the body.
pub fn lead(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .text_style(TextStyle::Name("Lead".into()))
        .color(TEXT)
}

/// Ordinary text.
pub fn body(text: impl Into<String>) -> RichText {
    RichText::new(text).color(TEXT)
}

/// Ordinary text, emphasised -- card titles and file names.
pub fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text).strong().color(TEXT)
}

/// Secondary text: counts, paths, hints.
pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).small().color(MUTED)
}

/// Label for a filled accent button, always readable on the blue.
pub fn on_accent(text: impl Into<String>, enabled: bool) -> RichText {
    RichText::new(text).color(if enabled { ON_ACCENT } else { DIM })
}

/// The fill for the main action button.
pub fn button_fill(enabled: bool) -> Color32 {
    if enabled { ACCENT_FILL } else { FIELD }
}

// -- shapes -------------------------------------------------------------

/// A card: the only container used in the window.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, EDGE))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::same(10))
}

/// The drop target, which lights up while a file is over the window.
///
/// `tall` is the empty state, which is worth the room because it is the one
/// instruction the program has; once files are loaded it shrinks to a bar.
pub fn drop_card(hovering: bool, tall: bool) -> egui::Frame {
    let (fill, stroke) = if hovering {
        (ACCENT_FILL.gamma_multiply(0.30), Stroke::new(2.0, ACCENT))
    } else {
        (CARD, Stroke::new(1.0, EDGE))
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(egui::Margin::symmetric(10, if tall { 12 } else { 5 }))
}

/// A small coloured tag, for what a file holds. Drawn rather than written, so
/// it stays the same height whatever the text.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) {
    let font = FontId::new(SIZE_SMALL, FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, color);
    let pad = egui::vec2(6.0, 3.0);
    let (rect, _) = ui.allocate_exact_size(galley.size() + pad * 2.0, egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(4),
        color.gamma_multiply(0.16),
    );
    ui.painter().galley(rect.min + pad, galley, color);
}

/// The status mark in front of a row. Drawn, so it looks the same on every
/// machine whatever the font happens to contain.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}
