//! The time range as a picture.
//!
//! An HMI hands over its log in pieces, and what a person wants out of it is
//! usually "that morning" or "those two hours" -- neither of which is easy to
//! type as a date. So the files are drawn on one axis, each as a bar in the
//! place it actually covers, and the slices to convert are drawn over them and
//! dragged by their edges.
//!
//! Several slices are allowed: a window can be split in two and the halves
//! pulled apart, which leaves the quiet hour in the middle out of the export.

use eframe::egui;

use crate::convert::{self, Range};
use crate::theme::{self, ACCENT, DIM, EDGE, MUTED, OK, TEXT};

/// One file, where it logs, and whether it is ticked in the table above.
pub struct Lane {
    pub name: String,
    pub span: (i64, i64),
    pub on: bool,
    /// How many values fall into each equal part of the span, from the probe.
    pub density: Vec<u32>,
}

/// What is being dragged, while it is being dragged.
#[derive(Clone, Copy, PartialEq)]
pub enum Grab {
    /// An edge of window `i`.
    Start(usize),
    End(usize),
    /// The whole of window `i`, held this many ticks from its start.
    Whole(usize, i64),
    /// A window being drawn from nothing, anchored where it began.
    New(usize, i64),
}

/// How much room the drawing needs, not counting the labels above it.
const AXIS_H: f32 = 16.0;
const LANE_H: f32 = 9.0;
const LANE_GAP: f32 = 3.0;
/// How many lanes are drawn one above the other before they are stacked into
/// a single band -- past this the bars would be hairlines.
const MAX_LANES: usize = 6;
/// How close the pointer has to be to an edge to take hold of it.
const GRIP: f32 = 5.0;
/// 100-nanosecond ticks in a second, as everywhere else in the program.
const SECOND: i64 = 10_000_000;

