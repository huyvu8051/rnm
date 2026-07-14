use std::path::PathBuf;

/// What happens when an item is picked (Enter). Add a variant here for each new
/// kind of picker; the popup itself doesn't need to know about any of them.
#[derive(Clone)]
pub enum TelescopeAction {
    OpenFile(PathBuf),
    SwitchEnv(String),
    SwitchBuffer(PathBuf),
}

/// A single selectable row: what's shown/fuzzy-matched, and what picking it does.
pub struct TelescopeItem {
    pub display: String,
    pub action: TelescopeAction,
}

impl TelescopeItem {
    pub fn new(display: impl Into<String>, action: TelescopeAction) -> Self {
        Self { display: display.into(), action }
    }
}

/// Generic fuzzy-picker popup: one shared query/selection/filter implementation
/// reused by every feature that needs a "type to filter, Enter to pick" list
/// (open file, switch environment, switch buffer, ...). To add a new picker,
/// build a `Vec<TelescopeItem>` and call `open()` — no changes needed here.
#[derive(Default)]
pub struct Telescope {
    pub active: bool,
    pub title: String,
    pub items: Vec<TelescopeItem>,
    pub query: String,
    pub selected: usize,
}

impl Telescope {
    pub fn open(&mut self, title: impl Into<String>, items: Vec<TelescopeItem>) {
        self.title = title.into();
        self.items = items;
        self.query.clear();
        self.selected = 0;
        self.active = true;
    }

    pub fn close(&mut self) {
        self.active = false;
    }

    pub fn filtered(&self) -> Vec<&TelescopeItem> {
        let q = self.query.to_lowercase();
        self.items.iter().filter(|it| it.display.to_lowercase().contains(&q)).collect()
    }

    pub fn move_up(&mut self) {
        if !self.filtered().is_empty() {
            self.selected = self.selected.saturating_sub(1);
        }
    }

    pub fn move_down(&mut self) {
        let len = self.filtered().len();
        if len > 0 && self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    /// Ctrl+U — emacs/readline-style clear-line for the query input.
    pub fn clear_query(&mut self) {
        self.query.clear();
        self.selected = 0;
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    /// Returns the action for the currently selected item (if any) and closes the popup.
    pub fn confirm(&mut self) -> Option<TelescopeAction> {
        let action = self.filtered().get(self.selected).map(|it| it.action.clone());
        if action.is_some() {
            self.active = false;
        }
        action
    }
}
