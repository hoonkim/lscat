use std::{
    cmp,
    collections::HashMap,
    env, fs,
    io::{self, Stderr, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as TerminalEvent, KeyCode, KeyEvent,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify::{Config as NotifyConfig, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::{Color, Line, Modifier, Span, Style},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Wrap,
    },
};
use serde::Deserialize;
use syntect::{
    highlighting::{
        HighlightIterator, HighlightState, Highlighter as SynHighlighter, Theme, ThemeSet,
    },
    parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet},
};

fn main() -> Result<()> {
    let config = CliConfig::from_args()?;
    let start_dir = config
        .start_dir
        .unwrap_or(env::current_dir().context("failed to read current directory")?);
    let app_config = AppConfig::load()?;
    let mut app = App::new(start_dir, app_config)?;

    let final_dir = run_app(&mut app)?;

    if config.print_cwd {
        println!("{}", final_dir.display());
    }

    if let Some(path) = config.output_file {
        fs::write(&path, final_dir.display().to_string())
            .with_context(|| format!("failed to write output file {}", path.display()))?;
    }

    if config.spawn_shell {
        spawn_shell(&final_dir)?;
    }

    Ok(())
}

#[derive(Debug)]
struct CliConfig {
    print_cwd: bool,
    spawn_shell: bool,
    output_file: Option<PathBuf>,
    start_dir: Option<PathBuf>,
}

impl CliConfig {
    fn from_args() -> Result<Self> {
        let mut print_cwd = false;
        let mut spawn_shell = false;
        let mut output_file = None;
        let mut start_dir = None;
        let mut args = env::args_os().skip(1);

        while let Some(arg) = args.next() {
            match arg.to_string_lossy().as_ref() {
                "--print-cwd" => print_cwd = true,
                "--spawn-shell" => spawn_shell = true,
                "--output" => {
                    let Some(path) = args.next() else {
                        anyhow::bail!("--output requires a file path");
                    };
                    output_file = Some(PathBuf::from(path));
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                value if value.starts_with('-') => {
                    anyhow::bail!("unknown option: {value}");
                }
                _ => {
                    if start_dir.is_some() {
                        anyhow::bail!("only one start directory can be provided");
                    }
                    start_dir = Some(PathBuf::from(arg));
                }
            }
        }

        if print_cwd && spawn_shell {
            anyhow::bail!("--print-cwd and --spawn-shell cannot be used together");
        }

        Ok(Self {
            print_cwd,
            spawn_shell,
            output_file,
            start_dir,
        })
    }
}

fn print_help() {
    println!(
        "lscat - terminal file explorer\n\n\
         Usage:\n\
           lscat [--print-cwd] [--spawn-shell] [--output FILE] [START_DIR]\n\n\
         Keys:\n\
           Up/Down, k/j   Move selection\n\
           u              Toggle hidden files\n\
           d              Toggle git diff mode ([/] change, </> file)\n\
           /              Search in current directory\n\
           !COMMAND       Run command here, then return to lscat\n\
           Enter, l       Open selected directory\n\
           Left, h        Go to parent directory\n\
           g/G            Jump to first/last item\n\
           r              Refresh directory\n\
           Esc, q         Close and return the current directory\n"
    );
}

fn spawn_shell(cwd: &Path) -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    Command::new(&shell)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("failed to start shell {shell} in {}", cwd.display()))?;
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct AppConfig {
    #[serde(default, alias = "extensions", alias = "commands")]
    openers: HashMap<String, OpenerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OpenerConfig {
    Command(String),
    Detailed {
        command: String,
        #[serde(default)]
        wait: bool,
        #[serde(default)]
        mode: OpenerMode,
    },
}

impl OpenerConfig {
    fn command(&self) -> &str {
        match self {
            Self::Command(command) => command,
            Self::Detailed { command, .. } => command,
        }
    }

    fn wait(&self) -> bool {
        match self {
            Self::Command(_) => false,
            Self::Detailed { wait, .. } => *wait,
        }
    }

    fn mode(&self) -> OpenerMode {
        match self {
            Self::Command(_) => OpenerMode::Inline,
            Self::Detailed { mode, .. } => *mode,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum OpenerMode {
    #[default]
    Inline,
    KittyOverlay,
    KittyTab,
    KittyWindow,
}

impl AppConfig {
    fn load() -> Result<Self> {
        let Some(path) = config_path()? else {
            return Ok(Self::default());
        };

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config {}", path.display()))?;
        serde_yaml::from_str(&raw)
            .with_context(|| format!("failed to parse config {}", path.display()))
    }

    fn command_for(&self, entry: &Entry) -> Option<PendingCommand> {
        let extension = entry
            .path
            .extension()
            .map(|extension| extension.to_string_lossy().to_lowercase())?;
        let opener = self
            .openers
            .get(&extension)
            .or_else(|| self.openers.get(&format!(".{extension}")))?;

        Some(PendingCommand {
            command: expand_command_template(opener.command(), entry),
            wait: opener.wait(),
            mode: opener.mode(),
        })
    }
}

fn config_path() -> Result<Option<PathBuf>> {
    let mut candidates = Vec::new();
    candidates.push(env::current_dir()?.join(".lscat/config.yaml"));

    if let Some(home) = env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".lscat/config.yaml"));
    }

    Ok(candidates.into_iter().find(|path| path.is_file()))
}

fn expand_command_template(template: &str, entry: &Entry) -> String {
    let file = shell_quote(&entry.name);
    let path = shell_quote(&entry.path.display().to_string());
    let name = shell_quote(
        entry
            .path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or(&entry.name),
    );

    if template.contains("{file}") || template.contains("{path}") || template.contains("{name}") {
        template
            .replace("{file}", &file)
            .replace("{path}", &path)
            .replace("{name}", &name)
    } else {
        format!("{template} {file}")
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':'))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Debug)]
struct App {
    cwd: PathBuf,
    config: AppConfig,
    entries: Vec<Entry>,
    show_hidden: bool,
    selected: usize,
    scroll: usize,
    git_scroll: usize,
    git_status: Option<GitStatus>,
    last_click: Option<(usize, Instant)>,
    search_active: bool,
    search: String,
    command_active: bool,
    command_input: String,
    pending_command: Option<PendingCommand>,
    message: Option<String>,
    diff_mode: bool,
    diff_selected: usize,
    diff_scroll: usize,
    diff_view_rows: usize,
    diff_focus: usize,
    diff_raw_lines: Vec<RawDiffLine>,
    diff_lines: Vec<DiffLine>,
    diff_highlighted: usize,
    diff_hl: Option<DiffHlState>,
    diff_key: String,
    diff_raw: String,
    highlighter: Option<Highlighter>,
}

#[derive(Debug, Clone)]
struct PendingCommand {
    command: String,
    wait: bool,
    mode: OpenerMode,
}