/// Draw the timeline and let it be edited. True when something changed.
///
/// `windows` is the selection: empty means the whole range. `grab` holds a
/// drag between frames.
pub fn ui(
    ui: &mut egui::Ui,
    lanes: &[Lane],
    bounds: Option<(i64, i64)>,
    windows: &mut Vec<Range>,
    selected: &mut usize,
    grab: &mut Option<Grab>,
) -> bool {
    // Past a handful of files the bars would be hairlines, so they are all
    // drawn into one band -- and then only one row is needed.
    let lane_rows = if lanes.len() > MAX_LANES {
        1
    } else {
        lanes.len().max(1)
    };
    let height = AXIS_H + lane_rows as f32 * (LANE_H + LANE_GAP) + 6.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click_and_drag(),
    );
    let paint = ui.painter().with_clip_rect(rect.expand(2.0));

    // Nothing loaded yet: an empty track, so the card does not jump about
    // when the first file arrives.
    let Some((t0, t1)) = bounds.filter(|(a, b)| b > a) else {
        paint.rect_filled(track(rect), egui::CornerRadius::same(4), theme::FIELD);
        paint.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "the log appears here once a file is loaded",
            egui::FontId::proportional(12.0),
            DIM,
        );
        return false;
    };

    // A little air at both ends, so a window at the very edge can still be
    // taken hold of.
    let pad = ((t1 - t0) / 40).max(SECOND);
    let (lo, hi) = (t0 - pad, t1 + pad);
    let span = (hi - lo) as f64;
    let x_of = |t: i64| rect.left() + ((t - lo) as f64 / span) as f32 * rect.width();
    let t_of = |x: f32| lo + (((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * span) as i64;

    hours(&paint, rect, lo, hi, &x_of);

    // -- the files ---------------------------------------------------------
    let band = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + AXIS_H),
        egui::pos2(rect.right(), rect.bottom() - 6.0),
    );
    paint.rect_filled(band, egui::CornerRadius::same(4), theme::FIELD);
    let stacked = lanes.len() > MAX_LANES;
    let mut under_pointer: Option<&Lane> = None;
    for (i, lane) in lanes.iter().enumerate() {
        let row = if stacked { 0 } else { i };
        let top = band.top() + 2.0 + row as f32 * (LANE_H + LANE_GAP);
        let bar = egui::Rect::from_min_max(
            egui::pos2(x_of(lane.span.0), top),
            egui::pos2(x_of(lane.span.1).max(x_of(lane.span.0) + 2.0), top + LANE_H),
        );
        let color = if lane.on {
            OK.gamma_multiply(if stacked { 0.6 } else { 1.0 })
        } else {
            DIM
        };
        // The file covers all of this...
        paint.rect_filled(bar, egui::CornerRadius::same(2), color.gamma_multiply(0.18));
        // ...but only logged where it is bright. A plant that stands still
        // overnight leaves a gap, and a slice taken over the gap is empty.
        if !lane.density.is_empty() {
            let busiest = lane.density.iter().copied().max().unwrap_or(1).max(1) as f32;
            let parts = lane.density.len();
            let wide = (bar.width() / parts as f32).max(1.0);
            for (k, n) in lane.density.iter().enumerate() {
                if *n == 0 {
                    continue;
                }
                let x = bar.left() + bar.width() * k as f32 / parts as f32;
                // The square root keeps a quiet hour visible beside a busy one.
                let strength = (*n as f32 / busiest).sqrt().clamp(0.25, 1.0);
                paint.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, bar.top()), egui::vec2(wide, bar.height())),
                    egui::CornerRadius::ZERO,
                    color.gamma_multiply(strength * if stacked { 0.6 } else { 0.95 }),
                );
            }
        } else {
            paint.rect_filled(bar, egui::CornerRadius::same(2), color.gamma_multiply(0.8));
        }
        // Which file is which, on hover -- there is no room to write it.
        if let Some(p) = response.hover_pos()
            && bar.expand2(egui::vec2(0.0, 1.5)).contains(p)
            && under_pointer.is_none()
        {
            under_pointer = Some(lane);
        }
    }
    if let Some(lane) = under_pointer {
        response.clone().show_tooltip_ui(|ui| {
            ui.label(theme::body(lane.name.clone()));
            ui.label(theme::muted(format!(
                "{}  to  {}",
                convert::filetime_to_short(lane.span.0),
                convert::filetime_to_short(lane.span.1)
            )));
        });
    }

    // -- what is left out --------------------------------------------------
    let shade = egui::Color32::from_black_alpha(130);
    if windows.is_empty() {
        // Everything is taken: no shading at all.
    } else {
        let mut edges: Vec<(f32, f32)> = windows
            .iter()
            .map(|w| {
                (
                    w.from.map_or(rect.left(), &x_of),
                    w.until.map_or(rect.right(), &x_of),
                )
            })
            .collect();
        edges.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut cursor = rect.left();
        for (a, b) in &edges {
            if *a > cursor {
                paint.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(cursor, band.top()),
                        egui::pos2(*a, band.bottom()),
                    ),
                    egui::CornerRadius::ZERO,
                    shade,
                );
            }
            cursor = cursor.max(*b);
        }
        if cursor < rect.right() {
            paint.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(cursor, band.top()),
                    egui::pos2(rect.right(), band.bottom()),
                ),
                egui::CornerRadius::ZERO,
                shade,
            );
        }
    }

    // -- the windows themselves --------------------------------------------
    for (i, w) in windows.iter().enumerate() {
        let (a, b) = (
            w.from.map_or(rect.left(), &x_of),
            w.until.map_or(rect.right(), &x_of),
        );
        let here = egui::Rect::from_min_max(
            egui::pos2(a.min(b), band.top()),
            egui::pos2(a.max(b), band.bottom()),
        );
        let picked = *selected == i;
        paint.rect(
            here,
            egui::CornerRadius::same(3),
            ACCENT.gamma_multiply(if picked { 0.14 } else { 0.07 }),
            egui::Stroke::new(if picked { 1.5 } else { 1.0 }, ACCENT),
            egui::StrokeKind::Inside,
        );
        for x in [a, b] {
            paint.rect_filled(
                egui::Rect::from_center_size(
                    egui::pos2(x, here.center().y),
                    egui::vec2(5.0, here.height() * 0.8),
                ),
                egui::CornerRadius::same(2),
                if picked { ACCENT } else { ACCENT.gamma_multiply(0.7) },
            );
        }
    }

    // -- dragging ----------------------------------------------------------
    let mut changed = false;
    let snaps: Vec<i64> = lanes
        .iter()
        .filter(|l| l.on)
        .flat_map(|l| [l.span.0, l.span.1])
        .collect();
    let snap = |t: i64, x_of: &dyn Fn(i64) -> f32| -> i64 {
        // The edge of a file is nearly always what is meant, so it pulls.
        let x = x_of(t);
        match snaps
            .iter()
            .copied()
            .min_by(|a, b| (x_of(*a) - x).abs().total_cmp(&(x_of(*b) - x).abs()))
        {
            Some(near) if (x_of(near) - x).abs() <= 6.0 => near,
            // Otherwise land on a whole second, which reads better than the
            // seven decimals a pixel works out to.
            _ => (t / SECOND) * SECOND,
        }
    };

    if response.drag_started()
        && let Some(p) = response.interact_pointer_pos()
    {
        *grab = take_hold(windows, p, rect, &x_of, &t_of, selected);
        if let Some(Grab::New(i, _)) = grab {
            *selected = *i;
            changed = true;
        }
    }

    if let (Some(hold), Some(p)) = (*grab, response.interact_pointer_pos()) {
        let t = snap(t_of(p.x), &x_of);
        match hold {
            Grab::Start(i) => {
                if let Some(w) = windows.get_mut(i) {
                    w.from = Some(t.min(w.until.unwrap_or(i64::MAX)));
                }
            }
            Grab::End(i) => {
                if let Some(w) = windows.get_mut(i) {
                    w.until = Some(t.max(w.from.unwrap_or(i64::MIN)));
                }
            }
            Grab::Whole(i, held) => {
                if let Some(w) = windows.get_mut(i)
                    && let (Some(a), Some(b)) = (w.from, w.until)
                {
                    let width = b - a;
                    let start = (t - held).clamp(lo, hi - width);
                    w.from = Some(start);
                    w.until = Some(start + width);
                }
            }
            Grab::New(i, anchor) => {
                if let Some(w) = windows.get_mut(i) {
                    w.from = Some(anchor.min(t));
                    w.until = Some(anchor.max(t));
                }
            }
        }
        changed = true;
        ui.ctx().set_cursor_icon(match hold {
            Grab::Whole(..) => egui::CursorIcon::Grabbing,
            _ => egui::CursorIcon::ResizeHorizontal,
        });
    }

    if response.drag_stopped() {
        // A window nobody could see was a stray click, not a selection.
        if let Some(Grab::New(i, _)) = *grab
            && windows
                .get(i)
                .and_then(|w| Some(x_of(w.until?) - x_of(w.from?)))
                .unwrap_or(0.0)
                < 3.0
        {
            windows.remove(i);
            *selected = selected.saturating_sub(1);
        }
        *grab = None;
        changed = true;
    }

    // Clicking a window chooses it, so the two fields below edit that one.
    if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
        && let Some(i) = window_at(windows, p.x, rect, &x_of)
    {
        *selected = i;
    }

    // -- the pointer's own time --------------------------------------------
    if let Some(p) = response.hover_pos()
        && grab.is_none()
    {
        let x = p.x.clamp(rect.left(), rect.right());
        paint.vline(
            x,
            band.top()..=band.bottom(),
            egui::Stroke::new(1.0, MUTED.gamma_multiply(0.7)),
        );
        let text = convert::filetime_to_short(t_of(x));
        let at = egui::pos2(x.min(rect.right() - 54.0).max(rect.left() + 54.0), rect.top() + 1.0);
        paint.text(
            at,
            egui::Align2::CENTER_TOP,
            text,
            egui::FontId::proportional(11.0),
            TEXT,
        );
        ui.ctx().set_cursor_icon(match cursor_over(windows, p, rect, &x_of) {
            Some(true) => egui::CursorIcon::ResizeHorizontal,
            Some(false) => egui::CursorIcon::Grab,
            None => egui::CursorIcon::Crosshair,
        });
    }

    changed
}

