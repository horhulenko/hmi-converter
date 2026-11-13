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
#[derive(Clone, Copy, PartialEq, Debug)]
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
const GRIP: f32 = 14.0;
/// How far from a slice a press on bare track still reaches for its edge,
/// rather than drawing a new slice.
const REACH: f32 = 40.0;
/// 100-nanosecond ticks in a second, as everywhere else in the program.
const SECOND: i64 = 10_000_000;

/// Draw the timeline and let it be edited.
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
) -> Edit {
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
        return Edit::default();
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

    // -- what a press here would take hold of -------------------------------
    // Decided from where the button went down, not where the pointer is once
    // the drag is recognised a few pixels later.
    let press = ui
        .input(|i| i.pointer.press_origin())
        .filter(|p| rect.contains(*p));
    let hover = response.hover_pos();
    let aim = if grab.is_some() {
        None
    } else {
        hover.map(|p| hit(windows, p.x, *selected, rect, &x_of))
    };

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
        let moving = matches!(*grab, Some(Grab::Whole(g, _)) if g == i)
            || aim == Some(Hit::Middle(i));
        let edge = if picked || moving { TEXT } else { MUTED };
        paint.rect(
            here,
            egui::CornerRadius::same(4),
            egui::Color32::from_white_alpha(if moving { 16 } else if picked { 10 } else { 4 }),
            egui::Stroke::new(if picked || moving { 2.0 } else { 1.0 }, edge),
            egui::StrokeKind::Inside,
        );
        // A handle at each end. The one that a press would take, or that is
        // being dragged, lights up -- so it is clear before pressing.
        for (x, end) in [(a, false), (b, true)] {
            let live = aim == Some(Hit::Edge(i, end))
                || matches!(*grab, Some(Grab::Start(g)) if g == i && !end)
                || matches!(*grab, Some(Grab::End(g)) if g == i && end)
                || matches!(*grab, Some(Grab::New(g, _)) if g == i);
            let handle = egui::Rect::from_center_size(
                egui::pos2(x, here.center().y),
                egui::vec2(
                    if live { 12.0 } else { 8.0 },
                    (here.height() * if live { 0.7 } else { 0.5 }).clamp(18.0, 44.0),
                ),
            );
            if live {
                paint.vline(x, band.top()..=band.bottom(), egui::Stroke::new(2.0, ACCENT));
            }
            paint.rect(
                handle,
                egui::CornerRadius::same(4),
                if live { ACCENT } else { edge },
                egui::Stroke::new(1.0, ACCENT_DEEP),
                egui::StrokeKind::Outside,
            );
            let lines = if live { theme::ON_ACCENT } else { ACCENT_DEEP };
            for k in [-1.0, 1.0] {
                paint.hline(
                    (handle.left() + 3.0)..=(handle.right() - 3.0),
                    handle.center().y + k * 3.0,
                    egui::Stroke::new(1.0, lines),
                );
            }
        }
    }

    // -- dragging ----------------------------------------------------------
    let mut edit = Edit::default();
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
            Some(near) if (x_of(near) - x).abs() <= 8.0 => near,
            // Otherwise land on a whole second, which reads better than the
            // seven decimals a pixel works out to.
            _ => (t / SECOND) * SECOND,
        }
    };

    if response.drag_started()
        && let Some(p) = press.or(response.interact_pointer_pos())
    {
        edit.began = true;
        *grab = Some(take_hold(windows, p.x, rect, &x_of, &t_of, selected));
        edit.changed = true;
    }

    if let (Some(hold), Some(p)) = (*grab, response.interact_pointer_pos()) {
        let t = snap(t_of(p.x), &x_of);
        match hold {
            // An edge dragged past the other one simply becomes that one:
            // the slice turns round instead of refusing to move.
            Grab::Start(i) => {
                if let Some(w) = windows.get_mut(i) {
                    match w.until {
                        Some(u) if t > u => {
                            w.from = Some(u);
                            w.until = Some(t);
                            *grab = Some(Grab::End(i));
                        }
                        _ => w.from = Some(t),
                    }
                }
            }
            Grab::End(i) => {
                if let Some(w) = windows.get_mut(i) {
                    match w.from {
                        Some(f) if t < f => {
                            w.until = Some(f);
                            w.from = Some(t);
                            *grab = Some(Grab::Start(i));
                        }
                        _ => w.until = Some(t),
                    }
                }
            }
            Grab::Whole(i, held) => {
                if let Some(w) = windows.get_mut(i)
                    && let (Some(a), Some(b)) = (w.from, w.until)
                {
                    let width = b - a;
                    let start = (t - held).clamp(lo, (hi - width).max(lo));
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
        edit.changed = true;
        ui.ctx().set_cursor_icon(match hold {
            Grab::Whole(..) => egui::CursorIcon::Grabbing,
            _ => egui::CursorIcon::ResizeHorizontal,
        });
    }

    if response.drag_stopped() {
        // A slice nobody could see was a stray click, not a selection.
        if let Some(Grab::New(i, _)) = *grab
            && windows
                .get(i)
                .and_then(|w| Some(x_of(w.until?) - x_of(w.from?)))
                .unwrap_or(0.0)
                < 6.0
        {
            windows.remove(i);
            *selected = selected.saturating_sub(1);
        }
        // Slices that were pushed into each other become one.
        merge(windows, selected);
        *grab = None;
        edit.changed = true;
    }

    // Clicking a slice chooses it, so the two fields below edit that one.
    if response.clicked()
        && let Some(p) = response.interact_pointer_pos()
        && let Hit::Edge(i, _) | Hit::Middle(i) = hit(windows, p.x, *selected, rect, &x_of)
    {
        *selected = i;
    }

    // -- the pointer's own time --------------------------------------------
    if let (Some(p), Some(aim)) = (hover, aim) {
        let x = p.x.clamp(rect.left(), rect.right());
        if aim == Hit::Bare {
            paint.vline(x, band.top()..=band.bottom(), egui::Stroke::new(1.0, ACCENT));
        }
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
        ui.ctx().set_cursor_icon(match aim {
            Hit::Edge(..) => egui::CursorIcon::ResizeHorizontal,
            Hit::Middle(_) => egui::CursorIcon::Grab,
            Hit::Bare => egui::CursorIcon::Crosshair,
        });
    }

    edit
}

/// What happened on the timeline this frame.
#[derive(Default, Clone, Copy)]
pub struct Edit {
    /// The slices changed.
    pub changed: bool,
    /// A drag began: the slices as they were a moment ago are worth keeping,
    /// to go back to.
    pub began: bool,
}

/// What a press at some point of the timeline takes hold of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    /// An edge of slice `i`: the end if true, the start if false.
    Edge(usize, bool),
    /// The middle of slice `i`, to move it whole.
    Middle(usize),
    /// Bare track, far from every slice: a new slice is drawn.
    Bare,
}

/// Decide what a press at `x` means, leaning hard towards what was probably
/// wanted: an edge is caught from well outside it, and a press on bare track
/// near a slice reaches for that slice's nearest edge rather than starting a
/// second slice beside it. Only a press clearly away from every slice draws
/// a new one.
fn hit(
    windows: &[Range],
    x: f32,
    selected: usize,
    rect: egui::Rect,
    x_of: &impl Fn(i64) -> f32,
) -> Hit {
    let ends = |w: &Range| {
        (
            w.from.map_or(rect.left(), x_of),
            w.until.map_or(rect.right(), x_of),
        )
    };

    // 1. An edge within reach. Inside a narrow slice the reach shrinks to a
    //    third of it, so there is still a middle to move it by.
    let mut best: Option<(f32, bool, Hit)> = None;
    for (i, w) in windows.iter().enumerate() {
        let (a, b) = ends(w);
        let inner = GRIP.min((b - a) / 3.0);
        for (edge_x, end) in [(a, false), (b, true)] {
            let inside = if end { x < edge_x } else { x > edge_x };
            let d = (x - edge_x).abs();
            if d <= if inside { inner } else { GRIP } {
                // Nearest wins; between equals, the chosen slice wins.
                let key = (d, i != selected);
                if best.is_none_or(|(bd, bo, _)| key < (bd, bo)) {
                    best = Some((d, i != selected, Hit::Edge(i, end)));
                }
            }
        }
    }
    if let Some((_, _, h)) = best {
        return h;
    }

    // 2. Inside a slice: move it.
    if let Some(i) = windows.iter().position(|w| {
        let (a, b) = ends(w);
        x > a && x < b
    }) {
        return Hit::Middle(i);
    }

    // 3. Near a slice, though not on it: its nearest edge -- dragging out from
    //    just beside a slice nearly always means making it bigger.
    windows
        .iter()
        .enumerate()
        .flat_map(|(i, w)| {
            let (a, b) = ends(w);
            [((x - a).abs(), Hit::Edge(i, false)), ((x - b).abs(), Hit::Edge(i, true))]
        })
        .filter(|(d, _)| *d <= REACH)
        .min_by(|p, q| p.0.total_cmp(&q.0))
        .map_or(Hit::Bare, |(_, h)| h)
}

/// Begin a drag at `x`: resize, move, or draw a new slice on bare track.
fn take_hold(
    windows: &mut Vec<Range>,
    x: f32,
    rect: egui::Rect,
    x_of: &impl Fn(i64) -> f32,
    t_of: &impl Fn(f32) -> i64,
    selected: &mut usize,
) -> Grab {
    match hit(windows, x, *selected, rect, x_of) {
        Hit::Edge(i, end) => {
            *selected = i;
            if end { Grab::End(i) } else { Grab::Start(i) }
        }
        Hit::Middle(i) => {
            *selected = i;
            let w = windows[i];
            let held = t_of(x) - w.from.unwrap_or_else(|| t_of(rect.left()));
            Grab::Whole(i, held)
        }
        Hit::Bare => {
            let anchor = t_of(x);
            windows.push(Range {
                from: Some(anchor),
                until: Some(anchor),
            });
            *selected = windows.len() - 1;
            Grab::New(windows.len() - 1, anchor)
        }
    }
}

/// Put the slices in time order and join any that overlap or touch, keeping
/// the chosen one chosen.
fn merge(windows: &mut Vec<Range>, selected: &mut usize) {
    if windows.len() < 2 {
        return;
    }
    let lo = |w: &Range| w.from.unwrap_or(i64::MIN);
    let hi = |w: &Range| w.until.unwrap_or(i64::MAX);
    let chosen = windows.get(*selected).copied();
    windows.sort_by_key(|w| lo(w));
    let mut out: Vec<Range> = Vec::with_capacity(windows.len());
    for w in windows.drain(..) {
        match out.last_mut() {
            Some(last) if lo(&w) <= hi(last) => {
                if hi(&w) > hi(last) {
                    last.until = w.until;
                }
            }
            _ => out.push(w),
        }
    }
    *windows = out;
    *selected = chosen
        .and_then(|c| {
            windows
                .iter()
                .position(|w| lo(w) <= lo(&c) && hi(&c) <= hi(w))
        })
        .unwrap_or(0)
        .min(windows.len().saturating_sub(1));
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One pixel per tick, over a 1000-pixel track.
    fn track() -> (egui::Rect, impl Fn(i64) -> f32) {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 100.0));
        (rect, |t: i64| t as f32)
    }

    fn slice(a: i64, b: i64) -> Range {
        Range {
            from: Some(a),
            until: Some(b),
        }
    }

    #[test]
    fn an_edge_is_caught_from_well_outside_it() {
        let (rect, x_of) = track();
        let w = [slice(100, 300)];
        assert_eq!(hit(&w, 300.0, 0, rect, &x_of), Hit::Edge(0, true));
        assert_eq!(hit(&w, 290.0, 0, rect, &x_of), Hit::Edge(0, true));
        assert_eq!(hit(&w, 312.0, 0, rect, &x_of), Hit::Edge(0, true));
        // Beside the slice, on bare track, the edge still answers -- this is
        // the press that used to start a second slice by mistake.
        assert_eq!(hit(&w, 335.0, 0, rect, &x_of), Hit::Edge(0, true));
        assert_eq!(hit(&w, 70.0, 0, rect, &x_of), Hit::Edge(0, false));
    }

    #[test]
    fn the_middle_moves_and_far_away_draws_anew() {
        let (rect, x_of) = track();
        let w = [slice(100, 300)];
        assert_eq!(hit(&w, 200.0, 0, rect, &x_of), Hit::Middle(0));
        assert_eq!(hit(&w, 600.0, 0, rect, &x_of), Hit::Bare);
    }

    #[test]
    fn a_narrow_slice_keeps_a_middle() {
        let (rect, x_of) = track();
        let w = [slice(100, 118)];
        assert_eq!(hit(&w, 109.0, 0, rect, &x_of), Hit::Middle(0));
        assert_eq!(hit(&w, 101.0, 0, rect, &x_of), Hit::Edge(0, false));
        assert_eq!(hit(&w, 125.0, 0, rect, &x_of), Hit::Edge(0, true));
    }

    #[test]
    fn the_nearer_edge_wins_between_two_slices() {
        let (rect, x_of) = track();
        let w = [slice(100, 300), slice(330, 500)];
        assert_eq!(hit(&w, 310.0, 0, rect, &x_of), Hit::Edge(0, true));
        assert_eq!(hit(&w, 322.0, 0, rect, &x_of), Hit::Edge(1, false));
    }

    #[test]
    fn overlapping_slices_become_one() {
        let mut w = vec![slice(400, 600), slice(100, 300), slice(250, 450)];
        let mut selected = 2;
        merge(&mut w, &mut selected);
        assert_eq!(w, vec![slice(100, 600)]);
        assert_eq!(selected, 0);

        let mut apart = vec![slice(500, 600), slice(100, 200)];
        let mut selected = 0;
        merge(&mut apart, &mut selected);
        assert_eq!(apart, vec![slice(100, 200), slice(500, 600)]);
        // Still the same slice chosen, now in second place.
        assert_eq!(selected, 1);
    }
}