impl App {
    fn new(path: PathBuf, config: AppConfig) -> Result<Self> {
        let cwd = fs::canonicalize(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        let mut app = Self {
            cwd,
            config,
            entries: Vec::new(),
            show_hidden: false,
            selected: 0,
            scroll: 0,
            git_scroll: 0,
            git_status: None,
            last_click: None,
            search_active: false,
            search: String::new(),
            command_active: false,
            command_input: String::new(),
            pending_command: None,
            message: None,
            diff_mode: false,
            diff_selected: 0,
            diff_scroll: 0,
            diff_view_rows: 0,
            diff_focus: 0,
            diff_raw_lines: Vec::new(),
            diff_lines: Vec::new(),
            diff_highlighted: 0,
            diff_hl: None,
            diff_key: String::new(),
            diff_raw: String::new(),
            highlighter: None,
        };
        app.refresh()?;
        Ok(app)
    }

    fn refresh(&mut self) -> Result<()> {
        let selected_name = self.selected_entry().map(|entry| entry.name.clone());
        self.entries = read_entries(&self.cwd)?;
        self.git_status = read_git_status(&self.cwd).ok().flatten();
        self.git_scroll = cmp::min(self.git_scroll, self.git_status_lines().saturating_sub(1));
        let len = self.filtered_len();
        if len == 0 {
            self.selected = 0;
            self.scroll = 0;
        } else {
            self.selected = cmp::min(self.selected, len - 1);
            self.scroll = cmp::min(self.scroll, len - 1);
            if let Some(selected_name) = selected_name {
                if let Some(index) = self
                    .filtered_indices()
                    .iter()
                    .position(|entry_index| self.entries[*entry_index].name == selected_name)
                {
                    self.selected = index;
                }
            }
        }

        if self.diff_mode {
            let count = self.diff_target_count();
            if count == 0 {
                self.exit_diff_mode();
            } else {
                self.diff_selected = cmp::min(self.diff_selected, count - 1);
                self.reload_diff();
            }
        }

        Ok(())
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.filtered_len();
        if len == 0 {
            self.selected = 0;
            return;
        }

        let len = len as isize;
        let next = (self.selected as isize + delta).clamp(0, len - 1);
        self.selected = next as usize;
    }

    fn select(&mut self, index: usize) {
        let len = self.filtered_len();
        if len > 0 {
            self.selected = cmp::min(index, len - 1);
        }
    }

    fn scroll_by(&mut self, delta: isize, visible_rows: usize) {
        let len = self.filtered_len();
        if len == 0 || visible_rows == 0 {
            self.scroll = 0;
            return;
        }

        let max_scroll = len.saturating_sub(visible_rows);
        let next = (self.scroll as isize + delta).clamp(0, max_scroll as isize);
        self.scroll = next as usize;
    }

    fn scroll_git_by(&mut self, delta: isize, visible_rows: usize) {
        let len = self.git_status_lines();
        if len == 0 || visible_rows == 0 {
            self.git_scroll = 0;
            return;
        }

        let max_scroll = len.saturating_sub(visible_rows);
        let next = (self.git_scroll as isize + delta).clamp(0, max_scroll as isize);
        self.git_scroll = next as usize;
    }

    fn git_status_lines(&self) -> usize {
        self.git_status
            .as_ref()
            .map(|status| status.display_lines().len())
            .unwrap_or(0)
    }

    fn ensure_selected_visible(&mut self, visible_rows: usize) {
        let len = self.filtered_len();
        if visible_rows == 0 || len == 0 {
            self.scroll = 0;
            return;
        }

        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
        self.scroll = cmp::min(self.scroll, len.saturating_sub(visible_rows));
    }

    fn first(&mut self) {
        self.selected = 0;
    }

    fn last(&mut self) {
        let len = self.filtered_len();
        if len > 0 {
            self.selected = len - 1;
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let index = self.filtered_indices().get(self.selected).copied()?;
        self.entries.get(index)
    }

    fn select_entry_named(&mut self, name: &str) {
        if let Some(entry_index) = self.entries.iter().position(|entry| entry.name == name) {
            if self.entries[entry_index].hidden {
                self.show_hidden = true;
            }

            self.selected = self
                .entries
                .iter()
                .take(entry_index)
                .filter(|entry| self.show_hidden || !entry.hidden)
                .count();
        }
    }

    fn clear_search_filter(&mut self) {
        self.search_active = false;
        self.search.clear();
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.search.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.show_hidden || !entry.hidden)
            .filter_map(|(index, entry)| {
                (query.is_empty() || entry.name.to_lowercase().contains(&query)).then_some(index)
            })
            .collect()
    }

    fn filtered_len(&self) -> usize {
        self.filtered_indices().len()
    }

    fn set_search(&mut self, search: String) {
        self.search = search;
        self.selected = 0;
        self.scroll = 0;
        self.git_scroll = 0;
        self.last_click = None;
    }

    fn toggle_hidden(&mut self) {
        let selected_name = self.selected_entry().map(|entry| entry.name.clone());
        self.show_hidden = !self.show_hidden;
        self.selected = 0;
        self.scroll = 0;
        self.last_click = None;

        if let Some(selected_name) = selected_name {
            if let Some(index) = self
                .filtered_indices()
                .iter()
                .position(|entry_index| self.entries[*entry_index].name == selected_name)
            {
                self.selected = index;
            }
        }

        self.message = Some(if self.show_hidden {
            "showing hidden files".to_string()
        } else {
            "hiding hidden files".to_string()
        });
    }

    fn open_selected(&mut self) {
        let Some(entry) = self.selected_entry() else {
            return;
        };

        if !entry.is_dir {
            if let Some(pending_command) = self.config.command_for(entry) {
                self.pending_command = Some(pending_command);
            } else {
                self.message = Some(format!("no opener configured for {}", entry.name));
            }
            return;
        }

        let path = entry.path.clone();
        if let Err(err) = self.change_dir(path) {
            self.message = Some(err.to_string());
        }
    }

    fn toggle_diff_mode(&mut self) {
        if self.diff_mode {
            self.exit_diff_mode();
            return;
        }

        let initial = {
            let Some(status) = self.git_status.as_ref() else {
                self.message = Some("not a git repository".to_string());
                return;
            };
            let targets = status.diff_targets();
            if targets.is_empty() {
                self.message = Some("no changes to diff".to_string());
                return;
            }

            self.selected_entry()
                .and_then(|entry| entry.path.strip_prefix(&status.root).ok())
                .map(normalize_git_path)
                .and_then(|rel| targets.iter().position(|target| target.path == rel))
                .unwrap_or(0)
        };

        if self.highlighter.is_none() {
            self.highlighter = Some(Highlighter::new());
        }

        self.diff_mode = true;
        self.diff_selected = initial;
        self.message = None;
        self.load_diff();
    }

    fn exit_diff_mode(&mut self) {
        self.diff_mode = false;
        self.diff_scroll = 0;
        self.clear_diff();
        self.message = None;
    }

    fn clear_diff(&mut self) {
        self.diff_raw_lines = Vec::new();
        self.diff_lines = Vec::new();
        self.diff_highlighted = 0;
        self.diff_focus = 0;
        self.diff_hl = None;
        self.diff_key.clear();
        self.diff_raw.clear();
    }

    fn diff_target_count(&self) -> usize {
        self.git_status
            .as_ref()
            .map(|status| status.diff_targets().len())
            .unwrap_or(0)
    }

    fn load_diff(&mut self) {
        self.diff_scroll = 0;
        self.reload_diff();
    }

    fn reload_diff(&mut self) {
        let target = self.git_status.as_ref().and_then(|status| {
            status
                .diff_targets()
                .into_iter()
                .nth(self.diff_selected)
                .map(|target| (status.root.clone(), target))
        });

        let Some((root, target)) = target else {
            self.clear_diff();
            return;
        };

        let key = format!("{}\u{0}{}", target.staged, target.path);
        match run_git_diff(&root, &target) {
            Ok(raw) => {
                // Skip re-parsing/re-highlighting when the diff is unchanged so
                // that already-highlighted lines (and progress) are preserved.
                if key == self.diff_key && raw == self.diff_raw && !self.diff_raw_lines.is_empty()
                {
                    self.diff_scroll = cmp::min(self.diff_scroll, self.max_diff_scroll());
                    return;
                }

                if raw.trim().is_empty() {
                    self.set_diff_body(vec![RawDiffLine::message("(no diff)")], None);
                } else {
                    self.set_diff_body(parse_diff_body(&raw), Some(target.path.clone()));
                }
                self.diff_key = key;
                self.diff_raw = raw;
            }
            Err(message) => {
                self.set_diff_body(vec![RawDiffLine::message(message)], None);
                self.diff_key = key;
                self.diff_raw = String::new();
            }
        }

        self.diff_scroll = cmp::min(self.diff_scroll, self.max_diff_scroll());
    }

    /// Install a freshly parsed diff body and reset the incremental highlight
    /// state. Highlighting itself is deferred to `ensure_diff_highlighted`.
    fn set_diff_body(&mut self, body: Vec<RawDiffLine>, syntax_path: Option<String>) {
        self.diff_raw_lines = body;
        self.diff_lines = Vec::new();
        self.diff_highlighted = 0;
        self.diff_focus = 0;
        self.diff_hl = self.highlighter.as_ref().map(|highlighter| {
            let syntax = match syntax_path.as_deref() {
                Some(path) => highlighter.syntax_for(path),
                None => highlighter.syntax_set.find_syntax_plain_text(),
            };
            let syn_highlighter = SynHighlighter::new(&highlighter.theme);
            DiffHlState {
                parse_old: ParseState::new(syntax),
                parse_new: ParseState::new(syntax),
                hl_old: HighlightState::new(&syn_highlighter, ScopeStack::new()),
                hl_new: HighlightState::new(&syn_highlighter, ScopeStack::new()),
            }
        });
    }

    /// Highlight diff lines up to (and including) `index`, picking up where the
    /// last call left off. Cheap when already highlighted past `index`.
    fn ensure_diff_highlighted(&mut self, index: usize) {
        let Some(highlighter) = self.highlighter.as_ref() else {
            return;
        };
        let Some(state) = self.diff_hl.as_mut() else {
            return;
        };
        let target = cmp::min(index + 1, self.diff_raw_lines.len());
        if self.diff_highlighted >= target {
            return;
        }

        let syn_highlighter = SynHighlighter::new(&highlighter.theme);
        while self.diff_highlighted < target {
            let raw = &self.diff_raw_lines[self.diff_highlighted];
            let line = build_diff_line(raw, &highlighter.syntax_set, &syn_highlighter, state);
            self.diff_lines.push(line);
            self.diff_highlighted += 1;
        }
    }

    fn diff_line_count(&self) -> usize {
        cmp::max(self.diff_raw_lines.len(), self.diff_lines.len())
    }

    fn max_diff_scroll(&self) -> usize {
        self.diff_line_count()
            .saturating_sub(self.diff_view_rows.max(1))
    }

    fn scroll_diff_by(&mut self, delta: isize) {
        if self.diff_line_count() == 0 {
            self.diff_scroll = 0;
            self.diff_focus = 0;
            return;
        }
        let max = self.max_diff_scroll() as isize;
        let next = (self.diff_scroll as isize + delta).clamp(0, max);
        self.diff_scroll = next as usize;
        // Manual scrolling moves the change-jump reference to the top of the view.
        self.diff_focus = self.diff_scroll;
    }

    fn set_diff_scroll(&mut self, scroll: usize) {
        self.diff_scroll = cmp::min(scroll, self.max_diff_scroll());
        self.diff_focus = self.diff_scroll;
    }

    fn diff_change_indices(&self) -> Vec<usize> {
        change_block_starts(&self.diff_raw_lines)
    }

    /// Jump so the change at `index` sits a few lines below the top, rather than
    /// flush against it, for readable leading context.
    fn focus_change(&mut self, index: usize) {
        self.diff_focus = index;
        self.diff_scroll = cmp::min(
            index.saturating_sub(CHANGE_JUMP_MARGIN),
            self.max_diff_scroll(),
        );
    }

    fn next_change(&mut self) {
        if let Some(next) = self
            .diff_change_indices()
            .into_iter()
            .find(|index| *index > self.diff_focus)
        {
            self.focus_change(next);
        }
    }

    fn prev_change(&mut self) {
        if let Some(prev) = self
            .diff_change_indices()
            .into_iter()
            .rev()
            .find(|index| *index < self.diff_focus)
        {
            self.focus_change(prev);
        }
    }

    fn next_diff_file(&mut self) {
        let count = self.diff_target_count();
        if count > 0 && self.diff_selected + 1 < count {
            self.diff_selected += 1;
            self.load_diff();
        }
    }

    fn prev_diff_file(&mut self) {
        if self.diff_selected > 0 {
            self.diff_selected -= 1;
            self.load_diff();
        }
    }

    fn diff_list_offset(&self, visible: usize) -> usize {
        if visible > 0 && self.diff_selected >= visible {
            self.diff_selected + 1 - visible
        } else {
            0
        }
    }

    fn current_diff_title(&self) -> String {
        self.git_status
            .as_ref()
            .and_then(|status| status.diff_targets().into_iter().nth(self.diff_selected))
            .map(|target| {
                let marker = if target.staged { "S" } else { "M" };
                format!(" diff {marker} {} ", target.path)
            })
            .unwrap_or_else(|| " diff ".to_string())
    }

    fn jump_to_git_path(&mut self, git_path: &str, visible_rows: usize) {
        let Some(root) = self.git_status.as_ref().map(|status| status.root.clone()) else {
            return;
        };
        let full_path = root.join(git_path);

        self.clear_search_filter();

        if full_path.is_dir() {
            if let Err(err) = self.change_dir(full_path) {
                self.message = Some(err.to_string());
            }
            self.clear_search_filter();
            return;
        }

        let Some(parent) = full_path.parent().map(Path::to_path_buf) else {
            return;
        };
        let Some(file_name) = full_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            return;
        };

        if file_name.starts_with('.') {
            self.show_hidden = true;
        }

        if let Err(err) = self.change_dir(parent) {
            self.message = Some(err.to_string());
            return;
        }

        self.clear_search_filter();
        self.select_entry_named(&file_name);
        self.ensure_selected_visible(visible_rows);
    }