/// The strip the bars and windows live in.
fn track(rect: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + AXIS_H),
        egui::pos2(rect.right(), rect.bottom() - 6.0),
    )
}

/// What the pointer would take hold of: an edge (true) or the middle (false).
fn cursor_over(
    windows: &[Range],
    p: egui::Pos2,
    rect: egui::Rect,
    x_of: &impl Fn(i64) -> f32,
) -> Option<bool> {
    for w in windows {
        let a = w.from.map_or(rect.left(), x_of);
        let b = w.until.map_or(rect.right(), x_of);
        if (p.x - a).abs() <= GRIP || (p.x - b).abs() <= GRIP {
            return Some(true);
        }
        if p.x > a && p.x < b {
            return Some(false);
        }
    }
    None
}

fn window_at(
    windows: &[Range],
    x: f32,
    rect: egui::Rect,
    x_of: &impl Fn(i64) -> f32,
) -> Option<usize> {
    windows.iter().position(|w| {
        let a = w.from.map_or(rect.left(), x_of);
        let b = w.until.map_or(rect.right(), x_of);
        x >= a - GRIP && x <= b + GRIP
    })
}

/// Decide what a press on the timeline begins: resizing a window, moving one,
/// or drawing a new one on empty track.
fn take_hold(
    windows: &mut Vec<Range>,
    p: egui::Pos2,
    rect: egui::Rect,
    x_of: &impl Fn(i64) -> f32,
    t_of: &impl Fn(f32) -> i64,
    selected: &mut usize,
) -> Option<Grab> {
    for (i, w) in windows.iter().enumerate() {
        let a = w.from.map_or(rect.left(), x_of);
        let b = w.until.map_or(rect.right(), x_of);
        if (p.x - a).abs() <= GRIP {
            *selected = i;
            return Some(Grab::Start(i));
        }
        if (p.x - b).abs() <= GRIP {
            *selected = i;
            return Some(Grab::End(i));
        }
        if p.x > a && p.x < b {
            *selected = i;
            let held = t_of(p.x) - w.from.unwrap_or_else(|| t_of(rect.left()));
            return Some(Grab::Whole(i, held));
        }
    }
    // Dragging on bare track draws another window.
    let anchor = t_of(p.x);
    windows.push(Range {
        from: Some(anchor),
        until: Some(anchor),
    });
    Some(Grab::New(windows.len() - 1, anchor))
}

