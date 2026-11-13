#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! HMI Log Converter -- turns a pair of WinCC logging databases (tags + values)
//! into one flat CSV table. Drop the two .db3 files in any order; the program
//! works out by itself which one is which.

mod chart;
mod convert;
mod plotview;
mod theme;
mod timeline;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use chart::{ChartJob, ChartSummary, Detail, Loaded};
use convert::{Column, Job, Kept, OutColumn, Probe, Progress, Range, Summary, Windows};
use eframe::egui;
use plotview::PlotView;
use theme::{ACCENT, BAD, DIM, MUTED, Step, TEXT, WARN, body, hint, muted, strong};

/// A counting request answered on a worker thread: which request, which file,
/// and what the time range keeps of it.
type CountReply = (u64, PathBuf, Result<Kept, String>);

/// Values read for the chart drawn in this window.
type PlotReply = Result<Loaded, String>;

/// A finished chart on its way back from the worker thread.
type ChartReply = Result<ChartSummary, String>;

fn main() -> eframe::Result {
    // File names on the command line convert without opening a window, unless
    // --gui asks for the window with those files already loaded.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let windowed = args.iter().any(|a| a == "--gui");
    args.retain(|a| a != "--gui");
    if !args.is_empty() && !windowed {
        std::process::exit(cli(&args));
    }
    let preload: Vec<PathBuf> = args.iter().map(PathBuf::from).collect();

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1060.0, 900.0])
        .with_min_inner_size([720.0, 600.0])
        .with_drag_and_drop(true)
        .with_title("HMI Log Converter");

    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "HMI Log Converter",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, preload)))),
    )
}

// ---------------------------------------------------------------- settings

/// What the user chose last time, kept between sessions.
#[derive(Clone, Default)]
struct Settings {
    from: String,
    until: String,
    /// The long-table columns as they were arranged. Empty before the first run.
    columns: Vec<ColumnRow>,
}

impl Settings {
    fn file() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("hmi-converter").join("last-range.txt"))
    }

    /// A `key=value` file -- small enough not to need a format crate.
    ///
    /// A column line reads `column=<key>|<0 or 1>|<heading>`. The heading comes
    /// last and is taken whole, so it may contain anything the user typed.
    fn load() -> Self {
        let mut s = Self::default();
        let Some(path) = Self::file() else { return s };
        let Ok(text) = std::fs::read_to_string(path) else {
            return s;
        };
        for line in text.lines() {
            match line.split_once('=') {
                Some(("from", v)) => s.from = v.trim().to_string(),
                Some(("until", v)) => s.until = v.trim().to_string(),
                Some(("column", v)) => {
                    let mut parts = v.splitn(3, '|');
                    let Some(source) = parts.next().and_then(Column::parse) else {
                        continue;
                    };
                    if s.columns.iter().any(|r| r.source == source) {
                        continue;
                    }
                    s.columns.push(ColumnRow {
                        source,
                        on: parts.next() != Some("0"),
                        title: parts.next().unwrap_or_default().to_string(),
                    });
                }
                _ => {}
            }
        }
        s
    }

    fn save(&self) {
        let Some(path) = Self::file() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut text = format!("from={}\nuntil={}\n", self.from, self.until);
        for row in &self.columns {
            text.push_str(&format!(
                "column={}|{}|{}\n",
                row.source.key(),
                u8::from(row.on),
                row.title.replace(['\n', '\r'], " ")
            ));
        }
        let _ = std::fs::write(path, text);
    }

    /// The arrangement to start with: what was saved, with any column the file
    /// did not mention added at the end, so the list is always complete.
    fn column_rows(&self) -> Vec<ColumnRow> {
        if self.columns.is_empty() {
            return ColumnRow::all();
        }
        let mut rows = self.columns.clone();
        for row in ColumnRow::all() {
            if !rows.iter().any(|r| r.source == row.source) {
                rows.push(row);
            }
        }
        rows
    }
}

// ---------------------------------------------------------------- headless

