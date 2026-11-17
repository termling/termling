//! A local filesystem-usage pane.
//!
//! Shows how full the real mounted filesystems are. It learns this by shelling
//! out to `df -k` via [`std::process::Command`] — a plain process spawn, *not* a
//! network call, so the zero-network promise holds. `df -k` reports block counts
//! in 1 KiB units on both macOS (BSD) and Linux; its columns are
//! `Filesystem <blocks> Used Avail Capacity … Mounted-on`, with the mount point
//! always last and the capacity the first field ending in `%`. Parsing is
//! defensive: pseudo-filesystems (devfs/tmpfs/map/overlay …) and any line that
//! doesn't look like a real mount are skipped, so odd `df` output never panics.
//! Disks change slowly, so the pane refreshes at most once every
//! [`REFRESH_TICKS`] ticks and renders a cached snapshot in between.

use std::process::Command;

use super::bg::Bg;
use crate::doodle::{Row, Seg};
use crate::theme::Theme;

/// Re-run `df` at most this often (in dashboard ticks).
const REFRESH_TICKS: u64 = 60;
/// How many filesystems to show.
const MAX_ROWS: usize = 5;

/// One filesystem row: its mount point, percent used, and used/total sizes (KiB).
#[derive(Clone)]
struct Fs {
    mount: String,
    used_pct: u32,
    used_kb: u64,
    size_kb: u64,
}

/// A parsed snapshot of the real filesystems (already trimmed to [`MAX_ROWS`]).
/// Empty until the first background collection lands.
#[derive(Clone, Default)]
struct DiskInfo {
    filesystems: Vec<Fs>,
}

/// The disk-usage tile. Holds a cached [`DiskInfo`]; re-collection runs on a
/// background thread (see [`Bg`]) so shelling out to `df` never stalls rendering.
pub struct DiskPane {
    info: DiskInfo,
    /// Tick at which we may refresh again; `0` means "due now".
    next_refresh: u64,
    /// Off-thread re-collection; its result lands on a later tick.
    bg: Bg<DiskInfo>,
}

impl DiskPane {
    /// Build the pane and kick off the first collection off-thread, so building it
    /// (which `App::new` does eagerly for every pane) never blocks startup on
    /// `df`. `info` stays empty until the first result lands, so `render` shows
    /// its placeholder until then.
    pub fn new() -> DiskPane {
        let bg = Bg::new();
        bg.trigger(collect);
        DiskPane {
            info: DiskInfo::default(),
            next_refresh: REFRESH_TICKS,
            bg,
        }
    }
}

impl Default for DiskPane {
    fn default() -> Self {
        DiskPane::new()
    }
}

impl super::Pane for DiskPane {
    fn title(&self) -> String {
        "disk".into()
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
        if self.info.filesystems.is_empty() {
            return vec![
                super::line("no disk data", theme.dim),
                super::line("(or df unavailable)", theme.dim),
            ];
        }

        self.info
            .filesystems
            .iter()
            .map(|fs| {
                vec![
                    Seg {
                        text: format!("{}  ", fs.mount),
                        color: theme.accent,
                    },
                    Seg {
                        text: format!("{}%", fs.used_pct),
                        // Dim while there's headroom; flag a nearly-full disk.
                        color: if fs.used_pct >= 90 {
                            theme.bad
                        } else {
                            theme.gem
                        },
                    },
                    Seg {
                        text: format!(" {}/{}", human_kb(fs.used_kb), human_kb(fs.size_kb)),
                        color: theme.dim,
                    },
                ]
            })
            .collect()
    }
}

