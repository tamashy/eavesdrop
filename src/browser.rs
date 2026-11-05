//! Recordings list + player: a view of the main app, also usable on its own
//! (`eavesdrop play`).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Gauge, Paragraph, Row, Table, TableState},
};

use crate::{
    library::{self, Recording},
    player::{MAX_VOLUME, Player},
    ui::{fmt_bytes, fmt_duration, short_path},
};

const TICK: Duration = Duration::from_millis(100);
const SEEK_SHORT: f64 = 5.0;
const SEEK_LONG: f64 = 30.0;
const VOLUME_STEP: f32 = 0.1;

pub struct Library {
    dir: PathBuf,
    /// Set while a recording is running: playback would be captured too.
    pub recording: bool,
    recs: Vec<Recording>,
    table: TableState,
    /// Currently loaded recording: (path, player).
    playing: Option<(PathBuf, Player)>,
    volume: f32,
    status: Option<(Color, String)>,
}

impl Library {
    pub fn new(dir: PathBuf) -> Result<Self> {
        let mut app = Self {
            dir,
            recording: false,
            recs: Vec::new(),
            table: TableState::default(),
            playing: None,
            volume: 1.0,
            status: None,
        };
        app.rescan()?;
        Ok(app)
    }

    pub fn rescan(&mut self) -> Result<()> {
        let selected = self.selected().map(|r| r.path.clone());
        self.recs = library::scan(&self.dir)?;
        let idx = selected
            .and_then(|p| self.recs.iter().position(|r| r.path == p))
            .unwrap_or(0);
        self.table.select((!self.recs.is_empty()).then_some(idx));
        Ok(())
    }

    fn selected(&self) -> Option<&Recording> {
        self.table.selected().and_then(|i| self.recs.get(i))
    }

    pub fn select_path(&mut self, path: &Path) {
        if let Some(i) = self.recs.iter().position(|r| r.path == path) {
            self.table.select(Some(i));
        }
    }

    pub fn set_status(&mut self, color: Color, text: impl Into<String>) {
        self.status = Some((color, text.into()));
    }

    /// Pauses playback (e.g. because a recording is starting).
    pub fn pause_playback(&self) {
        if let Some(p) = self.player() {
            p.set_paused(true);
        }
    }

    fn refuse_while_recording(&mut self) -> bool {
        if self.recording {
            self.set_status(
                Color::Yellow,
                "⏺ Recording in progress — pause or stop it to listen (playback would be recorded too)",
            );
        }
        self.recording
    }

    fn play_selected(&mut self) {
        let Some(path) = self.selected().map(|r| r.path.clone()) else {
            return;
        };
        if self.refuse_while_recording() {
            return;
        }
        self.playing = None; // stop the previous one first (releases the device)
        match Player::open(&path, self.volume) {
            Ok(p) => {
                self.playing = Some((path, p));
                self.status = None;
            }
            Err(e) => self.status = Some((Color::Red, format!("✗ {e:#}"))),
        }
    }

    fn player(&self) -> Option<&Player> {
        self.playing.as_ref().map(|(_, p)| p)
    }

    fn set_volume(&mut self, v: f32) {
        self.volume = (v.clamp(0.0, MAX_VOLUME) * 10.0).round() / 10.0;
        if let Some(p) = self.player() {
            p.set_volume(self.volume);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.recs.is_empty() {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.recs.len() as isize - 1);
        self.table.select(Some(next as usize));
    }

    fn toggle_playback(&mut self) {
        match self.player() {
            Some(p) if p.paused() || p.finished() => {
                if !self.refuse_while_recording() {
                    self.player().map(Player::toggle);
                    self.status = None;
                }
            }
            Some(p) => p.toggle(),
            None => self.play_selected(),
        }
    }

    pub fn handle_key(&mut self, k: KeyEvent) {
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Enter => self.play_selected(),
            KeyCode::Char(' ') | KeyCode::Char('p') => self.toggle_playback(),

            KeyCode::Left | KeyCode::Char('h') => {
                self.seek_by(if shift { -SEEK_LONG } else { -SEEK_SHORT })
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.seek_by(if shift { SEEK_LONG } else { SEEK_SHORT })
            }
            KeyCode::Char('[') => self.seek_by(-SEEK_LONG),
            KeyCode::Char(']') => self.seek_by(SEEK_LONG),
            KeyCode::Home => self.seek_by(f64::NEG_INFINITY),
            KeyCode::Char(c @ '0'..='9') => {
                if let Some(p) = self.player() {
                    p.seek_ratio(c.to_digit(10).unwrap_or(0) as f64 / 10.0);
                }
            }

            KeyCode::Char('+') | KeyCode::Char('=') => self.set_volume(self.volume + VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.set_volume(self.volume - VOLUME_STEP),

            KeyCode::Char('r') => {
                if let Err(e) = self.rescan() {
                    self.status = Some((Color::Red, format!("✗ {e:#}")));
                }
            }
            _ => {}
        }
    }

    fn seek_by(&self, secs: f64) {
        if let Some(p) = self.player() {
            if secs.is_infinite() {
                p.seek(Duration::ZERO);
            } else {
                p.seek_by(secs);
            }
        }
    }
}

/// Runs the recordings browser. If `start` is given, selects and plays it.
pub fn run(dir: PathBuf, start: Option<PathBuf>) -> Result<()> {
    let mut app = Library::new(dir)?;
    if let Some(path) = start {
        app.select_path(&path);
        app.play_selected();
    }

    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.draw(f, f.area()))?;
            if event::poll(TICK)?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    _ => app.handle_key(k),
                }
            }
        }
    })();
    ratatui::restore();
    result
}

