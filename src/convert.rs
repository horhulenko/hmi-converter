//! Reading the two HMI SQLite databases and writing the CSV tables.
//!
//! Layout of the source data (WinCC Unified / "iseg" logging):
//!   Tags.db3    -> LoggingTag(pk_Key, Name, ...)                one row per logged tag
//!   Values.db3  -> LoggedProcessValue(pk_TimeStamp, pk_fk_Id, Quality, Value)
//!                  LoggedProcessValueText / LoggedProcessValue64   same shape, other value types
//! The join is LoggedProcessValue.pk_fk_Id == LoggingTag.pk_Key.
//! Timestamps are Windows FILETIME: 100-nanosecond ticks since 1601-01-01 UTC.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use rusqlite::{Connection, OpenFlags};

/// The three value tables we merge, in the order they are written.
const VALUE_TABLES: [&str; 3] = [
    "LoggedProcessValue",
    "LoggedProcessValueText",
    "LoggedProcessValue64",
];

pub const TAG_TABLE: &str = "LoggingTag";
pub const MAIN_VALUE_TABLE: &str = "LoggedProcessValue";

/// Quality code meaning "good" in the WinCC logs.
const QUALITY_GOOD: i64 = 192;

// ---------------------------------------------------------------- time

const FILETIME_EPOCH_DIFF_SECS: i64 = 11_644_473_600; // 1601-01-01 -> 1970-01-01
const TICKS_PER_SEC: i64 = 10_000_000;

/// FILETIME ticks -> `18.09.2026 06:03:48.7883740` (UTC), the form written to
/// the CSV files. Every date shown or written by this program looks like this.
pub fn filetime_to_text(ticks: i64) -> String {
    let (y, m, d, h, mi, s, frac) = parts(ticks);
    format!("{d:02}.{m:02}.{y:04} {h:02}:{mi:02}:{s:02}.{frac:07}")
}

/// The same instant without the fraction: `18.09.2026 06:03:48`.
/// Used wherever a whole second is precise enough to read.
pub fn filetime_to_short(ticks: i64) -> String {
    let (y, m, d, h, mi, s, _) = parts(ticks);
    format!("{d:02}.{m:02}.{y:04} {h:02}:{mi:02}:{s:02}")
}

/// Split FILETIME ticks into (year, month, day, hour, minute, second, 100ns).
fn parts(ticks: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let secs = ticks.div_euclid(TICKS_PER_SEC) - FILETIME_EPOCH_DIFF_SECS;
    let frac = ticks.rem_euclid(TICKS_PER_SEC);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    (y, m, d, sod / 3600, (sod % 3600) / 60, sod % 60, frac)
}

/// (year, month, day) -> days since the Unix epoch. Inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The last tick of the given day, used when only a date is typed as an upper bound.
const TICKS_PER_DAY: i64 = 86_400 * TICKS_PER_SEC;

/// Turn something a person typed into FILETIME ticks.
///
/// Accepted: `2026-09-18`, `18.09.2026`, either of those followed by `06:30`,
/// `06:30:45` or `06:30:45.123`, with `T` or a space in between and an optional
/// trailing `Z`. A bare number is taken to be raw FILETIME ticks.
///
/// `end_of_day` extends a date without a time to 23:59:59.9999999, which is
/// what people mean by "until the 18th".
pub fn parse_when(text: &str, end_of_day: bool) -> Result<i64, String> {
    let s = text.trim().trim_end_matches(['z', 'Z']).trim();
    if s.is_empty() {
        return Err("empty".into());
    }

    // Raw ticks, as they appear in the pk_TimeStamp_Raw column.
    if s.chars().all(|c| c.is_ascii_digit()) {
        return match s.parse::<i64>() {
            Ok(n) if n >= 0 => Ok(n),
            _ => Err("diese Zahl ist kein Zeitstempel".into()),
        };
    }

    let (date_part, time_part) = match s.split_once(['T', 't', ' ']) {
        Some((d, t)) => (d.trim(), t.trim()),
        None => (s, ""),
    };

    let nums: Result<Vec<i64>, _> = if date_part.contains('.') {
        date_part
            .split('.')
            .map(|p| p.trim().parse::<i64>())
            .collect()
    } else {
        date_part
            .split('-')
            .map(|p| p.trim().parse::<i64>())
            .collect()
    };
    let nums = nums.map_err(|_| "Datum bitte als 18.09.2026 schreiben".to_string())?;
    let (y, mo, d) = match (nums.len(), date_part.contains('.')) {
        (3, false) => (nums[0], nums[1], nums[2]),
        (3, true) => (nums[2], nums[1], nums[0]), // 18.09.2026
        _ => return Err("Datum bitte als 18.09.2026 oder 2026-09-18 schreiben".into()),
    };
    if !(1601..=9999).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return Err("dieses Datum gibt es nicht".into());
    }

    let mut ticks = (days_from_civil(y, mo, d) * 86_400 + FILETIME_EPOCH_DIFF_SECS) * TICKS_PER_SEC;

    if time_part.is_empty() {
        if end_of_day {
            ticks += TICKS_PER_DAY - 1;
        }
        return Ok(ticks);
    }

    let (hms, frac) = match time_part.split_once([',', '.']) {
        Some((a, b)) => (a, b),
        None => (time_part, ""),
    };
    let parts: Result<Vec<i64>, _> = hms.split(':').map(|p| p.trim().parse::<i64>()).collect();
    let parts = parts.map_err(|_| "Uhrzeit bitte als 06:30 oder 06:30:45 schreiben".to_string())?;
    let (h, mi, sec) = match parts.len() {
        2 => (parts[0], parts[1], 0),
        3 => (parts[0], parts[1], parts[2]),
        _ => return Err("Uhrzeit bitte als 06:30 oder 06:30:45 schreiben".into()),
    };
    if h > 23 || mi > 59 || sec > 59 || h < 0 || mi < 0 || sec < 0 {
        return Err("diese Uhrzeit gibt es nicht".into());
    }
    ticks += ((h * 3600) + (mi * 60) + sec) * TICKS_PER_SEC;

    if !frac.is_empty() {
        if !frac.chars().all(|c| c.is_ascii_digit()) {
            return Err("Sekundenbruchteile bitte nur als Ziffern".into());
        }
        let mut digits = frac.to_string();
        digits.truncate(7);
        while digits.len() < 7 {
            digits.push('0');
        }
        ticks += digits.parse::<i64>().unwrap_or(0);
    }

    Ok(ticks)
}

/// Days since the Unix epoch -> (year, month, day). Howard Hinnant civil_from_days.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- tag names