    fn parent(&mut self) {
        let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else {
            return;
        };

        if let Err(err) = self.change_dir(parent) {
            self.message = Some(err.to_string());
        }
    }

    fn change_dir(&mut self, path: PathBuf) -> Result<()> {
        let old_name = self
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        self.cwd = fs::canonicalize(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        self.selected = 0;
        self.scroll = 0;
        self.last_click = None;
        self.search_active = false;
        self.search.clear();
        self.command_active = false;
        self.command_input.clear();
        self.pending_command = None;
        self.message = None;
        self.diff_mode = false;
        self.diff_selected = 0;
        self.diff_scroll = 0;
        self.clear_diff();
        self.refresh()?;

        if let Some(old_name) = old_name {
            self.select_entry_named(&old_name);
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    len: Option<u64>,
    hidden: bool,
}

#[derive(Debug, Clone)]
struct GitStatus {
    root: PathBuf,
    branch: String,
    changes: Vec<GitChange>,
}

#[derive(Debug, Clone)]
struct GitChange {
    path: String,
    staged: bool,
    modified: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitFileState {
    Clean,
    Modified,
    Staged,
    Both,
}

impl GitStatus {
    fn file_state(&self, entry: &Entry) -> GitFileState {
        let Ok(entry_path) = entry.path.strip_prefix(&self.root) else {
            return GitFileState::Clean;
        };
        let entry_path = normalize_git_path(entry_path);

        let mut staged = false;
        let mut modified = false;
        for change in &self.changes {
            if git_path_matches_entry(&change.path, &entry_path, entry.is_dir) {
                staged |= change.staged;
                modified |= change.modified;
            }
        }

        match (staged, modified) {
            (true, true) => GitFileState::Both,
            (true, false) => GitFileState::Staged,
            (false, true) => GitFileState::Modified,
            (false, false) => GitFileState::Clean,
        }
    }

    fn diff_targets(&self) -> Vec<DiffTarget> {
        let mut targets = Vec::new();
        for change in self.changes.iter().filter(|change| change.staged) {
            targets.push(DiffTarget {
                path: change.path.clone(),
                staged: true,
            });
        }
        for change in self.changes.iter().filter(|change| change.modified) {
            targets.push(DiffTarget {
                path: change.path.clone(),
                staged: false,
            });
        }
        targets
    }

    fn display_lines(&self) -> Vec<GitStatusLine> {
        let mut lines = Vec::new();
        let staged = self.changes.iter().filter(|change| change.staged).count();
        let modified = self.changes.iter().filter(|change| change.modified).count();

        lines.push(GitStatusLine {
            label: format!("branch {}", self.branch),
            style: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            path: None,
        });
        lines.push(GitStatusLine {
            label: format!("staged {staged}  modified {modified}"),
            style: Style::default().fg(Color::Gray),
            path: None,
        });

        if staged > 0 {
            lines.push(GitStatusLine {
                label: "staged".to_string(),
                style: Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                path: None,
            });
            for change in self.changes.iter().filter(|change| change.staged) {
                lines.push(GitStatusLine {
                    label: format!("S {}", change.path),
                    style: Style::default().fg(Color::Green),
                    path: Some(change.path.clone()),
                });
            }
        }

        if modified > 0 {
            lines.push(GitStatusLine {
                label: "modified".to_string(),
                style: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                path: None,
            });
            for change in self.changes.iter().filter(|change| change.modified) {
                lines.push(GitStatusLine {
                    label: format!("M {}", change.path),
                    style: Style::default().fg(Color::Red),
                    path: Some(change.path.clone()),
                });
            }
        }

        if self.changes.is_empty() {
            lines.push(GitStatusLine {
                label: "clean".to_string(),
                style: Style::default().fg(Color::Green),
                path: None,
            });
        }

        lines
    }
}

#[derive(Debug, Clone)]
struct GitStatusLine {
    label: String,
    style: Style,
    path: Option<String>,
}

#[derive(Debug, Clone)]
struct DiffTarget {
    path: String,
    staged: bool,
}

/// Background tints approximating a translucent overlay on a dark terminal.
const ADD_BG: Color = Color::Rgb(22, 51, 29);
const DEL_BG: Color = Color::Rgb(58, 27, 31);

/// Lines of leading context kept above a change when jumping with `[`/`]`.
const CHANGE_JUMP_MARGIN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffKind {
    Context,
    Add,
    Del,
}

/// One parsed diff body line, before syntax highlighting (cheap to build).
#[derive(Debug, Clone)]
struct RawDiffLine {
    code: String,
    kind: DiffKind,
}

impl RawDiffLine {
    fn message(text: impl Into<String>) -> Self {
        Self {
            code: text.into(),
            kind: DiffKind::Context,
        }
    }

    fn is_change(&self) -> bool {
        matches!(self.kind, DiffKind::Add | DiffKind::Del)
    }
}

/// One rendered diff line: highlighted spans plus its background tint.
#[derive(Debug, Clone)]
struct DiffLine {
    spans: Vec<Span<'static>>,
    bg: Option<Color>,
}

/// Incremental syntect state for the two sides of a diff (owned, no borrows, so
/// it can live in `App` and be advanced one line at a time).
struct DiffHlState {
    parse_old: ParseState,
    parse_new: ParseState,
    hl_old: HighlightState,
    hl_new: HighlightState,
}

impl std::fmt::Debug for DiffHlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DiffHlState")
    }
}

struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Highlighter")
    }
}