/// Batch mode: `hmi-converter Tags.db3 Values.db3 [options]`.
fn cli(args: &[String]) -> i32 {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out_dir: Option<PathBuf> = None;
    let mut base_name: Option<String> = None;
    let mut semicolon = false;
    let mut shorten_names = false;
    let mut columns: Vec<OutColumn> = OutColumn::all();
    let (mut write_long, mut write_wide) = (true, true);
    let mut range = Range::default();
    let mut windows = Windows::whole();
    let mut chart_too = false;
    let mut detail = Detail::Balanced;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("Usage: hmi-converter [TAGS.db3 VALUES.db3] [options]");
                println!();
                println!("Options:");
                println!("  -o, --out FOLDER   where to write the CSV files");
                println!("  -n, --name PREFIX  name of the files, before _long / _wide");
                println!("  --from WHEN        ignore entries before this date/time");
                println!("  --until WHEN       ignore entries after this date/time");
                println!("  --short-names      shorten tag names to the telling part");
                println!("  --columns LIST     long-table columns, in order; any of");
                println!("                     time,raw,id,value,name,quality");
                println!("                     add =HEADING to rename one, e.g. value=Wert");
                println!("  --semicolon        use ; instead of , as separator");
                println!("  --long-only        write only the long table");
                println!("  --wide-only        write only the wide table");
                println!("  --chart            also write the interactive chart page");
                println!("  --detail LEVEL     chart detail: full, balanced or light");
                println!("  --gui              open the window with these files loaded");
                println!();
                println!("WHEN looks like 18.09.2026 or \"18.09.2026 06:30\" (UTC).");
                println!("Start without arguments to open the window.");
                return 0;
            }
            "-o" | "--out" => match it.next() {
                Some(d) => out_dir = Some(PathBuf::from(d)),
                None => {
                    eprintln!("{a} needs a folder");
                    return 2;
                }
            },
            "-n" | "--name" => match it.next() {
                Some(n) => base_name = Some(n.clone()),
                None => {
                    eprintln!("{a} needs a file name");
                    return 2;
                }
            },
            "--from" | "--until" => {
                let end_of_day = a == "--until";
                match it.next() {
                    Some(v) => match convert::parse_when(v, end_of_day) {
                        Ok(t) if end_of_day => range.until = Some(t),
                        Ok(t) => range.from = Some(t),
                        Err(e) => {
                            eprintln!("{a} {v}: {e}");
                            return 2;
                        }
                    },
                    None => {
                        eprintln!("{a} needs a date, for example 18.09.2026");
                        return 2;
                    }
                }
            }
            "--columns" => match it.next() {
                Some(list) => {
                    let mut chosen: Vec<OutColumn> = Vec::new();
                    for part in list.split(',') {
                        // `value` keeps its heading, `value=Wert` renames it.
                        let (key, header) = match part.split_once('=') {
                            Some((k, h)) => (k, Some(h.trim())),
                            None => (part, None),
                        };
                        let Some(source) = Column::parse(key) else {
                            eprintln!("unknown column: {key}");
                            return 2;
                        };
                        if chosen.iter().any(|c| c.source == source) {
                            continue;
                        }
                        let mut col = OutColumn::new(source);
                        if let Some(h) = header.filter(|h| !h.is_empty()) {
                            col.header = h.to_string();
                        }
                        chosen.push(col);
                    }
                    if chosen.is_empty() {
                        eprintln!("--columns needs at least one column");
                        return 2;
                    }
                    columns = chosen;
                }
                None => {
                    eprintln!("{a} needs a list, for example time,id,value,name");
                    return 2;
                }
            },
            "--chart" => chart_too = true,
            "--detail" => match it.next().map(|v| v.to_lowercase()) {
                Some(v) if v == "full" => detail = Detail::Full,
                Some(v) if v == "balanced" => detail = Detail::Balanced,
                Some(v) if v == "light" => detail = Detail::Light,
                _ => {
                    eprintln!("--detail takes full, balanced or light");
                    return 2;
                }
            },
            "--short-names" => shorten_names = true,
            "--semicolon" => semicolon = true,
            "--long-only" => write_wide = false,
            "--wide-only" => write_long = false,
            other => inputs.push(PathBuf::from(other)),
        }
    }

    // Folders are searched, so `hmi-converter D:\log` converts a whole export.
    let mut tag_dbs: Vec<PathBuf> = Vec::new();
    let mut value_dbs: Vec<PathBuf> = Vec::new();
    for path in convert::candidates(&inputs) {
        match convert::probe(&path) {
            Ok(p) => {
                if p.has_values {
                    value_dbs.push(p.path.clone());
                }
                if p.has_tags {
                    tag_dbs.push(p.path.clone());
                }
                println!(
                    "{}  {}  {}",
                    convert::short_name(&p.path),
                    p.role().to_lowercase(),
                    if p.has_values {
                        format!("{} values", sep(p.value_rows))
                    } else {
                        format!("{} tags", sep(p.tag_count))
                    }
                );
            }
            Err(e) => eprintln!("{}: {e}", path.display()),
        }
    }
    if value_dbs.is_empty() {
        eprintln!("Need at least one database with a LoggedProcessValue table.");
        return 2;
    }
    if !range.is_open() {
        windows = Windows::one(range);
    }
    if tag_dbs.is_empty() {
        eprintln!("No LoggingTag table found -- tags will be written as id_<number>.");
    }
    // The values are read oldest first, so the tables come out in time order.
    value_dbs.sort_by_key(|p| {
        convert::probe(p)
            .ok()
            .and_then(|x| x.span.map(|s| s.0))
            .unwrap_or(i64::MAX)
    });

    let job = Job {
        out_dir: out_dir
            .or_else(|| value_dbs[0].parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from(".")),
        base_name: base_name.unwrap_or_else(|| match value_dbs.as_slice() {
            [one] => stem_of(one),
            many => {
                let stems: Vec<String> = many.iter().map(|p| stem_of(p)).collect();
                let shared = common_prefix(&stems);
                let cut = shared
                    .trim_end_matches(|c: char| !"_-.".contains(c))
                    .trim_end_matches(['_', '-', '.']);
                if cut.len() >= 3 {
                    cut.to_string()
                } else {
                    stem_of(&value_dbs[0])
                }
            }
        }),
        tag_dbs: tag_dbs.clone(),
        value_dbs: value_dbs.clone(),
        semicolon,
        write_long,
        write_wide,
        windows,
        shorten_names,
        columns,
    };

    let (tx, rx) = channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let reporter = thread::spawn(move || {
        for msg in rx {
            match msg {
                Progress::Stage(s) => println!("{s}..."),
                Progress::Rows { done, total } if total > 0 => {
                    print!("\r  {} / {} rows", sep(done), sep(total));
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                _ => {}
            }
        }
        println!();
    });

    let result = convert::run(&job, &tx, &cancel);
    drop(tx);
    let _ = reporter.join();

    match result {
        Ok(s) => {
            if s.sources.len() > 1 {
                for (path, rows) in &s.sources {
                    println!(
                        "  {} values from {}",
                        sep(*rows),
                        convert::short_name(path)
                    );
                }
            }
            for (path, rows) in &s.files {
                println!("{}  ({} rows)", path.display(), sep(*rows));
            }
            println!(
                "{} values, {} tags, {:.1} s",
                sep(s.rows),
                s.tags_used,
                s.secs
            );
        }
        Err(e) => {
            eprintln!("failed: {e}");
            return 1;
        }
    }

    if !chart_too {
        return 0;
    }

    // The chart page, from the same files and the same time range.
    let chart_job = ChartJob {
        tag_dbs: job.tag_dbs.clone(),
        value_dbs: job.value_dbs.clone(),
        out_dir: job.out_dir.clone(),
        base_name: job.base_name.clone(),
        windows: job.windows.clone(),
        shorten_names: job.shorten_names,
        detail,
    };
    let (tx, rx) = channel();
    let reporter = thread::spawn(move || {
        for msg in rx {
            if let Progress::Stage(s) = msg {
                println!("{s}...");
            }
        }
    });
    let built = chart::build(&chart_job, &tx, &cancel);
    drop(tx);
    let _ = reporter.join();

    match built {
        Ok(c) => {
            println!(
                "{}  ({} of {} points, {})",
                c.path.display(),
                sep(c.drawn),
                sep(c.read),
                human_bytes(c.bytes)
            );
            0
        }
        Err(e) => {
            eprintln!("chart failed: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------- state

enum State {
    Setup,
    Running {
        rx: Receiver<Progress>,
        cancel: Arc<AtomicBool>,
        stage: String,
        done: u64,
        total: u64,
    },
    Done(Box<Summary>),
    /// A chart page was written and handed to the browser.
    Charted(Box<ChartSummary>),
    /// The chart drawn in this window.
    Plotting(Box<PlotView>),
    Failed(String),
}

/// What the live "how much would be kept" preview currently shows for one file.
#[derive(Clone)]
enum Count {
    /// No time range is set, so the whole file is taken.
    Whole,
    Counting,
    Kept(Kept),
    Bad(String),
}

/// One database the program found, and whether it takes part in the import.
#[derive(Clone)]
struct Source {
    probe: Probe,
    /// The checkbox in the preview table.
    on: bool,
    kept: Count,
}

impl Source {
    fn path(&self) -> &Path {
        &self.probe.path
    }

    /// How many values this file contributes, as far as it is known.
    fn values(&self) -> Option<u64> {
        if !self.probe.has_values {
            return None;
        }
        match &self.kept {
            Count::Whole => Some(self.probe.value_rows),
            Count::Kept(k) => Some(k.rows),
            _ => None,
        }
    }

    /// The period this file covers after the time range has been applied.
    fn span(&self) -> Option<(i64, i64)> {
        match &self.kept {
            Count::Kept(k) => k.span,
            _ => self.probe.span,
        }
    }
}

/// What the program is doing with the files it was just handed.
struct Scan {
    rx: Receiver<ScanMsg>,
    /// Files looked at so far, and how many there are in all.
    looked: usize,
    total: usize,
}

/// Why a file that is already in the list is not opened again.
const ALREADY: &str = "schon in der Liste";

/// One step of looking through what was dropped, from the worker thread.
enum ScanMsg {
    /// How many files are going to be looked at.
    Total(usize),
    Found(Box<Probe>),
    Skipped(PathBuf, String),
    Done,
}

struct App {
    /// Every database found, in the order they are imported.
    sources: Vec<Source>,
    /// Looking through a dropped folder, while it lasts.
    scan: Option<Scan>,
    /// The row last ticked, so shift-click can fill in the ones between.
    anchor: Option<usize>,
    /// Narrows the table when there are many files.
    filter: String,
    /// Feedback about the files that were just handed over.
    notes: Vec<(String, egui::Color32)>,
    state: State,

    // -- output
    out_dir: Option<PathBuf>,
    base_name: Option<String>,
    semicolon: bool,
    write_long: bool,
    write_wide: bool,
    shorten_names: bool,
    /// How much of the data goes into the chart page.
    detail: Detail,
    /// A finished chart waiting to be reported, from the worker thread.
    chart_done: (Sender<ChartReply>, Receiver<ChartReply>),
    /// Values read for the chart in this window.
    plot_done: (Sender<PlotReply>, Receiver<PlotReply>),
    /// Every column of the long table, in order, as the user has arranged them.
    long_columns: Vec<ColumnRow>,
    /// Whether the column chooser is open.
    columns_open: bool,
    /// The column being dragged, and where inside its row it was grabbed.
    drag: Option<(Column, f32)>,

    // -- time range
    /// The slices of the log to convert. Empty means all of it.
    windows: Vec<Range>,
    /// Which slice the two fields below edit, and the timeline highlights.
    selected: usize,
    /// An edge or a window being dragged on the timeline.
    grab: Option<timeline::Grab>,
    /// Earlier arrangements of the slices, newest last, to go back to.
    undo: Vec<Vec<Range>>,
    from_text: String,
    until_text: String,
    /// What is wrong with what was typed, if anything.
    range_error: Option<String>,
    /// The range used the last time something was converted.
    last: Settings,
    /// What the per-file counts were computed for, so they are only recomputed
    /// when the range or the chosen files change.
    preview_key: Option<PreviewKey>,
    /// A change waiting for the typing to settle before it is counted.
    pending: Option<PreviewKey>,
    pending_since: std::time::Instant,
    counter: (Sender<CountReply>, Receiver<CountReply>),
    counter_gen: u64,
    /// The same number where the counting thread can see it: when it moves on,
    /// a thread halfway through a folder of large files gives up rather than
    /// scanning for an answer nobody wants any more.
    counting_now: Arc<std::sync::atomic::AtomicU64>,
}

/// The windows and the files a set of counts belongs to.
type PreviewKey = (Vec<Range>, Vec<PathBuf>);

impl App {
    fn new(cc: &eframe::CreationContext<'_>, preload: Vec<PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);
        let last = Settings::load();
        let mut app = Self {
            sources: Vec::new(),
            scan: None,
            anchor: None,
            filter: String::new(),
            notes: Vec::new(),
            state: State::Setup,
            out_dir: None,
            base_name: None,
            semicolon: false,
            write_long: true,
            write_wide: true,
            shorten_names: false,
            detail: Detail::Balanced,
            chart_done: channel(),
            plot_done: channel(),
            long_columns: last.column_rows(),
            columns_open: false,
            drag: None,
            windows: Vec::new(),
            selected: 0,
            grab: None,
            undo: Vec::new(),
            from_text: String::new(),
            until_text: String::new(),
            range_error: None,
            last,
            preview_key: None,
            pending: None,
            pending_since: std::time::Instant::now(),
            counter: channel(),
            counter_gen: 0,
            counting_now: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        if !preload.is_empty() {
            app.accept(preload);
        }
        app
    }

    /// The slices to convert, plus a message for whatever does not parse.
    fn windows(&self) -> Result<Windows, String> {
        match &self.range_error {
            Some(e) => Err(e.clone()),
            None => Ok(Windows {
                spans: self.windows.clone(),
            }),
        }
    }

    /// What the two fields say, as one window.
    fn typed_window(&self) -> Result<Option<Range>, String> {
        let from = if self.from_text.trim().is_empty() {
            None
        } else {
            Some(convert::parse_when(&self.from_text, false).map_err(|e| format!("Von: {e}"))?)
        };
        let until = if self.until_text.trim().is_empty() {
            None
        } else {
            Some(convert::parse_when(&self.until_text, true).map_err(|e| format!("Bis: {e}"))?)
        };
        if let (Some(a), Some(b)) = (from, until)
            && a > b
        {
            return Err("„Von“ liegt nach „Bis“".into());
        }
        Ok(match (from, until) {
            (None, None) => None,
            _ => Some(Range { from, until }),
        })
    }

    /// Take what was typed into the slice the fields belong to. Emptying both
    /// fields drops that slice, which is how the whole log is asked for again.
    fn text_edited(&mut self) {
        match self.typed_window() {
            Err(e) => self.range_error = Some(e),
            Ok(None) => {
                self.range_error = None;
                if self.selected < self.windows.len() {
                    self.windows.remove(self.selected);
                }
                self.selected = self.selected.min(self.windows.len().saturating_sub(1));
            }
            Ok(Some(w)) => {
                self.range_error = None;
                match self.windows.get_mut(self.selected) {
                    Some(slot) => *slot = w,
                    None => {
                        self.windows.push(w);
                        self.selected = self.windows.len() - 1;
                    }
                }
            }
        }
    }

    /// Keep the slices as they are now, before something changes them.
    fn remember_slices(&mut self, slices: Vec<Range>) {
        if self.undo.last() != Some(&slices) {
            self.undo.push(slices);
            if self.undo.len() > 100 {
                self.undo.remove(0);
            }
        }
    }

    /// Go back to the slices as they were before the last change.
    fn undo_slices(&mut self) {
        if let Some(earlier) = self.undo.pop() {
            self.windows = earlier;
            self.selected = self.selected.min(self.windows.len().saturating_sub(1));
            self.grab = None;
            self.text_from_window();
        }
    }

    /// Write the chosen slice back into the two fields, after the timeline
    /// has been dragged.
    fn text_from_window(&mut self) {
        let shown = |t: Option<i64>| t.map(convert::filetime_to_short).unwrap_or_default();
        let (from, until) = match self.windows.get(self.selected) {
            Some(w) => (shown(w.from), shown(w.until)),
            None => (String::new(), String::new()),
        };
        self.from_text = from;
        self.until_text = until;
        self.range_error = None;
    }

    /// The whole period the ticked files cover, which the timeline is drawn on.
    fn data_bounds(&self) -> Option<(i64, i64)> {
        self.sources
            .iter()
            .filter(|s| s.probe.has_values)
            .filter_map(|s| s.probe.span)
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
    }

    // -- the files that take part ------------------------------------------

    /// Every included file that holds logged values, in the order they are read.
    fn value_dbs(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .filter(|s| s.on && s.probe.has_values)
            .map(|s| s.path().to_path_buf())
            .collect()
    }

    /// Every included file that holds tag names.
    fn tag_dbs(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .filter(|s| s.on && s.probe.has_tags)
            .map(|s| s.path().to_path_buf())
            .collect()
    }

    fn included(&self) -> impl Iterator<Item = &Source> {
        self.sources.iter().filter(|s| s.on)
    }

    /// How many values the import would write, when every file has been counted.
    fn total_values(&self) -> Option<u64> {
        self.included()
            .filter(|s| s.probe.has_values)
            .map(Source::values)
            .sum()
    }

    /// The period the included files cover together.
    fn total_span(&self) -> Option<(i64, i64)> {
        self.included()
            .filter(|s| s.probe.has_values)
            .filter_map(Source::span)
            .reduce(|(a, b), (c, d)| (a.min(c), b.max(d)))
    }

    /// How many tags the included tag databases describe between them.
    fn total_tags(&self) -> usize {
        let mut ids: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
        for s in self.included().filter(|s| s.probe.has_tags) {
            ids.extend(s.probe.tag_names.iter().map(|(id, _)| *id));
        }
        ids.len()
    }

    /// Pairs of included files that log the same period. They convert, but the
    /// same reading then appears twice, which is worth saying out loud.
    fn overlaps(&self) -> Vec<(String, String)> {
        let mut spans: Vec<(&str, (i64, i64))> = Vec::new();
        for s in self.included().filter(|s| s.probe.has_values) {
            if let Some(span) = s.probe.span {
                spans.push((s.probe.path.to_str().unwrap_or_default(), span));
            }
        }
        let mut out = Vec::new();
        for (i, (a, (a0, a1))) in spans.iter().enumerate() {
            for (b, (b0, b1)) in &spans[i + 1..] {
                if a0 <= b1 && b0 <= a1 {
                    out.push((
                        file_name(Path::new(a)).to_string(),
                        file_name(Path::new(b)).to_string(),
                    ));
                }
            }
        }
        out
    }

    /// Where the files go, and what they are called.
    fn out_dir(&self) -> PathBuf {
        self.chosen_dir().unwrap_or_else(|| PathBuf::from("."))
    }

    /// The folder as far as it is known: either the one picked, or the one the
    /// first included value database sits in.
    fn chosen_dir(&self) -> Option<PathBuf> {
        self.out_dir.clone().or_else(|| {
            self.included()
                .find(|s| s.probe.has_values)
                .or_else(|| self.sources.first())?
                .path()
                .parent()
                .map(|p| p.to_path_buf())
        })
    }

    fn base_name(&self) -> String {
        self.base_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| self.suggested_name())
    }

    /// A name for the output that says where it came from.
    ///
    /// One file gives its own name. Several give what their names have in
    /// common -- an HMI numbers its files, so `Anlage1_Werte_20260302_053000` and
    /// `Anlage1_Werte_20260303_090000` suggest `Anlage1_Werte`. Failing that,
    /// the folder they sit in.
    fn suggested_name(&self) -> String {
        let stems: Vec<String> = self
            .included()
            .filter(|s| s.probe.has_values)
            .map(|s| stem_of(s.path()))
            .collect();
        match stems.as_slice() {
            [] => "hmi".into(),
            [one] => one.clone(),
            many => {
                let shared = common_prefix(many);
                let cut = shared.trim_end_matches(|c: char| !"_-.".contains(c));
                let trimmed = cut.trim_end_matches(['_', '-', '.']);
                if trimmed.len() >= 3 {
                    return trimmed.to_string();
                }
                self.chosen_dir()
                    .and_then(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "hmi".into())
            }
        }
    }

    /// The columns of the long table, in the order the user put them, under the
    /// headings they gave them.
    fn columns(&self) -> Vec<OutColumn> {
        self.long_columns
            .iter()
            .filter(|r| r.on)
            .map(ColumnRow::out)
            .collect()
    }

    /// The files that pressing Convert would write.
    fn output_files(&self) -> Vec<String> {
        let base = self.base_name();
        let mut out = Vec::new();
        if self.write_long {
            out.push(format!("{base}_long.csv"));
        }
        if self.write_wide {
            out.push(format!("{base}_wide.csv"));
        }
        out
    }

    fn ready(&self) -> bool {
        !self.value_dbs().is_empty()
            && (self.write_long || self.write_wide)
            && (!self.write_long || !self.columns().is_empty())
            && self.windows().is_ok()
            && self.total_values() != Some(0)
    }

    /// Recount whenever the typed range or the chosen files changed.
    ///
    /// Counting touches every row of every file, so it waits for a short pause
    /// in typing instead of running on each keystroke -- and with no range at
    /// all nothing needs counting, because then the whole of each file is taken.
    fn refresh_preview(&mut self, ctx: &egui::Context) {
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

        while let Ok((seq, path, result)) = self.counter.1.try_recv() {
            if seq != self.counter_gen {
                continue;
            }
            if let Some(s) = self.sources.iter_mut().find(|s| s.path() == path) {
                s.kept = match result {
                    Ok(k) => Count::Kept(k),
                    Err(e) => Count::Bad(e),
                };
            }
        }

        let files = self.value_dbs();
        let key: PreviewKey = (self.windows.clone(), files.clone());

        // Something changed: show it at once, then let the timer run down.
        if self.preview_key.as_ref() != Some(&key) && self.pending.as_ref() != Some(&key) {
            self.pending = Some(key.clone());
            self.pending_since = std::time::Instant::now();
            let open = self.windows().map(|w| w.is_open()).unwrap_or(false);
            let bad = self.windows().err();
            for s in &mut self.sources {
                if !s.probe.has_values {
                    continue;
                }
                s.kept = match (&bad, open, s.on) {
                    (Some(e), _, _) => Count::Bad(e.clone()),
                    (None, true, _) | (None, _, false) => Count::Whole,
                    _ => Count::Counting,
                };
            }
        }

        let Some(key) = self.pending.clone() else {
            return;
        };
        if self.pending_since.elapsed() < SETTLE {
            ctx.request_repaint_after(SETTLE / 4);
            return;
        }

        self.pending = None;
        self.preview_key = Some(key);
        let windows = match self.windows() {
            Ok(w) if !w.is_open() => w,
            // Nothing to count: without a range every file is taken whole, and
            // a range that does not parse is already reported.
            _ => return,
        };

        self.counter_gen += 1;
        let seq = self.counter_gen;
        self.counting_now.store(seq, Ordering::Relaxed);
        let tx = self.counter.0.clone();
        let ctx = ctx.clone();
        let live = self.counting_now.clone();
        thread::spawn(move || {
            for path in files {
                if live.load(Ordering::Relaxed) != seq {
                    return;
                }
                let kept = convert::count_kept(&path, &windows);
                if tx.send((seq, path, kept)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
    }

    fn note(&mut self, text: impl Into<String>, color: egui::Color32) {
        self.notes.push((text.into(), color));
    }

    /// Take whatever the user dropped or picked and look through it.
    ///
    /// Folders are searched, and every file inside is opened to see whether it
    /// is an HMI database -- which can take a moment for a folder full of
    /// hundred-megabyte files, so it happens on a worker thread and the table
    /// fills up as the answers arrive.
    fn accept(&mut self, paths: Vec<PathBuf>) {
        if matches!(self.state, State::Running { .. }) {
            self.notes.clear();
            self.note("Die Umwandlung läuft noch – bitte warten.", WARN);
            return;
        }
        self.state = State::Setup;
        self.notes.clear();

        let known: Vec<PathBuf> = self.sources.iter().map(|s| s.path().to_path_buf()).collect();
        let (tx, rx) = channel();
        self.scan = Some(Scan {
            rx,
            looked: 0,
            total: 0,
        });
        thread::spawn(move || {
            let files = convert::candidates(&paths);
            let _ = tx.send(ScanMsg::Total(files.len()));
            for path in files {
                // Already in the table: nothing to open again.
                if known.iter().any(|k| k == &path) {
                    let _ = tx.send(ScanMsg::Skipped(path, ALREADY.into()));
                    continue;
                }
                let msg = match convert::probe(&path) {
                    Ok(p) => ScanMsg::Found(Box::new(p)),
                    Err(e) => ScanMsg::Skipped(path, e),
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            let _ = tx.send(ScanMsg::Done);
        });
    }

    /// Collect whatever the scanning thread has found so far.
    fn pump_scan(&mut self, ctx: &egui::Context) {
        let Some(scan) = &mut self.scan else { return };
        let mut added = 0usize;
        let mut finished = false;
        let mut skipped: Vec<(PathBuf, String)> = Vec::new();
        let mut found: Vec<Probe> = Vec::new();

        while let Ok(msg) = scan.rx.try_recv() {
            match msg {
                ScanMsg::Total(n) => scan.total = n,
                ScanMsg::Found(p) => {
                    scan.looked += 1;
                    found.push(*p);
                }
                ScanMsg::Skipped(path, why) => {
                    scan.looked += 1;
                    skipped.push((path, why));
                }
                ScanMsg::Done => finished = true,
            }
        }

        for p in found {
            added += 1;
            self.sources.push(Source {
                probe: p,
                on: true,
                kept: Count::Whole,
            });
        }
        if added > 0 {
            self.sort_sources();
            // The counts belong to the files that were chosen before.
            self.preview_key = None;
        }
        for (path, why) in skipped {
            // One line per file would bury the useful ones; only real
            // surprises are worth a word.
            if why != ALREADY {
                self.note(format!("{} übersprungen: {why}", file_name(&path)), MUTED);
            }
        }
        if finished {
            let n = self.sources.len();
            self.scan = None;
            if n == 0 {
                self.note("Darin wurde keine HMI-Datenbank gefunden.", BAD);
            }
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Tag databases first, then the value files in the order they were logged:
    /// the order the import reads them, and the order they read in the table.
    fn sort_sources(&mut self) {
        self.sources.sort_by(|a, b| {
            let rank = |s: &Source| u8::from(s.probe.has_values);
            rank(a)
                .cmp(&rank(b))
                .then(a.probe.span.map(|s| s.0).cmp(&b.probe.span.map(|s| s.0)))
                .then_with(|| file_name(a.path()).cmp(&file_name(b.path())))
        });
        self.anchor = None;
    }

    fn start(&mut self) {
        if self.value_dbs().is_empty() {
            return;
        }
        let windows = match self.windows() {
            Ok(w) => w,
            Err(e) => {
                self.state = State::Failed(e);
                return;
            }
        };

        let job = Job {
            tag_dbs: self.tag_dbs(),
            value_dbs: self.value_dbs(),
            out_dir: self.out_dir(),
            base_name: self.base_name(),
            semicolon: self.semicolon,
            write_long: self.write_long,
            write_wide: self.write_wide,
            windows,
            shorten_names: self.shorten_names,
            columns: self.columns(),
        };

        // Remember what was chosen, so the next run starts where this left off.
        if let Some(w) = self.windows.first() {
            let shown = |t: Option<i64>| t.map(convert::filetime_to_short).unwrap_or_default();
            self.last.from = shown(w.from);
            self.last.until = shown(w.until);
        }
        self.remember_columns();

        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        thread::spawn(move || {
            let result = convert::run(&job, &tx, &worker_cancel);
            let _ = match result {
                Ok(s) => tx.send(Progress::Finished(Box::new(s))),
                Err(e) => tx.send(Progress::Failed(e)),
            };
        });

        self.notes.clear();
        self.state = State::Running {
            rx,
            cancel,
            stage: "Wird gestartet".into(),
            done: 0,
            total: 0,
        };
    }

    /// What to read, for either way of charting.
    fn chart_job(&mut self) -> Option<ChartJob> {
        if self.value_dbs().is_empty() {
            return None;
        }
        let windows = match self.windows() {
            Ok(w) => w,
            Err(e) => {
                self.state = State::Failed(e);
                return None;
            }
        };
        Some(ChartJob {
            tag_dbs: self.tag_dbs(),
            value_dbs: self.value_dbs(),
            out_dir: self.out_dir(),
            base_name: self.base_name(),
            windows,
            shorten_names: self.shorten_names,
            detail: self.detail,
        })
    }

    /// Build the chart page on a worker, then hand it to the browser.
    fn start_chart(&mut self) {
        let Some(job) = self.chart_job() else { return };

        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let done = self.chart_done.0.clone();
        thread::spawn(move || {
            let _ = done.send(chart::build(&job, &tx, &worker_cancel));
        });

        self.notes.clear();
        self.state = State::Running {
            rx,
            cancel,
            stage: "Wird gestartet".into(),
            done: 0,
            total: 0,
        };
    }

    /// Read the values for the chart drawn in this window.
    fn start_plot(&mut self) {
        let Some(job) = self.chart_job() else { return };
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let done = self.plot_done.0.clone();
        thread::spawn(move || {
            let _ = done.send(chart::load(&job, &tx, &worker_cancel));
        });

        self.notes.clear();
        self.state = State::Running {
            rx,
            cancel,
            stage: "Wird gestartet".into(),
            done: 0,
            total: 0,
        };
    }

    fn pump(&mut self) {
        let mut finished: Option<State> = None;
        if let State::Running {
            rx,
            stage,
            done,
            total,
            ..
        } = &mut self.state
        {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Progress::Stage(s) => *stage = s,
                    Progress::Rows { done: d, total: t } => {
                        *done = d;
                        *total = t;
                    }
                    Progress::Finished(s) => finished = Some(State::Done(s)),
                    Progress::Failed(e) => finished = Some(State::Failed(e)),
                }
            }
        }
        if let Some(s) = finished {
            self.state = s;
        }

        // A chart reports on its own channel, because it produces a page
        // rather than a pair of tables.
        while let Ok(result) = self.plot_done.1.try_recv() {
            self.state = match result {
                Ok(data) => State::Plotting(Box::new(PlotView::new(data))),
                Err(e) => State::Failed(e),
            };
        }

        while let Ok(result) = self.chart_done.1.try_recv() {
            self.state = match result {
                Ok(summary) => {
                    chart::open(&summary.path);
                    State::Charted(Box::new(summary))
                }
                Err(e) => State::Failed(e),
            };
        }
    }
}

// ---------------------------------------------------------------- drawing

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump();
        if matches!(self.state, State::Setup) {
            let ctx = ui.ctx().clone();
            self.pump_scan(&ctx);
            self.refresh_preview(&ctx);
        }
        let ctx = ui.ctx().clone();

        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if !dropped.is_empty() {
            self.accept(dropped);
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());

        let setup = matches!(self.state, State::Setup);
        let plotting = matches!(self.state, State::Plotting(_));

        // The setup screen ends in a bar across the whole window: what is
        // about to happen on the left, the buttons that do it on the right.
        if setup {
            egui::Panel::bottom("action")
                .show_separator_line(false)
                .frame(
                    egui::Frame::new()
                        .fill(theme::SURFACE)
                        .inner_margin(egui::Margin::symmetric(theme::MARGIN, 18)),
                )
                .show(ui, |ui| {
                    let r = ui.max_rect();
                    ui.painter().with_clip_rect(egui::Rect::EVERYTHING).hline(
                        (r.left() - 100.0)..=(r.right() + 100.0),
                        r.top() - 18.0,
                        egui::Stroke::new(1.0, theme::EDGE),
                    );
                    self.action_bar(ui);
                });
        }

        egui::Frame::central_panel(ui.style())
            .inner_margin(egui::Margin::same(theme::MARGIN))
            .show(ui, |ui| {
                if setup {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| self.setup_view(ui));
                } else if plotting {
                    // The chart takes the whole window, so it is not scrolled.
                    self.plot_view(ui);
                } else {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| match &self.state {
                            State::Running { .. } => self.running_view(ui),
                            State::Done(_) => self.done_view(ui),
                            State::Charted(_) => self.chart_view(ui),
                            State::Failed(_) => self.failed_view(ui),
                            State::Setup | State::Plotting(_) => {}
                        });
                }
            });

        if setup {
            self.columns_window(&ctx);
        }
        if hovering {
            drop_overlay(&ctx);
        }

        if matches!(self.state, State::Running { .. }) {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

/// While files are held over the window, the whole of it is the target.
fn drop_overlay(ctx: &egui::Context) {
    let paint = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("drop-overlay"),
    ));
    let screen = ctx.content_rect();
    paint.rect_filled(screen, egui::CornerRadius::ZERO, theme::BG.gamma_multiply(0.93));
    let target = screen.shrink(18.0);
    paint.rect(
        target,
        egui::CornerRadius::same(14),
        theme::ACCENT_DEEP.gamma_multiply(0.35),
        egui::Stroke::new(2.0, ACCENT),
        egui::StrokeKind::Inside,
    );
    paint.text(
        target.center() - egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        "Zum Hinzufügen loslassen",
        egui::FontId::new(22.0, theme::bold_family()),
        TEXT,
    );
    paint.text(
        target.center() + egui::vec2(0.0, 18.0),
        egui::Align2::CENTER_CENTER,
        "Ordner werden nach HMI-Datenbanken durchsucht",
        egui::FontId::proportional(14.0),
        MUTED,
    );
}

impl App {
    // -- choosing files and options ---------------------------------------

    /// Three steps on one rail, top to bottom: what comes in, which part of
    /// it, and what goes out. The bar underneath does it.
    fn setup_view(&mut self, ui: &mut egui::Ui) {
        let loaded = !self.sources.is_empty();
        let later = if loaded { Step::Current } else { Step::Later };
        theme::step(ui, 1, if loaded { Step::Done } else { Step::Current }, false, |ui| {
            self.files_step(ui)
        });
        ui.add_space(theme::SECTION);
        theme::step(ui, 2, later, false, |ui| self.range_step(ui, loaded));
        ui.add_space(theme::SECTION);
        theme::step(ui, 3, later, true, |ui| self.output_step(ui, loaded));
        ui.add_space(theme::GAP);
    }

    // -- step 1: the files -------------------------------------------------

    fn files_step(&mut self, ui: &mut egui::Ui) {
        let loaded = !self.sources.is_empty();
        theme::step_title(
            ui,
            "Dateien laden",
            if loaded {
                "Ankreuzen, was umgewandelt wird. Ein Klick auf die Zeile schaltet sie um."
            } else {
                "Die Datenbanken aus dem HMI-Export: Tag-Namen und Messwerte."
            },
            if loaded { Step::Done } else { Step::Current },
            |_| {},
        );
        ui.add_space(theme::GAP + 2.0);

        let picked = if loaded {
            let picked = self.file_table(ui);
            self.file_warnings(ui);
            picked
        } else {
            self.drop_zone(ui)
        };
        self.notes_view(ui);
        if let Some(files) = picked {
            self.accept(files);
        }
    }

    /// The empty state: one large place to drop the files, and the two ways
    /// of picking them instead.
    fn drop_zone(&mut self, ui: &mut egui::Ui) -> Option<Vec<PathBuf>> {
        let mut picked = None;
        let size = egui::vec2(ui.available_width(), 230.0);
        let rect = egui::Rect::from_min_size(ui.cursor().min, size);
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(12), theme::SURFACE);
        let r = rect.shrink(1.0);
        let corners = vec![
            r.left_top(),
            r.right_top(),
            r.right_bottom(),
            r.left_bottom(),
            r.left_top(),
        ];
        ui.painter().extend(egui::Shape::dashed_line(
            &corners,
            egui::Stroke::new(1.5, theme::EDGE),
            7.0,
            5.0,
        ));

        ui.allocate_ui_with_layout(
            size,
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.set_min_size(size);
                ui.add_space(52.0);
                ui.label(theme::title("HMI-Datenbanken hierher ziehen"));
                ui.add_space(2.0);
                match &self.scan {
                    Some(scan) => {
                        ui.horizontal(|ui| {
                            ui.add_space((ui.available_width() - 200.0).max(0.0) / 2.0);
                            ui.spinner();
                            ui.label(hint(if scan.total > 0 {
                                format!("Durchsuche {} von {} Dateien …", scan.looked, scan.total)
                            } else {
                                "Suche …".into()
                            }));
                        });
                    }
                    None => {
                        ui.label(hint("einzelne .db3-Dateien oder den ganzen Ordner"));
                    }
                }
                ui.add_space(24.0);
                ui.horizontal(|ui| {
                    let size = egui::vec2(180.0, 38.0);
                    let width = size.x * 2.0 + ui.spacing().item_spacing.x;
                    ui.add_space((ui.available_width() - width).max(0.0) / 2.0);
                    if theme::primary(ui, "Dateien wählen …", true, size).clicked() {
                        picked = pick_databases();
                    }
                    if theme::secondary(ui, "Ordner wählen …", true, size).clicked() {
                        picked = pick_folder();
                    }
                });
            },
        );
        picked
    }

    /// Every database that was found, what is inside it, and a tick to leave
    /// it out of the import. Its foot says what the selection adds up to and
    /// holds the ways to change the list. Returns files picked to add.
    fn file_table(&mut self, ui: &mut egui::Ui) -> Option<Vec<PathBuf>> {
        let all = self.sources.len();
        let on = self.included().count();
        let mut every: Option<bool> = None;
        let mut picked: Option<Vec<PathBuf>> = None;
        let mut clear = false;

        theme::panel()
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let cols = Cols::new(ui.available_width());
                ui.horizontal(|ui| {
                    ui.set_height(30.0);
                    ui.spacing_mut().item_spacing.x = Cols::GAP;
                    cell(ui, cols.tick, egui::Align::Center, |ui| {
                        let mut ticked = on == all;
                        if theme::check(ui, &mut ticked, "")
                            .on_hover_text(if on == all {
                                "Alle abwählen"
                            } else {
                                "Alle auswählen"
                            })
                            .changed()
                        {
                            every = Some(ticked);
                        }
                    });
                    cell(ui, cols.name, egui::Align::Min, |ui| {
                        if all > 8 {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.filter)
                                    .hint_text("Datei filtern …")
                                    .desired_width(cols.name.min(260.0)),
                            );
                        } else {
                            ui.label(muted("Datei"));
                        }
                    });
                    cell(ui, cols.role, egui::Align::Min, |ui| {
                        ui.label(muted("Inhalt"));
                    });
                    cell(ui, cols.rows, egui::Align::Max, |ui| {
                        ui.label(muted("Werte"));
                    });
                    cell(ui, cols.span, egui::Align::Min, |ui| {
                        ui.label(muted("Zeitraum"));
                    });
                    cell(ui, cols.drop, egui::Align::Center, |_| {});
                });
                theme::divider(ui);
                let room = (ui.clip_rect().height() * 0.4).clamp(150.0, 420.0);
                self.table_rows(ui, &cols, room);
                theme::divider(ui);

                // -- the foot: what this adds up to, and the list's own tools
                ui.horizontal(|ui| {
                    ui.set_min_height(34.0);
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.label(strong(format!("{on} von {all}")));
                    ui.label(hint("ausgewählt"));
                    if let Some((a, b)) = self.total_span() {
                        ui.label(hint("·"));
                        ui.label(hint(format!(
                            "{} – {}",
                            convert::filetime_to_short(a),
                            convert::filetime_to_short(b)
                        )));
                    }
                    if !self.tag_dbs().is_empty() {
                        ui.label(hint("·"));
                        ui.label(hint(format!("{} Tag-Namen", sep(self.total_tags() as u64))));
                    }
                    if self.scan.is_some() {
                        ui.spinner();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::quiet(ui, "Leeren")
                            .on_hover_text("Alle Dateien aus der Liste nehmen")
                            .clicked()
                        {
                            clear = true;
                        }
                        if theme::quiet(ui, "+ Ordner").on_hover_text("Einen Ordner durchsuchen").clicked() {
                            picked = pick_folder();
                        }
                        if theme::quiet(ui, "+ Dateien").on_hover_text("Weitere Dateien hinzufügen").clicked() {
                            picked = pick_databases();
                        }
                    });
                });
            });

        if let Some(v) = every {
            self.set_all(v);
        }
        if clear {
            self.sources.clear();
            self.filter.clear();
            self.notes.clear();
            self.preview_key = None;
            self.windows.clear();
            self.undo.clear();
            self.text_from_window();
        }
        picked
    }

    fn set_all(&mut self, on: bool) {
        for s in &mut self.sources {
            s.on = on;
        }
        self.preview_key = None;
    }

    /// One row per file, and whatever was clicked on them.
    fn table_rows(&mut self, ui: &mut egui::Ui, cols: &Cols, room: f32) {
        let needle = self.filter.trim().to_lowercase();
        let shown: Vec<usize> = (0..self.sources.len())
            .filter(|&i| {
                needle.is_empty()
                    || file_name(self.sources[i].path())
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect();

        let mut act: Option<RowAct> = None;
        egui::ScrollArea::vertical()
            .id_salt("sources")
            // Only as tall as the rows need, until there are enough of them to
            // push the rest of the window out of sight.
            .max_height(room)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if shown.is_empty() {
                    ui.label(hint("Keine Datei passt zum Filter."));
                }
                for &i in &shown {
                    if let Some(a) = source_row(ui, &mut self.sources[i], cols, i) {
                        act = Some(a);
                    }
                }
            });

        match act {
            Some(RowAct::Toggle(i, shift)) => {
                let wanted = !self.sources[i].on;
                match (shift, self.anchor) {
                    // Shift fills in everything between the two rows, which is
                    // how a hundred segment files are handled in two clicks.
                    (true, Some(from)) if from < self.sources.len() => {
                        let (lo, hi) = (from.min(i), from.max(i));
                        for s in &mut self.sources[lo..=hi] {
                            s.on = wanted;
                        }
                    }
                    _ => {
                        self.sources[i].on = wanted;
                        self.anchor = Some(i);
                    }
                }
                self.preview_key = None;
            }
            Some(RowAct::Only(i)) => {
                for (k, s) in self.sources.iter_mut().enumerate() {
                    s.on = k == i;
                }
                self.anchor = Some(i);
                self.preview_key = None;
            }
            Some(RowAct::Remove(i)) => {
                self.sources.remove(i);
                self.anchor = None;
                self.preview_key = None;
            }
            Some(RowAct::Reveal(i)) => reveal(self.sources[i].path()),
            None => {}
        }
    }

    /// What about the chosen files must not be missed.
    fn file_warnings(&mut self, ui: &mut egui::Ui) {
        let values = self.value_dbs().len();
        let tag_dbs = self.tag_dbs().len();
        let warn = |ui: &mut egui::Ui, text: String| {
            ui.add_space(6.0);
            theme::banner(ui, text, WARN)
        };
        if values == 0 {
            warn(ui, "Keine Datei mit Messwerten ausgewählt.".into());
        } else if tag_dbs == 0 {
            warn(
                ui,
                "Keine Tag-Datenbank ausgewählt – die Tags heißen dann id_<Nummer>.".into(),
            );
        }
        // One line, whatever the number of pairs: the files are named in the
        // table above, and the first pair in the hover text.
        let overlaps = self.overlaps();
        if let Some((a, b)) = overlaps.first() {
            warn(
                ui,
                match overlaps.len() {
                    1 => format!(
                        "{a} und {b} decken denselben Zeitraum ab – diese Werte \
                         würden doppelt geschrieben."
                    ),
                    n => format!(
                        "{n} Dateipaare decken denselben Zeitraum ab – diese Werte \
                         würden doppelt geschrieben."
                    ),
                },
            )
            .on_hover_text(format!("zum Beispiel\n{a}\nund\n{b}"));
        }
    }

    // -- step 2: the time range --------------------------------------------

    /// Cut the log down to a slice of time -- or several -- with the files
    /// drawn underneath, so it is clear what is being cut.
    fn range_step(&mut self, ui: &mut egui::Ui, loaded: bool) {
        let state = if loaded { Step::Current } else { Step::Later };
        let subtitle = if !loaded {
            "Optional: nur einen Ausschnitt umwandeln."
        } else {
            match self.windows.len() {
                0 => "Optional: über die Leiste ziehen, um nur einen Ausschnitt umzuwandeln.",
                1 => "An den Griffen ziehen zum Anpassen, in der Mitte zum Verschieben.",
                _ => "Nur was in den Ausschnitten liegt, wird umgewandelt.",
            }
        };
        theme::step_title(ui, "Zeitraum wählen", subtitle, state, |ui| {
            if loaded {
                self.kept_label(ui);
            }
        });
        if !loaded {
            return;
        }
        ui.add_space(theme::GAP + 2.0);

        let bounds = self.data_bounds();
        let lanes: Vec<timeline::Lane> = self
            .sources
            .iter()
            .filter(|s| s.probe.has_values)
            .filter_map(|s| {
                Some(timeline::Lane {
                    name: file_name(s.path()),
                    span: s.probe.span?,
                    on: s.on,
                    density: s.probe.density.clone(),
                })
            })
            .collect();
        // Esc while dragging puts the slices back as they were before the
        // drag; Ctrl+Z steps back through every change.
        if self.grab.is_some() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.undo_slices();
        }
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !typing
            && ui.input_mut(|i| {
                i.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Z,
                ))
            })
        {
            self.undo_slices();
        }
        let before = self.windows.clone();
        let edit = timeline::ui(
            ui,
            &lanes,
            bounds,
            &mut self.windows,
            &mut self.selected,
            &mut self.grab,
        );
        if edit.began {
            self.remember_slices(before);
        }
        if edit.changed {
            self.text_from_window();
        }
        ui.add_space(theme::GAP);

        // -- the exact dates of whichever slice is chosen, and the tools for
        //    slices, lined up under the two ends of the timeline
        ui.horizontal(|ui| {
            let last_from = self.last.from.clone();
            let last_until = self.last.until.clone();
            let mut edited = false;
            let exact = |t: Option<i64>| match t {
                Some(t) => format!("genau {}", convert::filetime_to_text(t)),
                None => "ohne Grenze".to_string(),
            };
            let slice = self.windows.get(self.selected).copied();
            edited |= range_field(ui, "Von", &mut self.from_text, exact(slice.and_then(|w| w.from)));
            ui.add_space(10.0);
            edited |= range_field(
                ui,
                "Bis",
                &mut self.until_text,
                exact(slice.and_then(|w| w.until)),
            );

            // The last range converted, one click away.
            let last_known = !last_from.trim().is_empty() || !last_until.trim().is_empty();
            let differs = last_from.trim() != self.from_text.trim()
                || last_until.trim() != self.until_text.trim();
            if last_known
                && differs
                && theme::quiet(ui, "Wie zuletzt")
                    .on_hover_text(format!(
                        "Den zuletzt umgewandelten Zeitraum einsetzen:\n{} – {}",
                        last_from.trim(),
                        last_until.trim()
                    ))
                    .clicked()
            {
                self.remember_slices(self.windows.clone());
                self.from_text = last_from.trim().to_string();
                self.until_text = last_until.trim().to_string();
                edited = true;
            }
            if edited {
                self.text_edited();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.range_buttons(ui, self.data_bounds());
            });
        });

        if let Some(e) = &self.range_error {
            ui.add_space(4.0);
            theme::banner(ui, e.clone(), BAD);
        }
        // -- every slice, for picking one without aiming at the timeline
        if self.windows.len() > 1 {
            ui.add_space(4.0);
            self.slice_chips(ui);
        }
    }

    /// Split / remove / start again, as one group: all a slice needs besides
    /// dragging. Laid out right to left, so the group ends where the
    /// timeline ends.
    fn range_buttons(&mut self, ui: &mut egui::Ui, bounds: Option<(i64, i64)>) {
        let any = !self.windows.is_empty();
        egui::Frame::new()
            .fill(theme::SURFACE)
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::same(2))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if theme::quiet_enabled(ui, "Alles", any)
                    .on_hover_text("Alle Ausschnitte verwerfen und den ganzen Zeitraum umwandeln")
                    .clicked()
                {
                    self.remember_slices(self.windows.clone());
                    self.windows.clear();
                    self.selected = 0;
                    self.text_from_window();
                }
                if theme::quiet_enabled(ui, "Entfernen", any)
                    .on_hover_text("Den gewählten Ausschnitt verwerfen")
                    .clicked()
                {
                    self.remember_slices(self.windows.clone());
                    self.windows.remove(self.selected.min(self.windows.len() - 1));
                    self.selected = self.selected.min(self.windows.len().saturating_sub(1));
                    self.text_from_window();
                }
                if theme::quiet_enabled(ui, "Teilen", any)
                    .on_hover_text(
                        "Den gewählten Ausschnitt halbieren, damit die Hälften \
                         auseinandergezogen und die Mitte ausgelassen werden kann",
                    )
                    .clicked()
                    && let Some(w) = self.windows.get(self.selected).copied()
                    && let (Some(a), Some(b)) = (
                        w.from.or(bounds.map(|x| x.0)),
                        w.until.or(bounds.map(|x| x.1)),
                    )
                    && b > a + 20_000_000
                {
                    self.remember_slices(self.windows.clone());
                    let mid = a + (b - a) / 2;
                    self.windows[self.selected] = Range {
                        from: Some(a),
                        until: Some(mid),
                    };
                    self.windows.insert(
                        self.selected + 1,
                        Range {
                            from: Some(mid),
                            until: Some(b),
                        },
                    );
                    self.text_from_window();
                }
                let can_undo = !self.undo.is_empty();
                if theme::quiet_enabled(ui, "Rückgängig", can_undo)
                    .on_hover_text("Die letzte Änderung an den Ausschnitten zurücknehmen (Strg+Z)")
                    .clicked()
                {
                    self.undo_slices();
                }
            });
    }

    /// One chip per slice: which one is being edited, and a way to drop it.
    fn slice_chips(&mut self, ui: &mut egui::Ui) {
        let mut pick: Option<usize> = None;
        let mut drop: Option<usize> = None;
        let labels: Vec<String> = self
            .windows
            .iter()
            .map(|w| {
                let short = |t: Option<i64>| match t {
                    Some(t) => convert::filetime_to_short(t)[..16].to_string(),
                    None => "offen".into(),
                };
                format!("{} – {}", short(w.from), short(w.until))
            })
            .collect();

        ui.horizontal_wrapped(|ui| {
            for (i, label) in labels.iter().enumerate() {
                if ui
                    .selectable_label(i == self.selected, body(format!("{}.  {label}", i + 1)))
                    .clicked()
                {
                    pick = Some(i);
                }
                if theme::cross(ui)
                    .on_hover_text("Diesen Ausschnitt entfernen")
                    .clicked()
                {
                    drop = Some(i);
                }
                ui.add_space(10.0);
            }
        });

        if let Some(i) = pick {
            self.selected = i;
            self.text_from_window();
        }
        if let Some(i) = drop {
            self.remember_slices(self.windows.clone());
            self.windows.remove(i);
            self.selected = self.selected.min(self.windows.len().saturating_sub(1));
            self.text_from_window();
        }
    }

    /// How much of the log the slices keep, live: a short bar and the count,
    /// beside the title, so the effect of a drag is seen at once. Drawn
    /// right to left.
    fn kept_label(&mut self, ui: &mut egui::Ui) {
        if self.value_dbs().is_empty() {
            return;
        }
        if self.windows().map(|w| w.is_open()).unwrap_or(false) {
            theme::chip(ui, "ganzer Zeitraum", true);
            return;
        }
        let total: u64 = self
            .included()
            .filter(|s| s.probe.has_values)
            .map(|s| s.probe.value_rows)
            .sum();
        match self.total_values() {
            Some(0) => {
                ui.label(body("Keine Werte im Ausschnitt").color(BAD));
            }
            Some(kept) => {
                let share = kept as f32 / total.max(1) as f32;
                ui.label(hint(format!("von {}", sep(total))));
                ui.label(strong(format!("{} Werte", sep(kept))));
                ui.add_space(6.0);
                ui.label(strong(format!("{:.0} %", share * 100.0)).color(theme::ACCENT_HOVER));
                theme::meter(ui, share, 90.0);
            }
            None => {
                ui.label(hint("wird gezählt …"));
                ui.spinner();
            }
        }
    }

    // -- step 3: what comes out --------------------------------------------

    fn output_step(&mut self, ui: &mut egui::Ui, loaded: bool) {
        let state = if loaded { Step::Current } else { Step::Later };
        theme::step_title(
            ui,
            "Ausgabe festlegen",
            "Was in die CSV-Dateien kommt und wohin sie gespeichert werden.",
            state,
            |_| {},
        );
        if !loaded {
            return;
        }
        ui.add_space(theme::GAP + 2.0);
        // A grid: the names in one column, and the controls in two more
        // whose left edges are the same on every row.
        const LABEL: f32 = 140.0;
        const FIRST: f32 = 300.0;

        theme::panel()
            .inner_margin(egui::Margin::symmetric(16, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;

                // -- which tables
                theme::form_row(ui, "Tabellen", LABEL, |ui| {
                    theme::form_cell(ui, FIRST, |ui| {
                        theme::check(ui, &mut self.write_long, "Lang")
                            .on_hover_text("Eine Zeile je Messwert");
                        if self.write_long {
                            let chosen = self.columns().len();
                            if theme::quiet(
                                ui,
                                &format!("Spalten ({chosen} von {}) …", Column::ALL.len()),
                            )
                            .on_hover_text("Spalten auswählen, ordnen und umbenennen")
                            .clicked()
                            {
                                self.columns_open = true;
                            }
                        }
                    });
                    theme::check(ui, &mut self.write_wide, "Breit")
                        .on_hover_text("Eine Zeile je Zeitpunkt, eine Spalte je Tag");
                    if !self.write_long && !self.write_wide {
                        ui.add_space(12.0);
                        ui.label(body("mindestens eine auswählen").color(WARN));
                    }
                });
                theme::divider(ui);

                // -- how they are written
                theme::form_row(ui, "Format", LABEL, |ui| {
                    theme::form_cell(ui, FIRST, |ui| {
                        theme::check(ui, &mut self.semicolon, "Semikolon als Trennzeichen")
                            .on_hover_text("Für Excel mit deutschem Dezimalkomma");
                    });
                    theme::check(ui, &mut self.shorten_names, "Tag-Namen kürzen")
                        .on_hover_text("Nur den Teil behalten, der die Tags unterscheidet");
                });
                if self.shorten_names {
                    // Under the box it belongs to, in the second column.
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL + FIRST + ui.spacing().item_spacing.x * 2.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            match self.shorten_example() {
                                Some((full, short)) => {
                                    ui.add(
                                        egui::Label::new(muted(format!("Beispiel: {full}")))
                                            .wrap(),
                                    );
                                    ui.add(
                                        egui::Label::new(muted(format!("wird zu  {short}"))
                                            .color(TEXT))
                                        .wrap(),
                                    );
                                }
                                None => {
                                    ui.label(muted(
                                        "Für ein Beispiel eine Tag-Datenbank auswählen.",
                                    ));
                                }
                            }
                        });
                    });
                    ui.add_space(10.0);
                }
                theme::divider(ui);

                // -- where they go
                theme::form_row(ui, "Speichern als", LABEL, |ui| {
                    theme::form_cell(ui, FIRST, |ui| {
                        let mut name = self.base_name();
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut name)
                                    .desired_width(FIRST - 24.0)
                                    .margin(egui::vec2(8.0, 6.0)),
                            )
                            .on_hover_text("Der Teil vor _long.csv und _wide.csv")
                            .changed()
                        {
                            self.base_name = Some(name);
                        }
                    });
                    ui.label(hint("in"));
                    match self.chosen_dir() {
                        Some(dir) => {
                            if ui
                                .add(
                                    egui::Label::new(body(shown_path(&dir)))
                                        .wrap()
                                        .sense(egui::Sense::click()),
                                )
                                .on_hover_text("Ordner im Explorer öffnen")
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                open_folder(&dir);
                            }
                        }
                        None => {
                            ui.label(hint("dem Ordner der ersten Datenbank"));
                        }
                    }
                    if theme::quiet(ui, "Ändern …")
                        .on_hover_text("Ordner und Namen im Explorer wählen")
                        .clicked()
                    {
                        self.browse_output();
                    }
                });
                theme::divider(ui);

                // -- how much goes into a chart
                theme::form_row(ui, "Diagramm-Detail", LABEL, |ui| {
                    theme::form_cell(ui, FIRST, |ui| {
                        let options: Vec<(Detail, &str, &str)> =
                            [Detail::Light, Detail::Balanced, Detail::Full]
                                .into_iter()
                                .map(|d| (d, d.label(), d.describe()))
                                .collect();
                        theme::segmented(ui, &mut self.detail, &options);
                    });
                    ui.label(hint(self.detail.describe()));
                });
            });
    }

    /// Write the column arrangement to the settings file, so reopening the
    /// chooser -- or the program -- shows it exactly as it was left.
    fn remember_columns(&mut self) {
        self.last.columns = self.long_columns.clone();
        self.last.save();
    }

    /// Pick the columns of the long table and put them in order.
    fn columns_window(&mut self, ctx: &egui::Context) {
        if !self.columns_open {
            return;
        }

        // The list is taken out for the duration, so the rows can be drawn and
        // rearranged without borrowing the whole app twice.
        let mut rows = std::mem::take(&mut self.long_columns);
        let mut drag = self.drag;
        let semicolon = self.semicolon;
        let mut close = false;
        let mut reset = false;
        
        // A dialog over the rest of the window, which steps back meanwhile.
        let modal = egui::Modal::new(egui::Id::new("columns"))
            .backdrop_color(egui::Color32::from_black_alpha(160))
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0, theme::EDGE))
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(ctx, |ui| {
                ui.set_width(680.0);
                ui.label(theme::title("Spalten der langen Tabelle"));
                ui.add_space(4.0);
                ui.label(hint(
                    "Anhaken, was geschrieben wird. Zum Umsortieren am Griff ziehen. \
                     Überschriften lassen sich frei benennen.",
                ));
                ui.add_space(theme::GAP + 4.0);

                column_rows(ui, &mut rows, &mut drag);

                ui.add_space(theme::GAP + 4.0);
                ui.label(muted("Erste Zeile der Datei"));
                let chosen: Vec<OutColumn> =
                    rows.iter().filter(|r| r.on).map(ColumnRow::out).collect();
                if chosen.is_empty() {
                    ui.label(body("leer – mindestens eine Spalte anhaken").color(BAD));
                } else {
                    let sep = if semicolon { ";" } else { "," };
                    let headers: Vec<&str> = chosen.iter().map(|c| c.header.as_str()).collect();
                    ui.add(egui::Label::new(body(headers.join(sep))).wrap());
                    let mut seen = headers.clone();
                    seen.sort_unstable();
                    seen.dedup();
                    if seen.len() != headers.len() {
                        theme::warning(
                            ui,
                            "Zwei Spalten haben dieselbe Überschrift – das vertragen \
                             viele Programme nicht.",
                            WARN,
                        );
                    }
                }

                ui.add_space(theme::GAP + 8.0);
                ui.horizontal(|ui| {
                    if theme::primary(ui, "Fertig", true, egui::vec2(120.0, 36.0)).clicked() {
                        close = true;
                    }
                    if theme::secondary(ui, "Zurücksetzen", true, egui::vec2(130.0, 36.0))
                        .on_hover_text("Alle Spalten mit ursprünglicher Überschrift und Reihenfolge")
                        .clicked()
                    {
                        reset = true;
                    }
                });
            });

        if modal.should_close() {
            close = true;
        }
        self.long_columns = if reset { ColumnRow::all() } else { rows };
        self.drag = drag;
        if close {
            self.columns_open = false;
            self.drag = None;
            self.remember_columns();
        }
    }

    /// One real tag, before and after shortening, so the option is not a guess.
    fn shorten_example(&self) -> Option<(String, String)> {
        let tags = self.included().find(|s| s.probe.has_tags)?;
        let short = convert::shorten_tag_names(&tags.probe.tag_names);
        let (id, full) = tags.probe.tag_names.first()?;
        Some((full.clone(), short.get(id)?.clone()))
    }

    /// Ask for the output name with a file dialog, so the folder and the
    /// suggested name are both visible in Explorer.
    fn browse_output(&mut self) {
        let suggested = format!("{}_long.csv", self.base_name());
        let Some(path) = rfd::FileDialog::new()
            .set_directory(self.out_dir())
            .set_file_name(&suggested)
            .set_title("Speicherort und Dateinamen wählen")
            .add_filter("CSV", &["csv"])
            .save_file()
        else {
            return;
        };
        if let Some(dir) = path.parent() {
            self.out_dir = Some(dir.to_path_buf());
        }
        // The dialog hands back one full file name; keep only the common part.
        let mut stem = stem_of(&path);
        for tail in ["_long", "_wide"] {
            if let Some(cut) = stem.strip_suffix(tail) {
                stem = cut.to_string();
            }
        }
        if !stem.trim().is_empty() {
            self.base_name = Some(stem);
        }
    }

    /// The bar under the steps: on the left, in so many words, what pressing
    /// the button will do -- or what still stands in the way; on the right,
    /// the button, with the two ways of charting before it.
    fn action_bar(&mut self, ui: &mut egui::Ui) {
        let ready = self.ready();
        let can_chart = !self.value_dbs().is_empty() && self.windows().is_ok();
        let files = self.value_dbs().len();

        ui.horizontal(|ui| {
            ui.set_min_height(48.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if ready {
                    match self.total_values() {
                        Some(n) => {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    egui::RichText::new(sep(n))
                                        .font(egui::FontId::new(20.0, theme::bold_family()))
                                        .color(TEXT),
                                );
                                ui.label(body(format!(
                                    "Werte aus {files} {}",
                                    if files == 1 { "Datei" } else { "Dateien" }
                                )));
                            });
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(body("Werte werden gezählt …"));
                            });
                        }
                    }
                    ui.label(muted(format!("ergibt  {}", self.output_files().join("  ·  "))));
                } else {
                    let (why, how) = if self.sources.is_empty() {
                        ("Noch keine Dateien", "Oben die HMI-Datenbanken hineinziehen oder wählen.")
                    } else if self.value_dbs().is_empty() {
                        ("Keine Messwerte ausgewählt", "Mindestens eine Datei mit Werten ankreuzen.")
                    } else if !self.write_long && !self.write_wide {
                        ("Keine Tabelle gewählt", "Unter Ausgabe „Lang“ oder „Breit“ ankreuzen.")
                    } else if self.write_long && self.columns().is_empty() {
                        ("Die lange Tabelle hat keine Spalten", "Unter Ausgabe „Spalten …“ öffnen.")
                    } else if self.total_values() == Some(0) {
                        ("Keine Werte im Zeitraum", "Den Ausschnitt verschieben oder vergrößern.")
                    } else {
                        ("Zeitraum unklar", "Die Eingabe unter Von und Bis prüfen.")
                    };
                    ui.label(strong(why).color(MUTED));
                    ui.label(muted(how));
                }
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::primary(ui, "In CSV umwandeln", ready, egui::vec2(220.0, 46.0)).clicked() {
                    self.start();
                }
                ui.add_space(4.0);
                if theme::secondary(ui, "Im Browser", can_chart, egui::vec2(124.0, 46.0))
                    .on_hover_text("Eine eigenständige Diagrammseite schreiben und öffnen")
                    .clicked()
                {
                    self.start_chart();
                }
                if theme::secondary(ui, "Diagramm", can_chart, egui::vec2(124.0, 46.0))
                    .on_hover_text("Die Werte hier im Fenster zeichnen")
                    .clicked()
                {
                    self.start_plot();
                }
            });
        });
    }

    // -- while it runs -----------------------------------------------------
    fn running_view(&mut self, ui: &mut egui::Ui) {
        let State::Running {
            cancel,
            stage,
            done,
            total,
            ..
        } = &self.state
        else {
            return;
        };
        let (stage, done, total, cancel) = (stage.clone(), *done, *total, cancel.clone());

        ui.add_space((ui.available_height() * 0.3).max(40.0));
        theme::column(ui, 520.0, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::title(format!("{stage} …")));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if total > 0 {
                        ui.label(
                            strong(format!(
                                "{:.0} %",
                                done as f32 / total as f32 * 100.0
                            ))
                            .color(theme::ACCENT_HOVER),
                        );
                    }
                });
            });
            ui.add_space(4.0);
            theme::progress(ui, (total > 0).then(|| done as f32 / total as f32), 520.0);
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(muted(if total > 0 {
                    format!("{} von {} Zeilen", sep(done), sep(total))
                } else {
                    "wird vorbereitet".into()
                }));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(muted("Die Quelldateien werden nur gelesen."));
                });
            });
            ui.add_space(28.0);
            ui.vertical_centered(|ui| {
                if theme::secondary(ui, "Abbrechen", true, egui::vec2(150.0, 40.0)).clicked() {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
        });
    }

    // -- the result --------------------------------------------------------
    fn done_view(&mut self, ui: &mut egui::Ui) {
        let State::Done(s) = &self.state else { return };
        let s = s.clone();

        ui.add_space(24.0);
        theme::column(ui, 760.0, |ui| {
            result_heading(
                ui,
                "Umwandlung fertig",
                format!(
                    "{} Werte · {} Tags · {} {} · {} s",
                    sep(s.rows),
                    s.tags_used,
                    s.sources.len(),
                    if s.sources.len() == 1 {
                        "Datenbank"
                    } else {
                        "Datenbanken"
                    },
                    decimal(s.secs as f64, 1)
                ),
            );
            ui.add_space(theme::SECTION);

            let files: Vec<(PathBuf, String)> = s
                .files
                .iter()
                .map(|(path, rows)| {
                    (
                        path.clone(),
                        format!("{} Zeilen · {}", sep(*rows), human_size(path)),
                    )
                })
                .collect();
            written_files(ui, "Geschrieben", &files);

            if s.bad_quality > 0 {
                ui.add_space(6.0);
                theme::banner(
                    ui,
                    format!(
                        "{} Zeilen haben eine andere Qualität als 192 – sie sind enthalten, \
                         siehe Spalte Quality.",
                        sep(s.bad_quality)
                    ),
                    WARN,
                );
            }
            if !s.unknown_ids.is_empty() {
                ui.add_space(6.0);
                theme::banner(
                    ui,
                    format!(
                        "{} Tag-Nummern hatten keinen Namen und heißen id_<Nummer>.",
                        s.unknown_ids.len()
                    ),
                    WARN,
                );
            }

            ui.add_space(theme::GAP);
            egui::CollapsingHeader::new(hint("Details"))
                .id_salt("done-details")
                .default_open(false)
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    if let Some((a, b)) = s.span {
                        kv(
                            ui,
                            "Zeitraum",
                            format!(
                                "{} – {}",
                                convert::filetime_to_short(a),
                                convert::filetime_to_short(b)
                            ),
                        );
                    }
                    kv(
                        ui,
                        "Tags",
                        format!("{} von {} haben Werte", s.tags_used, s.tags_total),
                    );
                    kv(ui, "Zeitpunkte", sep(s.timestamps));
                    for (k, (path, rows)) in s.sources.iter().enumerate() {
                        kv(
                            ui,
                            if k == 0 { "Quellen" } else { "" },
                            format!("{} Werte aus {}", sep(*rows), file_name(path)),
                        );
                    }
                });

            ui.add_space(theme::SECTION);
            ui.horizontal(|ui| {
                if theme::primary(ui, "Ordner öffnen", true, egui::vec2(180.0, 44.0)).clicked()
                    && let Some((p, _)) = s.files.first()
                {
                    reveal(p);
                }
                if theme::secondary(ui, "Diagramm", true, egui::vec2(124.0, 44.0)).clicked() {
                    self.start_plot();
                }
                if theme::secondary(ui, "Im Browser", true, egui::vec2(124.0, 44.0)).clicked() {
                    self.start_chart();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::quiet(ui, "Weitere umwandeln")
                        .on_hover_text("Zurück zu den Dateien")
                        .clicked()
                    {
                        self.state = State::Setup;
                        self.notes.clear();
                    }
                });
            });
        });
    }

    /// The chart inside this window, with a bar to leave it by.
    fn plot_view(&mut self, ui: &mut egui::Ui) {
        let mut leave = false;
        let mut to_browser = false;
        if let State::Plotting(view) = &mut self.state {
            ui.horizontal(|ui| {
                if theme::quiet(ui, "Zurück").clicked() {
                    leave = true;
                }
                ui.add_space(6.0);
                ui.label(theme::title("Diagramm"));
                ui.label(hint(view.caption()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::secondary(ui, "Im Browser öffnen", true, egui::vec2(0.0, 34.0))
                        .on_hover_text("Dieselben Werte als Seite, mit mehr Diagrammarten")
                        .clicked()
                    {
                        to_browser = true;
                    }
                });
            });
            ui.add_space(theme::GAP);
            view.ui(ui);
        }
        if leave {
            self.state = State::Setup;
        }
        if to_browser {
            self.start_chart();
        }
    }

    /// What was drawn, and where the page went.
    fn chart_view(&mut self, ui: &mut egui::Ui) {
        let State::Charted(c) = &self.state else {
            return;
        };
        let (path, read, drawn, tags, skipped, bytes, secs) = (
            c.path.clone(),
            c.read,
            c.drawn,
            c.tags,
            c.skipped_text,
            c.bytes,
            c.secs,
        );

        ui.add_space(24.0);
        theme::column(ui, 760.0, |ui| {
            result_heading(
                ui,
                "Diagramm im Browser geöffnet",
                format!(
                    "{tags} Tags · {} von {} Punkten · {} s",
                    sep(drawn),
                    sep(read),
                    decimal(secs as f64, 1)
                ),
            );
            ui.add_space(theme::SECTION);
            written_files(
                ui,
                "Geschrieben",
                &[(
                    path.clone(),
                    format!("{} · eine Datei, funktioniert ohne Internet", human_bytes(bytes)),
                )],
            );
            if skipped > 0 {
                ui.add_space(6.0);
                theme::banner(
                    ui,
                    format!("{skipped} Tags enthalten Text statt Zahlen und werden nicht gezeichnet."),
                    WARN,
                );
            }

            ui.add_space(theme::SECTION);
            ui.horizontal(|ui| {
                if theme::primary(ui, "Erneut öffnen", true, egui::vec2(180.0, 44.0)).clicked() {
                    chart::open(&path);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if theme::quiet(ui, "Zurück zu den Dateien").clicked() {
                        self.state = State::Setup;
                        self.notes.clear();
                    }
                });
            });
        });
    }

    fn failed_view(&mut self, ui: &mut egui::Ui) {
        let State::Failed(msg) = &self.state else {
            return;
        };
        let msg = msg.clone();
        let cancelled = msg == "cancelled";

        ui.add_space(24.0);
        theme::column(ui, 760.0, |ui| {
            ui.label(theme::heading(if cancelled {
                "Abgebrochen"
            } else {
                "Das hat nicht geklappt"
            }));
            ui.add_space(theme::GAP);
            if cancelled {
                ui.label(hint(
                    "Es wurde nichts fertiggestellt. Halb geschriebene CSV-Dateien können \
                     gelöscht werden.",
                ));
            } else {
                theme::banner(ui, msg, BAD);
                ui.add_space(theme::GAP);
                ui.label(hint(
                    "Stammen die Dateien aus demselben HMI-Export, und darf in den \
                     Ausgabeordner geschrieben werden?",
                ));
            }
            ui.add_space(theme::SECTION);
            if theme::primary(ui, "Zurück zu den Dateien", true, egui::vec2(220.0, 44.0)).clicked() {
                self.state = State::Setup;
            }
        });
    }

    fn notes_view(&mut self, ui: &mut egui::Ui) {
        if self.notes.is_empty() {
            return;
        }
        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("notes")
            .max_height(96.0)
            .show(ui, |ui| {
                for (text, color) in &self.notes {
                    ui.add(egui::Label::new(muted(text).color(*color)).wrap());
                }
            });
    }
}

