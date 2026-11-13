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
use crate::theme::{self, ACCENT, ACCENT_DEEP, DIM, EDGE, MUTED, SURFACE, TEXT};

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

/// Room for the time labels above the bars.
const AXIS_H: f32 = 24.0;
/// The height the bars share between them, and the limits on one bar.
const LANES_H: f32 = 96.0;
const LANE_MIN: f32 = 14.0;
const LANE_MAX: f32 = 30.0;
const LANE_GAP: f32 = 4.0;
/// Inside the track, above the first bar and below the last.
const TRACK_PAD: f32 = 8.0;
/// How many lanes are drawn one above the other before they are stacked into
/// a single band -- past this the bars would be too thin to read.
const MAX_LANES: usize = 7;
/// The height of that single band.
const STACKED_H: f32 = 56.0;
/// How close the pointer has to be to an edge to take hold of it.
const GRIP: f32 = 7.0;
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
    let stacked = lanes.len() > MAX_LANES;
    let rows = lanes.len().max(1);
    let lane_h = if stacked {
        STACKED_H
    } else {
        (LANES_H / rows as f32 - LANE_GAP).clamp(LANE_MIN, LANE_MAX)
    };
    let lanes_h = if stacked {
        STACKED_H
    } else {
        rows as f32 * (lane_h + LANE_GAP) - LANE_GAP
    };
    // Never shorter than a comfortable target, however few files there are.
    let track_h = (lanes_h + TRACK_PAD * 2.0).max(LANES_H * 0.75);
    let height = AXIS_H + track_h;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click_and_drag(),
    );
    let paint = ui.painter().with_clip_rect(rect.expand(2.0));

    // Nothing loaded yet: one empty box the size of the whole picture, so the
    // card is the same height before and after the first file arrives.
    let Some((t0, t1)) = bounds.filter(|(a, b)| b > a) else {
        let empty = egui::Rect::from_min_max(
            egui::pos2(rect.left(), rect.top() + AXIS_H),
            rect.max,
        );
        paint.rect_filled(empty, egui::CornerRadius::same(8), SURFACE);
        paint.text(
            empty.center(),
            egui::Align2::CENTER_CENTER,
            "Hier erscheint der Verlauf, sobald Dateien geladen sind",
            egui::FontId::proportional(13.0),
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

    // -- the files ---------------------------------------------------------
    let band = egui::Rect::from_min_max(egui::pos2(rect.left(), rect.top() + AXIS_H), rect.max);
    paint.rect_filled(band, egui::CornerRadius::same(8), SURFACE);
    hours(&paint, rect, band, lo, hi, &x_of);
    let first_top = band.center().y - lanes_h / 2.0;
    let mut under_pointer: Option<&Lane> = None;
    for (i, lane) in lanes.iter().enumerate() {
        let row = if stacked { 0 } else { i };
        let top = first_top + row as f32 * (lane_h + LANE_GAP);
        let bar = egui::Rect::from_min_max(
            egui::pos2(x_of(lane.span.0), top),
            egui::pos2(x_of(lane.span.1).max(x_of(lane.span.0) + 2.0), top + lane_h),
        );
        let color = if lane.on {
            ACCENT.gamma_multiply(if stacked { 0.7 } else { 1.0 })
        } else {
            DIM
        };
        // The file covers all of this...
        paint.rect_filled(bar, egui::CornerRadius::same(3), color.gamma_multiply(0.16));
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
                let strength = (*n as f32 / busiest).sqrt().clamp(0.3, 1.0);
                paint.rect_filled(
                    egui::Rect::from_min_size(egui::pos2(x, bar.top()), egui::vec2(wide, bar.height())),
                    egui::CornerRadius::ZERO,
                    color.gamma_multiply(strength * if stacked { 0.7 } else { 1.0 }),
                );
            }
        } else {
            paint.rect_filled(bar, egui::CornerRadius::same(3), color.gamma_multiply(0.8));
        }
        // Which file is which, on hover -- there is no room to write it.
        if let Some(p) = response.hover_pos()
            && bar.expand2(egui::vec2(0.0, LANE_GAP / 2.0)).contains(p)
            && under_pointer.is_none()
        {
            under_pointer = Some(lane);
        }
    }
    if let Some(lane) = under_pointer {
        response.clone().show_tooltip_ui(|ui| {
            ui.label(theme::strong(lane.name.clone()));
            ui.label(theme::muted(format!(
                "{} – {}",
                convert::filetime_to_short(lane.span.0),
                convert::filetime_to_short(lane.span.1)
            )));
        });
    }

    // -- what is left out --------------------------------------------------
    let shade = egui::Color32::from_rgba_premultiplied(0x0B, 0x0D, 0x10, 200);
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
        let edge = if picked { TEXT } else { MUTED };
        paint.rect(
            here,
            egui::CornerRadius::same(4),
            egui::Color32::from_white_alpha(if picked { 10 } else { 4 }),
            egui::Stroke::new(if picked { 2.0 } else { 1.0 }, edge),
            egui::StrokeKind::Inside,
        );
        // A handle at each end, to show where to take hold.
        for x in [a, b] {
            let handle = egui::Rect::from_center_size(
                egui::pos2(x, here.center().y),
                egui::vec2(8.0, (here.height() * 0.5).clamp(18.0, 34.0)),
            );
            paint.rect(
                handle,
                egui::CornerRadius::same(3),
                edge,
                egui::Stroke::new(1.0, ACCENT_DEEP),
                egui::StrokeKind::Outside,
            );
            for k in [-1.0, 1.0] {
                paint.hline(
                    (handle.left() + 2.0)..=(handle.right() - 2.0),
                    handle.center().y + k * 3.0,
                    egui::Stroke::new(1.0, ACCENT_DEEP),
                );
            }
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
        paint.vline(x, band.top()..=band.bottom(), egui::Stroke::new(1.0, ACCENT));
        // The exact time under the pointer, in a pill over the scale.
        let galley = paint.layout_no_wrap(
            convert::filetime_to_short(t_of(x)),
            egui::FontId::proportional(12.0),
            theme::ON_ACCENT,
        );
        let size = galley.size() + egui::vec2(14.0, 4.0);
        let left = (x - size.x / 2.0).clamp(rect.left(), rect.right() - size.x);
        let pill = egui::Rect::from_min_size(egui::pos2(left, rect.top()), size);
        paint.rect_filled(pill, egui::CornerRadius::same(4), ACCENT);
        paint.galley(pill.min + egui::vec2(7.0, 2.0), galley, theme::ON_ACCENT);
        ui.ctx().set_cursor_icon(match cursor_over(windows, p, rect, &x_of) {
            Some(true) => egui::CursorIcon::ResizeHorizontal,
            Some(false) => egui::CursorIcon::Grab,
            None => egui::CursorIcon::Crosshair,
        });
    }

    changed
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
    band: egui::Rect,
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
    // About one label per 130 pixels keeps them from touching.
    let wanted = ((hi - lo) as f64 / (rect.width() / 130.0).max(1.0) as f64) as i64;
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
            let stamp = convert::filetime_to_short(t);
            // Midnight is where the date changes, so it is drawn brighter and
            // says which day begins.
            let midnight = &stamp[11..19] == "00:00:00";
            paint.vline(
                x,
                band.top()..=band.bottom(),
                egui::Stroke::new(1.0, if midnight { DIM } else { EDGE }),
            );
            let label = if day || midnight {
                format!("{}.", &stamp[..5])
            } else {
                stamp[11..16].to_string()
            };
            paint.text(
                egui::pos2(x, rect.top() + 4.0),
                egui::Align2::CENTER_TOP,
                label,
                egui::FontId::proportional(12.0),
                if midnight || day { TEXT } else { MUTED },
            );
        }
        t += step;
    }
}