/// Characters that separate the parts of a tag name.
const SEPARATORS: [u8; 4] = *b":_.-";

/// Try to shorten every tag name to the part that actually distinguishes it.
///
/// `Anlage1::Zustand_DruckvorPumpe2:Archivvariable_1` becomes
/// `DruckvorPumpe2`: whatever every name has in common is dropped, and so is
/// the leading category (`Zustand_`, `Parameter_`, ...).
///
/// Names must stay unique, because they become column headings. Any name that
/// would clash keeps its category, then its full text, so shortening never
/// merges two different tags.
pub fn shorten_tag_names(names: &[(i64, String)]) -> HashMap<i64, String> {
    let mut sorted: Vec<&(i64, String)> = names.iter().collect();
    sorted.sort_by_key(|(id, _)| *id);
    let full: Vec<&str> = sorted.iter().map(|(_, n)| n.as_str()).collect();

    let (pre, suf) = common_affixes(&full);
    let core: Vec<String> = full.iter().map(|f| core_of(f, pre, suf)).collect();
    let short: Vec<String> = core.iter().map(|c| drop_category(c)).collect();

    // Shortest form first, falling back for whatever would clash.
    let seen = tally(&short);
    let mut chosen: Vec<String> = (0..full.len())
        .map(|i| {
            if seen[&short[i]] == 1 {
                short[i].clone()
            } else {
                core[i].clone()
            }
        })
        .collect();

    let seen = tally(&chosen);
    for (i, name) in chosen.iter_mut().enumerate() {
        if seen[&*name] > 1 {
            *name = full[i].to_string();
        }
    }

    // Two tags configured with the very same name: keep them apart by number.
    let seen = tally(&chosen);
    for (i, name) in chosen.iter_mut().enumerate() {
        if seen[&*name] > 1 {
            *name = format!("{name}_id{}", sorted[i].0);
        }
    }

    sorted
        .iter()
        .zip(chosen)
        .map(|((id, _), name)| (*id, name))
        .collect()
}

/// Drop whatever every one of these names has in common, for reading in tight
/// places. Unlike [`shorten_tag_names`] the category is kept, so this can never
/// make two names look alike, and running it twice changes nothing.
pub fn trim_common(names: &[String]) -> Vec<String> {
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    let (pre, suf) = common_affixes(&borrowed);
    borrowed.iter().map(|n| core_of(n, pre, suf)).collect()
}

fn tally(names: &[String]) -> HashMap<String, usize> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for n in names {
        *counts.entry(n.clone()).or_insert(0) += 1;
    }
    counts
}

/// How many bytes at the start and at the end every name shares, cut back to a
/// separator so that words are never sliced in half.
fn common_affixes(names: &[&str]) -> (usize, usize) {
    if names.len() < 2 {
        return (0, 0);
    }
    let first = names[0].as_bytes();
    let shortest = names.iter().map(|n| n.len()).min().unwrap_or(0);

    let mut pre = first.len();
    for n in &names[1..] {
        let b = n.as_bytes();
        let mut i = 0;
        while i < pre && i < b.len() && b[i] == first[i] {
            i += 1;
        }
        pre = i;
    }
    while pre > 0 && !SEPARATORS.contains(&first[pre - 1]) {
        pre -= 1;
    }

    let mut suf = shortest - pre.min(shortest);
    for n in &names[1..] {
        let b = n.as_bytes();
        let mut i = 0;
        while i < suf && b[b.len() - 1 - i] == first[first.len() - 1 - i] {
            i += 1;
        }
        suf = i;
    }
    // The common tail must begin at a separator, or it is not a whole part.
    let mut trimmed = 0;
    for i in (first.len() - suf)..first.len() {
        if SEPARATORS.contains(&first[i]) {
            trimmed = first.len() - i;
            break;
        }
    }
    (pre, trimmed)
}

/// The name with the shared head and tail removed.
fn core_of(full: &str, pre: usize, suf: usize) -> String {
    let end = full.len().saturating_sub(suf).max(pre);
    let core = full
        .get(pre..end)
        .unwrap_or(full)
        .trim_matches(|c: char| c == ':' || c == '_' || c.is_whitespace());
    if core.is_empty() {
        full.to_string()
    } else {
        core.to_string()
    }
}

/// `Zustand_DruckvorPumpe2` -> `DruckvorPumpe2`; a name without a
/// category (`Fehler1`) is left alone.
fn drop_category(core: &str) -> String {
    // Still a full path (a single tag has nothing to compare against): leave it.
    if core.contains(':') {
        return core.to_string();
    }
    match core.split_once('_') {
        Some((head, rest)) if !head.is_empty() && !rest.is_empty() => rest.to_string(),
        _ => core.to_string(),
    }
}

// ---------------------------------------------------------------- opening

/// Open a database for reading without ever modifying the original file.
///
/// Plain read-only mode fails on some HMI exports because SQLite wants to
/// replay a leftover -wal journal, so fall back to `immutable=1` and finally
/// to a scratch copy of the file together with its -wal/-shm siblings.
pub fn open_readonly(path: &Path) -> Result<Connection, String> {
    let ro = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(c) = Connection::open_with_flags(path, ro)
        && c.prepare("SELECT 1 FROM sqlite_master LIMIT 1").is_ok()
    {
        tune(&c);
        return Ok(c);
    }

    let uri = format!(
        "file:{}?immutable=1",
        path.to_string_lossy()
            .replace('?', "%3f")
            .replace('#', "%23")
    );
    if let Ok(c) = Connection::open_with_flags(&uri, ro | OpenFlags::SQLITE_OPEN_URI)
        && c.prepare("SELECT 1 FROM sqlite_master LIMIT 1").is_ok()
    {
        tune(&c);
        return Ok(c);
    }

    // Last resort: work on a copy, so a database still in use by the HMI converts too.
    let scratch = std::env::temp_dir().join("hmi-converter-scratch");
    fs::create_dir_all(&scratch).map_err(|e| format!("Temporärer Ordner kann nicht angelegt werden: {e}"))?;
    let name = path.file_name().unwrap_or_default().to_os_string();
    let copy = scratch.join(&name);
    fs::copy(path, &copy).map_err(|e| format!("Datei kann nicht gelesen werden: {e}"))?;
    for suffix in ["-wal", "-shm"] {
        let sibling = PathBuf::from(format!("{}{}", path.to_string_lossy(), suffix));
        if sibling.exists() {
            let target = scratch.join(format!("{}{}", name.to_string_lossy(), suffix));
            let _ = fs::copy(&sibling, target);
        }
    }
    let c = Connection::open(&copy).map_err(|e| format!("Datenbank kann nicht geöffnet werden: {e}"))?;
    tune(&c);
    Ok(c)
}

