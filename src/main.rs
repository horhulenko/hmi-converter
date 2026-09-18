#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! HMI Log Converter -- turns a pair of WinCC logging databases (tags + values)
//! into one flat CSV table. Drop the two .db3 files in any order; the program
//! works out by itself which one is which.

mod chart;
mod convert;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use chart::{ChartJob, ChartSummary, Detail};
use convert::{Column, Job, Kept, OutColumn, Probe, Progress, Range, Summary};
use eframe::egui;
use theme::{ACCENT, BAD, DIM, MUTED, OK, TEXT, WARN, body, dot, lead, muted, strong};

/// A counting request answered on a worker thread: which request, and its result.
type CountReply = (u64, Result<Kept, String>);

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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([880.0, 690.0])
            .with_min_inner_size([560.0, 520.0])
            .with_drag_and_drop(true)
            .with_title("HMI Log Converter"),
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

    let (mut tags, mut values) = (None, None);
    for path in &inputs {
        match convert::probe(path) {
            Ok(p) => {
                if p.has_values {
                    values = Some(p.path.clone());
                }
                if p.has_tags {
                    tags = Some(p.path.clone());
                }
                if !p.has_values && !p.has_tags {
                    eprintln!("{}: nothing usable", path.display());
                }
            }
            Err(e) => eprintln!("{}: {e}", path.display()),
        }
    }
    let (Some(tags), Some(values)) = (tags, values) else {
        eprintln!("Need one database with LoggingTag and one with LoggedProcessValue.");
        return 2;
    };

    let job = Job {
        out_dir: out_dir
            .or_else(|| values.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from(".")),
        base_name: base_name.unwrap_or_else(|| stem_of(&values)),
        tags_db: tags,
        values_db: values,
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
        tags_db: job.tags_db.clone(),
        values_db: job.values_db.clone(),
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
    Failed(String),
}

/// What the live "how much would be kept" preview currently shows.
enum Preview {
    /// Nothing typed, or no value database yet.
    Idle,
    Counting,
    Kept(Kept),
    Bad(String),
}

