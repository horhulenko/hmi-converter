#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! HMI Log Converter -- turns a pair of WinCC logging databases (tags + values)
//! into one flat CSV table. Drop the two .db3 files in any order; the program
//! works out by itself which one is which.

mod chart;
mod convert;
mod plotview;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use chart::{ChartJob, ChartSummary, Detail, Loaded};
use convert::{Column, Job, Kept, OutColumn, Probe, Progress, Range, Summary};
use eframe::egui;
use plotview::PlotView;
use theme::{ACCENT, BAD, DIM, MUTED, OK, TEXT, WARN, body, dot, lead, muted, strong};

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
        .with_inner_size([880.0, 690.0])
        .with_min_inner_size([560.0, 520.0])
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
        range,
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
        range: job.range,
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
    from_text: String,
    until_text: String,
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
}

/// The range and the files a set of counts belongs to.
type PreviewKey = (String, String, Vec<PathBuf>);

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
            from_text: String::new(),
            until_text: String::new(),
            last,
            preview_key: None,
            pending: None,
            pending_since: std::time::Instant::now(),
            counter: channel(),
            counter_gen: 0,
        };
        if !preload.is_empty() {
            app.accept(preload);
        }
        app
    }

    /// The chosen window, plus a message for whatever does not parse.
    fn range(&self) -> Result<Range, String> {
        let from = if self.from_text.trim().is_empty() {
            None
        } else {
            Some(convert::parse_when(&self.from_text, false).map_err(|e| format!("From: {e}"))?)
        };
        let until = if self.until_text.trim().is_empty() {
            None
        } else {
            Some(convert::parse_when(&self.until_text, true).map_err(|e| format!("Until: {e}"))?)
        };
        if let (Some(a), Some(b)) = (from, until)
            && a > b
        {
            return Err("From is later than Until".into());
        }
        Ok(Range { from, until })
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
    /// common -- an HMI numbers its files, so `..._Werte_20260302_053000` and
    /// `..._Werte_20260303_090000` suggest `Anlage1_Werte`. Failing that,
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
            && self.range().is_ok()
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
        let key: PreviewKey = (
            self.from_text.trim().to_string(),
            self.until_text.trim().to_string(),
            files.clone(),
        );

        // Something changed: show it at once, then let the timer run down.
        if self.preview_key.as_ref() != Some(&key) && self.pending.as_ref() != Some(&key) {
            self.pending = Some(key.clone());
            self.pending_since = std::time::Instant::now();
            let open = self.range().map(|r| r.is_open()).unwrap_or(false);
            let bad = self.range().err();
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
        let range = match self.range() {
            Ok(r) if !r.is_open() => r,
            // Nothing to count: without a range every file is taken whole, and
            // a range that does not parse is already reported.
            _ => return,
        };

        self.counter_gen += 1;
        let seq = self.counter_gen;
        let tx = self.counter.0.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            for path in files {
                let kept = convert::count_kept(&path, range);
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
            self.note("Busy converting -- wait until it is finished.", WARN);
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
                    let _ = tx.send(ScanMsg::Skipped(path, "already in the list".into()));
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
            if why != "already in the list" {
                self.note(format!("{} -- skipped: {why}", file_name(&path)), MUTED);
            }
        }
        if finished {
            let n = self.sources.len();
            self.scan = None;
            if n == 0 {
                self.note("No HMI database in there.", BAD);
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
        let range = match self.range() {
            Ok(r) => r,
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
            range,
            shorten_names: self.shorten_names,
            columns: self.columns(),
        };

        // Remember what was chosen, so the next run starts where this left off.
        let (from, until) = (
            self.from_text.trim().to_string(),
            self.until_text.trim().to_string(),
        );
        if !(from.is_empty() && until.is_empty()) {
            self.last.from = from;
            self.last.until = until;
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
            stage: "Starting".into(),
            done: 0,
            total: 0,
        };
    }

    /// What to read, for either way of charting.
    fn chart_job(&mut self) -> Option<ChartJob> {
        if self.value_dbs().is_empty() {
            return None;
        }
        let range = match self.range() {
            Ok(r) => r,
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
            range,
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
            stage: "Starting".into(),
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
            stage: "Starting".into(),
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

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            if matches!(self.state, State::Setup) {
                egui::Panel::bottom("action")
                    .show_separator_line(false)
                    .frame(egui::Frame::new().inner_margin(egui::Margin {
                        top: theme::GAP as i8,
                        ..egui::Margin::ZERO
                    }))
                    .show(ui, |ui| self.convert_button(ui));
            }
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match &self.state {
                    State::Running { .. } => self.running_view(ui),
                    State::Done(_) => self.done_view(ui),
                    State::Charted(_) => self.chart_view(ui),
                    State::Plotting(_) => self.plot_view(ui),
                    State::Failed(_) => self.failed_view(ui),
                    State::Setup => self.setup_view(ui, hovering),
                });
        });

        if matches!(self.state, State::Setup) {
            self.columns_window(&ctx);
        }

        if matches!(self.state, State::Running { .. }) {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

impl App {
    // -- choosing files and options ---------------------------------------
    fn setup_view(&mut self, ui: &mut egui::Ui, hovering: bool) {
        // What comes in on top, what goes out below, so one screen holds the
        // whole job however many files there are.
        self.drop_zone(ui, hovering);
        ui.add_space(theme::GAP);
        self.sources_view(ui);
        ui.add_space(theme::GAP);
        ui.columns(2, |c| {
            self.range_view(&mut c[0]);
            self.options_view(&mut c[1]);
        });
        ui.add_space(theme::GAP);
        self.output_view(ui);
        self.notes_view(ui);
    }

    /// Where files are dropped. Big and inviting while the table is empty,
    /// one line once it has something in it.
    fn drop_zone(&mut self, ui: &mut egui::Ui, hovering: bool) {
        let empty = self.sources.is_empty();
        let mut picked: Option<Vec<PathBuf>> = None;
        theme::drop_card(hovering).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if empty || hovering {
                ui.vertical_centered(|ui| {
                    ui.add_space(4.0);
                    ui.label(lead(if hovering {
                        "Release to look through them"
                    } else {
                        "Drag the .db3 files here, or a whole folder"
                    }));
                    ui.label(muted(
                        "any number, any order, any file name -- every file is \
                         opened to see whether it is an HMI database",
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        // Centred as a pair, so the two buttons read as one choice.
                        let width = 300.0_f32.min(ui.available_width());
                        ui.add_space((ui.available_width() - width).max(0.0) / 2.0);
                        if ui.button("Choose files...").clicked() {
                            picked = pick_databases();
                        }
                        if ui.button("Choose a folder...").clicked() {
                            picked = pick_folder();
                        }
                    });
                    ui.add_space(2.0);
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label(muted("Drop more files or folders here to add them"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Choose a folder...").clicked() {
                            picked = pick_folder();
                        }
                        if ui.small_button("Choose files...").clicked() {
                            picked = pick_databases();
                        }
                    });
                });
            }
        });
        if let Some(files) = picked {
            self.accept(files);
        }
    }

    // -- the preview table -------------------------------------------------

    /// Every database that was found, what is inside it, and a tick to leave it
    /// out of the import.
    fn sources_view(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(theme::ROW_TOP);
            self.table_head(ui);
            if self.sources.is_empty() {
                ui.add_space(theme::GAP);
                ui.label(muted(match &self.scan {
                    Some(_) => "Looking...",
                    None => "Nothing loaded yet -- drop the files above.",
                }));
                return;
            }
            ui.add_space(8.0);
            let cols = Cols::new(ui.available_width());
            column_titles(ui, &cols);
            self.table_rows(ui, &cols);
            ui.add_space(6.0);
            self.table_foot(ui);
        });
    }

    /// The title line: how much is being imported, and the ticking buttons.
    fn table_head(&mut self, ui: &mut egui::Ui) {
        let on = self.included().count();
        let all = self.sources.len();
        let scanning = self.scan.is_some();
        let (looked, found) = self
            .scan
            .as_ref()
            .map(|s| (s.looked, s.total))
            .unwrap_or_default();

        ui.horizontal(|ui| {
            ui.label(strong("Files to import"));
            if all > 0 {
                ui.label(muted(format!("{on} of {all} included")));
            }
            if scanning {
                ui.spinner();
                ui.label(muted(if found > 0 {
                    format!("looking through {looked} of {found} files...")
                } else {
                    "looking...".into()
                }));
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if all > 0 {
                    if ui
                        .small_button("Remove all")
                        .on_hover_text("Empty the list and start again")
                        .clicked()
                    {
                        self.sources.clear();
                        self.filter.clear();
                        self.notes.clear();
                        self.preview_key = None;
                    }
                    if ui
                        .small_button("Invert")
                        .on_hover_text("Swap what is in for what is out")
                        .clicked()
                    {
                        for s in &mut self.sources {
                            s.on = !s.on;
                        }
                        self.preview_key = None;
                    }
                    if ui.small_button("None").clicked() {
                        self.set_all(false);
                    }
                    if ui.small_button("All").clicked() {
                        self.set_all(true);
                    }
                }
                // A filter earns its place only once the list is long.
                if all > 8 {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("filter by name")
                            .desired_width(130.0),
                    );
                }
            });
        });
    }

    fn set_all(&mut self, on: bool) {
        for s in &mut self.sources {
            s.on = on;
        }
        self.preview_key = None;
    }

    /// One row per file, and whatever was clicked on them.
    fn table_rows(&mut self, ui: &mut egui::Ui, cols: &Cols) {
        let needle = self.filter.trim().to_lowercase();
        let shown: Vec<usize> = (0..self.sources.len())
            .filter(|&i| {
                needle.is_empty()
                    || file_name(self.sources[i].path())
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect();

        let height = (shown.len().max(1) as f32 * (ROW_H + 3.0)).min(TABLE_MAX_H);
        let mut act: Option<RowAct> = None;
        egui::ScrollArea::vertical()
            .id_salt("sources")
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if shown.is_empty() {
                    ui.label(muted("No file matches the filter."));
                }
                for (nth, &i) in shown.iter().enumerate() {
                    if let Some(a) = source_row(ui, &mut self.sources[i], cols, i, nth % 2 == 1) {
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

    /// The line under the table: what the import adds up to, and anything
    /// about it worth a warning.
    fn table_foot(&mut self, ui: &mut egui::Ui) {
        let values = self.value_dbs().len();
        let tag_dbs = self.tag_dbs().len();
        let counting = self.included().any(|s| matches!(s.kept, Count::Counting));

        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if values == 0 {
                ui.label(body("Nothing to import -- tick a file with logged values.").color(WARN));
                return;
            }
            match self.total_values() {
                Some(0) => {
                    ui.label(body("The time range leaves nothing in these files.").color(BAD));
                }
                Some(n) => {
                    ui.label(strong(format!("{} values", sep(n))).color(OK));
                }
                None => {
                    if counting {
                        ui.spinner();
                    }
                    ui.label(muted("counting..."));
                }
            }
            ui.label(muted(format!(
                "from {values} {}",
                if values == 1 { "database" } else { "databases" }
            )));
            if let Some((a, b)) = self.total_span() {
                ui.label(muted(format!(
                    "{}  to  {}",
                    convert::filetime_to_short(a),
                    convert::filetime_to_short(b)
                )));
            }
            if tag_dbs > 0 {
                ui.label(muted(format!("{} tag names", sep(self.total_tags() as u64))));
            }
        });

        if values > 0 && tag_dbs == 0 {
            ui.label(
                body("No tag database included -- tags will be written as id_<number>.")
                    .color(WARN),
            );
        }
        let overlaps = self.overlaps();
        if let Some((a, b)) = overlaps.first() {
            ui.label(
                body(format!(
                    "{a} and {b} cover the same period{} -- those readings would \
                     be written twice.",
                    if overlaps.len() > 1 {
                        format!(", and {} other pair(s) do", overlaps.len() - 1)
                    } else {
                        String::new()
                    }
                ))
                .color(WARN),
            );
        }
    }

    /// Cut the log down to a time slice, with a live count of what survives.
    fn range_view(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(theme::ROW_BOTTOM);
            section(ui, "Time range", "optional");
            ui.add_space(theme::GAP);

            let last_from = self.last.from.clone();
            let last_until = self.last.until.clone();
            range_field(ui, "From", &mut self.from_text, &last_from, false);
            ui.add_space(4.0);
            range_field(ui, "Until", &mut self.until_text, &last_until, true);

            ui.add_space(theme::GAP);
            let range = self.range();
            if let Err(e) = &range {
                ui.label(body(e.clone()).color(BAD));
                return;
            }
            if self.value_dbs().is_empty() {
                ui.label(muted("Tick a file with logged values to see what is kept."));
                return;
            }
            if range.map(|r| r.is_open()).unwrap_or(false) {
                ui.label(muted(
                    "Everything in the ticked files is converted. Narrow this to \
                     cut the log down to one day or one shift.",
                ));
                return;
            }

            let total: u64 = self
                .included()
                .filter(|s| s.probe.has_values)
                .map(|s| s.probe.value_rows)
                .sum();
            match self.total_values() {
                Some(kept) => {
                    ui.label(
                        strong(if kept == 0 {
                            "Nothing is left in this range.".to_string()
                        } else {
                            format!("Keeps {} of {} values", sep(kept), sep(total))
                        })
                        .color(if kept == 0 { BAD } else { OK }),
                    );
                    if let Some((a, b)) = self.total_span() {
                        ui.label(muted(format!(
                            "{}  to  {}",
                            convert::filetime_to_short(a),
                            convert::filetime_to_short(b)
                        )));
                    }
                    let empty = self
                        .included()
                        .filter(|s| s.probe.has_values)
                        .filter(|s| s.values() == Some(0))
                        .count();
                    if empty > 0 && kept > 0 {
                        ui.label(muted(format!(
                            "{empty} of the ticked files have nothing in this range"
                        )));
                    }
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(muted("counting..."));
                    });
                }
            }
        });
    }

    /// Where the CSV files go and what they are called.
    fn output_view(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(theme::ROW_BOTTOM);
            section(ui, "Save to", "");
            ui.add_space(theme::GAP);

            ui.horizontal(|ui| {
                ui.label(body("Name"));
                let mut name = self.base_name();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut name)
                            .desired_width(150.0)
                            .hint_text("file name"),
                    )
                    .changed()
                {
                    self.base_name = Some(name);
                }
                if ui
                    .button("Browse...")
                    .on_hover_text("Pick the folder and the name in Explorer")
                    .clicked()
                {
                    self.browse_output();
                }
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(muted("Folder"));
                match self.chosen_dir() {
                    Some(dir) => {
                        if ui
                            .link(muted(elide(&dir.display().to_string(), 38)))
                            .on_hover_text(format!(
                                "{}\n\nClick to open in Explorer",
                                dir.display()
                            ))
                            .clicked()
                        {
                            open_folder(&dir);
                        }
                    }
                    None => {
                        ui.label(muted("beside the value database, once it is loaded"));
                    }
                }
            });
            for f in self.output_files() {
                ui.label(muted(format!("writes  {f}")));
            }
            if self.output_files().is_empty() {
                ui.label(body("Pick at least one table below.").color(WARN));
            }
        });
    }

    /// What the CSV files are to contain.
    fn options_view(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(theme::ROW_BOTTOM);
            section(ui, "What to write", "");
            ui.add_space(theme::GAP);
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.write_long, "Long table -- a row per value");
                if self.write_long {
                    let chosen = self.columns().len();
                    if ui
                        .small_button("Columns...")
                        .on_hover_text("Choose which columns to write, and in what order")
                        .clicked()
                    {
                        self.columns_open = true;
                    }
                    if chosen != Column::ALL.len() {
                        ui.label(muted(format!("{chosen} of {}", Column::ALL.len())));
                    }
                }
            });
            ui.checkbox(&mut self.write_wide, "Wide table -- a column per tag");
            ui.checkbox(&mut self.semicolon, "Separate with ;  (for Excel)");
            ui.checkbox(&mut self.shorten_names, "Shorten tag names");

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(muted("Chart detail"));
                for d in [Detail::Light, Detail::Balanced, Detail::Full] {
                    let picked = self.detail == d;
                    if ui
                        .selectable_label(picked, body(d.label()))
                        .on_hover_text(d.describe())
                        .clicked()
                    {
                        self.detail = d;
                    }
                }
            });

            if self.shorten_names {
                ui.add_space(4.0);
                match self.shorten_example() {
                    Some((full, short)) => {
                        ui.label(muted(full));
                        ui.label(muted(format!("becomes   {short}")).color(OK));
                    }
                    None => {
                        ui.label(muted("Load the tag database to see an example."));
                    }
                }
            }
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
        let mut open = true;

        egui::Window::new("Long table columns")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(660.0)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .frame(theme::card().inner_margin(egui::Margin::same(16)))
            .show(ctx, |ui| {
                ui.set_width(660.0);
                ui.label(muted(
                    "Tick what to write, drag a row by its handle to reorder, \
                     and type any heading you like.",
                ));
                ui.add_space(theme::GAP);

                column_rows(ui, &mut rows, &mut drag);

                ui.add_space(theme::GAP);
                ui.separator();
                ui.add_space(theme::GAP);

                ui.label(muted("The first line of the file will read"));
                let chosen: Vec<OutColumn> =
                    rows.iter().filter(|r| r.on).map(ColumnRow::out).collect();
                if chosen.is_empty() {
                    ui.label(body("nothing -- tick at least one column").color(BAD));
                } else {
                    let sep = if semicolon { ";" } else { "," };
                    let headers: Vec<&str> = chosen.iter().map(|c| c.header.as_str()).collect();
                    ui.label(body(headers.join(sep)));
                    let mut seen = headers.clone();
                    seen.sort_unstable();
                    seen.dedup();
                    if seen.len() != headers.len() {
                        ui.label(
                            body("Two columns share a heading -- most tools dislike that.")
                                .color(WARN),
                        );
                    }
                }

                ui.add_space(theme::GAP);
                ui.horizontal(|ui| {
                    if ui
                        .add(
                            egui::Button::new(theme::on_accent("Done", true))
                                .fill(theme::ACCENT_FILL)
                                .min_size(egui::vec2(90.0, 30.0)),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                    if ui
                        .button("Reset")
                        .on_hover_text("All six columns, original headings, original order")
                        .clicked()
                    {
                        reset = true;
                    }
                });
            });

        self.long_columns = if reset { ColumnRow::all() } else { rows };
        self.drag = drag;
        if close || !open {
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
            .set_title("Choose where to save and how to name the files")
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

    fn convert_button(&mut self, ui: &mut egui::Ui) {
        let ready = self.ready();
        let can_chart = !self.value_dbs().is_empty() && self.range().is_ok();
        let width = ui.available_width();
        let chart_width = 150.0_f32.min(width * 0.22);
        // The button says how much it is about to convert, so there is no need
        // to read the table again before pressing it.
        let files = self.value_dbs().len();
        let caption = match (ready, self.total_values()) {
            (true, Some(n)) if files > 1 => {
                format!("Convert {} values from {files} files", sep(n))
            }
            (true, Some(n)) => format!("Convert {} values to CSV", sep(n)),
            _ => "Convert to CSV".to_string(),
        };

        ui.horizontal(|ui| {
            let label = theme::on_accent(caption, ready);
            let button = egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow()))
                .min_size(egui::vec2(width - chart_width * 2.0 - 16.0, 42.0))
                .fill(theme::button_fill(ready));
            if ui.add_enabled(ready, button).clicked() {
                self.start();
            }

            let label = |text: &'static str| {
                if can_chart {
                    body(text)
                } else {
                    body(text).color(DIM)
                }
            };

            let here =
                egui::Button::new((egui::Atom::grow(), label("Chart here"), egui::Atom::grow()))
                    .min_size(egui::vec2(chart_width, 42.0));
            if ui
                .add_enabled(can_chart, here)
                .on_hover_text(
                    "Draw the values in this window: pan, zoom and switch tags \
                     with no waiting.",
                )
                .clicked()
            {
                self.start_plot();
            }

            let browser =
                egui::Button::new((egui::Atom::grow(), label("In browser"), egui::Atom::grow()))
                    .min_size(egui::vec2(chart_width, 42.0));
            if ui
                .add_enabled(can_chart, browser)
                .on_hover_text(
                    "Write one self-contained page and open it: eleven kinds of \
                     chart, nothing downloaded.",
                )
                .clicked()
            {
                self.start_chart();
            }
        });

        if !ready {
            let hint = if self.sources.is_empty() {
                "Step 1 -- drop the .db3 files or their folder above.    \
                 Step 2 -- press Convert."
            } else if self.value_dbs().is_empty() {
                "Tick at least one file that holds logged values."
            } else if !self.write_long && !self.write_wide {
                "Pick at least one table to write."
            } else if self.write_long && self.columns().is_empty() {
                "The long table has no columns left -- press Columns..."
            } else if self.total_values() == Some(0) {
                "The time range leaves nothing to convert."
            } else {
                "Check the time range above."
            };
            ui.add_space(6.0);
            ui.vertical_centered(|ui| ui.label(muted(hint)));
        }
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

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(lead(format!("{stage}...")));
            ui.add_space(theme::GAP);

            let fraction = if total > 0 {
                (done as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .text(theme::on_accent(
                        if total > 0 {
                            format!("{}  /  {} rows", sep(done), sep(total))
                        } else {
                            "preparing".into()
                        },
                        true,
                    ))
                    .fill(theme::ACCENT_FILL)
                    .animate(total == 0)
                    .desired_height(24.0),
            );
        });

        ui.add_space(theme::GAP);
        if ui
            .add(egui::Button::new("Cancel").min_size(egui::vec2(140.0, 34.0)))
            .clicked()
        {
            cancel.store(true, Ordering::Relaxed);
        }
        ui.add_space(6.0);
        ui.label(muted("The source databases are only read, never changed."));
    }

    // -- the result --------------------------------------------------------
    fn done_view(&mut self, ui: &mut egui::Ui) {
        let State::Done(s) = &self.state else { return };
        let s = s.clone();

        ui.label(theme::heading("Finished").color(OK));
        ui.label(muted(format!(
            "{} values from {} tags in {:.1} s",
            sep(s.rows),
            s.tags_used,
            s.secs
        )));
        ui.add_space(theme::GAP);

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (i, (path, rows)) in s.files.iter().enumerate() {
                if i > 0 {
                    ui.add_space(theme::GAP);
                }
                ui.horizontal(|ui| {
                    dot(ui, OK);
                    ui.vertical(|ui| {
                        if ui
                            .link(strong(file_name(path)))
                            .on_hover_text(format!(
                                "{}\n\nClick to show in Explorer",
                                path.display()
                            ))
                            .clicked()
                        {
                            reveal(path);
                        }
                        ui.label(muted(format!(
                            "{} rows    {}",
                            sep(*rows),
                            human_size(path)
                        )));
                    });
                });
            }
        });

        ui.add_space(theme::GAP);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            section(ui, "Details", "");
            ui.add_space(theme::GAP);
            if let Some((a, b)) = s.span {
                kv(
                    ui,
                    "Time range",
                    format!(
                        "{}  to  {}",
                        convert::filetime_to_short(a),
                        convert::filetime_to_short(b)
                    ),
                );
            }
            kv(
                ui,
                "Tags",
                format!(
                    "{} of {} configured tags logged values",
                    s.tags_used, s.tags_total
                ),
            );
            kv(ui, "Timestamps", sep(s.timestamps));
            if s.bad_quality > 0 {
                ui.add_space(4.0);
                ui.label(
                    body(format!(
                        "{} rows have a Quality other than 192 -- kept, see the Quality column",
                        sep(s.bad_quality)
                    ))
                    .color(WARN),
                );
            }
            if !s.unknown_ids.is_empty() {
                ui.label(
                    body(format!(
                        "{} tag ids had no name and were written as id_<number>",
                        s.unknown_ids.len()
                    ))
                    .color(WARN),
                );
            }
        });

        ui.add_space(theme::GAP);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(theme::on_accent("Open folder", true))
                        .min_size(egui::vec2(150.0, 36.0))
                        .fill(theme::ACCENT_FILL),
                )
                .clicked()
                && let Some((p, _)) = s.files.first()
            {
                reveal(p);
            }
            if ui
                .add(egui::Button::new("Chart here").min_size(egui::vec2(130.0, 36.0)))
                .clicked()
            {
                self.start_plot();
            }
            if ui
                .add(egui::Button::new("In browser").min_size(egui::vec2(130.0, 36.0)))
                .clicked()
            {
                self.start_chart();
            }
            if ui
                .add(egui::Button::new("Convert more").min_size(egui::vec2(150.0, 36.0)))
                .clicked()
            {
                self.state = State::Setup;
                self.notes.clear();
            }
        });
    }

    /// The chart inside this window, with a bar to leave it by.
    fn plot_view(&mut self, ui: &mut egui::Ui) {
        let mut leave = false;
        let mut to_browser = false;
        if let State::Plotting(view) = &mut self.state {
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new("Back").min_size(egui::vec2(90.0, 28.0)))
                    .clicked()
                {
                    leave = true;
                }
                ui.label(theme::lead("Charts"));
                ui.label(muted(view.caption()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Open in browser")
                        .on_hover_text("The same values as a page, with more kinds of chart")
                        .clicked()
                    {
                        to_browser = true;
                    }
                });
            });
            ui.add_space(6.0);
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

        ui.label(theme::heading("Charts ready").color(OK));
        ui.label(muted(format!(
            "opened in your browser -- {tags} tags, {} points drawn of {} in {secs:.1} s",
            sep(drawn),
            sep(read)
        )));
        ui.add_space(theme::GAP);

        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                dot(ui, OK);
                ui.vertical(|ui| {
                    if ui
                        .link(strong(file_name(&path)))
                        .on_hover_text(format!("{}\n\nClick to show in Explorer", path.display()))
                        .clicked()
                    {
                        reveal(&path);
                    }
                    ui.label(muted(format!(
                        "{}    one file, nothing fetched from the internet",
                        human_bytes(bytes)
                    )));
                });
            });
            if skipped > 0 {
                ui.add_space(6.0);
                ui.label(
                    body(format!(
                        "{skipped} tags log text rather than numbers and are not drawn"
                    ))
                    .color(WARN),
                );
            }
        });

        ui.add_space(theme::GAP);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(theme::on_accent("Open again", true))
                        .min_size(egui::vec2(150.0, 36.0))
                        .fill(theme::ACCENT_FILL),
                )
                .clicked()
            {
                chart::open(&path);
            }
            if ui
                .add(egui::Button::new("Back").min_size(egui::vec2(150.0, 36.0)))
                .clicked()
            {
                self.state = State::Setup;
                self.notes.clear();
            }
        });
    }

    fn failed_view(&mut self, ui: &mut egui::Ui) {
        let State::Failed(msg) = &self.state else {
            return;
        };
        let msg = msg.clone();
        let cancelled = msg == "cancelled";

        ui.label(
            theme::heading(if cancelled {
                "Cancelled"
            } else {
                "It did not work"
            })
            .color(if cancelled { WARN } else { BAD }),
        );
        ui.add_space(theme::GAP);
        if cancelled {
            ui.label(body(
                "Nothing was finished. Any half-written CSV file can be deleted.",
            ));
        } else {
            theme::card().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(body(msg).color(BAD));
            });
            ui.add_space(theme::GAP);
            ui.label(muted(
                "Check that both files come from the same HMI export and that the \
                 output folder can be written to.",
            ));
        }
        ui.add_space(theme::GAP);
        if ui
            .add(
                egui::Button::new(theme::on_accent("Back", true))
                    .min_size(egui::vec2(150.0, 36.0))
                    .fill(theme::ACCENT_FILL),
            )
            .clicked()
        {
            self.state = State::Setup;
        }
    }

    fn notes_view(&mut self, ui: &mut egui::Ui) {
        if self.notes.is_empty() {
            return;
        }
        ui.add_space(theme::GAP);
        egui::ScrollArea::vertical()
            .id_salt("notes")
            .max_height(96.0)
            .show(ui, |ui| {
                for (text, color) in &self.notes {
                    ui.label(muted(text).color(*color));
                }
            });
    }
}