fn tune(c: &Connection) {
    let _ = c.execute_batch("PRAGMA temp_store=MEMORY; PRAGMA cache_size=-65536;");
}

pub fn table_names(conn: &Connection) -> Result<BTreeSet<String>, String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(|e| format!("keine lesbare SQLite-Datenbank: {e}"))?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut out = BTreeSet::new();
    for r in rows {
        out.insert(r.map_err(|e| e.to_string())?);
    }
    Ok(out)
}

fn count(conn: &Connection, table: &str) -> u64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
        r.get::<_, i64>(0)
    })
    .map(|n| n as u64)
    .unwrap_or(0)
}

// ---------------------------------------------------------------- finding files

/// The first bytes of every SQLite file, whatever it is called.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// How deep a dropped folder is searched for databases.
const MAX_DEPTH: usize = 6;

/// How many files are looked at before a folder is given up on, so that
/// dropping something like `C:\` cannot hang the program.
const MAX_FILES: usize = 4000;

/// A file SQLite could open, decided by its first bytes rather than its name.
///
/// An HMI export names its databases `..._Werte_20260302_053000.db3`, but
/// nothing guarantees that, so the header is what is trusted.
pub fn looks_like_sqlite(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 16];
    f.read_exact(&mut head).is_ok() && &head == SQLITE_MAGIC
}

/// A journal belongs to a database and is never opened on its own.
fn is_sidecar(path: &Path) -> bool {
    let name = path.to_string_lossy().to_lowercase();
    name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal")
}

/// Every file worth probing behind what was dropped or picked.
///
/// A file given by name is kept as it is -- the user pointed at it, so if it
/// turns out not to be a database that is worth saying. Inside a folder, only
/// files that really are SQLite are kept, so dropping a whole export directory
/// produces no noise about its logs and its `.ini` files.
pub fn candidates(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
    let mut budget = MAX_FILES;

    let push = |path: PathBuf, out: &mut Vec<PathBuf>, seen: &mut BTreeSet<PathBuf>| {
        let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            out.push(path);
        }
    };

    for path in paths {
        if path.is_dir() {
            walk(path, 0, &mut budget, &mut |file| {
                if !is_sidecar(&file) && looks_like_sqlite(&file) {
                    push(file, &mut out, &mut seen);
                }
            });
        } else if path.is_file() && !is_sidecar(path) {
            push(path.clone(), &mut out, &mut seen);
        }
    }
    out
}

/// Every file under `dir`, folders first so that one export stays together.
fn walk(dir: &Path, depth: usize, budget: &mut usize, found: &mut impl FnMut(PathBuf)) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => dirs.push(path),
            Ok(t) if t.is_file() => files.push(path),
            _ => {}
        }
    }
    files.sort();
    dirs.sort();
    for f in files {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        found(f);
    }
    for d in dirs {
        walk(&d, depth + 1, budget, found);
    }
}

// ---------------------------------------------------------------- probing

/// What a single file turned out to be.
#[derive(Clone, Debug)]
pub struct Probe {
    pub path: PathBuf,
    pub has_tags: bool,
    pub has_values: bool,
    pub tag_count: u64,
    /// Every configured tag, so the window can preview the shortening.
    /// Empty unless this is the tag database.
    pub tag_names: Vec<(i64, String)>,
    pub value_rows: u64,
    /// First and last logged instant, as FILETIME ticks.
    pub span: Option<(i64, i64)>,
    /// How many values fall into each of [`DENSITY`] equal parts of the span.
    ///
    /// An HMI stops logging overnight or while the plant is down, so a file
    /// that covers a day may hold two hours of it. The timeline draws this, so
    /// a slice is taken where there is something to take.
    pub density: Vec<u32>,
    /// Size of the file on disk.
    pub bytes: u64,
}

/// How finely [`Probe::density`] cuts a file up. About one part per pixel of
/// a wide window, which is as much as can be drawn.
pub const DENSITY: usize = 240;

impl Probe {
    /// What this file can be used for, for the table in the window.
    pub fn role(&self) -> &'static str {
        match (self.has_tags, self.has_values) {
            (true, true) => "Tags + Werte",
            (true, false) => "Tag-Namen",
            _ => "Messwerte",
        }
    }
}

/// Inspect a file and decide, from its schema alone, which role it can play.
pub fn probe(path: &Path) -> Result<Probe, String> {
    let conn = open_readonly(path)?;
    let tables = table_names(&conn)?;
    let has_tags = tables.contains(TAG_TABLE);
    let has_values = tables.contains(MAIN_VALUE_TABLE);

    if !has_tags && !has_values {
        return Err(format!(
            "keine HMI-Protokolltabellen ({TAG_TABLE} oder {MAIN_VALUE_TABLE}) enthalten"
        ));
    }

    let tag_count = if has_tags { count(&conn, TAG_TABLE) } else { 0 };
    let tag_names = if has_tags {
        read_tag_names(&conn).unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut value_rows = 0;
    let mut span = None;
    let mut density = Vec::new();
    if has_values {
        let used: Vec<&str> = VALUE_TABLES
            .iter()
            .copied()
            .filter(|t| tables.contains(*t))
            .collect();
        let bounds = conn.query_row(
            &format!("SELECT MIN(pk_TimeStamp), MAX(pk_TimeStamp) FROM \"{MAIN_VALUE_TABLE}\""),
            [],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
        );
        if let Ok((Some(lo), Some(hi))) = bounds {
            span = Some((lo, hi));
        }

        // Counting and spreading the rows over the span is one pass either
        // way, so the picture costs nothing that the count did not.
        match span {
            Some((lo, hi)) if hi > lo => {
                density = vec![0u32; DENSITY];
                let step = ((hi - lo) / DENSITY as i64).max(1);
                let inner = used
                    .iter()
                    .map(|t| format!("SELECT pk_TimeStamp FROM \"{t}\""))
                    .collect::<Vec<_>>()
                    .join(" UNION ALL ");
                let sql = format!(
                    "SELECT (pk_TimeStamp - {lo}) / {step}, COUNT(*) FROM ({inner}) GROUP BY 1"
                );
                if let Ok(mut stmt) = conn.prepare(&sql)
                    && let Ok(mut rows) = stmt.query([])
                {
                    while let Ok(Some(r)) = rows.next() {
                        let slot: i64 = r.get(0).unwrap_or(-1);
                        let n: i64 = r.get(1).unwrap_or(0);
                        value_rows += n as u64;
                        if let Ok(slot) = usize::try_from(slot)
                            && slot < DENSITY
                        {
                            density[slot] = density[slot].saturating_add(n as u32);
                        }
                    }
                }
            }
            _ => {
                for t in &used {
                    value_rows += count(&conn, t);
                }
            }
        }
    }

    Ok(Probe {
        path: path.to_path_buf(),
        has_tags,
        has_values,
        tag_count,
        tag_names,
        value_rows,
        span,
        density,
        bytes: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
    })
}

/// Every tag id and its name, taken from all the given tag databases.
///
/// HMI exports repeat the same tag table beside every batch of values, so the
/// same ids usually arrive several times; the first name found for an id wins
/// and later files only add what is new.
pub fn read_all_tag_names(dbs: &[PathBuf]) -> Result<Vec<(i64, String)>, String> {
    let mut by_key: HashMap<i64, String> = HashMap::new();
    for db in dbs {
        let conn = open_readonly(db)?;
        for (id, name) in read_tag_names(&conn)? {
            by_key.entry(id).or_insert(name);
        }
    }
    let mut out: Vec<(i64, String)> = by_key.into_iter().collect();
    out.sort_by_key(|(id, _)| *id);
    Ok(out)
}

/// Every tag id and its configured name. When a tag has several configuration
/// versions the newest name wins.
pub fn read_tag_names(conn: &Connection) -> Result<Vec<(i64, String)>, String> {
    let sql = format!("SELECT pk_Key, Name FROM \"{TAG_TABLE}\" ORDER BY pk_StartTime");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("{TAG_TABLE}: {e}"))?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut by_key: HashMap<i64, String> = HashMap::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let key: i64 = row.get(0).map_err(|e| e.to_string())?;
        let name: Option<String> = row.get(1).map_err(|e| e.to_string())?;
        if let Some(name) = name {
            by_key.insert(key, name);
        }
    }
    let mut out: Vec<(i64, String)> = by_key.into_iter().collect();
    out.sort_by_key(|(id, _)| *id);
    Ok(out)
}

