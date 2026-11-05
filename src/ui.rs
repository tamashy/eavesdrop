//! Main terminal UI. Two views, switched with Tab:
//!  * Record     — start / pause / stop, level meters, level history, warnings
//!  * Recordings — list of saved recordings and a player (see `browser`)
//!
//! Stopping a recording saves it in the background and gets the next one ready,
//! so you can listen to it and record again without leaving the app.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Gauge, Paragraph, Sparkline, Wrap},
};

use crate::{
    Meter,
    browser::Library,
    clock::State,
    record::{Outcome, Recorder, Saving, Take},
};

const TICK: Duration = Duration::from_millis(100);
const HISTORY: usize = 600; // 60 s at 10 Hz
const SILENCE: f32 = 0.001; // -60 dBFS
const SILENT_WARN_AFTER: Duration = Duration::from_secs(15);

pub struct Config {
    pub recorder: Recorder,
    /// Auto-stop (and save) after this much *recorded* time.
    pub limit: Option<Duration>,
    /// Start the first recording right away.
    pub start_now: bool,
    /// Fires on SIGINT (only relevant if raw mode didn't catch Ctrl+C).
    pub stop: mpsc::Receiver<()>,
}

struct Channel {
    name: String,
    meter: Arc<Meter>,
    level: f32,
    hist: VecDeque<u64>,
    last_sound: Option<Instant>,
}

impl Channel {
    fn new((name, meter): (String, Arc<Meter>)) -> Self {
        Self {
            name,
            meter,
            level: 0.0,
            hist: VecDeque::with_capacity(HISTORY),
            last_sound: None,
        }
    }

    fn tick(&mut self, now: Instant) {
        let p = self.meter.take_peak();
        // Fast attack, smooth release so the meter is readable.
        self.level = p.max(self.level * 0.75);
        if self.hist.len() == HISTORY {
            self.hist.pop_front();
        }
        self.hist.push_back((level_ratio(p) * 100.0) as u64);
        if p > SILENCE {
            self.last_sound = Some(now);
        }
    }

    fn muted(&self) -> bool {
        self.meter.muted.load(Ordering::Relaxed)
    }
}

pub fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// Peak (0..1) → meter position (0..1) on a -60..0 dB scale.
fn level_ratio(p: f32) -> f64 {
    if p <= 0.0 {
        return 0.0;
    }
    (((20.0 * p.log10()) + 60.0) / 60.0).clamp(0.0, 1.0) as f64
}

fn db_label(p: f32) -> String {
    if p <= SILENCE {
        "  -∞ dB".into()
    } else {
        format!("{:>4.0} dB", 20.0 * p.log10())
    }
}

