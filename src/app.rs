use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::Backend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};

use crate::{
    fs_tree::{FileTree, TreeEntryKind},
    review::{FileReview, LineState, ReviewStats, ReviewStore},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pane {
    Overview,
    Files,
    Viewer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitignorePrompt {
    Hidden,
    Waiting,
}

pub struct App {
    root: PathBuf,
    repo_link: Option<String>,
    tree: FileTree,
    store: ReviewStore,
    active_pane: Pane,
    current_file: Option<PathBuf>,
    current_review: Option<FileReview>,
    current_line: usize,
    scroll: u16,
    viewer_height: usize,
    stats: ReviewStats,
    prompt: GitignorePrompt,
    status: String,
}

impl App {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = root.canonicalize()?;
        let tree = FileTree::new(root.clone())?;
        let mut store = ReviewStore::open(root.clone())?;
        let prompt = if store.needs_gitignore_prompt() {
            GitignorePrompt::Waiting
        } else {
            GitignorePrompt::Hidden
        };
        let files = tree.files();
        let stats = store.workspace_stats(files)?;

        Ok(Self {
            repo_link: git_repo_link(&root),
            root,
            tree,
            store,
            active_pane: Pane::Files,
            current_file: None,
            current_review: None,
            current_line: 0,
            scroll: 0,
            viewer_height: 12,
            stats,
            prompt,
            status: "Ready".to_string(),
        })
    }

    pub fn run<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        loop {
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(Duration::from_millis(200))?
                && let Event::Key(key) = event::read()?
                && self.handle_key(key)?
            {
                return Ok(());
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.prompt == GitignorePrompt::Waiting {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.store.answer_gitignore_prompt(true)?;
                    self.prompt = GitignorePrompt::Hidden;
                    self.status = ".debth/ added to .gitignore".to_string();
                    return Ok(false);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.store.answer_gitignore_prompt(false)?;
                    self.prompt = GitignorePrompt::Hidden;
                    self.status = ".debth/ will not be added to .gitignore".to_string();
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('1') => self.active_pane = Pane::Overview,
            KeyCode::Char('2') => self.active_pane = Pane::Files,
            KeyCode::Char('3') => self.active_pane = Pane::Viewer,
            KeyCode::Tab => self.next_pane(),
            KeyCode::BackTab => self.previous_pane(),
            KeyCode::Char('h') => self.previous_pane(),
            KeyCode::Char('l') => self.next_pane(),
            KeyCode::Char('a') if self.active_pane == Pane::Files => {
                self.mark_selected_file(LineState::Accepted)?;
            }
            KeyCode::Char('r') if self.active_pane == Pane::Files => {
                self.mark_selected_file(LineState::Rejected)?;
            }
            KeyCode::Char('i') if self.active_pane == Pane::Files => {
                self.ignore_selected_entry()?;
            }
            KeyCode::Char('a') if self.active_pane == Pane::Viewer => {
                self.mark_current_line(LineState::Accepted)?;
            }
            KeyCode::Char('r') if self.active_pane == Pane::Viewer => {
                self.mark_current_line(LineState::Rejected)?;
            }
            KeyCode::Char('u') if self.active_pane == Pane::Viewer => {
                self.mark_current_line(LineState::Unreviewed)?;
            }
            KeyCode::Char('g') => self.move_viewer_to_start(),
            KeyCode::Char('G') => self.move_viewer_to_end(),
            KeyCode::Enter => self.open_or_toggle_selected()?,
            KeyCode::Char(' ') if self.active_pane == Pane::Files => {
                self.open_or_toggle_selected()?
            }
            KeyCode::Left if self.active_pane == Pane::Files => {
                self.tree.collapse_selected()?;
            }
            KeyCode::Right if self.active_pane == Pane::Files => {
                if let Some(path) = self.tree.expand_selected()? {
                    self.open_file(path)?;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_up()?,
            KeyCode::Down | KeyCode::Char('j') => self.move_down()?,
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            _ => {}
        }

        Ok(false)
    }

    fn next_pane(&mut self) {
        self.active_pane = match self.active_pane {
            Pane::Overview => Pane::Files,
            Pane::Files => Pane::Viewer,
            Pane::Viewer => Pane::Overview,
        };
    }

    fn previous_pane(&mut self) {
        self.active_pane = match self.active_pane {
            Pane::Overview => Pane::Viewer,
            Pane::Files => Pane::Overview,
            Pane::Viewer => Pane::Files,
        };
    }

    fn open_or_toggle_selected(&mut self) -> Result<()> {
        if self.active_pane != Pane::Files {
            return Ok(());
        }

        if let Some(path) = self.tree.toggle_selected()? {
            self.open_file(path)?;
        }
        Ok(())
    }

    fn open_file(&mut self, path: PathBuf) -> Result<()> {
        let review = self.store.load_file(&path)?.clone();
        self.current_file = Some(path.clone());
        self.current_review = Some(review);
        self.current_line = 0;
        self.scroll = 0;
        self.active_pane = Pane::Viewer;
        self.status = format!("Opened {}", display_path(&self.root, &path));
        self.refresh_stats()?;
        Ok(())
    }

    fn mark_current_line(&mut self, state: LineState) -> Result<()> {
        let Some(path) = self.current_file.clone() else {
            return Ok(());
        };
        let Some(review) = self.current_review.as_mut() else {
            return Ok(());
        };
        if review.lines.is_empty() {
            return Ok(());
        }

        self.store.set_line_state(&path, self.current_line, state)?;
        review.states[self.current_line] = state;
        self.status = match state {
            LineState::Unreviewed => "Marked current line unreviewed".to_string(),
            LineState::Accepted => "Accepted current line".to_string(),
            LineState::Rejected => "Rejected current line".to_string(),
        };
        self.refresh_stats()?;
        self.move_viewer_down();
        Ok(())
    }

    fn mark_selected_file(&mut self, state: LineState) -> Result<()> {
        let Some(path) = self.selected_file_path() else {
            self.status = "Select a file before accepting or rejecting it".to_string();
            return Ok(());
        };

        let line_count = self.store.set_file_state(&path, state)?;
        if self.current_file.as_deref() == Some(path.as_path()) {
            self.current_review = Some(self.store.load_file(&path)?.clone());
            self.current_line = if line_count == 0 {
                0
            } else {
                self.current_line.min(line_count - 1)
            };
            self.ensure_cursor_visible();
        }

        self.status = match state {
            LineState::Accepted => {
                format!(
                    "Accepted {line_count} lines in {}",
                    display_path(&self.root, &path)
                )
            }
            LineState::Rejected => {
                format!(
                    "Rejected {line_count} lines in {}",
                    display_path(&self.root, &path)
                )
            }
            LineState::Unreviewed => {
                format!(
                    "Marked {line_count} lines unreviewed in {}",
                    display_path(&self.root, &path)
                )
            }
        };
        self.refresh_stats()
    }

    fn selected_file_path(&self) -> Option<PathBuf> {
        let entry = self.tree.selected_entry()?;
        (entry.kind == TreeEntryKind::File).then(|| entry.path.clone())
    }

    fn ignore_selected_entry(&mut self) -> Result<()> {
        let Some(ignored) = self.tree.ignore_selected()? else {
            self.status = "Select a file or directory before ignoring it".to_string();
            return Ok(());
        };

        if let Some(current_file) = &self.current_file
            && path_covers(
                &ignored.path,
                current_file,
                ignored.kind == TreeEntryKind::Directory,
            )
        {
            self.current_file = None;
            self.current_review = None;
            self.current_line = 0;
            self.scroll = 0;
        }

        self.status = format!("Ignored {} in .debth/ignore", ignored.pattern);
        self.refresh_stats()
    }

    fn refresh_stats(&mut self) -> Result<()> {
        self.tree.refresh()?;
        self.stats = self.store.workspace_stats(self.tree.files())?;
        Ok(())
    }

    fn move_up(&mut self) -> Result<()> {
        match self.active_pane {
            Pane::Overview => {}
            Pane::Files => self.tree.move_up(),
            Pane::Viewer => self.move_viewer_up(),
        }
        Ok(())
    }

    fn move_down(&mut self) -> Result<()> {
        match self.active_pane {
            Pane::Overview => {}
            Pane::Files => self.tree.move_down(),
            Pane::Viewer => self.move_viewer_down(),
        }
        Ok(())
    }

    fn move_viewer_up(&mut self) {
        self.current_line = self.current_line.saturating_sub(1);
        self.ensure_cursor_visible();
    }

    fn move_viewer_down(&mut self) {
        let Some(review) = &self.current_review else {
            return;
        };
        if !review.lines.is_empty() {
            self.current_line = (self.current_line + 1).min(review.lines.len() - 1);
        }
        self.ensure_cursor_visible();
    }

    fn page_up(&mut self) {
        if self.active_pane == Pane::Viewer {
            self.current_line = self.current_line.saturating_sub(12);
            self.ensure_cursor_visible();
        }
    }

    fn page_down(&mut self) {
        if self.active_pane != Pane::Viewer {
            return;
        }

        let Some(review) = &self.current_review else {
            return;
        };
        if !review.lines.is_empty() {
            self.current_line = (self.current_line + 12).min(review.lines.len() - 1);
            self.ensure_cursor_visible();
        }
    }

    fn move_viewer_to_start(&mut self) {
        if self.active_pane == Pane::Viewer {
            self.current_line = 0;
            self.scroll = 0;
        }
    }

    fn move_viewer_to_end(&mut self) {
        if self.active_pane != Pane::Viewer {
            return;
        }
        if let Some(review) = &self.current_review
            && !review.lines.is_empty()
        {
            self.current_line = review.lines.len() - 1;
            self.scroll = self.current_line.saturating_sub(8) as u16;
        }
    }

    fn ensure_cursor_visible(&mut self) {
        if self.current_line < self.scroll as usize {
            self.scroll = self.current_line as u16;
        }

        let height = self.viewer_height.max(1);
        let bottom = self.scroll as usize + height;
        if self.current_line >= bottom {
            self.scroll = self.current_line.saturating_sub(height.saturating_sub(1)) as u16;
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let root = frame.area();
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(root);
        let sidebar_width = sidebar_width(root.width);
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar_width), Constraint::Min(1)])
            .split(vertical[0]);
        let sidebar = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(9), Constraint::Min(8)])
            .split(body[0]);

        self.render_overview(frame, sidebar[0]);
        self.render_tree(frame, sidebar[1]);
        if self.active_pane == Pane::Overview {
            self.render_repo_info(frame, body[1]);
        } else {
            self.render_viewer(frame, body[1]);
        }
        self.render_footer(frame, vertical[1]);

        if self.prompt == GitignorePrompt::Waiting {
            self.render_gitignore_prompt(frame, root);
        }
    }

    fn render_overview(&self, frame: &mut Frame, area: Rect) {
        let reviewed = self.stats.reviewed();
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    format!("{}%", self.stats.reviewed_percent()),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" reviewed"),
            ]),
            Line::from(format!("Understood: {}", self.stats.accepted)),
            Line::from(Span::styled(
                format!("Rejected: {}", self.stats.rejected),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(format!("Unreviewed: {}", self.stats.unreviewed)),
            Line::from(format!("Total: {reviewed}/{}", self.stats.total)),
        ];
        let block = Block::default()
            .title("[1] Review")
            .borders(Borders::ALL)
            .border_style(pane_border(self.active_pane == Pane::Overview));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_tree(&self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .tree
            .visible()
            .iter()
            .enumerate()
            .map(|entry| {
                let (index, entry) = entry;
                let is_selected = index == self.tree.selected();
                let is_open = self.current_file.as_deref() == Some(entry.path.as_path());
                let indent = "  ".repeat(entry.depth);
                let selected_marker = if is_selected {
                    "> "
                } else if is_open {
                    "* "
                } else {
                    "  "
                };
                let marker = match entry.kind {
                    TreeEntryKind::Directory if entry.expanded => "v ",
                    TreeEntryKind::Directory => "> ",
                    TreeEntryKind::File => "  ",
                };
                let style = Style::default();
                let row_style = if is_selected && self.active_pane == Pane::Files {
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else if is_open {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(selected_marker, row_style),
                    Span::raw(indent),
                    Span::styled(marker, style),
                    Span::styled(entry.name.clone(), style),
                ]))
                .style(row_style)
            })
            .collect();

        let block = Block::default()
            .title("[2] Files")
            .borders(Borders::ALL)
            .border_style(pane_border(self.active_pane == Pane::Files));
        frame.render_widget(List::new(items).block(block), area);
    }

    fn render_viewer(&mut self, frame: &mut Frame, area: Rect) {
        let title = self
            .current_file
            .as_ref()
            .map(|path| display_path(&self.root, path))
            .unwrap_or_else(|| "No file selected".to_string());
        let title = format!("[3] {title}");
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(pane_border(self.active_pane == Pane::Viewer));

        let inner = block.inner(area);
        self.viewer_height = inner.height as usize;
        frame.render_widget(block, area);

        let Some(review) = &self.current_review else {
            let empty = Paragraph::new("Select a file from the explorer and press Enter.")
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true });
            frame.render_widget(empty, inner);
            return;
        };

        if review.lines.is_empty() {
            frame.render_widget(
                Paragraph::new("Empty file").alignment(Alignment::Center),
                inner,
            );
            return;
        }

        let height = inner.height as usize;
        let start = self.scroll as usize;
        let end = (start + height).min(review.lines.len());
        let width = inner.width as usize;
        let number_width = review.lines.len().to_string().len().max(3);

        let lines: Vec<Line> = review.lines[start..end]
            .iter()
            .enumerate()
            .map(|(offset, text)| {
                let line_index = start + offset;
                let is_cursor = line_index == self.current_line;
                let state = review
                    .states
                    .get(line_index)
                    .copied()
                    .unwrap_or(LineState::Unreviewed);
                let state_style = if is_cursor {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Blue)
                        .add_modifier(Modifier::BOLD)
                } else {
                    line_style(state)
                };
                let line_number_style = if is_cursor {
                    state_style
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let line_number = format!("{:>number_width$} ", line_index + 1);
                let cursor_marker = if is_cursor { "> " } else { "  " };
                let available = width.saturating_sub(number_width + 4);
                let mut visible_text = text.replace('\t', "    ");
                if visible_text.chars().count() > available {
                    visible_text = visible_text
                        .chars()
                        .take(available.saturating_sub(1))
                        .collect();
                    visible_text.push_str("...");
                }

                Line::from(vec![
                    Span::styled(cursor_marker, line_number_style),
                    Span::styled(line_number, line_number_style),
                    Span::styled(
                        review_marker(state),
                        state_style.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(visible_text, state_style),
                ])
            })
            .collect();

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_repo_info(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .title("Repository")
            .borders(Borders::ALL)
            .border_style(Style::default());
        let repo_link = self
            .repo_link
            .as_deref()
            .unwrap_or("No git remote configured");
        let lines = vec![
            Line::from("Git repository"),
            Line::from(""),
            Line::from(vec![
                Span::styled("Remote: ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(
                    repo_link.to_string(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]),
            Line::from(vec![
                Span::styled("Path:   ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(self.root.to_string_lossy().to_string()),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let footer = vec![
            Line::from(vec![
                Span::styled(
                    format!("[{}] ", pane_name(self.active_pane)),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.status.as_str(), Style::default().fg(Color::Gray)),
            ]),
            shortcut_line(self.active_pane),
        ];
        frame.render_widget(Paragraph::new(footer), area);
    }

    fn render_gitignore_prompt(&self, frame: &mut Frame, root: Rect) {
        let area = centered_rect(60, 7, root);
        frame.render_widget(Clear, area);
        let text = vec![
            Line::from("This git repo has no .debth/ entry in .gitignore."),
            Line::from("Add it now?"),
            Line::from(""),
            Line::from(vec![
                Span::styled("y", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" yes    "),
                Span::styled("n", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" no"),
            ]),
        ];
        let block = Block::default()
            .title("First start")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));
        frame.render_widget(
            Paragraph::new(text)
                .block(block)
                .alignment(Alignment::Center),
            area,
        );
    }
}

fn sidebar_width(width: u16) -> u16 {
    if width <= 2 {
        return width;
    }

    let desired = if width < 70 {
        (width / 2).clamp(18, 34)
    } else {
        34
    };
    desired.min(width.saturating_sub(1))
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(height.min(area.height)),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(width.min(area.width)),
            Constraint::Min(0),
        ])
        .split(popup_layout[1])[1]
}

fn pane_border(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn pane_name(pane: Pane) -> &'static str {
    match pane {
        Pane::Overview => "1 Review",
        Pane::Files => "2 Files",
        Pane::Viewer => "3 Viewer",
    }
}

fn shortcut_line(pane: Pane) -> Line<'static> {
    let parts = match pane {
        Pane::Overview => vec![("tab", "next pane"), ("h/l", "switch pane")],
        Pane::Files => vec![
            ("j/k", "move"),
            ("enter", "open"),
            ("space", "toggle"),
            ("a", "accept file"),
            ("r", "reject file"),
            ("i", "ignore"),
            ("<-/->", "tree"),
        ],
        Pane::Viewer => vec![
            ("j/k", "line"),
            ("a", "accept"),
            ("r", "reject"),
            ("u", "unreview"),
            ("pgup/pgdn", "scroll"),
        ],
    };

    let mut spans = Vec::new();
    for (index, (shortcut, description)) in parts.into_iter().enumerate() {
        if index > 0 {
            spans.push(shortcut_text(" | "));
        }
        spans.push(key(shortcut));
        spans.push(shortcut_text(" "));
        spans.push(shortcut_text(description));
    }

    Line::from(spans).style(Style::default().fg(Color::Blue))
}

