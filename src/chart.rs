//! Building the chart page.
//!
//! The values are read out of the database, thinned to a size a browser is
//! happy with, and written into one self-contained HTML file: the ECharts
//! library is baked into the program and copied into the page, so the chart
//! opens with no network and no installed anything.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Instant;

use crate::convert::{self, Progress, Windows};

/// The charting library, kept beside the program so nothing is fetched.
const ECHARTS: &str = include_str!("../assets/echarts.min.js");
/// The page around it.
const TEMPLATE: &str = include_str!("../assets/chart.html");

/// Windows FILETIME ticks -> milliseconds since 1970, which is what the time
/// axis of the chart expects.
fn ticks_to_ms(ticks: i64) -> i64 {
    ticks / 10_000 - 11_644_473_600_000
}

/// How much of the data ends up in the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detail {
    /// Every point. Truthful, and a heavy file for a long log.
    Full,
    /// At most a few thousand points per tag: the same shape, far lighter.
    Balanced,
    /// Enough to see the trend, and quick on a tired machine.
    Light,
}

impl Detail {
    /// The most points one tag may contribute, if there is a limit.
    fn cap(self) -> Option<usize> {
        match self {
            Self::Full => None,
            Self::Balanced => Some(4000),
            Self::Light => Some(1000),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Balanced => "Balanced",
            Self::Light => "Light",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Self::Full => "every point, largest file",
            Self::Balanced => "up to 4000 points per tag",
            Self::Light => "up to 1000 points per tag",
        }
    }
}

pub struct ChartJob {
    pub tag_dbs: Vec<PathBuf>,
    pub value_dbs: Vec<PathBuf>,
    pub out_dir: PathBuf,
    pub base_name: String,
    pub windows: Windows,
    pub shorten_names: bool,
    pub detail: Detail,
}

#[derive(Debug, Default)]
pub struct ChartSummary {
    pub path: PathBuf,
    /// Points read, and points that made it into the page.
    pub read: u64,
    pub drawn: u64,
    pub tags: usize,
    /// Tags whose values are text, which cannot go on an axis.
    pub skipped_text: usize,
    pub bytes: u64,
    pub secs: f32,
}

/// One tag and its readings, in milliseconds since 1970.
pub struct Series {
    pub name: String,
    pub points: Vec<(i64, f64)>,
}

/// Everything read out of the databases, ready to be drawn either way.
pub struct Loaded {
    pub series: Vec<Series>,
    /// Points read, and points left after thinning.
    pub read: u64,
    pub drawn: u64,
    /// Tags whose values are text, which cannot go on an axis.
    pub skipped_text: usize,
    pub first_ms: i64,
    pub last_ms: i64,
}

/// Largest-Triangle-Three-Buckets: drop points but keep the shape, so a thinned
/// line still shows its peaks instead of averaging them away.
fn lttb(points: &[(i64, f64)], target: usize) -> Vec<(i64, f64)> {
    if target < 3 || points.len() <= target {
        return points.to_vec();
    }
    let mut out = Vec::with_capacity(target);
    out.push(points[0]);

    // The first and last point are kept, the rest share the buckets between.
    let every = (points.len() - 2) as f64 / (target - 2) as f64;
    let mut a = 0usize;

    for i in 0..(target - 2) {
        // The average of the next bucket, which the triangle reaches towards.
        let next_lo = ((i + 1) as f64 * every) as usize + 1;
        let next_hi = (((i + 2) as f64 * every) as usize + 1).min(points.len());
        let (mut sum_x, mut sum_y) = (0f64, 0f64);
        let count = (next_hi.saturating_sub(next_lo)).max(1);
        for p in &points[next_lo.min(points.len())..next_hi] {
            sum_x += p.0 as f64;
            sum_y += p.1;
        }
        let (avg_x, avg_y) = (sum_x / count as f64, sum_y / count as f64);

        let lo = (i as f64 * every) as usize + 1;
        let hi = (((i + 1) as f64 * every) as usize + 1).min(points.len());
        let (ax, ay) = (points[a].0 as f64, points[a].1);

        let mut best = lo;
        let mut best_area = -1.0;
        for (k, p) in points[lo..hi].iter().enumerate() {
            let area = ((ax - avg_x) * (p.1 - ay) - (ax - p.0 as f64) * (avg_y - ay)).abs();
            if area > best_area {
                best_area = area;
                best = lo + k;
            }
        }
        if best < points.len() {
            out.push(points[best]);
            a = best;
        }
    }

    out.push(points[points.len() - 1]);
    out
}