// ---------------------------------------------------------------- file table

/// Height of one row of the file table.
const FILE_ROW: f32 = 28.0;
/// The table starts scrolling at this height, so the cards below it stay put
/// whether three files were dropped or three hundred.
const TABLE_MAX_H: f32 = 236.0;

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
/// extra columns give way to it rather than squeezing it out.
struct Cols {
    tick: f32,
    role: f32,
    name: f32,
    rows: f32,
    span: f32,
    size: f32,
    drop: f32,
}

impl Cols {
    const GAP: f32 = 8.0;

    fn new(total: f32) -> Self {
        let (tick, role, rows, drop) = (22.0, 88.0, 104.0, 22.0);
        let mut span = 178.0;
        let mut size = 58.0;
        let left = |span: f32, size: f32| {
            let shown = 4.0
                + if span > 0.0 { 1.0 } else { 0.0 }
                + if size > 0.0 { 1.0 } else { 0.0 };
            total - (tick + role + rows + drop + span + size) - Self::GAP * shown
        };
        if left(span, size) < 150.0 {
            size = 0.0;
        }
        if left(span, size) < 150.0 {
            span = 0.0;
        }
        Self {
            tick,
            role,
            name: left(span, size).max(70.0),
            rows,
            span,
            size,
            drop,
        }
    }
}

