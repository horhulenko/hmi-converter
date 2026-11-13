//! The chart drawn inside the window.
//!
//! This is the second way to look at a log. The browser page is richer in kinds
//! of chart; this one is richer in feel: it redraws every frame, so panning,
//! zooming and switching tags on and off happen under the hand with no waiting
//! and no file in between.

use eframe::egui;
use egui_plot::{
    Bar, BarChart, BoxElem, BoxPlot, BoxSpread, CoordinatesFormatter, Corner, FilledArea,
    HoverPosition, Legend, Line, MarkerShape, Plot, PlotPoint, Points,
};

use crate::chart::Loaded;
use crate::convert;
use crate::theme::{self, ACCENT, DIM, MUTED, body, hint, muted, strong};

/// The shapes this view can draw.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Lines,
    Area,
    Steps,
    Scatter,
    Multiples,
    Histogram,
    Box,
    Correlation,
}

impl Kind {
    const ALL: [Self; 8] = [
        Self::Lines,
        Self::Area,
        Self::Steps,
        Self::Scatter,
        Self::Multiples,
        Self::Histogram,
        Self::Box,
        Self::Correlation,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Lines => "Linien",
            Self::Area => "Fläche",
            Self::Steps => "Stufen",
            Self::Scatter => "Punkte",
            Self::Multiples => "Einzeln",
            Self::Histogram => "Histogramm",
            Self::Box => "Boxplot",
            Self::Correlation => "Korrelation",
        }
    }

    /// Whether the horizontal axis is time.
    fn over_time(self) -> bool {
        matches!(
            self,
            Self::Lines | Self::Area | Self::Steps | Self::Scatter | Self::Multiples
        )
    }
}

/// Everything the in-window chart remembers between frames.
pub struct PlotView {
    data: Loaded,
    /// The names as shown: what every tag has in common is dropped, because a
    /// narrow list of `...:Archivvariable_1` tells nobody anything.
    display: Vec<String>,
    colours: Vec<egui::Color32>,
    on: Vec<bool>,
    filter: String,
    kind: Kind,
    log_y: bool,
    normalise: bool,
    crosshair: bool,
    /// The two tags of the correlation plot.
    x: usize,
    y: usize,
    bins: usize,
    /// Set for one frame to make the plots forget their zoom.
    reset: bool,
    /// The time under the pointer, for the readout beside the chart.
    cursor: Option<f64>,
}

impl PlotView {
    pub fn new(data: Loaded) -> Self {
        let count = data.series.len();
        let colours = (0..count).map(colour_of).collect();
        let display = convert::trim_common(
            &data
                .series
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
        );

        // A hundred lines at once hide each other, so start with the busiest.
        let mut on = vec![count <= 12; count];
        if count > 12 {
            let mut by_size: Vec<usize> = (0..count).collect();
            by_size.sort_by_key(|&i| std::cmp::Reverse(data.series[i].points.len()));
            for &i in by_size.iter().take(8) {
                on[i] = true;
            }
        }

        Self {
            colours,
            display,
            on,
            filter: String::new(),
            kind: Kind::Lines,
            log_y: false,
            normalise: false,
            crosshair: true,
            x: 0,
            y: 1.min(count.saturating_sub(1)),
            bins: 20,
            reset: false,
            cursor: None,
            data,
        }
    }

    fn chosen(&self) -> Vec<usize> {
        (0..self.data.series.len())
            .filter(|&i| self.on[i])
            .collect()
    }

