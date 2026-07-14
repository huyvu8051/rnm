use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap},
    Terminal,
};
use tokio::sync::mpsc;

use crate::env::EnvManager;
use crate::history::{HistoryEntry, HistoryManager};
use crate::request::RequestFile;
use crate::telescope::{Telescope, TelescopeAction, TelescopeItem};

pub struct TuiApp {
    env_manager: EnvManager,
    buffers: Vec<PathBuf>,
    request_state: ListState,
    envs: Vec<String>,
    env_scroll: u16,
    selected_panel: ActivePanel,
    // Left-column panel (Requests or Environments) to return to on Ctrl+h from Response.
    last_left_panel: ActivePanel,
    response_view: String,
    status_view: String,
    // All requests currently in flight (drives per-buffer spinners).
    running_requests: std::collections::HashSet<PathBuf>,
    // Which in-flight request's result should populate the Response panel when it finishes.
    focused_request: Option<PathBuf>,
    response_scroll: u16,
    show_help: bool,
    spinner_frame: usize,
    preview_scroll: u16,
    // Space is the leader key; true right after Space until the next keypress resolves it.
    leader_pending: bool,

    // Generic fuzzy-picker popup, shared by every "open file" / "switch env" /
    // "switch buffer" feature — see src/telescope.rs.
    telescope: Telescope,

    // Request/response history
    history_manager: HistoryManager,
    history: Vec<HistoryEntry>,
    history_active: bool,
    history_state: ListState,

    // Inline grid editor for the selected request's Query params / Headers.
    grid_edit: Option<GridEditState>,
}

#[derive(PartialEq, Clone, Copy)]
enum ActivePanel {
    Preview,
    Environments,
    Response,
}

/// Preview content split so query params / headers render as grids rather than flat text.
struct PreviewData {
    top: String,
    query_rows: Vec<(String, String)>,
    header_rows: Vec<(String, String)>,
    body_text: String,
    raw_file: String,
}

#[derive(PartialEq, Clone, Copy)]
enum GridSection {
    Query,
    Headers,
    Body,
}

#[derive(PartialEq, Clone, Copy)]
enum GridCol {
    Key,
    Value,
}

/// Inline editor state for a request file's Query params / Headers grids.
/// `original` keeps the rest of the file (method/url/body/exports/name) untouched
/// across a save, and is parsed from the RAW file so `{{var}}` placeholders survive.
/// A point-in-time copy of both grids, for undo/redo.
struct GridSnapshot {
    query_rows: Vec<(String, String)>,
    header_rows: Vec<(String, String)>,
    body_text: String,
}

/// One state in the undo tree. Unlike a linear undo/redo stack, undoing and then
/// making a new edit does NOT discard the old branch — it stays reachable forever
/// via the Undo Tree browser (`u`), same idea as vim's undotree plugin.
struct UndoNode {
    parent: Option<usize>,
    children: Vec<usize>,
    depth: usize,
    snapshot: GridSnapshot,
    label: String,
    timestamp: String,
    created_at: u64,
}

struct GridEditState {
    path: PathBuf,
    original: RequestFile,
    query_rows: Vec<(String, String)>,
    header_rows: Vec<(String, String)>,
    /// Raw body text as edited. Re-parsed as JSON on save, falling back to a
    /// plain YAML string scalar if it isn't valid JSON (mirrors how the
    /// request runner already accepts either form for `body:`).
    body_text: String,
    section: GridSection,
    row: usize,
    col: GridCol,
    editing_text: Option<String>,
    undo_nodes: Vec<UndoNode>,
    undo_current: usize,
    undo_tree_active: bool,
    undo_tree_selected: usize,
}

impl GridEditState {
    /// Only valid for the Query/Headers sections — callers must guard on
    /// `section != GridSection::Body` before reaching here (Body has no rows).
    fn rows(&self) -> &Vec<(String, String)> {
        match self.section {
            GridSection::Query => &self.query_rows,
            GridSection::Headers => &self.header_rows,
            GridSection::Body => unreachable!("rows() called for Body section"),
        }
    }

    fn rows_mut(&mut self) -> &mut Vec<(String, String)> {
        match self.section {
            GridSection::Query => &mut self.query_rows,
            GridSection::Headers => &mut self.header_rows,
            GridSection::Body => unreachable!("rows_mut() called for Body section"),
        }
    }

    fn focused_value(&self) -> String {
        self.rows().get(self.row)
            .map(|(k, v)| match self.col { GridCol::Key => k.clone(), GridCol::Value => v.clone() })
            .unwrap_or_default()
    }

    fn set_focused_value(&mut self, val: String) {
        let (row, col) = (self.row, self.col);
        if let Some(r) = self.rows_mut().get_mut(row) {
            match col {
                GridCol::Key => r.0 = val,
                GridCol::Value => r.1 = val,
            }
        }
    }

    fn snapshot(&self) -> GridSnapshot {
        GridSnapshot {
            query_rows: self.query_rows.clone(),
            header_rows: self.header_rows.clone(),
            body_text: self.body_text.clone(),
        }
    }

    /// Records the CURRENT (post-mutation) state as a new child of the current
    /// undo node and moves onto it. Never overwrites or discards existing nodes,
    /// so undoing and then editing again still leaves the old branch reachable.
    fn commit(&mut self, label: String) {
        let depth = self.undo_nodes[self.undo_current].depth + 1;
        let new_id = self.undo_nodes.len();
        self.undo_nodes.push(UndoNode {
            parent: Some(self.undo_current),
            children: Vec::new(),
            depth,
            snapshot: self.snapshot(),
            label,
            timestamp: crate::history::current_timestamp(),
            created_at: crate::history::now_unix(),
        });
        self.undo_nodes[self.undo_current].children.push(new_id);
        self.undo_current = new_id;
    }

    fn restore_from(&mut self, node_id: usize) {
        let snap = &self.undo_nodes[node_id].snapshot;
        self.query_rows = snap.query_rows.clone();
        self.header_rows = snap.header_rows.clone();
        self.body_text = snap.body_text.clone();
        self.undo_current = node_id;
        self.clamp_cursor();
    }

    /// Returns true if something actually changed (so the caller knows to autosave).
    fn undo(&mut self) -> bool {
        if let Some(parent) = self.undo_nodes[self.undo_current].parent {
            self.restore_from(parent);
            true
        } else {
            false
        }
    }

    /// Redo follows the most-recently-created branch by default, matching vim.
    /// Use the Undo Tree browser (`u`) to jump to an older branch instead.
    fn redo(&mut self) -> bool {
        if let Some(&child) = self.undo_nodes[self.undo_current].children.last() {
            self.restore_from(child);
            true
        } else {
            false
        }
    }

    fn jump_to(&mut self, node_id: usize) -> bool {
        if node_id < self.undo_nodes.len() && node_id != self.undo_current {
            self.restore_from(node_id);
            true
        } else {
            false
        }
    }

    fn clamp_cursor(&mut self) {
        if self.section == GridSection::Body {
            return;
        }
        let len = self.rows().len();
        self.row = if len == 0 { 0 } else { self.row.min(len - 1) };
    }
}