/// The top of a result screen: a tick, what happened, and in one line how much.
fn result_heading(ui: &mut egui::Ui, title: &str, line: String) {
    ui.horizontal(|ui| {
        theme::done_mark(ui);
        ui.add_space(6.0);
        ui.vertical(|ui| {
            ui.label(theme::heading(title));
            ui.label(hint(line));
        });
    });
}

/// The files that were written, in one panel: each name to click, what is in
/// it, and a way to find it.
fn written_files(ui: &mut egui::Ui, title: &str, files: &[(PathBuf, String)]) {
    ui.label(muted(title));
    ui.add_space(2.0);
    theme::panel()
        .inner_margin(egui::Margin::symmetric(16, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            for (k, (path, detail)) in files.iter().enumerate() {
                if k > 0 {
                    theme::divider(ui);
                }
                ui.horizontal(|ui| {
                    ui.set_min_height(58.0);
                    ui.vertical(|ui| {
                        ui.add_space(10.0);
                        ui.spacing_mut().item_spacing.y = 2.0;
                        if ui
                            .add(
                                egui::Label::new(strong(file_name(path)))
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text(path.display().to_string())
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .clicked()
                        {
                            reveal(path);
                        }
                        ui.label(muted(detail));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::quiet(ui, "Im Ordner zeigen").clicked() {
                            reveal(path);
                        }
                    });
                });
            }
        });
}

// ---------------------------------------------------------------- file table

/// Height of one row of the file table.
const FILE_ROW: f32 = 36.0;

/// What a click on one row of the file table asked for.
enum RowAct {
    /// Tick or untick this row; with shift held, everything back to the last
    /// row that was ticked.
    Toggle(usize, bool),
    /// Import this file and no other.
    Only(usize),
    /// Take it out of the list altogether.
    Remove(usize),
    Reveal(usize),
}

/// How wide each column of the file table is at the width the window happens
/// to have. The file name takes what is left over, and in a narrow window the
/// time column gives way to it rather than squeezing it out.
struct Cols {
    tick: f32,
    name: f32,
    role: f32,
    rows: f32,
    span: f32,
    drop: f32,
}

impl Cols {
    const GAP: f32 = 12.0;

    fn new(total: f32) -> Self {
        let (tick, role, rows, drop) = (22.0, 96.0, 96.0, 24.0);
        let mut span = 230.0;
        let left = |span: f32| {
            let shown = 4.0 + if span > 0.0 { 1.0 } else { 0.0 };
            total - (tick + role + rows + drop + span) - Self::GAP * shown
        };
        if left(span) < 220.0 {
            span = 0.0;
        }
        Self {
            tick,
            name: left(span).max(120.0),
            role,
            rows,
            span,
            drop,
        }
    }
}

/// One file: the tick, its name, what it holds, and how much of it is taken.
fn source_row(ui: &mut egui::Ui, src: &mut Source, cols: &Cols, index: usize) -> Option<RowAct> {
    let mut act: Option<RowAct> = None;
    let full_path = src.path().display().to_string();
    let on = src.on;

    let out = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("file-row", index))
            .sense(egui::Sense::click()),
        |ui| {
            // Kept for later, so the background can be painted under the row
            // once it is known whether the pointer is on it.
            let bg = ui.painter().add(egui::Shape::Noop);
            ui.horizontal(|ui| {
                ui.set_min_height(FILE_ROW);
                ui.spacing_mut().item_spacing.x = Cols::GAP;
                let shift = ui.input(|i| i.modifiers.shift);

                cell(ui, cols.tick, egui::Align::Center, |ui| {
                    let mut ticked = on;
                    if theme::check(ui, &mut ticked, "")
                        .on_hover_text("Diese Datei umwandeln")
                        .clicked()
                    {
                        act = Some(RowAct::Toggle(index, shift));
                    }
                });

                cell(ui, cols.name, egui::Align::Min, |ui| {
                    let name = file_name(src.path());
                    let text = if on { body(name) } else { body(name).color(DIM) };
                    if ui
                        .add(egui::Label::new(text).wrap().sense(egui::Sense::click()))
                        .on_hover_text(format!("{full_path}\n\nKlicken: im Explorer zeigen"))
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        act = Some(RowAct::Reveal(index));
                    }
                });

                cell(ui, cols.role, egui::Align::Min, |ui| {
                    let (text, tags) = match (src.probe.has_tags, src.probe.has_values) {
                        (true, true) => ("Tags + Werte", true),
                        (true, false) => ("Tag-Namen", true),
                        _ => ("Werte", false),
                    };
                    theme::chip(ui, text, tags && on);
                });

                cell(ui, cols.rows, egui::Align::Max, |ui| {
                    row_amount(ui, src);
                });

                cell(ui, cols.span, egui::Align::Min, |ui| match src.span() {
                    Some((a, b)) => {
                        let text = format!(
                            "{}  ·  {}",
                            &convert::filetime_to_short(a)[..16],
                            duration_text(b - a)
                        );
                        ui.label(if on { hint(text) } else { hint(text).color(DIM) })
                            .on_hover_text(format!(
                                "{}\nbis {}",
                                convert::filetime_to_text(a),
                                convert::filetime_to_text(b)
                            ));
                    }
                    None => {
                        ui.label(muted("–"));
                    }
                });

                cell(ui, cols.drop, egui::Align::Center, |ui| {
                    if theme::cross(ui)
                        .on_hover_text("Aus der Liste nehmen")
                        .clicked()
                    {
                        act = Some(RowAct::Remove(index));
                    }
                });
            });
            bg
        },
    );

    let response = out.response;
    let hovered = response.hovered();
    // Clicking the row itself is the easiest way to include or exclude a file;
    // the widgets on it keep their own clicks.
    if response.clicked() {
        act = Some(RowAct::Toggle(index, ui.input(|i| i.modifiers.shift)));
    }
    // One file on its own, without unticking the rest by hand.
    if response.secondary_clicked() {
        act = Some(RowAct::Only(index));
    }
    if hovered {
        response.clone().on_hover_text(format!(
            "{}  ·  Rechtsklick: nur diese Datei\n{} auf der Festplatte",
            if on { "Klicken: weglassen" } else { "Klicken: aufnehmen" },
            human_bytes(src.probe.bytes)
        ));
    }

    let fill = if hovered {
        theme::RAISED
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().set(
        out.inner,
        egui::epaint::RectShape::filled(
            response.rect.expand2(egui::vec2(4.0, 1.0)),
            egui::CornerRadius::same(6),
            fill,
        ),
    );
    act
}