impl Highlighter {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_default();
        Self { syntax_set, theme }
    }

    fn syntax_for(&self, path: &str) -> &SyntaxReference {
        let extension = Path::new(path)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("");
        self.syntax_set
            .find_syntax_by_extension(extension)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text())
    }
}

/// Start indices of each maximal run of added/removed lines (the "change points").
fn change_block_starts(lines: &[RawDiffLine]) -> Vec<usize> {
    let mut indices = Vec::new();
    let mut prev_change = false;
    for (index, line) in lines.iter().enumerate() {
        if line.is_change() && !prev_change {
            indices.push(index);
        }
        prev_change = line.is_change();
    }
    indices
}

fn syntect_color(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

/// Context lines requested from `git diff` so the entire file is shown, not just
/// the changed hunks. Capped well above any realistic source file length.
const FULL_FILE_CONTEXT: u32 = 1_000_000;

fn run_git_diff(root: &Path, target: &DiffTarget) -> Result<String, String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .arg("diff")
        .arg(format!("--unified={FULL_FILE_CONTEXT}"));
    if target.staged {
        command.arg("--cached");
    }
    command.arg("--").arg(&target.path);

    let output = command
        .output()
        .map_err(|err| format!("failed to run git diff: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        return Err(if message.is_empty() {
            "git diff failed".to_string()
        } else {
            message.to_string()
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse a unified diff into body lines, dropping the file/hunk headers so only
/// the code remains. Header lines never carry a +/-/space prefix, so they can't
/// be confused with file content.
fn parse_diff_body(raw: &str) -> Vec<RawDiffLine> {
    let mut lines = Vec::new();
    let mut in_body = false;
    for raw_line in raw.lines() {
        if !in_body {
            // The first `@@` marks the start of the body.
            if raw_line.starts_with("@@") {
                in_body = true;
            }
            continue;
        }

        // With full context there is a single hunk, but guard against extra
        // hunk headers and the "\ No newline at end of file" marker.
        if raw_line.starts_with("@@") || raw_line.starts_with('\\') {
            continue;
        }

        let kind = match raw_line.chars().next() {
            Some('+') => DiffKind::Add,
            Some('-') => DiffKind::Del,
            _ => DiffKind::Context,
        };
        let code = raw_line.get(1..).unwrap_or("").replace('\t', "    ");
        lines.push(RawDiffLine { code, kind });
    }

    lines
}

/// Highlight one code line, advancing `parse`/`hl` so multi-line constructs keep
/// their context across calls. Returns the styled spans.
fn highlight_one(
    parse: &mut ParseState,
    hl: &mut HighlightState,
    syntax_set: &SyntaxSet,
    syn_highlighter: &SynHighlighter,
    code: &str,
) -> Vec<Span<'static>> {
    let line = format!("{code}\n");
    let ops = parse.parse_line(&line, syntax_set).unwrap_or_default();
    HighlightIterator::new(hl, &ops, &line, syn_highlighter)
        .filter_map(|(style, text)| {
            let text = text.strip_suffix('\n').unwrap_or(text);
            if text.is_empty() {
                return None;
            }
            Some(Span::styled(
                text.to_string(),
                Style::default().fg(syntect_color(style.foreground)),
            ))
        })
        .collect()
}

/// Advance the parse/highlight state for one line without producing spans (used
/// to keep the "old" side in sync over context lines).
fn advance_one(
    parse: &mut ParseState,
    hl: &mut HighlightState,
    syntax_set: &SyntaxSet,
    syn_highlighter: &SynHighlighter,
    code: &str,
) {
    let line = format!("{code}\n");
    let ops = parse.parse_line(&line, syntax_set).unwrap_or_default();
    HighlightIterator::new(hl, &ops, &line, syn_highlighter).for_each(|_| {});
}

/// Build the rendered line for one raw diff line, applying the add/remove
/// background tint and a marker gutter.
fn build_diff_line(
    raw: &RawDiffLine,
    syntax_set: &SyntaxSet,
    syn_highlighter: &SynHighlighter,
    state: &mut DiffHlState,
) -> DiffLine {
    let (bg, marker_color, marker_char) = match raw.kind {
        DiffKind::Add => (Some(ADD_BG), Color::Green, '+'),
        DiffKind::Del => (Some(DEL_BG), Color::Red, '-'),
        DiffKind::Context => (None, Color::DarkGray, ' '),
    };

    let highlighted = match raw.kind {
        DiffKind::Add => highlight_one(
            &mut state.parse_new,
            &mut state.hl_new,
            syntax_set,
            syn_highlighter,
            &raw.code,
        ),
        DiffKind::Del => highlight_one(
            &mut state.parse_old,
            &mut state.hl_old,
            syntax_set,
            syn_highlighter,
            &raw.code,
        ),
        DiffKind::Context => {
            let spans = highlight_one(
                &mut state.parse_new,
                &mut state.hl_new,
                syntax_set,
                syn_highlighter,
                &raw.code,
            );
            advance_one(
                &mut state.parse_old,
                &mut state.hl_old,
                syntax_set,
                syn_highlighter,
                &raw.code,
            );
            spans
        }
    };

    let marker_style = match bg {
        Some(bg) => Style::default().fg(marker_color).bg(bg),
        None => Style::default().fg(marker_color),
    };

    let mut spans = Vec::with_capacity(highlighted.len() + 1);
    spans.push(Span::styled(format!("{marker_char} "), marker_style));
    for span in highlighted {
        let style = match bg {
            Some(bg) => span.style.bg(bg),
            None => span.style,
        };
        spans.push(Span::styled(span.content, style));
    }

    DiffLine { spans, bg }
}

fn read_git_status(cwd: &Path) -> Result<Option<GitStatus>> {
    let root_output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .context("failed to run git rev-parse")?;
    if !root_output.status.success() {
        return Ok(None);
    }

    let root = String::from_utf8_lossy(&root_output.stdout)
        .trim()
        .to_string();
    if root.is_empty() {
        return Ok(None);
    }
    let root = PathBuf::from(root);

    let status_output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("status")
        .arg("--porcelain=v1")
        .arg("-b")
        .output()
        .context("failed to run git status")?;
    if !status_output.status.success() {
        return Ok(None);
    }

    let mut branch = "unknown".to_string();
    let mut changes = Vec::new();
    for line in String::from_utf8_lossy(&status_output.stdout).lines() {
        if let Some(raw_branch) = line.strip_prefix("## ") {
            branch = raw_branch
                .split("...")
                .next()
                .unwrap_or(raw_branch)
                .to_string();
            continue;
        }

        if line.len() < 4 {
            continue;
        }

        let mut chars = line.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        let path = line[3..].split(" -> ").last().unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }

        changes.push(GitChange {
            path,
            staged: x != ' ' && x != '?',
            modified: y != ' ' || x == '?',
        });
    }

    Ok(Some(GitStatus {
        root,
        branch,
        changes,
    }))
}

fn normalize_git_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn git_path_matches_entry(change_path: &str, entry_path: &str, is_dir: bool) -> bool {
    change_path == entry_path || (is_dir && change_path.starts_with(&format!("{entry_path}/")))
}

fn read_entries(dir: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();

    for item in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let item = item?;
        let path = item.path();
        let name = item.file_name().to_string_lossy().to_string();
        let metadata = item.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());
        let len = metadata
            .as_ref()
            .and_then(|m| m.is_file().then_some(m.len()));
        let hidden = name.starts_with('.');

        entries.push(Entry {
            name,
            path,
            is_dir,
            len,
            hidden,
        });
    }

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(entries)
}