struct App {
    tags: Option<Probe>,
    values: Option<Probe>,
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
    preview: Preview,
    /// What the preview was computed for, so it is only recomputed when it changes.
    preview_key: Option<(String, String, PathBuf)>,
    /// A change waiting for the typing to settle before it is counted.
    pending: Option<(String, String, PathBuf)>,
    pending_since: std::time::Instant,
    counter: (Sender<CountReply>, Receiver<CountReply>),
    counter_gen: u64,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, preload: Vec<PathBuf>) -> Self {
        theme::apply(&cc.egui_ctx);
        let last = Settings::load();
        let mut app = Self {
            tags: None,
            values: None,
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
            long_columns: last.column_rows(),
            columns_open: false,
            drag: None,
            from_text: String::new(),
            until_text: String::new(),
            last,
            preview: Preview::Idle,
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

    /// Where the files go, and what they are called.
    fn out_dir(&self) -> PathBuf {
        self.chosen_dir().unwrap_or_else(|| PathBuf::from("."))
    }

    /// The folder as far as it is known: either the one picked, or the one the
    /// value database sits in.
    fn chosen_dir(&self) -> Option<PathBuf> {
        self.out_dir
            .clone()
            .or_else(|| self.values.as_ref()?.path.parent().map(|p| p.to_path_buf()))
    }

    fn base_name(&self) -> String {
        self.base_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| match &self.values {
                Some(v) => stem_of(&v.path),
                None => "hmi".into(),
            })
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
        self.tags.is_some()
            && self.values.is_some()
            && (self.write_long || self.write_wide)
            && (!self.write_long || !self.columns().is_empty())
            && self.range().is_ok()
            && !matches!(self.preview, Preview::Kept(ref k) if k.rows == 0)
    }

    /// Recount whenever the typed range or the value database changed.
    ///
    /// Counting touches every row, so it waits for a short pause in typing
    /// instead of running on each keystroke.
    fn refresh_preview(&mut self, ctx: &egui::Context) {
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

        while let Ok((seq, result)) = self.counter.1.try_recv() {
            if seq == self.counter_gen {
                self.preview = match result {
                    Ok(k) => Preview::Kept(k),
                    Err(e) => Preview::Bad(e),
                };
            }
        }

        let Some(values) = self.values.clone() else {
            self.preview = Preview::Idle;
            self.preview_key = None;
            self.pending = None;
            return;
        };
        let key = (
            self.from_text.trim().to_string(),
            self.until_text.trim().to_string(),
            values.path.clone(),
        );

        // Something changed: show it at once, then let the timer run down.
        if self.preview_key.as_ref() != Some(&key) && self.pending.as_ref() != Some(&key) {
            self.pending = Some(key.clone());
            self.pending_since = std::time::Instant::now();
            self.preview = match self.range() {
                Ok(_) => Preview::Counting,
                Err(e) => Preview::Bad(e),
            };
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
            Ok(r) => r,
            Err(e) => {
                self.preview = Preview::Bad(e);
                return;
            }
        };

        self.counter_gen += 1;
        let seq = self.counter_gen;
        let tx = self.counter.0.clone();
        let path = values.path.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send((seq, convert::count_kept(&path, range)));
            ctx.request_repaint();
        });
    }

    fn note(&mut self, text: impl Into<String>, color: egui::Color32) {
        self.notes.push((text.into(), color));
    }

    /// Take whatever the user dropped or picked and sort it out.
    fn accept(&mut self, paths: Vec<PathBuf>) {
        self.notes.clear();
        if matches!(self.state, State::Running { .. }) {
            self.note("Busy converting -- wait until it is finished.", WARN);
            return;
        }
        self.state = State::Setup;

        // A dropped folder is searched for databases, so dropping the whole
        // export directory works just as well as dropping the two files.
        let mut files = Vec::new();
        for p in paths {
            if p.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&p) {
                    for e in entries.flatten() {
                        let path = e.path();
                        if path.is_file() {
                            files.push(path);
                        }
                    }
                }
            } else {
                files.push(p);
            }
        }

        // Journal files belong to a database, they are never opened on their own.
        files.retain(|p| {
            let n = p.to_string_lossy().to_lowercase();
            !(n.ends_with("-wal") || n.ends_with("-shm") || n.ends_with("-journal"))
        });

        if files.is_empty() {
            self.note("Nothing usable in there.", BAD);
            return;
        }

        for path in files {
            match convert::probe(&path) {
                Ok(p) => self.assign(p),
                Err(e) => self.note(format!("{} -- skipped: {e}", file_name(&path)), BAD),
            }
        }
    }

    /// Put a recognised database into the slot (or slots) it fits, and say what
    /// happened -- including when it is a file that is already loaded.
    fn assign(&mut self, p: Probe) {
        let name = file_name(&p.path);
        let mut loaded: Vec<&str> = Vec::new();
        let mut again: Vec<&str> = Vec::new();

        if p.has_values {
            if self.values.as_ref().map(|v| &v.path) == Some(&p.path) {
                again.push("logged values");
            } else {
                if let Some(old) = &self.values {
                    self.note(
                        format!("Replaced the value database {}", file_name(&old.path)),
                        MUTED,
                    );
                }
                self.values = Some(p.clone());
                loaded.push("logged values");
            }
        }
        if p.has_tags {
            if self.tags.as_ref().map(|t| &t.path) == Some(&p.path) {
                again.push("tag names");
            } else {
                if let Some(old) = &self.tags {
                    self.note(
                        format!("Replaced the tag database {}", file_name(&old.path)),
                        MUTED,
                    );
                }
                self.tags = Some(p.clone());
                loaded.push("tag names");
            }
        }

        if !loaded.is_empty() {
            self.note(format!("{name} -- loaded as {}", loaded.join(" + ")), OK);
        }
        if !again.is_empty() {
            self.note(
                format!(
                    "{name} is already loaded as {} -- nothing to do",
                    again.join(" + ")
                ),
                WARN,
            );
        }
    }

    fn start(&mut self) {
        let (Some(tags), Some(values)) = (&self.tags, &self.values) else {
            return;
        };
        let range = match self.range() {
            Ok(r) => r,
            Err(e) => {
                self.state = State::Failed(e);
                return;
            }
        };

        let job = Job {
            tags_db: tags.path.clone(),
            values_db: values.path.clone(),
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

    /// Build the chart page on a worker, then hand it to the browser.
    fn start_chart(&mut self) {
        let (Some(tags), Some(values)) = (&self.tags, &self.values) else {
            return;
        };
        let range = match self.range() {
            Ok(r) => r,
            Err(e) => {
                self.state = State::Failed(e);
                return;
            }
        };

        let job = ChartJob {
            tags_db: tags.path.clone(),
            values_db: values.path.clone(),
            out_dir: self.out_dir(),
            base_name: self.base_name(),
            range,
            shorten_names: self.shorten_names,
            detail: self.detail,
        };

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
            self.refresh_preview(ui.ctx());
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
        // Two cards per row: what comes in on the left, what goes out on the
        // right, so the whole job fits on one screen without scrolling.
        self.drop_zone(ui, hovering);
        ui.add_space(theme::GAP);
        ui.columns(2, |c| {
            self.files_view(&mut c[0]);
            self.range_view(&mut c[1]);
        });
        ui.add_space(theme::GAP);
        ui.columns(2, |c| {
            self.output_view(&mut c[0]);
            self.options_view(&mut c[1]);
        });
        self.notes_view(ui);
    }

    fn drop_zone(&mut self, ui: &mut egui::Ui, hovering: bool) {
        // Once both databases are in, this steps out of the way.
        let full = self.tags.is_some() && self.values.is_some();
        theme::drop_card(hovering).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                if full && !hovering {
                    ui.horizontal(|ui| {
                        ui.label(muted("Drop other .db3 files here to replace them"));
                        if ui.small_button("Choose files...").clicked()
                            && let Some(files) = pick_databases()
                        {
                            self.accept(files);
                        }
                    });
                    return;
                }
                ui.add_space(4.0);
                ui.label(lead(if hovering {
                    "Release to load"
                } else {
                    "Drag the two .db3 files here"
                }));
                ui.label(muted("any order, any file name"));
                ui.add_space(8.0);
                if ui.button("Choose files...").clicked()
                    && let Some(files) = pick_databases()
                {
                    self.accept(files);
                }
                ui.add_space(2.0);
            });
        });
    }

    fn files_view(&mut self, ui: &mut egui::Ui) {
        let tags = self.tags.clone();
        let values = self.values.clone();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(theme::ROW_TOP);
            section(ui, "Databases", "");
            ui.add_space(theme::GAP);
            slot_row(
                ui,
                "Tag names",
                tags.as_ref(),
                "waiting for the database that holds LoggingTag",
                |p| format!("{} tags", sep(p.tag_count)),
            );
            ui.add_space(theme::GAP);
            slot_row(
                ui,
                "Logged values",
                values.as_ref(),
                "waiting for the database that holds LoggedProcessValue",
                |p| match p.span {
                    Some((a, b)) => format!(
                        "{} rows\n{}  to  {}",
                        sep(p.value_rows),
                        convert::filetime_to_short(a),
                        convert::filetime_to_short(b)
                    ),
                    None => format!("{} rows", sep(p.value_rows)),
                },
            );
        });
    }

    /// Cut the log down to a time slice, with a live count of what survives.
    fn range_view(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(theme::ROW_TOP);
            section(ui, "Time range", "optional");
            ui.add_space(theme::GAP);

            let last_from = self.last.from.clone();
            let last_until = self.last.until.clone();
            range_field(ui, "From", &mut self.from_text, &last_from, false);
            ui.add_space(4.0);
            range_field(ui, "Until", &mut self.until_text, &last_until, true);

            ui.add_space(theme::GAP);
            match &self.preview {
                Preview::Idle => {
                    ui.label(muted("Load the value database to see how much is kept."));
                }
                Preview::Counting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(muted("counting..."));
                    });
                }
                Preview::Bad(e) => {
                    ui.label(body(e).color(BAD));
                }
                Preview::Kept(k) => {
                    let color = if k.rows == 0 {
                        BAD
                    } else if k.rows == k.total_rows {
                        TEXT
                    } else {
                        OK
                    };
                    ui.label(
                        strong(if k.rows == 0 {
                            "Nothing is left in this range.".to_string()
                        } else {
                            format!(
                                "Keeps {} of {} values  ({} timestamps)",
                                sep(k.rows),
                                sep(k.total_rows),
                                sep(k.timestamps)
                            )
                        })
                        .color(color),
                    );
                    if let Some((a, b)) = k.span {
                        ui.label(muted(format!(
                            "{}  to  {}",
                            convert::filetime_to_short(a),
                            convert::filetime_to_short(b)
                        )));
                    }
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
        let tags = self.tags.as_ref()?;
        let short = convert::shorten_tag_names(&tags.tag_names);
        let (id, full) = tags.tag_names.first()?;
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
        let can_chart = self.tags.is_some() && self.values.is_some() && self.range().is_ok();
        let width = ui.available_width();
        let chart_width = 220.0_f32.min(width * 0.32);

        ui.horizontal(|ui| {
            let label = theme::on_accent("Convert to CSV", ready);
            let button = egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow()))
                .min_size(egui::vec2(width - chart_width - 8.0, 42.0))
                .fill(theme::button_fill(ready));
            if ui.add_enabled(ready, button).clicked() {
                self.start();
            }

            let label = if can_chart {
                body("Open charts")
            } else {
                body("Open charts").color(DIM)
            };
            let button = egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow()))
                .min_size(egui::vec2(chart_width, 42.0));
            if ui
                .add_enabled(can_chart, button)
                .on_hover_text(
                    "Draw the values in the browser: eleven kinds of chart, \
                     tags on and off as you watch. Nothing is downloaded.",
                )
                .clicked()
            {
                self.start_chart();
            }
        });

        if !ready {
            let hint = if self.tags.is_none() && self.values.is_some() {
                "Still missing the tag database."
            } else if self.values.is_none() && self.tags.is_some() {
                "Still missing the value database."
            } else if !self.write_long && !self.write_wide {
                "Pick at least one table to write."
            } else if self.write_long && self.columns().is_empty() {
                "The long table has no columns left -- press Columns..."
            } else if self.tags.is_none() && self.values.is_none() {
                "Step 1 -- drop both .db3 files above.    Step 2 -- press Convert."
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
                .add(egui::Button::new("Open charts").min_size(egui::vec2(150.0, 36.0)))
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
                on: true,
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

/// One loaded database: state dot, what it is, the file (click to show it in
/// Explorer) and what was found inside.
fn slot_row(
    ui: &mut egui::Ui,
    title: &str,
    probe: Option<&Probe>,
    missing: &str,
    detail: impl Fn(&Probe) -> String,
) {
    ui.horizontal(|ui| {
        dot(ui, if probe.is_some() { OK } else { DIM });
        ui.vertical(|ui| {
            ui.label(strong(title));
            match probe {
                Some(p) => {
                    if ui
                        .link(body(file_name(&p.path)).color(ACCENT))
                        .on_hover_text(format!("{}\n\nClick to show in Explorer", p.path.display()))
                        .clicked()
                    {
                        reveal(&p.path);
                    }
                    ui.label(muted(detail(p)));
                }
                None => {
                    ui.label(muted(missing));
                }
            }
        });
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

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

fn stem_of(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hmi".into())
}

/// 602912 -> "602 912"
fn sep(n: u64) -> String {
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

fn elide(s: &str, max: usize) -> String {
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
    fn long_paths_are_shortened_from_the_left() {
        assert_eq!(elide("short", 20), "short");
        assert_eq!(elide("D:/a/very/long/path/x", 10), ".../path/x");
    }
}
