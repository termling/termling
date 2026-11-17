//! A local top-CPU-processes pane.
//!
//! Shows the handful of processes using the most CPU right now. It learns this by
//! shelling out to `ps` via [`std::process::Command`] — a plain process spawn,
//! *not* a network call, so the zero-network promise holds. `ps aux` is parsed
//! the same way on macOS (BSD) and Linux: its columns are
//! `USER PID %CPU %MEM VSZ RSS TT STAT STARTED TIME COMMAND`, so `%CPU` is the
//! third field and the command is everything from the eleventh on. Parsing is
//! defensive: any line that doesn't look like a process row is skipped, so odd
//! `ps` output never panics. Spawning `ps` is cheap but not free, so the pane
//! refreshes at most once every [`REFRESH_TICKS`] ticks and renders a cached
//! snapshot in between.

use std::process::Command;

use super::bg::Bg;
use crate::doodle::{Row, Seg};
use crate::theme::Theme;

/// Re-run `ps` at most this often (in dashboard ticks).
const REFRESH_TICKS: u64 = 30;
/// How many top processes to show.
const TOP_N: usize = 6;

/// One process row: its CPU percentage and a short command name.
#[derive(Clone)]
struct Proc {
    cpu: f32,
    name: String,
}

/// A parsed snapshot of the top CPU consumers (already sorted, already trimmed to
/// [`TOP_N`]). Empty until the first background collection lands.
#[derive(Clone, Default)]
struct ProcInfo {
    procs: Vec<Proc>,
}

/// The top-processes tile. Holds a cached [`ProcInfo`]; re-collection runs on a
/// background thread (see [`Bg`]) so shelling out to `ps` never stalls rendering.
pub struct ProcessesPane {
    info: ProcInfo,
    /// Tick at which we may refresh again; `0` means "due now".
    next_refresh: u64,
    /// Off-thread re-collection; its result lands on a later tick.
    bg: Bg<ProcInfo>,
}

impl ProcessesPane {
    /// Build the pane and kick off the first collection off-thread, so building it
    /// (which `App::new` does eagerly for every pane) never blocks startup on
    /// `ps`. `info` stays empty until the first result lands, so `render` shows
    /// its placeholder until then.
    pub fn new() -> ProcessesPane {
        let bg = Bg::new();
        bg.trigger(collect);
        ProcessesPane {
            info: ProcInfo::default(),
            next_refresh: REFRESH_TICKS,
            bg,
        }
    }
}

impl Default for ProcessesPane {
    fn default() -> Self {
        ProcessesPane::new()
    }
}

impl super::Pane for ProcessesPane {
    fn title(&self) -> String {
        "processes".into()
    }

    fn tick(&mut self, frame: u64) {
        // Pull in a completed background collection, if one landed.
        if let Some(fresh) = self.bg.take() {
            self.info = fresh;
        }
        // Kick off the next collection off-thread when due.
        if frame >= self.next_refresh {
            self.bg.trigger(collect);
            self.next_refresh = frame.saturating_add(REFRESH_TICKS);
        }
    }

    fn refresh(&mut self) {
        // Force the next tick to recollect (off-thread; never blocks here).
        self.next_refresh = 0;
    }

    fn render(&self, _width: u16, _height: u16, theme: &Theme) -> Vec<Row> {
        if self.info.procs.is_empty() {
            return vec![
                super::line("no process data", theme.dim),
                super::line("(or ps unavailable)", theme.dim),
            ];
        }

        let mut rows = Vec::new();
        rows.push(super::line("top cpu", theme.dim));
        for p in &self.info.procs {
            rows.push(vec![
                Seg {
                    text: format!("{:>6.1}%", p.cpu),
                    color: theme.accent,
                },
                Seg {
                    text: format!("  {}", p.name),
                    color: theme.dim,
                },
            ]);
        }
        rows
    }
}