fn run_app(app: &mut App) -> Result<PathBuf> {
    let mut terminal = setup_terminal()?;
    let result = app_loop(&mut terminal, app);
    restore_terminal(&mut terminal)?;
    result
}

struct DirectoryWatcher {
    watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<notify::Event>>,
    cwd: PathBuf,
    git_root: Option<PathBuf>,
}

impl DirectoryWatcher {
    fn new(cwd: &Path, git_root: Option<&Path>) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = tx.send(event);
            },
            NotifyConfig::default(),
        )
        .context("failed to create filesystem watcher")?;
        let git_root = git_root.map(Path::to_path_buf);
        watcher
            .watch(cwd, cwd_watch_mode(cwd, git_root.as_deref()))
            .with_context(|| format!("failed to watch {}", cwd.display()))?;

        if let Some(root) = git_root.as_ref().filter(|root| root.as_path() != cwd) {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .with_context(|| format!("failed to watch git root {}", root.display()))?;
        }

        Ok(Self {
            watcher,
            rx,
            cwd: cwd.to_path_buf(),
            git_root,
        })
    }

    fn sync_paths(&mut self, cwd: &Path, git_root: Option<&Path>) -> Result<()> {
        let next_git_root = git_root.map(Path::to_path_buf);
        if self.cwd == cwd && self.git_root == next_git_root {
            return Ok(());
        }

        self.watcher
            .unwatch(&self.cwd)
            .with_context(|| format!("failed to unwatch {}", self.cwd.display()))?;
        if let Some(root) = self.git_root.as_ref().filter(|root| *root != &self.cwd) {
            self.watcher
                .unwatch(root)
                .with_context(|| format!("failed to unwatch git root {}", root.display()))?;
        }

        self.watcher
            .watch(cwd, cwd_watch_mode(cwd, next_git_root.as_deref()))
            .with_context(|| format!("failed to watch {}", cwd.display()))?;
        if let Some(root) = next_git_root.as_ref().filter(|root| root.as_path() != cwd) {
            self.watcher
                .watch(root, RecursiveMode::Recursive)
                .with_context(|| format!("failed to watch git root {}", root.display()))?;
        }

        self.cwd = cwd.to_path_buf();
        self.git_root = next_git_root;
        self.drain();
        Ok(())
    }

    fn drain(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            if event.is_ok() {
                changed = true;
            }
        }
        changed
    }
}

fn cwd_watch_mode(cwd: &Path, git_root: Option<&Path>) -> RecursiveMode {
    if git_root.is_some_and(|root| root == cwd) {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    }
}

fn git_root(app: &App) -> Option<&Path> {
    app.git_status.as_ref().map(|status| status.root.as_path())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stderr>>> {
    terminal::enable_raw_mode().context("failed to enable raw mode")?;
    let mut stderr = io::stderr();
    execute!(
        stderr,
        EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    )
    .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stderr);
    let terminal = Terminal::new(backend).context("failed to create terminal")?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stderr>>) -> Result<()> {
    terminal::disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        cursor::Show
    )
    .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")?;
    Ok(())
}

fn app_loop(terminal: &mut Terminal<CrosstermBackend<Stderr>>, app: &mut App) -> Result<PathBuf> {
    let mut directory_watcher = DirectoryWatcher::new(&app.cwd, git_root(app))?;
    let mut pending_refresh_at = None;
    let refresh_debounce = Duration::from_millis(150);

    loop {
        // Highlight just the lines about to be drawn (lazy: keeps opening large
        // files instant regardless of total length).
        if app.diff_mode {
            let size = terminal.size()?;
            let layout = UiLayout::from(
                Rect::new(0, 0, size.width, size.height),
                app.git_status.is_some(),
                true,
            );
            let visible_rows = visible_file_rows(layout.files);
            app.diff_view_rows = visible_rows;
            app.ensure_diff_highlighted(app.diff_scroll + visible_rows);
        }

        terminal.draw(|frame| draw(frame, app))?;

        if directory_watcher.drain() {
            pending_refresh_at = Some(Instant::now() + refresh_debounce);
        }

        if pending_refresh_at.is_some_and(|at| Instant::now() >= at) {
            if let Err(err) = app.refresh() {
                app.message = Some(err.to_string());
            }
            if let Err(err) = directory_watcher.sync_paths(&app.cwd, git_root(app)) {
                app.message = Some(err.to_string());
            }
            pending_refresh_at = None;
        }

        let poll_timeout = pending_refresh_at
            .map(|at| at.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|| Duration::from_millis(200));

        if !event::poll(poll_timeout)? {
            continue;
        }

        let size = terminal.size()?;
        let layout = UiLayout::from(
            Rect::new(0, 0, size.width, size.height),
            app.git_status.is_some(),
            app.diff_mode,
        );
        let visible_rows = visible_file_rows(layout.files);
        app.diff_view_rows = visible_rows;

        match event::read()? {
            TerminalEvent::Key(key) => {
                match handle_key(app, key)? {
                    KeyAction::Continue => {}
                    KeyAction::Quit => return Ok(app.cwd.clone()),
                }
                run_pending_command(terminal, app);
                if let Err(err) = directory_watcher.sync_paths(&app.cwd, git_root(app)) {
                    app.message = Some(err.to_string());
                }
                app.ensure_selected_visible(visible_rows);
            }
            TerminalEvent::Mouse(mouse) => {
                if handle_mouse(app, mouse, layout)? {
                    return Ok(app.cwd.clone());
                }
                run_pending_command(terminal, app);
                if let Err(err) = directory_watcher.sync_paths(&app.cwd, git_root(app)) {
                    app.message = Some(err.to_string());
                }
            }
            _ => {}
        }
    }
}