/// The headings above the rows.
fn column_titles(ui: &mut egui::Ui, cols: &Cols) {
    ui.horizontal(|ui| {
        ui.set_height(18.0);
        ui.spacing_mut().item_spacing.x = Cols::GAP;
        cell(ui, cols.tick, egui::Align::Center, |_| {});
        cell(ui, cols.role, egui::Align::Min, |ui| {
            ui.label(muted("Holds"));
        });
        cell(ui, cols.name, egui::Align::Min, |ui| {
            ui.label(muted("File"));
        });
        cell(ui, cols.rows, egui::Align::Max, |ui| {
            ui.label(muted("Values"));
        });
        cell(ui, cols.span, egui::Align::Min, |ui| {
            ui.label(muted("Logged from"));
        });
        cell(ui, cols.size, egui::Align::Max, |ui| {
            ui.label(muted("Size"));
        });
        cell(ui, cols.drop, egui::Align::Center, |_| {});
    });
    ui.separator();
}

/// One file: the tick, what it holds, its name, and how much of it is taken.
fn source_row(
    ui: &mut egui::Ui,
    src: &mut Source,
    cols: &Cols,
    index: usize,
    shaded: bool,
) -> Option<RowAct> {
    let mut act: Option<RowAct> = None;
    let full_path = src.path().display().to_string();

    let out = ui.scope_builder(
        egui::UiBuilder::new()
            .id_salt(("file-row", index))
            .sense(egui::Sense::click()),
        |ui| {
            // Kept for later, so the background can be painted under the row
            // once it is known whether the pointer is on it.
            let bg = ui.painter().add(egui::Shape::Noop);
            ui.horizontal(|ui| {
                ui.set_height(FILE_ROW);
                ui.spacing_mut().item_spacing.x = Cols::GAP;
                let shift = ui.input(|i| i.modifiers.shift);

                cell(ui, cols.tick, egui::Align::Center, |ui| {
                    let mut on = src.on;
                    if ui
                        .add(egui::Checkbox::without_text(&mut on))
                        .on_hover_text("Include this file in the import")
                        .clicked()
                    {
                        act = Some(RowAct::Toggle(index, shift));
                    }
                });

                cell(ui, cols.role, egui::Align::Min, |ui| {
                    let (text, color) = match (src.probe.has_tags, src.probe.has_values) {
                        (true, true) => ("tags + values", ACCENT),
                        (true, false) => ("tag names", WARN),
                        _ => ("values", OK),
                    };
                    theme::chip(ui, text, if src.on { color } else { DIM });
                });

                cell(ui, cols.name, egui::Align::Min, |ui| {
                    let name = file_name(src.path());
                    let text = if src.on {
                        strong(name)
                    } else {
                        body(name).color(DIM)
                    };
                    if ui
                        .add(
                            egui::Label::new(text)
                                .truncate()
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text(format!("{full_path}\n\nClick to show in Explorer"))
                        .clicked()
                    {
                        act = Some(RowAct::Reveal(index));
                    }
                });

                cell(ui, cols.rows, egui::Align::Max, |ui| {
                    row_amount(ui, src);
                });

                cell(ui, cols.span, egui::Align::Min, |ui| match src.span() {
                    Some((a, b)) => {
                        let text = format!(
                            "{}   +{}",
                            convert::filetime_to_short(a),
                            duration_text(b - a)
                        );
                        ui.add(
                            egui::Label::new(if src.on { muted(text) } else { muted(text).color(DIM) })
                                .truncate(),
                        )
                        .on_hover_text(format!(
                            "{}\nto {}",
                            convert::filetime_to_text(a),
                            convert::filetime_to_text(b)
                        ));
                    }
                    None => {
                        ui.label(muted("-"));
                    }
                });

                cell(ui, cols.size, egui::Align::Max, |ui| {
                    ui.label(muted(human_bytes(src.probe.bytes)));
                });

                cell(ui, cols.drop, egui::Align::Center, |ui| {
                    if cross(ui)
                        .on_hover_text("Take this file off the list")
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
        response.clone().on_hover_text(if src.on {
            "Click to leave this file out    (right-click: this file only)"
        } else {
            "Click to include this file    (right-click: this file only)"
        });
    }

    let fill = if hovered {
        theme::ACCENT_FILL.gamma_multiply(0.22)
    } else if !src.on {
        egui::Color32::TRANSPARENT
    } else if shaded {
        theme::FIELD.gamma_multiply(0.45)
    } else {
        egui::Color32::TRANSPARENT
    };
    ui.painter().set(
        out.inner,
        egui::epaint::RectShape::filled(
            response.rect.expand2(egui::vec2(2.0, 0.0)),
            egui::CornerRadius::same(4),
            fill,
        ),
    );
    act
}

/// How much this file contributes, and what it was out of.
fn row_amount(ui: &mut egui::Ui, src: &Source) {
    if !src.probe.has_values {
        ui.label(muted(format!("{} tags", sep(src.probe.tag_count))));
        return;
    }
    match &src.kept {
        Count::Counting => {
            ui.spinner();
        }
        Count::Bad(e) => {
            ui.label(muted("--").color(BAD)).on_hover_text(e.clone());
        }
        Count::Whole => {
            let text = sep(src.probe.value_rows);
            ui.label(if src.on { body(text) } else { body(text).color(DIM) })
                .on_hover_text("Every value in the file, because no time range is set");
        }
        Count::Kept(k) => {
            let text = sep(k.rows);
            let color = if !src.on {
                DIM
            } else if k.rows == 0 {
                BAD
            } else if k.rows < k.total_rows {
                WARN
            } else {
                TEXT
            };
            ui.label(body(text).color(color)).on_hover_text(format!(
                "{} of the {} values in this file are inside the time range",
                sep(k.rows),
                sep(k.total_rows)
            ));
        }
    }
}

/// A little x, drawn rather than written, to take a row off the list.
fn cross(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
    let color = if response.hovered() { BAD } else { DIM };
    let r = 4.0;
    let c = rect.center();
    let stroke = egui::Stroke::new(1.6, color);
    ui.painter().line_segment([c - egui::vec2(r, r), c + egui::vec2(r, r)], stroke);
    ui.painter().line_segment([c + egui::vec2(r, -r), c + egui::vec2(-r, r)], stroke);
    response
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
        add(ui);
    });
}

/// `9 h 7 m`, `1 d 9 h`, `38 s` -- how long a file covers, in two parts at most.
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
        (0, 0, _) => format!("{m} m"),
        (0, _, _) => format!("{h} h {m} m"),
        _ => format!("{d} d {h} h"),
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
const ROW_H: f32 = 46.0;
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
            egui::vec2(area.width(), ROW_H - 4.0),
        );

        if held {
            ui.painter().rect_filled(
                rect.expand(2.0),
                egui::CornerRadius::same(6),
                theme::ACCENT_FILL.gamma_multiply(0.35),
            );
        }

        let response = ui
            .scope_builder(
                egui::UiBuilder::new().max_rect(rect).id_salt(anim_id),
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
            ui.allocate_exact_size(egui::vec2(22.0, ROW_H - 12.0), egui::Sense::drag());
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

        ui.add_space(2.0);
        ui.checkbox(&mut row.on, "");

        // -- what the column holds
        ui.vertical(|ui| {
            ui.add_space(4.0);
            ui.label(strong(row.source.header()).color(text_color));
            ui.label(muted(row.source.describe()));
        });

        // -- the heading to write
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(4.0);
            ui.add(
                egui::TextEdit::singleline(&mut row.title)
                    .hint_text(row.source.header())
                    .desired_width(190.0),
            )
            .on_hover_text("The heading written into the CSV. Empty keeps the original.");
            ui.label(muted("heading"));
        });
    });

    handle
}