/// Shell out to `ps aux` and fold its output into a sorted [`ProcInfo`]. Any
/// failure (missing binary, non-zero exit) yields an empty snapshot, and any line
/// that doesn't parse as a process row is skipped.
fn collect() -> ProcInfo {
    let Ok(out) = Command::new("ps").args(["aux"]).output() else {
        return ProcInfo::default();
    };
    if !out.status.success() {
        return ProcInfo::default();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    ProcInfo {
        procs: parse(&text),
    }
}

/// Parse `ps aux` output: columns are
/// `USER PID %CPU %MEM VSZ RSS TT STAT STARTED TIME COMMAND`. We only need `%CPU`
/// (field 2) and `COMMAND` (field 10 onward). The header line and any short or
/// malformed line are skipped. Split out from [`collect`] so it can be tested
/// without a live `ps`.
fn parse(text: &str) -> Vec<Proc> {
    let mut procs: Vec<Proc> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 11 {
            continue;
        }
        // The %CPU column; a non-numeric value (e.g. the "%CPU" header) is skipped.
        let Ok(cpu) = fields[2].parse::<f32>() else {
            continue;
        };
        if !cpu.is_finite() {
            continue;
        }
        let name = short_command(&fields[10..]);
        if name.is_empty() {
            continue;
        }
        procs.push(Proc { cpu, name });
    }
    // Highest CPU first; `total_cmp` keeps odd floats (NaN already filtered) sane.
    procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    procs.truncate(TOP_N);
    procs
}

/// Turn the trailing COMMAND fields into a short, readable name: take the first
/// token (the executable), then its basename, so `/usr/bin/cargo` → `cargo`.
fn short_command(cmd_fields: &[&str]) -> String {
    let first = cmd_fields.first().copied().unwrap_or("");
    // A bracketed kernel thread like "[kworker/0]" has no path; keep it as-is.
    let base = first.rsplit('/').next().unwrap_or(first);
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::Pane;
    use crate::theme::THEMES;

    #[test]
    fn renders_non_empty_without_panicking() {
        // Runs against the live `ps` on whatever host executes the test; either
        // branch of `render` must yield rows and never panic.
        let pane = ProcessesPane::new();
        for theme in THEMES {
            let rows = pane.render(40, 12, theme);
            assert!(!rows.is_empty());
        }
    }

    #[test]
    fn empty_info_renders_a_placeholder() {
        let pane = ProcessesPane {
            info: ProcInfo::default(),
            next_refresh: REFRESH_TICKS,
            bg: Bg::new(),
        };
        let rows = pane.render(40, 12, &THEMES[0]);
        assert!(!rows.is_empty());
        assert_eq!(pane.title(), "processes");
    }

    #[test]
    fn parse_sorts_by_cpu_and_shortens_names() {
        let sample = "\
USER   PID %CPU %MEM VSZ RSS TT STAT STARTED TIME COMMAND
alice  101  3.5  0.1 100 200 ?? Ss 1Jan 0:01 /usr/bin/cargo build
bob    102 42.0  0.2 100 200 ?? Rs 1Jan 0:02 /System/Library/foo
carol  103  0.0  0.0 100 200 ?? Ss 1Jan 0:00 bash
garbage line that is too short
";
        let procs = parse(sample);
        assert_eq!(procs.len(), 3);
        // Sorted by %CPU descending.
        assert_eq!(procs[0].name, "foo");
        assert_eq!(procs[1].name, "cargo");
        assert_eq!(procs[2].name, "bash");
        assert!(procs[0].cpu > procs[1].cpu);
    }

    #[test]
    fn parse_never_panics_on_junk() {
        for junk in ["", "\n\n", "one two three", "x x x x x x x x x x x"] {
            let _ = parse(junk);
        }
    }

    #[test]
    fn refresh_marks_due_now() {
        let mut pane = ProcessesPane::new();
        pane.next_refresh = 9_999;
        Pane::refresh(&mut pane);
        assert_eq!(pane.next_refresh, 0);
    }

    #[test]
    fn ticking_refreshes_on_cadence_without_panicking() {
        let mut pane = ProcessesPane::new();
        for f in 0..(REFRESH_TICKS * 2 + 1) {
            pane.tick(f);
        }
        assert!(!pane.render(40, 12, &THEMES[0]).is_empty());
    }
}