/// How much this file contributes, and what it was out of.
fn row_amount(ui: &mut egui::Ui, src: &Source) {
    if !src.probe.has_values {
        ui.label(hint(format!("{} Tags", sep(src.probe.tag_count))));
        return;
    }
    let on = src.on;
    match &src.kept {
        Count::Counting => {
            ui.spinner();
        }
        Count::Bad(e) => {
            ui.label(muted("–").color(BAD)).on_hover_text(e.clone());
        }
        Count::Whole => {
            let text = sep(src.probe.value_rows);
            ui.label(if on { body(text) } else { body(text).color(DIM) })
                .on_hover_text("Alle Werte der Datei – kein Zeitraum gewählt");
        }
        Count::Kept(k) => {
            let text = sep(k.rows);
            let color = if !on {
                DIM
            } else if k.rows == 0 {
                MUTED
            } else {
                TEXT
            };
            ui.label(body(text).color(color)).on_hover_text(format!(
                "{} von {} Werten liegen im Zeitraum",
                sep(k.rows),
                sep(k.total_rows)
            ));
        }
    }
}

/// A fixed-width box inside a row, so every column lines up down the table.
fn cell(ui: &mut egui::Ui, width: f32, align: egui::Align, add: impl FnOnce(&mut egui::Ui)) {
    if width <= 0.0 {
        return;
    }
    let layout = match align {
        egui::Align::Max => egui::Layout::right_to_left(egui::Align::Center),
        _ => egui::Layout::left_to_right(egui::Align::Center),
    };
    ui.allocate_ui_with_layout(egui::vec2(width, FILE_ROW), layout, |ui| {
        ui.set_width(width);
        ui.set_min_height(FILE_ROW);
        add(ui);
    });
}