enum AppEvent {
    TerminalEvent(Event),
    ResponseFinished {
        status: String,
        response: String,
        method: String,
        url: String,
        path: PathBuf,
    },
    Tick,
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl TuiApp {
    pub fn new(env_manager: EnvManager) -> Result<Self> {
        let mut app = Self {
            history_manager: HistoryManager::new(env_manager.config_dir()),
            env_manager,
            buffers: Vec::new(),
            request_state: ListState::default(),
            envs: Vec::new(),
            env_scroll: 0,
            selected_panel: ActivePanel::Preview,
            last_left_panel: ActivePanel::Preview,
            response_view: String::new(),
            status_view: String::new(),
            running_requests: std::collections::HashSet::new(),
            focused_request: None,
            response_scroll: 0,
            show_help: false,
            spinner_frame: 0,
            preview_scroll: 0,
            leader_pending: false,
            telescope: Telescope::default(),
            history: Vec::new(),
            history_active: false,
            history_state: ListState::default(),
            grid_edit: None,
        };

        // Open some initial buffer if any YAML requests are in current folder (not recursively)
        app.load_initial_buffers()?;
        app.refresh_envs()?;
        Ok(app)
    }

    fn load_initial_buffers(&mut self) -> Result<()> {
        let dir = Path::new(".");
        if dir.is_dir() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                        if ext == "yaml" || ext == "yml" {
                            if let Ok(content) = fs::read_to_string(&path) {
                                if content.contains("method:") && content.contains("url:") {
                                    self.buffers.push(path);
                                }
                            }
                        }
                    }
                }
            }
        }
        if !self.buffers.is_empty() {
            self.request_state.select(Some(0));
        }
        Ok(())
    }

    fn scan_dir(&self, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
        if dir.is_dir() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                        if name.starts_with('.') {
                            continue;
                        }
                    }
                    self.scan_dir(&path, files)?;
                } else if path.is_file() {
                    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                        if ext == "yaml" || ext == "yml" {
                            if let Ok(content) = fs::read_to_string(&path) {
                                if content.contains("method:") && content.contains("url:") {
                                    files.push(path);
                                }
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// `t` (Preview/Response panel): search the whole project for request files to open.
    fn open_file_telescope(&mut self) -> Result<()> {
        let mut files = Vec::new();
        self.scan_dir(Path::new("."), &mut files)?;
        let items = files.into_iter()
            .map(|p| TelescopeItem::new(p.to_string_lossy().to_string(), TelescopeAction::OpenFile(p)))
            .collect();
        self.telescope.open(" Telescope (Fuzzy Search *.yaml) ", items);
        Ok(())
    }

    /// `t` (Environments panel): pick which environment is active.
    fn open_env_telescope(&mut self) -> Result<()> {
        self.refresh_envs()?;
        let items = self.envs.iter()
            .map(|name| TelescopeItem::new(name.clone(), TelescopeAction::SwitchEnv(name.clone())))
            .collect();
        self.telescope.open(" Telescope (Choose Environment) ", items);
        Ok(())
    }

    /// `<leader>b`: fuzzy-pick among already-open buffers (vs `t`, which searches
    /// the whole project for files to open as a new buffer).
    fn open_buffer_telescope(&mut self) {
        let items = self.buffers.iter()
            .map(|p| TelescopeItem::new(p.to_string_lossy().to_string(), TelescopeAction::SwitchBuffer(p.clone())))
            .collect();
        self.telescope.open(" Telescope (Switch Buffer) ", items);
    }

    /// Applies whatever was picked in the Telescope popup.
    fn apply_telescope_action(&mut self, action: TelescopeAction) {
        match action {
            TelescopeAction::OpenFile(path) => {
                if !self.buffers.contains(&path) {
                    self.buffers.push(path.clone());
                }
                if let Some(index) = self.buffers.iter().position(|p| p == &path) {
                    self.request_state.select(Some(index));
                    self.preview_scroll = 0;
                }
            }
            TelescopeAction::SwitchEnv(name) => {
                let _ = self.env_manager.set_active_env(&name);
                let _ = self.refresh_envs();
            }
            TelescopeAction::SwitchBuffer(path) => {
                if let Some(index) = self.buffers.iter().position(|p| p == &path) {
                    self.request_state.select(Some(index));
                    self.preview_scroll = 0;
                }
            }
        }
    }

    fn refresh_envs(&mut self) -> Result<()> {
        self.envs = self.env_manager.list_envs()?;
        Ok(())
    }

    pub async fn run(mut self) -> Result<()> {
        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        let (tx, mut rx) = mpsc::channel(100);

        // Suspended while an external editor owns the terminal, so this
        // listener doesn't race the editor's own stdin reads (that race is
        // what causes leftover keystrokes, e.g. needing an extra Enter after `:q`).
        let editor_active = Arc::new(AtomicBool::new(false));

        // Spawn input listener thread
        let tx_input = tx.clone();
        let editor_active_bg = editor_active.clone();
        tokio::spawn(async move {
            loop {
                if editor_active_bg.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                    if let Ok(event) = event::read() {
                        if tx_input.send(AppEvent::TerminalEvent(event)).await.is_err() {
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        // Periodic tick so the UI keeps redrawing (e.g. to animate the
        // pending-request spinner) even when no key is being pressed.
        let tx_tick = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(120)).await;
                if tx_tick.send(AppEvent::Tick).await.is_err() {
                    break;
                }
            }
        });

        // Run application event loop
        loop {
            terminal.draw(|f| self.ui(f))?;

            if let Some(app_event) = rx.recv().await {
                match app_event {
                    AppEvent::TerminalEvent(Event::Key(key)) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                            break;
                        }

                        // Emacs-style Ctrl+n / Ctrl+p act as Down / Up everywhere below.
                        let key_code = if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
                            KeyCode::Down
                        } else if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
                            KeyCode::Up
                        } else {
                            key.code
                        };

                        // 1. Handle Telescope Input Mode (generic — see src/telescope.rs)
                        if self.telescope.active {
                            match key_code {
                                KeyCode::Esc => {
                                    self.telescope.close();
                                }
                                KeyCode::Backspace => {
                                    self.telescope.backspace();
                                }
                                // No 'j'/'k' here (unlike other lists) — this is a text
                                // input, so j/k must be typeable into the search query.
                                // Ctrl+n/Ctrl+p (normalized to Down/Up above) still navigate.
                                KeyCode::Up => {
                                    self.telescope.move_up();
                                }
                                KeyCode::Down => {
                                    self.telescope.move_down();
                                }
                                // Ctrl+U: emacs/readline-style clear-line for the query.
                                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                    self.telescope.clear_query();
                                }
                                // Any other Ctrl+<char> combo (e.g. Ctrl+- round-tripping as
                                // Ctrl+7 on some terminals) is swallowed here rather than
                                // leaking into the query — only unmodified chars get typed.
                                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                    self.telescope.push_char(c);
                                }
                                KeyCode::Enter => {
                                    if let Some(action) = self.telescope.confirm() {
                                        self.apply_telescope_action(action);
                                    }
                                }
                                _ => {}
                            }
                            continue;
                        }

                        // 2. Handle History Popup Mode
                        if self.history_active {
                            match key_code {
                                KeyCode::Esc | KeyCode::Char('q') => {
                                    self.history_active = false;
                                }
                                KeyCode::Up | KeyCode::Char('k') => {
                                    if let Some(i) = self.history_state.selected() {
                                        self.history_state.select(Some(i.saturating_sub(1)));
                                    }
                                }
                                KeyCode::Down | KeyCode::Char('j') => {
                                    if let Some(i) = self.history_state.selected() {
                                        if i + 1 < self.history.len() {
                                            self.history_state.select(Some(i + 1));
                                        }
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(entry) = self.history_state.selected().and_then(|i| self.history.get(i)) {
                                        self.status_view = format!(
                                            "History | {} {} -> {} ({})",
                                            entry.method, entry.url, entry.status, entry.timestamp
                                        );
                                        self.response_view = entry.response.clone();
                                        self.response_scroll = 0;
                                    }
                                    self.history_active = false;
                                }
                                _ => {}
                            }
                            continue;
                        }

                        // 3. Handle Grid Edit Mode (inline Query/Headers editor)
                        if let Some(mut edit) = self.grid_edit.take() {
                            let mut exit_edit_mode = false;
                            let mut changed = false;

                            if edit.undo_tree_active {
                                match key_code {
                                    KeyCode::Esc | KeyCode::Char('q') => {
                                        edit.undo_tree_active = false;
                                    }
                                    // Rows print newest (highest id) first, so "up" moves to a
                                    // newer node (higher id) and "down" to an older one (lower id).
                                    KeyCode::Up | KeyCode::Char('k') => {
                                        if edit.undo_tree_selected + 1 < edit.undo_nodes.len() {
                                            edit.undo_tree_selected += 1;
                                        }
                                    }
                                    KeyCode::Down | KeyCode::Char('j') => {
                                        edit.undo_tree_selected = edit.undo_tree_selected.saturating_sub(1);
                                    }
                                    KeyCode::Enter => {
                                        if edit.jump_to(edit.undo_tree_selected) {
                                            changed = true;
                                        }
                                        edit.undo_tree_active = false;
                                    }
                                    _ => {}
                                }
                            } else if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
                                edit.editing_text = None;
                                if edit.undo() {
                                    changed = true;
                                }
                            } else if key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('-') | KeyCode::Char('_') | KeyCode::Char('7')) {
                                // Ctrl+Y or Ctrl+- : redo. Ctrl+- and Ctrl+_ send the same
                                // control byte (0x1F) on most terminals, and how that byte
                                // round-trips back to a KeyCode varies by terminal — some
                                // report '_', some '-', some '7' — so all are accepted.
                                edit.editing_text = None;
                                if edit.redo() {
                                    changed = true;
                                }
                            } else if edit.editing_text.is_some() {
                                // Ctrl+D commits the body text (multi-line, so Enter must stay
                                // available for literal newlines instead of meaning "commit").
                                if edit.section == GridSection::Body
                                    && key.modifiers.contains(KeyModifiers::CONTROL)
                                    && key.code == KeyCode::Char('d')
                                {
                                    let buf = edit.editing_text.take().unwrap();
                                    let old_len = edit.body_text.len();
                                    edit.body_text = buf;
                                    edit.commit(format!("Body edited ({} \u{2192} {} chars)", old_len, edit.body_text.len()));
                                    changed = true;
                                } else {
                                    match key_code {
                                        KeyCode::Esc => {
                                            edit.editing_text = None;
                                        }
                                        KeyCode::Enter if edit.section == GridSection::Body => {
                                            if let Some(buf) = edit.editing_text.as_mut() {
                                                buf.push('\n');
                                            }
                                        }
                                        KeyCode::Enter => {
                                            let buf = edit.editing_text.take().unwrap();
                                            let section_name = match edit.section { GridSection::Query => "Query", GridSection::Headers => "Headers", GridSection::Body => unreachable!() };
                                            let col_name = match edit.col { GridCol::Key => "key", GridCol::Value => "value" };
                                            let row_key = edit.rows().get(edit.row).map(|(k, _)| k.clone()).unwrap_or_default();
                                            let old_val = edit.focused_value();
                                            edit.set_focused_value(buf.clone());
                                            let label = format!(
                                                "{} {}.{}: '{}' \u{2192} '{}'",
                                                section_name, row_key, col_name, old_val, buf
                                            );
                                            edit.commit(label);
                                            changed = true;
                                        }
                                        KeyCode::Backspace => {
                                            if let Some(buf) = edit.editing_text.as_mut() {
                                                buf.pop();
                                            }
                                        }
                                        // Ctrl+U: emacs/readline-style clear-line for the cell.
                                        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                            if let Some(buf) = edit.editing_text.as_mut() {
                                                buf.clear();
                                            }
                                        }
                                        KeyCode::Char(c) => {
                                            if !key.modifiers.contains(KeyModifiers::CONTROL) {
                                                if let Some(buf) = edit.editing_text.as_mut() {
                                                    buf.push(c);
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            } else {
                                match key_code {
                                    KeyCode::Esc => {
                                        exit_edit_mode = true;
                                    }
                                    KeyCode::Char('u') => {
                                        edit.undo_tree_active = true;
                                        edit.undo_tree_selected = edit.undo_current;
                                    }
                                    KeyCode::Tab => {
                                        edit.section = match edit.section {
                                            GridSection::Query => GridSection::Headers,
                                            GridSection::Headers => GridSection::Body,
                                            GridSection::Body => GridSection::Query,
                                        };
                                        edit.row = 0;
                                        edit.col = GridCol::Key;
                                    }
                                    KeyCode::Up | KeyCode::Char('k') if edit.section != GridSection::Body => {
                                        edit.row = edit.row.saturating_sub(1);
                                    }
                                    KeyCode::Down | KeyCode::Char('j') if edit.section != GridSection::Body => {
                                        let len = edit.rows().len();
                                        if len > 0 && edit.row + 1 < len {
                                            edit.row += 1;
                                        }
                                    }
                                    KeyCode::Left | KeyCode::Char('h') if edit.section != GridSection::Body => {
                                        edit.col = GridCol::Key;
                                    }
                                    KeyCode::Right | KeyCode::Char('l') if edit.section != GridSection::Body => {
                                        edit.col = GridCol::Value;
                                    }
                                    KeyCode::Enter | KeyCode::Char('i') if edit.section == GridSection::Body => {
                                        edit.editing_text = Some(edit.body_text.clone());
                                    }
                                    KeyCode::Enter | KeyCode::Char('i') => {
                                        if !edit.rows().is_empty() {
                                            edit.editing_text = Some(edit.focused_value());
                                        }
                                    }
                                    KeyCode::Char('a') if edit.section != GridSection::Body => {
                                        edit.rows_mut().push((String::new(), String::new()));
                                        edit.row = edit.rows().len() - 1;
                                        edit.col = GridCol::Key;
                                        edit.editing_text = Some(String::new());
                                    }
                                    KeyCode::Char('d') if edit.section != GridSection::Body => {
                                        if !edit.rows().is_empty() {
                                            let section_name = match edit.section { GridSection::Query => "Query", GridSection::Headers => "Headers", GridSection::Body => unreachable!() };
                                            let row = edit.row;
                                            let (key_name, val_name) = edit.rows()[row].clone();
                                            edit.rows_mut().remove(row);
                                            let len = edit.rows().len();
                                            if len > 0 && edit.row >= len {
                                                edit.row = len - 1;
                                            }
                                            edit.commit(format!("{} delete {} (was '{}')", section_name, key_name, val_name));
                                            changed = true;
                                        }
                                    }
                                    _ => {}
                                }
                            }

                            if changed {
                                self.autosave_grid_edit(&edit);
                            }
                            if !exit_edit_mode {
                                self.grid_edit = Some(edit);
                            }
                            continue;
                        }

                        // 4. Handle Leader Key (Space) pending — next key resolves the chord
                        if self.leader_pending {
                            self.leader_pending = false;
                            match key_code {
                                KeyCode::Char('b') => {
                                    self.open_buffer_telescope();
                                }
                                KeyCode::Char('h') => {
                                    self.open_history();
                                }
                                _ => {}
                            }
                            continue;
                        }

                        // 5. Handle Help Mode
                        if self.show_help {
                            self.show_help = false;
                            continue;
                        }

                        // 6. Normal Mode Keybindings
                        match key_code {
                            KeyCode::Char('?') | KeyCode::Esc => {
                                self.show_help = true;
                            }
                            KeyCode::Char('i') => {
                                if self.selected_panel == ActivePanel::Preview {
                                    self.enter_grid_edit()?;
                                }
                            }
                            KeyCode::Char('t') => {
                                if self.selected_panel == ActivePanel::Environments {
                                    let _ = self.open_env_telescope();
                                } else {
                                    let _ = self.open_file_telescope();
                                }
                            }
                            KeyCode::Char('q') => break,
                            KeyCode::Tab => {
                                self.selected_panel = match self.selected_panel {
                                    ActivePanel::Preview => ActivePanel::Environments,
                                    ActivePanel::Environments => ActivePanel::Response,
                                    ActivePanel::Response => ActivePanel::Preview,
                                };
                            }
                            // Close/Delete buffer
                            KeyCode::Char('d') => {
                                if self.selected_panel == ActivePanel::Preview {
                                    if let Some(index) = self.request_state.selected() {
                                        if index < self.buffers.len() {
                                            self.buffers.remove(index);
                                            if self.buffers.is_empty() {
                                                self.request_state.select(None);
                                            } else {
                                                let next = index.min(self.buffers.len() - 1);
                                                self.request_state.select(Some(next));
                                            }
                                            self.preview_scroll = 0;
                                        }
                                    }
                                }
                            }
                            // Ctrl+hjkl: move focus between panels (tmux/vim-window style)
                            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.selected_panel = self.last_left_panel;
                            }
                            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.selected_panel = ActivePanel::Response;
                            }
                            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if self.selected_panel != ActivePanel::Response {
                                    self.selected_panel = ActivePanel::Environments;
                                }
                            }
                            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                if self.selected_panel != ActivePanel::Response {
                                    self.selected_panel = ActivePanel::Preview;
                                }
                            }
                            // Vim Navigation (h/l to switch panels)
                            KeyCode::Char('h') | KeyCode::Left => {
                                if self.selected_panel == ActivePanel::Response {
                                    self.selected_panel = ActivePanel::Preview;
                                } else if self.selected_panel == ActivePanel::Environments {
                                    self.selected_panel = ActivePanel::Preview;
                                }
                            }
                            KeyCode::Char('l') | KeyCode::Right => {
                                if self.selected_panel == ActivePanel::Preview || self.selected_panel == ActivePanel::Environments {
                                    self.selected_panel = ActivePanel::Response;
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if self.selected_panel == ActivePanel::Response {
                                    self.response_scroll = self.response_scroll.saturating_sub(1);
                                } else if self.selected_panel == ActivePanel::Environments {
                                    self.env_scroll = self.env_scroll.saturating_sub(1);
                                } else if self.selected_panel == ActivePanel::Preview {
                                    self.preview_scroll = self.preview_scroll.saturating_sub(1);
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if self.selected_panel == ActivePanel::Response {
                                    self.response_scroll = self.response_scroll.saturating_add(1);
                                } else if self.selected_panel == ActivePanel::Environments {
                                    self.env_scroll = self.env_scroll.saturating_add(1);
                                } else if self.selected_panel == ActivePanel::Preview {
                                    self.preview_scroll = self.preview_scroll.saturating_add(1);
                                }
                            }
                            // Shift+H / Shift+L: switch to the previous/next open buffer
                            // (works regardless of which panel is focused).
                            KeyCode::Char('H') => {
                                self.move_buffer_selection(-1);
                            }
                            KeyCode::Char('L') => {
                                self.move_buffer_selection(1);
                            }
                            KeyCode::Char(' ') => {
                                self.leader_pending = true;
                            }
                            // Edit selected file with Vim / Editor
                            KeyCode::Char('e') => {
                                if self.selected_panel == ActivePanel::Preview {
                                    if let Some(index) = self.request_state.selected() {
                                        if let Some(path) = self.buffers.get(index).cloned() {
                                            self.open_editor(&mut terminal, &path, &editor_active)?;
                                            // Discard any stray events the background
                                            // listener queued during the race window
                                            // before it saw the suspend flag.
                                            while rx.try_recv().is_ok() {}
                                        }
                                    }
                                } else if self.selected_panel == ActivePanel::Response {
                                    if !self.response_view.is_empty() {
                                        // Response text isn't backed by a file, so dump it to a
                                        // scratch file the editor can open (e.g. to copy from it).
                                        let temp_path = std::env::temp_dir()
                                            .join(format!("rnm_response_{}.txt", std::process::id()));
                                        fs::write(&temp_path, &self.response_view)?;
                                        self.open_editor(&mut terminal, &temp_path, &editor_active)?;
                                        let _ = fs::remove_file(&temp_path);
                                        while rx.try_recv().is_ok() {}
                                    }
                                } else if self.selected_panel == ActivePanel::Environments {
                                    if let Some(name) = self.env_manager.get_active_env_name()? {
                                        let path = self.env_manager.env_file_path(&name);
                                        self.open_editor(&mut terminal, &path, &editor_active)?;
                                        while rx.try_recv().is_ok() {}
                                    }
                                }
                            }
                            KeyCode::Enter => {
                                if self.selected_panel == ActivePanel::Preview {
                                    self.start_request_execution(tx.clone()).await?;
                                }
                            }
                            KeyCode::Char('r') => {
                                self.refresh_envs()?;
                            }
                            _ => {}
                        }

                        if self.selected_panel != ActivePanel::Response {
                            self.last_left_panel = self.selected_panel;
                        }
                    }
                    AppEvent::ResponseFinished { status, response, method, url, path } => {
                        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("request").to_string();
                        let _ = self.history_manager.record(&filename, &method, &url, &status, &response);

                        self.running_requests.remove(&path);
                        if Some(&path) == self.focused_request.as_ref() {
                            self.status_view = status;
                            self.response_view = response;
                            self.focused_request = None;
                        }
                    }
                    AppEvent::Tick => {
                        if !self.running_requests.is_empty() {
                            self.spinner_frame = self.spinner_frame.wrapping_add(1);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        Ok(())
    }

    fn open_editor<B: ratatui::backend::Backend>(&mut self, terminal: &mut Terminal<B>, path: &Path, editor_active: &Arc<AtomicBool>) -> Result<()> {
        // Stop the background input listener from reading stdin before the
        // editor takes it over, otherwise the two race for keystrokes.
        editor_active.store(true, Ordering::Relaxed);

        disable_raw_mode()?;
        execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        // Open editor: $VISUAL (conventional for full-screen editors), then
        // $EDITOR, then fall back to vim. This runs whatever binary the user
        // points to directly, so their own nvim/vim config and plugins load
        // exactly as if invoked from their shell.
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vim".to_string());
        let _ = std::process::Command::new(editor)
            .arg(path)
            .status();

        // Restore terminal
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture
        )?;
        let _ = io::stdout().flush();
        terminal.clear()?;
        terminal.hide_cursor()?;

        editor_active.store(false, Ordering::Relaxed);

        Ok(())
    }

    /// Switches the buffer the Preview panel shows (Shift+H/L), independent of panel focus.
    fn move_buffer_selection(&mut self, offset: i32) {
        if self.buffers.is_empty() { return; }
        let current = self.request_state.selected().unwrap_or(0) as i32;
        let next = (current + offset).rem_euclid(self.buffers.len() as i32) as usize;
        self.request_state.select(Some(next));
        self.preview_scroll = 0;
    }

    fn open_history(&mut self) {
        self.history = self.history_manager.load_all().unwrap_or_default();
        self.history.reverse(); // most recent first
        self.history_state.select(if self.history.is_empty() { None } else { Some(0) });
        self.history_active = true;
    }

    /// Builds the live preview for the currently selected request buffer.
    /// Pure/read-only so it can be recomputed on every frame as the selection moves.
    /// Query params are returned separately (sorted) so the UI can render them as
    /// a grid/table, Postman-style, instead of flat text.
    fn compute_selected_preview(&self) -> PreviewData {
        let empty = |top: String| PreviewData { top, query_rows: Vec::new(), header_rows: Vec::new(), body_text: String::new(), raw_file: String::new() };

        let Some(index) = self.request_state.selected() else {
            return empty("No request selected.".to_string());
        };
        let Some(path) = self.buffers.get(index) else {
            return empty("No request selected.".to_string());
        };
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("request");

        let raw_content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => return empty(format!("[Failed to read {}: {}]", filename, e)),
        };

        let env_vars = self.env_manager.load_active_env().unwrap_or_default();
        let interpolated = self.env_manager.replace_variables(&raw_content, &env_vars);

        match serde_yaml::from_str::<RequestFile>(&interpolated) {
            Ok(req_file) => {
                let body_str = req_file.body.as_ref()
                    .map(|b| serde_json::to_string_pretty(b).unwrap_or_else(|_| format!("{:?}", b)))
                    .unwrap_or_else(|| "  None".to_string());

                let mut query_rows: Vec<(String, String)> = req_file.query.as_ref()
                    .map(|q| q.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                query_rows.sort_by(|a, b| a.0.cmp(&b.0));

                let mut header_rows: Vec<(String, String)> = req_file.headers.as_ref()
                    .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                header_rows.sort_by(|a, b| a.0.cmp(&b.0));

                let top = format!(
                    "* Name: {}\n* Method: {}\n* URL: {}",
                    req_file.name.as_deref().unwrap_or("(unnamed)"),
                    req_file.method.to_uppercase(),
                    req_file.url,
                );

                PreviewData {
                    top,
                    query_rows,
                    header_rows,
                    body_text: body_str,
                    raw_file: interpolated,
                }
            }
            Err(e) => PreviewData {
                top: format!("[Could not parse as a request: {}]", e),
                query_rows: Vec::new(),
                header_rows: Vec::new(),
                body_text: String::new(),
                raw_file: interpolated,
            },
        }
    }

    /// Snapshot the selected buffer's Query params / Headers into an editable grid,
    /// parsed from the RAW (uninterpolated) file so `{{var}}` placeholders survive edits.
    fn enter_grid_edit(&mut self) -> Result<()> {
        let Some(index) = self.request_state.selected() else { return Ok(()); };
        let Some(path) = self.buffers.get(index).cloned() else { return Ok(()); };

        let raw_content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) => {
                self.status_view = format!("Cannot edit: {}", e);
                return Ok(());
            }
        };
        let original: RequestFile = match serde_yaml::from_str(&raw_content) {
            Ok(rf) => rf,
            Err(e) => {
                self.status_view = format!("Cannot edit: {}", e);
                return Ok(());
            }
        };

        let mut query_rows: Vec<(String, String)> = original.query.clone()
            .map(|q| q.into_iter().collect())
            .unwrap_or_default();
        query_rows.sort_by(|a, b| a.0.cmp(&b.0));

        let mut header_rows: Vec<(String, String)> = original.headers.clone()
            .map(|h| h.into_iter().collect())
            .unwrap_or_default();
        header_rows.sort_by(|a, b| a.0.cmp(&b.0));

        let body_text = original.body.as_ref()
            .map(|b| serde_json::to_string_pretty(b).unwrap_or_else(|_| format!("{:?}", b)))
            .unwrap_or_default();

        let root = UndoNode {
            parent: None,
            children: Vec::new(),
            depth: 0,
            snapshot: GridSnapshot { query_rows: query_rows.clone(), header_rows: header_rows.clone(), body_text: body_text.clone() },
            label: "Initial".to_string(),
            timestamp: crate::history::current_timestamp(),
            created_at: crate::history::now_unix(),
        };

        self.grid_edit = Some(GridEditState {
            path,
            original,
            query_rows,
            header_rows,
            body_text,
            section: GridSection::Query,
            row: 0,
            col: GridCol::Key,
            editing_text: None,
            undo_nodes: vec![root],
            undo_current: 0,
            undo_tree_active: false,
            undo_tree_selected: 0,
        });
        Ok(())
    }

    /// Saves the grid state to disk and updates the status bar, used after every
    /// committed mutation (edit/add/delete/undo/redo/jump) per the auto-save model.
    fn autosave_grid_edit(&mut self, edit: &GridEditState) {
        match self.save_grid_edit(edit) {
            Ok(()) => {
                self.status_view = format!(
                    "Saved {}",
                    edit.path.file_name().and_then(|s| s.to_str()).unwrap_or("")
                );
            }
            Err(e) => {
                self.status_view = format!("Save failed: {}", e);
            }
        }
    }

    /// Writes the edited Query params / Headers back into the request file, keeping
    /// every other field (method/url/body/exports/name) from the original untouched.
    /// Rows with an empty key are dropped (lets Esc-after-'a' discard a blank row).
    fn save_grid_edit(&self, edit: &GridEditState) -> Result<()> {
        let mut req_file = edit.original.clone();

        let query_map: std::collections::HashMap<String, String> = edit.query_rows.iter()
            .filter(|(k, _)| !k.is_empty())
            .cloned()
            .collect();
        let header_map: std::collections::HashMap<String, String> = edit.header_rows.iter()
            .filter(|(k, _)| !k.is_empty())
            .cloned()
            .collect();

        req_file.query = if query_map.is_empty() { None } else { Some(query_map) };
        req_file.headers = if header_map.is_empty() { None } else { Some(header_map) };

        let trimmed_body = edit.body_text.trim();
        req_file.body = if trimmed_body.is_empty() {
            None
        } else {
            match serde_json::from_str::<serde_json::Value>(trimmed_body) {
                Ok(json_val) => Some(serde_yaml::to_value(&json_val)?),
                // Not valid JSON — store as a plain string scalar, same as the
                // request runner already accepts for a `body: "..."` string.
                Err(_) => Some(serde_yaml::Value::String(edit.body_text.clone())),
            }
        };

        let yaml = serde_yaml::to_string(&req_file)?;
        fs::write(&edit.path, yaml)?;
        Ok(())
    }

    async fn start_request_execution(&mut self, tx: mpsc::Sender<AppEvent>) -> Result<()> {
        if let Some(index) = self.request_state.selected() {
            if let Some(path) = self.buffers.get(index).cloned() {
                // Don't fire a second copy of a request that's already in flight.
                if self.running_requests.contains(&path) {
                    return Ok(());
                }

                let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("request");
                self.status_view = format!("Sending {}...", filename);
                self.response_view = String::new();
                self.running_requests.insert(path.clone());
                self.focused_request = Some(path.clone());
                self.response_scroll = 0;

                let env_profile = self.env_manager.get_active_env_name()?.unwrap_or_else(|| "default".to_string());
                let env_vars = self.env_manager.load_env(&env_profile)?;
                let env_manager_clone = self.env_manager.clone();

                tokio::spawn(async move {
                    let result = Self::execute_request_task(path.clone(), env_vars, env_manager_clone).await;
                    let (status, response, method, url) = match result {
                        Ok(res) => res,
                        Err(e) => (
                            "Request Failed".to_string(),
                            format!("Error: {}", e),
                            "?".to_string(),
                            "?".to_string(),
                        ),
                    };
                    let _ = tx.send(AppEvent::ResponseFinished { status, response, method, url, path }).await;
                });
            }
        }
        Ok(())
    }

    async fn execute_request_task(
        path: PathBuf,
        env_vars: std::collections::HashMap<String, String>,
        env_manager: EnvManager,
    ) -> Result<(String, String, String, String)> {
        let file_content = fs::read_to_string(&path)?;
        let interpolated = env_manager.replace_variables(&file_content, &env_vars);
        let req_file: RequestFile = serde_yaml::from_str(&interpolated)?;

        let client = reqwest::Client::new();
        let method = reqwest::Method::from_bytes(req_file.method.to_uppercase().as_bytes())?;

        let mut builder = client.request(method, &req_file.url);
        if let Some(ref query) = req_file.query {
            builder = builder.query(query);
        }
        if let Some(ref headers) = req_file.headers {
            for (k, v) in headers {
                builder = builder.header(k, v);
            }
        }
        if let Some(ref body) = req_file.body {
            match body {
                serde_yaml::Value::String(s) => { builder = builder.body(s.clone()); }
                other => {
                    let json_val = serde_json::to_value(other)?;
                    builder = builder.json(&json_val);
                }
            }
        }

        let start = std::time::Instant::now();
        let res = builder.send().await?;
        let duration = start.elapsed();
        let status = res.status();
        let status_view = format!("Status: {} - {:?}", status, duration);

        let headers_str = res.headers()
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("<binary>")))
            .collect::<Vec<String>>()
            .join("\n");

        let body_bytes = res.bytes().await?;
        let body_str = String::from_utf8_lossy(&body_bytes);

        let mut export_logs: Vec<String> = Vec::new();

        let formatted_body = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&body_str) {
            if let Some(ref exp_map) = req_file.exports {
                for (env_var, json_path) in exp_map {
                    match Self::resolve_json_path_static(&json_val, json_path) {
                        Some(val) => {
                            let val_str = match val {
                                serde_json::Value::String(s) => s.clone(),
                                serde_json::Value::Number(n) => n.to_string(),
                                serde_json::Value::Bool(b) => b.to_string(),
                                serde_json::Value::Null => "null".to_string(),
                                other => serde_json::to_string(other).unwrap_or_default(),
                            };
                            match env_manager.update_active_env_var(env_var, &val_str) {
                                Ok(()) => export_logs.push(format!("  Exported {} = {}", env_var, val_str)),
                                Err(e) => export_logs.push(format!("  Failed to export {}: {}", env_var, e)),
                            }
                        }
                        None => {
                            export_logs.push(format!("  Failed to export {}: path '{}' not found in response", env_var, json_path));
                        }
                    }
                }
            }
            serde_json::to_string_pretty(&json_val).unwrap_or_else(|_| body_str.to_string())
        } else {
            if req_file.exports.is_some() {
                export_logs.push("  Skipped: response body is not valid JSON".to_string());
            }
            body_str.to_string()
        };

        let exports_section = if req_file.exports.is_some() {
            format!(
                "\n\n=== Exports ===\n{}",
                if export_logs.is_empty() { "  None".to_string() } else { export_logs.join("\n") }
            )
        } else {
            String::new()
        };

        let req_headers_str = req_file.headers.as_ref()
            .map(|h| sorted_kv_lines(h))
            .unwrap_or_else(|| "  None".to_string());

        let req_query_str = req_file.query.as_ref()
            .map(|q| sorted_kv_lines(q))
            .unwrap_or_else(|| "  None".to_string());

        let req_body_str = req_file.body.as_ref()
            .map(|b| serde_json::to_string_pretty(&b).unwrap_or_else(|_| format!("{:?}", b)))
            .unwrap_or_else(|| "  None".to_string());

        let response_view = format!(
            "=== Request ===\n\
             * Method: {}\n\
             * URL: {}\n\
             * Query:\n{}\n\
             * Headers:\n{}\n\
             * Body:\n{}\n\n\
             === Response ===\n\
             * Status: {}\n\
             * Duration: {:?}\n\
             * Headers:\n{}\n\
             * Body:\n{}\
             {}",
            req_file.method.to_uppercase(),
            req_file.url,
            req_query_str,
            req_headers_str,
            req_body_str,
            status,
            duration,
            headers_str,
            formatted_body,
            exports_section
        );
        Ok((status_view, response_view, req_file.method.to_uppercase(), req_file.url.clone()))
    }

    fn resolve_json_path_static<'a>(json: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
        if path.starts_with('/') {
            json.pointer(path)
        } else {
            let parts: Vec<&str> = path.split('.').collect();
            let mut current = json;
            for part in parts {
                current = current.get(part)?;
            }
            Some(current)
        }
    }

    fn ui(&mut self, f: &mut ratatui::Frame) {
        let size = f.size();

        // Top layout: Sidebar and Main Content
        let main_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30), // Sidebar
                Constraint::Percentage(70), // Content area
            ])
            .split(size);

        // Content area: live Preview beside the Response panel
        let content_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(40), // Preview
                Constraint::Percentage(60), // Response
            ])
            .split(main_chunks[1]);

        // Sidebar layout: Requests on top, Environments below
        let sidebar_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(60),
                Constraint::Percentage(40),
            ])
            .split(main_chunks[0]);

        // 1. Render Open Buffers list — informational only, not a focus target;
        // switch buffers with Shift+H/L or <leader>b, not by focusing this panel.
        let req_border_style = Style::default().fg(Color::White);

        let req_items: Vec<ListItem> = self.buffers
            .iter()
            .map(|path| {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("Request");
                let is_pending = self.running_requests.contains(path);
                if is_pending {
                    let spinner = SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()];
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{} ", spinner), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                        Span::styled(name, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    ]))
                } else {
                    ListItem::new(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(name, Style::default().fg(Color::Cyan)),
                    ]))
                }
            })
            .collect();

        let req_list = List::new(req_items)
            .block(Block::default().borders(Borders::ALL).title(" Open Buffers ('d' to close) ").border_style(req_border_style))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol(">>");
        f.render_stateful_widget(req_list, sidebar_chunks[0], &mut self.request_state);

        // 2. Render active Environment's variables (switch env via 't' -> Telescope)
        let env_border_style = if self.selected_panel == ActivePanel::Environments {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let active_env = self.env_manager.get_active_env_name().unwrap_or(None);
        let env_title = match &active_env {
            Some(name) => format!(" Environment | {} ('t' to switch) ", name),
            None => " Environment | none ('t' to choose) ".to_string(),
        };
        let env_text = match &active_env {
            Some(name) => {
                let mut vars: Vec<(String, String)> = self.env_manager.load_env(name).unwrap_or_default().into_iter().collect();
                vars.sort_by(|a, b| a.0.cmp(&b.0));
                if vars.is_empty() {
                    "No variables set for this environment.\nPress 'e' to edit its file.".to_string()
                } else {
                    vars.iter().map(|(k, v)| format!("{} = {}", k, v)).collect::<Vec<String>>().join("\n")
                }
            }
            None => "No active environment.\nPress 't' to choose one via Telescope.".to_string(),
        };

        let env_panel = Paragraph::new(env_text)
            .block(Block::default().borders(Borders::ALL).title(env_title).border_style(env_border_style))
            .wrap(Wrap { trim: false })
            .scroll((self.env_scroll, 0));
        f.render_widget(env_panel, sidebar_chunks[1]);

        // 3. Render live Preview panel (always reflects the selected request buffer).
        // Query params & Headers render as Postman-style KEY/VALUE grids; press 'i' (on the
        // Preview panel) to edit them inline: a=add row, d=delete row, Enter/i=edit cell,
        // Tab=switch grid, Ctrl+S=save, Esc=cancel edit / exit editor.
        let selected_path = self.request_state.selected().and_then(|i| self.buffers.get(i));
        let editing_selected = self.grid_edit.as_ref().map_or(false, |e| Some(&e.path) == selected_path);
        let preview_focused = self.selected_panel == ActivePanel::Preview;

        let preview_title = match selected_path {
            Some(path) => {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("request");
                if editing_selected {
                    format!(" Preview | {} [EDITING] ", name)
                } else {
                    format!(" Preview | {} ", name)
                }
            }
            None => " Preview ".to_string(),
        };
        let preview_border_style = if editing_selected || preview_focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let preview_block = Block::default().borders(Borders::ALL).title(preview_title).border_style(preview_border_style);
        let preview_inner = preview_block.inner(content_chunks[0]);
        f.render_widget(preview_block, content_chunks[0]);

        if self.buffers.is_empty() {
            let empty_msg = Paragraph::new("No buffers open. Press 't' to open Telescope and search for request files.")
                .wrap(Wrap { trim: false });
            f.render_widget(empty_msg, preview_inner);
        } else {
            struct FocusInfo { section: GridSection, row: usize, col: GridCol, editing_text: Option<String> }

            let (top, query_rows, header_rows, body_content, focus): (String, Vec<(String, String)>, Vec<(String, String)>, String, Option<FocusInfo>) =
                if let Some(edit) = &self.grid_edit {
                    let top = format!(
                        "* Name: {}\n* Method: {}\n* URL: {}",
                        edit.original.name.as_deref().unwrap_or("(unnamed)"),
                        edit.original.method.to_uppercase(),
                        edit.original.url,
                    );
                    let body_content = if edit.section == GridSection::Body {
                        match &edit.editing_text {
                            Some(buf) => format!("{}\u{2588}", buf),
                            None => edit.body_text.clone(),
                        }
                    } else {
                        edit.body_text.clone()
                    };
                    let focus = FocusInfo { section: edit.section, row: edit.row, col: edit.col, editing_text: edit.editing_text.clone() };
                    (top, edit.query_rows.clone(), edit.header_rows.clone(), body_content, Some(focus))
                } else {
                    let preview = self.compute_selected_preview();
                    let body_content = if preview.body_text.is_empty() {
                        format!("None\n\n--- Raw File ---\n{}", preview.raw_file)
                    } else {
                        format!("{}\n\n--- Raw File ---\n{}", preview.body_text, preview.raw_file)
                    };
                    (preview.top, preview.query_rows, preview.header_rows, body_content, None)
                };

            let query_label = match &focus {
                Some(f) if f.section == GridSection::Query => "* Query Params: (Tab to switch to Headers)",
                _ => "* Query Params:",
            };
            let header_label = match &focus {
                Some(f) if f.section == GridSection::Headers => "* Headers: (Tab to switch to Body)",
                _ => "* Headers:",
            };
            let body_label = match &focus {
                Some(f) if f.section == GridSection::Body => {
                    if f.editing_text.is_some() {
                        "* Body: (editing — Enter: newline, Ctrl+D: save & exit, Esc: cancel)"
                    } else {
                        "* Body: (Tab to switch to Query — i/Enter to edit)"
                    }
                }
                _ => "* Body:",
            };
            let label_style = |is_focused: bool| if is_focused {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            let top_height = top.lines().count().max(1) as u16;
            let query_h = (query_rows.len() as u16 + 1).clamp(2, 8);
            let header_h = (header_rows.len() as u16 + 1).clamp(2, 8);

            let preview_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(top_height),
                    Constraint::Length(1),
                    Constraint::Length(query_h),
                    Constraint::Length(1),
                    Constraint::Length(header_h),
                    Constraint::Length(1),
                    Constraint::Min(0),
                ])
                .split(preview_inner);

            f.render_widget(Paragraph::new(top).wrap(Wrap { trim: false }), preview_chunks[0]);

            let query_focus_active = matches!(&focus, Some(f) if f.section == GridSection::Query);
            f.render_widget(
                Paragraph::new(query_label).style(label_style(query_focus_active)),
                preview_chunks[1],
            );
            let query_focus = focus.as_ref().filter(|f| f.section == GridSection::Query)
                .map(|f| (f.row, f.col, f.editing_text.as_deref()));
            f.render_widget(render_kv_table(&query_rows, query_focus), preview_chunks[2]);

            let header_focus_active = matches!(&focus, Some(f) if f.section == GridSection::Headers);
            f.render_widget(
                Paragraph::new(header_label).style(label_style(header_focus_active)),
                preview_chunks[3],
            );
            let header_focus = focus.as_ref().filter(|f| f.section == GridSection::Headers)
                .map(|f| (f.row, f.col, f.editing_text.as_deref()));
            f.render_widget(render_kv_table(&header_rows, header_focus), preview_chunks[4]);

            let body_focus_active = matches!(&focus, Some(f) if f.section == GridSection::Body);
            f.render_widget(
                Paragraph::new(body_label).style(label_style(body_focus_active)),
                preview_chunks[5],
            );
            let body_style = if body_focus_active {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            f.render_widget(
                Paragraph::new(body_content).style(body_style).wrap(Wrap { trim: false }).scroll((self.preview_scroll, 0)),
                preview_chunks[6],
            );
        }

        // 4. Render Main Content (Response / Status Panel)
        let response_border_style = if self.selected_panel == ActivePanel::Response {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let display_title = format!(" Response | {} ", self.status_view);
        let display_text = if self.focused_request.is_some() && self.response_view.is_empty() {
            "Sending HTTP request in background... You can continue navigating and trigger other requests too."
        } else if self.response_view.is_empty() {
            "No response yet. Press Enter to run the selected request.\n\n\
             Press Space then h to browse Request/Response History.\n\
             Press ? for the full list of keybindings."
        } else {
            &self.response_view
        };

        let response_panel = Paragraph::new(display_text)
            .block(Block::default().borders(Borders::ALL).title(display_title).border_style(response_border_style))
            .wrap(Wrap { trim: false })
            .scroll((self.response_scroll, 0));
        f.render_widget(response_panel, content_chunks[1]);

        // Draw Telescope Popup if active (generic — see src/telescope.rs)
        if self.telescope.active {
            let telescope_area = centered_rect(60, 60, size);
            f.render_widget(Clear, telescope_area);

            let telescope_layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Input query box
                    Constraint::Min(3),    // Results list
                ])
                .split(telescope_area);

            let query_widget = Paragraph::new(self.telescope.query.as_str())
                .block(Block::default().borders(Borders::ALL).title(self.telescope.title.clone()));
            f.render_widget(query_widget, telescope_layout[0]);

            let matches = self.telescope.filtered();
            let matches_items: Vec<ListItem> = matches
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let style = if idx == self.telescope.selected {
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(if idx == self.telescope.selected { ">> " } else { "   " }),
                        Span::styled(item.display.clone(), style),
                    ]))
                })
                .collect();

            let matches_list = List::new(matches_items)
                .block(Block::default().borders(Borders::ALL).title(format!(" {} found ", matches.len())));
            f.render_widget(matches_list, telescope_layout[1]);
        }

        // Draw Help Popup if active
        if self.show_help {
            let block = Block::default()
                .title(" Help | Press any key to close ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD));
            
            let help_text = "\
               Keybindings:\n\n\
               * Shift+H / Shift+L: Switch to previous / next open buffer\n\
                                     (Preview panel always shows the selected buffer)\n\
               * Space b          : Telescope: switch between already-open buffers\n\
               * Space h          : Open Request/Response History\n\
               * t                : Preview panel: Open Telescope File Finder (open new buffer)\n\
                                     Environments panel: Open Telescope Environment Switcher\n\
               * d                : Close selected buffer (Preview panel)\n\
               * Tab              : Switch panels (Cycle: Preview -> Environments -> Response)\n\
               * h/l, Left/Right  : Switch panels (Preview/Env <-> Response only)\n\
               * Ctrl+h/j/k/l     : Move focus between panels (tmux/vim-window style)\n\
               * j/k, Up/Down     : Scroll Preview / Response / environment vars\n\
               * Ctrl+n / Ctrl+p  : Same as Down / Up (Emacs style), everywhere\n\
               * Enter            : Run the selected request (Preview panel)\n\
               * e                : Edit request file (Preview panel) /\n\
                                     view response in editor to copy (Response panel) /\n\
                                     edit active environment's variables (Environments panel)\n\
               * i                : Edit Query Params / Headers / Body inline (Preview panel):\n\
                                     Tab cycles Query -> Headers -> Body, j/k row, h/l column,\n\
                                     a add row, d delete row (Query/Headers only),\n\
                                     Enter/i edit cell or body (auto-saves to disk on commit),\n\
                                     Body text: Enter = newline, Ctrl+D = save & exit,\n\
                                     Ctrl+Z/Ctrl+Y undo/redo, u: browse Undo Tree, Esc: exit\n\
               * ? / Esc          : Toggle this Help popup\n\
               * q, Ctrl+C        : Quit app";

            let paragraph = Paragraph::new(help_text)
                .block(block)
                .wrap(Wrap { trim: false });

            let area = centered_rect(50, 45, size);
            f.render_widget(Clear, area);
            f.render_widget(paragraph, area);
        }

        // Draw History Popup if active
        if self.history_active {
            let area = centered_rect(75, 70, size);
            f.render_widget(Clear, area);

            let history_items: Vec<ListItem> = self.history
                .iter()
                .map(|entry| {
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{} ", entry.timestamp), Style::default().fg(Color::DarkGray)),
                        Span::styled(format!("{:<6}", entry.method), Style::default().fg(Color::Cyan)),
                        Span::raw(format!("{}  ", entry.url)),
                        Span::styled(entry.status.clone(), Style::default().fg(Color::Yellow)),
                    ]))
                })
                .collect();

            let title = if self.history.is_empty() {
                " History (empty) | Esc/q to close ".to_string()
            } else {
                format!(" History ({} entries) | Enter: view in Response, Esc/q: close ", self.history.len())
            };

            let history_list = List::new(history_items)
                .block(Block::default().borders(Borders::ALL).title(title).border_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)))
                .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
                .highlight_symbol(">> ");
            f.render_stateful_widget(history_list, area, &mut self.history_state);
        }

        // Draw Undo Tree Popup if active (branching history for the grid editor)
        if let Some(edit) = self.grid_edit.as_ref() {
            if edit.undo_tree_active {
                let area = centered_rect(90, 75, size);
                f.render_widget(Clear, area);

                let popup_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                    .split(area);

                let now = crate::history::now_unix();
                let lines = build_undo_tree_graph_lines(&edit.undo_nodes, edit.undo_current, now);
                let tree_items: Vec<ListItem> = lines.iter().map(|(id, line)| {
                    let is_current = *id == Some(edit.undo_current);
                    let is_selected = *id == Some(edit.undo_tree_selected);
                    let style = if id.is_none() {
                        Style::default().fg(Color::DarkGray)
                    } else if is_current {
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(if is_selected { ">> " } else { "   " }),
                        Span::styled(line.clone(), style),
                    ]))
                }).collect();

                let tree_title = format!(
                    " Undo Tree ({} states) | \u{25cf}=current \u{25cb}=other | Enter: jump, Esc/q: close ",
                    edit.undo_nodes.len()
                );
                let tree_list = List::new(tree_items)
                    .block(Block::default().borders(Borders::ALL).title(tree_title).border_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)));
                f.render_widget(tree_list, popup_chunks[0]);

                // Diff panel: selected node vs its parent (what changed to reach it)
                let selected_node = edit.undo_nodes.get(edit.undo_tree_selected);
                let parent_node = selected_node.and_then(|n| n.parent).and_then(|p| edit.undo_nodes.get(p));
                let diff_lines: Vec<Line> = match (selected_node, parent_node) {
                    (Some(node), Some(parent)) => {
                        let mut merged = diff_rows(&parent.snapshot.query_rows, &node.snapshot.query_rows)
                            .into_iter().map(|(k, l)| (k, format!("Query {}", l))).collect::<Vec<_>>();
                        merged.extend(
                            diff_rows(&parent.snapshot.header_rows, &node.snapshot.header_rows)
                                .into_iter().map(|(k, l)| (k, format!("Headers {}", l)))
                        );
                        if merged.is_empty() {
                            vec![Line::from("(no change vs previous state)")]
                        } else {
                            merged.into_iter().map(|(kind, l)| {
                                let color = match kind {
                                    DiffKind::Added => Color::Green,
                                    DiffKind::Removed => Color::Red,
                                    DiffKind::Changed => Color::Yellow,
                                };
                                Line::from(Span::styled(l, Style::default().fg(color)))
                            }).collect()
                        }
                    }
                    _ => vec![Line::from("(initial state — nothing before it)")],
                };

                let diff_panel = Paragraph::new(diff_lines)
                    .block(Block::default().borders(Borders::ALL).title(" Diff vs previous state ").border_style(Style::default().fg(Color::White)))
                    .wrap(Wrap { trim: false });
                f.render_widget(diff_panel, popup_chunks[1]);
            }
        }
    }
}

