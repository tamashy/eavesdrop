//! Recordings list + player: a view of the main app, also usable on its own
//! (`eavesdrop play`). Recordings can be renamed, deleted and described.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use ratatui::{
    Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::{Line, Span},
    widgets::{Cell, Gauge, Padding, Paragraph, Row, Table, TableState, Wrap},
};

use crate::{
    input::{self, Action, TextInput},
    library::{self, Recording},
    meta,
    player::{MAX_VOLUME, Player},
    theme::{self, Themed, Tone},
    ui::{fmt_bytes, fmt_duration, short_path},
};

const TICK: Duration = Duration::from_millis(100);
const SEEK_SHORT: f64 = 5.0;
const SEEK_LONG: f64 = 30.0;
const VOLUME_STEP: f32 = 0.1;

enum Dialog {
    Rename {
        path: PathBuf,
        input: TextInput,
        error: Option<String>,
    },
    Describe {
        path: PathBuf,
        input: TextInput,
        error: Option<String>,
    },
    Delete {
        path: PathBuf,
        /// Moving to the Trash failed (reason): offer deleting permanently.
        trash_failed: Option<String>,
    },
}

pub struct Library {
    dir: PathBuf,
    /// Set while a recording is running: playback would be captured too.
    pub recording: bool,
    recs: Vec<Recording>,
    table: TableState,
    /// Currently loaded recording: (path, player).
    playing: Option<(PathBuf, Player)>,
    volume: f32,
    status: Option<(Tone, String)>,
    dialog: Option<Dialog>,
    /// Scroll offset of the description in the details card.
    card_scroll: u16,
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
            dialog: None,
            card_scroll: 0,
        };
        app.rescan()?;
        Ok(app)
    }

    pub fn rescan(&mut self) -> Result<()> {
        let selected = self.selected().map(|r| r.path.clone());
        let idx = self.table.selected().unwrap_or(0);
        self.recs = library::scan(&self.dir)?;
        // Keep the selection on the same file, or at the same row if it's gone.
        let idx = selected
            .and_then(|p| self.recs.iter().position(|r| r.path == p))
            .unwrap_or(idx.min(self.recs.len().saturating_sub(1)));
        self.table.select((!self.recs.is_empty()).then_some(idx));
        Ok(())
    }

    fn rescan_or_report(&mut self) {
        if let Err(e) = self.rescan() {
            self.set_status(Tone::Err, format!("✗ {e:#}"));
        }
    }

    fn selected(&self) -> Option<&Recording> {
        self.table.selected().and_then(|i| self.recs.get(i))
    }

    pub fn select_path(&mut self, path: &Path) {
        if let Some(i) = self.recs.iter().position(|r| r.path == path) {
            self.table.select(Some(i));
        }
    }

    pub fn set_status(&mut self, tone: Tone, text: impl Into<String>) {
        self.status = Some((tone, text.into()));
    }

    /// A dialog is open: it gets all keys (including q / Tab).
    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// Pasted text (bracketed paste) goes into the open text field.
    pub fn paste(&mut self, s: &str) {
        if let Some(Dialog::Rename { input, .. } | Dialog::Describe { input, .. }) =
            &mut self.dialog
        {
            input.insert_str(s);
        }
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
                Tone::Warn,
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
            Err(e) => self.status = Some((Tone::Err, format!("✗ {e:#}"))),
        }
    }

    fn player(&self) -> Option<&Player> {
        self.playing.as_ref().map(|(_, p)| p)
    }

    /// Stops playback if `path` is playing (the file must be closed to rename or
    /// delete it on Windows). Returns (position, paused) to resume afterwards.
    fn release(&mut self, path: &Path) -> Option<(Duration, bool)> {
        if self.playing.as_ref().is_some_and(|(p, _)| p == path) {
            let (_, p) = self.playing.take()?;
            return Some((p.position(), p.paused() || p.finished()));
        }
        None
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
        if next != cur {
            self.card_scroll = 0;
        }
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
        if self.dialog.is_some() {
            self.handle_dialog_key(k);
            return;
        }
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            // Clamped to the description's length when drawn.
            KeyCode::Char('J') => self.card_scroll = self.card_scroll.saturating_add(1),
            KeyCode::Char('K') => self.card_scroll = self.card_scroll.saturating_sub(1),
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

            KeyCode::Char('e') => {
                if let Some(r) = self.selected() {
                    self.dialog = Some(Dialog::Describe {
                        path: r.path.clone(),
                        input: TextInput::new(&r.description, true),
                        error: None,
                    });
                }
            }
            KeyCode::Char('r') | KeyCode::F(2) => {
                if let Some(r) = self.selected() {
                    let stem = r.path.file_stem().unwrap_or_default().to_string_lossy();
                    self.dialog = Some(Dialog::Rename {
                        path: r.path.clone(),
                        input: TextInput::new(&stem, false),
                        error: None,
                    });
                }
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(r) = self.selected() {
                    self.dialog = Some(Dialog::Delete {
                        path: r.path.clone(),
                        trash_failed: None,
                    });
                }
            }
            KeyCode::Char('R') | KeyCode::F(5) => self.rescan_or_report(),
            _ => {}
        }
    }

    fn handle_dialog_key(&mut self, k: KeyEvent) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        self.dialog = match dialog {
            Dialog::Rename {
                path,
                mut input,
                error,
            } => match input.handle_key(k) {
                Action::None => Some(Dialog::Rename { path, input, error }),
                Action::Cancel => None,
                Action::Submit => match self.rename(&path, &input.text()) {
                    Ok(()) => None,
                    Err(e) => Some(Dialog::Rename {
                        path,
                        input,
                        error: Some(format!("{e:#}")),
                    }),
                },
            },
            Dialog::Describe {
                path,
                mut input,
                error,
            } => match input.handle_key(k) {
                Action::None => Some(Dialog::Describe { path, input, error }),
                Action::Cancel => None,
                Action::Submit => match meta::set_description(&path, &input.text()) {
                    Ok(()) => {
                        self.set_status(
                            Tone::Ok,
                            format!("✓ Saved description of {}", name_of(&path)),
                        );
                        self.rescan_or_report();
                        None
                    }
                    Err(e) => Some(Dialog::Describe {
                        path,
                        input,
                        error: Some(format!("{e:#}")),
                    }),
                },
            },
            Dialog::Delete { path, trash_failed } => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.delete(path, trash_failed.is_some())
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => None,
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => None,
                _ => Some(Dialog::Delete { path, trash_failed }),
            },
        };
    }

    fn rename(&mut self, path: &Path, name: &str) -> Result<()> {
        let resume = self.release(path);
        let result = library::rename(path, name);
        let now_at = result.as_ref().map(PathBuf::as_path).unwrap_or(path);
        // Pick up where playback was, under the new name.
        if let Some((pos, paused)) = resume
            && let Ok(p) = Player::open(now_at, self.volume)
        {
            p.seek(pos);
            p.set_paused(paused);
            self.playing = Some((now_at.to_path_buf(), p));
        }
        let new = result?;
        self.rescan_or_report();
        self.select_path(&new);
        self.set_status(Tone::Ok, format!("✓ Renamed to {}", name_of(&new)));
        Ok(())
    }

    /// Deletes `path` (to the Trash, or permanently). Returns the follow-up dialog, if any.
    fn delete(&mut self, path: PathBuf, permanently: bool) -> Option<Dialog> {
        self.release(&path);
        let result = if permanently {
            library::delete(&path)
        } else {
            library::trash(&path)
        };
        match result {
            Ok(()) => {
                let how = if permanently {
                    "Deleted"
                } else if cfg!(windows) {
                    "Moved to the Recycle Bin:"
                } else {
                    "Moved to the Trash:"
                };
                self.set_status(Tone::Ok, format!("✓ {how} {}", name_of(&path)));
                self.rescan_or_report();
                None
            }
            Err(e) if !permanently => Some(Dialog::Delete {
                path,
                trash_failed: Some(format!("{e:#}")),
            }),
            Err(e) => {
                self.set_status(Tone::Err, format!("✗ {e:#}"));
                self.rescan_or_report();
                None
            }
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

fn name_of(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Runs the recordings browser. If `start` is given, selects and plays it.
pub fn run(dir: PathBuf, start: Option<PathBuf>) -> Result<()> {
    let mut app = Library::new(dir)?;
    if let Some(path) = start {
        app.select_path(&path);
        app.play_selected();
    }

    let mut terminal = crate::ui::init_terminal();
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| {
                crate::ui::fill_background(f);
                app.draw(f, f.area());
            })?;
            if !event::poll(TICK)? {
                continue;
            }
            match event::read()? {
                Event::Paste(s) => app.paste(&s),
                Event::Key(k) if k.kind == KeyEventKind::Press => {
                    if app.modal_open() {
                        app.handle_key(k);
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(());
                        }
                        KeyCode::Char('t') => {
                            let t = theme::cycle();
                            app.set_status(Tone::Ok, format!("Theme: {}", t.name));
                        }
                        _ => app.handle_key(k),
                    }
                }
                _ => {}
            }
        }
    })();
    crate::ui::restore_terminal();
    result
}