/// `9 h 7 min`, `1 Tag 9 h`, `38 s` -- how long a file covers, in two parts at
/// most.
fn duration_text(ticks: i64) -> String {
    let secs = (ticks / 10_000_000).max(0);
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    match (d, h, m) {
        (0, 0, 0) => format!("{s} s"),
        (0, 0, _) => format!("{m} min"),
        (0, _, _) => format!("{h} h {m} min"),
        (1, _, _) => format!("1 Tag {h} h"),
        _ => format!("{d} Tage {h} h"),
    }
}

// ---------------------------------------------------------------- columns

/// One line of the column chooser.
#[derive(Clone, PartialEq, Eq)]
struct ColumnRow {
    source: Column,
    on: bool,
    /// What the user typed; empty means the original heading.
    title: String,
}

impl ColumnRow {
    fn all() -> Vec<Self> {
        Column::ALL
            .into_iter()
            .map(|source| Self {
                source,
                // Every column is offered, but the source file is left unticked
                // until it is wanted: with one database it says the same thing
                // on every row.
                on: Column::DEFAULT.contains(&source),
                title: String::new(),
            })
            .collect()
    }

    /// The heading this row will write.
    fn header(&self) -> &str {
        let typed = self.title.trim();
        if typed.is_empty() {
            self.source.header()
        } else {
            typed
        }
    }