// ---------------------------------------------------------------- time range

/// An inclusive time window, in FILETIME ticks. `None` means "no limit".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Range {
    pub from: Option<i64>,
    pub until: Option<i64>,
}

impl Range {
    pub fn is_open(&self) -> bool {
        self.from.is_none() && self.until.is_none()
    }

}

/// One or more windows, which is what the user draws on the timeline.
///
/// Empty means no limit at all. Several windows let one export hold, say, the
/// two interesting hours of a shift without the quiet morning in between.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Windows {
    pub spans: Vec<Range>,
}

impl Windows {
    /// Everything there is.
    pub fn whole() -> Self {
        Self::default()
    }

    pub fn one(range: Range) -> Self {
        Self { spans: vec![range] }
    }

    /// Nothing is left out: no window at all, or one that is open at both ends.
    pub fn is_open(&self) -> bool {
        self.spans.is_empty() || self.spans.iter().any(Range::is_open)
    }

    /// The SQL that keeps the rows inside any one of the windows.
    pub fn where_sql(&self) -> String {
        if self.is_open() {
            return String::new();
        }
        let parts: Vec<String> = self
            .spans
            .iter()
            .filter_map(|r| match (r.from, r.until) {
                (Some(a), Some(b)) => Some(format!("(pk_TimeStamp >= {a} AND pk_TimeStamp <= {b})")),
                (Some(a), None) => Some(format!("(pk_TimeStamp >= {a})")),
                (None, Some(b)) => Some(format!("(pk_TimeStamp <= {b})")),
                (None, None) => None,
            })
            .collect();
        match parts.len() {
            0 => String::new(),
            _ => format!(" WHERE ({})", parts.join(" OR ")),
        }
    }
}

/// The SQL that restricts a value table to the chosen windows, shared with
/// the charts.
pub fn range_where(windows: &Windows) -> String {
    windows.where_sql()
}

/// Milliseconds since 1970 -> FILETIME ticks, the inverse of the chart's clock.
pub fn ms_to_ticks(ms: i64) -> i64 {
    (ms + 11_644_473_600_000) * 10_000
}

/// How much data a time window would keep. Drives the live preview in the window.
#[derive(Clone, Debug, Default)]
pub struct Kept {
    pub rows: u64,
    pub total_rows: u64,
    /// First and last instant that survives the range, as FILETIME ticks.
    pub span: Option<(i64, i64)>,
}

/// Count what a time window would keep, without writing anything.
pub fn count_kept(values_db: &Path, windows: &Windows) -> Result<Kept, String> {
    let conn = open_readonly(values_db)?;
    let present = table_names(&conn)?;
    let used: Vec<&str> = VALUE_TABLES
        .iter()
        .copied()
        .filter(|t| present.contains(*t))
        .collect();
    if used.is_empty() {
        return Err(format!("keine Tabelle {MAIN_VALUE_TABLE}"));
    }

    let total_rows: u64 = used.iter().map(|t| count(&conn, t)).sum();
    let where_sql = windows.where_sql();
    let inner = used
        .iter()
        .map(|t| format!("SELECT pk_TimeStamp FROM \"{t}\"{where_sql}"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let sql =
        format!("SELECT COUNT(*), MIN(pk_TimeStamp), MAX(pk_TimeStamp) FROM ({inner})");

    let (rows, lo, hi) = conn
        .query_row(&sql, [], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    Ok(Kept {
        rows,
        total_rows,
        span: match (lo, hi) {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        },
    })
}

// ---------------------------------------------------------------- columns

/// A column of the long table. The user picks which of these are written and
/// in what order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    /// The readable instant, `18.09.2026 06:03:48.7883740`.
    Time,
    /// The untouched FILETIME number the database stores.
    TimeRaw,
    /// The tag number the value belongs to.
    Id,
    /// The logged value.
    Value,
    /// The name of the tag.
    Name,
    /// 192 means good; anything else is doubtful.
    Quality,
    /// Which of the imported databases the row was read from.
    File,
}

impl Column {
    /// Every column there is, in the order they are written by default.
    pub const ALL: [Self; 7] = [
        Self::Time,
        Self::TimeRaw,
        Self::Id,
        Self::Value,
        Self::Name,
        Self::Quality,
        Self::File,
    ];

    /// The columns written unless the user says otherwise. The source file is
    /// left out, because most exports come from one file and then the column
    /// would say the same thing on every row.
    pub const DEFAULT: [Self; 6] = [
        Self::Time,
        Self::TimeRaw,
        Self::Id,
        Self::Value,
        Self::Name,
        Self::Quality,
    ];

    /// The heading written into the CSV.
    pub fn header(self) -> &'static str {
        match self {
            Self::Time => "pk_TimeStamp",
            Self::TimeRaw => "pk_TimeStamp_Raw",
            Self::Id => "pk_fk_Id",
            Self::Value => "Value",
            Self::Name => "Name",
            Self::Quality => "Quality",
            Self::File => "SourceFile",
        }
    }

    /// What it holds, in words, for the chooser.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Time => "Zeit, als 18.09.2026 06:03:48.7883740",
            Self::TimeRaw => "die unveränderte Zahl aus der Datenbank",
            Self::Id => "Tag-Nummer",
            Self::Value => "der protokollierte Wert",
            Self::Name => "Tag-Name",
            Self::Quality => "192 ist gut, alles andere zweifelhaft",
            Self::File => "die Datenbank, aus der die Zeile stammt",
        }
    }

    /// The short spelling accepted on the command line.
    pub fn key(self) -> &'static str {
        match self {
            Self::Time => "time",
            Self::TimeRaw => "raw",
            Self::Id => "id",
            Self::Value => "value",
            Self::Name => "name",
            Self::Quality => "quality",
            Self::File => "file",
        }
    }

    /// Read back a [`Column::key`], or a full heading.
    pub fn parse(text: &str) -> Option<Self> {
        let t = text.trim().to_lowercase();
        Self::ALL
            .into_iter()
            .find(|c| c.key() == t || c.header().to_lowercase() == t)
    }
}