fn run_pending_command(terminal: &mut Terminal<CrosstermBackend<Stderr>>, app: &mut App) {
    let Some(pending_command) = app.pending_command.take() else {
        return;
    };

    match open_command_over_app(
        terminal,
        &app.cwd,
        &pending_command.command,
        pending_command.wait,
        pending_command.mode,
    ) {
        Ok(()) => {
            app.message = Some(format!("ran: {}", pending_command.command));
            if let Err(err) = app.refresh() {
                app.message = Some(err.to_string());
            }
        }
        Err(err) => app.message = Some(err.to_string()),
    }
}

fn open_command_over_app(
    terminal: &mut Terminal<CrosstermBackend<Stderr>>,
    cwd: &Path,
    command: &str,
    wait: bool,
    mode: OpenerMode,
) -> Result<()> {
    if mode != OpenerMode::Inline {
        return run_kitty_command(cwd, command, wait, mode);
    }

    restore_terminal(terminal)?;
    let command_result = run_shell_command(cwd, command);
    let wait_result = if wait { wait_for_return() } else { Ok(()) };
    *terminal = setup_terminal()?;
    command_result.and(wait_result)
}

fn run_shell_command(cwd: &Path, command: &str) -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = Command::new(&shell)
        .arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .env("PATH", opener_path())
        .status()
        .with_context(|| format!("failed to run command: {command}"))?;

    if !status.success() {
        anyhow::bail!("command exited with {status}: {command}");
    }

    Ok(())
}

fn run_kitty_command(cwd: &Path, command: &str, wait: bool, mode: OpenerMode) -> Result<()> {
    let launch_type = match mode {
        OpenerMode::Inline => unreachable!("inline commands are handled separately"),
        OpenerMode::KittyOverlay => "overlay",
        OpenerMode::KittyTab => "tab",
        OpenerMode::KittyWindow => "window",
    };
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let command = if wait {
        format!("{command}; printf '\\nPress Enter to close...'; read _")
    } else {
        command.to_string()
    };
    let status = Command::new("kitty")
        .arg("@")
        .arg("launch")
        .arg("--type")
        .arg(launch_type)
        .arg("--cwd")
        .arg(cwd)
        .arg(&shell)
        .arg("-lc")
        .arg(&command)
        .env("PATH", opener_path())
        .status()
        .with_context(|| format!("failed to launch kitty {launch_type}: {command}"))?;

    if !status.success() {
        anyhow::bail!("kitty launch exited with {status}: {command}");
    }

    Ok(())
}

fn opener_path() -> String {
    let mut parts = env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .map(str::to_string)
        .collect::<Vec<_>>();

    for path in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/Users/kimh/.local/bin",
        "/Users/kimh/.cargo/bin",
        "/Applications/kitty.app/Contents/MacOS",
    ] {
        if !parts.iter().any(|part| part == path) {
            parts.push(path.to_string());
        }
    }

    parts.join(":")
}

fn wait_for_return() -> Result<()> {
    eprint!("\nPress Enter to return to lscat...");
    io::stderr().flush().context("failed to flush prompt")?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to wait for Enter")?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Continue,
    Quit,
}

fn handle_key(app: &mut App, key: KeyEvent) -> Result<KeyAction> {
    if app.diff_mode {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(KeyAction::Quit),
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) | (KeyCode::Char('d'), _) => {
                app.exit_diff_mode()
            }
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.scroll_diff_by(1),
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.scroll_diff_by(-1),
            (KeyCode::PageDown, _) => app.scroll_diff_by(app.diff_view_rows as isize),
            (KeyCode::PageUp, _) => app.scroll_diff_by(-(app.diff_view_rows as isize)),
            (KeyCode::Char(']'), _) => app.next_change(),
            (KeyCode::Char('['), _) => app.prev_change(),
            (KeyCode::Char('>'), _) => app.next_diff_file(),
            (KeyCode::Char('<'), _) => app.prev_diff_file(),
            (KeyCode::Char('g'), _) => app.set_diff_scroll(0),
            (KeyCode::Char('G'), _) => app.set_diff_scroll(app.diff_line_count()),
            _ => {}
        }

        return Ok(KeyAction::Continue);
    }

    if app.command_active {
        match key.code {
            KeyCode::Esc => {
                app.command_active = false;
                app.command_input.clear();
                app.message = None;
            }
            KeyCode::Enter => {
                let command = app.command_input.trim().to_string();
                app.command_active = false;
                app.command_input.clear();
                if !command.is_empty() {
                    app.pending_command = Some(PendingCommand {
                        command,
                        wait: true,
                        mode: OpenerMode::Inline,
                    });
                }
            }
            KeyCode::Backspace => {
                app.command_input.pop();
            }
            KeyCode::Char(ch) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    app.command_input.push(ch);
                }
            }
            _ => {}
        }

        return Ok(KeyAction::Continue);
    }

    if app.search_active {
        match key.code {
            KeyCode::Esc => {
                app.search_active = false;
                app.set_search(String::new());
                app.message = None;
            }
            KeyCode::Enter => {
                app.search_active = false;
                app.open_selected();
            }
            KeyCode::Down => app.move_by(1),
            KeyCode::Up => app.move_by(-1),
            KeyCode::Backspace => {
                let mut search = app.search.clone();
                search.pop();
                app.set_search(search);
            }
            KeyCode::Char(ch) => {
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
                    let mut search = app.search.clone();
                    search.push(ch);
                    app.set_search(search);
                }
            }
            _ => {}
        }

        return Ok(KeyAction::Continue);
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(KeyAction::Quit),
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) => return Ok(KeyAction::Quit),
        (KeyCode::Char('!'), _) => {
            app.command_active = true;
            app.command_input.clear();
            app.message = None;
        }
        (KeyCode::Char('/'), _) => {
            app.search_active = true;
            app.message = None;
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), _) => app.move_by(1),
        (KeyCode::Up, _) | (KeyCode::Char('k'), _) => app.move_by(-1),
        (KeyCode::Char('g'), _) => app.first(),
        (KeyCode::Char('G'), _) => app.last(),
        (KeyCode::Char('u'), _) => app.toggle_hidden(),
        (KeyCode::Char('d'), _) => app.toggle_diff_mode(),
        (KeyCode::Enter, _) | (KeyCode::Right, _) | (KeyCode::Char('l'), _) => app.open_selected(),
        (KeyCode::Backspace, _) | (KeyCode::Left, _) | (KeyCode::Char('h'), _) => app.parent(),
        (KeyCode::Char('r'), _) => {
            app.refresh()?;
            app.message = Some("refreshed".to_string());
        }
        _ => {}
    }

    Ok(KeyAction::Continue)
}

