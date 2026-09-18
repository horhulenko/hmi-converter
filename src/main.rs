#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! HMI Log Converter -- turns a pair of WinCC logging databases (tags + values)
//! into one flat CSV table. Drop the two .db3 files in any order; the program
//! works out by itself which one is which.

mod convert;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use convert::{Job, Kept, Probe, Progress, Range, Summary};

/// A counting request answered on a worker thread: which request, and its result.
type CountReply = (u64, Result<Kept, String>);
use eframe::egui;

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4D, 0xA3, 0xFF);
const OK: egui::Color32 = egui::Color32::from_rgb(0x4C, 0xC7, 0x8C);
const WARN: egui::Color32 = egui::Color32::from_rgb(0xE8, 0xB3, 0x39);
const BAD: egui::Color32 = egui::Color32::from_rgb(0xE8, 0x6B, 0x6B);
const MUTED: egui::Color32 = egui::Color32::from_gray(0x9A);

fn main() -> eframe::Result {
    // Given file names on the command line, convert without opening a window.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(cli(&args));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 620.0])
            .with_min_inner_size([580.0, 540.0])
            .with_drag_and_drop(true)
            .with_title("HMI Log Converter"),
        ..Default::default()
    };
    eframe::run_native(
        "HMI Log Converter",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

// ---------------------------------------------------------------- settings

/// The last time range the user actually converted, kept between sessions.
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

/// Batch mode: `hmi-converter Tags.db3 Values.db3 [-o FOLDER] [--semicolon]`.
fn cli(args: &[String]) -> i32 {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out_dir: Option<PathBuf> = None;
    let mut semicolon = false;
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
                println!("  --from WHEN        ignore entries before this date/time");
                println!("  --until WHEN       ignore entries after this date/time");
                println!("  --semicolon        use ; instead of , as separator");
                println!("  --long-only        write only the long table");
                println!("  --wide-only        write only the wide table");
                println!();
                println!("WHEN looks like 2026-09-18 or \"2026-09-18 06:30\" (UTC).");
                println!("Start without arguments to open the window.");
                return 0;
            }
            "-o" | "--out" => match it.next() {
                Some(d) => out_dir = Some(PathBuf::from(d)),
                None => {
                    eprintln!("-o needs a folder");
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
                        eprintln!("{a} needs a date, for example 2026-09-18");
                        return 2;
                    }
                }
            }
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
        base_name: values
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "hmi".into()),
        tags_db: tags,
        values_db: values,
        semicolon,
        write_long,
        write_wide,
        range,
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
    out_dir: Option<PathBuf>,
    semicolon: bool,
    write_long: bool,
    write_wide: bool,
    state: State,

    // -- time range
    from_text: String,
    until_text: String,
    /// The range used the last time something was converted, from the settings file.
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
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.all_styles_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 7.0);
        });
        Self {
            tags: None,
            values: None,
            notes: Vec::new(),
            out_dir: None,
            semicolon: false,
            write_long: true,
            write_wide: true,
            state: State::Setup,
            from_text: String::new(),
            until_text: String::new(),
            last: Settings::load(),
            preview: Preview::Idle,
            preview_key: None,
            pending: None,
            pending_since: std::time::Instant::now(),
            counter: channel(),
            counter_gen: 0,
        }
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

        if files.is_empty() {
            self.note("Nothing usable in there.", BAD);
            return;
        }

        // Journal files belong to a database, they are never dropped on their own.
        files.retain(|p| {
            let n = p.to_string_lossy().to_lowercase();
            !(n.ends_with("-wal") || n.ends_with("-shm") || n.ends_with("-journal"))
        });

        for path in files {
            match convert::probe(&path) {
                Ok(p) => self.assign(p),
                Err(e) => {
                    let name = file_name(&path);
                    self.note(format!("{name} -- skipped: {e}"), BAD);
                }
            }
        }

        if self.out_dir.is_none()
            && let Some(v) = &self.values
        {
            self.out_dir = v.path.parent().map(|p| p.to_path_buf());
        }
    }

    /// Put a recognised database into the slot (or slots) it fits.
    fn assign(&mut self, p: Probe) {
        let name = file_name(&p.path);
        let mut roles: Vec<&str> = Vec::new();

        if p.has_values {
            if let Some(old) = &self.values
                && old.path != p.path
            {
                self.note(
                    format!("Replaced values file {}", file_name(&old.path)),
                    MUTED,
                );
            }
            self.values = Some(p.clone());
            roles.push("logged values");
        }
        if p.has_tags {
            if let Some(old) = &self.tags
                && old.path != p.path
            {
                self.note(format!("Replaced tag file {}", file_name(&old.path)), MUTED);
            }
            self.tags = Some(p.clone());
            roles.push("tag names");
        }

        self.note(format!("{name} -- recognised as {}", roles.join(" + ")), OK);
    }

    fn start(&mut self) {
        let (Some(tags), Some(values)) = (&self.tags, &self.values) else {
            return;
        };
        let out_dir = match self.out_dir.clone() {
            Some(d) => d,
            None => match values.path.parent() {
                Some(p) => p.to_path_buf(),
                None => {
                    self.state = State::Failed("no output folder selected".into());
                    return;
                }
            },
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
            out_dir,
            base_name: values
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "hmi".into()),
            semicolon: self.semicolon,
            write_long: self.write_long,
            write_wide: self.write_wide,
            range,
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

        egui::Frame::central_panel(ui.style()).show(ui, |ui| match &self.state {
            State::Running { .. } => self.running_view(ui),
            State::Done(_) => self.done_view(ui),
            State::Failed(_) => self.failed_view(ui),
            State::Setup => self.setup_view(ui, hovering),
        });

        if matches!(self.state, State::Running { .. }) {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

impl App {
    // -- step 1 + 2: files and options ------------------------------------
    fn setup_view(&mut self, ui: &mut egui::Ui, hovering: bool) {
        self.drop_zone(ui, hovering);
        ui.add_space(4.0);

        let tags = self.tags.clone();
        let values = self.values.clone();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            slot_row(
                ui,
                "Tag names",
                tags.as_ref(),
                "waiting for the database that holds LoggingTag",
                |p| format!("{} tags", sep(p.tag_count)),
            );
            ui.add_space(6.0);
            slot_row(
                ui,
                "Logged values",
                values.as_ref(),
                "waiting for the database that holds LoggedProcessValue",
                |p| {
                    let mut s = format!("{} rows", sep(p.value_rows));
                    if let Some((a, b)) = &p.span {
                        s.push_str(&format!("   {}  ->  {}", short_time(a), short_time(b)));
                    }
                    s
                },
            );
        });

        ui.add_space(2.0);
        self.range_view(ui);
        ui.add_space(2.0);
        self.options(ui);

        ui.add_space(8.0);
        let ready = self.ready();
        let button = egui::Button::new(egui::RichText::new("Convert to CSV").size(16.0))
            .min_size(egui::vec2(ui.available_width(), 40.0))
            .fill(if ready {
                ACCENT
            } else {
                egui::Color32::from_gray(60)
            });
        if ui.add_enabled(ready, button).clicked() {
            self.start();
        }

        if !ready {
            let hint = if self.tags.is_none() && self.values.is_some() {
                "Still missing the tag database."
            } else if self.values.is_none() && self.tags.is_some() {
                "Still missing the value database."
            } else if !self.write_long && !self.write_wide {
                "Pick at least one output table under Options."
            } else {
                "Step 1 -- drop both .db3 files above.   Step 2 -- press Convert."
            };
            ui.add_space(2.0);
            ui.label(egui::RichText::new(hint).color(MUTED).size(13.0));
        }

        self.notes_view(ui);
    }

    fn drop_zone(&mut self, ui: &mut egui::Ui, hovering: bool) {
        let (fill, stroke) = if hovering {
            (ACCENT.gamma_multiply(0.18), egui::Stroke::new(2.0, ACCENT))
        } else {
            (
                egui::Color32::from_gray(32),
                egui::Stroke::new(1.0, egui::Color32::from_gray(70)),
            )
        };

        egui::Frame::group(ui.style())
            .fill(fill)
            .stroke(stroke)
            .inner_margin(egui::Margin::symmetric(12, 16))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    let head = if hovering {
                        "Release to load"
                    } else {
                        "Drag the two .db3 files here"
                    };
                    ui.label(egui::RichText::new(head).size(16.0).strong());
                    ui.label(
                        egui::RichText::new("any order, any file name")
                            .color(MUTED)
                            .size(12.0),
                    );
                    ui.add_space(6.0);
                    if ui.button("Choose files...").clicked()
                        && let Some(files) = rfd::FileDialog::new()
                            .add_filter("HMI databases", &["db3", "db", "sqlite", "sqlite3"])
                            .add_filter("All files", &["*"])
                            .set_title("Select Tags and Values databases")
                            .pick_files()
                    {
                        self.accept(files);
                    }
                });
            });
    }

    /// Cut the log down to a time slice, with a live count of what survives.
    fn range_view(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Time range").strong());
                ui.label(
                    egui::RichText::new("optional -- leave empty to take everything")
                        .color(MUTED)
                        .size(12.0),
                );
            });
            ui.add_space(4.0);

            let last_from = self.last.from.clone();
            let last_until = self.last.until.clone();
            range_field(ui, "From ", &mut self.from_text, &last_from, false);
            range_field(ui, "Until", &mut self.until_text, &last_until, true);

            ui.add_space(6.0);
            match &self.preview {
                Preview::Idle => {
                    ui.label(
                        egui::RichText::new("Load the value database to see how much is kept.")
                            .color(MUTED)
                            .size(12.5),
                    );
                }
                Preview::Counting => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("counting...").color(MUTED));
                    });
                }
                Preview::Bad(e) => {
                    ui.label(egui::RichText::new(e).color(BAD).size(13.0));
                }
                Preview::Kept(k) => {
                    let all = k.rows == k.total_rows;
                    let color = if k.rows == 0 {
                        BAD
                    } else if all {
                        MUTED
                    } else {
                        OK
                    };
                    ui.label(
                        egui::RichText::new(if k.rows == 0 {
                            "Nothing is left in this range.".to_string()
                        } else {
                            format!(
                                "Keeps {} of {} values   ({} timestamps)",
                                sep(k.rows),
                                sep(k.total_rows),
                                sep(k.timestamps)
                            )
                        })
                        .color(color)
                        .size(13.5)
                        .strong(),
                    );
                    if let Some((a, b)) = &k.span {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}   ->   {}",
                                short_time(a),
                                short_time(b)
                            ))
                            .color(MUTED)
                            .size(12.0),
                        );
                    }
                }
            }
        });
    }

    fn options(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("Options")
            .default_open(false)
            .show(ui, |ui| {
                ui.checkbox(
                    &mut self.write_long,
                    "Long table -- one row per value (pk_TimeStamp, pk_fk_Id, Value, Name)",
                );
                ui.checkbox(
                    &mut self.write_wide,
                    "Wide table -- one row per timestamp, one column per tag",
                );
                ui.checkbox(
                    &mut self.semicolon,
                    "Use ; as separator (for Excel with a comma decimal mark)",
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Save to:");
                    let shown = self
                        .out_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "next to the value database".into());
                    ui.label(egui::RichText::new(elide(&shown, 46)).color(MUTED))
                        .on_hover_text(shown);
                    if ui.small_button("Change...").clicked()
                        && let Some(d) = rfd::FileDialog::new().pick_folder()
                    {
                        self.out_dir = Some(d);
                    }
                });
            });
    }

    // -- step 3: progress --------------------------------------------------
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

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(format!("{stage}..."))
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(6.0);

            let fraction = if total > 0 {
                (done as f32 / total as f32).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let text = if total > 0 {
                format!("{}  /  {} rows", sep(done), sep(total))
            } else {
                "preparing".into()
            };
            ui.add(
                egui::ProgressBar::new(fraction)
                    .text(text)
                    .fill(ACCENT)
                    .animate(total == 0)
                    .desired_height(22.0),
            );
        });

        ui.add_space(8.0);
        if ui
            .add(egui::Button::new("Cancel").min_size(egui::vec2(120.0, 32.0)))
            .clicked()
        {
            cancel.store(true, Ordering::Relaxed);
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("The source databases are only read, never changed.")
                .color(MUTED)
                .size(12.0),
        );
    }

    // -- step 4: result ----------------------------------------------------
    fn done_view(&mut self, ui: &mut egui::Ui) {
        let State::Done(s) = &self.state else { return };
        let s = s.clone();

        ui.label(
            egui::RichText::new("Finished")
                .size(18.0)
                .strong()
                .color(OK),
        );
        ui.label(
            egui::RichText::new(format!(
                "{} values from {} tags in {:.1} s",
                sep(s.rows),
                s.tags_used,
                s.secs
            ))
            .color(MUTED),
        );
        ui.add_space(8.0);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (path, rows) in &s.files {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("[ok]").color(OK).monospace());
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(file_name(path)).strong());
                        ui.label(
                            egui::RichText::new(format!(
                                "{} rows   {}",
                                sep(*rows),
                                human_size(path)
                            ))
                            .color(MUTED)
                            .size(12.0),
                        );
                    });
                });
            }
        });

        ui.add_space(8.0);
        ui.label(egui::RichText::new("Details").strong());
        if let Some((a, b)) = &s.span {
            ui.label(format!("Time range   {a}  ->  {b}"));
        }
        ui.label(format!(
            "Tags   {} of {} configured tags actually logged values",
            s.tags_used, s.tags_total
        ));
        ui.label(format!("Timestamps   {}", sep(s.timestamps)));
        if s.bad_quality > 0 {
            ui.label(
                egui::RichText::new(format!(
                    "{} rows have a Quality other than 192 (not good) -- kept, see the Quality column",
                    sep(s.bad_quality)
                ))
                .color(WARN),
            );
        }
        if !s.unknown_ids.is_empty() {
            ui.label(
                egui::RichText::new(format!(
                    "{} tag ids had no name in the tag database, written as id_<number>",
                    s.unknown_ids.len()
                ))
                .color(WARN),
            );
        }

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new("Open folder")
                        .min_size(egui::vec2(140.0, 34.0))
                        .fill(ACCENT),
                )
                .clicked()
                && let Some((p, _)) = s.files.first()
            {
                reveal(p);
            }
            if ui
                .add(egui::Button::new("Convert more").min_size(egui::vec2(140.0, 34.0)))
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
            egui::RichText::new(if cancelled {
                "Cancelled"
            } else {
                "It did not work"
            })
            .size(18.0)
            .strong()
            .color(if cancelled { WARN } else { BAD }),
        );
        ui.add_space(6.0);
        if cancelled {
            ui.label("Nothing was finished. Any half-written CSV file can be deleted.");
        } else {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new(msg).color(BAD));
            });
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Check that both files are the databases from the same HMI export and \
                     that the output folder can be written to.",
                )
                .color(MUTED),
            );
        }
        ui.add_space(12.0);
        if ui
            .add(
                egui::Button::new("Back")
                    .min_size(egui::vec2(140.0, 34.0))
                    .fill(ACCENT),
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
        ui.add_space(10.0);
        ui.separator();
        egui::ScrollArea::vertical()
            .max_height(110.0)
            .show(ui, |ui| {
                for (text, color) in &self.notes {
                    ui.label(egui::RichText::new(text).color(*color).size(12.5));
                }
            });
    }
}