/// A column of the long table as it will be written: where the data comes
/// from, and the heading the user put on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutColumn {
    pub source: Column,
    pub header: String,
}

impl OutColumn {
    /// A column under its original heading.
    pub fn new(source: Column) -> Self {
        Self {
            source,
            header: source.header().to_string(),
        }
    }

    /// The columns written unless the user says otherwise.
    pub fn all() -> Vec<Self> {
        Column::DEFAULT.into_iter().map(Self::new).collect()
    }
}

// ---------------------------------------------------------------- job

pub struct Job {
    /// Every database that holds tag names. They are merged; may be empty, and
    /// then tags are written as `id_<number>`.
    pub tag_dbs: Vec<PathBuf>,
    /// Every database that holds logged values, merged into one time order.
    pub value_dbs: Vec<PathBuf>,
    pub out_dir: PathBuf,
    pub base_name: String,
    pub semicolon: bool,
    pub write_long: bool,
    pub write_wide: bool,
    /// Only values inside one of these windows are written.
    pub windows: Windows,
    /// Shorten tag names to the part that tells them apart.
    pub shorten_names: bool,
    /// The columns of the long table, in the order they are written.
    pub columns: Vec<OutColumn>,
}

#[derive(Debug)]
pub enum Progress {
    Stage(String),
    Rows { done: u64, total: u64 },
    Finished(Box<Summary>),
    Failed(String),
}

#[derive(Debug, Default, Clone)]
pub struct Summary {
    /// Written file and how many data rows it holds.
    pub files: Vec<(PathBuf, u64)>,
    /// Each imported value database and how many of its rows were written, so
    /// the finished screen can account for every file.
    pub sources: Vec<(PathBuf, u64)>,
    pub rows: u64,
    pub timestamps: u64,
    pub tags_used: usize,
    pub tags_total: usize,
    pub unknown_ids: Vec<i64>,
    pub bad_quality: u64,
    /// First and last instant actually written, as FILETIME ticks.
    pub span: Option<(i64, i64)>,
    pub secs: f32,
}

// ------------------------------------------------- reading several databases

/// One logged value, as it travels from a reader thread to the writer.
struct ValueRow {
    ts: i64,
    id: i64,
    quality: i64,
    value: String,
}

/// How many rows a reader thread hands over at a time.
const MERGE_BATCH: usize = 4096;
/// How many batches may wait per file before its reader has to slow down.
const MERGE_AHEAD: usize = 3;

/// The logged values of any number of databases, in one global time order.
///
/// An HMI logs into a new file every so often, so a day of history arrives as
/// a row of files. Each is read on its own thread -- they are separate files,
/// so the reading overlaps -- and the rows are merged here. Everything below
/// therefore stays a single sorted pass, whatever the number of files.
struct ValueStream {
    feeds: Vec<Feed>,
}

struct Feed {
    rx: std::sync::mpsc::Receiver<Result<Vec<ValueRow>, String>>,
    batch: std::vec::IntoIter<ValueRow>,
    /// The next row of this file, once it is known.
    head: Option<ValueRow>,
    ended: bool,
}

impl Feed {
    /// Make sure `head` holds this file's next row, if it has one left.
    fn fill(&mut self) -> Result<(), String> {
        if self.head.is_some() {
            return Ok(());
        }
        loop {
            if let Some(row) = self.batch.next() {
                self.head = Some(row);
                return Ok(());
            }
            if self.ended {
                return Ok(());
            }
            match self.rx.recv() {
                Ok(Ok(batch)) => self.batch = batch.into_iter(),
                Ok(Err(e)) => {
                    self.ended = true;
                    return Err(e);
                }
                // The thread finished and dropped its end of the channel.
                Err(_) => {
                    self.ended = true;
                    return Ok(());
                }
            }
        }
    }
}

impl ValueStream {
    /// Start one reader per database. Dropping the stream stops them all,
    /// because their sends then fail.
    fn open(dbs: &[PathBuf], windows: &Windows, cancel: &Arc<AtomicBool>) -> Self {
        let mut feeds = Vec::with_capacity(dbs.len());
        for db in dbs {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<ValueRow>, String>>(
                MERGE_AHEAD,
            );
            let path = db.clone();
            let cancel = cancel.clone();
            let windows = windows.clone();
            std::thread::spawn(move || {
                if let Err(e) = read_one(&path, &windows, &cancel, &tx) {
                    let _ = tx.send(Err(format!("{}: {e}", short_name(&path))));
                }
            });
            feeds.push(Feed {
                rx,
                batch: Vec::new().into_iter(),
                head: None,
                ended: false,
            });
        }
        Self { feeds }
    }

    /// The earliest row not yet handed out, and which file it came from.
    fn next(&mut self) -> Result<Option<(usize, ValueRow)>, String> {
        for feed in &mut self.feeds {
            feed.fill()?;
        }
        let mut best: Option<usize> = None;
        for (i, feed) in self.feeds.iter().enumerate() {
            let Some(head) = &feed.head else { continue };
            let better = match best {
                None => true,
                Some(b) => match &self.feeds[b].head {
                    Some(h) => (head.ts, head.id) < (h.ts, h.id),
                    None => true,
                },
            };
            if better {
                best = Some(i);
            }
        }
        Ok(best.and_then(|i| self.feeds[i].head.take().map(|row| (i, row))))
    }
}