impl Library {
    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        draw(f, self, area);
        draw_dialog(f, self, area);
    }
}

pub fn key_span(k: &'static str) -> Span<'static> {
    Span::styled(format!(" {k} "), theme::get().key())
}

/// The details card goes to the right of the list from this width on, below it otherwise.
const SIDE_CARD_MIN_WIDTH: u16 = 100;

fn draw(f: &mut Frame, app: &mut Library, area: Rect) {
    let t = theme::get();
    let [main_area, now_area, status_area, footer] = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(5),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(area);
    let side = main_area.width >= SIDE_CARD_MIN_WIDTH;
    let [list_area, card_area] = if side {
        let w = (main_area.width * 2 / 5).clamp(36, 64);
        Layout::horizontal([Constraint::Min(40), Constraint::Length(w)]).areas(main_area)
    } else {
        Layout::vertical([Constraint::Fill(1), Constraint::Fill(1)]).areas(main_area)
    };

    // ---- list --------------------------------------------------------------
    let playing_path = app.playing.as_ref().map(|(p, _)| p.clone());
    let block = theme::panel().title(Line::from(vec![
        Span::raw(" Recordings · ").bold(),
        Span::raw(short_path(
            &app.dir,
            list_area.width.saturating_sub(24) as usize,
        )),
        Span::raw(format!(" ({}) ", app.recs.len())).subtle(),
    ]));
    if app.recs.is_empty() {
        f.render_widget(
            Paragraph::new("No recordings yet. Run `eavesdrop` and press Space to record.")
                .subtle()
                .wrap(Wrap { trim: true })
                .block(block),
            list_area,
        );
    } else {
        let name_w = app
            .recs
            .iter()
            .map(|r| r.name.chars().count())
            .max()
            .unwrap_or(4)
            .clamp(4, 40) as u16;
        let rows = app.recs.iter().map(|r| {
            let marker = if playing_path.as_ref() == Some(&r.path) {
                "▶"
            } else {
                " "
            };
            Row::new(vec![
                Cell::from(marker).fg(t.ok),
                Cell::from(library::fmt_modified(r.modified)),
                Cell::from(
                    r.duration
                        .map(fmt_duration)
                        .unwrap_or_else(|| "--:--:--".into()),
                ),
                Cell::from(format!("{:>9}", fmt_bytes(r.size))),
                Cell::from(r.name.clone()),
                Cell::from(r.summary().to_string()).fg(t.info),
            ])
        });
        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(16),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(name_w),
                Constraint::Fill(1),
            ],
        )
        .header(
            Row::new(vec!["", "Date", "Length", "     Size", "File", "About"]).style(t.subtle()),
        )
        .column_spacing(2)
        .row_highlight_style(Style::new().bg(t.highlight).bold())
        .block(block);
        f.render_stateful_widget(table, list_area, &mut app.table);
    }

    // ---- details card of the selected recording ----------------------------
    draw_card(f, app, card_area, playing_path.as_deref(), side);

    // ---- now playing -------------------------------------------------------
    let now_block = theme::panel().title(" Now playing ");
    match &app.playing {
        None => f.render_widget(
            Paragraph::new("Nothing playing — select a recording and press Enter")
                .subtle()
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
                ("■ ", t.off)
            } else if p.paused() {
                ("❚❚ ", t.warn)
            } else {
                ("▶ ", t.ok)
            };
            f.render_widget(
                Line::from(vec![
                    Span::styled(icon, Style::new().fg(color).bold()),
                    Span::raw(name_of(path)).bold(),
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
                    .gauge_style(Style::new().fg(t.info).bg(t.track)),
                bar,
            );
            let width = 20usize;
            let filled = ((app.volume / MAX_VOLUME) * width as f32).round() as usize;
            f.render_widget(
                Line::from(vec![
                    Span::raw("Volume ").subtle(),
                    Span::styled(
                        "█".repeat(filled),
                        Style::new().fg(if app.volume > 1.0 { t.warn } else { t.ok }),
                    ),
                    Span::styled("░".repeat(width - filled), Style::new().fg(t.off)),
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
            .map(|e| (Tone::Err, format!("✗ {e}")))
    });
    if let Some((tone, text)) = status {
        f.render_widget(
            Line::from(Span::styled(text, Style::new().fg(tone.color()))),
            status_area,
        );
    }

    // ---- footer ------------------------------------------------------------
    let key = key_span;
    f.render_widget(
        Paragraph::new(vec![
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
                Span::raw(" volume"),
            ]),
            Line::from(vec![
                key("e"),
                Span::raw(" describe  "),
                key("J K"),
                Span::raw(" scroll  "),
                key("r"),
                Span::raw(" rename  "),
                key("d"),
                Span::raw(" delete  "),
                key("F5"),
                Span::raw(" refresh  "),
                key("t"),
                Span::raw(" theme  "),
                key("q"),
                Span::raw(" quit"),
            ]),
        ]),
        footer,
    );
}

/// Details of the selected recording: file info and the full description.
/// `tall`: list the file info one item per line (side card), else on one line.
fn draw_card(f: &mut Frame, app: &mut Library, area: Rect, playing: Option<&Path>, tall: bool) {
    let t = theme::get();
    let block = theme::panel()
        .title(" Details ")
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(r) = app.selected() else {
        f.render_widget(Paragraph::new("No recording selected").subtle(), inner);
        return;
    };

    let field = |label: &'static str, value: String| {
        Line::from(vec![
            Span::raw(format!("{label:<8}")).subtle(),
            Span::raw(value),
        ])
    };
    let mut head = vec![Line::from(Span::raw(r.name.clone()).bold())];
    if playing == Some(r.path.as_path()) {
        head.push(Line::from(Span::styled(
            "▶ loaded in player",
            Style::new().fg(t.ok),
        )));
    }
    let date = library::fmt_modified(r.modified);
    let length = r
        .duration
        .map(fmt_duration)
        .unwrap_or_else(|| "--:--:--".into());
    let size = fmt_bytes(r.size);
    if tall {
        head.extend([
            field("Date", date),
            field("Length", length),
            field("Size", size),
        ]);
    } else {
        head.push(Line::from(vec![
            Span::raw(date),
            Span::raw(" · ").subtle(),
            Span::raw(length),
            Span::raw(" · ").subtle(),
            Span::raw(size),
        ]));
    }
    let head = Paragraph::new(head).wrap(Wrap { trim: false });
    let head_h = (head.line_count(inner.width) as u16).min(inner.height);
    let description = r.description.clone();

    let [head_area, rule_area, about_area] = Layout::vertical([
        Constraint::Length(head_h),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    f.render_widget(head, head_area);
    f.render_widget(
        Line::from(vec![
            Span::styled("About ", Style::new().fg(t.accent).bold()),
            Span::styled(
                "─".repeat(rule_area.width.saturating_sub(6) as usize),
                Style::new().fg(t.border),
            ),
        ]),
        rule_area,
    );

    if description.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("No description — press ").subtle(),
                key_span("e"),
                Span::raw(" to add what this recording is about").subtle(),
            ]))
            .wrap(Wrap { trim: false }),
            about_area,
        );
        return;
    }
    let about = Paragraph::new(description)
        .fg(t.info)
        .wrap(Wrap { trim: false });
    let total = about.line_count(about_area.width) as u16;
    let max_scroll = total.saturating_sub(about_area.height);
    app.card_scroll = app.card_scroll.min(max_scroll);
    f.render_widget(about.scroll((app.card_scroll, 0)), about_area);

    // Scroll position hint in the bottom border when it doesn't all fit.
    if max_scroll > 0 && area.height > 2 {
        let more = match (app.card_scroll > 0, app.card_scroll < max_scroll) {
            (true, true) => " ↑↓ more · J K ",
            (false, _) => " ↓ more · J K ",
            (true, false) => " ↑ more · J K ",
        };
        let w = more.chars().count() as u16;
        let x = area.right().saturating_sub(w + 2).max(area.x + 1);
        f.render_widget(
            Span::raw(more).subtle(),
            Rect::new(x, area.bottom() - 1, w.min(area.width.saturating_sub(2)), 1),
        );
    }
}

