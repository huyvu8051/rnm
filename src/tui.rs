use std::fs;
use std::io;
use std::path::{Path, PathBuf};
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
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use tokio::sync::mpsc;

use crate::env::EnvManager;
use crate::request::RequestFile;

pub struct TuiApp {
    env_manager: EnvManager,
    requests: Vec<PathBuf>,
    request_state: ListState,
    envs: Vec<String>,
    env_state: ListState,
    selected_panel: ActivePanel,
    response_view: String,
    status_view: String,
    loading: bool,
    running_request: Option<PathBuf>,
    response_scroll: u16,
}

#[derive(PartialEq, Clone, Copy)]
enum ActivePanel {
    Requests,
    Environments,
    Response,
}

enum AppEvent {
    TerminalEvent(Event),
    ResponseFinished {
        status: String,
        response: String,
        path: PathBuf,
    },
}

impl TuiApp {
    pub fn new(env_manager: EnvManager) -> Result<Self> {
        let mut app = Self {
            env_manager,
            requests: Vec::new(),
            request_state: ListState::default(),
            envs: Vec::new(),
            env_state: ListState::default(),
            selected_panel: ActivePanel::Requests,
            response_view: String::new(),
            status_view: String::new(),
            loading: false,
            running_request: None,
            response_scroll: 0,
        };

        app.refresh_requests()?;
        app.refresh_envs()?;
        Ok(app)
    }

