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
        if visible_rows == 0 || self.filtered_len() == 0 {
            self.scroll = 0;
            return;
        }

        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible_rows {
            self.scroll = self.selected + 1 - visible_rows;
        }
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
        self.refresh()?;

        if let Some(old_name) = old_name {
            if let Some(index) = self.entries.iter().position(|entry| entry.name == old_name) {
                self.selected = index;
            }
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

    fn display_lines(&self) -> Vec<GitStatusLine> {
        let mut lines = Vec::new();
        let staged = self.changes.iter().filter(|change| change.staged).count();
        let modified = self.changes.iter().filter(|change| change.modified).count();

        lines.push(GitStatusLine {
            label: format!("branch {}", self.branch),
            style: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        });
        lines.push(GitStatusLine {
            label: format!("staged {staged}  modified {modified}"),
            style: Style::default().fg(Color::Gray),
        });

        if staged > 0 {
            lines.push(GitStatusLine {
                label: "staged".to_string(),
                style: Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            });
            for change in self.changes.iter().filter(|change| change.staged) {
                lines.push(GitStatusLine {
                    label: format!("S {}", change.path),
                    style: Style::default().fg(Color::Green),
                });
            }
        }

        if modified > 0 {
            lines.push(GitStatusLine {
                label: "modified".to_string(),
                style: Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            });
            for change in self.changes.iter().filter(|change| change.modified) {
                lines.push(GitStatusLine {
                    label: format!("M {}", change.path),
                    style: Style::default().fg(Color::Red),
                });
            }
        }

        if self.changes.is_empty() {
            lines.push(GitStatusLine {
                label: "clean".to_string(),
                style: Style::default().fg(Color::Green),
            });
        }

        lines
    }
}

#[derive(Debug, Clone)]
struct GitStatusLine {
    label: String,
    style: Style,
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
    path: PathBuf,
}

impl DirectoryWatcher {
    fn new(path: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(
            move |event| {
                let _ = tx.send(event);
            },
            NotifyConfig::default(),
        )
        .context("failed to create filesystem watcher")?;
        watcher
            .watch(path, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch {}", path.display()))?;

        Ok(Self {
            watcher,
            rx,
            path: path.to_path_buf(),
        })
    }

    fn sync_path(&mut self, path: &Path) -> Result<()> {
        if self.path == path {
            return Ok(());
        }

        self.watcher
            .unwatch(&self.path)
            .with_context(|| format!("failed to unwatch {}", self.path.display()))?;
        self.watcher
            .watch(path, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch {}", path.display()))?;
        self.path = path.to_path_buf();
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
    let mut directory_watcher = DirectoryWatcher::new(&app.cwd)?;

    loop {
        terminal.draw(|frame| draw(frame, app))?;

        if directory_watcher.drain() {
            if let Err(err) = app.refresh() {
                app.message = Some(err.to_string());
            }
        }

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }

        let size = terminal.size()?;
        let layout = UiLayout::from(
            Rect::new(0, 0, size.width, size.height),
            app.git_status.is_some(),
        );
        let visible_rows = visible_file_rows(layout.files);

        match event::read()? {
            TerminalEvent::Key(key) => {
                match handle_key(app, key)? {
                    KeyAction::Continue => {}
                    KeyAction::Quit => return Ok(app.cwd.clone()),
                }
                run_pending_command(terminal, app);
                if let Err(err) = directory_watcher.sync_path(&app.cwd) {
                    app.message = Some(err.to_string());
                }
                app.ensure_selected_visible(visible_rows);
            }
            TerminalEvent::Mouse(mouse) => {
                if handle_mouse(app, mouse, layout)? {
                    return Ok(app.cwd.clone());
                }
                run_pending_command(terminal, app);
                if let Err(err) = directory_watcher.sync_path(&app.cwd) {
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
    let layout = UiLayout::from(frame.area(), app.git_status.is_some());

    draw_header(frame, layout.header, app);
    draw_entries(frame, layout.files, app);
    draw_preview(frame, layout.preview, app);
    if let Some(area) = layout.git {
        draw_git_status(frame, area, app);
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
    fn from(area: Rect, show_git: bool) -> Self {
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

        let (preview, git) = if show_git {
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
    let text = if app.command_active {
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
            "/ search  u hidden  ! command  Click select  Double-click open  Wheel scroll  Esc/q close"
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