fn draw_dialog(f: &mut Frame, app: &mut Library, area: Rect) {
    let Some(dialog) = &mut app.dialog else {
        return;
    };
    let error_line = |e: &String| {
        Line::from(Span::styled(
            format!("✗ {e}"),
            Style::new().fg(theme::get().err),
        ))
    };
    match dialog {
        Dialog::Rename { path, input, error } => {
            let mut info = vec![Line::from(vec![
                Span::raw("Current: ").subtle(),
                Span::raw(name_of(path)),
            ])];
            info.extend(error.iter().map(error_line));
            let hint = Line::from(vec![
                key_span("enter"),
                Span::raw(" rename   "),
                key_span("esc"),
                Span::raw(" cancel   .wav is added automatically"),
            ]);
            input::draw_input(f, area, "Rename recording".into(), info, input, 1, hint);
        }
        Dialog::Describe { path, input, error } => {
            let mut info = vec![Line::from(vec![
                Span::raw("What is ").subtle(),
                Span::raw(name_of(path)).bold(),
                Span::raw(" about? Topic, people, key points…").subtle(),
            ])];
            info.extend(error.iter().map(error_line));
            input::draw_input(
                f,
                area,
                "Describe recording".into(),
                info,
                input,
                8,
                describe_hint(input),
            );
        }
        Dialog::Delete { path, trash_failed } => {
            let name = name_of(path);
            let also = if meta::path_for(path).exists() {
                " and its description"
            } else {
                ""
            };
            match trash_failed {
                None => {
                    let bin = if cfg!(windows) {
                        "Recycle Bin"
                    } else {
                        "Trash"
                    };
                    input::draw_confirm(
                        f,
                        area,
                        "Delete recording".into(),
                        vec![
                            Line::from(vec![
                                Span::raw("Move "),
                                Span::raw(name).bold(),
                                Span::raw(format!("{also} to the {bin}?")),
                            ]),
                            Line::from(format!("You can restore it from the {bin}.")).subtle(),
                        ],
                        Tone::Warn,
                    );
                }
                Some(reason) => input::draw_confirm(
                    f,
                    area,
                    "Delete permanently?".into(),
                    vec![
                        Line::from(Span::styled(
                            format!("Couldn't move it to the Trash: {reason}"),
                            Style::new().fg(theme::get().err),
                        )),
                        Line::from(vec![
                            Span::raw("Delete "),
                            Span::raw(name).bold(),
                            Span::raw(format!("{also} permanently? This can't be undone.")),
                        ]),
                    ],
                    Tone::Err,
                ),
            }
        }
    }
}