fn level_color(p: f32) -> Color {
    let db = 20.0 * p.max(1e-9).log10();
    if db > -3.0 {
        Color::Red
    } else if db > -12.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// "~/…/eavesdrop/eavesdrop_2026-11-05.wav" — home shortened, middle elided to fit `max` chars.
pub fn short_path(p: &Path, max: usize) -> String {
    let mut s = p.display().to_string();
    if let Some(home) = dirs::home_dir().map(|h| h.display().to_string())
        && let Some(rest) = s.strip_prefix(&home)
    {
        s = format!("~{rest}");
    }
    let n = s.chars().count();
    if n <= max || max < 8 {
        return s;
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

pub fn fmt_bytes(b: u64) -> String {
    match b {
        b if b >= 1 << 30 => format!("{:.2} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", b / 1024),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Record,
    Recordings,
}

/// The current take with its UI state.
struct Live {
    take: Take,
    call: Channel,
    mic: Option<Channel>,
}

struct App {
    rec: Recorder,
    limit: Option<Duration>,
    live: Option<Live>,
    /// Opening the capture streams failed (Space retries).
    open_error: Option<String>,
    library: Library,
    view: View,
    /// Takes being finished in the background.
    saving: Vec<Saving>,
    /// Result of the last save, shown on the Record view.
    notes: Vec<(Color, String)>,
}

impl App {
    fn new(rec: Recorder, limit: Option<Duration>) -> Result<Self> {
        let library = Library::new(rec.out_dir.clone())?;
        let mut app = Self {
            rec,
            limit,
            live: None,
            open_error: None,
            library,
            view: View::Record,
            saving: Vec::new(),
            notes: Vec::new(),
        };
        app.open_take();
        Ok(app)
    }

    fn open_take(&mut self) {
        match self.rec.new_take() {
            Ok(take) => {
                let call = Channel::new((self.rec.call.name.clone(), take.call.clone()));
                let mic = match (&self.rec.mic, &take.mic) {
                    (Some(i), Some(m)) => Some(Channel::new((i.name.clone(), m.clone()))),
                    _ => None,
                };
                self.live = Some(Live { take, call, mic });
                self.open_error = None;
            }
            Err(e) => self.open_error = Some(format!("{e:#}")),
        }
    }

    fn state(&self) -> Option<State> {
        self.live.as_ref().map(|l| l.take.clock.state())
    }

    fn toggle_recording(&mut self) {
        let Some(live) = &self.live else {
            self.open_take(); // retry after an error
            return;
        };
        if live.take.clock.toggle() == State::Recording {
            // Playing back now would end up in the recording.
            self.library.pause_playback();
        }
    }

    /// Stops the current take, saves it in the background, gets a new one ready.
    fn stop_and_save(&mut self) {
        let Some(live) = self.live.take() else { return };
        let name = live.take.output_path();
        self.saving.push(live.take.stop());
        self.notes = vec![(
            Color::Yellow,
            format!(
                "… Saving {}",
                name.file_name().unwrap_or_default().to_string_lossy()
            ),
        )];
        self.open_take();
    }

    /// Collects finished saves.
    fn poll_saving(&mut self) {
        let (done, pending): (Vec<_>, Vec<_>) =
            self.saving.drain(..).partition(|h| h.is_finished());
        self.saving = pending;
        for h in done {
            let mut saved = None;
            let result = h
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("save thread panicked")));
            match result {
                Ok(Outcome::Discarded) => continue,
                Ok(Outcome::Saved {
                    path,
                    recorded,
                    warnings,
                    ..
                }) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let msg = format!(
                        "✓ Saved {name} ({}) — Tab to listen",
                        fmt_duration(recorded)
                    );
                    self.library
                        .set_status(Color::Green, format!("✓ Saved {name}"));
                    self.notes = vec![(Color::Green, msg)];
                    self.notes
                        .extend(warnings.into_iter().map(|w| (Color::Yellow, w)));
                    saved = Some(path.clone());
                }
                Err(e) => {
                    let msg = format!("✗ Saving failed: {e:#}");
                    self.library.set_status(Color::Red, msg.clone());
                    self.notes = vec![(Color::Red, msg)];
                }
            }
            if let Err(e) = self.library.rescan() {
                self.library.set_status(Color::Red, format!("✗ {e:#}"));
            }
            if let Some(p) = &saved {
                self.library.select_path(p);
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        if let Some(live) = self.live.as_mut() {
            live.call.tick(now);
            if let Some(m) = live.mic.as_mut() {
                m.tick(now);
            }
        }
        if let (Some(limit), Some(live)) = (self.limit, &self.live)
            && live.take.clock.recorded() >= limit
        {
            self.stop_and_save();
        }
        self.poll_saving();
        self.library.recording = self.state() == Some(State::Recording);
    }

    /// Returns false to quit.
    fn handle_key(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Tab | KeyCode::BackTab => {
                self.view = match self.view {
                    View::Record => View::Recordings,
                    View::Recordings => View::Record,
                };
                return true;
            }
            _ => {}
        }
        match self.view {
            View::Record => match k.code {
                KeyCode::Esc => return false,
                KeyCode::Char(' ') | KeyCode::Char('p') | KeyCode::Enter => self.toggle_recording(),
                KeyCode::Char('s') => {
                    if self.live.as_ref().is_some_and(|l| l.take.clock.started()) {
                        self.stop_and_save();
                    }
                }
                KeyCode::Char('m') => {
                    if let Some(m) = self.live.as_ref().and_then(|l| l.mic.as_ref()) {
                        m.meter.muted.fetch_xor(true, Ordering::Relaxed);
                    }
                }
                _ => {}
            },
            View::Recordings => match k.code {
                KeyCode::Esc => self.view = View::Record,
                _ => {
                    self.library.recording = self.state() == Some(State::Recording);
                    self.library.handle_key(k);
                }
            },
        }
        true
    }

    /// Stops the current take (saving it if it was started) and returns all pending saves.
    fn finish(mut self) -> Vec<Saving> {
        if let Some(live) = self.live.take() {
            self.saving.push(live.take.stop());
        }
        self.saving
    }
}

/// Runs the app until the user quits. Returns the saves still in progress
/// (including the current recording, if one was started).
pub fn run(cfg: Config) -> Result<Vec<Saving>> {
    let Config {
        recorder,
        limit,
        start_now,
        stop,
    } = cfg;
    let mut app = App::new(recorder, limit)?;
    if start_now {
        app.toggle_recording();
    }

    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        let mut last_tick = Instant::now() - TICK;
        loop {
            let now = Instant::now();
            if now.duration_since(last_tick) >= TICK {
                last_tick = now;
                app.tick(now);
                terminal.draw(|f| draw(f, &mut app))?;
            }
            if stop.try_recv().is_ok() {
                return Ok(());
            }
            if event::poll(TICK.saturating_sub(last_tick.elapsed()))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
                && !app.handle_key(k)
            {
                return Ok(());
            }
        }
    })();
    ratatui::restore();
    let pending = app.finish();
    result.map(|()| pending)
}