/// Write `s` into a JSON string, quotes and all.
fn json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Read the values out of the databases and thin them to the chosen detail.
pub fn load(
    job: &ChartJob,
    tx: &Sender<Progress>,
    cancel: &Arc<AtomicBool>,
) -> Result<Loaded, String> {
    let say = |s: &str| {
        let _ = tx.send(Progress::Stage(s.to_string()));
    };

    if job.value_dbs.is_empty() {
        return Err("no database with logged values was chosen".into());
    }

    // -- names -------------------------------------------------------------
    say("Reading tag names");
    let listed = convert::read_all_tag_names(&job.tag_dbs)?;
    let names = if job.shorten_names {
        convert::shorten_tag_names(&listed)
    } else {
        listed.into_iter().collect()
    };

    // -- values ------------------------------------------------------------
    // One file at a time, each tag collecting its own points. Several files are
    // one log cut into pieces, so the points of a tag are put back together
    // and sorted once at the end.
    let where_sql = convert::range_where(&job.windows);
    let mut text_ids: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    let mut points: HashMap<i64, Vec<(i64, f64)>> = HashMap::new();
    let mut order: Vec<i64> = Vec::new();
    let mut total: u64 = 0;
    let mut read: u64 = 0;
    let (mut first_ms, mut last_ms) = (i64::MAX, i64::MIN);

    // What there is to read, so the progress bar means something.
    let mut plan: Vec<(PathBuf, Vec<&'static str>)> = Vec::new();
    for db in &job.value_dbs {
        let conn = convert::open_readonly(db)?;
        let present = convert::table_names(&conn)?;

        // Text readings have no place on an axis; they are counted and left out.
        if present.contains("LoggedProcessValueText") {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT DISTINCT pk_fk_Id FROM \"LoggedProcessValueText\"{where_sql}"
                ))
                .map_err(|e| e.to_string())?;
            let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
            while let Some(r) = rows.next().map_err(|e| e.to_string())? {
                text_ids.insert(r.get(0).map_err(|e| e.to_string())?);
            }
        }

        let numeric: Vec<&'static str> = ["LoggedProcessValue", "LoggedProcessValue64"]
            .into_iter()
            .filter(|t| present.contains(*t))
            .collect();
        for t in &numeric {
            total += conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM \"{t}\"{where_sql}"),
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n as u64)
                .unwrap_or(0);
        }
        plan.push((db.clone(), numeric));
    }
    if plan.iter().all(|(_, n)| n.is_empty()) {
        return Err("the chosen databases hold no numeric values to plot".into());
    }
    if total == 0 {
        return Err("no values to plot in the chosen time range".into());
    }

    for (db, numeric) in &plan {
        if numeric.is_empty() {
            continue;
        }
        say(&format!("Reading {}", convert::short_name(db)));
        let conn = convert::open_readonly(db)?;
        // Grouped by tag, so each tag's points arrive in one run and in order.
        let sql = numeric
            .iter()
            .map(|t| format!("SELECT pk_fk_Id, pk_TimeStamp, Value FROM \"{t}\"{where_sql}"))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
            + " ORDER BY pk_fk_Id, pk_TimeStamp";
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;

        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let id: i64 = row.get(0).map_err(|e| e.to_string())?;
            let ticks: i64 = row.get(1).map_err(|e| e.to_string())?;
            let value: f64 = match row.get_ref(2).map_err(|e| e.to_string())? {
                rusqlite::types::ValueRef::Real(f) => f,
                rusqlite::types::ValueRef::Integer(i) => i as f64,
                _ => continue,
            };
            if !value.is_finite() {
                continue;
            }

            let ms = ticks_to_ms(ticks);
            first_ms = first_ms.min(ms);
            last_ms = last_ms.max(ms);
            points
                .entry(id)
                .or_insert_with(|| {
                    order.push(id);
                    Vec::new()
                })
                .push((ms, value));

            read += 1;
            if read.is_multiple_of(8192) {
                let _ = tx.send(Progress::Rows { done: read, total });
            }
        }
    }
    let _ = tx.send(Progress::Rows { done: read, total });

    if points.is_empty() {
        return Err("no numeric values to plot".into());
    }

    // Tag numbers, so the series are in the same order whatever the files.
    order.sort_unstable();
    let several = plan.len() > 1;
    let mut series: Vec<Series> = Vec::with_capacity(order.len());
    for id in order {
        let mut tag_points = points.remove(&id).unwrap_or_default();
        // Pieces of one log overlap only at their seams, so this sort is
        // nearly free, and it costs nothing at all for a single file.
        if several {
            tag_points.sort_by_key(|(ms, _)| *ms);
        }
        series.push(Series {
            name: names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("id_{id}")),
            points: tag_points,
        });
    }
    let skipped_text = text_ids.len();

    // -- thin it out -------------------------------------------------------
    say("Preparing the chart");
    if let Some(cap) = job.detail.cap() {
        for s in &mut series {
            if s.points.len() > cap {
                s.points = lttb(&s.points, cap);
            }
        }
    }
    let drawn: u64 = series.iter().map(|s| s.points.len() as u64).sum();

    Ok(Loaded {
        series,
        read,
        drawn,
        skipped_text,
        first_ms,
        last_ms,
    })
}