/// Hint line under a description editor.
pub fn describe_hint(input: &TextInput) -> Line<'static> {
    if input.discard_armed() {
        Line::from(Span::styled(
            "Unsaved changes — press Esc again to discard, Ctrl+S to save",
            Style::new().fg(theme::get().warn),
        ))
    } else {
        Line::from(vec![
            key_span("ctrl+s"),
            Span::raw(" save   "),
            key_span("esc"),
            Span::raw(" cancel   "),
            key_span("enter"),
            Span::raw(" new line"),
        ])
    }
}

/// Plain listing for `eavesdrop recordings`.
pub fn print_list(dir: &Path) -> Result<()> {
    let recs = library::scan(dir)?;
    println!("Recordings in {}", dir.display());
    if recs.is_empty() {
        println!("  (none)");
    }
    for r in recs {
        let about = match r.summary() {
            "" => String::new(),
            s => format!("  — {s}"),
        };
        println!(
            "  {}  {}  {:>9}  {}{about}",
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

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn sample_dir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("eavesdrop-browser-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let wav = d.join("standup.wav");
        let mut w = hound::WavWriter::create(&wav, spec).unwrap();
        for _ in 0..8000 {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
        meta::set_description(
            &wav,
            "Weekly standup\nAction items:\n- bump version\n- retag",
        )
        .unwrap();
        d
    }

    fn render(app: &mut Library, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f, f.area())).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn details_card_shows_every_description_line() {
        let dir = sample_dir("card");
        let mut app = Library::new(dir.clone()).unwrap();
        for (w, h) in [(120, 24), (80, 30)] {
            let screen = render(&mut app, w, h);
            if std::env::var_os("SHOW_SCREEN").is_some() {
                println!("{screen}\n");
            }
            assert!(screen.contains("Details"), "{w}x{h}:\n{screen}");
            for line in [
                "Weekly standup",
                "Action items:",
                "- bump version",
                "- retag",
            ] {
                assert!(screen.contains(line), "{w}x{h} missing {line:?}:\n{screen}");
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
