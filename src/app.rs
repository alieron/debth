use std::{
    collections::HashMap,
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
    path_stats: HashMap<PathBuf, ReviewStats>,
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
        let (stats, path_stats) = collect_review_stats(&root, &tree, &mut store)?;

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
            path_stats,
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
                self.mark_selected_entry(LineState::Accepted)?;
            }
            KeyCode::Char('r') if self.active_pane == Pane::Files => {
                self.mark_selected_entry(LineState::Rejected)?;
            }
            KeyCode::Char('u') if self.active_pane == Pane::Files => {
                self.mark_selected_entry(LineState::Unreviewed)?;
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

    fn mark_selected_entry(&mut self, state: LineState) -> Result<()> {
        let Some(entry) = self.tree.selected_entry().cloned() else {
            self.status = "Select a file or directory before marking it".to_string();
            return Ok(());
        };

        let paths = match entry.kind {
            TreeEntryKind::File => vec![entry.path.clone()],
            TreeEntryKind::Directory => self
                .tree
                .files()
                .into_iter()
                .filter(|path| path.starts_with(&entry.path))
                .collect(),
        };

        if paths.is_empty() {
            self.status = format!("No files in {}", display_path(&self.root, &entry.path));
            return Ok(());
        }

        let mut line_count = 0;
        for path in &paths {
            line_count += self.store.set_file_state(path, state)?;
        }

        if let Some(current_file) = self.current_file.clone()
            && paths.iter().any(|path| path == &current_file)
        {
            let review = self.store.load_file(&current_file)?.clone();
            let current_line_count = review.lines.len();
            self.current_review = Some(review);
            self.current_line = if current_line_count == 0 {
                0
            } else {
                self.current_line.min(current_line_count - 1)
            };
            self.ensure_cursor_visible();
        }

        let file_count = paths.len();
        let target = bulk_action_target(&self.root, &entry.path, entry.kind, file_count);
        self.status = match state {
            LineState::Accepted => format!("Accepted {line_count} lines in {target}"),
            LineState::Rejected => format!("Rejected {line_count} lines in {target}"),
            LineState::Unreviewed => format!("Marked {line_count} lines unreviewed in {target}"),
        };
        self.refresh_stats()
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
        let (stats, path_stats) = collect_review_stats(&self.root, &self.tree, &mut self.store)?;
        self.stats = stats;
        self.path_stats = path_stats;
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
        let block = Block::default()
            .title("[2] Files")
            .borders(Borders::ALL)
            .border_style(pane_border(self.active_pane == Pane::Files));
        let inner_width = block.inner(area).width as usize;
        let status_columns = tree_status_columns(
            self.tree
                .visible()
                .iter()
                .filter(|entry| tree_entry_shows_indicators(&entry.kind, entry.expanded))
                .filter_map(|entry| self.path_stats.get(&entry.path).copied()),
        );
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
                let stats = self
                    .path_stats
                    .get(&entry.path)
                    .copied()
                    .unwrap_or_default();
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
                } else {
                    Style::default()
                };
                let entry_style = tree_entry_style(stats, row_style, is_selected);
                let indicator_stats = if tree_entry_shows_indicators(&entry.kind, entry.expanded) {
                    stats
                } else {
                    ReviewStats::default()
                };
                ListItem::new(Line::from(tree_row_spans(TreeRow {
                    selected_marker,
                    indent,
                    marker,
                    name: &entry.name,
                    indicator_stats,
                    row_style,
                    entry_style,
                    is_selected,
                    inner_width,
                    status_columns,
                })))
                .style(row_style)
            })
            .collect();

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

fn collect_review_stats(
    root: &Path,
    tree: &FileTree,
    store: &mut ReviewStore,
) -> Result<(ReviewStats, HashMap<PathBuf, ReviewStats>)> {
    let mut workspace_stats = ReviewStats::default();
    let mut path_stats = HashMap::new();

    for file in tree.files() {
        let stats = store.file_stats(&file)?;
        add_review_stats(&mut workspace_stats, stats);
        path_stats.insert(file.clone(), stats);
        add_parent_stats(root, &file, stats, &mut path_stats);
    }

    Ok((workspace_stats, path_stats))
}

fn add_parent_stats(
    root: &Path,
    file: &Path,
    stats: ReviewStats,
    path_stats: &mut HashMap<PathBuf, ReviewStats>,
) {
    let mut parent = file.parent();
    while let Some(directory) = parent {
        if !directory.starts_with(root) {
            break;
        }

        let directory_stats = path_stats.entry(directory.to_path_buf()).or_default();
        add_review_stats(directory_stats, stats);

        if directory == root {
            break;
        }
        parent = directory.parent();
    }
}