    /// The points of one tag as the chart wants them: normalised and made
    /// log-safe if those are asked for.
    fn points(&self, i: usize) -> Vec<[f64; 2]> {
        let s = &self.data.series[i];
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        if self.normalise {
            for (_, v) in &s.points {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
        }
        let span = hi - lo;

        s.points
            .iter()
            .filter_map(|(t, v)| {
                let mut v = *v;
                if self.normalise {
                    v = if span > 0.0 {
                        (v - lo) / span * 100.0
                    } else {
                        50.0
                    };
                }
                // A log axis has no place for zero or less, so those go.
                if self.log_y {
                    if v <= 0.0 {
                        return None;
                    }
                    v = v.log10();
                }
                Some([*t as f64, v])
            })
            .collect()
    }

    /// The reading that still stands at `t` -- tags do not log in step, so this
    /// is what the HMI itself would be showing.
    fn value_at(&self, i: usize, t: f64) -> Option<(i64, f64)> {
        let p = &self.data.series[i].points;
        if p.is_empty() || t < p[0].0 as f64 {
            return None;
        }
        let (mut lo, mut hi) = (0usize, p.len() - 1);
        while lo < hi {
            let mid = lo.midpoint(hi + 1);
            if (p[mid].0 as f64) <= t {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Some(p[lo])
    }

    // ------------------------------------------------------------ drawing

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.controls(ui);
        ui.add_space(theme::GAP);
        egui::Panel::left(ui.id().with("tags"))
            .resizable(true)
            .default_size(300.0)
            .show_separator_line(false)
            .frame(theme::panel().inner_margin(egui::Margin::same(14)))
            .show(ui, |ui| self.side(ui));
        ui.add_space(theme::GAP);
        self.plot(ui);
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        {
            ui.horizontal_wrapped(|ui| {
                let options: Vec<(Kind, &str, &str)> =
                    Kind::ALL.iter().map(|k| (*k, k.label(), "")).collect();
                if theme::segmented(ui, &mut self.kind, &options) {
                    self.reset = true;
                }
                ui.add_space(12.0);
                if theme::check(ui, &mut self.log_y, "Logarithmisch")
                    .on_hover_text("Werte von null oder kleiner lassen sich darauf nicht zeigen")
                    .changed()
                {
                    self.reset = true;
                }
                if theme::check(ui, &mut self.normalise, "Normiert 0–100 %")
                    .on_hover_text("Jeder Tag in seinem eigenen Bereich, für Tags in verschiedenen Einheiten")
                    .changed()
                {
                    self.reset = true;
                }
                theme::check(ui, &mut self.crosshair, "Fadenkreuz");

                if self.kind == Kind::Correlation {
                    ui.add_space(12.0);
                    self.tag_picker(ui, "X", true);
                    self.tag_picker(ui, "Y", false);
                }
                if self.kind == Kind::Histogram {
                    ui.add_space(12.0);
                    ui.label(hint("Klassen"));
                    if ui
                        .add(egui::Slider::new(&mut self.bins, 5..=80).logarithmic(false))
                        .changed()
                    {
                        self.reset = true;
                    }
                }

                ui.add_space(12.0);
                if theme::quiet(ui, "Ansicht zurücksetzen")
                    .on_hover_text("Wieder alles im Blick. Ein Doppelklick ins Diagramm tut dasselbe.")
                    .clicked()
                {
                    self.reset = true;
                }
            });
        }
    }

    fn tag_picker(&mut self, ui: &mut egui::Ui, label: &str, is_x: bool) {
        let current = if is_x { self.x } else { self.y };
        let name = self.display.get(current).cloned().unwrap_or_default();
        ui.label(hint(label));
        egui::ComboBox::from_id_salt(if is_x { "x-tag" } else { "y-tag" })
            .selected_text(name)
            .width(260.0)
            .show_ui(ui, |ui| {
                for (i, name) in self.display.iter().enumerate() {
                    let picked = i == current;
                    if ui.selectable_label(picked, name).clicked() {
                        if is_x {
                            self.x = i;
                        } else {
                            self.y = i;
                        }
                        self.reset = true;
                    }
                }
            });
    }

    /// The tag list, and under it the readout of what stands at the cursor.
    fn side(&mut self, ui: &mut egui::Ui) {
        {
            {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Tags filtern …")
                            .desired_width((ui.available_width() - 76.0).max(70.0))
                            .margin(egui::vec2(8.0, 6.0)),
                    );
                    ui.label(muted(format!(
                        "{} von {}",
                        self.chosen().len(),
                        self.data.series.len()
                    )));
                });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let shown: Vec<usize> = (0..self.data.series.len())
                        .filter(|&i| self.matches(i))
                        .collect();
                    if theme::quiet(ui, "Alle").clicked() {
                        for i in &shown {
                            self.on[*i] = true;
                        }
                    }
                    if theme::quiet(ui, "Keine").clicked() {
                        for i in &shown {
                            self.on[*i] = false;
                        }
                    }
                    if theme::quiet(ui, "Umkehren").clicked() {
                        for i in &shown {
                            self.on[*i] = !self.on[*i];
                        }
                    }
                });
                ui.add_space(6.0);

