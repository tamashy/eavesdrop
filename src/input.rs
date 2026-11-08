//! Minimal text input for dialogs: single-line (rename) or multi-line with
//! soft wrapping (description), plus popup helpers.

use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};

pub enum Action {
    None,
    Submit,
    Cancel,
}

pub struct TextInput {
    chars: Vec<char>,
    /// Cursor position as an index into `chars`.
    cursor: usize,
    multiline: bool,
    original: String,
    scroll: usize,
    /// Multi-line: Esc was pressed once with unsaved changes.
    discard_armed: bool,
}

impl TextInput {
    pub fn new(text: &str, multiline: bool) -> Self {
        let chars: Vec<char> = text.chars().collect();
        Self {
            cursor: chars.len(),
            chars,
            multiline,
            original: text.to_string(),
            scroll: 0,
            discard_armed: false,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn changed(&self) -> bool {
        self.text() != self.original
    }

    pub fn discard_armed(&self) -> bool {
        self.discard_armed
    }

    /// Inserts text at the cursor (typing or paste). Single-line inputs get newlines as spaces.
    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        for c in s.chars() {
            let c = if c == '\n' && !self.multiline { ' ' } else { c };
            if c == '\n' || c == '\t' || !c.is_control() {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
        }
        self.discard_armed = false;
    }

    pub fn handle_key(&mut self, k: KeyEvent) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if k.code != KeyCode::Esc {
            self.discard_armed = false;
        }
        match k.code {
            KeyCode::Esc => {
                if self.multiline && self.changed() && !self.discard_armed {
                    self.discard_armed = true;
                    return Action::None;
                }
                return Action::Cancel;
            }
            KeyCode::Char('c') if ctrl => return Action::Cancel,
            KeyCode::Char('s') if ctrl => return Action::Submit,
            KeyCode::F(2) => return Action::Submit,
            KeyCode::Enter if !self.multiline || alt || ctrl => return Action::Submit,
            KeyCode::Enter => self.insert_str("\n"),
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start(self.cursor);
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char('w') if ctrl => self.delete_word_back(),
            KeyCode::Backspace if alt || ctrl => self.delete_word_back(),
            KeyCode::Char(c) if !ctrl => self.insert_str(&c.to_string()),
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = self.line_start(self.cursor),
            KeyCode::End => self.cursor = self.line_end(self.cursor),
            KeyCode::Up if self.multiline => self.move_line(-1),
            KeyCode::Down if self.multiline => self.move_line(1),
            _ => {}
        }
        Action::None
    }

    fn line_start(&self, i: usize) -> usize {
        self.chars[..i]
            .iter()
            .rposition(|&c| c == '\n')
            .map_or(0, |p| p + 1)
    }

    fn line_end(&self, i: usize) -> usize {
        self.chars[i..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(self.chars.len(), |p| i + p)
    }

    fn move_line(&mut self, dir: i32) {
        let start = self.line_start(self.cursor);
        let col = self.cursor - start;
        let target_start = if dir < 0 {
            if start == 0 {
                self.cursor = 0;
                return;
            }
            self.line_start(start - 1)
        } else {
            let end = self.line_end(self.cursor);
            if end == self.chars.len() {
                self.cursor = end;
                return;
            }
            end + 1
        };
        let target_end = self.line_end(target_start);
        self.cursor = target_start + col.min(target_end - target_start);
    }

    fn delete_word_back(&mut self) {
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.chars.drain(i..self.cursor);
        self.cursor = i;
    }

    /// Visual rows (start, end) for `width` columns, and the cursor's (row, col).
    fn layout(&self, width: usize) -> (Vec<(usize, usize)>, (usize, usize)) {
        let width = width.max(1);
        let mut rows = Vec::new();
        let (mut row, mut col, mut start) = (0usize, 0usize, 0usize);
        let mut cur = None;
        for (i, &c) in self.chars.iter().enumerate() {
            if c != '\n' && col == width {
                rows.push((start, i));
                (row, col, start) = (row + 1, 0, i);
            }
            if i == self.cursor {
                cur = Some((row, col));
            }
            if c == '\n' {
                rows.push((start, i));
                (row, col, start) = (row + 1, 0, i + 1);
            } else {
                col += 1;
            }
        }
        if cur.is_none() {
            if col == width {
                rows.push((start, self.chars.len()));
                (row, col, start) = (row + 1, 0, self.chars.len());
            }
            cur = Some((row, col));
        }
        rows.push((start, self.chars.len()));
        (rows, cur.unwrap_or((0, 0)))
    }

    /// Renders the text into `area` and places the terminal cursor.
    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        if !self.multiline {
            // Single line: scroll horizontally to keep the cursor visible.
            let w = area.width as usize - 1;
            let skip = self.cursor.saturating_sub(w);
            let text: String = self.chars.iter().skip(skip).take(w + 1).collect();
            f.render_widget(Line::from(text), area);
            f.set_cursor_position((area.x + (self.cursor - skip) as u16, area.y));
            return;
        }
        let (rows, (crow, ccol)) = self.layout(area.width as usize);
        let h = area.height as usize;
        if crow < self.scroll {
            self.scroll = crow;
        } else if crow >= self.scroll + h {
            self.scroll = crow + 1 - h;
        }
        let lines: Vec<Line> = rows
            .iter()
            .skip(self.scroll)
            .take(h)
            .map(|&(s, e)| Line::from(self.chars[s..e].iter().collect::<String>()))
            .collect();
        f.render_widget(Paragraph::new(lines), area);
        f.set_cursor_position((area.x + ccol as u16, area.y + (crow - self.scroll) as u16));
    }
}

/// A rectangle of at most `w`×`h` centered in `area`.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [r] = Layout::vertical([Constraint::Length(h.min(area.height))])
        .flex(Flex::Center)
        .areas(r);
    r
}