fn handle_mouse(app: &mut App, mouse: MouseEvent, layout: UiLayout) -> Result<bool> {
    let visible_rows = visible_file_rows(layout.files);
    let git_rows = layout.git.map(visible_git_rows).unwrap_or(0);

    if app.diff_mode {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(area) = layout.git {
                    let offset = app.diff_list_offset(visible_git_rows(area));
                    if let Some(index) = row_at(area, offset, mouse.column, mouse.row)
                        && index < app.diff_target_count()
                    {
                        app.diff_selected = index;
                        app.load_diff();
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => app.exit_diff_mode(),
            MouseEventKind::ScrollDown => app.scroll_diff_by(1),
            MouseEventKind::ScrollUp => app.scroll_diff_by(-1),
            _ => {}
        }

        return Ok(false);
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(path) = path_at(app, layout.header, mouse.column, mouse.row) {
                if let Err(err) = app.change_dir(path) {
                    app.message = Some(err.to_string());
                }
            } else if let Some(index) = row_at(layout.files, app.scroll, mouse.column, mouse.row) {
                if index < app.filtered_len() {
                    let is_double_click = app.last_click.is_some_and(|(last_index, last_time)| {
                        last_index == index && last_time.elapsed() <= Duration::from_millis(500)
                    });
                    app.select(index);
                    app.last_click = Some((index, Instant::now()));

                    if is_double_click {
                        app.open_selected();
                    }
                }
            } else if let Some(area) = layout.git {
                if let Some(index) = row_at(area, app.git_scroll, mouse.column, mouse.row) {
                    if let Some(status) = app.git_status.as_ref() {
                        let lines = status.display_lines();
                        if let Some(path) = lines.get(index).and_then(|line| line.path.clone()) {
                            app.jump_to_git_path(&path, visible_rows);
                        }
                    }
                }
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            if in_rect(layout.files, mouse.column, mouse.row) {
                app.parent();
            }
        }
        MouseEventKind::ScrollDown => {
            if layout
                .git
                .is_some_and(|area| in_rect(area, mouse.column, mouse.row))
            {
                app.scroll_git_by(3, git_rows);
            } else {
                app.scroll_by(3, visible_rows);
            }
        }
        MouseEventKind::ScrollUp => {
            if layout
                .git
                .is_some_and(|area| in_rect(area, mouse.column, mouse.row))
            {
                app.scroll_git_by(-3, git_rows);
            } else {
                app.scroll_by(-3, visible_rows);
            }
        }
        _ => {}
    }

    Ok(false)
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let layout = UiLayout::from(frame.area(), app.git_status.is_some(), app.diff_mode);

    draw_header(frame, layout.header, app);
    if app.diff_mode {
        draw_diff(frame, layout.files, app);
        if let Some(area) = layout.git {
            draw_diff_file_list(frame, area, app);
        }
    } else {
        draw_entries(frame, layout.files, app);
        draw_preview(frame, layout.preview, app);
        if let Some(area) = layout.git {
            draw_git_status(frame, area, app);
        }
    }
    draw_footer(frame, layout.footer, app);
}

#[derive(Debug, Clone, Copy)]
struct UiLayout {
    header: Rect,
    files: Rect,
    preview: Rect,
    git: Option<Rect>,
    footer: Rect,
}

impl UiLayout {
    fn from(area: Rect, show_git: bool, diff_mode: bool) -> Self {
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(2),
            ])
            .split(area);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(vertical[1]);

        let (preview, git) = if show_git && diff_mode {
            (body[1], Some(body[1]))
        } else if show_git {
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(body[1]);
            (right[0], Some(right[1]))
        } else {
            (body[1], None)
        };

        Self {
            header: vertical[0],
            files: body[0],
            preview,
            git,
            footer: vertical[2],
        }
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let kitty = env::var("TERM").is_ok_and(|term| term.contains("kitty"));
    let title = if kitty { " lscat - kitty " } else { " lscat " };
    let paragraph = Paragraph::new(path_line(&app.cwd))
        .block(Block::default().borders(Borders::ALL).title(title))
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, area);
}

fn path_line(path: &Path) -> Line<'static> {
    let mut spans = Vec::new();

    for (index, segment) in path_segments(path).iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("/", Style::default().fg(Color::DarkGray)));
        }

        spans.push(Span::styled(
            segment.label.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::UNDERLINED),
        ));
    }

    Line::from(spans)
}

#[derive(Debug, Clone)]
struct PathSegment {
    label: String,
    path: PathBuf,
}

fn path_segments(path: &Path) -> Vec<PathSegment> {
    let mut segments = Vec::new();
    let mut current = PathBuf::new();

    for component in path.components() {
        current.push(component.as_os_str());
        let label = component.as_os_str().to_string_lossy().to_string();
        segments.push(PathSegment {
            label,
            path: current.clone(),
        });
    }

    segments
}

fn path_at(app: &App, area: Rect, column: u16, row: u16) -> Option<PathBuf> {
    if !in_rect(area, column, row) || row != area.y + 1 {
        return None;
    }

    let mut x = area.x + 1;
    for (index, segment) in path_segments(&app.cwd).iter().enumerate() {
        if index > 0 {
            x = x.saturating_add(1);
        }

        let width = segment.label.chars().count() as u16;
        if column >= x && column < x.saturating_add(width) {
            return Some(segment.path.clone());
        }
        x = x.saturating_add(width);
    }

    None
}

fn draw_entries(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible_rows = visible_file_rows(area);
    let filtered_indices = app.filtered_indices();
    let rows = filtered_indices
        .iter()
        .filter_map(|index| app.entries.get(*index))
        .skip(app.scroll)
        .take(visible_rows)
        .map(|entry| {
            let icon = if entry.is_dir { "DIR" } else { "FILE" };
            let git_state = app
                .git_status
                .as_ref()
                .map(|status| status.file_state(entry))
                .unwrap_or(GitFileState::Clean);
            let name_style = if git_state == GitFileState::Both {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if git_state == GitFileState::Staged {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if git_state == GitFileState::Modified {
                Style::default().fg(Color::Red)
            } else if entry.is_dir {
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD)
            } else if entry.hidden {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new([
                Cell::from(icon),
                Cell::from(entry.name.clone()).style(name_style),
                Cell::from(entry_size(entry)),
            ])
        });

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(16),
            Constraint::Length(10),
        ],
    )
    .block(Block::default().borders(Borders::ALL).title(" files "))
    .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White))
    .highlight_symbol("> ");

    let mut state = TableState::default();
    if !filtered_indices.is_empty()
        && app.selected >= app.scroll
        && app.selected < app.scroll + visible_rows
    {
        state.select(Some(app.selected - app.scroll));
    }

    frame.render_stateful_widget(table, area, &mut state);
}

fn draw_git_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(git_status) = app.git_status.as_ref() else {
        return;
    };

    let lines = git_status.display_lines();
    let visible_rows = visible_git_rows(area);
    let items = lines
        .iter()
        .skip(app.git_scroll)
        .take(visible_rows)
        .map(|line| ListItem::new(line.label.clone()).style(line.style))
        .collect::<Vec<_>>();
    let title = format!(" git {} ", git_status.branch);
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));

    frame.render_widget(list, area);
}