    fn out(&self) -> OutColumn {
        OutColumn {
            source: self.source,
            header: self.header().to_string(),
        }
    }
}

/// Height of one row of the chooser.
const ROW_H: f32 = 54.0;
/// How long a row takes to slide to a new place.
const SLIDE: f32 = 0.16;

/// Draw the rows at animated positions and let them be dragged into order.
///
/// Rows are placed by hand rather than stacked, so that when the order changes
/// each one slides to its new place instead of jumping.
fn column_rows(ui: &mut egui::Ui, rows: &mut [ColumnRow], drag: &mut Option<(Column, f32)>) {
    let ctx = ui.ctx().clone();
    let count = rows.len();
    let (area, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), count as f32 * ROW_H),
        egui::Sense::hover(),
    );
    let pointer = ctx.pointer_interact_pos();
    if drag.is_some() && !ctx.input(|i| i.pointer.any_down()) {
        *drag = None;
    }

    // The dragged row is painted last so it rides above the others.
    let dragged_index = drag.and_then(|(c, _)| rows.iter().position(|r| r.source == c));
    let mut order: Vec<usize> = (0..count).collect();
    if let Some(d) = dragged_index {
        order.retain(|&i| i != d);
        order.push(d);
    }

    let mut moved: Option<(usize, usize)> = None;
    for &i in &order {
        let source = rows[i].source;
        let anim_id = egui::Id::new(("column-row", source.key()));
        let held = dragged_index == Some(i);

        // Where the row sits this frame.
        let y = if held {
            let grab = drag.map(|(_, g)| g).unwrap_or(0.0);
            let wanted = pointer.map_or(i as f32 * ROW_H, |p| p.y - area.top() - grab);
            let clamped = wanted.clamp(0.0, (count - 1) as f32 * ROW_H);
            // Keep the stored value level with the pointer, so letting go
            // glides from here into the slot instead of snapping.
            ctx.animate_value_with_time(anim_id, clamped, 0.0);
            clamped
        } else {
            ctx.animate_value_with_time(anim_id, i as f32 * ROW_H, SLIDE)
        };

        let rect = egui::Rect::from_min_size(
            egui::pos2(area.left(), area.top() + y),
            egui::vec2(area.width(), ROW_H - 6.0),
        );

        ui.painter().rect_filled(
            rect,
            egui::CornerRadius::same(8),
            if held { theme::ACCENT_DEEP } else { theme::RAISED.gamma_multiply(0.6) },
        );

        let response = ui
            .scope_builder(
                egui::UiBuilder::new().max_rect(rect.shrink2(egui::vec2(8.0, 0.0))).id_salt(anim_id),
                |ui| column_row(ui, &mut rows[i], anim_id, held),
            )
            .inner;

        if response.drag_started() {
            let grab = pointer.map_or(ROW_H / 2.0, |p| p.y - rect.top());
            *drag = Some((source, grab));
        }
        if response.drag_stopped() {
            *drag = None;
        }
        if held {
            // Live reordering: the others move aside while the row is held.
            let wanted = ((y + ROW_H / 2.0) / ROW_H)
                .floor()
                .clamp(0.0, (count - 1) as f32);
            let wanted = wanted as usize;
            if wanted != i {
                moved = Some((i, wanted));
            }
        }
    }

    ui.advance_cursor_after_rect(area);

    if let Some((from, to)) = moved {
        let row = rows[from].clone();
        if from < to {
            rows[from..=to].rotate_left(1);
        } else {
            rows[to..=from].rotate_right(1);
        }
        rows[to] = row;
    }
}

