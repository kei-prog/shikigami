use std::{
    collections::BTreeMap,
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

const FORMAT_VERSION: u8 = 1;
const ENABLE_ENV: &str = "SHI_PERF";
const LOG_FILE_NAME: &str = "performance-v1.jsonl";
const MAX_LOG_BYTES: usize = 1024 * 1024;
const MAX_SESSIONS: usize = 200;
const MAX_EVENTS_PER_SESSION: usize = 2048;
const FRAME_BUCKET_UPPER_MICROS: [u64; 7] = [1_000, 2_000, 4_000, 8_000, 16_000, 32_000, u64::MAX];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PerformanceEvent {
    pub metric: String,
    pub elapsed_micros: u64,
    pub duration_micros: u64,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct FrameSummary {
    pub render_buckets: [u64; 7],
    pub draw_buckets: [u64; 7],
    pub render_total_micros: u64,
    pub draw_total_micros: u64,
    pub render_max_micros: u64,
    pub draw_max_micros: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredSession {
    version: u8,
    started_at_unix_millis: u64,
    duration_millis: u64,
    events: Vec<PerformanceEvent>,
    frames: FrameSummary,
    #[serde(default)]
    dropped_events: u64,
}

#[derive(Default)]
struct AtomicHistogram {
    buckets: [AtomicU64; 7],
    total_micros: AtomicU64,
    max_micros: AtomicU64,
}

impl AtomicHistogram {
    fn record(&self, duration: Duration) {
        let micros = duration.as_micros().min(u64::MAX as u128) as u64;
        let index = FRAME_BUCKET_UPPER_MICROS
            .iter()
            .position(|upper| micros < *upper)
            .unwrap_or(FRAME_BUCKET_UPPER_MICROS.len() - 1);
        self.buckets[index].fetch_add(1, Ordering::Relaxed);
        self.total_micros.fetch_add(micros, Ordering::Relaxed);
        self.max_micros.fetch_max(micros, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ([u64; 7], u64, u64) {
        (
            std::array::from_fn(|index| self.buckets[index].load(Ordering::Relaxed)),
            self.total_micros.load(Ordering::Relaxed),
            self.max_micros.load(Ordering::Relaxed),
        )
    }
}

pub struct PerformanceSession {
    enabled: bool,
    started: Instant,
    started_at_unix_millis: u64,
    interactive: AtomicBool,
    events: Mutex<Vec<PerformanceEvent>>,
    dropped_events: AtomicU64,
    render_frames: AtomicHistogram,
    draw_frames: AtomicHistogram,
}

impl PerformanceSession {
    pub fn start() -> Arc<Self> {
        let value = env::var_os(ENABLE_ENV);
        Self::start_with_enabled(enabled_from_env_value(value.as_deref()))
    }

    fn start_with_enabled(enabled: bool) -> Arc<Self> {
        let started_at_unix_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        Arc::new(Self {
            enabled,
            started: Instant::now(),
            started_at_unix_millis,
            interactive: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
            dropped_events: AtomicU64::new(0),
            render_frames: AtomicHistogram::default(),
            draw_frames: AtomicHistogram::default(),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn start_timer(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub fn mark_interactive(&self) {
        self.interactive.store(true, Ordering::Relaxed);
    }

    pub fn record_elapsed(&self, metric: &str) {
        if !self.enabled {
            return;
        }
        let elapsed = self.started.elapsed();
        self.record_event(metric, elapsed, "success", &[]);
    }

    pub fn record_duration(
        &self,
        metric: &str,
        started: Option<Instant>,
        outcome: &str,
        tags: &[(&str, &str)],
    ) {
        if let Some(started) = started {
            self.record_event(metric, started.elapsed(), outcome, tags);
        }
    }

    pub fn record_value(
        &self,
        metric: &str,
        duration: Duration,
        outcome: &str,
        tags: &[(&str, &str)],
    ) {
        if !self.enabled {
            return;
        }
        self.record_event(metric, duration, outcome, tags);
    }

    fn record_event(&self, metric: &str, duration: Duration, outcome: &str, tags: &[(&str, &str)]) {
        let event = PerformanceEvent {
            metric: metric.to_owned(),
            elapsed_micros: self.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            duration_micros: duration.as_micros().min(u64::MAX as u128) as u64,
            outcome: outcome.to_owned(),
            tags: tags
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        };
        if let Ok(mut events) = self.events.lock() {
            if events.len() == MAX_EVENTS_PER_SESSION {
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
                return;
            }
            events.push(event);
        }
    }

    pub fn record_frame(&self, render: Duration, draw: Duration) {
        if !self.enabled {
            return;
        }
        self.render_frames.record(render);
        self.draw_frames.record(draw);
    }

    pub fn save(&self) -> Result<()> {
        if !self.enabled || !self.interactive.load(Ordering::Relaxed) {
            return Ok(());
        }
        let path = performance_path()?;
        self.save_to(&path)
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        let (render_buckets, render_total_micros, render_max_micros) =
            self.render_frames.snapshot();
        let (draw_buckets, draw_total_micros, draw_max_micros) = self.draw_frames.snapshot();
        let session = StoredSession {
            version: FORMAT_VERSION,
            started_at_unix_millis: self.started_at_unix_millis,
            duration_millis: self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            events: self
                .events
                .lock()
                .map(|events| events.iter().cloned().collect())
                .unwrap_or_default(),
            frames: FrameSummary {
                render_buckets,
                draw_buckets,
                render_total_micros,
                draw_total_micros,
                render_max_micros,
                draw_max_micros,
            },
            dropped_events: self.dropped_events.load(Ordering::Relaxed),
        };
        save_session(path, &session)
    }
}

fn enabled_from_env_value(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| value == "1")
}

pub fn print_report() -> Result<()> {
    let path = performance_path()?;
    let sessions = load_sessions(&path)?;
    if sessions.is_empty() {
        println!("No Shikigami performance sessions recorded yet");
        println!("Log: {}", path.display());
        return Ok(());
    }

    let mut samples: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut render_buckets = [0_u64; 7];
    let mut draw_buckets = [0_u64; 7];
    let mut render_max = 0;
    let mut draw_max = 0;
    let mut dropped_events = 0;
    for session in &sessions {
        dropped_events += session.dropped_events;
        for event in &session.events {
            if event.outcome != "success" {
                continue;
            }
            let mut label = event.metric.clone();
            for (key, value) in &event.tags {
                if matches!(key.as_str(), "method" | "kind" | "scope") {
                    label.push_str(&format!(" [{key}={value}]"));
                }
            }
            samples
                .entry(label)
                .or_default()
                .push(event.duration_micros);
        }
        for index in 0..7 {
            render_buckets[index] += session.frames.render_buckets[index];
            draw_buckets[index] += session.frames.draw_buckets[index];
        }
        render_max = render_max.max(session.frames.render_max_micros);
        draw_max = draw_max.max(session.frames.draw_max_micros);
    }

    println!("Shikigami performance ({} sessions)", sessions.len());
    println!(
        "{:<54} {:>7} {:>10} {:>10} {:>10}",
        "metric", "samples", "p50", "p95", "max"
    );
    for (label, mut values) in samples {
        values.sort_unstable();
        println!(
            "{:<54} {:>7} {:>10} {:>10} {:>10}",
            label,
            values.len(),
            format_duration(percentile(&values, 50)),
            format_duration(percentile(&values, 95)),
            format_duration(*values.last().unwrap_or(&0)),
        );
    }
    print_frame_row("frame.render", &render_buckets, render_max);
    print_frame_row("frame.draw", &draw_buckets, draw_max);
    if dropped_events > 0 {
        println!("Dropped {dropped_events} events from long-running sessions");
    }
    println!("Log: {}", path.display());
    Ok(())
}

fn print_frame_row(label: &str, buckets: &[u64; 7], max: u64) {
    let count = buckets.iter().sum::<u64>();
    if count == 0 {
        return;
    }
    println!(
        "{:<54} {:>7} {:>10} {:>10} {:>10}",
        label,
        count,
        format_duration(histogram_percentile(buckets, 50)),
        format_duration(histogram_percentile(buckets, 95)),
        format_duration(max),
    );
}

fn histogram_percentile(buckets: &[u64; 7], percentile: u64) -> u64 {
    let total = buckets.iter().sum::<u64>();
    if total == 0 {
        return 0;
    }
    let target = total.saturating_mul(percentile).div_ceil(100);
    let mut seen = 0;
    for (index, count) in buckets.iter().enumerate() {
        seen += count;
        if seen >= target {
            return FRAME_BUCKET_UPPER_MICROS[index];
        }
    }
    u64::MAX
}

fn percentile(values: &[u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let index = (values.len() * percentile).div_ceil(100).saturating_sub(1);
    values[index.min(values.len() - 1)]
}

fn format_duration(micros: u64) -> String {
    if micros == u64::MAX {
        ">=32ms".into()
    } else if micros >= 1_000 {
        format!("{:.1}ms", micros as f64 / 1_000.0)
    } else {
        format!("{micros}us")
    }
}

fn performance_path() -> Result<PathBuf> {
    Ok(paths::project_dirs()?.cache_dir().join(LOG_FILE_NAME))
}

fn load_sessions(path: &Path) -> Result<Vec<StoredSession>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = fs::read_to_string(path)
        .with_context(|| format!("read performance log {}", path.display()))?;
    Ok(data
        .lines()
        .filter_map(|line| serde_json::from_str::<StoredSession>(line).ok())
        .filter(|session| session.version == FORMAT_VERSION)
        .collect())
}

fn save_session(path: &Path, session: &StoredSession) -> Result<()> {
    let parent = path
        .parent()
        .context("performance log path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create performance directory {}", parent.display()))?;
    let mut lines = if path.exists() {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter(|line| serde_json::from_str::<StoredSession>(line).is_ok())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    lines.push(serde_json::to_string(session)?);
    if lines.len() > MAX_SESSIONS {
        lines.drain(..lines.len() - MAX_SESSIONS);
    }
    while lines.len() > 1 && lines.iter().map(|line| line.len() + 1).sum::<usize>() > MAX_LOG_BYTES
    {
        lines.remove(0);
    }
    let mut output = lines.join("\n");
    output.push('\n');
    fs::write(path, output).with_context(|| format!("write performance log {}", path.display()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn saves_and_loads_an_interactive_session() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("performance.jsonl");
        let session = PerformanceSession::start_with_enabled(true);
        session.mark_interactive();
        session.record_value(
            "startup.app_load",
            Duration::from_millis(12),
            "success",
            &[],
        );
        session.record_frame(Duration::from_micros(800), Duration::from_millis(2));

        session.save_to(&path).unwrap();

        let stored = load_sessions(&path).unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].events[0].duration_micros, 12_000);
        assert_eq!(stored[0].frames.render_buckets[0], 1);
        assert_eq!(stored[0].frames.draw_buckets[2], 1);
    }

    #[test]
    fn disabled_session_does_not_collect_measurements() {
        let session = PerformanceSession::start_with_enabled(false);
        session.record_value("event", Duration::from_millis(1), "success", &[]);
        session.record_frame(Duration::from_millis(1), Duration::from_millis(2));

        assert!(session.events.lock().unwrap().is_empty());
        assert_eq!(session.render_frames.snapshot().0, [0; 7]);
        assert_eq!(session.draw_frames.snapshot().0, [0; 7]);
    }

    #[test]
    fn only_one_enables_performance_measurement() {
        assert!(enabled_from_env_value(Some(OsStr::new("1"))));
        assert!(!enabled_from_env_value(None));
        assert!(!enabled_from_env_value(Some(OsStr::new("0"))));
        assert!(!enabled_from_env_value(Some(OsStr::new("true"))));
    }

    #[test]
    fn percentile_uses_the_nearest_rank() {
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 95), 4);
    }

    #[test]
    fn histogram_reports_the_matching_upper_bound() {
        assert_eq!(histogram_percentile(&[1, 1, 8, 0, 0, 0, 0], 50), 4_000);
        assert_eq!(histogram_percentile(&[1, 1, 8, 0, 0, 0, 0], 95), 4_000);
    }

    #[test]
    fn bounds_events_in_a_long_running_session() {
        let session = PerformanceSession::start_with_enabled(true);
        for _ in 0..MAX_EVENTS_PER_SESSION + 5 {
            session.record_value("event", Duration::ZERO, "success", &[]);
        }

        assert_eq!(session.events.lock().unwrap().len(), MAX_EVENTS_PER_SESSION);
        assert_eq!(session.dropped_events.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn bounds_the_number_of_saved_sessions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("performance.jsonl");
        let session = StoredSession {
            version: FORMAT_VERSION,
            started_at_unix_millis: 0,
            duration_millis: 1,
            events: Vec::new(),
            frames: FrameSummary::default(),
            dropped_events: 0,
        };
        for _ in 0..MAX_SESSIONS + 5 {
            save_session(&path, &session).unwrap();
        }

        assert_eq!(load_sessions(&path).unwrap().len(), MAX_SESSIONS);
        assert!(fs::metadata(path).unwrap().len() <= MAX_LOG_BYTES as u64);
    }

    #[test]
    #[ignore = "manual performance measurement"]
    fn measures_frame_recording_overhead() {
        let session = PerformanceSession::start_with_enabled(true);
        let iterations = 1_000_000;
        let started = Instant::now();
        for _ in 0..iterations {
            session.record_frame(Duration::from_micros(500), Duration::from_millis(1));
        }
        let elapsed = started.elapsed();
        eprintln!(
            "recorded {iterations} frames in {elapsed:?} ({:?}/frame)",
            elapsed / iterations
        );
    }
}