/// Read the values and write the self-contained page.
pub fn build(
    job: &ChartJob,
    tx: &Sender<Progress>,
    cancel: &Arc<AtomicBool>,
) -> Result<ChartSummary, String> {
    let started = Instant::now();
    let Loaded {
        series,
        read,
        drawn,
        skipped_text,
        first_ms,
        last_ms,
    } = load(job, tx, cancel)?;

    let _ = tx.send(Progress::Stage("Writing the page".into()));
    let mut payload = String::with_capacity(drawn as usize * 18 + 1024);
    payload.push_str("{\"from\":");
    payload.push_str(&first_ms.to_string());
    payload.push_str(",\"to\":");
    payload.push_str(&last_ms.to_string());

    payload.push_str(",\"meta\":");
    let span = format!(
        "{}  to  {}",
        convert::filetime_to_short(convert::ms_to_ticks(first_ms)),
        convert::filetime_to_short(convert::ms_to_ticks(last_ms))
    );
    json_string(&mut payload, &format!("{} tags   {span}", series.len()));

    payload.push_str(",\"foot\":");
    let foot = format!(
        "{} of {} points drawn ({}) - {} - built by HMI Log Converter, \
         charts by Apache ECharts, all offline",
        drawn,
        read,
        job.detail.label(),
        match job.value_dbs.as_slice() {
            [one] => convert::short_name(one),
            many => format!("{} databases", many.len()),
        },
    );
    json_string(&mut payload, &foot);

    payload.push_str(",\"series\":[");
    for (i, s) in series.iter().enumerate() {
        if i > 0 {
            payload.push(',');
        }
        payload.push_str("{\"name\":");
        json_string(&mut payload, &s.name);
        payload.push_str(",\"data\":[");
        for (k, (ms, v)) in s.points.iter().enumerate() {
            if k > 0 {
                payload.push(',');
            }
            payload.push('[');
            payload.push_str(&ms.to_string());
            payload.push(',');
            payload.push_str(&format_value(*v));
            payload.push(']');
        }
        payload.push_str("]}");
    }
    payload.push_str("]}");

    let title = format!("{} - HMI log", job.base_name);
    let page = TEMPLATE
        .replace("__TITLE__", &html_escape(&title))
        .replace("/*__ECHARTS__*/", ECHARTS)
        .replace("/*__PAYLOAD__*/", &payload);

    fs::create_dir_all(&job.out_dir).map_err(|e| format!("cannot create output folder: {e}"))?;
    let path = job.out_dir.join(format!("{}_chart.html", job.base_name));
    fs::write(&path, &page).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    Ok(ChartSummary {
        bytes: page.len() as u64,
        path,
        read,
        drawn,
        tags: series.len(),
        skipped_text,
        secs: started.elapsed().as_secs_f32(),
    })
}