/// One file's rows, sorted and sent off in batches.
fn read_one(
    path: &Path,
    windows: &Windows,
    cancel: &Arc<AtomicBool>,
    tx: &std::sync::mpsc::SyncSender<Result<Vec<ValueRow>, String>>,
) -> Result<(), String> {
    let conn = open_readonly(path)?;
    let present = table_names(&conn)?;
    let used: Vec<&str> = VALUE_TABLES
        .iter()
        .copied()
        .filter(|t| present.contains(*t))
        .collect();
    if used.is_empty() {
        return Err(format!("enthält keine Tabelle {MAIN_VALUE_TABLE}"));
    }

    let where_sql = windows.where_sql();
    let sql = used
        .iter()
        .map(|t| format!("SELECT pk_TimeStamp, pk_fk_Id, Quality, Value FROM \"{t}\"{where_sql}"))
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
        + " ORDER BY pk_TimeStamp, pk_fk_Id";
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;

    let mut batch: Vec<ValueRow> = Vec::with_capacity(MERGE_BATCH);
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        batch.push(ValueRow {
            ts: row.get(0).map_err(|e| e.to_string())?,
            id: row.get(1).map_err(|e| e.to_string())?,
            quality: row.get(2).unwrap_or(-1),
            value: text_of(row.get_ref(3).map_err(|e| e.to_string())?),
        });
        if batch.len() == MERGE_BATCH && tx.send(Ok(std::mem::take(&mut batch))).is_err() {
            // Nobody is listening any more: the conversion was cancelled.
            return Ok(());
        }
        if batch.capacity() < MERGE_BATCH {
            batch.reserve(MERGE_BATCH);
        }
    }
    if !batch.is_empty() {
        let _ = tx.send(Ok(batch));
    }
    Ok(())
}

/// Whatever the column holds, as the text the CSV will carry.
fn text_of(value: rusqlite::types::ValueRef<'_>) -> String {
    match value {
        rusqlite::types::ValueRef::Null => String::new(),
        rusqlite::types::ValueRef::Integer(i) => i.to_string(),
        rusqlite::types::ValueRef::Real(f) => f.to_string(),
        rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        rusqlite::types::ValueRef::Blob(b) => {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("0x");
            for byte in b {
                s.push_str(&format!("{byte:02x}"));
            }
            s
        }
    }
}

/// Just the file name, for messages about one of many files.
pub fn short_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// How many rows one database holds inside the window, and which tag ids
/// occur in it.
fn survey(path: &Path, windows: &Windows) -> Result<(u64, BTreeSet<i64>), String> {
    let conn = open_readonly(path)?;
    let present = table_names(&conn)?;
    let used: Vec<&str> = VALUE_TABLES
        .iter()
        .copied()
        .filter(|t| present.contains(*t))
        .collect();
    if used.is_empty() {
        return Err(format!(
            "{} holds no {MAIN_VALUE_TABLE} table",
            short_name(path)
        ));
    }
    let where_sql = windows.where_sql();
    let mut rows = 0u64;
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for t in &used {
        rows += conn
            .query_row(
                &format!("SELECT COUNT(*) FROM \"{t}\"{where_sql}"),
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u64)
            .unwrap_or(0);
        let mut stmt = conn
            .prepare(&format!("SELECT DISTINCT pk_fk_Id FROM \"{t}\"{where_sql}"))
            .map_err(|e| e.to_string())?;
        let mut found = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(r) = found.next().map_err(|e| e.to_string())? {
            ids.insert(r.get(0).map_err(|e| e.to_string())?);
        }
    }
    Ok((rows, ids))
}