/// The scale above the bars: a handful of labelled lines, at a step that comes
/// out in whole minutes, hours or days rather than in arbitrary numbers.
fn hours(
    paint: &egui::Painter,
    rect: egui::Rect,
    lo: i64,
    hi: i64,
    x_of: &impl Fn(i64) -> f32,
) {
    const STEPS: [i64; 13] = [
        SECOND,
        10 * SECOND,
        30 * SECOND,
        60 * SECOND,
        5 * 60 * SECOND,
        15 * 60 * SECOND,
        30 * 60 * SECOND,
        3600 * SECOND,
        3 * 3600 * SECOND,
        6 * 3600 * SECOND,
        12 * 3600 * SECOND,
        24 * 3600 * SECOND,
        7 * 24 * 3600 * SECOND,
    ];
    // About one label per 120 pixels keeps them from touching.
    let wanted = ((hi - lo) as f64 / (rect.width() / 120.0).max(1.0) as f64) as i64;
    let step = STEPS
        .iter()
        .copied()
        .find(|s| *s >= wanted)
        .unwrap_or(STEPS[STEPS.len() - 1]);
    let day = step >= 24 * 3600 * SECOND;

    let mut t = (lo / step) * step;
    while t <= hi {
        if t >= lo {
            let x = x_of(t);
            paint.vline(
                x,
                rect.top() + AXIS_H - 3.0..=rect.bottom() - 6.0,
                egui::Stroke::new(1.0, EDGE),
            );
            let stamp = convert::filetime_to_short(t);
            let label = if day {
                stamp[..10].to_string()
            } else {
                // dd.mm HH:MM, which is as much as fits and as much as helps.
                format!("{} {}", &stamp[..5], &stamp[11..16])
            };
            paint.text(
                egui::pos2(x, rect.top()),
                egui::Align2::LEFT_TOP,
                label,
                egui::FontId::proportional(10.0),
                MUTED,
            );
        }
        t += step;
    }
}