/// Shell out to `df -k` and fold its output into a [`DiskInfo`]. Any failure
/// (missing binary, non-zero exit) yields an empty snapshot, and any line that
/// doesn't parse as a real filesystem is skipped.
fn collect() -> DiskInfo {
    let Ok(out) = Command::new("df").args(["-k"]).output() else {
        return DiskInfo::default();
    };
    if !out.status.success() {
        return DiskInfo::default();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    DiskInfo {
        filesystems: parse(&text),
    }
}

/// Parse `df -k` output. The header line and pseudo-filesystems are skipped; the
/// mount point is the last field and the capacity is the first field ending in
/// `%`. Split out from [`collect`] so it can be tested without a live `df`.
fn parse(text: &str) -> Vec<Fs> {
    let mut filesystems = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // Need at least: fs, blocks, used, avail, capacity, …, mount.
        if fields.len() < 6 {
            continue;
        }
        let fs_name = fields[0];
        if is_pseudo(fs_name) {
            continue;
        }
        // Block counts are in 1 KiB units (`-k`); a non-numeric value means this
        // isn't a real data row (e.g. the header, or a wrapped long device name).
        let (Ok(size_kb), Ok(used_kb)) = (fields[1].parse::<u64>(), fields[2].parse::<u64>())
        else {
            continue;
        };
        if size_kb == 0 {
            continue;
        }
        // Capacity is the first token ending in '%' (macOS also has a later
        // "%iused" column, so we take the earliest).
        let Some(used_pct) = fields
            .iter()
            .find_map(|f| f.strip_suffix('%').and_then(|n| n.parse::<u32>().ok()))
        else {
            continue;
        };
        let mount = fields.last().copied().unwrap_or("").to_string();
        if mount.is_empty() {
            continue;
        }
        filesystems.push(Fs {
            mount,
            used_pct,
            used_kb,
            size_kb,
        });
    }
    filesystems.truncate(MAX_ROWS);
    filesystems
}

/// Whether a filesystem name is a pseudo/virtual filesystem we skip: devfs,
/// tmpfs, overlay, and macOS automount `map` entries.
fn is_pseudo(fs_name: &str) -> bool {
    matches!(fs_name, "devfs" | "tmpfs" | "overlay" | "map" | "none")
        || fs_name.starts_with("tmpfs")
}

/// Kibibytes as a compact human size: `493M`, `232G`.
fn human_kb(kb: u64) -> String {
    let mb = kb as f64 / 1024.0;
    if mb < 1024.0 {
        format!("{:.0}M", mb.round())
    } else {
        format!("{:.0}G", (mb / 1024.0).round())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::Pane;
    use crate::theme::THEMES;

    #[test]
    fn renders_non_empty_without_panicking() {
        // Runs against the live `df` on whatever host executes the test; either
        // branch of `render` must yield rows and never panic.
        let pane = DiskPane::new();
        for theme in THEMES {
            let rows = pane.render(40, 12, theme);
            assert!(!rows.is_empty());
        }
    }

    #[test]
    fn empty_info_renders_a_placeholder() {
        let pane = DiskPane {
            info: DiskInfo::default(),
            next_refresh: REFRESH_TICKS,
            bg: Bg::new(),
        };
        let rows = pane.render(40, 12, &THEMES[0]);
        assert!(!rows.is_empty());
        assert_eq!(pane.title(), "disk");
    }

    #[test]
    fn parse_keeps_real_filesystems_and_drops_pseudo() {
        // A blend of macOS- and Linux-shaped rows plus pseudo filesystems.
        let sample = "\
Filesystem     1024-blocks      Used Available Capacity iused ifree %iused Mounted on
/dev/disk3s1s1   482797652  12345584  14761248    46%  458734 1 0% /
devfs                  249       249         0   100%     864 0 100% /dev
map auto_home            0         0         0   100%       0 0 100% /System/Volumes/Data/home
tmpfs              8152312         0   8152312     0% /dev/shm
/dev/sda1        103080888  40000000  60000000    40% /
";
        let fs = parse(sample);
        // Keeps the two real disks; drops devfs, the map automount, and tmpfs.
        assert_eq!(fs.len(), 2);
        assert_eq!(fs[0].mount, "/");
        assert_eq!(fs[0].used_pct, 46);
        assert_eq!(fs[1].used_pct, 40);
    }

    #[test]
    fn parse_never_panics_on_junk() {
        for junk in [
            "",
            "\n\n",
            "a b c",
            "Filesystem Size Used Avail Use% Mounted",
        ] {
            let _ = parse(junk);
        }
    }

    #[test]
    fn human_kb_is_sane() {
        assert_eq!(human_kb(493_284), "482M");
        assert_eq!(human_kb(482_797_652), "460G");
    }

    #[test]
    fn refresh_marks_due_now() {
        let mut pane = DiskPane::new();
        pane.next_refresh = 9_999;
        Pane::refresh(&mut pane);
        assert_eq!(pane.next_refresh, 0);
    }

    #[test]
    fn ticking_refreshes_on_cadence_without_panicking() {
        let mut pane = DiskPane::new();
        for f in 0..(REFRESH_TICKS * 2 + 1) {
            pane.tick(f);
        }
        assert!(!pane.render(40, 12, &THEMES[0]).is_empty());
    }
}
