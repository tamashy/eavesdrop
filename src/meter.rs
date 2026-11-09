//! Level meter drawing for the Record view: a segmented bar with green / yellow /
//! red zones and a peak-hold marker, a dB scale, and a level history whose bars
//! are colored by the same zones.
//!
//! Levels are positions on a -60..0 dBFS scale, 0.0..=1.0 (see `ui::level_ratio`).

use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use crate::theme::Theme;

/// Start of the yellow zone: -12 dB.
const WARN_AT: f64 = 48.0 / 60.0;
/// Start of the red zone: -3 dB.
const HOT_AT: f64 = 57.0 / 60.0;

/// Tick labels in placement priority (later ones are dropped if they'd overlap).
const TICKS: [i32; 8] = [0, -12, -24, -36, -48, -60, -6, -3];

const HORIZONTAL: [&str; 8] = ["▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
const VERTICAL: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Zone color of a position on the scale.
pub fn zone(t: &Theme, ratio: f64) -> Color {
    if ratio >= HOT_AT {
        t.err
    } else if ratio >= WARN_AT {
        t.warn
    } else {
        t.ok
    }
}

/// One-row bar: each cell takes the color of its zone, so the bar turns yellow
/// and red as it reaches loud levels. `peak` is drawn as a thin marker.
/// `off` (muted) draws it all in the theme's inactive color.
pub fn bar(buf: &mut Buffer, area: Rect, t: &Theme, level: f64, peak: f64, off: bool) {
    let w = area.width as usize;
    if w == 0 || area.height == 0 {
        return;
    }
    let color = |r: f64| if off { t.off } else { zone(t, r) };
    // Filled length in eighths of a cell.
    let filled = (level.clamp(0.0, 1.0) * w as f64 * 8.0).round() as usize;
    let peak_cell = (peak > level && peak > 0.0)
        .then(|| ((peak.clamp(0.0, 1.0) * w as f64).ceil() as usize).clamp(1, w) - 1);
    for i in 0..w {
        let mid = (i as f64 + 0.5) / w as f64;
        let eighths = filled.saturating_sub(i * 8).min(8);
        let cell = &mut buf[(area.x + i as u16, area.y)];
        cell.set_bg(t.track);
        if eighths > 0 {
            cell.set_symbol(HORIZONTAL[eighths - 1]).set_fg(color(mid));
        } else if peak_cell == Some(i) {
            cell.set_symbol("▕").set_fg(color(mid));
        } else {
            cell.set_symbol(" ");
        }
    }
}

/// dB labels under the bar ("-60 … -12 … 0"), placed where they don't collide.
pub fn scale(buf: &mut Buffer, area: Rect, color: Color) {
    let w = area.width as usize;
    if w == 0 || area.height == 0 {
        return;
    }
    let mut used = vec![false; w];
    for db in TICKS {
        let label = db.to_string();
        let len = label.chars().count();
        if len > w {
            continue;
        }
        // Tick position, then the label centered on it, kept inside the row.
        let pos = ((db + 60) as f64 / 60.0 * w as f64).round() as usize;
        let start = pos.saturating_sub(len / 2).min(w - len);
        // Keep one free cell on each side.
        let lo = start.saturating_sub(1);
        let hi = (start + len + 1).min(w);
        if used[lo..hi].iter().any(|&u| u) {
            continue;
        }
        used[start..start + len].iter_mut().for_each(|u| *u = true);
        buf.set_string(area.x + start as u16, area.y, &label, color);
    }
}

/// Level history, newest on the right; one column per sample (0..=100), bars
/// growing from the bottom with eighth-block precision, colored by zone.
pub fn history(buf: &mut Buffer, area: Rect, t: &Theme, samples: &[u64], off: bool) {
    let (w, h) = (area.width as usize, area.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let shown = &samples[samples.len().saturating_sub(w)..];
    let x0 = area.x as usize + w - shown.len();
    for (i, &v) in shown.iter().enumerate() {
        let ratio = v.min(100) as f64 / 100.0;
        let color = if off { t.off } else { zone(t, ratio) };
        // A silent column still gets a baseline dot so the timeline is visible.
        let eighths = ((ratio * (h * 8) as f64).round() as usize).max(1);
        for row in 0..h {
            let in_row = eighths.saturating_sub(row * 8).min(8);
            if in_row == 0 {
                break;
            }
            let y = area.y + (h - 1 - row) as u16;
            buf[((x0 + i) as u16, y)]
                .set_symbol(VERTICAL[in_row - 1])
                .set_fg(color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    fn row(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn bar_fills_and_shows_peak() {
        let t = &theme::THEMES[0];
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        bar(&mut buf, area, t, 0.5, 0.9, false);
        assert_eq!(row(&buf, 0), "█████   ▕ ");
        // Zones: start green, peak marker sits in the yellow zone.
        assert_eq!(buf[(0, 0)].fg, t.ok);
        assert_eq!(buf[(8, 0)].fg, t.warn);

        let mut buf = Buffer::empty(area);
        bar(&mut buf, area, t, 1.0, 1.0, false);
        assert_eq!(buf[(9, 0)].fg, t.err);
    }

    #[test]
    fn bar_partial_cell() {
        let t = &theme::THEMES[0];
        let area = Rect::new(0, 0, 4, 1);
        let mut buf = Buffer::empty(area);
        bar(&mut buf, area, t, 0.375, 0.0, false); // 1.5 cells
        assert_eq!(row(&buf, 0), "█▌  ");
    }

    #[test]
    fn scale_labels_do_not_overlap() {
        for w in [10u16, 30, 60, 117] {
            let area = Rect::new(0, 0, w, 1);
            let mut buf = Buffer::empty(area);
            scale(&mut buf, area, Color::Reset);
            let s = row(&buf, 0);
            assert!(s.contains('0'), "{w}: {s:?}");
            // Labels are separated by spaces, never glued together.
            assert!(!s.contains("0-"), "{w}: {s:?}");
            if w >= 60 {
                assert!(s.starts_with("-60") && s.trim_end().ends_with('0'), "{s:?}");
                assert!(s.contains("-12"), "{s:?}");
            }
        }
    }

    #[test]
    fn history_draws_bars_right_aligned() {
        let t = &theme::THEMES[0];
        let area = Rect::new(0, 0, 4, 2);
        let mut buf = Buffer::empty(area);
        history(&mut buf, area, t, &[100, 50], false); // newest (50) on the right
        assert_eq!(row(&buf, 0), "  █ ");
        assert_eq!(row(&buf, 1), "  ██");
        assert_eq!(buf[(2, 1)].fg, t.err);
        assert_eq!(buf[(3, 1)].fg, t.ok);
    }
}