// ---------------------------------------------------------------- helpers

fn slot_row(
    ui: &mut egui::Ui,
    title: &str,
    probe: Option<&Probe>,
    missing: &str,
    detail: impl Fn(&Probe) -> String,
) {
    ui.horizontal(|ui| {
        let (mark, color) = match probe {
            Some(_) => ("[ok]", OK),
            None => ("[  ]", MUTED),
        };
        ui.label(egui::RichText::new(mark).color(color).monospace());
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(title).strong());
            match probe {
                Some(p) => {
                    ui.label(
                        egui::RichText::new(file_name(&p.path))
                            .color(ACCENT)
                            .size(12.5),
                    )
                    .on_hover_text(p.path.display().to_string());
                    ui.label(egui::RichText::new(detail(p)).color(MUTED).size(12.0));
                }
                None => {
                    ui.label(egui::RichText::new(missing).color(MUTED).size(12.5));
                }
            }
        });
    });
}

/// One date/time input: the field itself, the remembered value as placeholder
/// and as a button, and the exact instant it resolves to.
fn range_field(ui: &mut egui::Ui, label: &str, text: &mut String, last: &str, end_of_day: bool) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).monospace());

        let hint = if last.trim().is_empty() {
            "2026-09-18 06:30".to_string()
        } else {
            last.trim().to_string()
        };
        ui.add(
            egui::TextEdit::singleline(text)
                .hint_text(&hint)
                .desired_width(200.0),
        );

        if !last.trim().is_empty()
            && text.trim() != last.trim()
            && ui
                .small_button("Use last")
                .on_hover_text(format!("Paste the value used last time: {}", last.trim()))
                .clicked()
        {
            *text = last.trim().to_string();
        }
        if !text.is_empty() && ui.small_button("Clear").clicked() {
            text.clear();
        }

        if !text.trim().is_empty() {
            match convert::parse_when(text, end_of_day) {
                Ok(t) => {
                    ui.label(
                        egui::RichText::new(format!("= {}", convert::filetime_to_iso(t)))
                            .color(MUTED)
                            .size(12.0),
                    );
                }
                Err(e) => {
                    ui.label(egui::RichText::new(e).color(BAD).size(12.0));
                }
            }
        }
    });
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
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

/// "2026-09-18T06:03:48.7883740Z" -> "2026-09-18 06:03:48"
fn short_time(iso: &str) -> String {
    iso.replace('T', " ")
        .split('.')
        .next()
        .unwrap_or(iso)
        .to_string()
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

/// Show the finished file in the system file manager.
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
    fn timestamps_shorten() {
        assert_eq!(
            short_time("2026-09-18T06:03:48.7883740Z"),
            "2026-09-18 06:03:48"
        );
    }
}