// ---------------------------------------------------------------- pieces

/// A card heading with an optional grey remark beside it.
fn section(ui: &mut egui::Ui, title: &str, remark: &str) {
    ui.horizontal(|ui| {
        ui.label(strong(title));
        if !remark.is_empty() {
            ui.label(muted(remark));
        }
    });
}

/// A right-hand value beside a grey label.
fn kv(ui: &mut egui::Ui, key: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [110.0, 18.0],
            egui::Label::new(muted(key)).halign(egui::Align::LEFT),
        );
        ui.label(body(value.into()));
    });
}

/// One date/time input: the field, the remembered value as placeholder and as a
/// button, and the exact instant it resolves to.
fn range_field(ui: &mut egui::Ui, label: &str, text: &mut String, last: &str, end_of_day: bool) {
    ui.horizontal(|ui| {
        ui.add_sized([40.0, 18.0], egui::Label::new(body(label)));

        let hint = if last.trim().is_empty() {
            "18.09.2026 06:30".to_string()
        } else {
            last.trim().to_string()
        };
        ui.add(
            egui::TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(146.0),
        );

        if !last.trim().is_empty()
            && text.trim() != last.trim()
            && ui
                .small_button("Use last")
                .on_hover_text(format!("Paste what was used last time: {}", last.trim()))
                .clicked()
        {
            *text = last.trim().to_string();
        }
        if !text.is_empty() && ui.small_button("Clear").clicked() {
            text.clear();
        }
    });

    // The instant it resolves to, on its own line so the row stays narrow.
    if !text.trim().is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(44.0);
            match convert::parse_when(text, end_of_day) {
                Ok(t) => ui.label(muted(format!("= {}", convert::filetime_to_text(t)))),
                Err(e) => ui.label(muted(e).color(BAD)),
            };
        });
    }
}