/// One row: handle, tick, what it holds, and the heading to write.
fn column_row(
    ui: &mut egui::Ui,
    row: &mut ColumnRow,
    anim_id: egui::Id,
    held: bool,
) -> egui::Response {
    // Everything on the row fades with the tick, so an unused column reads as
    // switched off rather than merely unticked.
    let lit = ui
        .ctx()
        .animate_bool_with_time(anim_id.with("on"), row.on, 0.15);
    let text_color = DIM.lerp_to_gamma(TEXT, lit);
    let grip_color = DIM.lerp_to_gamma(MUTED, lit);

    let mut handle = ui.interact(
        egui::Rect::NOTHING,
        anim_id.with("handle"),
        egui::Sense::hover(),
    );

    ui.horizontal_centered(|ui| {
        // -- drag handle
        let (grip, response) =
            ui.allocate_exact_size(egui::vec2(22.0, ROW_H - 16.0), egui::Sense::drag());
        let response = response.on_hover_cursor(if held {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        });
        let bars =
            ui.ctx()
                .animate_bool_with_time(anim_id.with("grip"), held || response.hovered(), 0.12);
        let paint = ui.painter();
        let colour = grip_color.lerp_to_gamma(ACCENT, bars);
        for k in 0..3 {
            let y = grip.center().y + (k as f32 - 1.0) * 5.0;
            paint.hline(
                (grip.center().x - 7.0)..=(grip.center().x + 7.0),
                y,
                egui::Stroke::new(2.0, colour),
            );
        }
        handle = response;

        ui.add_space(4.0);
        theme::check(ui, &mut row.on, "");
        ui.add_space(4.0);

        // -- what the column holds
        ui.vertical(|ui| {
            ui.add_space(6.0);
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(strong(row.source.header()).color(text_color));
            ui.label(muted(row.source.describe()));
        });

        // -- the heading to write
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut row.title)
                    .hint_text(row.source.header())
                    .desired_width(200.0)
                    .margin(egui::vec2(8.0, 5.0)),
            )
            .on_hover_text("Überschrift in der CSV-Datei. Leer lässt die ursprüngliche.");
            ui.label(muted("Überschrift"));
        });
    });

    handle
}

