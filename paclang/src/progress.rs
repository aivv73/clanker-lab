//! TTY-gated stderr progress blocks for long refresh stages.
//!
//! Progress never touches stdout, so piped structured reports stay clean.
//! When stderr is not a terminal (automation, tests), every method is a
//! no-op and nothing is written. The finished block is left on the terminal;
//! callers finalize with [`Progress::finish`] on every exit path so
//! interruption does not leave a torn line.

use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

/// Progress bar cells per block. Fixed so unit tests and documentation can
/// rely on the layout without querying terminal dimensions.
const BAR_CELLS: u64 = 15;

/// One refresh-stage progress block: a labeled bar plus downloaded bytes,
/// two stage counters (for example resolved/unresolved), elapsed time, and
/// ETA. Generic over the writer so tests can capture the output.
pub(crate) struct Progress<W> {
    writer: Option<W>,
    label: &'static str,
    counters: [(&'static str, u64); 2],
    total: u64,
    completed: u64,
    downloaded_bytes: u64,
    started: Instant,
    drawn_lines: usize,
    finished: bool,
}

impl Progress<io::Stderr> {
    /// A live block when stderr is a terminal, otherwise a silent no-op.
    pub(crate) fn stderr(label: &'static str, first: &'static str, second: &'static str) -> Self {
        let writer = io::stderr().is_terminal().then(io::stderr);
        Self::with_writer(label, first, second, writer)
    }
}

impl Progress<io::Sink> {
    /// An explicitly silent block, for stages that currently report nothing.
    #[allow(dead_code)]
    pub(crate) fn disabled(label: &'static str, first: &'static str, second: &'static str) -> Self {
        Self::with_writer(label, first, second, None)
    }
}

#[cfg(test)]
impl Progress<Vec<u8>> {
    fn capture(label: &'static str, first: &'static str, second: &'static str) -> Self {
        Self::with_writer(label, first, second, Some(Vec::new()))
    }

    fn output(&self) -> String {
        String::from_utf8(self.writer.clone().expect("capture always writes"))
            .expect("progress is UTF-8")
    }
}

impl<W> Progress<W> {
    fn with_writer(
        label: &'static str,
        first: &'static str,
        second: &'static str,
        writer: Option<W>,
    ) -> Self {
        Self {
            writer,
            label,
            counters: [(first, 0), (second, 0)],
            total: 0,
            completed: 0,
            downloaded_bytes: 0,
            started: Instant::now(),
            drawn_lines: 0,
            finished: false,
        }
    }
}

impl<W: Write> Progress<W> {
    /// Set the denominator and draw the initial block. A zero total draws
    /// nothing, so empty selections stay silent even on a terminal.
    pub(crate) fn set_total(&mut self, total: u64) {
        self.total = total;
        self.draw();
    }

    /// Record one or more finished units with their downloaded bytes and
    /// stage counters, then redraw.
    pub(crate) fn advance(
        &mut self,
        completed: u64,
        downloaded_bytes: u64,
        first: u64,
        second: u64,
    ) {
        self.completed = self.completed.saturating_add(completed);
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(downloaded_bytes);
        self.counters[0].1 = self.counters[0].1.saturating_add(first);
        self.counters[1].1 = self.counters[1].1.saturating_add(second);
        self.draw();
    }

    /// Redraw the final state and leave the block on the terminal. Call this
    /// on every exit path, including failures, so interruption does not
    /// leave a torn line. There is deliberately no `Drop` impl: `Drop`
    /// cannot carry the `Write` bound, so finalization stays explicit.
    pub(crate) fn finish(&mut self) {
        if !self.finished {
            self.finished = true;
            self.draw();
        }
    }

    fn draw(&mut self) {
        if self.writer.is_none() || self.total == 0 {
            return;
        }
        let elapsed = self.started.elapsed();
        let eta = self
            .eta(elapsed)
            .map(format_duration)
            .unwrap_or_else(|| "--:--".to_owned());
        let filled = self.completed.saturating_mul(BAR_CELLS) / self.total.max(1);
        let filled = filled.min(BAR_CELLS) as usize;
        let width = self.label.len();
        let mut lines = vec![format!(
            "{}  [{}{}]  {}/{}",
            self.label,
            "█".repeat(filled),
            "░".repeat(BAR_CELLS as usize - filled),
            self.completed.min(self.total),
            self.total,
        )];
        lines.push(format!(
            "{:<width$}  {}",
            "Downloaded",
            format_bytes(self.downloaded_bytes),
            width = width
        ));
        for (name, value) in &self.counters {
            lines.push(format!("{name:<width$}  {value}", width = width));
        }
        lines.push(format!(
            "{:<width$}  {}",
            "Elapsed",
            format_duration(elapsed),
            width = width
        ));
        lines.push(format!("{:<width$}  {}", "ETA", eta, width = width));
        // Progress must never fail a refresh: a closed stderr just freezes
        // the block.
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        if self.drawn_lines > 0 {
            let _ = write!(writer, "\x1b[{}A", self.drawn_lines);
        }
        for line in &lines {
            let _ = writeln!(writer, "\r\x1b[K{line}");
        }
        let _ = writer.flush();
        self.drawn_lines = lines.len();
    }

    fn eta(&self, elapsed: Duration) -> Option<Duration> {
        if self.completed >= self.total {
            return Some(Duration::ZERO);
        }
        if self.completed == 0 {
            return None;
        }
        let remaining = self.total - self.completed;
        Some(Duration::from_secs_f64(
            elapsed.as_secs_f64() / self.completed as f64 * remaining as f64,
        ))
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::{Progress, format_bytes, format_duration};
    use std::time::Duration;

    #[test]
    fn byte_units_use_one_decimal_above_bytes() {
        for (bytes, expected) in [
            (0, "0 B"),
            (512, "512 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (2 * 1024 * 1024, "2.0 MiB"),
            (3 * 1024 * 1024 * 1024, "3.0 GiB"),
        ] {
            assert_eq!(format_bytes(bytes), expected);
        }
    }

    #[test]
    fn durations_show_hours_only_above_an_hour() {
        for (seconds, expected) in [(0, "00:00"), (61, "01:01"), (3725, "1:02:05")] {
            assert_eq!(format_duration(Duration::from_secs(seconds)), expected);
        }
    }

    #[test]
    fn block_shows_bar_counts_and_counters() {
        let mut progress = Progress::capture("Fetching recipes", "Resolved", "Unresolved");
        progress.set_total(100);
        assert!(progress.output().contains("--:--"));
        progress.advance(50, 1536, 48, 2);
        let output = progress.output();
        assert!(output.contains("[███████░░░░░░░░]"), "{output}");
        assert!(output.contains("50/100"), "{output}");
        assert!(output.contains("Resolved"), "{output}");
        assert!(output.contains("1.5 KiB"), "{output}");
        let last_draw = output.rsplit("\x1b[6A").next().unwrap();
        assert!(!last_draw.contains("--:--"), "{last_draw}");
    }

    #[test]
    fn empty_total_draws_nothing() {
        let mut progress = Progress::capture("Fetching recipes", "Resolved", "Unresolved");
        progress.set_total(0);
        progress.advance(0, 0, 0, 0);
        progress.finish();
        assert!(progress.output().is_empty());
    }

    #[test]
    fn completed_block_reports_zero_eta() {
        let mut progress = Progress::capture("Fetching recipes", "Resolved", "Unresolved");
        progress.set_total(2);
        progress.advance(2, 0, 2, 0);
        let output = progress.output();
        assert!(output.contains("ETA"), "{output}");
        assert!(output.contains("00:00"), "{output}");
    }
}