fn draw(f: &mut Frame, app: &mut App) {
    let [tabs, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(f.area());

    // ---- tab bar (with the recording state, visible from both views) ------
    let tab = |label: &'static str, v: View| {
        if app.view == v {
            Span::styled(
                format!(" {label} "),
                Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
            )
        } else {
            Span::raw(format!(" {label} ")).dim()
        }
    };
    let mut spans = vec![
        Span::raw(" eavesdrop  ").bold(),
        tab("● Record", View::Record),
        Span::raw(" "),
        tab("♫ Recordings", View::Recordings),
        Span::raw("   Tab switch").dim(),
    ];
    if let Some(live) = &app.live {
        let clock = &live.take.clock;
        let blink = (clock.origin().elapsed().as_millis() / 600).is_multiple_of(2);
        match clock.state() {
            State::Recording => {
                spans.push(Span::styled(
                    if blink { "   ● REC " } else { "     REC " },
                    Style::new().fg(Color::Red).bold(),
                ));
                spans.push(Span::raw(fmt_duration(clock.recorded())).bold());
            }
            State::Paused => {
                spans.push(Span::styled(
                    "   ❚❚ PAUSED ",
                    Style::new().fg(Color::Yellow).bold(),
                ));
                spans.push(Span::raw(fmt_duration(clock.recorded())).bold());
            }
            State::Ready => {}
        }
    }
    if !app.saving.is_empty() {
        spans.push(Span::raw("   saving…").fg(Color::Yellow));
    }
    f.render_widget(Line::from(spans), tabs);

    match app.view {
        View::Record => draw_record(f, body, app),
        View::Recordings => app.library.draw(f, body),
    }
}

fn draw_record(f: &mut Frame, area: Rect, app: &App) {
    let Some(live) = &app.live else {
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "✗ Cannot open audio devices: {}",
                app.open_error.as_deref().unwrap_or("?")
            ),
            Style::new().fg(Color::Red),
        ))];
        lines.push(Line::from("Press Space to try again, Tab for recordings, q to quit.").dim());
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
        return;
    };
    let clock = &live.take.clock;
    let (call, mic) = (&live.call, live.mic.as_ref());
    let limit = app.limit;
    let state = clock.state();
    let recorded = clock.recorded();
    // Warnings look at how long the devices have been open, so problems show up
    // while still in the ready state, before anything is recorded.
    let mut warnings = collect_warnings(clock.origin().elapsed(), call, mic);
    warnings.extend(app.notes.iter().cloned());

    let [header, call_area, mic_area, info, warn_area, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    // ---- header ------------------------------------------------------------
    let blink = (clock.origin().elapsed().as_millis() / 600).is_multiple_of(2);
    let (icon, label, color) = match state {
        State::Ready => (" ○ ", "READY ", Color::Yellow),
        State::Recording => (if blink { " ● " } else { "   " }, "REC ", Color::Red),
        State::Paused => (" ❚❚ ", "PAUSED ", Color::Yellow),
    };
    let mut spans = vec![
        Span::styled(icon, Style::new().fg(color).bold()),
        Span::styled(label, Style::new().fg(color).bold()),
        Span::styled(fmt_duration(recorded), Style::new().bold()),
    ];
    match state {
        State::Ready => {
            spans.push(Span::raw("   press Space to start recording").fg(Color::Yellow))
        }
        State::Paused => spans.push(Span::raw("   press Space to resume").fg(Color::Yellow)),
        State::Recording => {}
    }
    if let Some(l) = limit {
        spans.push(
            Span::raw(format!(
                "   auto-stop in {}",
                fmt_duration(l.saturating_sub(recorded))
            ))
            .dim(),
        );
    }
    f.render_widget(Line::from(spans), header);

    // ---- channels ----------------------------------------------------------
    draw_channel(f, call_area, "Call audio", Some(call));
    draw_channel(f, mic_area, "Microphone", mic);

    // ---- info --------------------------------------------------------------
    let bytes = call.meter.bytes.load(Ordering::Relaxed)
        + mic
            .map(|m| m.meter.bytes.load(Ordering::Relaxed))
            .unwrap_or(0);
    let output = live.take.output_path();
    let info_text = vec![
        Line::from(vec![
            Span::raw(if state == State::Ready {
                "Will save  "
            } else {
                "Saving to  "
            })
            .dim(),
            Span::raw(short_path(&output, info.width.saturating_sub(13) as usize)),
        ]),
        Line::from(vec![
            Span::raw("Recorded   ").dim(),
            Span::raw(fmt_bytes(bytes)),
            Span::raw(" (raw tracks)").dim(),
        ]),
    ];
    f.render_widget(
        Paragraph::new(info_text).block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" Recording "),
        ),
        info,
    );

    // ---- warnings / save results -------------------------------------------
    if !warnings.is_empty() {
        let lines: Vec<Line> = warnings
            .into_iter()
            .map(|(color, text)| Line::from(Span::styled(text, Style::new().fg(color))))
            .collect();
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), warn_area);
    }

    // ---- footer ------------------------------------------------------------
    let key = |k: &'static str| {
        Span::styled(
            format!(" {k} "),
            Style::new().fg(Color::Black).bg(Color::Gray),
        )
    };
    let toggle = match state {
        State::Ready => " start   ",
        State::Recording => " pause   ",
        State::Paused => " resume   ",
    };
    let mut footer_spans = vec![key("space"), Span::raw(toggle)];
    if state != State::Ready {
        footer_spans.extend([key("s"), Span::raw(" stop & save   ")]);
    }
    if let Some(m) = mic {
        footer_spans.push(key("m"));
        footer_spans.push(Span::raw(if m.muted() {
            " unmute mic   "
        } else {
            " mute mic   "
        }));
    }
    footer_spans.extend([
        key("tab"),
        Span::raw(" recordings   "),
        key("q"),
        Span::raw(if state == State::Ready {
            " quit"
        } else {
            " save & quit"
        }),
    ]);
    f.render_widget(Line::from(footer_spans), footer);
}