    fn refresh_requests(&mut self) -> Result<()> {
        let mut files = Vec::new();
        self.scan_dir(Path::new("."), &mut files)?;
        self.requests = files;
        if !self.requests.is_empty() {
            if self.request_state.selected().is_none() {
                self.request_state.select(Some(0));
            }
        } else {
            self.request_state.select(None);
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

    fn refresh_envs(&mut self) -> Result<()> {
        self.envs = self.env_manager.list_envs()?;
        let active = self.env_manager.get_active_env_name()?;
        if let Some(active_name) = active {
            let index = self.envs.iter().position(|e| e == &active_name);
            self.env_state.select(index);
        } else if !self.envs.is_empty() {
            if self.env_state.selected().is_none() {
                self.env_state.select(Some(0));
            }
        } else {
            self.env_state.select(None);
        }
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

        // Spawn input listener thread
        let tx_input = tx.clone();
        tokio::spawn(async move {
            loop {
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

        // Run application event loop
        loop {
            terminal.draw(|f| self.ui(f))?;

            if let Some(app_event) = rx.recv().await {
                match app_event {
                    AppEvent::TerminalEvent(Event::Key(key)) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                            break;
                        }

                        match key.code {
                            KeyCode::Char('q') => break,
                            KeyCode::Tab => {
                                self.selected_panel = match self.selected_panel {
                                    ActivePanel::Requests => ActivePanel::Environments,
                                    ActivePanel::Environments => ActivePanel::Response,
                                    ActivePanel::Response => ActivePanel::Requests,
                                };
                            }
                            // Vim Navigation (h/l to switch panels)
                            KeyCode::Char('h') | KeyCode::Left => {
                                if self.selected_panel == ActivePanel::Response {
                                    self.selected_panel = ActivePanel::Requests;
                                } else if self.selected_panel == ActivePanel::Environments {
                                    self.selected_panel = ActivePanel::Requests;
                                }
                            }
                            KeyCode::Char('l') | KeyCode::Right => {
                                if self.selected_panel == ActivePanel::Requests || self.selected_panel == ActivePanel::Environments {
                                    self.selected_panel = ActivePanel::Response;
                                }
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                if self.selected_panel == ActivePanel::Response {
                                    self.response_scroll = self.response_scroll.saturating_sub(1);
                                } else {
                                    self.move_selection(-1);
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                if self.selected_panel == ActivePanel::Response {
                                    self.response_scroll = self.response_scroll.saturating_add(1);
                                } else {
                                    self.move_selection(1);
                                }
                            }
                            // Edit selected file with Vim / Editor
                            KeyCode::Char('e') => {
                                if self.selected_panel == ActivePanel::Requests {
                                     if let Some(index) = self.request_state.selected() {
                                         if let Some(path) = self.requests.get(index).cloned() {
                                             self.open_editor(&mut terminal, &path)?;
                                         }
                                     }
                                }
                            }
                            KeyCode::Enter => {
                                if self.selected_panel == ActivePanel::Requests {
                                    self.start_request_execution(tx.clone()).await?;
                                } else if self.selected_panel == ActivePanel::Environments {
                                    let _ = self.activate_selected_env();
                                }
                            }
                            KeyCode::Char('r') => {
                                self.refresh_requests()?;
                                self.refresh_envs()?;
                            }
                            _ => {}
                        }
                    }
                    AppEvent::ResponseFinished { status, response, path } => {
                        // Check if the received response is for the currently running request
                        if Some(path) == self.running_request {
                            self.status_view = status;
                            self.response_view = response;
                            self.loading = false;
                            self.running_request = None;
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

    fn open_editor<B: ratatui::backend::Backend>(&mut self, terminal: &mut Terminal<B>, path: &Path) -> Result<()> {
        disable_raw_mode()?;
        execute!(
            io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        // Open editor (default to vim)
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
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
        terminal.clear()?;

        // Refresh lists
        self.refresh_requests()?;
        Ok(())
    }

    fn move_selection(&mut self, offset: i32) {
        match self.selected_panel {
            ActivePanel::Requests => {
                if self.requests.is_empty() { return; }
                let current = self.request_state.selected().unwrap_or(0) as i32;
                let next = (current + offset).rem_euclid(self.requests.len() as i32) as usize;
                self.request_state.select(Some(next));
            }
            ActivePanel::Environments => {
                if self.envs.is_empty() { return; }
                let current = self.env_state.selected().unwrap_or(0) as i32;
                let next = (current + offset).rem_euclid(self.envs.len() as i32) as usize;
                self.env_state.select(Some(next));
            }
            _ => {}
        }
    }

    fn activate_selected_env(&mut self) -> Result<()> {
        if let Some(index) = self.env_state.selected() {
            if let Some(name) = self.envs.get(index) {
                self.env_manager.set_active_env(name)?;
            }
        }
        Ok(())
    }

    async fn start_request_execution(&mut self, tx: mpsc::Sender<AppEvent>) -> Result<()> {
        if let Some(index) = self.request_state.selected() {
            if let Some(path) = self.requests.get(index).cloned() {
                self.loading = true;
                let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("request");
                self.status_view = format!("Sending {}...", filename);
                self.response_view = String::new();
                self.running_request = Some(path.clone());
                self.response_scroll = 0;

                let env_profile = self.env_manager.get_active_env_name()?.unwrap_or_else(|| "default".to_string());
                let env_vars = self.env_manager.load_env(&env_profile)?;
                let env_manager_clone = self.env_manager.clone();

                // Spawn task to perform request in background
                tokio::spawn(async move {
                    let result = Self::execute_request_task(path.clone(), env_vars, env_manager_clone).await;
                    let (status, response) = match result {
                        Ok(res) => res,
                        Err(e) => ("Request Failed".to_string(), format!("Error: {}", e)),
                    };
                    let _ = tx.send(AppEvent::ResponseFinished { status, response, path }).await;
                });
            }
        }
        Ok(())
    }

    async fn execute_request_task(
        path: PathBuf,
        env_vars: std::collections::HashMap<String, String>,
        env_manager: EnvManager,
    ) -> Result<(String, String)> {
        let file_content = fs::read_to_string(&path)?;
        let interpolated = env_manager.replace_variables(&file_content, &env_vars);
        let req_file: RequestFile = serde_yaml::from_str(&interpolated)?;

        let client = reqwest::Client::new();
        let method = reqwest::Method::from_bytes(req_file.method.to_uppercase().as_bytes())?;

        let mut builder = client.request(method, &req_file.url);
        if let Some(headers) = req_file.headers {
            for (k, v) in headers {
                builder = builder.header(k, v);
            }
        }
        if let Some(body) = req_file.body {
            match body {
                serde_yaml::Value::String(s) => { builder = builder.body(s); }
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

        let formatted_body = if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&body_str) {
            // Extract exports if any
            if let Some(ref exp_map) = req_file.exports {
                for (env_var, json_path) in exp_map {
                    if let Some(val) = Self::resolve_json_path_static(&json_val, json_path) {
                        let val_str = match val {
                            serde_json::Value::String(s) => s.clone(),
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            _ => serde_json::to_string(val).unwrap_or_default(),
                        };
                        let _ = env_manager.update_active_env_var(env_var, &val_str);
                    }
                }
            }
            serde_json::to_string_pretty(&json_val).unwrap_or_else(|_| body_str.to_string())
        } else {
            body_str.to_string()
        };

        let response_view = format!("=== Headers ===\n{}\n\n=== Body ===\n{}", headers_str, formatted_body);
        Ok((status_view, response_view))
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

        // Sidebar layout: Requests on top, Environments below
        let sidebar_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(60),
                Constraint::Percentage(40),
            ])
            .split(main_chunks[0]);

        // 1. Render Requests List
        let req_border_style = if self.selected_panel == ActivePanel::Requests {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let req_items: Vec<ListItem> = self.requests
            .iter()
            .map(|path| {
                let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("Request");
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(name, Style::default().fg(Color::Cyan)),
                ]))
            })
            .collect();

        let req_list = List::new(req_items)
            .block(Block::default().borders(Borders::ALL).title(" Requests (Enter to Run, 'e' to Edit) ").border_style(req_border_style))
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol(">>");
        f.render_stateful_widget(req_list, sidebar_chunks[0], &mut self.request_state);

        // 2. Render Environments List
        let env_border_style = if self.selected_panel == ActivePanel::Environments {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let active_env = self.env_manager.get_active_env_name().unwrap_or(None);
        let env_items: Vec<ListItem> = self.envs
            .iter()
            .map(|env_name| {
                let is_active = Some(env_name) == active_env.as_ref();
                let style = if is_active {
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(Line::from(vec![
                    Span::raw(if is_active { "* " } else { "  " }),
                    Span::styled(env_name, style),
                ]))
            })
            .collect();

        let env_list = List::new(env_items)
            .block(Block::default().borders(Borders::ALL).title(" Environments (Enter to Select) ").border_style(env_border_style))
            .highlight_style(Style::default().bg(Color::DarkGray))
            .highlight_symbol(">>");
        f.render_stateful_widget(env_list, sidebar_chunks[1], &mut self.env_state);

        // 3. Render Main Content (Response / Status Panel)
        let response_border_style = if self.selected_panel == ActivePanel::Response {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let display_title = format!(" Response | {} ", self.status_view);
        let display_text = if self.loading && self.response_view.is_empty() {
            "Sending HTTP request in background... You can continue navigating using Vim keys."
        } else if self.response_view.is_empty() {
            "Select a request and press Enter to execute.\n\n\
             Keybindings:\n\
             - Tab, h/l, Left/Right: Switch panels\n\
             - j/k, Up/Down: Navigate / Scroll response\n\
             - Enter: Run request / Kích hoạt environment\n\
             - e: Open selected request file in Vim\n\
             - r: Refresh folder files\n\
             - q, Ctrl+C: Quit"
        } else {
            &self.response_view
        };

        let response_panel = Paragraph::new(display_text)
            .block(Block::default().borders(Borders::ALL).title(display_title).border_style(response_border_style))
            .wrap(Wrap { trim: false })
            .scroll((self.response_scroll, 0));
        f.render_widget(response_panel, main_chunks[1]);
    }
}
