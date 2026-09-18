#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! HMI Log Converter -- turns a pair of WinCC logging databases (tags + values)
//! into one flat CSV table. Drop the two .db3 files in any order; the program
//! works out by itself which one is which.

mod convert;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use convert::{Job, Kept, Probe, Progress, Range, Summary};
use eframe::egui;
use theme::{ACCENT, BAD, DIM, MUTED, OK, TEXT, WARN, body, dot, lead, muted, strong};

/// A counting request answered on a worker thread: which request, and its result.
type CountReply = (u64, Result<Kept, String>);

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
            .with_inner_size([700.0, 812.0])
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
}

impl Settings {
    fn file() -> Option<PathBuf> {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("hmi-converter").join("last-range.txt"))
    }

    /// A two-line `key=value` file -- small enough not to need a format crate.
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
        let _ = std::fs::write(path, format!("from={}\nuntil={}\n", self.from, self.until));
    }

    fn is_empty(&self) -> bool {
        self.from.trim().is_empty() && self.until.trim().is_empty()
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
    let (mut write_long, mut write_wide) = (true, true);
    let mut range = Range::default();

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
                println!("  --semicolon        use ; instead of , as separator");
                println!("  --long-only        write only the long table");
                println!("  --wide-only        write only the wide table");
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
            0
        }
        Err(e) => {
            eprintln!("failed: {e}");
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
            from_text: String::new(),
            until_text: String::new(),
            last: Settings::load(),
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
        };

        // Remember what was typed so the next run can offer it again.
        let typed = Settings {
            from: self.from_text.trim().to_string(),
            until: self.until_text.trim().to_string(),
        };
        if !typed.is_empty() {
            typed.save();
            self.last = typed;
        }

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
                    State::Failed(_) => self.failed_view(ui),
                    State::Setup => self.setup_view(ui, hovering),
                });
        });

        if matches!(self.state, State::Running { .. }) {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

impl App {
    // -- choosing files and options ---------------------------------------
    fn setup_view(&mut self, ui: &mut egui::Ui, hovering: bool) {
        self.drop_zone(ui, hovering);
        ui.add_space(theme::GAP);
        self.files_view(ui);
        ui.add_space(theme::GAP);
        self.range_view(ui);
        ui.add_space(theme::GAP);
        self.output_view(ui);
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
                        "{} rows    {}  to  {}",
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
            section(ui, "Time range", "optional -- empty means everything");
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
            section(ui, "Save to", "");
            ui.add_space(theme::GAP);

            ui.horizontal(|ui| {
                ui.label(body("Name"));
                let mut name = self.base_name();
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut name)
                            .desired_width(220.0)
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
                            .link(muted(elide(&dir.display().to_string(), 52)))
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

            ui.add_space(theme::GAP);
            self.options_view(ui);
        });
    }

    fn options_view(&mut self, ui: &mut egui::Ui) {
        ui.checkbox(&mut self.write_long, "Long table -- one row per value");
        ui.checkbox(
            &mut self.write_wide,
            "Wide table -- one row per timestamp, one column per tag",
        );
        ui.checkbox(
            &mut self.semicolon,
            "Separate with ;  (Excel with a comma decimal mark)",
        );
        ui.checkbox(&mut self.shorten_names, "Shorten tag names");

        if self.shorten_names {
            ui.add_space(2.0);
            ui.indent("shorten_example", |ui| match self.shorten_example() {
                Some((full, short)) => {
                    ui.label(muted(full.to_string()));
                    ui.label(muted(format!("becomes   {short}")).color(OK));
                }
                None => {
                    ui.label(muted("Load the tag database to see an example."));
                }
            });
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
        let label = theme::on_accent("Convert to CSV", ready);
        let button = egui::Button::new((egui::Atom::grow(), label, egui::Atom::grow()))
            .min_size(egui::vec2(ui.available_width(), 42.0))
            .fill(theme::button_fill(ready));
        if ui.add_enabled(ready, button).clicked() {
            self.start();
        }

        if !ready {
            let hint = if self.tags.is_none() && self.values.is_some() {
                "Still missing the tag database."
            } else if self.values.is_none() && self.tags.is_some() {
                "Still missing the value database."
            } else if !self.write_long && !self.write_wide {
                "Pick at least one table to write."
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
                .add(egui::Button::new("Convert more").min_size(egui::vec2(150.0, 36.0)))
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
        ui.add_sized([44.0, 18.0], egui::Label::new(body(label)));

        let hint = if last.trim().is_empty() {
            "18.09.2026 06:30".to_string()
        } else {
            last.trim().to_string()
        };
        ui.add(
            egui::TextEdit::singleline(text)
                .hint_text(hint)
                .desired_width(190.0),
        );

        if !last.trim().is_empty()
            && text.trim() != last.trim()
            && ui
                .button("Use last")
                .on_hover_text(format!("Paste what was used last time: {}", last.trim()))
                .clicked()
        {
            *text = last.trim().to_string();
        }
        if !text.is_empty() && ui.button("Clear").clicked() {
            text.clear();
        }

        if !text.trim().is_empty() {
            match convert::parse_when(text, end_of_day) {
                Ok(t) => ui.label(muted(format!("= {}", convert::filetime_to_text(t)))),
                Err(e) => ui.label(muted(e).color(BAD)),
            };
        }
    });
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

fn human_size(p: &Path) -> String {
    let Ok(md) = std::fs::metadata(p) else {
        return String::new();
    };
    let b = md.len() as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} kB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
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