/// Formats a HashMap's entries as sorted "  key: value" lines. HashMap iteration
/// order is randomized per-instance, so without sorting, re-parsing the same YAML
/// on every redraw (e.g. the live Preview pane) makes headers/query params visibly
/// flicker as their printed order shifts from frame to frame.
fn sorted_kv_lines(map: &std::collections::HashMap<String, String>) -> String {
    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries.iter().map(|(k, v)| format!("  {}: {}", k, v)).collect::<Vec<String>>().join("\n")
}

fn relative_time(created_at: u64, now: u64) -> String {
    let elapsed = now.saturating_sub(created_at);
    if elapsed < 5 {
        "just now".to_string()
    } else if elapsed < 60 {
        format!("{}s ago", elapsed)
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

/// Renders the undo tree as a vertical column graph, `git log --graph` style:
/// nodes newest-first (descending id — ids are creation order), each branch kept
/// in its own fixed column joined by `│`, with a `/` fold-connector row wherever a
/// branch's column merges back into its parent's column further down (further back
/// in time). `None` in the result marks a connector-only row (not a real, selectable node).
fn build_undo_tree_graph_lines(nodes: &[UndoNode], current: usize, now: u64) -> Vec<(Option<usize>, String)> {
    if nodes.is_empty() {
        return Vec::new();
    }

    // A node's earliest-created child continues its column; every other child
    // (a genuine branch) gets a fresh column to the right.
    let mut column = vec![0usize; nodes.len()];
    let mut next_col = 1usize;
    fn assign(nodes: &[UndoNode], id: usize, col: usize, column: &mut Vec<usize>, next_col: &mut usize) {
        column[id] = col;
        for (i, &child) in nodes[id].children.iter().enumerate() {
            let c = if i == 0 {
                col
            } else {
                let c = *next_col;
                *next_col += 1;
                c
            };
            assign(nodes, child, c, column, next_col);
        }
    }
    assign(nodes, 0, 0, &mut column, &mut next_col);
    let total_cols = next_col;

    let row_cells = |active: &std::collections::BTreeSet<usize>, mark: Option<(usize, &str)>| -> String {
        let mut s = String::new();
        for c in 0..total_cols {
            match mark {
                Some((mc, glyph)) if mc == c => s.push_str(glyph),
                _ if active.contains(&c) => s.push('\u{2502}'),
                _ => s.push(' '),
            }
            s.push(' ');
        }
        s
    };

    let mut active: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut out: Vec<(Option<usize>, String)> = Vec::new();

    for id in (0..nodes.len()).rev() {
        let col = column[id];
        active.insert(col);

        let node = &nodes[id];
        let marker = if id == current { "\u{25cf}" } else { "\u{25cb}" };
        let cells = row_cells(&active, Some((col, marker)));
        let branch_note = if node.children.len() > 1 {
            format!(" ({} branches)", node.children.len())
        } else {
            String::new()
        };
        let line = format!(
            "{}{} ({})  {}{}",
            cells, node.label, relative_time(node.created_at, now), node.timestamp, branch_note
        );
        out.push((Some(id), line));

        // This column ends here (going further back in time) if we've reached the
        // root, or if the parent lives in a different column — i.e. this branch's
        // fork point. Either way, fold it back before continuing.
        let is_terminal = match node.parent {
            None => true,
            Some(p) => column[p] != col,
        };
        if is_terminal {
            active.remove(&col);
            if node.parent.is_some() {
                out.push((None, row_cells(&active, Some((col, "/")))));
            }
        }
    }

    out
}

enum DiffKind { Added, Removed, Changed }

/// Diffs two (key, value) row-lists, returning only the lines that differ —
/// used to compare an undo-tree node against its parent, undotree-style.
fn diff_rows(old: &[(String, String)], new: &[(String, String)]) -> Vec<(DiffKind, String)> {
    let old_map: std::collections::HashMap<&str, &str> = old.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let new_map: std::collections::HashMap<&str, &str> = new.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

    let mut keys: Vec<&str> = old_map.keys().chain(new_map.keys()).cloned().collect();
    keys.sort();
    keys.dedup();

    let mut lines = Vec::new();
    for k in keys {
        match (old_map.get(k), new_map.get(k)) {
            (None, Some(v)) => lines.push((DiffKind::Added, format!("+ {}: {}", k, v))),
            (Some(v), None) => lines.push((DiffKind::Removed, format!("- {}: {}", k, v))),
            (Some(ov), Some(nv)) if ov != nv => lines.push((DiffKind::Changed, format!("~ {}: {} \u{2192} {}", k, ov, nv))),
            _ => {}
        }
    }
    lines
}

/// Renders a KEY/VALUE grid (Postman-style). `focus` is `Some((row, col, editing_text))`
/// when this grid is the one currently being edited — highlights that cell and, if
/// `editing_text` is set, shows the in-progress buffer with a cursor instead of the stored value.
fn render_kv_table(rows: &[(String, String)], focus: Option<(usize, GridCol, Option<&str>)>) -> Table<'static> {
    let table_rows: Vec<Row> = rows.iter().enumerate().map(|(i, (k, v))| {
        let cell_focus = focus.filter(|(fr, _, _)| *fr == i);

        let key_text = match cell_focus {
            Some((_, GridCol::Key, Some(buf))) => format!("{}\u{2588}", buf),
            _ => k.clone(),
        };
        let val_text = match cell_focus {
            Some((_, GridCol::Value, Some(buf))) => format!("{}\u{2588}", buf),
            _ => v.clone(),
        };

        let focused_style = Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD);
        let key_style = if matches!(cell_focus, Some((_, GridCol::Key, _))) { focused_style } else { Style::default().fg(Color::Cyan) };
        let val_style = if matches!(cell_focus, Some((_, GridCol::Value, _))) { focused_style } else { Style::default() };

        Row::new(vec![
            Cell::from(key_text).style(key_style),
            Cell::from(val_text).style(val_style),
        ])
    }).collect();

    Table::new(table_rows, [Constraint::Percentage(35), Constraint::Percentage(65)])
        .header(Row::new(vec!["KEY", "VALUE"]).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)))
        .column_spacing(1)
}

fn centered_rect(percent_x: u16, percent_y: u16, r: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