fn draw_diff(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let visible_rows = visible_file_rows(area);
    let inner_width = area.width.saturating_sub(2) as usize;
    let total = app.diff_line_count();
    let skip = cmp::min(app.diff_scroll, total.saturating_sub(visible_rows.max(1)));
    let lines = app
        .diff_lines
        .iter()
        .skip(skip)
        .take(visible_rows)
        .map(|line| {
            let mut spans = line.spans.clone();
            if let Some(bg) = line.bg {
                let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
                if used < inner_width {
                    spans.push(Span::styled(
                        " ".repeat(inner_width - used),
                        Style::default().bg(bg),
                    ));
                }
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(app.current_diff_title()),
    );
    frame.render_widget(paragraph, area);
}

fn draw_diff_file_list(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(git_status) = app.git_status.as_ref() else {
        return;
    };

    let targets = git_status.diff_targets();
    let visible_rows = visible_git_rows(area);
    let offset = app.diff_list_offset(visible_rows);
    let items = targets
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible_rows)
        .map(|(index, target)| {
            let marker = if target.staged { "S" } else { "M" };
            let color = if target.staged {
                Color::Green
            } else {
                Color::Red
            };
            let selected = index == app.diff_selected;
            let prefix = if selected { "> " } else { "  " };
            let style = if selected {
                Style::default()
                    .fg(color)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };
            ListItem::new(format!("{prefix}{marker} {}", target.path)).style(style)
        })
        .collect::<Vec<_>>();

    let title = format!(" git {} ", git_status.branch);
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn visible_file_rows(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

fn visible_git_rows(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

fn row_at(area: Rect, scroll: usize, column: u16, row: u16) -> Option<usize> {
    if !in_rect(area, column, row) {
        return None;
    }

    let first_row = area.y + 1;
    let last_row = area.y + area.height.saturating_sub(1);
    if row < first_row || row >= last_row {
        return None;
    }

    Some(scroll + usize::from(row - first_row))
}

fn in_rect(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.x + area.width && row >= area.y && row < area.y + area.height
}

fn draw_preview(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(entry) = app.selected_entry() else {
        let empty = Paragraph::new("empty directory")
            .block(Block::default().borders(Borders::ALL).title(" preview "))
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty, area);
        return;
    };

    if entry.is_dir {
        let children = fs::read_dir(&entry.path)
            .ok()
            .map(|read_dir| {
                read_dir
                    .filter_map(|item| item.ok())
                    .take(area.height.saturating_sub(5) as usize)
                    .map(|item| {
                        let name = item.file_name().to_string_lossy().to_string();
                        let marker = item
                            .metadata()
                            .ok()
                            .and_then(|m| m.is_dir().then_some("/"))
                            .unwrap_or("");
                        ListItem::new(format!("{name}{marker}"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let list = List::new(children)
            .block(Block::default().borders(Borders::ALL).title(" preview "))
            .style(Style::default().fg(Color::Gray));
        frame.render_widget(list, area);
        return;
    }

    let metadata = fs::metadata(&entry.path).ok();
    let modified = metadata
        .and_then(|m| m.modified().ok())
        .and_then(|time| time.elapsed().ok())
        .map(format_elapsed)
        .unwrap_or_else(|| "unknown".to_string());

    let extension = entry
        .path
        .extension()
        .map(|ext| ext.to_string_lossy().to_string())
        .unwrap_or_else(|| "none".to_string());
    let content = vec![
        Line::from(Span::styled(
            entry.name.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("type: file"),
        Line::from(format!("size: {}", entry_size(entry))),
        Line::from(format!("ext: {extension}")),
        Line::from(format!("modified: {modified}")),
        Line::from(""),
        Line::from("Open directories with Enter. Files are shown as metadata for now."),
    ];

    let paragraph = Paragraph::new(content)
        .block(Block::default().borders(Borders::ALL).title(" preview "))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let text = if app.diff_mode {
        "j/k scroll  [ ] change  < > file  d/Esc/q exit".to_string()
    } else if app.command_active {
        format!("!{}", app.command_input)
    } else if app.search_active {
        format!("/{}", app.search)
    } else if !app.search.is_empty() {
        format!(
            "/{}  {} match(es)  Esc/q close  / edit search",
            app.search,
            app.filtered_len()
        )
    } else {
        app.message.clone().unwrap_or_else(|| {
            "/ search  u hidden  d diff  ! command  Click select  Double-click open  Wheel scroll  Esc/q close"
                .to_string()
        })
    };
    let paragraph = Paragraph::new(text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

fn entry_size(entry: &Entry) -> String {
    if entry.is_dir {
        return "<dir>".to_string();
    }

    let Some(bytes) = entry.len else {
        return "?".to_string();
    };

    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{size:.1}{}", UNITS[unit])
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(code: &str, kind: DiffKind) -> RawDiffLine {
        RawDiffLine {
            code: code.to_string(),
            kind,
        }
    }

    fn render(line: &RawDiffLine) -> DiffLine {
        let highlighter = Highlighter::new();
        let syntax = highlighter.syntax_for("main.rs");
        let syn_highlighter = SynHighlighter::new(&highlighter.theme);
        let mut state = DiffHlState {
            parse_old: ParseState::new(syntax),
            parse_new: ParseState::new(syntax),
            hl_old: HighlightState::new(&syn_highlighter, ScopeStack::new()),
            hl_new: HighlightState::new(&syn_highlighter, ScopeStack::new()),
        };
        build_diff_line(line, &highlighter.syntax_set, &syn_highlighter, &mut state)
    }

    #[test]
    fn added_line_gets_green_background_and_syntax_colors() {
        let line = render(&raw("    let value: u32 = 1;", DiffKind::Add));
        assert_eq!(line.bg, Some(ADD_BG));
        // Every span (marker + code) carries the add background tint.
        assert!(line.spans.iter().all(|span| span.style.bg == Some(ADD_BG)));
        // Syntax highlighting should produce more than one foreground color.
        let fg_colors: std::collections::HashSet<_> = line
            .spans
            .iter()
            .skip(1)
            .map(|span| format!("{:?}", span.style.fg))
            .collect();
        assert!(
            fg_colors.len() > 1,
            "expected multiple syntax colors, got {fg_colors:?}"
        );
    }

    #[test]
    fn removed_line_gets_red_background() {
        let line = render(&raw("let y = 2;", DiffKind::Del));
        assert_eq!(line.bg, Some(DEL_BG));
    }

    #[test]
    fn context_line_has_no_background() {
        let line = render(&raw("let z = 3;", DiffKind::Context));
        assert_eq!(line.bg, None);
    }

    #[test]
    fn parse_diff_body_drops_headers_and_classifies() {
        let diff = "diff --git a/x.rs b/x.rs\n\
                    index 1111111..2222222 100644\n\
                    --- a/x.rs\n\
                    +++ b/x.rs\n\
                    @@ -1,2 +1,3 @@\n\
                    \x20fn main() {}\n\
                    +let x = 1;\n\
                    \x20const Y: u8 = 2;\n";
        let body = parse_diff_body(diff);
        // File/hunk headers removed; the three body lines remain.
        assert_eq!(body.len(), 3);
        assert_eq!(body[0].kind, DiffKind::Context);
        assert_eq!(body[1].kind, DiffKind::Add);
        assert_eq!(body[1].code, "let x = 1;");
        assert_eq!(body[2].kind, DiffKind::Context);
    }

    #[test]
    fn change_blocks_group_consecutive_lines() {
        let lines = vec![
            raw("context", DiffKind::Context), // 0
            raw("context", DiffKind::Context), // 1
            raw("removed a", DiffKind::Del),   // 2 <- block start
            raw("removed b", DiffKind::Del),   // 3
            raw("added a", DiffKind::Add),     // 4 (still same block)
            raw("context", DiffKind::Context), // 5
            raw("added c", DiffKind::Add),     // 6 <- new block start
        ];
        assert_eq!(change_block_starts(&lines), vec![2, 6]);
    }
}