/// Append one CSV field, quoting it only when it has to be quoted.
fn field(out: &mut String, s: &str, delim: char) {
    if s.contains(delim) || s.contains('"') || s.contains('\n') || s.contains('\r') {
        out.push('"');
        for ch in s.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
}

/// Run the whole conversion, reporting progress as it goes.
pub fn run(job: &Job, tx: &Sender<Progress>, cancel: &Arc<AtomicBool>) -> Result<Summary, String> {
    let started = Instant::now();
    let delim = if job.semicolon { ';' } else { ',' };
    let say = |s: &str| {
        let _ = tx.send(Progress::Stage(s.to_string()));
    };

    if job.value_dbs.is_empty() {
        return Err("Keine Datenbank mit Messwerten ausgewählt".into());
    }

    // -- 1. tag names ------------------------------------------------------
    say("Tag-Namen lesen");
    let listed = read_all_tag_names(&job.tag_dbs)?;
    let tags_total = listed.len();
    let names: HashMap<i64, String> = if job.shorten_names {
        shorten_tag_names(&listed)
    } else {
        listed.into_iter().collect()
    };

    // -- 2. find out what the chosen databases actually hold ----------------
    say(&format!(
        "{} {} durchsuchen",
        job.value_dbs.len(),
        if job.value_dbs.len() == 1 {
            "Datenbank"
        } else {
            "Datenbanken"
        }
    ));
    // Every tag id that really occurs in the window, across all the files:
    // this is the column order of the wide table.
    let mut id_set: BTreeSet<i64> = BTreeSet::new();
    let mut total: u64 = 0;
    for db in &job.value_dbs {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }
        let (rows, ids) = survey(db, &job.windows)?;
        total += rows;
        id_set.extend(ids);
    }
    if total == 0 {
        return Err(if job.windows.is_open() {
            "Die gewählten Datenbanken enthalten keine Messwerte".into()
        } else {
            "Im gewählten Zeitraum liegen keine Messwerte".to_string()
        });
    }
    let ids: Vec<i64> = id_set.into_iter().collect();
    let col_of: HashMap<i64, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let unknown_ids: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|id| !names.contains_key(id))
        .collect();
    let header_name = |id: i64| -> String {
        names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("id_{id}"))
    };

    // -- 3. output files ---------------------------------------------------
    fs::create_dir_all(&job.out_dir).map_err(|e| format!("Ausgabeordner kann nicht angelegt werden: {e}"))?;
    let long_path = job.out_dir.join(format!("{}_long.csv", job.base_name));
    let wide_path = job.out_dir.join(format!("{}_wide.csv", job.base_name));

    if job.write_long && job.columns.is_empty() {
        return Err("Die lange Tabelle braucht mindestens eine Spalte".into());
    }
    let mut long = if job.write_long {
        let f = fs::File::create(&long_path)
            .map_err(|e| format!("{} kann nicht geschrieben werden: {e}", long_path.display()))?;
        Some(BufWriter::with_capacity(1 << 20, f))
    } else {
        None
    };
    let mut wide = if job.write_wide {
        let f = fs::File::create(&wide_path)
            .map_err(|e| format!("{} kann nicht geschrieben werden: {e}", wide_path.display()))?;
        Some(BufWriter::with_capacity(1 << 20, f))
    } else {
        None
    };

    let mut line = String::with_capacity(256);
    if let Some(w) = long.as_mut() {
        line.clear();
        for (i, c) in job.columns.iter().enumerate() {
            if i > 0 {
                line.push(delim);
            }
            field(&mut line, &c.header, delim);
        }
        line.push('\n');
        w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    }
    if let Some(w) = wide.as_mut() {
        line.clear();
        field(&mut line, "pk_TimeStamp", delim);
        line.push(delim);
        field(&mut line, "pk_TimeStamp_Raw", delim);
        for id in &ids {
            line.push(delim);
            field(&mut line, &header_name(*id), delim);
        }
        line.push('\n');
        w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    }

    // -- 4. one sorted pass over every chosen database ----------------------
    say("Umwandeln");
    let source_names: Vec<String> = job.value_dbs.iter().map(|p| short_name(p)).collect();
    let mut from_file: Vec<u64> = vec![0; job.value_dbs.len()];
    let mut stream = ValueStream::open(&job.value_dbs, &job.windows, cancel);

    let mut done: u64 = 0;
    let mut timestamps: u64 = 0;
    let mut bad_quality: u64 = 0;
    let mut first_ts: Option<i64> = None;
    let mut last_ts: i64 = 0;

    // The wide table is streamed as well: rows arrive sorted by timestamp, so a
    // group is complete as soon as a different timestamp shows up.
    let mut bucket: Vec<String> = vec![String::new(); ids.len()];
    let mut bucket_ts: Option<(i64, String)> = None;

    while let Some((src, row)) = stream.next()? {
        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let ValueRow {
            ts,
            id,
            quality,
            value,
        } = row;
        from_file[src] += 1;

        if quality != QUALITY_GOOD {
            bad_quality += 1;
        }
        if first_ts.is_none() {
            first_ts = Some(ts);
        }
        last_ts = ts;

        // A timestamp is shared by ~100 consecutive rows, so format it once.
        let iso = match &bucket_ts {
            Some((t, iso)) if *t == ts => iso.clone(),
            _ => filetime_to_text(ts),
        };

        if let Some(w) = long.as_mut() {
            line.clear();
            for (i, c) in job.columns.iter().enumerate() {
                if i > 0 {
                    line.push(delim);
                }
                // Only the chosen columns are built, so leaving one out saves
                // the work of producing it as well as the space.
                match c.source {
                    Column::Time => field(&mut line, &iso, delim),
                    Column::TimeRaw => field(&mut line, &ts.to_string(), delim),
                    Column::Id => field(&mut line, &id.to_string(), delim),
                    Column::Value => field(&mut line, &value, delim),
                    Column::Name => field(&mut line, &header_name(id), delim),
                    Column::Quality => field(&mut line, &quality.to_string(), delim),
                    Column::File => field(&mut line, &source_names[src], delim),
                }
            }
            line.push('\n');
            w.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        }

        match &bucket_ts {
            Some((t, _)) if *t == ts => {}
            Some(prev) => {
                if let Some(w) = wide.as_mut() {
                    flush_bucket(w, &mut line, &mut bucket, prev, delim)?;
                }
                timestamps += 1;
                bucket_ts = Some((ts, iso));
            }
            None => bucket_ts = Some((ts, iso)),
        }
        if let Some(col) = col_of.get(&id) {
            bucket[*col] = value;
        }

        done += 1;
        if done.is_multiple_of(8192) {
            let _ = tx.send(Progress::Rows { done, total });
        }
    }

    if let Some(prev) = &bucket_ts {
        if let Some(w) = wide.as_mut() {
            flush_bucket(w, &mut line, &mut bucket, prev, delim)?;
        }
        timestamps += 1;
    }

    say("Dateien schreiben");
    let mut files = Vec::new();
    if let Some(mut w) = long {
        w.flush().map_err(|e| e.to_string())?;
        files.push((long_path, done));
    }
    if let Some(mut w) = wide {
        w.flush().map_err(|e| e.to_string())?;
        files.push((wide_path, timestamps));
    }
    let _ = tx.send(Progress::Rows { done, total });

    Ok(Summary {
        files,
        sources: job
            .value_dbs
            .iter()
            .cloned()
            .zip(from_file.iter().copied())
            .collect(),
        rows: done,
        timestamps,
        tags_used: ids.len(),
        tags_total,
        unknown_ids,
        bad_quality,
        span: first_ts.map(|f| (f, last_ts)),
        secs: started.elapsed().as_secs_f32(),
    })
}