                let readout = self.readout();
                let list_height = (ui.available_height() - readout_height(&readout)).max(90.0);
                egui::ScrollArea::vertical()
                    .id_salt("plot-tags")
                    .max_height(list_height)
                    .show(ui, |ui| {
                        for i in 0..self.data.series.len() {
                            if !self.matches(i) {
                                continue;
                            }
                            ui.horizontal(|ui| {
                                theme::check(ui, &mut self.on[i], "");
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(10.0, 10.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    egui::CornerRadius::same(2),
                                    if self.on[i] {
                                        self.colours[i]
                                    } else {
                                        self.colours[i].gamma_multiply(0.3)
                                    },
                                );
                                let name = self.display[i].clone();
                                let text = if self.on[i] {
                                    body(name)
                                } else {
                                    body(name).color(DIM)
                                };
                                ui.add(egui::Label::new(text).wrap()).on_hover_text(format!(
                                    "{}\n{} Punkte",
                                    self.data.series[i].name,
                                    self.data.series[i].points.len()
                                ));
                            });
                        }
                    });

                if !readout.is_empty() {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(4.0);
                    let at = self.cursor.unwrap_or_default() as i64;
                    ui.label(strong(convert::filetime_to_short(convert::ms_to_ticks(at))));
                    egui::ScrollArea::vertical()
                        .id_salt("plot-readout")
                        .max_height(readout_height(&readout) - 30.0)
                        .show(ui, |ui| {
                            for (i, value, held) in &readout {
                                ui.horizontal(|ui| {
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(8.0, 8.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().rect_filled(
                                        rect,
                                        egui::CornerRadius::same(2),
                                        self.colours[*i],
                                    );
                                    ui.add(
                                        egui::Label::new(muted(self.display[*i].clone())).wrap(),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let text = format_value(*value);
                                            ui.label(if *held {
                                                body(text).color(MUTED)
                                            } else {
                                                body(text)
                                            });
                                        },
                                    );
                                });
                            }
                        });
                }
            }
        }
    }

    fn matches(&self, i: usize) -> bool {
        let q = self.filter.trim().to_lowercase();
        q.is_empty() || self.data.series[i].name.to_lowercase().contains(&q)
    }

    /// Every chosen tag at the cursor, largest first.
    fn readout(&self) -> Vec<(usize, f64, bool)> {
        let Some(t) = self.cursor else {
            return Vec::new();
        };
        if !self.kind.over_time() {
            return Vec::new();
        }
        let mut rows: Vec<(usize, f64, bool)> = self
            .chosen()
            .into_iter()
            .filter_map(|i| {
                self.value_at(i, t)
                    .map(|(at, v)| (i, v, t - at as f64 > 60_000.0))
            })
            .collect();
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        rows
    }

    fn plot(&mut self, ui: &mut egui::Ui) {
        let ids = self.chosen();
        if ids.is_empty() && self.kind != Kind::Correlation {
            theme::panel().show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.centered_and_justified(|ui| {
                    ui.label(hint("Links mindestens einen Tag auswählen."))
                });
            });
            return;
        }

        theme::panel().show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match self.kind {
                Kind::Multiples => self.multiples(ui, &ids),
                Kind::Histogram => self.histogram(ui, &ids),
                Kind::Box => self.boxes(ui, &ids),
                Kind::Correlation => self.correlation(ui),
                _ => self.over_time(ui, &ids),
            }
        });
        self.reset = false;
    }

    /// The common shell: time across the bottom, values up the side.
    fn base<'a>(&self, id: &'static str) -> Plot<'a> {
        let log = self.log_y;
        let plot = Plot::new(id)
            .allow_boxed_zoom(true)
            .allow_double_click_reset(true)
            .show_crosshair(self.crosshair)
            .show_grid([true, true])
            .grid_color(theme::EDGE)
            .cursor_color(ACCENT)
            .show_background(false)
            .y_axis_min_width(58.0)
            .x_axis_formatter(|mark, _| time_label(mark.value))
            .y_axis_formatter(move |mark, _| {
                if log {
                    format_value(10f64.powf(mark.value))
                } else {
                    format_value(mark.value)
                }
            });
        if log {
            // Ticks at the powers of ten, since the values are their logarithms.
            plot.y_grid_spacer(|input| {
                let (lo, hi) = input.bounds;
                let mut marks = Vec::new();
                let mut e = lo.floor();
                while e <= hi.ceil() && marks.len() < 64 {
                    marks.push(egui_plot::GridMark {
                        value: e,
                        step_size: 1.0,
                    });
                    e += 1.0;
                }
                marks
            })
        } else {
            plot
        }
    }

    fn over_time(&mut self, ui: &mut egui::Ui, ids: &[usize]) {
        let kind = self.kind;
        let log = self.log_y;
        let names: Vec<String> = ids.iter().map(|&i| self.display[i].clone()).collect();
        let curves: Vec<Vec<[f64; 2]>> = ids
            .iter()
            .map(|&i| {
                let p = self.points(i);
                if kind == Kind::Steps { stairs(&p) } else { p }
            })
            .collect();
        let colours: Vec<egui::Color32> = ids.iter().map(|&i| self.colours[i]).collect();

        let mut plot = self
            .base("over-time")
            .legend(
                Legend::default()
                    .position(Corner::RightTop)
                    .background_alpha(0.85)
                    .text_style(egui::TextStyle::Small),
            )
            .coordinates_formatter(
                Corner::LeftBottom,
                CoordinatesFormatter::new(move |p: &PlotPoint, _| {
                    let v = if log { 10f64.powf(p.y) } else { p.y };
                    format!("{}   {}", time_label(p.x), format_value(v))
                }),
            )
            .label_formatter(move |pos| match pos {
                HoverPosition::NearDataPoint {
                    plot_name,
                    position,
                    ..
                } => {
                    let v = if log {
                        10f64.powf(position.y)
                    } else {
                        position.y
                    };
                    Some(format!(
                        "{plot_name}\n{}\n{}",
                        time_label(position.x),
                        format_value(v)
                    ))
                }
                HoverPosition::Elsewhere { .. } => None,
            });
        if self.reset {
            plot = plot.reset();
        }

        let response = plot.show(ui, |plot_ui| {
            for (k, curve) in curves.iter().enumerate() {
                match kind {
                    Kind::Scatter => plot_ui.points(
                        Points::new(names[k].clone(), curve.clone())
                            .color(colours[k])
                            .radius(1.8)
                            .shape(MarkerShape::Circle),
                    ),
                    Kind::Area => {
                        let xs: Vec<f64> = curve.iter().map(|p| p[0]).collect();
                        let ys: Vec<f64> = curve.iter().map(|p| p[1]).collect();
                        let floor = ys.iter().copied().fold(f64::INFINITY, f64::min).min(0.0);
                        let base = vec![floor; ys.len()];
                        plot_ui.add(
                            FilledArea::new(names[k].clone(), &xs, &base, &ys)
                                .fill_color(colours[k].gamma_multiply(0.22))
                                .stroke(egui::Stroke::new(1.4, colours[k])),
                        );
                    }
                    _ => plot_ui.line(
                        Line::new(names[k].clone(), curve.clone())
                            .color(colours[k])
                            .width(1.4),
                    ),
                }
            }
            plot_ui.pointer_coordinate().map(|p| p.x)
        });

        self.cursor = response.inner;
    }

    /// One little chart per tag, all sharing a time axis: drag one, all move.
    fn multiples(&mut self, ui: &mut egui::Ui, ids: &[usize]) {
        let link = ui.id().with("multiples-link");
        let columns = if ids.len() <= 2 {
            1
        } else if ids.len() <= 6 {
            2
        } else {
            3
        };
        let rows = ids.len().div_ceil(columns);
        let spacing = ui.spacing().item_spacing;
        let cell = egui::vec2(
            (ui.available_width() - spacing.x * (columns as f32 - 1.0)) / columns as f32,
            ((ui.available_height() - spacing.y * (rows as f32 - 1.0)) / rows as f32).max(110.0),
        );

        let mut cursor = None;
        egui::ScrollArea::vertical()
            .id_salt("multiples")
            .show(ui, |ui| {
                for row in 0..rows {
                    ui.horizontal(|ui| {
                        for column in 0..columns {
                            let Some(&i) = ids.get(row * columns + column) else {
                                break;
                            };
                            let name = self.display[i].clone();
                            let colour = self.colours[i];
                            let curve = self.points(i);
                            ui.allocate_ui(cell, |ui| {
                                ui.vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new(name.clone())
                                            .small()
                                            .color(colour),
                                    );
                                    let mut plot = self
                                        .base("multi")
                                        .id(ui.id().with(i))
                                        // One time axis for all of them, so
                                        // panning one pans the lot.
                                        .link_axis(link, [true, false])
                                        .link_cursor(link, [true, false])
                                        .height(cell.y - 22.0)
                                        .width(cell.x);
                                    if self.reset {
                                        plot = plot.reset();
                                    }
                                    let r = plot.show(ui, |plot_ui| {
                                        plot_ui.line(
                                            Line::new(name.clone(), curve.clone())
                                                .color(colour)
                                                .width(1.2),
                                        );
                                        plot_ui.pointer_coordinate().map(|p| p.x)
                                    });
                                    if r.inner.is_some() {
                                        cursor = r.inner;
                                    }
                                });
                            });
                        }
                    });
                }
            });
        self.cursor = cursor;
    }

    fn histogram(&mut self, ui: &mut egui::Ui, ids: &[usize]) {
        let bins = self.bins;
        let mut charts = Vec::new();
        for &i in ids {
            let values: Vec<f64> = self.points(i).iter().map(|p| p[1]).collect();
            if values.is_empty() {
                continue;
            }
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            for v in &values {
                lo = lo.min(*v);
                hi = hi.max(*v);
            }
            if lo == hi {
                lo -= 0.5;
                hi += 0.5;
            }
            let width = (hi - lo) / bins as f64;
            let mut counts = vec![0u32; bins];
            for v in &values {
                let b = (((v - lo) / width) as usize).min(bins - 1);
                counts[b] += 1;
            }
            let bars: Vec<Bar> = counts
                .iter()
                .enumerate()
                .map(|(b, c)| {
                    Bar::new(lo + (b as f64 + 0.5) * width, *c as f64)
                        .width(width * 0.9)
                        .fill(self.colours[i].gamma_multiply(0.8))
                })
                .collect();
            charts.push(BarChart::new(self.display[i].clone(), bars).color(self.colours[i]));
        }

        let log = self.log_y;
        let mut plot = Plot::new("histogram")
            .legend(Legend::default().position(Corner::RightTop))
            .show_background(false)
            .allow_boxed_zoom(true)
            .y_axis_min_width(52.0)
            .x_axis_formatter(move |m, _| {
                format_value(if log { 10f64.powf(m.value) } else { m.value })
            })
            .x_axis_label("Wert")
            .y_axis_label("Anzahl");
        if self.reset {
            plot = plot.reset();
        }
        plot.show(ui, |plot_ui| {
            for c in charts {
                plot_ui.bar_chart(c);
            }
        });
        self.cursor = None;
    }

    fn boxes(&mut self, ui: &mut egui::Ui, ids: &[usize]) {
        let mut elements = Vec::new();
        let mut labels: Vec<(f64, String)> = Vec::new();
        for (k, &i) in ids.iter().enumerate() {
            let mut values: Vec<f64> = self.points(i).iter().map(|p| p[1]).collect();
            if values.is_empty() {
                continue;
            }
            values.sort_by(f64::total_cmp);
            let at = |q: f64| {
                let pos = (values.len() - 1) as f64 * q;
                let base = pos.floor() as usize;
                let rest = pos - base as f64;
                match values.get(base + 1) {
                    Some(next) => values[base] + rest * (next - values[base]),
                    None => values[base],
                }
            };
            let spread = BoxSpread::new(
                values[0],
                at(0.25),
                at(0.5),
                at(0.75),
                values[values.len() - 1],
            );
            let name = self.display[i].clone();
            elements.push(
                BoxElem::new(k as f64, spread)
                    .name(name.clone())
                    .fill(self.colours[i].gamma_multiply(0.35))
                    .stroke(egui::Stroke::new(1.4, self.colours[i])),
            );
            labels.push((k as f64, name));
        }

        let log = self.log_y;
        let shown = labels.clone();
        let mut plot = Plot::new("boxes")
            .show_background(false)
            .allow_boxed_zoom(true)
            .y_axis_min_width(58.0)
            .y_axis_formatter(move |m, _| {
                format_value(if log { 10f64.powf(m.value) } else { m.value })
            })
            .x_axis_formatter(move |m, _| {
                shown
                    .iter()
                    .find(|(at, _)| (at - m.value).abs() < 0.01)
                    .map(|(_, n)| n.clone())
                    .unwrap_or_default()
            });
        if self.reset {
            plot = plot.reset();
        }
        plot.show(ui, |plot_ui| {
            plot_ui.box_plot(BoxPlot::new("spread", elements));
        });
        self.cursor = None;
    }

    fn correlation(&mut self, ui: &mut egui::Ui) {
        let (Some(a), Some(b)) = (self.data.series.get(self.x), self.data.series.get(self.y))
        else {
            ui.centered_and_justified(|ui| ui.label(hint("Oben zwei Tags auswählen.")));
            return;
        };

        // Paired on the nearest reading, as the two tags rarely log together.
        let mut pairs = Vec::with_capacity(a.points.len());
        let mut j = 0usize;
        for (t, va) in &a.points {
            while j + 1 < b.points.len()
                && (b.points[j + 1].0 - t).abs() <= (b.points[j].0 - t).abs()
            {
                j += 1;
            }
            if let Some((_, vb)) = b.points.get(j)
                && (!self.log_y || (*va > 0.0 && *vb > 0.0))
            {
                pairs.push([*va, *vb]);
            }
        }

        let (xn, yn) = (self.display[self.x].clone(), self.display[self.y].clone());
        let colour = self.colours[self.y];
        let mut plot = Plot::new("correlation")
            .show_background(false)
            .allow_boxed_zoom(true)
            .show_crosshair(self.crosshair)
            .y_axis_min_width(58.0)
            .x_axis_label(xn.clone())
            .y_axis_label(yn.clone())
            .label_formatter(move |pos| match pos {
                HoverPosition::NearDataPoint { position, .. }
                | HoverPosition::Elsewhere { position } => Some(format!(
                    "{}\n{}",
                    format_value(position.x),
                    format_value(position.y)
                )),
            });
        if self.reset {
            plot = plot.reset();
        }
        plot.show(ui, |plot_ui| {
            plot_ui.points(
                Points::new(format!("{yn} gegen {xn}"), pairs)
                    .color(colour)
                    .radius(2.0),
            );
        });
        self.cursor = None;
    }

    /// The line under the chart, for the screen that holds this view.
    pub fn caption(&self) -> String {
        format!(
            "{} Tags · {} von {} Punkten{}",
            self.data.series.len(),
            crate::sep(self.data.drawn),
            crate::sep(self.data.read),
            if self.data.skipped_text > 0 {
                format!(" · {} Text-Tags ausgelassen", self.data.skipped_text)
            } else {
                String::new()
            }
        )
    }
}