impl Library {
    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        draw(f, self, area);
    }
}

fn draw(f: &mut Frame, app: &mut Library, area: Rect) {
    let [list_area, now_area, status_area, footer] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);

    // ---- list --------------------------------------------------------------
    let playing_path = app.playing.as_ref().map(|(p, _)| p.clone());
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![
            Span::raw(" Recordings · ").bold(),
            Span::raw(short_path(
                &app.dir,
                list_area.width.saturating_sub(24) as usize,
            )),
            Span::raw(format!(" ({}) ", app.recs.len())).dim(),
        ]));
    if app.recs.is_empty() {
        f.render_widget(
            Paragraph::new("No recordings yet. Run `eavesdrop` to record a call.")
                .dim()
                .block(block),
            list_area,
        );
    } else {
        let rows = app.recs.iter().map(|r| {
            let marker = if playing_path.as_ref() == Some(&r.path) {
                "▶"
            } else {
                " "
            };
            Row::new(vec![
                Cell::from(marker).fg(Color::Green),
                Cell::from(library::fmt_modified(r.modified)),
                Cell::from(
                    r.duration
                        .map(fmt_duration)
                        .unwrap_or_else(|| "--:--:--".into()),
                ),
                Cell::from(format!("{:>9}", fmt_bytes(r.size))),
                Cell::from(r.name.clone()),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Min(10),
            ],
        )
        .header(Row::new(vec!["", "Date", "Length", "     Size", "File"]).style(Style::new().dim()))
        .column_spacing(2)
        .row_highlight_style(Style::new().bg(Color::DarkGray).bold())
        .block(block);
        f.render_stateful_widget(table, list_area, &mut app.table);
    }

    // ---- now playing -------------------------------------------------------
    let now_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Now playing ");
    match &app.playing {
        None => f.render_widget(
            Paragraph::new("Nothing playing — select a recording and press Enter")
                .dim()
                .block(now_block),
            now_area,
        ),
        Some((path, p)) => {
            let inner = now_block.inner(now_area);
            f.render_widget(now_block, now_area);
            let [title, bar, vol] = Layout::vertical([Constraint::Length(1); 3]).areas(inner);

            let pos = p.position();
            let dur = p.duration();
            let (icon, color) = if p.finished() {
                ("■ ", Color::DarkGray)
            } else if p.paused() {
                ("❚❚ ", Color::Yellow)
            } else {
                ("▶ ", Color::Green)
            };
            f.render_widget(
                Line::from(vec![
                    Span::styled(icon, Style::new().fg(color).bold()),
                    Span::raw(path.file_name().unwrap_or_default().to_string_lossy()).bold(),
                ]),
                title,
            );
            let ratio = if dur.is_zero() {
                0.0
            } else {
                (pos.as_secs_f64() / dur.as_secs_f64()).clamp(0.0, 1.0)
            };
            f.render_widget(
                Gauge::default()
                    .ratio(ratio)
                    .label(format!("{} / {}", fmt_duration(pos), fmt_duration(dur)))
                    .use_unicode(true)
                    .gauge_style(Style::new().fg(Color::Cyan).bg(Color::Black)),
                bar,
            );
            let width = 20usize;
            let filled = ((app.volume / MAX_VOLUME) * width as f32).round() as usize;
            f.render_widget(
                Line::from(vec![
                    Span::raw("Volume ").dim(),
                    Span::styled(
                        "█".repeat(filled),
                        Style::new().fg(if app.volume > 1.0 {
                            Color::Yellow
                        } else {
                            Color::Green
                        }),
                    ),
                    Span::raw("░".repeat(width - filled)).dim(),
                    Span::raw(format!(" {:>3.0}%", app.volume * 100.0)),
                ]),
                vol,
            );
        }
    }

    // ---- status ------------------------------------------------------------
    let status = app.status.clone().or_else(|| {
        app.player()
            .and_then(|p| p.error())
            .map(|e| (Color::Red, format!("✗ {e}")))
    });
    if let Some((color, text)) = status {
        f.render_widget(
            Line::from(Span::styled(text, Style::new().fg(color))),
            status_area,
        );
    }

    // ---- footer ------------------------------------------------------------
    let key = |k: &'static str| {
        Span::styled(
            format!(" {k} "),
            Style::new().fg(Color::Black).bg(Color::Gray),
        )
    };
    f.render_widget(
        Line::from(vec![
            key("↑↓"),
            Span::raw(" select  "),
            key("enter"),
            Span::raw(" play  "),
            key("space"),
            Span::raw(" pause  "),
            key("←→"),
            Span::raw(" ±5s  "),
            key("[ ]"),
            Span::raw(" ±30s  "),
            key("0-9"),
            Span::raw(" jump  "),
            key("+ -"),
            Span::raw(" volume  "),
            key("r"),
            Span::raw(" refresh  "),
            key("q"),
            Span::raw(" quit"),
        ]),
        footer,
    );
}

/// Plain listing for `eavesdrop recordings`.
pub fn print_list(dir: &Path) -> Result<()> {
    let recs = library::scan(dir)?;
    println!("Recordings in {}", dir.display());
    if recs.is_empty() {
        println!("  (none)");
    }
    for r in recs {
        println!(
            "  {}  {}  {:>9}  {}",
            library::fmt_modified(r.modified),
            r.duration
                .map(fmt_duration)
                .unwrap_or_else(|| "--:--:--".into()),
            fmt_bytes(r.size),
            r.name
        );
    }
    Ok(())
}