/// Write one finished timestamp group of the wide table and reset the buffer.
fn flush_bucket(
    w: &mut BufWriter<fs::File>,
    line: &mut String,
    bucket: &mut [String],
    ts: &(i64, String),
    delim: char,
) -> Result<(), String> {
    line.clear();
    field(line, &ts.1, delim);
    line.push(delim);
    field(line, &ts.0.to_string(), delim);
    for cell in bucket.iter_mut() {
        line.push(delim);
        field(line, cell, delim);
        cell.clear();
    }
    line.push('\n');
    w.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_matches_known_points() {
        // 1601-01-01 is tick zero.
        assert_eq!(filetime_to_text(0), "01.01.1601 00:00:00.0000000");
        // 1970-01-01, the Unix epoch.
        assert_eq!(
            filetime_to_text(FILETIME_EPOCH_DIFF_SECS * TICKS_PER_SEC),
            "01.01.1970 00:00:00.0000000"
        );
        // A real row out of Values.db3.
        assert_eq!(
            filetime_to_text(134_341_850_287_883_740),
            "18.09.2026 06:03:48.7883740"
        );
        // A leap day, to exercise the calendar maths.
        assert_eq!(
            filetime_to_text(133_536_384_000_000_000),
            "29.02.2024 00:00:00.0000000"
        );
    }

    #[test]
    fn typed_dates_round_trip() {
        let day = parse_when("2026-09-18", false).unwrap();
        assert_eq!(filetime_to_text(day), "18.09.2026 00:00:00.0000000");

        // The same day written the German way.
        assert_eq!(parse_when("18.09.2026", false).unwrap(), day);

        // An upper bound given as a bare date covers the whole day.
        assert_eq!(
            filetime_to_text(parse_when("2026-09-18", true).unwrap()),
            "18.09.2026 23:59:59.9999999"
        );

        for (text, want) in [
            ("18.09.2026 06:30", "18.09.2026 06:30:00.0000000"),
            ("2026-09-18 06:30", "18.09.2026 06:30:00.0000000"),
            ("18.09.2026 06:30:45", "18.09.2026 06:30:45.0000000"),
            ("2026-09-18T06:30:45", "18.09.2026 06:30:45.0000000"),
            ("18.09.2026 06:30:45,123", "18.09.2026 06:30:45.1230000"),
            (
                "2026-09-18T06:03:48.7883740Z",
                "18.09.2026 06:03:48.7883740",
            ),
        ] {
            assert_eq!(
                filetime_to_text(parse_when(text, false).unwrap()),
                want,
                "{text}"
            );
        }

        // Raw ticks, as copied out of the pk_TimeStamp_Raw column.
        assert_eq!(
            parse_when("134341850287883740", false).unwrap(),
            134_341_850_287_883_740
        );

        for bad in [
            "",
            "yesterday",
            "2026-13-01",
            "2026-09-18 25:00",
            "18/09/2026",
        ] {
            assert!(parse_when(bad, false).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn trimming_leaves_the_part_that_differs() {
        let names: Vec<String> = [
            "Anlage1::Zustand_DruckvorPumpe2:Archivvariable_1",
            "Anlage1::Zustand_DrucknachPumpe2:Archivvariable_1",
            "Anlage1::Parameter_SollwertDruck:Archivvariable_1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let once = trim_common(&names);
        assert_eq!(once[0], "Zustand_DruckvorPumpe2");
        assert_eq!(once[1], "Zustand_DrucknachPumpe2");
        assert_eq!(once[2], "Parameter_SollwertDruck");

        // Doing it again must not eat into the names.
        assert_eq!(trim_common(&once), once);
    }

    #[test]
    fn tag_names_shorten_to_the_telling_part() {
        let tags: Vec<(i64, String)> = [
            "Anlage1::Zustand_DruckvorPumpe2:Archivvariable_1",
            "Anlage1::Zustand_IstwertVentilPumpe2:Archivvariable_1",
            "Anlage1::Zustand_DrucknachPumpe2:Archivvariable_1",
            "Anlage1::Zustand_IstwertVentilPumpe1:Archivvariable_1",
            "Anlage1::Parameter_SollwertDruck:Archivvariable_1",
            "Anlage1::Fehler1:Archivvariable_1",
            "Anlage1::Zustand_Temp_P1_LagerOben:Archivvariable_1",
        ]
        .iter()
        .enumerate()
        .map(|(i, n)| (i as i64 + 1, n.to_string()))
        .collect();

        let short = shorten_tag_names(&tags);
        assert_eq!(short[&1], "DruckvorPumpe2");
        assert_eq!(short[&2], "IstwertVentilPumpe2");
        assert_eq!(short[&3], "DrucknachPumpe2");
        assert_eq!(short[&4], "IstwertVentilPumpe1");
        assert_eq!(short[&5], "SollwertDruck");
        // A name without a category keeps all it has.
        assert_eq!(short[&6], "Fehler1");
        // Only the leading category goes; the rest of the path stays.
        assert_eq!(short[&7], "Temp_P1_LagerOben");
    }

    #[test]
    fn shortening_never_merges_two_tags() {
        // Same name under two categories: both must stay distinguishable.
        let tags: Vec<(i64, String)> = [
            (1, "Anlage1::Zustand_SollwertHand:Archivvariable_1"),
            (2, "Anlage1::Parameter_SollwertHand:Archivvariable_1"),
            (3, "Anlage1::Zustand_Druck:Archivvariable_1"),
        ]
        .iter()
        .map(|(i, n)| (*i, n.to_string()))
        .collect();

        let short = shorten_tag_names(&tags);
        assert_eq!(short[&1], "Zustand_SollwertHand");
        assert_eq!(short[&2], "Parameter_SollwertHand");
        assert_eq!(short[&3], "Druck");
        assert_ne!(short[&1], short[&2]);
    }

    #[test]
    fn a_single_tag_keeps_its_name() {
        let one = vec![(1, "Anlage1::Zustand_Druck:Archivvariable_1".to_string())];
        // Nothing to compare against, so only the category is dropped.
        assert_eq!(
            shorten_tag_names(&one)[&1],
            "Anlage1::Zustand_Druck:Archivvariable_1"
        );
    }

    #[test]
    fn columns_are_named_both_ways() {
        // Every column is reachable by its short key and by its heading.
        for c in Column::ALL {
            assert_eq!(Column::parse(c.key()), Some(c), "key of {c:?}");
            assert_eq!(Column::parse(c.header()), Some(c), "header of {c:?}");
            assert_eq!(
                Column::parse(&c.header().to_uppercase()),
                Some(c),
                "case of {c:?}"
            );
        }
        assert_eq!(Column::parse("pk_fk_id"), Some(Column::Id));
        assert_eq!(Column::parse("  value  "), Some(Column::Value));
        assert_eq!(Column::parse("nonsense"), None);

        // The headings are what the CSV has always used.
        let all: Vec<&str> = Column::ALL.iter().map(|c| c.header()).collect();
        assert_eq!(
            all,
            [
                "pk_TimeStamp",
                "pk_TimeStamp_Raw",
                "pk_fk_Id",
                "Value",
                "Name",
                "Quality",
                "SourceFile",
            ]
        );
        // The source file is offered but not written unless it is asked for,
        // so a single-file export reads exactly as it always did.
        let default: Vec<&str> = Column::DEFAULT.iter().map(|c| c.header()).collect();
        assert_eq!(default, all[..6]);
        assert!(!Column::DEFAULT.contains(&Column::File));
    }

    #[test]
    fn fields_are_quoted_only_when_needed() {
        let mut s = String::new();
        field(&mut s, "plain", ',');
        assert_eq!(s, "plain");

        s.clear();
        field(&mut s, "a,b", ',');
        assert_eq!(s, "\"a,b\"");

        s.clear();
        field(&mut s, "a,b", ';');
        assert_eq!(s, "a,b");

        s.clear();
        field(&mut s, "say \"hi\"", ',');
        assert_eq!(s, "\"say \"\"hi\"\"\"");
    }
}