/// Numbers for the page: short where that loses nothing, because every digit
/// is repeated once per point.
fn format_value(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    let short = format!("{v:.6}");
    if short
        .parse::<f64>()
        .map(|b| (b - v).abs() < 1e-9)
        .unwrap_or(false)
    {
        let trimmed = short.trim_end_matches('0').trim_end_matches('.');
        return trimmed.to_string();
    }
    format!("{v}")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Open a finished page in the browser.
pub fn open(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        // `explorer` hands the file to whatever opens .html, and unlike `start`
        // it needs no shell.
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_becomes_browser_time() {
        // The first row of the sample export, 18.09.2026 06:03:48.788 UTC.
        assert_eq!(ticks_to_ms(134_341_850_287_883_740), 1_789_711_428_788);
        // And back again, to the millisecond.
        assert_eq!(
            convert::filetime_to_short(convert::ms_to_ticks(1_789_711_428_788)),
            "18.09.2026 06:03:48"
        );
    }

    #[test]
    fn thinning_keeps_the_ends_and_the_peak() {
        let mut points: Vec<(i64, f64)> = (0..1000).map(|i| (i as i64, 0.0)).collect();
        points[500].1 = 99.0; // one spike in the middle

        let thin = lttb(&points, 50);
        assert_eq!(thin.len(), 50);
        assert_eq!(thin[0], points[0]);
        assert_eq!(thin[thin.len() - 1], points[999]);
        // The spike is the whole point of the chart, so it must survive.
        assert!(
            thin.iter().any(|p| p.1 == 99.0),
            "the peak was thinned away"
        );
        // Timestamps stay in order.
        assert!(thin.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn small_series_are_left_alone() {
        let points: Vec<(i64, f64)> = (0..10).map(|i| (i as i64, i as f64)).collect();
        assert_eq!(lttb(&points, 50), points);
    }

    #[test]
    fn numbers_stay_exact() {
        for v in [0.0, 1.0, -5.0, 0.007_000_000_216_066_837, 1e-9, 12345.678] {
            let text = format_value(v);
            let back: f64 = text.parse().expect("should parse back");
            assert!(
                (back - v).abs() <= v.abs() * 1e-6 + 1e-12,
                "{v} became {text}"
            );
        }
        assert_eq!(format_value(5.0), "5");
        assert_eq!(format_value(0.5), "0.5");
    }

    #[test]
    fn json_strings_are_escaped() {
        let mut s = String::new();
        json_string(&mut s, r#"a"b\c"#);
        assert_eq!(s, r#""a\"b\\c""#);
    }

    #[test]
    fn the_library_is_embedded() {
        // If this is ever empty the page would silently draw nothing.
        assert!(ECHARTS.len() > 500_000, "ECharts is not embedded");
        assert!(TEMPLATE.contains("/*__ECHARTS__*/"));
        assert!(TEMPLATE.contains("/*__PAYLOAD__*/"));
        // Nothing may be fetched at view time.
        for remote in ["http://", "https://", "//cdn", "src=\"//"] {
            assert!(
                !TEMPLATE.contains(remote),
                "the page refers to {remote}, so it would not work offline"
            );
        }
    }
}