fn draw_channel(f: &mut Frame, area: Rect, label: &str, ch: Option<&Channel>) {
    let Some(ch) = ch else {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().dim())
            .title(format!(" {label} · off "));
        f.render_widget(
            Paragraph::new("No microphone — recording call audio only")
                .dim()
                .block(block),
            area,
        );
        return;
    };

    let muted = ch.muted();
    let mut title = vec![
        Span::raw(format!(" {label} · ")),
        Span::raw(ch.name.clone()).bold(),
        Span::raw(" "),
    ];
    if muted {
        title.push(Span::styled(
            " MUTED ",
            Style::new().fg(Color::White).bg(Color::Red).bold(),
        ));
        title.push(Span::raw(" "));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(title));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [gauge_area, spark_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let color = if muted {
        Color::DarkGray
    } else {
        level_color(ch.level)
    };
    f.render_widget(
        Gauge::default()
            .ratio(level_ratio(ch.level))
            .label(db_label(ch.level))
            .use_unicode(true)
            .gauge_style(Style::new().fg(color).bg(Color::Black)),
        gauge_area,
    );

    let width = spark_area.width as usize;
    let data: Vec<u64> = ch
        .hist
        .iter()
        .skip(ch.hist.len().saturating_sub(width))
        .copied()
        .collect();
    f.render_widget(
        Sparkline::default().data(data).max(100).style(
            Style::new()
                .fg(if muted { Color::DarkGray } else { Color::Cyan })
                .add_modifier(Modifier::DIM),
        ),
        spark_area,
    );
}

fn collect_warnings(
    elapsed: Duration,
    call: &Channel,
    mic: Option<&Channel>,
) -> Vec<(Color, String)> {
    let mut w = Vec::new();
    for (label, ch) in [("Call audio", Some(call)), ("Microphone", mic)] {
        let Some(ch) = ch else { continue };
        if let Some(e) = ch.meter.error.lock().ok().and_then(|e| e.clone()) {
            w.push((Color::Red, format!("✗ {label}: {e}")));
        }
    }
    let silent_for = match call.last_sound {
        Some(t) => t.elapsed(),
        None => elapsed,
    };
    if silent_for >= SILENT_WARN_AFTER {
        let hint = if call.last_sound.is_none() && cfg!(target_os = "macos") {
            " If others are talking, allow your terminal app under System Settings › Privacy & \
             Security › Screen & System Audio Recording (\"System Audio Recording Only\"), then restart it."
        } else if call.last_sound.is_none() {
            " Check the call plays through the captured device (eavesdrop --list, --system)."
        } else {
            ""
        };
        w.push((
            Color::Yellow,
            format!("⚠ No call audio for {}.{hint}", fmt_duration(silent_for)),
        ));
    }
    if let Some(m) = mic
        && !m.muted()
        && m.last_sound.is_none()
        && elapsed >= SILENT_WARN_AFTER
    {
        w.push((
            Color::Yellow,
            "⚠ Microphone has been silent since the start — check mic permission / device.".into(),
        ));
    }
    w
}