/// Doubling the points so a line goes across and then up, the way a value that
/// is held between readings actually behaves.
fn stairs(points: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut out = Vec::with_capacity(points.len() * 2);
    for (i, p) in points.iter().enumerate() {
        if i > 0 {
            out.push([p[0], points[i - 1][1]]);
        }
        out.push(*p);
    }
    out
}

/// Milliseconds since 1970 -> `18.09 06:03:48`, short enough for an axis.
fn time_label(ms: f64) -> String {
    let full = convert::filetime_to_short(convert::ms_to_ticks(ms as i64));
    // `18.09.2026 06:03:48` -> `18.09 06:03:48`
    match full.get(..5).zip(full.get(10..)) {
        Some((day, time)) => format!("{day}{time}"),
        None => full,
    }
}

/// A value as it is read aloud, German style: `0,009`, `1,392`, `1235` --
/// never `9.00e-3`. Below one the digits that matter are kept, however many
/// noughts come first.
fn format_value(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        return if v == 0.0 { "0".into() } else { v.to_string() };
    }
    let a = v.abs();
    let decimals = if a >= 1000.0 {
        0
    } else if a >= 1.0 {
        3
    } else {
        // Three significant digits: 0,5 / 0,019 / 0,000123.
        ((-a.log10()).floor() as usize + 3).clamp(3, 12)
    };
    let s = format!("{v:.decimals$}");
    let s = if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        &s
    };
    s.replace('.', ",")
}