// ---------------------------------------------------------------- pieces

/// A value beside a grey label.
fn kv(ui: &mut egui::Ui, key: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [120.0, 20.0],
            egui::Label::new(hint(key)).halign(egui::Align::LEFT),
        );
        ui.add(egui::Label::new(body(value.into())).wrap());
    });
}

/// One date/time input with its label and a way to clear it. True when the
/// user changed it.
fn range_field(ui: &mut egui::Ui, label: &str, text: &mut String, exact: String) -> bool {
    let mut edited = false;
    ui.label(hint(label));
    edited |= ui
        .add(
            egui::TextEdit::singleline(text)
                .hint_text("TT.MM.JJJJ hh:mm")
                .desired_width(170.0)
                .margin(egui::vec2(8.0, 6.0)),
        )
        .on_hover_text(format!(
            "{exact}\n\nZum Beispiel 18.09.2026, 18.09.2026 06:30, \
             18.09.2026 06:30:45,123\noder eine pk_TimeStamp-Zahl"
        ))
        .changed();
    if !text.is_empty() && theme::cross(ui).on_hover_text("Leeren").clicked() {
        text.clear();
        edited = true;
    }
    edited
}

// ---------------------------------------------------------------- helpers

/// Ask for the two databases.
fn pick_databases() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .add_filter("HMI-Datenbanken", &["db3", "db", "sqlite", "sqlite3"])
        .add_filter("Alle Dateien", &["*"])
        .set_title("HMI-Datenbanken auswählen")
        .pick_files()
}

/// Ask for a whole folder, which is then searched for databases.
fn pick_folder() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Ordner mit HMI-Datenbanken auswählen")
        .pick_folder()
        .map(|dir| vec![dir])
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// What every one of these strings begins with, which is how the name for the
/// output is guessed from a row of numbered files.
fn common_prefix(items: &[String]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut end = first.len();
    for other in &items[1..] {
        end = end.min(
            first
                .char_indices()
                .zip(other.char_indices())
                .take_while(|((_, a), (_, b))| a == b)
                .map(|((i, c), _)| i + c.len_utf8())
                .last()
                .unwrap_or(0),
        );
    }
    first[..end].to_string()
}

fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hmi".into())
}

/// 602912 -> "602 912"
pub fn sep(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn human_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{} MB", decimal(b / (1024.0 * 1024.0), 1))
    } else if b >= 1024.0 {
        format!("{b:.0} kB", b = b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

/// A folder as it should be read: whole, absolute, with backslashes on Windows.
fn shown_path(p: &Path) -> String {
    #[cfg(target_os = "windows")]
    return windows_path(p).display().to_string();
    #[cfg(not(target_os = "windows"))]
    return std::path::absolute(p)
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string();
}

/// A number with a decimal comma, as it is written in German: `4,0`.
fn decimal(v: f64, places: usize) -> String {
    format!("{v:.places$}").replace('.', ",")
}

fn human_size(p: &Path) -> String {
    let Ok(md) = std::fs::metadata(p) else {
        return String::new();
    };
    human_bytes(md.len())
}

/// Show a file in the system file manager, selected.
fn reveal(path: &Path) {
    // Explorer reads its own command line: `/select,` must be followed by the
    // path in quotes, and the quotes must not wrap the switch as well. Quoting
    // the whole argument the usual way makes it give up on any path with a
    // space in it -- every `OneDrive - Firma` folder -- and open Documents.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", windows_path(path).display()))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

/// Open a folder itself.
fn open_folder(dir: &Path) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("explorer")
            .raw_arg(format!("\"{}\"", windows_path(dir).display()))
            .spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
}

/// A path Explorer understands: absolute, with backslashes only. Explorer
/// takes a relative or mixed-slash path for something it does not know and
/// falls back to the Documents folder.
#[cfg(target_os = "windows")]
pub fn windows_path(path: &Path) -> PathBuf {
    let full = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    PathBuf::from(full.to_string_lossy().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separator() {
        assert_eq!(sep(0), "0");
        assert_eq!(sep(999), "999");
        assert_eq!(sep(1_000), "1 000");
        assert_eq!(sep(602_912), "602 912");
        assert_eq!(sep(1_234_567), "1 234 567");
    }

    #[test]
    fn numbers_use_a_decimal_comma() {
        assert_eq!(decimal(4.04, 1), "4,0");
        assert_eq!(human_bytes(36 * 1024 * 1024), "36,0 MB");
    }

    #[test]
    fn durations_read_in_german() {
        let s = 10_000_000;
        assert_eq!(duration_text(38 * s), "38 s");
        assert_eq!(duration_text((3 * 3600 + 12 * 60) * s), "3 h 12 min");
        assert_eq!(duration_text((86_400 + 9 * 3600) * s), "1 Tag 9 h");
        assert_eq!(duration_text((2 * 86_400 + 3600) * s), "2 Tage 1 h");
    }
}