fn add_review_stats(total: &mut ReviewStats, next: ReviewStats) {
    total.total += next.total;
    total.accepted += next.accepted;
    total.rejected += next.rejected;
    total.unreviewed += next.unreviewed;
}

struct TreeRow<'a> {
    selected_marker: &'static str,
    indent: String,
    marker: &'static str,
    name: &'a str,
    indicator_stats: ReviewStats,
    row_style: Style,
    entry_style: Style,
    is_selected: bool,
    inner_width: usize,
    status_columns: TreeStatusColumns,
}

fn tree_row_spans(row: TreeRow<'_>) -> Vec<Span<'static>> {
    let mut indicator_spans = review_indicator_spans(
        row.indicator_stats,
        row.row_style,
        row.is_selected,
        row.status_columns,
    );
    let has_indicators = row.indicator_stats.rejected > 0 || row.indicator_stats.unreviewed > 0;
    let visible_name = if has_indicators {
        let prefix_width =
            text_width(row.selected_marker) + text_width(&row.indent) + text_width(row.marker);
        let status_column_start = row
            .inner_width
            .saturating_sub(row.status_columns.total_width());
        let name_width = status_column_start.saturating_sub(prefix_width + 1);
        truncate_to_width(row.name, name_width)
    } else {
        row.name.to_string()
    };

    let mut spans = vec![
        Span::styled(row.selected_marker, row.entry_style),
        Span::styled(row.indent, row.row_style),
        Span::styled(row.marker, row.entry_style),
        Span::styled(visible_name, row.entry_style),
    ];

    if has_indicators {
        let status_column_start = row
            .inner_width
            .saturating_sub(row.status_columns.total_width());
        let padding = status_column_start.saturating_sub(spans_width(&spans));
        if padding > 0 {
            spans.push(Span::styled(" ".repeat(padding), row.row_style));
        }
        spans.append(&mut indicator_spans);
    }

    spans
}

fn tree_entry_shows_indicators(kind: &TreeEntryKind, expanded: bool) -> bool {
    !(kind == &TreeEntryKind::Directory && expanded)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TreeStatusColumns {
    rejected: usize,
    unreviewed: usize,
}

impl TreeStatusColumns {
    fn total_width(self) -> usize {
        self.rejected + self.separator_width() + self.unreviewed
    }

    fn separator_width(self) -> usize {
        usize::from(self.rejected > 0 && self.unreviewed > 0)
    }
}

fn tree_status_columns<I>(stats: I) -> TreeStatusColumns
where
    I: IntoIterator<Item = ReviewStats>,
{
    let mut columns = TreeStatusColumns::default();
    for stats in stats {
        if let Some(indicator) = rejected_indicator(stats) {
            columns.rejected = columns.rejected.max(text_width(&indicator));
        }
        if let Some(indicator) = unreviewed_indicator(stats) {
            columns.unreviewed = columns.unreviewed.max(text_width(&indicator));
        }
    }
    columns
}

fn review_indicator_spans(
    stats: ReviewStats,
    row_style: Style,
    is_selected: bool,
    columns: TreeStatusColumns,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();

    if columns.rejected > 0 {
        push_indicator_column(
            &mut spans,
            rejected_indicator(stats),
            columns.rejected,
            tree_indicator_style(Color::Red, row_style, is_selected),
            row_style,
        );
    }

    if columns.rejected > 0 && columns.unreviewed > 0 {
        spans.push(Span::styled(" ", row_style));
    }

    if columns.unreviewed > 0 {
        push_indicator_column(
            &mut spans,
            unreviewed_indicator(stats),
            columns.unreviewed,
            tree_indicator_style(Color::Gray, row_style, is_selected),
            row_style,
        );
    }

    spans
}

fn push_indicator_column(
    spans: &mut Vec<Span<'static>>,
    indicator: Option<String>,
    column_width: usize,
    indicator_style: Style,
    row_style: Style,
) {
    if let Some(indicator) = indicator {
        let indicator_width = text_width(&indicator);
        spans.push(Span::styled(indicator, indicator_style));
        let padding = column_width.saturating_sub(indicator_width);
        if padding > 0 {
            spans.push(Span::styled(" ".repeat(padding), row_style));
        }
    } else if column_width > 0 {
        spans.push(Span::styled(" ".repeat(column_width), row_style));
    }
}

fn rejected_indicator(stats: ReviewStats) -> Option<String> {
    (stats.rejected > 0).then(|| format!("!{}", stats.rejected))
}

fn unreviewed_indicator(stats: ReviewStats) -> Option<String> {
    (stats.unreviewed > 0).then(|| format!("?{}", stats.unreviewed))
}

fn spans_width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

fn text_width(text: &str) -> usize {
    Span::raw(text).width()
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    if text_width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width <= 3 {
        return ".".repeat(max_width);
    }

    let mut truncated = String::new();
    for character in text.chars() {
        let mut next = truncated.clone();
        next.push(character);
        if text_width(&next) > max_width - 3 {
            break;
        }
        truncated.push(character);
    }
    truncated.push_str("...");
    truncated
}

fn tree_entry_style(stats: ReviewStats, row_style: Style, is_selected: bool) -> Style {
    let mut style = if is_selected {
        row_style
    } else {
        Style::default()
    };

    if let Some(color) = tree_status_color(stats) {
        style = style.fg(color).add_modifier(Modifier::BOLD);
    }

    style
}

fn tree_status_color(stats: ReviewStats) -> Option<Color> {
    if stats.rejected > 0 {
        Some(Color::Red)
    } else if stats.total > 0 && stats.accepted == stats.total {
        Some(Color::Green)
    } else {
        None
    }
}

fn bulk_action_target(root: &Path, path: &Path, kind: TreeEntryKind, file_count: usize) -> String {
    let display = display_path(root, path);
    match kind {
        TreeEntryKind::File => display,
        TreeEntryKind::Directory => format!("{display} ({file_count} {})", file_word(file_count)),
    }
}

fn file_word(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

fn tree_indicator_style(color: Color, row_style: Style, is_selected: bool) -> Style {
    if is_selected {
        row_style.fg(color)
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    }
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
            ("a", "accept"),
            ("r", "reject"),
            ("u", "unreview"),
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

    #[test]
    fn review_stats_are_collected_for_files_and_parent_directories() -> Result<()> {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("src"))?;
        let root = root.canonicalize()?;
        let file = root.join("src").join("main.rs");
        std::fs::write(&file, "accepted\nrejected\npending\n")?;

        let tree = FileTree::new(root.clone())?;
        let mut store = ReviewStore::open(root.clone())?;
        store.set_line_state(&file, 0, LineState::Accepted)?;
        store.set_line_state(&file, 1, LineState::Rejected)?;

        let (workspace_stats, path_stats) = collect_review_stats(&root, &tree, &mut store)?;

        let expected = ReviewStats {
            total: 3,
            accepted: 1,
            rejected: 1,
            unreviewed: 1,
        };
        assert_eq!(workspace_stats, expected);
        assert_eq!(path_stats.get(&file).copied(), Some(expected));
        assert_eq!(path_stats.get(&root.join("src")).copied(), Some(expected));

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn marking_directory_marks_all_descendant_files() -> Result<()> {
        let root = unique_temp_dir();
        std::fs::create_dir_all(root.join("src").join("nested"))?;
        std::fs::write(root.join("src").join("lib.rs"), "one\ntwo\n")?;
        std::fs::write(root.join("src").join("nested").join("mod.rs"), "three\n")?;
        std::fs::write(root.join("outside.rs"), "outside\n")?;
        let root = root.canonicalize()?;
        let src = root.join("src");
        let lib = src.join("lib.rs");
        let nested = src.join("nested").join("mod.rs");
        let outside = root.join("outside.rs");

        let mut app = App::new(root.clone())?;
        let src_index = app
            .tree
            .visible()
            .iter()
            .position(|entry| entry.path == src)
            .expect("src directory is visible");
        while app.tree.selected() < src_index {
            app.tree.move_down();
        }

        app.mark_selected_entry(LineState::Rejected)?;

        assert_file_state(&mut app.store, &lib, LineState::Rejected)?;
        assert_file_state(&mut app.store, &nested, LineState::Rejected)?;
        assert_file_state(&mut app.store, &outside, LineState::Unreviewed)?;
        let src_stats = app.path_stats.get(&src).copied().expect("src stats");
        assert_eq!(src_stats.rejected, 3);
        assert_eq!(src_stats.unreviewed, 0);

        app.mark_selected_entry(LineState::Unreviewed)?;

        assert_file_state(&mut app.store, &lib, LineState::Unreviewed)?;
        assert_file_state(&mut app.store, &nested, LineState::Unreviewed)?;

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn tree_status_color_prioritizes_rejected_then_fully_accepted() {
        assert_eq!(
            tree_status_color(ReviewStats {
                total: 2,
                accepted: 2,
                rejected: 0,
                unreviewed: 0,
            }),
            Some(Color::Green)
        );
        assert_eq!(
            tree_status_color(ReviewStats {
                total: 2,
                accepted: 1,
                rejected: 1,
                unreviewed: 0,
            }),
            Some(Color::Red)
        );
        assert_eq!(
            tree_status_color(ReviewStats {
                total: 2,
                accepted: 1,
                rejected: 0,
                unreviewed: 1,
            }),
            None
        );
        assert_eq!(tree_status_color(ReviewStats::default()), None);
    }

    #[test]
    fn tree_row_spans_aligns_rejected_and_unreviewed_indicator_columns() {
        let both_stats = ReviewStats {
            total: 24,
            accepted: 15,
            rejected: 2,
            unreviewed: 7,
        };
        let only_rejected_stats = ReviewStats {
            total: 12,
            accepted: 0,
            rejected: 12,
            unreviewed: 0,
        };
        let only_unreviewed_stats = ReviewStats {
            total: 70,
            accepted: 0,
            rejected: 0,
            unreviewed: 70,
        };
        let status_columns =
            tree_status_columns([both_stats, only_rejected_stats, only_unreviewed_stats]);

        let spans = tree_row_spans(TreeRow {
            selected_marker: "  ",
            indent: "  ".to_string(),
            marker: "  ",
            name: "a_very_long_file_name.rs",
            indicator_stats: both_stats,
            row_style: Style::default(),
            entry_style: Style::default(),
            is_selected: false,
            inner_width: 32,
            status_columns,
        });
        let text = span_text(&spans);

        assert_eq!(text_width(&text), 32);
        let rejected_start = marker_start(&text, "!2");
        let unreviewed_start = marker_start(&text, "?7");

        let rejected_spans = tree_row_spans(TreeRow {
            selected_marker: "  ",
            indent: "  ".to_string(),
            marker: "  ",
            name: "rejected.rs",
            indicator_stats: only_rejected_stats,
            row_style: Style::default(),
            entry_style: Style::default(),
            is_selected: false,
            inner_width: 32,
            status_columns,
        });
        let rejected_text = span_text(&rejected_spans);

        let unreviewed_spans = tree_row_spans(TreeRow {
            selected_marker: "  ",
            indent: "  ".to_string(),
            marker: "  ",
            name: "unreviewed.rs",
            indicator_stats: only_unreviewed_stats,
            row_style: Style::default(),
            entry_style: Style::default(),
            is_selected: false,
            inner_width: 32,
            status_columns,
        });
        let unreviewed_text = span_text(&unreviewed_spans);

        assert_eq!(marker_start(&rejected_text, "!12"), rejected_start);
        assert_eq!(marker_start(&unreviewed_text, "?70"), unreviewed_start);
        assert!(unreviewed_start > rejected_start);
    }

    #[test]
    fn expanded_directory_rows_hide_review_indicators() {
        let aggregate_stats = ReviewStats {
            total: 9,
            accepted: 0,
            rejected: 2,
            unreviewed: 7,
        };
        let status_columns = tree_status_columns([aggregate_stats]);
        let indicator_stats = if tree_entry_shows_indicators(&TreeEntryKind::Directory, true) {
            aggregate_stats
        } else {
            ReviewStats::default()
        };

        let spans = tree_row_spans(TreeRow {
            selected_marker: "  ",
            indent: String::new(),
            marker: "v ",
            name: "src",
            indicator_stats,
            row_style: Style::default(),
            entry_style: tree_entry_style(aggregate_stats, Style::default(), false),
            is_selected: false,
            inner_width: 24,
            status_columns,
        });
        let text = span_text(&spans);

        assert!(!text.contains('!'));
        assert!(!text.contains('?'));
        assert!(!tree_entry_shows_indicators(
            &TreeEntryKind::Directory,
            true
        ));
        assert!(tree_entry_shows_indicators(
            &TreeEntryKind::Directory,
            false
        ));
        assert!(tree_entry_shows_indicators(&TreeEntryKind::File, false));
    }

    fn assert_file_state(store: &mut ReviewStore, path: &Path, expected: LineState) -> Result<()> {
        let review = store.load_file(path)?;
        assert!(review.states.iter().all(|state| *state == expected));
        Ok(())
    }

    fn span_text(spans: &[Span<'static>]) -> String {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn marker_start(text: &str, marker: &str) -> usize {
        let index = text.find(marker).expect("marker exists");
        text_width(&text[..index])
    }

    fn unique_temp_dir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("debth-app-test-{}-{nanos}", std::process::id()))
    }
}