fn key(label: &'static str) -> Span<'static> {
    Span::styled(
        label,
        Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
    )
}

fn shortcut_text(text: &'static str) -> Span<'static> {
    Span::styled(text, Style::default().fg(Color::Blue))
}

fn git_repo_link(root: &Path) -> Option<String> {
    let config = std::fs::read_to_string(root.join(".git").join("config")).ok()?;
    let mut in_origin = false;

    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_origin = trimmed == r#"[remote "origin"]"#;
            continue;
        }

        if in_origin
            && let Some((key, value)) = trimmed.split_once('=')
            && key.trim() == "url"
        {
            return Some(normalize_git_url(value.trim()));
        }
    }

    None
}

fn normalize_git_url(url: &str) -> String {
    let without_git_suffix = url.strip_suffix(".git").unwrap_or(url);

    if let Some(rest) = without_git_suffix.strip_prefix("git@")
        && let Some((host, path)) = rest.split_once(':')
    {
        return format!("https://{host}/{path}");
    }

    if let Some(rest) = without_git_suffix.strip_prefix("ssh://git@")
        && let Some((host, path)) = rest.split_once('/')
    {
        return format!("https://{host}/{path}");
    }

    without_git_suffix.to_string()
}

fn line_style(state: LineState) -> Style {
    match state {
        LineState::Unreviewed => Style::default().fg(Color::Gray),
        LineState::Accepted => Style::default().fg(Color::Green),
        LineState::Rejected => Style::default().fg(Color::Yellow),
    }
}

fn review_marker(state: LineState) -> &'static str {
    match state {
        LineState::Unreviewed => "? ",
        LineState::Accepted => "+ ",
        LineState::Rejected => "! ",
    }
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn path_covers(base: &Path, path: &Path, base_is_dir: bool) -> bool {
    if base_is_dir {
        path.starts_with(base)
    } else {
        path == base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_git_remote_urls() {
        assert_eq!(
            normalize_git_url("git@github.com:alieron/debth.git"),
            "https://github.com/alieron/debth"
        );
        assert_eq!(
            normalize_git_url("ssh://git@github.com/alieron/debth.git"),
            "https://github.com/alieron/debth"
        );
        assert_eq!(
            normalize_git_url("https://github.com/alieron/debth.git"),
            "https://github.com/alieron/debth"
        );
    }
}