/// How much room the cursor readout is allowed, so the tag list keeps the rest.
fn readout_height(rows: &[(usize, f64, bool)]) -> f32 {
    if rows.is_empty() {
        0.0
    } else {
        (rows.len() as f32 * 20.0 + 40.0).min(260.0)
    }
}

/// One tag's colour: the same spread of hues the browser page uses, so the two
/// ways of looking at a log agree with each other.
fn colour_of(i: usize) -> egui::Color32 {
    let hue = (i as f32 * 137.508) % 360.0;
    let light = 0.58 + ((i % 3) as f32 - 1.0) * 0.09;
    let sat = 0.68 + (i % 2) as f32 * 0.12;
    egui::Color32::from(egui::ecolor::Hsva::new(hue / 360.0, sat, light, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stairs_hold_the_value_until_it_changes() {
        let points = vec![[0.0, 1.0], [10.0, 5.0], [20.0, 2.0]];
        let out = stairs(&points);
        assert_eq!(
            out,
            vec![
                [0.0, 1.0],
                [10.0, 1.0], // held across to the next reading
                [10.0, 5.0],
                [20.0, 5.0],
                [20.0, 2.0],
            ]
        );
    }

    #[test]
    fn axis_labels_are_short_and_still_dated() {
        // The first row of the sample export.
        assert_eq!(time_label(1_789_711_428_788.0), "18.09 06:03:48");
    }

    #[test]
    fn values_read_well_at_every_size() {
        assert_eq!(format_value(0.0), "0");
        assert_eq!(format_value(5.0), "5");
        assert_eq!(format_value(0.5), "0,5");
        assert_eq!(format_value(1.392), "1,392");
        assert_eq!(format_value(-25.0365), "-25,037");
        assert_eq!(format_value(1234.6), "1235");
        // Small values are written out, never as 9.00e-3.
        assert_eq!(format_value(0.009), "0,009");
        assert_eq!(format_value(0.004), "0,004");
        assert_eq!(format_value(0.000_123_4), "0,000123");
        assert!(!format_value(0.000_001).contains('e'));
    }

    #[test]
    fn the_two_views_agree_on_colour() {
        // The window and the page must not colour the same tag differently.
        for i in [0usize, 1, 7, 50] {
            assert_eq!(colour_of(i), colour_of(i));
        }
        assert_ne!(colour_of(0), colour_of(1));
    }
}