fn popup_block(title: String, color: Color) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::raw(format!(" {title} ")).bold())
}

/// Draws a dialog with a text input: `info` lines above, `hint` below.
pub fn draw_input(
    f: &mut Frame,
    area: Rect,
    title: String,
    info: Vec<Line>,
    input: &mut TextInput,
    input_height: u16,
    hint: Line,
) {
    let h = input_height + info.len() as u16 + 4;
    let popup = centered(area, 76, h);
    f.render_widget(Clear, popup);
    let block = popup_block(title, Color::Cyan);
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let [info_area, input_area, _, hint_area] = Layout::vertical([
        Constraint::Length(info.len() as u16),
        Constraint::Length(input_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(info).wrap(Wrap { trim: false }), info_area);
    let field = input_area;
    f.render_widget(
        Block::new().style(Style::new().bg(Color::Rgb(40, 40, 48))),
        field,
    );
    input.render(f, field);
    f.render_widget(hint.dim(), hint_area);
}

/// Draws a yes/no confirmation dialog.
pub fn draw_confirm(f: &mut Frame, area: Rect, title: String, body: Vec<Line>, color: Color) {
    let popup = centered(area, 76, body.len() as u16 + 4);
    f.render_widget(Clear, popup);
    let block = popup_block(title, color);
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let mut lines = body;
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" y ", Style::new().fg(Color::Black).bg(color)),
        Span::raw(" yes   "),
        Span::styled(" n ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" / "),
        Span::styled(" esc ", Style::new().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" no"),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_editing_and_submit() {
        let mut t = TextInput::new("memo", false);
        t.insert_str("_x");
        t.handle_key(key(KeyCode::Backspace));
        t.handle_key(key(KeyCode::Home));
        t.handle_key(key(KeyCode::Delete));
        assert_eq!(t.text(), "emo_");
        assert!(matches!(t.handle_key(key(KeyCode::Enter)), Action::Submit));
    }

    #[test]
    fn single_line_paste_flattens_newlines() {
        let mut t = TextInput::new("", false);
        t.insert_str("a\nb\r\nc");
        assert_eq!(t.text(), "a b c");
    }

    #[test]
    fn multiline_enter_inserts_and_up_down_keep_column() {
        let mut t = TextInput::new("abcd", true);
        t.handle_key(key(KeyCode::Enter));
        t.insert_str("xy");
        t.handle_key(key(KeyCode::Up)); // col 2 on line 0
        t.insert_str("!");
        assert_eq!(t.text(), "ab!cd\nxy");
        t.handle_key(key(KeyCode::Down));
        t.handle_key(key(KeyCode::End));
        assert_eq!(t.cursor, t.chars.len());
    }

    #[test]
    fn esc_with_changes_needs_confirmation_in_multiline() {
        let mut t = TextInput::new("a", true);
        t.insert_str("b");
        assert!(matches!(t.handle_key(key(KeyCode::Esc)), Action::None));
        assert!(t.discard_armed());
        assert!(matches!(t.handle_key(key(KeyCode::Esc)), Action::Cancel));
        let mut u = TextInput::new("a", true);
        assert!(matches!(u.handle_key(key(KeyCode::Esc)), Action::Cancel));
    }

    #[test]
    fn ctrl_w_deletes_word() {
        let mut t = TextInput::new("hello big world", false);
        t.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(t.text(), "hello big ");
    }

    #[test]
    fn wrap_layout_places_cursor() {
        let t = TextInput::new("abcdef\ngh", true);
        let (rows, cur) = t.layout(4);
        assert_eq!(rows, vec![(0, 4), (4, 6), (7, 9)]);
        assert_eq!(cur, (2, 2));
        let t = TextInput::new("abcd", true); // cursor at exact wrap point → next row
        assert_eq!(t.layout(4).1, (1, 0));
    }
}