// ---------------------------------------------------------------- helpers

/// Ask for the two databases.
fn pick_databases() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .add_filter("HMI databases", &["db3", "db", "sqlite", "sqlite3"])
        .add_filter("All files", &["*"])
        .set_title("Select the Tags and Values databases")
        .pick_files()
}

/// Ask for a whole folder, which is then searched for databases.
fn pick_folder() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Select the folder the HMI wrote its databases into")
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
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} kB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

fn human_size(p: &Path) -> String {
    let Ok(md) = std::fs::metadata(p) else {
        return String::new();
    };
    human_bytes(md.len())
}

/// Shorten from the end, keeping the front. Names are told apart by how they
/// begin, so this is the way round to cut them.
pub fn elide_end(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    format!(
        "{}...",
        chars[..max.saturating_sub(3)].iter().collect::<String>()
    )
}

pub fn elide(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    format!(
        "...{}",
        chars[chars.len() - max + 3..].iter().collect::<String>()
    )
}

/// Show a file in the system file manager, selected.
fn reveal(path: &Path) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
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
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(dir).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
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
    fn names_are_shortened_from_the_right() {
        assert_eq!(elide_end("short", 20), "short");
        // What tells two tags apart is at the front, so the front survives.
        assert_eq!(elide_end("Zustand_DruckvorPumpe2", 14), "Zustand_Dru...");
    }

    #[test]
    fn long_paths_are_shortened_from_the_left() {
        assert_eq!(elide("short", 20), "short");
        assert_eq!(elide("D:/a/very/long/path/x", 10), ".../path/x");
    }
}
