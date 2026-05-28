use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local};
use colored::Colorize;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use exif::{Context, Error as ExifError, Exif, Field, Reader, Tag, Value};
use little_exif::exif_tag::ExifTag as WritableExifTag;
use little_exif::ifd::ExifTagGroup;
use little_exif::metadata::Metadata as WritableMetadata;
use little_exif::rational::uR64;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use rattles::presets::prelude as spinners;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, DatabaseName, params, params_from_iter};
use serde_json::json;
use serde_yaml::{Mapping, Value as YamlValue};

pub mod cli;
pub mod version;

pub use cli::{
    CheckArgs, Cli, Command, InteractiveArgs, NewArgs, ReadArgs, ReadFormat, StripArgs, WriteArgs,
};

const GEONAMES_DATABASE: &[u8] = include_bytes!("../assets/geonames/cities1000.sqlite");
const NEAREST_LOCATION_LIMIT: usize = 5;
const NEAREST_CITY_MINIMUM_POPULATION: i64 = 200_000;
const EARTH_RADIUS_KM: f64 = 6_371.0088;
const METADATA_FILE_NAME: &str = "metadata.yml";
const LEGACY_METADATA_FILE_NAME: &str = "metadata.yaml";
const CUSTOM_TAG_PAYLOAD_MARKER: &str = concat!("exifmeta-v", env!("CARGO_PKG_VERSION"), "\n");
const LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX: &[u8] = b"exifmeta-custom-tags-v1\n";
const USER_COMMENT_ASCII_PREFIX: &[u8] = b"ASCII\0\0\0";
const PRETTY_UNKNOWN_VALUE_DISPLAY_LIMIT: usize = 120;
const PRETTY_KNOWN_VALUE_DISPLAY_LIMIT: usize = 2000;
const PRETTY_UNKNOWN_VALUE_OMITTED_LABEL: &str = "<long value omitted>";
const PRETTY_UNKNOWN_VALUE_OMITTED_HINT: &str = " (use `--format raw` to view)";
const METADATA_TEMPLATE: &str = r#"# yaml-language-server: $schema=https://raw.githubusercontent.com/hmerritt/exif-medadata/master/schemas/metadata.schema.json

# ───────────────────────────────────────────────
# Metadata file for images in this directory. Used by exifmeta, https://github.com/hmerritt/exifmeta
# ───────────────────────────────────────────────

# ───────────────────────────────────────────────
# Custom Properties
# These values will not be written as EXIF, and are meant for personal organisational
# purposes — e.g. private metadata for your shoot
# ───────────────────────────────────────────────
roll: 1
date: <today>
date_end: <today>
frame_count: <image-count-in-directory>
notable_frames: []
locations: []

# ───────────────────────────────────────────────
# Global EXIF Properties
# Any valid EXIF tag can be set here. Non-standard tags can also be set here, and will
# be written together in a single custom property.
# ───────────────────────────────────────────────
exif:
    # Camera & Lens
    Make:
    Model:
    LensMake:
    LensModel:
    FocalLength:
    MaxApertureValue:

    # Film / Capture
    ISOSpeedRatings:
    DateTimeOriginal:
    CreateDate:
    # 1 = Film Scanner
    # 2 = Reflection Print Scanner
    # 3 = Digital Camera
    FileSource: 1

    # Film
    FilmRoll:
    FilmMaker:
    FilmName:
    FilmFormat:
    FilmColor:
    FilmNegative:
    # Film Development
    FilmDevelopProcess:
    FilmDeveloper:
    FilmProcessLab:
    FilmProcessDate:
    FilmScanner:

    # Attribution
    Artist:
    Photographer:

# ───────────────────────────────────────────────
# Per Frame/File EXIF Properties
# Use this to set EXIF tags for individual files, like ExposureTime, FNumber, or
# GPS data. Values set here will override the above `exif` values.
# ───────────────────────────────────────────────
frames:
<frames>
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Error(String),
    Warning(String),
    Failure,
}

impl From<String> for CliError {
    fn from(error: String) -> Self {
        Self::Error(error)
    }
}

pub fn write(cli: Cli) -> Result<(), CliError> {
    if !matches!(
        &cli.command,
        Command::Strip(args) if args.json
    ) && !matches!(&cli.command, Command::Interactive(_))
    {
        version::print_title();
    }

    match cli.command {
        Command::Write(args) => write_command(cli.dry_run, args).map_err(Into::into),
        Command::New(args) => new_command(cli.dry_run, args),
        Command::Check(args) => check_command(args),
        Command::Read(args) => read_command(args).map_err(Into::into),
        Command::Interactive(args) => interactive_command(args).map_err(Into::into),
        Command::Strip(args) => strip_command(cli.dry_run, args),
    }
}

fn read_command(args: ReadArgs) -> Result<(), String> {
    let image = args.image;
    check_image_path(&image)?;

    let progress = TerminalSpinner::start(SpinnerPreset::random(), "reading exif".to_string());
    let metadata = read_metadata(&image);
    let metadata = metadata?;
    let output = format_read_output(&image, &metadata, args.format);
    progress.finish();

    println!("{output}");

    Ok(())
}

fn interactive_command(args: InteractiveArgs) -> Result<(), String> {
    let mut app = InteractiveApp::new(&args.path)?;
    let mut terminal = InteractiveTerminal::enter()?;

    loop {
        app.drain_preview_results();

        terminal
            .terminal
            .draw(|frame| render_interactive(frame, &mut app))
            .map_err(|error| format!("failed to draw interactive UI: {error}"))?;

        if event::poll(INTERACTIVE_PREVIEW_TICK)
            .map_err(|error| format!("failed to poll terminal event: {error}"))?
        {
            let event =
                event::read().map_err(|error| format!("failed to read terminal event: {error}"))?;
            if app.handle_event(event)? {
                break;
            }
        }

        app.tick_preview_spinner();
    }

    Ok(())
}

const INTERACTIVE_PREVIEW_TICK: Duration = Duration::from_millis(80);

struct InteractiveTerminal {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl InteractiveTerminal {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|error| format!("failed to enable raw mode: {error}"))?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(format!("failed to enter alternate screen: {error}"));
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)
            .map_err(|error| format!("failed to initialize terminal: {error}"))?;

        Ok(Self { terminal })
    }
}

impl Drop for InteractiveTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Debug)]
struct InteractiveApp {
    current_dir: PathBuf,
    entries: Vec<InteractiveEntry>,
    selected: usize,
    preview: InteractivePreviewContent,
    preview_scroll: u16,
    preview_viewport_width: u16,
    preview_viewport_height: u16,
    focus: InteractiveFocus,
    preview_sender: Sender<InteractivePreviewResult>,
    preview_receiver: Receiver<InteractivePreviewResult>,
    preview_request_id: u64,
    preview_loading: bool,
    preview_loading_visible: bool,
    preview_loading_deadline: Option<Instant>,
    preview_spinner: SpinnerPreset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveFocus {
    List,
    Preview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveDirectorySelection {
    First,
    ReselectChild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractivePreviewResult {
    request_id: u64,
    kind: InteractiveEntryKind,
    path: PathBuf,
    preview: InteractivePreviewContent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InteractivePreviewContent {
    Plain(String),
    Lines(Vec<Line<'static>>),
}

impl InteractivePreviewContent {
    fn plain(value: impl Into<String>) -> Self {
        Self::Plain(value.into())
    }

    fn rendered_line_count(&self, width: u16) -> usize {
        match self {
            Self::Plain(value) => rendered_line_count(value, width),
            Self::Lines(lines) => lines.len().max(1),
        }
    }

    #[cfg(test)]
    fn as_plain(&self) -> Option<&str> {
        match self {
            Self::Plain(value) => Some(value.as_str()),
            Self::Lines(_) => None,
        }
    }

    #[cfg(test)]
    fn as_lines(&self) -> Option<&[Line<'static>]> {
        match self {
            Self::Plain(_) => None,
            Self::Lines(lines) => Some(lines.as_slice()),
        }
    }
}

impl InteractiveApp {
    fn new(path: &Path) -> Result<Self, String> {
        let current_dir = fs::canonicalize(path)
            .map_err(|error| format!("failed to read directory {}: {error}", path.display()))?;
        if !current_dir.is_dir() {
            return Err(format!(
                "interactive path is not a directory: {}",
                current_dir.display()
            ));
        }

        let entries = interactive_entries(&current_dir)?;
        let (preview_sender, preview_receiver) = mpsc::channel();
        let mut app = Self {
            current_dir,
            entries,
            selected: 0,
            preview: InteractivePreviewContent::plain(""),
            preview_scroll: 0,
            preview_viewport_width: 0,
            preview_viewport_height: 0,
            focus: InteractiveFocus::List,
            preview_sender,
            preview_receiver,
            preview_request_id: 0,
            preview_loading: false,
            preview_loading_visible: false,
            preview_loading_deadline: None,
            preview_spinner: SpinnerPreset::random(),
        };
        app.request_preview_for_selection();
        Ok(app)
    }

    fn handle_event(&mut self, event: Event) -> Result<bool, String> {
        let Event::Key(key) = event else {
            return Ok(false);
        };

        if key.kind != KeyEventKind::Press {
            return Ok(false);
        }

        match key {
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('q'),
                ..
            } => Ok(true),
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                if self.focus == InteractiveFocus::Preview {
                    self.focus = InteractiveFocus::List;
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            _ => self.handle_focused_key(key),
        }
    }

    fn handle_focused_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        match self.focus {
            InteractiveFocus::List => self.handle_list_key(key),
            InteractiveFocus::Preview => Ok(self.handle_preview_key(key)),
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> Result<bool, String> {
        match key {
            KeyEvent {
                code: KeyCode::Up, ..
            } => {
                self.select_previous();
                Ok(false)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => {
                self.select_next();
                Ok(false)
            }
            KeyEvent {
                code: KeyCode::Right,
                ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                ..
            } => {
                self.focus_preview_if_file_or_open_selected()?;
                Ok(false)
            }
            KeyEvent {
                code: KeyCode::Left,
                ..
            }
            | KeyEvent {
                code: KeyCode::Backspace,
                ..
            } => {
                self.open_parent()?;
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> bool {
        match key {
            KeyEvent {
                code: KeyCode::Up, ..
            } => {
                self.scroll_preview_by(-1);
                false
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => {
                self.scroll_preview_by(1);
                false
            }
            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } => {
                self.scroll_preview_by(-8);
                false
            }
            KeyEvent {
                code: KeyCode::PageDown,
                ..
            } => {
                self.scroll_preview_by(8);
                false
            }
            KeyEvent {
                code: KeyCode::Home,
                ..
            } => {
                self.scroll_preview_to_top();
                false
            }
            KeyEvent {
                code: KeyCode::End, ..
            } => {
                self.scroll_preview_to_bottom();
                false
            }
            KeyEvent {
                code: KeyCode::Left,
                ..
            } => {
                self.focus = InteractiveFocus::List;
                false
            }
            _ => false,
        }
    }

    fn select_previous(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.request_preview_for_selection();
        }
    }

    fn select_next(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
            self.request_preview_for_selection();
        }
    }

    fn open_selected(&mut self) -> Result<(), String> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Ok(());
        };

        match entry.kind {
            InteractiveEntryKind::Parent => self.open_parent(),
            InteractiveEntryKind::Directory => self.open_directory(entry.path.clone()),
            InteractiveEntryKind::File => Ok(()),
        }
    }

    fn focus_preview_if_file_or_open_selected(&mut self) -> Result<(), String> {
        let Some(entry) = self.entries.get(self.selected) else {
            return Ok(());
        };

        match entry.kind {
            InteractiveEntryKind::Parent | InteractiveEntryKind::Directory => self.open_selected(),
            InteractiveEntryKind::File => {
                self.focus = InteractiveFocus::Preview;
                Ok(())
            }
        }
    }

    fn open_parent(&mut self) -> Result<(), String> {
        let Some(parent) = self.current_dir.parent() else {
            return Ok(());
        };
        self.open_directory_at(
            parent.to_path_buf(),
            InteractiveDirectorySelection::ReselectChild,
        )
    }

    fn open_directory(&mut self, directory: PathBuf) -> Result<(), String> {
        self.open_directory_at(directory, InteractiveDirectorySelection::First)
    }

    fn open_directory_at(
        &mut self,
        directory: PathBuf,
        selection: InteractiveDirectorySelection,
    ) -> Result<(), String> {
        let previous_dir = self.current_dir.clone();
        self.current_dir = fs::canonicalize(&directory).map_err(|error| {
            format!("failed to read directory {}: {error}", directory.display())
        })?;
        self.entries = interactive_entries(&self.current_dir)?;
        self.selected = match selection {
            InteractiveDirectorySelection::First => 0,
            InteractiveDirectorySelection::ReselectChild => self
                .entries
                .iter()
                .position(|entry| entry.path == previous_dir)
                .unwrap_or(0),
        };
        self.request_preview_for_selection();
        Ok(())
    }

    fn request_preview_for_selection(&mut self) {
        self.preview_scroll = 0;
        self.focus = InteractiveFocus::List;
        self.preview_request_id = self.preview_request_id.wrapping_add(1);

        let Some(entry) = self.entries.get(self.selected).cloned() else {
            self.preview_loading = false;
            self.preview_loading_visible = false;
            self.preview_loading_deadline = None;
            self.preview =
                InteractivePreviewContent::plain("No supported image files or folders found.");
            return;
        };

        self.preview_loading = true;
        self.preview_loading_visible = false;
        self.preview_loading_deadline = Some(Instant::now() + INTERACTIVE_PREVIEW_TICK);
        let request_id = self.preview_request_id;
        let sender = self.preview_sender.clone();

        thread::spawn(move || {
            let preview = interactive_preview(&entry);
            let _ = sender.send(InteractivePreviewResult {
                request_id,
                kind: entry.kind,
                path: entry.path,
                preview,
            });
        });
    }

    fn drain_preview_results(&mut self) {
        while let Ok(result) = self.preview_receiver.try_recv() {
            self.apply_preview_result(result);
        }
    }

    fn apply_preview_result(&mut self, result: InteractivePreviewResult) {
        if result.request_id != self.preview_request_id
            || !self.selected_entry_matches(result.kind, &result.path)
        {
            return;
        }

        self.preview = result.preview;
        self.preview_loading = false;
        self.preview_loading_visible = false;
        self.preview_loading_deadline = None;
        self.clamp_preview_scroll();
    }

    fn selected_entry_matches(&self, kind: InteractiveEntryKind, path: &Path) -> bool {
        self.entries
            .get(self.selected)
            .is_some_and(|entry| entry.kind == kind && entry.path == path)
    }

    fn tick_preview_spinner(&mut self) {
        if !self.preview_loading {
            return;
        }

        if !self.preview_loading_visible {
            if self
                .preview_loading_deadline
                .is_some_and(|deadline| Instant::now() < deadline)
            {
                return;
            }
            self.preview_loading_visible = true;
        }

        if let Some(entry) = self.entries.get(self.selected) {
            self.preview = loading_preview(entry, self.preview_spinner);
        }
    }

    fn set_preview_viewport(&mut self, width: u16, height: u16) {
        self.preview_viewport_width = width;
        self.preview_viewport_height = height;
        self.clamp_preview_scroll();
    }

    fn scroll_preview_by(&mut self, lines: i16) {
        if lines.is_negative() {
            self.preview_scroll = self.preview_scroll.saturating_sub(lines.unsigned_abs());
        } else {
            self.preview_scroll = self.preview_scroll.saturating_add(lines as u16);
        }
        self.clamp_preview_scroll();
    }

    fn scroll_preview_to_top(&mut self) {
        self.preview_scroll = 0;
    }

    fn scroll_preview_to_bottom(&mut self) {
        self.preview_scroll = self.max_preview_scroll();
    }

    fn clamp_preview_scroll(&mut self) {
        self.preview_scroll = self.preview_scroll.min(self.max_preview_scroll());
    }

    fn max_preview_scroll(&self) -> u16 {
        self.preview
            .rendered_line_count(self.preview_viewport_width)
            .saturating_sub(usize::from(self.preview_viewport_height))
            .min(usize::from(u16::MAX)) as u16
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InteractiveEntry {
    kind: InteractiveEntryKind,
    label: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractiveEntryKind {
    Parent,
    Directory,
    File,
}

fn interactive_entries(directory: &Path) -> Result<Vec<InteractiveEntry>, String> {
    let mut directories = Vec::new();
    let mut files = Vec::new();

    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read directory {}: {error}", directory.display()))?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if path.is_dir() {
            directories.push(InteractiveEntry {
                kind: InteractiveEntryKind::Directory,
                label: format!("{}/", file_name(&path)),
                path,
            });
        } else if path.is_file() && is_supported_image_file(&path) {
            files.push(InteractiveEntry {
                kind: InteractiveEntryKind::File,
                label: file_name(&path),
                path,
            });
        }
    }

    directories.sort_by_key(|entry| entry.label.to_ascii_lowercase());
    files.sort_by_key(|entry| entry.label.to_ascii_lowercase());

    let mut result = Vec::new();
    if let Some(parent) = directory.parent() {
        result.push(InteractiveEntry {
            kind: InteractiveEntryKind::Parent,
            label: "../".to_string(),
            path: parent.to_path_buf(),
        });
    }
    result.extend(directories);
    result.extend(files);
    Ok(result)
}

fn interactive_preview(entry: &InteractiveEntry) -> InteractivePreviewContent {
    match entry.kind {
        InteractiveEntryKind::Parent | InteractiveEntryKind::Directory => {
            render_directory_preview(&entry.path)
        }
        InteractiveEntryKind::File => read_preview(&entry.path),
    }
}

fn read_preview(image: &Path) -> InteractivePreviewContent {
    match read_metadata(image) {
        Ok(metadata) => {
            let output = strip_windows_verbatim_prefixes(&format_read_output(
                image,
                &metadata,
                ReadFormat::Pretty,
            ));
            InteractivePreviewContent::Lines(ansi_to_preview_lines(&output))
        }
        Err(error) => InteractivePreviewContent::plain(format!(
            "Failed to read {}\n\n{error}",
            display_path(image)
        )),
    }
}

fn ansi_to_preview_lines(value: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    let mut chars = value.chars().peekable();

    while let Some(char) = chars.next() {
        if char == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            let mut codes = String::new();
            for code_char in chars.by_ref() {
                if code_char == 'm' {
                    break;
                }
                codes.push(code_char);
            }
            push_preview_span(&mut spans, &mut text, style);
            apply_ansi_style_codes(&codes, &mut style);
        } else if char == '\n' {
            push_preview_span(&mut spans, &mut text, style);
            lines.push(Line::from(std::mem::take(&mut spans)));
        } else {
            text.push(char);
        }
    }

    push_preview_span(&mut spans, &mut text, style);
    lines.push(Line::from(spans));
    lines
}

fn push_preview_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

fn apply_ansi_style_codes(codes: &str, style: &mut Style) {
    let codes = if codes.is_empty() { "0" } else { codes };
    for code in codes.split(';').filter_map(|code| code.parse::<u16>().ok()) {
        match code {
            0 => *style = Style::default(),
            1 => *style = style.add_modifier(Modifier::BOLD),
            22 => *style = style.remove_modifier(Modifier::BOLD),
            33 => *style = style.fg(Color::Yellow),
            39 => *style = Style { fg: None, ..*style },
            94 => *style = style.fg(Color::LightBlue),
            96 => *style = style.fg(Color::LightCyan),
            _ => {}
        }
    }
}

fn render_directory_preview(directory: &Path) -> InteractivePreviewContent {
    match interactive_entries(directory) {
        Ok(entries) => {
            let entries = entries
                .into_iter()
                .filter(|entry| entry.kind != InteractiveEntryKind::Parent)
                .collect::<Vec<_>>();
            let mut lines = vec![Line::default()];
            if entries.is_empty() {
                lines.push(Line::from("<No directories or photos in this directory>"));
            } else {
                for entry in entries {
                    lines.push(Line::from(Span::styled(
                        entry.label,
                        interactive_entry_style(entry.kind),
                    )));
                }
            }
            InteractivePreviewContent::Lines(lines)
        }
        Err(error) => InteractivePreviewContent::plain(format!(
            "Failed to read folder {}\n\n{error}",
            display_path(directory)
        )),
    }
}

fn loading_preview(entry: &InteractiveEntry, spinner: SpinnerPreset) -> InteractivePreviewContent {
    match entry.kind {
        InteractiveEntryKind::Parent | InteractiveEntryKind::Directory => {
            InteractivePreviewContent::plain(format!(
                "{} Loading folder preview\n\n{}",
                spinner.frame(),
                display_path(&entry.path)
            ))
        }
        InteractiveEntryKind::File => {
            InteractivePreviewContent::plain(format!("{} Reading EXIF preview", spinner.frame()))
        }
    }
}

fn display_path(path: &Path) -> String {
    strip_windows_verbatim_prefixes(&path.display().to_string())
}

fn strip_windows_verbatim_prefixes(value: &str) -> String {
    value.replace("\\\\?\\UNC\\", "\\\\").replace("\\\\?\\", "")
}

fn selected_preview_title(app: &InteractiveApp) -> &'static str {
    match app.entries.get(app.selected).map(|entry| entry.kind) {
        Some(InteractiveEntryKind::Parent | InteractiveEntryKind::Directory) => "Directory",
        Some(InteractiveEntryKind::File) | None => "Read",
    }
}

fn interactive_entry_style(kind: InteractiveEntryKind) -> Style {
    match kind {
        InteractiveEntryKind::Parent => Style::default().fg(Color::Yellow),
        InteractiveEntryKind::Directory => Style::default().fg(Color::Cyan),
        InteractiveEntryKind::File => Style::default(),
    }
}

fn render_interactive(frame: &mut ratatui::Frame<'_>, app: &mut InteractiveApp) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(frame.area());

    let items = app
        .entries
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(Span::styled(
                entry.label.clone(),
                interactive_entry_style(entry.kind),
            )))
        })
        .collect::<Vec<_>>();

    let mut list_state = ListState::default();
    if !items.is_empty() {
        list_state.select(Some(app.selected));
    }

    let files = List::new(items)
        .block(
            Block::default()
                .title(format!(" {} ", display_path(&app.current_dir)))
                .borders(Borders::ALL),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(files, chunks[0], &mut list_state);

    let preview_inner_width = chunks[1].width.saturating_sub(2);
    let preview_inner_height = chunks[1].height.saturating_sub(2);
    app.set_preview_viewport(preview_inner_width, preview_inner_height);

    let preview_block_style = match app.focus {
        InteractiveFocus::List => Style::default(),
        InteractiveFocus::Preview => Style::default().fg(Color::Green),
    };
    let preview_text = match &app.preview {
        InteractivePreviewContent::Plain(value) => Text::from(value.as_str()),
        InteractivePreviewContent::Lines(lines) => Text::from(lines.clone()),
    };
    let preview = Paragraph::new(preview_text)
        .block(
            Block::default()
                .title(format!(" {} ", selected_preview_title(app)))
                .borders(Borders::ALL)
                .border_style(preview_block_style),
        )
        .scroll((app.preview_scroll, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(preview, chunks[1]);
}

fn rendered_line_count(value: &str, width: u16) -> usize {
    let width = usize::from(width);
    if width == 0 {
        return value.lines().count().max(1);
    }

    value
        .split('\n')
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum::<usize>()
        .max(1)
}

fn read_metadata(image: &Path) -> Result<ReadMetadata, String> {
    let file = File::open(image)
        .map_err(|error| format!("failed to open {}: {error}", image.display()))?;
    let mut reader = BufReader::new(file);
    let mut warnings = Vec::new();

    let exif = Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut reader)
        .or_else(|error| match error {
            ExifError::NotFound(_) => Ok(empty_exif()),
            error => error.distill_partial_result(|errors| {
                warnings.extend(errors.into_iter().map(|error| error.to_string()));
            }),
        })
        .map_err(|error| {
            format!(
                "failed to read EXIF metadata from {}: {error}",
                image.display()
            )
        })?;

    let file_info = ReadFileInfo::from_path(image, &exif)?;

    Ok(ReadMetadata {
        exif,
        warnings,
        file_info,
    })
}

fn empty_exif() -> Exif {
    Reader::new()
        .read_raw(vec![
            0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ])
        .expect("embedded empty EXIF should parse")
}

struct ReadMetadata {
    exif: Exif,
    warnings: Vec<String>,
    file_info: ReadFileInfo,
}

struct ReadFileInfo {
    rows: Vec<ReadInfoRow>,
}

impl ReadFileInfo {
    fn from_path(image: &Path, exif: &Exif) -> Result<Self, String> {
        let metadata = fs::metadata(image).map_err(|error| {
            format!(
                "failed to read file metadata for {}: {error}",
                image.display()
            )
        })?;
        let file_kind = detect_file_kind(image);
        let mut rows = Vec::new();

        rows.push(ReadInfoRow::new("File Name", file_name(image)));
        rows.push(ReadInfoRow::new("Directory", directory_name(image)));
        rows.push(ReadInfoRow::new(
            "File Size",
            format_file_size(metadata.len()),
        ));

        if let Ok(modified) = metadata.modified() {
            rows.push(ReadInfoRow::new(
                "File Modification Date/Time",
                format_system_time(modified),
            ));
        }

        if let Ok(accessed) = metadata.accessed() {
            rows.push(ReadInfoRow::new(
                "File Access Date/Time",
                format_system_time(accessed),
            ));
        }

        if let Ok(created) = metadata.created() {
            rows.push(ReadInfoRow::new(
                "File Creation Date/Time",
                format_system_time(created),
            ));
        }

        rows.push(ReadInfoRow::new(
            "File Permissions",
            format_permissions(&metadata),
        ));
        rows.push(ReadInfoRow::new("File Type", file_kind.file_type));
        rows.push(ReadInfoRow::new("File Type Extension", file_kind.extension));
        rows.push(ReadInfoRow::new("MIME Type", file_kind.mime_type));
        rows.push(ReadInfoRow::new(
            "Exif Byte Order",
            format_exif_byte_order(exif),
        ));

        Ok(Self { rows })
    }

    #[cfg(test)]
    fn empty() -> Self {
        Self { rows: Vec::new() }
    }
}

struct ReadInfoRow {
    name: String,
    value: String,
}

impl ReadInfoRow {
    fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

fn check_image_path(image: &Path) -> Result<(), String> {
    if !image.exists() {
        return Err(format!("image does not exist: {}", image.display()));
    }

    if !image.is_file() {
        return Err(format!("image path is not a file: {}", image.display()));
    }

    Ok(())
}

fn format_read_output(_image: &Path, metadata: &ReadMetadata, format: ReadFormat) -> String {
    let mut output = String::new();
    let mut warnings = metadata.warnings.clone();
    let custom_tags = custom_tags_from_exif(&metadata.exif);
    let mut rows = metadata
        .exif
        .fields()
        .filter(|field| {
            matches!(format, ReadFormat::Raw) || !is_exifmeta_custom_payload_field(field)
        })
        .map(|field| ReadRow::from_field(field, &metadata.exif, format))
        .collect::<Vec<_>>();

    if rows.is_empty() && custom_tags.is_empty() {
        output.push_str(&format_empty_exif_message(format));
    } else {
        sort_read_rows(&mut rows);

        match format {
            ReadFormat::Pretty => append_pretty_read_rows(
                &mut output,
                &metadata.file_info.rows,
                &rows,
                &custom_tags,
                &metadata.exif,
                &mut warnings,
            ),
            ReadFormat::Raw => append_raw_read_rows(&mut output, &rows),
        }

        output = output.trim_end().to_string();
    }

    if !warnings.is_empty() {
        output.push_str("\n\nWarnings:\n");
        for warning in &warnings {
            output.push_str(&format!("warning: {warning}\n"));
        }
        output = output.trim_end().to_string();
    }

    output
}

fn format_empty_exif_message(format: ReadFormat) -> String {
    match format {
        ReadFormat::Pretty => "<No EXIF metadata found>".yellow().to_string(),
        ReadFormat::Raw => "<No EXIF metadata found>".to_string(),
    }
}

fn sort_read_rows(rows: &mut [ReadRow]) {
    rows.sort_by(|left, right| {
        left.is_unknown
            .cmp(&right.is_unknown)
            .then(left.ifd.cmp(&right.ifd))
            .then(left.context.cmp(&right.context))
            .then(left.tag_id.cmp(&right.tag_id))
            .then(left.name.cmp(&right.name))
    });
}

fn append_raw_read_rows(output: &mut String, rows: &[ReadRow]) {
    let context_width = rows.iter().map(|row| row.context.len()).max().unwrap_or(0);
    let name_width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);

    for row in rows {
        output.push_str(&format!(
            "IFD {}  {:<context_width$}  0x{:04X}  {:<name_width$}  {}\n",
            row.ifd, row.context, row.tag_id, row.name, row.value
        ));
    }
}

fn append_pretty_read_rows(
    output: &mut String,
    info_rows: &[ReadInfoRow],
    rows: &[ReadRow],
    custom_tags: &[CustomTag],
    exif: &Exif,
    warnings: &mut Vec<String>,
) {
    let mut pretty_rows = pretty_read_rows(info_rows, rows, custom_tags);
    append_nearest_location_rows(&mut pretty_rows, exif, warnings);
    pretty_rows.sort_by(|left, right| {
        left.group
            .output_order()
            .cmp(&right.group.output_order())
            .then_with(|| pretty_read_row_sort_label(left).cmp(&pretty_read_row_sort_label(right)))
            .then(left.value.cmp(&right.value))
    });

    let mut first_group = true;
    for group in PrettyReadGroup::OUTPUT_ORDER {
        let group_rows = pretty_rows
            .iter()
            .filter(|row| row.group == group)
            .collect::<Vec<_>>();

        if group_rows.is_empty() {
            continue;
        }

        if !first_group {
            output.push('\n');
        }
        first_group = false;

        append_check_heading(output, group.label());

        let name_width = group_rows
            .iter()
            .map(|row| row.label.len())
            .max()
            .unwrap_or(0);
        for row in group_rows {
            output.push_str(&row.styled_label());
            output.push_str(&" ".repeat(name_width - row.label.len()));
            output.push_str(&format!("  {}\n", row.value));
        }
    }
}

fn pretty_read_row_sort_label(row: &PrettyReadRow) -> String {
    if let Some(suffix) = row.label.strip_prefix("GPS Nearest Location ") {
        format!("GPS Nearest 0 Location {suffix}")
    } else if row.label == "GPS Nearest City" {
        "GPS Nearest 1 City".to_string()
    } else {
        row.label.clone()
    }
}

fn pretty_read_rows(
    info_rows: &[ReadInfoRow],
    rows: &[ReadRow],
    custom_tags: &[CustomTag],
) -> Vec<PrettyReadRow> {
    let mut pretty_rows = info_rows
        .iter()
        .map(|row| PrettyReadRow {
            group: classify_info_row(row),
            label: row.name.clone(),
            label_color: None,
            value: row.value.clone(),
        })
        .collect::<Vec<_>>();

    let mut seen_ifd_0_1 = HashSet::new();
    let iso_speed_values = rows
        .iter()
        .filter(|row| !is_pretty_omitted_row(row) && row.name == "ISOSpeed")
        .map(pretty_read_value)
        .collect::<HashSet<_>>();
    for row in rows {
        if is_pretty_omitted_row(row) {
            continue;
        }

        let value = pretty_read_value(row);
        if is_duplicate_photographic_sensitivity(row, &value, &iso_speed_values) {
            continue;
        }

        if matches!(row.ifd, 0 | 1)
            && !seen_ifd_0_1.insert((row.name.clone(), row.context.clone(), value.clone()))
        {
            continue;
        }

        pretty_rows.push(PrettyReadRow {
            group: classify_exif_row(row),
            label: row.pretty_name.clone(),
            label_color: None,
            value,
        });
    }

    for tag in custom_tags {
        pretty_rows.push(PrettyReadRow {
            group: PrettyReadGroup::Custom,
            label: title_case_tag_name(&tag.name),
            label_color: None,
            value: custom_tag_value_label(&tag.value),
        });
    }

    pretty_rows
}

fn is_duplicate_photographic_sensitivity(
    row: &ReadRow,
    value: &str,
    iso_speed_values: &HashSet<String>,
) -> bool {
    row.name == "PhotographicSensitivity" && iso_speed_values.contains(value)
}

fn is_pretty_omitted_row(row: &ReadRow) -> bool {
    if row.is_unknown {
        return true;
    }

    matches!(
        row.name.as_str(),
        "GPSLatitudeRef" | "GPSLongitudeRef" | "StripByteCounts" | "StripOffsets"
    )
}

fn append_nearest_location_rows(
    pretty_rows: &mut Vec<PrettyReadRow>,
    exif: &Exif,
    warnings: &mut Vec<String>,
) {
    let Some((latitude, longitude)) = gps_coordinates(exif) else {
        return;
    };

    let nearest_location_rows =
        match nearest_locations(latitude, longitude, NEAREST_LOCATION_LIMIT, None) {
            Ok(locations) => locations,
            Err(error) => {
                warnings.push(format!("failed to query nearest locations: {error}"));
                Vec::new()
            }
        };
    append_location_rows(pretty_rows, "GPS Nearest Location", &nearest_location_rows);

    match nearest_locations(
        latitude,
        longitude,
        1,
        Some(NEAREST_CITY_MINIMUM_POPULATION),
    ) {
        Ok(locations) => append_non_duplicate_location_rows(
            pretty_rows,
            "GPS Nearest City",
            locations,
            &nearest_location_rows,
        ),
        Err(error) => warnings.push(format!("failed to query nearest city: {error}")),
    }
}

fn append_location_rows(
    pretty_rows: &mut Vec<PrettyReadRow>,
    label_prefix: &str,
    locations: &[GeoLocation],
) {
    for (index, location) in locations.iter().enumerate() {
        pretty_rows.push(PrettyReadRow {
            group: PrettyReadGroup::Gps,
            label: format!("{} {}", label_prefix, index + 1),
            label_color: Some(PrettyLabelColor::Green),
            value: format!(
                "({}) {}, {}",
                format_distance(location.distance_km),
                location.name,
                location.country_code
            ),
        });
    }
}

fn append_non_duplicate_location_rows(
    pretty_rows: &mut Vec<PrettyReadRow>,
    label: &str,
    locations: Vec<GeoLocation>,
    existing_locations: &[GeoLocation],
) {
    for location in locations {
        if existing_locations
            .iter()
            .all(|existing| !is_same_geo_location(existing, &location))
        {
            append_location_row(pretty_rows, label, &location);
        }
    }
}

fn append_location_row(pretty_rows: &mut Vec<PrettyReadRow>, label: &str, location: &GeoLocation) {
    pretty_rows.push(PrettyReadRow {
        group: PrettyReadGroup::Gps,
        label: label.to_string(),
        label_color: Some(PrettyLabelColor::Green),
        value: format!(
            "({}) {}, {}",
            format_distance(location.distance_km),
            location.name,
            location.country_code
        ),
    });
}

fn is_same_geo_location(left: &GeoLocation, right: &GeoLocation) -> bool {
    left.name == right.name
        && left.country_code == right.country_code
        && left.latitude.to_bits() == right.latitude.to_bits()
        && left.longitude.to_bits() == right.longitude.to_bits()
        && left.population == right.population
}

fn gps_coordinates(exif: &Exif) -> Option<(f64, f64)> {
    let latitude_field = exif.fields().find(|field| field.tag == Tag::GPSLatitude)?;
    let longitude_field = exif.fields().find(|field| field.tag == Tag::GPSLongitude)?;

    let latitude = decimal_gps_coordinate(latitude_field, exif)?;
    let longitude = decimal_gps_coordinate(longitude_field, exif)?;

    if latitude.is_finite() && longitude.is_finite() {
        Some((latitude, longitude))
    } else {
        None
    }
}

struct PrettyReadRow {
    group: PrettyReadGroup,
    label: String,
    label_color: Option<PrettyLabelColor>,
    value: String,
}

#[derive(Clone, Copy)]
enum PrettyLabelColor {
    Green,
}

impl PrettyReadRow {
    fn styled_label(&self) -> String {
        match self.label_color {
            Some(PrettyLabelColor::Green) => self.label.green().to_string(),
            None => self.label.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrettyReadGroup {
    File,
    Camera,
    Film,
    Custom,
    Exposure,
    Gps,
    Misc,
}

impl PrettyReadGroup {
    const OUTPUT_ORDER: [Self; 7] = [
        Self::File,
        Self::Camera,
        Self::Film,
        Self::Exposure,
        Self::Gps,
        Self::Custom,
        Self::Misc,
    ];

    fn output_order(self) -> usize {
        match self {
            Self::File => 0,
            Self::Camera => 1,
            Self::Film => 2,
            Self::Exposure => 3,
            Self::Gps => 4,
            Self::Custom => 5,
            Self::Misc => 6,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Camera => "camera",
            Self::Film => "film",
            Self::Custom => "custom",
            Self::Exposure => "exposure",
            Self::Gps => "gps",
            Self::Misc => "misc",
        }
    }
}

fn classify_info_row(row: &ReadInfoRow) -> PrettyReadGroup {
    if normalized_label_matches(
        &row.name,
        &[
            "filename",
            "directory",
            "filesize",
            "filemodificationdate/time",
            "fileaccessdate/time",
            "filecreationdate/time",
            "filepermissions",
            "filetype",
            "filetypeextension",
            "mimetype",
        ],
    ) {
        PrettyReadGroup::File
    } else {
        PrettyReadGroup::Misc
    }
}

fn classify_exif_row(row: &ReadRow) -> PrettyReadGroup {
    if row.context == "Gps"
        || normalized_label_starts_with(&row.name, &["gps"])
        || normalized_label_starts_with(&row.pretty_name, &["gps"])
    {
        return PrettyReadGroup::Gps;
    }

    if is_film_label(&row.name) || is_film_label(&row.pretty_name) {
        return PrettyReadGroup::Film;
    }

    if is_exposure_label(&row.name) || is_exposure_label(&row.pretty_name) {
        return PrettyReadGroup::Exposure;
    }

    if is_camera_label(&row.name) || is_camera_label(&row.pretty_name) {
        return PrettyReadGroup::Camera;
    }

    if is_file_label(&row.name) || is_file_label(&row.pretty_name) {
        return PrettyReadGroup::File;
    }

    PrettyReadGroup::Misc
}

fn is_file_label(label: &str) -> bool {
    let label = normalized_label(label);
    label != "filesource" && label.starts_with("file")
}

fn is_camera_label(label: &str) -> bool {
    normalized_label_matches(
        label,
        &[
            "make",
            "model",
            "filesource",
            "focallength",
            "focallengthin35mmfilm",
            "maxaperturevalue",
            "lensmake",
            "lensmodel",
        ],
    ) || normalized_label_starts_with(label, &["camera", "lens"])
}

fn is_film_label(label: &str) -> bool {
    normalized_label_matches(
        label,
        &[
            "filmroll",
            "filmmaker",
            "filmname",
            "filmformat",
            "filmcolor",
            "filmnegative",
            "filmdevelopprocess",
            "filmdeveloper",
            "filmprocesslab",
            "filmprocessdate",
            "filmscanner",
        ],
    ) || normalized_label_starts_with(label, &["analoguedata"])
}

fn is_exposure_label(label: &str) -> bool {
    normalized_label_matches(
        label,
        &[
            "exposuretime",
            "fnumber",
            "isospeedratings",
            "iso",
            "isospeed",
            "shutterspeedvalue",
            "aperturevalue",
            "brightnessvalue",
            "exposurebiasvalue",
            "exposuremode",
            "exposureprogram",
            "maxaperturevalue",
            "meteringmode",
            "photographicsensitivity",
            "sensitivitytype",
            "lightsource",
            "flash",
        ],
    )
}

fn normalized_label(label: &str) -> String {
    label
        .chars()
        .filter(|char| !char.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalized_label_matches(label: &str, tags: &[&str]) -> bool {
    let label = normalized_label(label);
    tags.contains(&label.as_str())
}

fn normalized_label_starts_with(label: &str, prefixes: &[&str]) -> bool {
    let label = normalized_label(label);
    prefixes.iter().any(|prefix| label.starts_with(prefix))
}

fn pretty_read_value(row: &ReadRow) -> String {
    if row.is_unknown && row.value.chars().count() > PRETTY_UNKNOWN_VALUE_DISPLAY_LIMIT {
        return pretty_unknown_value_omitted_message();
    }
    if !row.is_unknown && row.value.chars().count() > PRETTY_KNOWN_VALUE_DISPLAY_LIMIT {
        return pretty_unknown_value_omitted_message();
    }

    if row.name == "ExposureTime" {
        return pretty_exposure_time(&row.value).unwrap_or_else(|| row.value.clone());
    }

    if !row.is_unknown {
        if let Some(value) = row
            .value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            return value.to_string();
        }
    }

    row.value.clone()
}

fn pretty_unknown_value_omitted_message() -> String {
    format!(
        "{}{}",
        PRETTY_UNKNOWN_VALUE_OMITTED_LABEL.yellow(),
        PRETTY_UNKNOWN_VALUE_OMITTED_HINT
    )
}

fn pretty_exposure_time(value: &str) -> Option<String> {
    let denominator = value.strip_prefix("1/")?;
    let denominator = denominator.strip_suffix(" s").unwrap_or(denominator);
    let denominator = denominator.parse::<f64>().ok()?;

    if !denominator.is_finite() {
        return None;
    }

    Some(format!("1/{:.0}", denominator.round()))
}

struct FileKind {
    file_type: &'static str,
    extension: String,
    mime_type: &'static str,
}

fn detect_file_kind(image: &Path) -> FileKind {
    let signature = read_file_signature(image);
    let detected = signature.as_deref().and_then(file_kind_from_signature);
    let fallback = file_kind_from_extension(image);

    let (file_type, default_extension, mime_type) =
        detected
            .or(fallback)
            .unwrap_or(("Unknown", "", "application/octet-stream"));

    FileKind {
        file_type,
        extension: if default_extension.is_empty() {
            file_extension(image).unwrap_or_default()
        } else {
            default_extension.to_string()
        },
        mime_type,
    }
}

fn read_file_signature(image: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(image).ok()?;
    let mut buffer = vec![0; 32];
    let length = file.read(&mut buffer).ok()?;
    buffer.truncate(length);
    Some(buffer)
}

fn file_kind_from_signature(
    signature: &[u8],
) -> Option<(&'static str, &'static str, &'static str)> {
    if signature.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(("JPEG", "jpg", "image/jpeg"));
    }

    if signature.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("PNG", "png", "image/png"));
    }

    if signature.starts_with(b"II*\0") || signature.starts_with(b"MM\0*") {
        return Some(("TIFF", "tif", "image/tiff"));
    }

    if signature.len() >= 12 && signature.starts_with(b"RIFF") && &signature[8..12] == b"WEBP" {
        return Some(("WEBP", "webp", "image/webp"));
    }

    if signature.starts_with(&[0xff, 0x0a]) || signature.starts_with(b"\0\0\0\x0cJXL ") {
        return Some(("JXL", "jxl", "image/jxl"));
    }

    if signature.len() >= 12 && &signature[4..8] == b"ftyp" {
        let brand = &signature[8..12];
        return match brand {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" => {
                Some(("HEIC", "heic", "image/heic"))
            }
            b"mif1" | b"msf1" => Some(("HEIF", "heif", "image/heif")),
            b"avif" | b"avis" => Some(("AVIF", "avif", "image/avif")),
            _ => None,
        };
    }

    None
}

fn file_kind_from_extension(image: &Path) -> Option<(&'static str, &'static str, &'static str)> {
    match file_extension(image)?.as_str() {
        "jpg" | "jpeg" => Some(("JPEG", "jpg", "image/jpeg")),
        "png" => Some(("PNG", "png", "image/png")),
        "tif" | "tiff" => Some(("TIFF", "tif", "image/tiff")),
        "webp" => Some(("WEBP", "webp", "image/webp")),
        "jxl" => Some(("JXL", "jxl", "image/jxl")),
        "heif" | "hif" => Some(("HEIF", "heif", "image/heif")),
        "heic" => Some(("HEIC", "heic", "image/heic")),
        "avif" => Some(("AVIF", "avif", "image/avif")),
        _ => None,
    }
}

fn file_extension(image: &Path) -> Option<String> {
    image
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn file_name(image: &Path) -> String {
    image
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| image.display().to_string(), ToString::to_string)
}

fn directory_name(image: &Path) -> String {
    let directory = image.parent().map(|parent| parent.display().to_string());

    match directory.as_deref() {
        Some("") | None => ".".to_string(),
        Some(directory) => directory.to_string(),
    }
}

fn format_file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];

    if bytes < 1000 {
        return format!("{bytes} bytes");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        let formatted = format!("{value:.1}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();
        format!("{formatted} {}", UNITS[unit])
    }
}

fn format_system_time(time: SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();

    datetime.format("%Y:%m:%d %H:%M:%S%:z").to_string()
}

#[cfg(unix)]
fn format_permissions(metadata: &Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode();
    let mut output = String::with_capacity(10);
    output.push(if metadata.is_dir() { 'd' } else { '-' });

    for shift in [6, 3, 0] {
        output.push(if mode & (0o4 << shift) != 0 { 'r' } else { '-' });
        output.push(if mode & (0o2 << shift) != 0 { 'w' } else { '-' });
        output.push(if mode & (0o1 << shift) != 0 { 'x' } else { '-' });
    }

    output
}

#[cfg(not(unix))]
fn format_permissions(metadata: &Metadata) -> String {
    if metadata.permissions().readonly() {
        "read-only".to_string()
    } else {
        "read-write".to_string()
    }
}

fn format_exif_byte_order(exif: &Exif) -> &'static str {
    if exif.little_endian() {
        "Little-endian (Intel, II)"
    } else {
        "Big-endian (Motorola, MM)"
    }
}

struct ReadRow {
    is_unknown: bool,
    ifd: usize,
    context: String,
    tag_id: u16,
    name: String,
    pretty_name: String,
    value: String,
}

impl ReadRow {
    fn from_field(field: &Field, exif: &Exif, format: ReadFormat) -> Self {
        let is_unknown =
            field.tag.description().is_none() || matches!(field.value, Value::Unknown(..));
        let name = if is_unknown {
            format!(
                "Tag({:?}, 0x{:04X})",
                field.tag.context(),
                field.tag.number()
            )
        } else {
            field.tag.to_string()
        };
        let pretty_name = if is_unknown {
            format!("Unknown {} Tag", format!("{:?}", field.tag.context()))
        } else {
            title_case_tag_name(&name)
        };
        let mut value = if is_unknown {
            format_unknown_field_value(&field.value, format)
        } else {
            format_known_field_value(field, exif, &name)
        };

        if !is_unknown {
            if let Some(decimal) = decimal_gps_coordinate(field, exif) {
                value = format!("({}) {value}", format_decimal_coordinate(decimal));
            }
        }

        Self {
            is_unknown,
            ifd: usize::from(field.ifd_num.index()),
            context: format!("{:?}", field.tag.context()),
            tag_id: field.tag.number(),
            name,
            pretty_name,
            value,
        }
    }
}

fn format_unknown_field_value(value: &Value, format: ReadFormat) -> String {
    if matches!(format, ReadFormat::Pretty)
        && unknown_value_payload_len(value)
            .is_some_and(|length| length > PRETTY_UNKNOWN_VALUE_DISPLAY_LIMIT)
    {
        pretty_unknown_value_omitted_message()
    } else {
        format!("{value:?}")
    }
}

fn unknown_value_payload_len(value: &Value) -> Option<usize> {
    match value {
        Value::Byte(values) => Some(values.len()),
        Value::Ascii(values) => Some(values.iter().map(Vec::len).sum()),
        Value::Short(values) => Some(values.len()),
        Value::Long(values) => Some(values.len()),
        Value::Rational(values) => Some(values.len()),
        Value::SByte(values) => Some(values.len()),
        Value::Undefined(values, _) => Some(values.len()),
        Value::SShort(values) => Some(values.len()),
        Value::SLong(values) => Some(values.len()),
        Value::SRational(values) => Some(values.len()),
        Value::Float(values) => Some(values.len()),
        Value::Double(values) => Some(values.len()),
        Value::Unknown(_, count, _) => usize::try_from(*count).ok(),
    }
}

fn format_known_field_value(field: &Field, exif: &Exif, name: &str) -> String {
    if name == "UserComment" {
        if let Some(value) = visible_user_comment_text(field) {
            return value;
        }
    }

    let value = field.display_value().with_unit(exif).to_string();

    if name == "ExposureTime" {
        return value
            .strip_suffix(" s")
            .map_or(value.clone(), ToString::to_string);
    }

    value
}

fn visible_user_comment_text(field: &Field) -> Option<String> {
    let bytes = user_comment_bytes(field)?;
    let body = bytes
        .strip_prefix(USER_COMMENT_ASCII_PREFIX)
        .unwrap_or(bytes);
    std::str::from_utf8(body).ok().map(ToString::to_string)
}

fn title_case_tag_name(name: &str) -> String {
    let spaced = decamelcase_tag_name(name);

    spaced
        .split_whitespace()
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn decamelcase_tag_name(name: &str) -> String {
    let mut output = String::new();
    let mut chars = name.chars().peekable();
    let mut previous: Option<char> = None;

    while let Some(current) = chars.next() {
        let next = chars.peek().copied();
        let needs_space = previous.is_some_and(|previous| {
            (previous.is_ascii_lowercase() && current.is_ascii_uppercase())
                || (previous.is_ascii_alphabetic() && current.is_ascii_digit())
                || (previous.is_ascii_uppercase()
                    && current.is_ascii_uppercase()
                    && next.is_some_and(|next| next.is_ascii_lowercase()))
        });

        if needs_space {
            output.push(' ');
        }

        output.push(current);
        previous = Some(current);
    }

    output
}

fn title_case_word(word: &str) -> String {
    if word
        .chars()
        .all(|char| char.is_ascii_uppercase() || char.is_ascii_digit())
    {
        return word.to_string();
    }

    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };

    let mut output = String::new();
    output.push(first.to_ascii_uppercase());
    output.extend(chars.map(|char| char.to_ascii_lowercase()));
    output
}

fn decimal_gps_coordinate(field: &Field, exif: &Exif) -> Option<f64> {
    let reference_tag = match field.tag {
        Tag::GPSLatitude => Tag::GPSLatitudeRef,
        Tag::GPSLongitude => Tag::GPSLongitudeRef,
        _ => return None,
    };

    let values = match field.value {
        Value::Rational(ref values) => values,
        _ => return None,
    };
    let [degrees, minutes, seconds] = values.get(..3)? else {
        return None;
    };

    let decimal = degrees.to_f64() + minutes.to_f64() / 60.0 + seconds.to_f64() / 3600.0;
    if !decimal.is_finite() {
        return None;
    }

    let sign = exif
        .get_field(reference_tag, field.ifd_num)
        .and_then(gps_reference)
        .map_or(1.0, |reference| match reference {
            "S" | "W" => -1.0,
            _ => 1.0,
        });

    Some(decimal * sign)
}

fn gps_reference(field: &Field) -> Option<&str> {
    let Value::Ascii(ref values) = field.value else {
        return None;
    };
    let bytes = values.first()?;

    std::str::from_utf8(bytes).ok()
}

fn format_decimal_coordinate(value: f64) -> String {
    let formatted = format!("{value:.6}");

    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

#[derive(Debug, Clone, PartialEq)]
struct CustomTag {
    name: String,
    value: YamlValue,
}

fn custom_tags_from_exif(exif: &Exif) -> Vec<CustomTag> {
    exif.fields()
        .filter(|field| field.tag == Tag::UserComment)
        .find_map(custom_tags_from_field)
        .unwrap_or_default()
}

fn custom_tags_from_field(field: &Field) -> Option<Vec<CustomTag>> {
    custom_tags_from_bytes(user_comment_bytes(field)?)
}

fn user_comment_bytes(field: &Field) -> Option<&[u8]> {
    match &field.value {
        Value::Undefined(bytes, _) => Some(bytes.as_slice()),
        Value::Ascii(values) => values.first().map(Vec::as_slice),
        _ => None,
    }
}

fn is_exifmeta_custom_payload_field(field: &Field) -> bool {
    field.tag == Tag::UserComment
        && user_comment_bytes(field).is_some_and(|bytes| custom_tags_from_bytes(bytes).is_some())
}

fn custom_tags_from_bytes(bytes: &[u8]) -> Option<Vec<CustomTag>> {
    let body = if let Some(body) = bytes.strip_prefix(LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX) {
        return custom_tags_from_yaml_bytes(body);
    } else if let Some(body) = bytes.strip_prefix(USER_COMMENT_ASCII_PREFIX) {
        body
    } else {
        bytes
    };

    custom_tags_from_json_bytes(body)
}

fn custom_tag_json_body(bytes: &[u8]) -> &[u8] {
    const MARKER_PREFIX: &[u8] = b"exifmeta-v";

    if let Some(rest) = bytes.strip_prefix(MARKER_PREFIX) {
        if let Some(marker_end) = rest.iter().position(|byte| *byte == b'\n') {
            return &rest[(marker_end + 1)..];
        }
    }

    bytes
}

fn custom_tags_from_yaml_bytes(bytes: &[u8]) -> Option<Vec<CustomTag>> {
    let mapping = serde_yaml::from_slice::<Mapping>(bytes).ok()?;
    custom_tags_from_mapping(mapping)
}

fn custom_tags_from_json_bytes(bytes: &[u8]) -> Option<Vec<CustomTag>> {
    let mapping = serde_json::from_slice::<Mapping>(custom_tag_json_body(bytes)).ok()?;
    custom_tags_from_mapping(mapping)
}

fn custom_tags_from_mapping(mapping: Mapping) -> Option<Vec<CustomTag>> {
    let mut tags = Vec::new();

    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            continue;
        };
        tags.push(CustomTag {
            name: name.to_string(),
            value,
        });
    }

    if tags.is_empty() { None } else { Some(tags) }
}

fn encode_custom_tags(tags: &[CustomTag]) -> Result<Vec<u8>, String> {
    let mut mapping = Mapping::new();
    for tag in tags {
        mapping.insert(YamlValue::String(tag.name.clone()), tag.value.clone());
    }

    let mut bytes = USER_COMMENT_ASCII_PREFIX.to_vec();
    bytes.extend_from_slice(CUSTOM_TAG_PAYLOAD_MARKER.as_bytes());
    let body = serde_json::to_string(&mapping)
        .map_err(|error| format!("failed to encode custom tags: {error}"))?;
    bytes.extend_from_slice(body.as_bytes());
    Ok(bytes)
}

fn custom_tag_value_label(value: &YamlValue) -> String {
    match value {
        YamlValue::Null => "<null>".to_string(),
        YamlValue::Bool(value) => value.to_string(),
        YamlValue::Number(value) => value.to_string(),
        YamlValue::String(value) => value.clone(),
        YamlValue::Sequence(_) | YamlValue::Mapping(_) => serde_yaml::to_string(value)
            .map(|value| value.trim().replace('\n', " "))
            .unwrap_or_else(|_| format!("{value:?}")),
        YamlValue::Tagged(_) => format!("{value:?}"),
    }
}

struct GeoLocation {
    name: String,
    country_code: String,
    latitude: f64,
    longitude: f64,
    population: i64,
    #[allow(dead_code)]
    elevation: Option<i64>,
    distance_km: f64,
}

fn nearest_locations(
    latitude: f64,
    longitude: f64,
    limit: usize,
    minimum_population: Option<i64>,
) -> Result<Vec<GeoLocation>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let connection = open_embedded_geonames_database()?;
    let mut radius_km = 25.0;
    let mut candidates = Vec::new();

    while radius_km <= 20_000.0 {
        candidates = candidate_locations(
            &connection,
            latitude,
            longitude,
            radius_km,
            minimum_population,
        )?;

        for location in &mut candidates {
            location.distance_km =
                haversine_distance_km(latitude, longitude, location.latitude, location.longitude);
        }
        sort_locations_by_distance(&mut candidates);

        if candidates.len() >= limit && candidates[limit - 1].distance_km <= radius_km {
            break;
        }
        radius_km *= 2.0;
    }

    sort_locations_by_distance(&mut candidates);
    candidates.truncate(limit);

    Ok(candidates)
}

fn sort_locations_by_distance(locations: &mut [GeoLocation]) {
    locations.sort_by(|left, right| {
        left.distance_km
            .total_cmp(&right.distance_km)
            .then(left.country_code.cmp(&right.country_code))
            .then(left.name.cmp(&right.name))
            .then(left.population.cmp(&right.population))
    });
}

fn locations_by_name(connection: &Connection, name: &str) -> Result<Vec<GeoLocation>, String> {
    let mut statement = connection
        .prepare(
            "
        SELECT name, country_code, latitude, longitude, population, elevation
        FROM locations
        WHERE name = ?1 COLLATE NOCASE
        ORDER BY population DESC, country_code ASC, name ASC
        ",
        )
        .map_err(|error| format!("failed to prepare GeoNames location lookup: {error}"))?;

    let rows = statement
        .query_map(params![name], |row| {
            Ok(GeoLocation {
                name: row.get(0)?,
                country_code: row.get(1)?,
                latitude: row.get(2)?,
                longitude: row.get(3)?,
                population: row.get(4)?,
                elevation: row.get(5)?,
                distance_km: 0.0,
            })
        })
        .map_err(|error| format!("failed to query GeoNames location lookup: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read GeoNames location row: {error}"))
}

fn open_embedded_geonames_database() -> Result<Connection, String> {
    let mut connection = Connection::open_in_memory()
        .map_err(|error| format!("failed to open in-memory SQLite database: {error}"))?;
    let data = sqlite_owned_data(GEONAMES_DATABASE)?;

    connection
        .deserialize(DatabaseName::Main, data, true)
        .map_err(|error| format!("failed to load embedded GeoNames database: {error}"))?;

    Ok(connection)
}

fn sqlite_owned_data(bytes: &[u8]) -> Result<rusqlite::serialize::OwnedData, String> {
    let allocation_size = bytes
        .len()
        .try_into()
        .map_err(|_| "embedded GeoNames database is too large to load".to_string())?;
    let pointer = unsafe { rusqlite::ffi::sqlite3_malloc(allocation_size) };
    let pointer = NonNull::new(pointer.cast::<u8>()).ok_or_else(|| {
        "failed to allocate SQLite memory for embedded GeoNames database".to_string()
    })?;

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.as_ptr(), bytes.len());
        Ok(rusqlite::serialize::OwnedData::from_raw_nonnull(
            pointer,
            bytes.len(),
        ))
    }
}

fn candidate_locations(
    connection: &Connection,
    latitude: f64,
    longitude: f64,
    radius_km: f64,
    minimum_population: Option<i64>,
) -> Result<Vec<GeoLocation>, String> {
    let latitude_delta = radius_km / 111.0;
    let longitude_delta = if latitude.abs() >= 89.0 {
        180.0
    } else {
        (radius_km / (111.0 * latitude.to_radians().cos().abs())).min(180.0)
    };
    let minimum_latitude = (latitude - latitude_delta).max(-90.0);
    let maximum_latitude = (latitude + latitude_delta).min(90.0);
    let minimum_longitude = normalize_longitude(longitude - longitude_delta);
    let maximum_longitude = normalize_longitude(longitude + longitude_delta);
    let wraps_date_line = minimum_longitude > maximum_longitude;

    let population_filter = if minimum_population.is_some() {
        "          AND population > ?5\n"
    } else {
        ""
    };

    let sql = if wraps_date_line {
        format!(
            "
        SELECT name, country_code, latitude, longitude, population, elevation
        FROM locations
        WHERE latitude BETWEEN ?1 AND ?2
          AND (longitude >= ?3 OR longitude <= ?4)
{}
        ",
            population_filter
        )
    } else {
        format!(
            "
        SELECT name, country_code, latitude, longitude, population, elevation
        FROM locations
        WHERE latitude BETWEEN ?1 AND ?2
          AND longitude BETWEEN ?3 AND ?4
{}
        ",
            population_filter
        )
    };

    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| format!("failed to prepare GeoNames query: {error}"))?;
    let mut parameters = vec![
        SqlValue::Real(minimum_latitude),
        SqlValue::Real(maximum_latitude),
        SqlValue::Real(minimum_longitude),
        SqlValue::Real(maximum_longitude),
    ];
    if let Some(minimum_population) = minimum_population {
        parameters.push(SqlValue::Integer(minimum_population));
    }
    let rows = statement
        .query_map(params_from_iter(parameters), |row| {
            Ok(GeoLocation {
                name: row.get(0)?,
                country_code: row.get(1)?,
                latitude: row.get(2)?,
                longitude: row.get(3)?,
                population: row.get(4)?,
                elevation: row.get(5)?,
                distance_km: 0.0,
            })
        })
        .map_err(|error| format!("failed to query GeoNames database: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read GeoNames row: {error}"))
}

fn normalize_longitude(longitude: f64) -> f64 {
    let mut normalized = longitude;
    while normalized < -180.0 {
        normalized += 360.0;
    }
    while normalized > 180.0 {
        normalized -= 360.0;
    }
    normalized
}

fn haversine_distance_km(
    latitude: f64,
    longitude: f64,
    other_latitude: f64,
    other_longitude: f64,
) -> f64 {
    let latitude_delta = (other_latitude - latitude).to_radians();
    let longitude_delta = (other_longitude - longitude).to_radians();
    let latitude = latitude.to_radians();
    let other_latitude = other_latitude.to_radians();
    let a = (latitude_delta / 2.0).sin().powi(2)
        + latitude.cos() * other_latitude.cos() * (longitude_delta / 2.0).sin().powi(2);

    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

fn format_distance(distance_km: f64) -> String {
    if distance_km < 1.0 {
        format!("{:.0} m", distance_km * 1_000.0)
    } else {
        format!("{distance_km:.1} km")
    }
}

fn new_command(dry_run: bool, args: NewArgs) -> Result<(), CliError> {
    let directory = args.path;

    if !directory.exists() {
        return Err(CliError::Error(format!(
            "new path does not exist: {}",
            directory.display()
        )));
    }

    if !directory.is_dir() {
        return Err(CliError::Error(format!(
            "new path is not a directory: {}",
            directory.display()
        )));
    }

    let metadata_path = directory.join(METADATA_FILE_NAME);

    if metadata_path.exists() {
        return Err(CliError::Warning(format!(
            "{} already exists: {}",
            METADATA_FILE_NAME,
            metadata_path.display()
        )));
    }

    let today = Local::now().format("%Y-%m-%d").to_string();
    let image_files = supported_image_files_in_directory(&directory).map_err(CliError::Error)?;
    let image_count = image_files.len();
    let contents = render_metadata_template(&today, &image_files);

    if dry_run {
        println!(
            "new: would create {} (date={today}, frame_count={image_count})",
            metadata_path.display()
        );
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&metadata_path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CliError::Warning(format!(
                    "{} already exists: {}",
                    METADATA_FILE_NAME,
                    metadata_path.display()
                ))
            } else {
                CliError::Error(format!(
                    "failed to create {}: {error}",
                    metadata_path.display()
                ))
            }
        })?;

    file.write_all(contents.as_bytes()).map_err(|error| {
        CliError::Error(format!(
            "failed to write {}: {error}",
            metadata_path.display()
        ))
    })?;

    println!("created {}", metadata_path.display());

    Ok(())
}

fn render_metadata_template(today: &str, image_files: &[PathBuf]) -> String {
    let image_count = image_files.len();
    METADATA_TEMPLATE
        .replace("<today>", today)
        .replace("<image-count-in-directory>", &image_count.to_string())
        .replace("<frames>", &render_metadata_template_frames(image_files))
}

fn render_metadata_template_frames(image_files: &[PathBuf]) -> String {
    image_files
        .iter()
        .map(|path| format!("    {}:", yaml_quoted_string(&file_name(path))))
        .collect::<Vec<_>>()
        .join("\n")
}

fn yaml_quoted_string(value: &str) -> String {
    serde_json::to_string(value).expect("YAML string key should serialize")
}

fn is_supported_image_file(path: &Path) -> bool {
    detect_file_kind(path).file_type != "Unknown"
}

fn check_command(args: CheckArgs) -> Result<(), CliError> {
    let output = build_check_output(args.path.as_deref());
    print_check_output(&output);

    if output.error_count() == 0 {
        Ok(())
    } else {
        Err(CliError::Failure)
    }
}

fn build_check_output(path: Option<&Path>) -> CheckOutput {
    let mut output = CheckOutput::default();
    let resolution = match resolve_metadata_path(path) {
        Ok(resolution) => resolution,
        Err(error) => {
            output.file_errors.push(error);
            return output;
        }
    };

    output.metadata_path = Some(resolution.path.clone());
    output.file_warnings = resolution.warnings;

    let contents = match fs::read_to_string(&resolution.path) {
        Ok(contents) => contents,
        Err(error) => {
            output.file_errors.push(format!(
                "failed to read {}: {error}",
                resolution.path.display()
            ));
            return output;
        }
    };

    let yaml = match serde_yaml::from_str::<YamlValue>(&contents) {
        Ok(yaml) => {
            output.yaml_ok = true;
            yaml
        }
        Err(error) => {
            output.file_errors.push(format!(
                "failed to parse {}: {error}",
                resolution.path.display()
            ));
            return output;
        }
    };

    match check_metadata_file(&resolution.path, &yaml) {
        Ok(report) => {
            output.exif = Some(TagStageReport {
                tags: report.exif_tags,
                warnings: report.exif_warnings,
            });
            output.frames = Some(report.frames);
        }
        Err(error) => output.file_errors.push(error),
    }

    output
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CheckOutput {
    metadata_path: Option<PathBuf>,
    yaml_ok: bool,
    file_warnings: Vec<String>,
    file_errors: Vec<String>,
    exif: Option<TagStageReport>,
    frames: Option<FramesStageReport>,
}

impl CheckOutput {
    fn error_count(&self) -> usize {
        self.file_errors.len()
            + self.frames.as_ref().map_or(0, |frames| {
                frames
                    .frames
                    .iter()
                    .map(|frame| frame.errors.len())
                    .sum::<usize>()
            })
    }

    fn warning_count(&self) -> usize {
        self.file_warnings.len()
            + self.exif.as_ref().map_or(0, |report| report.warnings.len())
            + self.frames.as_ref().map_or(0, |frames| {
                frames.warnings.len()
                    + frames
                        .frames
                        .iter()
                        .map(|frame| frame.warnings.len())
                        .sum::<usize>()
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TagStageReport {
    tags: TagCounts,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FramesStageReport {
    frame_number_count: usize,
    file_count: usize,
    warnings: Vec<String>,
    frames: Vec<FrameReport>,
}

fn print_check_output(output: &CheckOutput) {
    print!("{}", format_check_output(output));
}

fn format_check_output(output: &CheckOutput) -> String {
    let mut rendered = String::new();
    let mut first_group = true;

    append_check_file_group(&mut rendered, &mut first_group, output);
    append_check_exif_group(&mut rendered, &mut first_group, output);
    append_check_frames_group(&mut rendered, &mut first_group, output);
    append_check_overview_group(&mut rendered, &mut first_group, output);

    rendered
}

fn append_check_file_group(output: &mut String, first_group: &mut bool, report: &CheckOutput) {
    append_spaced_check_heading(output, first_group, "file");

    if let Some(path) = &report.metadata_path {
        output.push_str(&format!(
            "metadata file: {}\n",
            format!("found {}", path.display()).green()
        ));
    } else {
        output.push_str("metadata file: skipped\n");
    }

    if report.yaml_ok {
        output.push_str(&format!("YAML format: {}\n", "ok".green()));
    } else {
        output.push_str("YAML format: skipped\n");
    }

    for warning in &report.file_warnings {
        output.push_str(&format!("{}\n", format_check_warning(warning)));
    }
    for error in &report.file_errors {
        output.push_str(&format!("{}\n", format_check_error(error)));
    }
}

fn append_check_exif_group(output: &mut String, first_group: &mut bool, report: &CheckOutput) {
    append_spaced_check_heading(output, first_group, "exif");

    let Some(exif) = &report.exif else {
        output.push_str("skipped\n");
        return;
    };

    append_check_tag_counts(output, &exif.tags);
    for warning in &exif.warnings {
        output.push_str(&format!("{}\n", format_check_warning(warning)));
    }
}

fn append_check_frames_group(output: &mut String, first_group: &mut bool, report: &CheckOutput) {
    append_spaced_check_heading(output, first_group, "frames");

    let Some(frames) = &report.frames else {
        output.push_str("skipped\n");
        return;
    };

    if frames.frames.is_empty() {
        output.push_str("skipped\n");
        return;
    }

    if frames.frame_number_count > 0 {
        output.push_str(&format!("frame numbers: {}\n", frames.frame_number_count));
        output.push_str(&format!("files: {}\n", frames.file_count));
        for warning in &frames.warnings {
            output.push_str(&format!("{}\n", format_check_warning(warning)));
        }
    }

    for frame in &frames.frames {
        output.push_str(&format!("{}\n", check_frame_title(frame).bright_cyan()));
        append_check_tag_counts(output, &frame.tags);
        for location_match in &frame.location_matches {
            output.push_str(&format!("{}\n", format_location_match(location_match)));
        }
        for warning in &frame.warnings {
            output.push_str(&format!("{}\n", format_check_warning(warning)));
        }
        for error in &frame.errors {
            output.push_str(&format!("{}\n", format_check_frame_error(error)));
        }
    }
}

fn append_check_overview_group(output: &mut String, first_group: &mut bool, report: &CheckOutput) {
    append_spaced_check_heading(output, first_group, "overview");

    let errors = report.error_count();
    let warnings = report.warning_count();
    output.push_str(&format!(
        "errors      {}\n",
        format_check_error_count(errors)
    ));
    output.push_str(&format!(
        "warnings    {}\n",
        format_check_warning_count(warnings)
    ));

    if errors > 0 {
        output.push_str(&format!("validation: {}\n", "error".red()));
    } else if warnings > 0 {
        output.push_str(&format!(
            "validation: {} {}\n",
            "success".green(),
            "(with warnings)"
        ));
    } else {
        output.push_str(&format!("validation: {}\n", "success".green()));
    }
}

fn append_check_heading(output: &mut String, label: &str) {
    const WIDTH: usize = 50;
    let dash_count = WIDTH.saturating_sub(label.len() + 1);
    output.push_str(&format!(
        "{}\n",
        format!("{label} {}", "─".repeat(dash_count)).bright_blue()
    ));
}

fn append_spaced_check_heading(output: &mut String, first_group: &mut bool, label: &str) {
    if *first_group {
        *first_group = false;
    } else {
        output.push('\n');
    }
    append_check_heading(output, label);
}

fn append_check_tag_counts(output: &mut String, counts: &TagCounts) {
    output.push_str(&format!("standard tags: {}\n", counts.standard));
    output.push_str(&format!("unknown tags: {}\n", counts.unknown));
}

fn format_check_error_count(count: usize) -> String {
    if count > 0 {
        count.to_string().red().to_string()
    } else {
        count.to_string()
    }
}

fn format_check_warning_count(count: usize) -> String {
    if count > 0 {
        count.to_string().yellow().to_string()
    } else {
        count.to_string()
    }
}

fn check_frame_title(frame: &FrameReport) -> String {
    if frame.is_numeric {
        if let Some(file) = &frame.file {
            return format!("{} ← {}", file_name(file), frame.key);
        }
        return format!("frame {}", frame.key);
    }

    frame.key.clone()
}

fn format_location_match(location_match: &LocationMatch) -> String {
    format!(
        "location: {} [{}, {} ({}, {})]",
        "match found".green(),
        location_match.name,
        location_match.country_code,
        format_decimal_coordinate(location_match.latitude),
        format_decimal_coordinate(location_match.longitude)
    )
}

fn format_check_error(error: &str) -> String {
    format!("error: {error}").red().to_string()
}

fn format_check_frame_error(error: &str) -> String {
    format_check_error(error)
}

struct MetadataPathResolution {
    path: PathBuf,
    warnings: Vec<String>,
}

fn resolve_metadata_path(path: Option<&Path>) -> Result<MetadataPathResolution, String> {
    match path {
        Some(path) if path.is_file() => Ok(MetadataPathResolution {
            path: path.to_path_buf(),
            warnings: Vec::new(),
        }),
        Some(path) if path.is_dir() => resolve_metadata_path_in_directory(path),
        Some(path) => Err(format!("metadata path does not exist: {}", path.display())),
        None => resolve_metadata_path_in_directory(Path::new(".")),
    }
}

fn format_check_warning(warning: &str) -> String {
    let output = format!("warning: {warning}");

    if is_location_lookup_warning(warning) {
        output.red().to_string()
    } else {
        format!("{}: {warning}", "warning".yellow())
    }
}

fn is_location_lookup_warning(warning: &str) -> bool {
    warning.contains("$Location: no match found in database")
}

fn resolve_metadata_path_in_directory(directory: &Path) -> Result<MetadataPathResolution, String> {
    let yml_path = directory.join(METADATA_FILE_NAME);
    let yaml_path = directory.join(LEGACY_METADATA_FILE_NAME);
    let yml_exists = yml_path.is_file();
    let yaml_exists = yaml_path.is_file();

    if yml_exists {
        let mut warnings = Vec::new();
        if yaml_exists {
            warnings.push(format!(
                "{} also exists and was ignored",
                yaml_path.display()
            ));
        }
        return Ok(MetadataPathResolution {
            path: yml_path,
            warnings,
        });
    }

    if yaml_exists {
        return Ok(MetadataPathResolution {
            path: yaml_path,
            warnings: Vec::new(),
        });
    }

    Err(format!(
        "no metadata.yml or metadata.yaml found in {}",
        directory.display()
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CheckReport {
    exif_tags: TagCounts,
    exif_warnings: Vec<String>,
    frame_tags: TagCounts,
    location_matches: Vec<LocationMatch>,
    frames: FramesStageReport,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TagCounts {
    standard: usize,
    unknown: usize,
    unknown_names: Vec<String>,
}

fn check_metadata_file(metadata_path: &Path, yaml: &YamlValue) -> Result<CheckReport, String> {
    let root = yaml
        .as_mapping()
        .ok_or_else(|| "metadata YAML root must be a mapping".to_string())?;
    let mut report = CheckReport::default();

    if let Some(exif) = yaml_mapping_get(root, "exif") {
        let exif = exif
            .as_mapping()
            .ok_or_else(|| "metadata YAML `exif` key must be a mapping".to_string())?;
        report.exif_tags = check_tag_mapping(exif, "exif")?;
        report.exif_warnings = tag_warnings("exif", &report.exif_tags);
    }

    if let Some(frames) = yaml_mapping_get(root, "frames") {
        let empty_frames = Mapping::new();
        let frames = match frames {
            YamlValue::Null => &empty_frames,
            YamlValue::Mapping(frames) => frames,
            _ => return Err("metadata YAML `frames` key must be a mapping".to_string()),
        };
        let frame_report = check_frames_mapping(metadata_path, frames)?;
        report.frame_tags = frame_report.tags;
        report
            .location_matches
            .extend(frame_report.location_matches);
        report.frames = frame_report.frames;
        report.warnings.extend(frame_report.warnings);
    }

    Ok(report)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FrameReport {
    key: String,
    is_numeric: bool,
    file: Option<PathBuf>,
    tags: TagCounts,
    location_matches: Vec<LocationMatch>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct LocationMatch {
    name: String,
    country_code: String,
    latitude: f64,
    longitude: f64,
}

impl Eq for LocationMatch {}

struct FrameValidationReport {
    tags: TagCounts,
    location_matches: Vec<LocationMatch>,
    frames: FramesStageReport,
    warnings: Vec<String>,
}

fn check_frames_mapping(
    metadata_path: &Path,
    frames: &Mapping,
) -> Result<FrameValidationReport, String> {
    let image_directory = path_parent_or_current(metadata_path);
    let image_files = supported_image_files_in_directory(image_directory)?;
    let geonames = open_embedded_geonames_database()?;
    let frame_number_count = frames
        .keys()
        .filter(|key| frame_number_from_key(key).is_some())
        .count();
    let file_count = image_files.len();
    let mut report = FrameValidationReport {
        tags: TagCounts::default(),
        location_matches: Vec::new(),
        frames: FramesStageReport {
            frame_number_count,
            file_count,
            warnings: frame_summary_warnings(frame_number_count, file_count),
            frames: Vec::new(),
        },
        warnings: Vec::new(),
    };

    for (frame_key, frame_value) in frames {
        let frame_number = frame_number_from_key(frame_key);
        let mut frame_report = FrameReport {
            key: yaml_key_label(frame_key),
            is_numeric: frame_number.is_some(),
            file: frame_number.and_then(|number| resolved_frame_file(number, &image_files)),
            ..FrameReport::default()
        };
        check_frame_reference(
            frame_key,
            image_directory,
            &image_files,
            &mut frame_report.warnings,
            &mut frame_report.errors,
        );
        collect_frame_tags(frame_value, &geonames, &mut frame_report)?;
        frame_report
            .warnings
            .extend(tag_warnings("exif", &frame_report.tags));
        merge_tag_counts(&mut report.tags, frame_report.tags.clone());
        report
            .location_matches
            .extend(frame_report.location_matches.clone());
        report.warnings.extend(frame_report.warnings.clone());
        report.frames.frames.push(frame_report);
    }

    Ok(report)
}

fn frame_summary_warnings(frame_number_count: usize, file_count: usize) -> Vec<String> {
    if frame_number_count == 0 {
        return Vec::new();
    }

    if frame_number_count > file_count {
        vec![format!(
            "there are more frame numbers ({frame_number_count}) than image files ({file_count})"
        )]
    } else {
        Vec::new()
    }
}

fn frame_number_from_key(frame_key: &YamlValue) -> Option<usize> {
    let YamlValue::Number(number) = frame_key else {
        return None;
    };
    let number = number.as_i64()?;
    usize::try_from(number).ok().filter(|number| *number > 0)
}

fn resolved_frame_file(frame_number: usize, image_files: &[PathBuf]) -> Option<PathBuf> {
    image_files.get(frame_number.checked_sub(1)?).cloned()
}

fn check_frame_reference(
    frame_key: &YamlValue,
    image_directory: &Path,
    image_files: &[PathBuf],
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    match frame_key {
        YamlValue::Number(number) => {
            let Some(frame_number) = number.as_i64() else {
                warnings.push(format!(
                    "frame reference `{}` is not a valid frame number",
                    yaml_key_label(frame_key)
                ));
                return;
            };

            if frame_number < 1 || frame_number as usize > image_files.len() {
                errors.push(format!(
                    "frame reference `{frame_number}` does not match an image file"
                ));
            }
        }
        YamlValue::String(file_name) => {
            if !image_directory.join(file_name).is_file() {
                errors.push("file does not exist".to_string());
            }
        }
        _ => warnings.push(format!(
            "frame reference `{}` is not a valid frame key",
            yaml_key_label(frame_key)
        )),
    }
}

fn collect_frame_tags(
    frame_value: &YamlValue,
    geonames: &Connection,
    report: &mut FrameReport,
) -> Result<(), String> {
    match frame_value {
        YamlValue::Mapping(mapping) => {
            collect_frame_tag_mapping(mapping, geonames, report)?;
            Ok(())
        }
        YamlValue::Sequence(items) => {
            for item in items {
                let mapping = item
                    .as_mapping()
                    .ok_or_else(|| "metadata YAML frame entries must be mappings".to_string())?;
                if mapping.len() != 1 {
                    return Err(
                        "metadata YAML frame sequence entries must contain one tag".to_string()
                    );
                }
                collect_frame_tag_mapping(mapping, geonames, report)?;
            }
            Ok(())
        }
        YamlValue::Null => Ok(()),
        _ => Err("metadata YAML frame values must be mappings or sequences".to_string()),
    }
}

fn collect_frame_tag_mapping(
    mapping: &Mapping,
    geonames: &Connection,
    report: &mut FrameReport,
) -> Result<(), String> {
    merge_tag_counts(&mut report.tags, check_tag_mapping(mapping, "frames")?);

    for (key, value) in mapping {
        if key.as_str() == Some("$Location") {
            check_location_value(value, geonames, report)?;
        }
    }

    Ok(())
}

fn check_location_value(
    value: &YamlValue,
    geonames: &Connection,
    report: &mut FrameReport,
) -> Result<(), String> {
    let Some(location_name) = location_name_from_yaml(value, &mut report.warnings) else {
        return Ok(());
    };

    let locations = locations_by_name(geonames, location_name)?;
    if let Some(location) = locations.first() {
        report.location_matches.push(LocationMatch {
            name: location.name.clone(),
            country_code: location.country_code.clone(),
            latitude: location.latitude,
            longitude: location.longitude,
        });
    } else {
        report.warnings.push(format!(
            "$Location: no match found in database [for <{location_name}>]"
        ));
    }

    Ok(())
}

fn location_name_from_yaml<'a>(
    value: &'a YamlValue,
    warnings: &mut Vec<String>,
) -> Option<&'a str> {
    match value {
        YamlValue::Null => None,
        YamlValue::String(location_name) => {
            let location_name = location_name.trim();
            if location_name.is_empty() {
                None
            } else {
                Some(location_name)
            }
        }
        _ => {
            warnings.push("frames $Location value must be a string".to_string());
            None
        }
    }
}

fn check_tag_mapping(mapping: &Mapping, context: &str) -> Result<TagCounts, String> {
    let mut counts = TagCounts::default();

    for (key, _) in mapping {
        let Some(tag_name) = key.as_str() else {
            return Err(format!(
                "metadata YAML `{context}` tag keys must be strings"
            ));
        };
        count_tag(tag_name, &mut counts);
    }

    Ok(counts)
}

fn count_tag(tag_name: &str, counts: &mut TagCounts) {
    if is_known_metadata_tag(tag_name) {
        counts.standard += 1;
    } else {
        counts.unknown += 1;
        if !counts.unknown_names.iter().any(|name| name == tag_name) {
            counts.unknown_names.push(tag_name.to_string());
        }
    }
}

fn merge_tag_counts(counts: &mut TagCounts, next: TagCounts) {
    counts.standard += next.standard;
    counts.unknown += next.unknown;
    for name in next.unknown_names {
        if !counts
            .unknown_names
            .iter()
            .any(|existing| existing == &name)
        {
            counts.unknown_names.push(name);
        }
    }
}

fn tag_warnings(context: &str, counts: &TagCounts) -> Vec<String> {
    counts
        .unknown_names
        .iter()
        .map(|name| format!("{context} tag is non-standard `{name}`"))
        .collect()
}

fn yaml_mapping_get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_string()))
}

fn yaml_key_label(value: &YamlValue) -> String {
    match value {
        YamlValue::String(value) => value.clone(),
        YamlValue::Number(value) => value.to_string(),
        YamlValue::Bool(value) => value.to_string(),
        _ => format!("{value:?}"),
    }
}

fn supported_image_files_in_directory(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read directory {}: {error}", directory.display()))?;
    let mut paths = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if path.is_file() && is_supported_image_file(&path) {
            paths.push(path);
        }
    }

    paths.sort_by_key(|path| file_name(path).to_ascii_lowercase());
    Ok(paths)
}

fn is_known_metadata_tag(tag_name: &str) -> bool {
    tag_name.starts_with('$') && matches!(tag_name, "$Location")
        || STANDARD_EXIF_TAG_NAMES.contains(&tag_name)
}

const STANDARD_EXIF_TAG_NAMES: &[&str] = &[
    "Acceleration",
    "ApertureValue",
    "Artist",
    "BitsPerSample",
    "BodySerialNumber",
    "BrightnessValue",
    "CFAPattern",
    "CameraElevationAngle",
    "CameraOwnerName",
    "ColorSpace",
    "ComponentsConfiguration",
    "CompressedBitsPerPixel",
    "Compression",
    "CompositeImage",
    "Contrast",
    "Copyright",
    "CreateDate",
    "CustomRendered",
    "DateTime",
    "DateTimeDigitized",
    "DateTimeOriginal",
    "DeviceSettingDescription",
    "DigitalZoomRatio",
    "ExifVersion",
    "ExposureBiasValue",
    "ExposureIndex",
    "ExposureMode",
    "ExposureProgram",
    "ExposureTime",
    "FNumber",
    "FileSource",
    "Flash",
    "FlashEnergy",
    "FlashpixVersion",
    "FocalLength",
    "FocalLengthIn35mmFilm",
    "FocalPlaneResolutionUnit",
    "FocalPlaneXResolution",
    "FocalPlaneYResolution",
    "GPSAltitude",
    "GPSAltitudeRef",
    "GPSAreaInformation",
    "GPSDateStamp",
    "GPSDestBearing",
    "GPSDestBearingRef",
    "GPSDestDistance",
    "GPSDestDistanceRef",
    "GPSDestLatitude",
    "GPSDestLatitudeRef",
    "GPSDestLongitude",
    "GPSDestLongitudeRef",
    "GPSDifferential",
    "GPSDOP",
    "GPSHPositioningError",
    "GPSImgDirection",
    "GPSImgDirectionRef",
    "GPSInfoIFDPointer",
    "GPSLatitude",
    "GPSLatitudeRef",
    "GPSLongitude",
    "GPSLongitudeRef",
    "GPSMapDatum",
    "GPSMeasureMode",
    "GPSProcessingMethod",
    "GPSSatellites",
    "GPSSpeed",
    "GPSSpeedRef",
    "GPSStatus",
    "GPSTimeStamp",
    "GPSTrack",
    "GPSTrackRef",
    "GPSVersionID",
    "GainControl",
    "Gamma",
    "Humidity",
    "ISO",
    "ISOSpeed",
    "ISOSpeedLatitudezzz",
    "ISOSpeedLatitudeyyy",
    "ISOSpeedRatings",
    "ImageDescription",
    "ImageHeight",
    "ImageLength",
    "ImageUniqueID",
    "ImageWidth",
    "InteropIFDPointer",
    "InteroperabilityIndex",
    "InteroperabilityVersion",
    "JPEGInterchangeFormat",
    "JPEGInterchangeFormatLength",
    "LensMake",
    "LensModel",
    "LensSerialNumber",
    "LensSpecification",
    "LightSource",
    "Make",
    "MakerNote",
    "MaxApertureValue",
    "MeteringMode",
    "Model",
    "OECF",
    "OffsetTime",
    "OffsetTimeDigitized",
    "OffsetTimeOriginal",
    "Orientation",
    "PhotographicSensitivity",
    "PhotometricInterpretation",
    "PixelXDimension",
    "PixelYDimension",
    "PlanarConfiguration",
    "Pressure",
    "PrimaryChromaticities",
    "RecommendedExposureIndex",
    "ReferenceBlackWhite",
    "RelatedImageFileFormat",
    "RelatedImageLength",
    "RelatedImageWidth",
    "RelatedSoundFile",
    "ResolutionUnit",
    "RowsPerStrip",
    "SamplesPerPixel",
    "Saturation",
    "SceneCaptureType",
    "SceneType",
    "SensingMethod",
    "Sharpness",
    "ShutterSpeedValue",
    "Software",
    "SourceExposureTimesOfCompositeImage",
    "SourceImageNumberOfCompositeImage",
    "SpatialFrequencyResponse",
    "SpectralSensitivity",
    "StandardOutputSensitivity",
    "StripByteCounts",
    "StripOffsets",
    "SubSecTime",
    "SubSecTimeDigitized",
    "SubSecTimeOriginal",
    "SubjectArea",
    "SubjectDistance",
    "SubjectDistanceRange",
    "SubjectLocation",
    "Temperature",
    "TileByteCounts",
    "TileOffsets",
    "TransferFunction",
    "UserComment",
    "WaterDepth",
    "WhiteBalance",
    "WhitePoint",
    "XResolution",
    "YCbCrCoefficients",
    "YCbCrPositioning",
    "YCbCrSubSampling",
    "YResolution",
];

fn write_command(dry_run: bool, args: WriteArgs) -> Result<(), String> {
    let started = Instant::now();
    let request = WriteRequest::from_args(&args);
    let strip_mode = StripMode::from_write_args(&args)?;
    let resolution = resolve_metadata_path(request.metadata.as_deref())?;

    let metadata_dir = path_parent_or_current(&resolution.path);
    let targets = resolve_write_targets(
        metadata_dir,
        request.targets.as_deref(),
        args.recursive,
        &args.extensions,
    )?;

    if targets.is_empty() {
        return Err("no target images matched".to_string());
    }

    let contents = fs::read_to_string(&resolution.path)
        .map_err(|error| format!("failed to read {}: {error}", resolution.path.display()))?;
    let yaml = serde_yaml::from_str::<YamlValue>(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", resolution.path.display()))?;
    check_metadata_file(&resolution.path, &yaml)?;

    let plan = build_write_plan(&resolution.path, &yaml, &targets)?;
    let mut summary = WriteSummary::default();
    let mut output = WriteOutput {
        metadata_path: resolution.path.clone(),
        dry_run,
        target_count: targets.len(),
        file_warnings: resolution.warnings,
        files: Vec::new(),
        skipped_files: Vec::new(),
    };
    summary.warnings += output.file_warnings.len();
    let spinner = SpinnerPreset::random();

    print!("{}", format_write_metadata_output(&output));
    print!("{}", format_write_frames_heading());
    flush_stdout();

    for image in targets {
        let Some(frame) = plan.frame_for_image(&image) else {
            summary.skipped_files += 1;
            output.skipped_files.push(image);
            print!(
                "{}",
                format_write_skipped_file_output(
                    output
                        .skipped_files
                        .last()
                        .expect("skipped file should exist")
                )
            );
            flush_stdout();
            continue;
        };

        let file_output = WriteFileOutput {
            label: frame.label,
            image,
            result: WriteFileResult::default(),
            elapsed_ms: 0,
            dry_run,
        };
        print!("{}", format_write_file_header_output(&file_output));
        flush_stdout();

        let file_started = Instant::now();
        let progress = TerminalSpinner::start(spinner, "writing metadata".to_string());
        let result = apply_tags_to_image(
            &file_output.image,
            &frame.tags,
            dry_run,
            &args,
            strip_mode.as_ref(),
        );
        progress.finish();
        let elapsed_ms = file_started.elapsed().as_millis();
        summary.add(&result);
        let file_output = WriteFileOutput {
            result,
            elapsed_ms,
            ..file_output
        };
        print!("{}", format_write_file_result_output(&file_output));
        flush_stdout();
        output.files.push(file_output);
    }

    summary.elapsed_ms = started.elapsed().as_millis();
    print!("{}", format_write_overview_output(&summary));
    flush_stdout();

    if summary.errors > 0 {
        Err("one or more target images failed".to_string())
    } else {
        Ok(())
    }
}

fn strip_command(dry_run: bool, args: StripArgs) -> Result<(), CliError> {
    let started = Instant::now();
    let mode = StripMode::from_args(&args).map_err(CliError::Error)?;
    let targets = resolve_write_targets(
        Path::new("."),
        args.targets.as_deref(),
        args.recursive,
        &args.extensions,
    )
    .map_err(CliError::Error)?;

    if targets.is_empty() {
        return Err(CliError::Error("no target images matched".to_string()));
    }

    let mut output = StripOutput {
        dry_run,
        mode: mode.name(),
        target_count: targets.len(),
        files: Vec::new(),
    };
    let mut summary = StripSummary::default();
    let spinner = SpinnerPreset::random();

    if !args.json {
        print!("{}", format_strip_heading_output(&output));
        flush_stdout();
    }

    for image in targets {
        let file_output = StripFileOutput {
            label: write_file_heading(&image),
            image,
            result: StripFileResult::default(),
            elapsed_ms: 0,
            dry_run,
        };

        if !args.json {
            print!("{}", format_strip_file_header_output(&file_output));
            flush_stdout();
        }

        let file_started = Instant::now();
        let result = if args.json {
            strip_metadata_from_image(&file_output.image, dry_run, args.verify, &mode)
        } else {
            let progress = TerminalSpinner::start(spinner, "stripping metadata".to_string());
            let result = strip_metadata_from_image(&file_output.image, dry_run, args.verify, &mode);
            progress.finish();
            result
        };
        let elapsed_ms = file_started.elapsed().as_millis();
        summary.add(&result);

        let file_output = StripFileOutput {
            result,
            elapsed_ms,
            ..file_output
        };

        if !args.json {
            print!("{}", format_strip_file_result_output(&file_output));
            flush_stdout();
        }

        output.files.push(file_output);
    }

    summary.elapsed_ms = started.elapsed().as_millis();

    if args.json {
        println!("{}", format_strip_json_output(&output, &summary));
    } else {
        print!("{}", format_strip_overview_output(&output, &summary));
        flush_stdout();
    }

    if summary.errors > 0 {
        Err(CliError::Failure)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct StripFileResult {
    stripped: bool,
    verified: bool,
    removed_tags: usize,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct StripSummary {
    stripped_files: usize,
    removed_tags: usize,
    verified_files: usize,
    warnings: usize,
    errors: usize,
    elapsed_ms: u128,
}

impl StripSummary {
    fn add(&mut self, result: &StripFileResult) {
        self.stripped_files += usize::from(result.stripped);
        self.removed_tags += result.removed_tags;
        self.verified_files += usize::from(result.verified);
        self.warnings += result.warnings.len();
        self.errors += result.errors.len();
    }
}

#[derive(Debug, Clone)]
struct StripOutput {
    dry_run: bool,
    mode: &'static str,
    target_count: usize,
    files: Vec<StripFileOutput>,
}

#[derive(Debug, Clone)]
struct StripFileOutput {
    label: String,
    image: PathBuf,
    result: StripFileResult,
    elapsed_ms: u128,
    dry_run: bool,
}

#[derive(Debug, Clone)]
enum StripBaseMode {
    All,
    None,
    Keep(TagSelectorSet),
    Privacy,
}

#[derive(Debug, Clone)]
struct StripMode {
    base: StripBaseMode,
    remove: Option<TagSelectorSet>,
}

impl StripMode {
    fn all() -> Self {
        Self {
            base: StripBaseMode::All,
            remove: None,
        }
    }

    #[cfg(test)]
    fn keep(selectors: TagSelectorSet) -> Self {
        Self {
            base: StripBaseMode::Keep(selectors),
            remove: None,
        }
    }

    #[cfg(test)]
    fn remove(selectors: TagSelectorSet) -> Self {
        Self {
            base: StripBaseMode::None,
            remove: Some(selectors),
        }
    }

    #[cfg(test)]
    fn privacy() -> Self {
        Self {
            base: StripBaseMode::Privacy,
            remove: None,
        }
    }

    #[cfg(test)]
    fn keep_with_remove(keep: TagSelectorSet, remove: TagSelectorSet) -> Self {
        Self {
            base: StripBaseMode::Keep(keep),
            remove: Some(remove),
        }
    }

    #[cfg(test)]
    fn privacy_with_remove(remove: TagSelectorSet) -> Self {
        Self {
            base: StripBaseMode::Privacy,
            remove: Some(remove),
        }
    }

    fn from_args(args: &StripArgs) -> Result<Self, String> {
        Self::from_parts(false, &args.keep, &args.remove, args.privacy)
            .map(|mode| mode.unwrap_or_else(Self::all))
    }

    fn from_write_args(args: &WriteArgs) -> Result<Option<Self>, String> {
        Self::from_parts(args.strip, &args.keep, &args.remove, args.privacy)
    }

    fn from_parts(
        strip: bool,
        keep: &[String],
        remove: &[String],
        privacy: bool,
    ) -> Result<Option<Self>, String> {
        if strip && (!keep.is_empty() || !remove.is_empty() || privacy) {
            return Err(
                "--strip cannot be combined with --keep, --remove, or --privacy".to_string(),
            );
        }
        if !keep.is_empty() && privacy {
            return Err("--keep and --privacy cannot be combined".to_string());
        }
        if strip {
            return Ok(Some(Self::all()));
        }

        let remove = (!remove.is_empty())
            .then(|| TagSelectorSet::from_values(remove))
            .transpose()?;
        let base = if !keep.is_empty() {
            StripBaseMode::Keep(TagSelectorSet::from_values(keep)?)
        } else if privacy {
            StripBaseMode::Privacy
        } else if remove.is_some() {
            StripBaseMode::None
        } else {
            return Ok(None);
        };

        Ok(Some(Self { base, remove }))
    }

    fn name(&self) -> &'static str {
        match &self.base {
            StripBaseMode::All => "all",
            StripBaseMode::None => "remove",
            StripBaseMode::Keep(_) => "keep",
            StripBaseMode::Privacy => "privacy",
        }
    }

    fn is_full_strip(&self) -> bool {
        matches!(self.base, StripBaseMode::All) && self.remove.is_none()
    }

    fn removal_decision(&self, field: &Field) -> StripRemovalDecision {
        if self
            .remove
            .as_ref()
            .is_some_and(|selectors| selectors.matches_field(field))
        {
            return StripRemovalDecision::Explicit;
        }

        if match &self.base {
            StripBaseMode::All => true,
            StripBaseMode::None => false,
            StripBaseMode::Keep(selectors) => !selectors.matches_field(field),
            StripBaseMode::Privacy => is_privacy_strip_field(field),
        } {
            StripRemovalDecision::Base
        } else {
            StripRemovalDecision::Keep
        }
    }

    fn should_remove(&self, field: &Field, is_tiff: bool) -> bool {
        match self.removal_decision(field) {
            StripRemovalDecision::Explicit => true,
            StripRemovalDecision::Base => !(is_tiff && is_required_tiff_structural_field(field)),
            StripRemovalDecision::Keep => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripRemovalDecision {
    Keep,
    Base,
    Explicit,
}

#[derive(Debug, Clone)]
struct TagSelectorSet {
    names: HashSet<String>,
}

impl TagSelectorSet {
    fn from_values(values: &[String]) -> Result<Self, String> {
        let mut names = HashSet::new();
        for value in values {
            for name in value.split(',') {
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                for alias in normalized_strip_tag_aliases(name) {
                    names.insert(alias);
                }
            }
        }

        if names.is_empty() {
            return Err("strip tag list cannot be empty".to_string());
        }

        Ok(Self { names })
    }

    fn matches_field(&self, field: &Field) -> bool {
        strip_field_names(field)
            .into_iter()
            .any(|name| self.names.contains(&name))
    }
}

fn normalized_strip_tag_aliases(name: &str) -> Vec<String> {
    match normalized_strip_tag_name(name).as_str() {
        "photographer" => vec![normalized_strip_tag_name("Artist")],
        "iso" | "isospeed" | "isospeedratings" | "photographicsensitivity" => [
            "ISO",
            "ISOSpeed",
            "ISOSpeedRatings",
            "PhotographicSensitivity",
        ]
        .into_iter()
        .map(normalized_strip_tag_name)
        .collect(),
        "createdate" | "datetimedigitized" => ["CreateDate", "DateTimeDigitized"]
            .into_iter()
            .map(normalized_strip_tag_name)
            .collect(),
        "modifydate" | "datetime" => ["ModifyDate", "DateTime"]
            .into_iter()
            .map(normalized_strip_tag_name)
            .collect(),
        "predictor" => ["Predictor", "0x013D"]
            .into_iter()
            .map(normalized_strip_tag_name)
            .collect(),
        "icc" | "iccprofile" | "intercolorprofile" => ["ICCProfile", "0x8773"]
            .into_iter()
            .map(normalized_strip_tag_name)
            .collect(),
        normalized => vec![normalized.to_string()],
    }
}

fn normalized_strip_tag_name(name: &str) -> String {
    name.chars()
        .filter(|char| char.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn strip_field_names(field: &Field) -> Vec<String> {
    let mut names = vec![
        normalized_strip_tag_name(&field.tag.to_string()),
        normalized_strip_tag_name(&format!("0x{:04X}", field.tag.number())),
    ];
    if field.tag == Tag::PhotographicSensitivity {
        names.extend(normalized_strip_tag_aliases("ISO"));
    }
    names.sort();
    names.dedup();
    names
}

fn is_privacy_strip_field(field: &Field) -> bool {
    if field.tag.context() == Context::Gps || field.tag.to_string().starts_with("GPS") {
        return true;
    }

    let name = field.tag.to_string();
    let normalized = normalized_strip_tag_name(&name);

    normalized.contains("serial")
        || normalized.contains("owner")
        || normalized.contains("datetime")
        || normalized.contains("date")
        || normalized.contains("offsettime")
        || normalized.contains("subsectime")
        || matches!(
            name.as_str(),
            "Software"
                | "UserComment"
                | "ImageDescription"
                | "Artist"
                | "Copyright"
                | "MakerNote"
                | "ImageUniqueID"
        )
}

fn strip_metadata_from_image(
    image: &Path,
    dry_run: bool,
    verify: bool,
    mode: &StripMode,
) -> StripFileResult {
    let mut result = StripFileResult::default();
    let is_tiff = is_tiff_image(image);
    let before = match read_metadata(image) {
        Ok(metadata) => Some(metadata),
        Err(error) if !mode.is_full_strip() || verify => {
            result
                .errors
                .push(format!("failed to read EXIF metadata: {error}"));
            return result;
        }
        Err(_) => None,
    };

    let planned_removals = before
        .as_ref()
        .map(|metadata| {
            strip_removal_targets(
                &metadata.exif,
                mode,
                is_tiff,
                &mut result.warnings,
                &mut result.errors,
            )
        })
        .unwrap_or_default();
    result.removed_tags = planned_removals.len();

    if !result.errors.is_empty() {
        return result;
    }

    if dry_run {
        result.stripped = mode.is_full_strip() || result.removed_tags > 0;
        return result;
    }

    if mode.is_full_strip() {
        let strip_result = if is_tiff {
            strip_tiff_full_metadata(image)
        } else {
            WritableMetadata::file_clear_metadata(image).map_err(|error| error.to_string())
        };
        if let Err(error) = strip_result {
            result
                .errors
                .push(format!("failed to strip EXIF metadata: {error}"));
            return result;
        }
        result.stripped = true;
    } else {
        if planned_removals.is_empty() {
            result.stripped = false;
        } else {
            let mut metadata = match WritableMetadata::new_from_path(image) {
                Ok(metadata) => metadata,
                Err(error) => {
                    result
                        .errors
                        .push(format!("failed to read writable EXIF metadata: {error}"));
                    return result;
                }
            };

            for target in &planned_removals {
                if metadata.remove_tag_by_hex_group(target.tag_id, target.group) == 0 {
                    result.warnings.push(format!(
                        "could not find writable EXIF tag `{}` to remove",
                        target.name
                    ));
                }
            }

            if let Err(error) = metadata.write_to_file(image) {
                result
                    .errors
                    .push(format!("failed to write stripped EXIF metadata: {error}"));
                return result;
            }
            result.stripped = true;
        }
    }

    if verify {
        match verify_strip_result(image, mode) {
            Ok(()) => {
                result.verified = true;
            }
            Err(error) => result.errors.push(format!("verification failed: {error}")),
        }
    }

    result
}

fn strip_tiff_full_metadata(image: &Path) -> Result<(), String> {
    let mut metadata = WritableMetadata::new_from_path(image)
        .map_err(|error| format!("failed to read writable EXIF metadata: {error}"))?;
    let preserved_tags = metadata
        .get_ifds()
        .iter()
        .filter(|ifd| ifd.get_ifd_type() == ExifTagGroup::GENERIC)
        .flat_map(|ifd| {
            ifd.get_tags()
                .iter()
                .filter(|tag| TIFF_VISUAL_STRUCTURAL_TAG_IDS.contains(&tag.as_u16()))
                .cloned()
                .map(|tag| (ifd.get_ifd_type(), ifd.get_generic_ifd_nr(), tag))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<(ExifTagGroup, u32, WritableExifTag)>>();

    metadata.reduce_to_a_minimum();
    for (group, ifd_number, tag) in preserved_tags {
        metadata.get_ifd_mut(group, ifd_number).set_tag(tag);
    }
    metadata
        .write_to_file(image)
        .map_err(|error| format!("failed to write stripped EXIF metadata: {error}"))
}

#[derive(Debug, Clone)]
struct StripRemovalTarget {
    group: ExifTagGroup,
    tag_id: u16,
    name: String,
}

fn strip_removal_targets(
    exif: &Exif,
    mode: &StripMode,
    is_tiff: bool,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> Vec<StripRemovalTarget> {
    let mut seen = HashSet::new();
    let mut targets = Vec::new();

    for field in exif.fields() {
        match mode.removal_decision(field) {
            StripRemovalDecision::Keep => continue,
            StripRemovalDecision::Base if is_tiff && is_required_tiff_structural_field(field) => {
                continue;
            }
            StripRemovalDecision::Explicit
                if is_tiff && is_required_tiff_structural_field(field) =>
            {
                errors.push(format!(
                    "cannot remove required TIFF tag `{}`: TIFF cannot be written without it",
                    strip_tag_display(field)
                ));
                continue;
            }
            StripRemovalDecision::Base | StripRemovalDecision::Explicit => {}
        }

        let Some(group) = writable_group_from_context(field.tag.context()) else {
            warnings.push(format!(
                "cannot remove EXIF tag `{}` from unsupported context {:?}",
                field.tag,
                field.tag.context()
            ));
            continue;
        };

        push_strip_removal_target(
            &mut targets,
            &mut seen,
            group,
            field.tag.number(),
            field.tag.to_string(),
        );
    }

    if is_tiff && mode.is_full_strip() {
        for &(group, tag_id, name) in TIFF_FULL_STRIP_AUXILIARY_TAGS {
            push_strip_removal_target(&mut targets, &mut seen, group, tag_id, name.to_string());
        }
    }

    targets
}

fn push_strip_removal_target(
    targets: &mut Vec<StripRemovalTarget>,
    seen: &mut HashSet<(String, u16)>,
    group: ExifTagGroup,
    tag_id: u16,
    name: String,
) {
    let key = (format!("{group:?}"), tag_id);
    if seen.insert(key) {
        targets.push(StripRemovalTarget {
            group,
            tag_id,
            name,
        });
    }
}

const TIFF_FULL_STRIP_AUXILIARY_TAGS: &[(ExifTagGroup, u16, &str)] = &[
    (ExifTagGroup::GENERIC, 0x8769, "ExifIFDPointer"),
    (ExifTagGroup::GENERIC, 0x8825, "GPSInfoIFDPointer"),
    (ExifTagGroup::EXIF, 0xA005, "InteropIFDPointer"),
];

fn strip_tag_display(field: &Field) -> String {
    let name = field.tag.to_string();
    if name.starts_with("Tag(") {
        format!("0x{:04X}", field.tag.number())
    } else {
        name
    }
}

fn is_tiff_image(image: &Path) -> bool {
    detect_file_kind(image).file_type == "TIFF"
}

fn is_required_tiff_structural_field(field: &Field) -> bool {
    field.tag.context() == Context::Tiff
        && (TIFF_VISUAL_STRUCTURAL_TAG_IDS.contains(&field.tag.number())
            || matches!(
                field.tag.to_string().as_str(),
                "ImageWidth"
                    | "ImageLength"
                    | "ImageHeight"
                    | "BitsPerSample"
                    | "Compression"
                    | "PhotometricInterpretation"
                    | "StripOffsets"
                    | "Orientation"
                    | "SamplesPerPixel"
                    | "RowsPerStrip"
                    | "StripByteCounts"
                    | "XResolution"
                    | "YResolution"
                    | "PlanarConfiguration"
                    | "ResolutionUnit"
                    | "ColorMap"
            ))
}

const TIFF_VISUAL_STRUCTURAL_TAG_IDS: &[u16] = &[
    0x0100, // ImageWidth
    0x0101, // ImageLength
    0x0102, // BitsPerSample
    0x0103, // Compression
    0x0106, // PhotometricInterpretation
    0x0111, // StripOffsets
    0x0112, // Orientation
    0x0115, // SamplesPerPixel
    0x0116, // RowsPerStrip
    0x0117, // StripByteCounts
    0x011A, // XResolution
    0x011B, // YResolution
    0x011C, // PlanarConfiguration
    0x0128, // ResolutionUnit
    0x013D, // Predictor
    0x0140, // ColorMap
    0x0142, // TileWidth
    0x0143, // TileLength
    0x0144, // TileOffsets
    0x0145, // TileByteCounts
    0x0152, // ExtraSamples
    0x0153, // SampleFormat
    0x8773, // ICC profile
];

fn writable_group_from_context(context: Context) -> Option<ExifTagGroup> {
    match context {
        Context::Tiff => Some(ExifTagGroup::GENERIC),
        Context::Exif => Some(ExifTagGroup::EXIF),
        Context::Gps => Some(ExifTagGroup::GPS),
        Context::Interop => Some(ExifTagGroup::INTEROP),
        _ => None,
    }
}

fn verify_strip_result(image: &Path, mode: &StripMode) -> Result<(), String> {
    let metadata = read_metadata(image)?;
    let is_tiff = is_tiff_image(image);
    let remaining = metadata
        .exif
        .fields()
        .filter(|field| mode.should_remove(field, is_tiff))
        .map(|field| field.tag.to_string())
        .collect::<Vec<_>>();
    if remaining.is_empty() {
        Ok(())
    } else {
        Err(format!("EXIF tags remain: {}", remaining.join(", ")))
    }
}

#[cfg(test)]
fn format_strip_output(output: &StripOutput, summary: &StripSummary) -> String {
    let mut rendered = String::new();
    rendered.push_str(&format_strip_heading_output(output));
    for file in &output.files {
        rendered.push_str(&format_strip_file_output(file));
    }
    rendered.push_str(&format_strip_overview_output(output, summary));
    rendered
}

fn format_strip_heading_output(output: &StripOutput) -> String {
    let mut rendered = String::new();
    let mut first_group = true;
    append_strip_heading_group(&mut rendered, &mut first_group, output);
    rendered
}

#[cfg(test)]
fn format_strip_file_output(file: &StripFileOutput) -> String {
    let mut rendered = String::new();
    append_strip_file_header(&mut rendered, file);
    append_strip_file_result(&mut rendered, file);
    rendered
}

fn format_strip_file_header_output(file: &StripFileOutput) -> String {
    let mut rendered = String::new();
    append_strip_file_header(&mut rendered, file);
    rendered
}

fn format_strip_file_result_output(file: &StripFileOutput) -> String {
    let mut rendered = String::new();
    append_strip_file_result(&mut rendered, file);
    rendered
}

fn format_strip_overview_output(output: &StripOutput, summary: &StripSummary) -> String {
    let mut rendered = String::new();
    let mut first_group = false;
    append_strip_overview_group(&mut rendered, &mut first_group, output, summary);
    rendered
}

fn append_strip_heading_group(rendered: &mut String, first_group: &mut bool, output: &StripOutput) {
    append_spaced_check_heading(rendered, first_group, "strip");
    rendered.push_str(&format!("mode: {}\n", output.mode));
    rendered.push_str(&format!("targets: {}\n", output.target_count));
    if output.dry_run {
        rendered.push_str(&format!("operation: {}\n", "dry-run".yellow()));
    }
}

fn append_strip_file_header(rendered: &mut String, file: &StripFileOutput) {
    append_write_frame_subtitle(rendered, &file.label);
    append_write_file_path(rendered, &file.image);
}

fn append_strip_file_result(rendered: &mut String, file: &StripFileOutput) {
    let action = if file.dry_run && file.result.stripped {
        "would strip EXIF"
    } else if file.dry_run {
        "would strip EXIF: no"
    } else if file.result.stripped {
        "stripped EXIF"
    } else {
        "stripped EXIF: no"
    };
    rendered.push_str(&format!("{action}\n"));
    rendered.push_str(&format!("removed {} tags\n", file.result.removed_tags));
    if file.result.verified {
        rendered.push_str("verified: no EXIF metadata\n");
    }
    rendered.push_str(&format!(
        "took {}\n",
        format_write_duration(file.elapsed_ms)
    ));
    for warning in &file.result.warnings {
        rendered.push_str(&format!("{}\n", format_check_warning(warning)));
    }
    for error in &file.result.errors {
        rendered.push_str(&format!("{}\n", format_check_error(error)));
    }
}

fn append_strip_overview_group(
    rendered: &mut String,
    first_group: &mut bool,
    output: &StripOutput,
    summary: &StripSummary,
) {
    append_spaced_check_heading(rendered, first_group, "overview");
    append_write_overview_row(rendered, "errors", summary.errors);
    append_write_overview_row(rendered, "warnings", summary.warnings);
    append_write_overview_row(
        rendered,
        if output.dry_run {
            "would strip"
        } else {
            "stripped"
        },
        summary.stripped_files,
    );
    append_write_overview_row(rendered, "removed tags", summary.removed_tags);
    append_write_overview_row(rendered, "verified", summary.verified_files);
    append_write_overview_row(rendered, "took", format_write_duration(summary.elapsed_ms));
    if summary.errors > 0 {
        append_write_overview_row(rendered, "status", "fail".red());
    } else if summary.warnings > 0 {
        append_write_overview_row(
            rendered,
            "status",
            format!("{} {}", "success".green(), "(with warnings)"),
        );
    } else {
        append_write_overview_row(rendered, "status", "success".green());
    }
}

fn format_strip_json_output(output: &StripOutput, summary: &StripSummary) -> String {
    let files = output
        .files
        .iter()
        .map(|file| {
            json!({
                "path": path_to_pattern(&file.image),
                "status": strip_file_status(file),
                "stripped": file.result.stripped && !file.dry_run,
                "would_strip": file.result.stripped && file.dry_run,
                "removed_tags": file.result.removed_tags,
                "verified": file.result.verified,
                "elapsed_ms": file.elapsed_ms,
                "warnings": file.result.warnings,
                "errors": file.result.errors,
            })
        })
        .collect::<Vec<_>>();

    json!({
        "command": "strip",
        "mode": output.mode,
        "dry_run": output.dry_run,
        "target_count": output.target_count,
        "files": files,
        "summary": {
            "files_stripped": if output.dry_run { 0 } else { summary.stripped_files },
            "files_would_strip": if output.dry_run { summary.stripped_files } else { 0 },
            "tags_removed": summary.removed_tags,
            "files_verified": summary.verified_files,
            "warnings": summary.warnings,
            "errors": summary.errors,
            "elapsed_ms": summary.elapsed_ms,
        },
        "status": if summary.errors > 0 { "fail" } else { "success" },
    })
    .to_string()
}

fn strip_file_status(file: &StripFileOutput) -> &'static str {
    if !file.result.errors.is_empty() {
        "error"
    } else if file.dry_run && file.result.stripped {
        "would_strip"
    } else if !file.result.stripped {
        "unchanged"
    } else if file.result.verified {
        "verified"
    } else {
        "stripped"
    }
}

#[derive(Debug, Clone, Default)]
struct WriteRequest {
    metadata: Option<PathBuf>,
    targets: Option<String>,
}

impl WriteRequest {
    fn from_args(args: &WriteArgs) -> Self {
        match (&args.metadata_or_targets, &args.targets) {
            (Some(metadata), Some(targets)) => Self {
                metadata: Some(metadata.clone()),
                targets: Some(targets.clone()),
            },
            (Some(single), None) if looks_like_metadata_path(single) => Self {
                metadata: Some(single.clone()),
                targets: None,
            },
            (Some(single), None) => Self {
                metadata: None,
                targets: Some(path_to_pattern(single)),
            },
            (None, Some(targets)) => Self {
                metadata: None,
                targets: Some(targets.clone()),
            },
            (None, None) => Self::default(),
        }
    }
}

fn looks_like_metadata_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(extension) if extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml")
    ) || path.is_dir()
}

fn path_to_pattern(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn path_parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn resolve_write_targets(
    metadata_dir: &Path,
    target_pattern: Option<&str>,
    recursive: bool,
    extensions: &[String],
) -> Result<Vec<PathBuf>, String> {
    let allowed_extensions = normalized_extensions(extensions);
    let mut targets = if let Some(pattern) = target_pattern {
        resolve_explicit_targets(metadata_dir, pattern)?
    } else {
        supported_image_files(metadata_dir, recursive)?
    };

    targets.retain(|path| {
        is_supported_image_file(path)
            && (allowed_extensions.is_empty()
                || file_extension(path)
                    .is_some_and(|extension| allowed_extensions.contains(&extension)))
    });
    targets.sort_by_key(|path| path_to_pattern(path));
    targets.dedup();
    Ok(targets)
}

fn normalized_extensions(extensions: &[String]) -> HashSet<String> {
    extensions
        .iter()
        .filter_map(|extension| {
            let normalized = extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect()
}

fn resolve_explicit_targets(metadata_dir: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Ok(Vec::new());
    }

    let candidate = PathBuf::from(pattern);
    let absolute_candidate = if candidate.is_absolute() {
        candidate.clone()
    } else {
        metadata_dir.join(&candidate)
    };

    if !contains_glob(pattern) {
        if absolute_candidate.is_dir() {
            return supported_image_files(&absolute_candidate, false);
        }
        return Ok(absolute_candidate
            .is_file()
            .then_some(absolute_candidate)
            .into_iter()
            .collect());
    }

    let recursive = pattern.split(['/', '\\']).any(|part| part == "**");
    let candidates = supported_image_files(metadata_dir, recursive)?;
    let normalized_pattern = pattern.replace('\\', "/");

    Ok(candidates
        .into_iter()
        .filter(|path| {
            let subject = if Path::new(pattern).is_absolute() {
                path_to_pattern(path)
            } else {
                path.strip_prefix(metadata_dir)
                    .map(path_to_pattern)
                    .unwrap_or_else(|_| path_to_pattern(path))
            };
            glob_matches(&normalized_pattern, &subject)
        })
        .collect())
}

fn contains_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn supported_image_files(directory: &Path, recursive: bool) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    collect_supported_image_files(directory, recursive, &mut paths)?;
    paths.sort_by_key(|path| file_name(path).to_ascii_lowercase());
    Ok(paths)
}

fn collect_supported_image_files(
    directory: &Path,
    recursive: bool,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read directory {}: {error}", directory.display()))?;

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if path.is_file() && is_supported_image_file(&path) {
            paths.push(path);
        } else if recursive && path.is_dir() {
            collect_supported_image_files(&path, recursive, paths)?;
        }
    }

    Ok(())
}

fn glob_matches(pattern: &str, subject: &str) -> bool {
    let pattern_parts = pattern.split('/').collect::<Vec<_>>();
    let subject_parts = subject.split('/').collect::<Vec<_>>();
    glob_parts_match(&pattern_parts, &subject_parts)
}

fn glob_parts_match(pattern: &[&str], subject: &[&str]) -> bool {
    if pattern.is_empty() {
        return subject.is_empty();
    }

    if pattern[0] == "**" {
        return glob_parts_match(&pattern[1..], subject)
            || (!subject.is_empty() && glob_parts_match(pattern, &subject[1..]));
    }

    !subject.is_empty()
        && wildcard_match(pattern[0].as_bytes(), subject[0].as_bytes())
        && glob_parts_match(&pattern[1..], &subject[1..])
}

fn wildcard_match(pattern: &[u8], subject: &[u8]) -> bool {
    if pattern.is_empty() {
        return subject.is_empty();
    }

    match pattern[0] {
        b'*' => {
            wildcard_match(&pattern[1..], subject)
                || (!subject.is_empty() && wildcard_match(pattern, &subject[1..]))
        }
        b'?' => !subject.is_empty() && wildcard_match(&pattern[1..], &subject[1..]),
        byte => {
            !subject.is_empty()
                && byte.eq_ignore_ascii_case(&subject[0])
                && wildcard_match(&pattern[1..], &subject[1..])
        }
    }
}

#[derive(Debug, Clone)]
struct WriteTag {
    name: String,
    value: YamlValue,
}

#[derive(Debug, Clone, Default)]
struct WritePlan {
    global: Vec<WriteTag>,
    frames: BTreeMap<PathBuf, WriteFramePlan>,
}

impl WritePlan {
    fn frame_for_image(&self, image: &Path) -> Option<WriteFramePlan> {
        let frame = self.frames.get(image);
        if self.global.is_empty() && frame.is_none_or(|frame| frame.tags.is_empty()) {
            return None;
        }

        let mut merged = self.global.clone();
        if let Some(frame) = frame {
            for tag in &frame.tags {
                if let Some(existing) = merged.iter_mut().find(|existing| existing.name == tag.name)
                {
                    *existing = tag.clone();
                } else {
                    merged.push(tag.clone());
                }
            }
        }
        normalize_iso_aliases(&mut merged);
        Some(WriteFramePlan {
            label: frame
                .map(|frame| frame.label.clone())
                .unwrap_or_else(|| write_file_heading(image)),
            tags: merged,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct WriteFramePlan {
    label: String,
    tags: Vec<WriteTag>,
}

fn normalize_iso_aliases(tags: &mut Vec<WriteTag>) {
    let Some(value) = tags
        .iter()
        .rev()
        .find(|tag| is_iso_alias(&tag.name))
        .map(|tag| tag.value.clone())
    else {
        return;
    };

    tags.retain(|tag| !is_iso_alias(&tag.name));
    for name in ["ISO", "ISOSpeed", "ISOSpeedRatings"] {
        tags.push(WriteTag {
            name: name.to_string(),
            value: value.clone(),
        });
    }
}

fn is_iso_alias(name: &str) -> bool {
    matches!(name, "ISO" | "ISOSpeed" | "ISOSpeedRatings")
}

fn build_write_plan(
    metadata_path: &Path,
    yaml: &YamlValue,
    targets: &[PathBuf],
) -> Result<WritePlan, String> {
    let root = yaml
        .as_mapping()
        .ok_or_else(|| "metadata YAML root must be a mapping".to_string())?;
    let mut plan = WritePlan::default();

    if let Some(exif) = yaml_mapping_get(root, "exif") {
        plan.global = collect_write_tags_from_mapping(
            exif.as_mapping()
                .ok_or_else(|| "metadata YAML `exif` key must be a mapping".to_string())?,
        )?;
    }

    if let Some(frames) = yaml_mapping_get(root, "frames") {
        let empty_frames = Mapping::new();
        let frames = match frames {
            YamlValue::Null => &empty_frames,
            YamlValue::Mapping(frames) => frames,
            _ => return Err("metadata YAML `frames` key must be a mapping".to_string()),
        };
        let image_directory = path_parent_or_current(metadata_path);
        for (frame_key, frame_value) in frames {
            let Some(image) = resolve_write_frame_target(frame_key, image_directory, targets)
            else {
                continue;
            };
            plan.frames.insert(
                image.clone(),
                WriteFramePlan {
                    label: write_frame_label(frame_key, &image),
                    tags: collect_write_tags_from_frame_value(frame_value)?,
                },
            );
        }
    }

    Ok(plan)
}

fn write_frame_label(frame_key: &YamlValue, image: &Path) -> String {
    if let Some(frame_number) = frame_number_from_key(frame_key) {
        return format!("{} ← frame {frame_number}", write_file_heading(image));
    }

    write_file_heading(image)
}

fn resolve_write_frame_target(
    frame_key: &YamlValue,
    image_directory: &Path,
    targets: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(frame_number) = frame_number_from_key(frame_key) {
        return targets.get(frame_number.checked_sub(1)?).cloned();
    }

    let file_name = frame_key.as_str()?;
    let absolute = image_directory.join(file_name);
    targets.iter().find(|target| **target == absolute).cloned()
}

fn collect_write_tags_from_frame_value(value: &YamlValue) -> Result<Vec<WriteTag>, String> {
    match value {
        YamlValue::Mapping(mapping) => collect_write_tags_from_mapping(mapping),
        YamlValue::Sequence(items) => {
            let mut tags = Vec::new();
            for item in items {
                let mapping = item
                    .as_mapping()
                    .ok_or_else(|| "metadata YAML frame entries must be mappings".to_string())?;
                if mapping.len() != 1 {
                    return Err(
                        "metadata YAML frame sequence entries must contain one tag".to_string()
                    );
                }
                tags.extend(collect_write_tags_from_mapping(mapping)?);
            }
            Ok(tags)
        }
        YamlValue::Null => Ok(Vec::new()),
        _ => Err("metadata YAML frame values must be mappings or sequences".to_string()),
    }
}

fn collect_write_tags_from_mapping(mapping: &Mapping) -> Result<Vec<WriteTag>, String> {
    let mut tags = Vec::new();
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err("metadata YAML tag keys must be strings".to_string());
        };
        tags.push(WriteTag {
            name: name.to_string(),
            value: value.clone(),
        });
    }
    Ok(tags)
}

#[derive(Debug, Clone, Default)]
struct WriteFileResult {
    written: usize,
    strip_attempted: bool,
    stripped: bool,
    removed_tags: usize,
    skipped: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct WriteSummary {
    written_tags: usize,
    stripped_files: usize,
    removed_tags: usize,
    skipped_tags: usize,
    skipped_files: usize,
    warnings: usize,
    errors: usize,
    elapsed_ms: u128,
}

impl WriteSummary {
    fn add(&mut self, result: &WriteFileResult) {
        self.written_tags += result.written;
        self.stripped_files += usize::from(result.stripped);
        self.removed_tags += result.removed_tags;
        self.skipped_tags += result.skipped.len();
        self.warnings += result.warnings.len() + result.skipped.len();
        self.errors += result.errors.len();
    }
}

#[derive(Debug, Clone)]
struct WriteOutput {
    metadata_path: PathBuf,
    dry_run: bool,
    target_count: usize,
    file_warnings: Vec<String>,
    files: Vec<WriteFileOutput>,
    skipped_files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct WriteFileOutput {
    label: String,
    image: PathBuf,
    result: WriteFileResult,
    elapsed_ms: u128,
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpinnerPreset {
    Dots,
    Pulse,
    FillSweep,
    DiagSwipe,
    Cascade,
    Columns,
    Sand,
    WaveRows,
    Scan,
}

const SPINNER_PRESETS: [SpinnerPreset; 9] = [
    SpinnerPreset::Dots,
    SpinnerPreset::Pulse,
    SpinnerPreset::FillSweep,
    SpinnerPreset::DiagSwipe,
    SpinnerPreset::Cascade,
    SpinnerPreset::Columns,
    SpinnerPreset::Sand,
    SpinnerPreset::WaveRows,
    SpinnerPreset::Scan,
];

impl SpinnerPreset {
    fn random() -> Self {
        let entropy = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        SPINNER_PRESETS[entropy as usize % SPINNER_PRESETS.len()]
    }

    fn frame(self) -> &'static str {
        match self {
            Self::Dots => spinners::dots().current_frame(),
            Self::Pulse => spinners::pulse().current_frame(),
            Self::FillSweep => spinners::fillsweep().current_frame(),
            Self::DiagSwipe => spinners::diagswipe().current_frame(),
            Self::Cascade => spinners::cascade().current_frame(),
            Self::Columns => spinners::columns().current_frame(),
            Self::Sand => spinners::sand().current_frame(),
            Self::WaveRows => spinners::waverows().current_frame(),
            Self::Scan => spinners::scan().current_frame(),
        }
    }

    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Self::Dots => "dots",
            Self::Pulse => "pulse",
            Self::FillSweep => "fillsweep",
            Self::DiagSwipe => "diagswipe",
            Self::Cascade => "cascade",
            Self::Columns => "columns",
            Self::Sand => "sand",
            Self::WaveRows => "waverows",
            Self::Scan => "scan",
        }
    }
}

struct TerminalSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    clear_width: usize,
}

impl TerminalSpinner {
    fn start(preset: SpinnerPreset, label: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let clear_width = label.len() + 16;
        print!("\r{} {}", preset.frame(), label);
        flush_stdout();
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                print!("\r{} {}", preset.frame(), label);
                flush_stdout();
                thread::sleep(Duration::from_millis(80));
            }
        });

        Self {
            stop,
            handle: Some(handle),
            clear_width,
        }
    }

    fn finish(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
            clear_spinner_line(self.clear_width);
        }
    }
}

impl Drop for TerminalSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
            clear_spinner_line(self.clear_width);
        }
    }
}

fn clear_spinner_line(width: usize) {
    print!("\r{}\r", " ".repeat(width));
    flush_stdout();
}

fn flush_stdout() {
    let _ = std::io::stdout().flush();
}

fn apply_tags_to_image(
    image: &Path,
    tags: &[WriteTag],
    dry_run: bool,
    args: &WriteArgs,
    strip_mode: Option<&StripMode>,
) -> WriteFileResult {
    let mut result = WriteFileResult::default();
    let mut writable_tags = Vec::new();
    let mut custom_tags = Vec::new();

    for tag in tags {
        if is_blank_yaml_value(&tag.value) {
            continue;
        }

        if tag.name == "$Location" {
            match expand_location_tag(&tag.value) {
                Ok(expanded) => writable_tags.extend(expanded),
                Err(WriteTagError::Ignored) => {}
                Err(WriteTagError::Warning(message)) => result.warnings.push(message),
            }
        } else if let Some(tag) = writable_exif_tag(&tag.name, &tag.value) {
            writable_tags.push(tag);
        } else {
            custom_tags.push(CustomTag {
                name: tag.name.clone(),
                value: tag.value.clone(),
            });
        }
    }
    dedupe_writable_tags(&mut writable_tags);
    if !custom_tags.is_empty() {
        match encode_custom_tags(&custom_tags) {
            Ok(payload) => writable_tags.push(WritableExifTag::UserComment(payload)),
            Err(error) => result.errors.push(error),
        }
    }

    if let Some(strip_mode) = strip_mode {
        let strip_result = strip_metadata_from_image(image, dry_run, false, strip_mode);
        result.strip_attempted = true;
        result.stripped = strip_result.stripped;
        result.removed_tags = strip_result.removed_tags;
        result.warnings.extend(strip_result.warnings);
        result.errors.extend(strip_result.errors);
        if !result.errors.is_empty() {
            return result;
        }
    }

    let mut metadata =
        WritableMetadata::new_from_path(image).unwrap_or_else(|_| WritableMetadata::new());
    if !custom_tags.is_empty()
        && metadata
            .get_tag(&WritableExifTag::UserComment(Vec::new()))
            .any(|tag| !writable_user_comment_has_custom_payload(tag))
    {
        result
            .warnings
            .push("replacing existing non-exifmeta UserComment".to_string());
    }

    let mut pending_written = 0;
    for tag in writable_tags {
        let is_custom_payload = matches!(tag, WritableExifTag::UserComment(_));
        if args.no_overwrite && !is_custom_payload && metadata.get_tag(&tag).next().is_some() {
            result.skipped.push(format!(
                "{} already exists",
                writable_tag_name(&tag).unwrap_or("EXIF tag")
            ));
            continue;
        }

        metadata.set_tag(tag);
        pending_written += if is_custom_payload {
            custom_tags.len()
        } else {
            1
        };
    }

    if dry_run {
        result.written = pending_written;
        return result;
    }

    if pending_written > 0 {
        if let Err(error) = metadata.write_to_file(image) {
            result
                .errors
                .push(format!("failed to write EXIF metadata: {error}"));
        } else {
            result.written = pending_written;
        }
    }

    result
}

fn writable_user_comment_has_custom_payload(tag: &WritableExifTag) -> bool {
    matches!(tag, WritableExifTag::UserComment(bytes) if custom_tags_from_bytes(bytes).is_some())
}

fn dedupe_writable_tags(tags: &mut Vec<WritableExifTag>) {
    let mut deduped: Vec<WritableExifTag> = Vec::new();
    for tag in tags.drain(..) {
        if let Some(existing) = deduped
            .iter_mut()
            .find(|existing| writable_tag_identity(existing) == writable_tag_identity(&tag))
        {
            *existing = tag;
        } else {
            deduped.push(tag);
        }
    }
    *tags = deduped;
}

fn writable_tag_identity(tag: &WritableExifTag) -> (u16, String) {
    (tag.as_u16(), format!("{:?}", tag.get_group()))
}

#[cfg(test)]
fn format_write_output(output: &WriteOutput, summary: &WriteSummary) -> String {
    let mut rendered = String::new();
    rendered.push_str(&format_write_metadata_output(output));
    rendered.push_str(&format_write_frames_heading());
    for file in &output.files {
        rendered.push_str(&format_write_file_output(file));
    }
    for skipped in &output.skipped_files {
        rendered.push_str(&format_write_skipped_file_output(skipped));
    }
    rendered.push_str(&format_write_overview_output(summary));

    rendered
}

fn format_write_metadata_output(output: &WriteOutput) -> String {
    let mut rendered = String::new();
    let mut first_group = true;
    append_write_metadata_group(&mut rendered, &mut first_group, output);
    rendered
}

fn format_write_frames_heading() -> String {
    let mut rendered = String::new();
    let mut first_group = false;
    append_spaced_check_heading(&mut rendered, &mut first_group, "frames");
    rendered
}

#[cfg(test)]
fn format_write_file_output(file: &WriteFileOutput) -> String {
    let mut rendered = String::new();
    append_write_file_header(&mut rendered, file);
    append_write_file_result(&mut rendered, file);
    rendered
}

fn format_write_file_header_output(file: &WriteFileOutput) -> String {
    let mut rendered = String::new();
    append_write_file_header(&mut rendered, file);
    rendered
}

fn format_write_file_result_output(file: &WriteFileOutput) -> String {
    let mut rendered = String::new();
    append_write_file_result(&mut rendered, file);
    rendered
}

fn format_write_skipped_file_output(image: &Path) -> String {
    let mut rendered = String::new();
    append_write_skipped_file_group(&mut rendered, image);
    rendered
}

fn format_write_overview_output(summary: &WriteSummary) -> String {
    let mut rendered = String::new();
    let mut first_group = false;
    append_write_overview_group(&mut rendered, &mut first_group, summary);
    rendered
}

fn append_write_metadata_group(
    rendered: &mut String,
    first_group: &mut bool,
    output: &WriteOutput,
) {
    append_spaced_check_heading(rendered, first_group, "write");
    rendered.push_str(&format!(
        "metadata file: {}\n",
        output.metadata_path.display()
    ));
    rendered.push_str(&format!("targets: {}\n", output.target_count));
    if output.dry_run {
        rendered.push_str(&format!("mode: {}\n", "dry-run".yellow()));
    }
    for warning in &output.file_warnings {
        rendered.push_str(&format!("{}\n", format_check_warning(warning)));
    }
}

fn append_write_file_header(rendered: &mut String, file: &WriteFileOutput) {
    append_write_frame_subtitle(rendered, &file.label);
    append_write_file_path(rendered, &file.image);
}

fn append_write_file_result(rendered: &mut String, file: &WriteFileOutput) {
    if file.result.strip_attempted {
        let action = if file.dry_run {
            "would strip EXIF"
        } else {
            "stripped EXIF"
        };
        rendered.push_str(&format!("{action}: {} tags\n", file.result.removed_tags));
    }
    let action = if file.dry_run { "would write" } else { "wrote" };
    rendered.push_str(&format!("{action} {} tags\n", file.result.written));
    rendered.push_str(&format!(
        "took {}\n",
        format_write_duration(file.elapsed_ms)
    ));
    for skipped in &file.result.skipped {
        rendered.push_str(&format!(
            "{}\n",
            format_check_warning(&format!("skipped {skipped}"))
        ));
    }
    for warning in &file.result.warnings {
        rendered.push_str(&format!("{}\n", format_check_warning(warning)));
    }
    for error in &file.result.errors {
        rendered.push_str(&format!("{}\n", format_check_error(error)));
    }
}

fn append_write_skipped_file_group(rendered: &mut String, image: &Path) {
    append_write_frame_subtitle(rendered, &write_file_heading(image));
    append_write_file_path(rendered, image);
    rendered.push_str("skipped: no metadata\n");
}

fn append_write_frame_subtitle(rendered: &mut String, label: &str) {
    rendered.push_str(&format!("{}\n", label.bright_cyan()));
}

fn write_file_heading(image: &Path) -> String {
    file_name(image)
}

fn append_write_file_path(rendered: &mut String, image: &Path) {
    if !is_current_directory_file(image) {
        rendered.push_str(&format!("file: {}\n", image.display()));
    }
}

fn is_current_directory_file(image: &Path) -> bool {
    image
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty() || parent == Path::new("."))
}

fn append_write_overview_group(
    rendered: &mut String,
    first_group: &mut bool,
    summary: &WriteSummary,
) {
    append_spaced_check_heading(rendered, first_group, "overview");
    append_write_overview_row(rendered, "errors", summary.errors);
    append_write_overview_row(rendered, "warnings", summary.warnings);
    append_write_overview_row(rendered, "written", summary.written_tags);
    append_write_overview_row(rendered, "stripped", summary.stripped_files);
    append_write_overview_row(rendered, "removed tags", summary.removed_tags);
    append_write_overview_row(rendered, "skipped", summary.skipped_tags);
    append_write_overview_row(rendered, "files skipped", summary.skipped_files);
    append_write_overview_row(rendered, "took", format_write_duration(summary.elapsed_ms));
    if summary.errors > 0 {
        append_write_overview_row(rendered, "status", "fail".red());
    } else if summary.warnings > 0 {
        append_write_overview_row(
            rendered,
            "status",
            format!("{} {}", "success".green(), "(with warnings)"),
        );
    } else {
        append_write_overview_row(rendered, "status", "success".green());
    }
}

fn append_write_overview_row(rendered: &mut String, label: &str, value: impl std::fmt::Display) {
    rendered.push_str(&format!("{label:<14} {value}\n"));
}

fn format_write_duration(elapsed_ms: u128) -> String {
    if elapsed_ms > 1500 {
        format!("{:.1}s", elapsed_ms as f64 / 1000.0)
    } else {
        format!("{elapsed_ms}ms")
    }
}

enum WriteTagError {
    Ignored,
    Warning(String),
}

fn is_blank_yaml_value(value: &YamlValue) -> bool {
    matches!(value, YamlValue::Null)
        || matches!(value, YamlValue::String(value) if value.trim().is_empty())
}

fn expand_location_tag(value: &YamlValue) -> Result<Vec<WritableExifTag>, WriteTagError> {
    let Some(location_name) = location_name_from_yaml(value, &mut Vec::new()) else {
        return Err(WriteTagError::Ignored);
    };
    let geonames = open_embedded_geonames_database()
        .map_err(|error| WriteTagError::Warning(format!("$Location lookup failed: {error}")))?;
    let locations = locations_by_name(&geonames, location_name)
        .map_err(|error| WriteTagError::Warning(format!("$Location lookup failed: {error}")))?;
    let Some(location) = locations.first() else {
        return Err(WriteTagError::Warning(format!(
            "$Location: no match found in database [for <{location_name}>]"
        )));
    };

    Ok(gps_tags(location.latitude, location.longitude, None))
}

fn gps_tags(latitude: f64, longitude: f64, altitude: Option<f64>) -> Vec<WritableExifTag> {
    let mut tags = vec![
        WritableExifTag::GPSLatitudeRef(if latitude < 0.0 { "S" } else { "N" }.to_string()),
        WritableExifTag::GPSLatitude(decimal_to_dms_rational(latitude.abs())),
        WritableExifTag::GPSLongitudeRef(if longitude < 0.0 { "W" } else { "E" }.to_string()),
        WritableExifTag::GPSLongitude(decimal_to_dms_rational(longitude.abs())),
        WritableExifTag::GPSMapDatum("WGS-84".to_string()),
    ];

    if let Some(altitude) = altitude {
        tags.push(WritableExifTag::GPSAltitudeRef(vec![if altitude < 0.0 {
            1
        } else {
            0
        }]));
        tags.push(WritableExifTag::GPSAltitude(vec![rational(altitude.abs())]));
    }

    tags
}

fn decimal_to_dms_rational(decimal: f64) -> Vec<uR64> {
    let degrees = decimal.trunc();
    let minutes_float = (decimal - degrees) * 60.0;
    let minutes = minutes_float.trunc();
    let seconds = (minutes_float - minutes) * 60.0;

    vec![
        rational(degrees),
        rational(minutes),
        rational_with_denominator(seconds, 10_000),
    ]
}

fn writable_exif_tag(name: &str, value: &YamlValue) -> Option<WritableExifTag> {
    let tag = match name {
        "Artist" | "Photographer" => WritableExifTag::Artist(yaml_string(value)?),
        "Copyright" => WritableExifTag::Copyright(yaml_string(value)?),
        "CreateDate" => WritableExifTag::CreateDate(yaml_datetime(value)?),
        "DateTimeOriginal" => WritableExifTag::DateTimeOriginal(yaml_datetime(value)?),
        "ExposureProgram" => WritableExifTag::ExposureProgram(vec![yaml_u16(value)?]),
        "ExposureTime" => WritableExifTag::ExposureTime(vec![yaml_rational(value)?]),
        "FNumber" => WritableExifTag::FNumber(vec![yaml_rational(value)?]),
        "FileSource" => WritableExifTag::FileSource(vec![yaml_u8(value)?]),
        "Flash" => WritableExifTag::Flash(vec![yaml_u16(value)?]),
        "FocalLength" => WritableExifTag::FocalLength(vec![yaml_rational(value)?]),
        "GPSAltitude" => WritableExifTag::GPSAltitude(vec![yaml_rational(value)?]),
        "GPSAltitudeRef" => WritableExifTag::GPSAltitudeRef(vec![yaml_u8(value)?]),
        "GPSLatitude" => {
            WritableExifTag::GPSLatitude(decimal_to_dms_rational(yaml_f64(value)?.abs()))
        }
        "GPSLatitudeRef" => WritableExifTag::GPSLatitudeRef(yaml_string(value)?),
        "GPSLongitude" => {
            WritableExifTag::GPSLongitude(decimal_to_dms_rational(yaml_f64(value)?.abs()))
        }
        "GPSLongitudeRef" => WritableExifTag::GPSLongitudeRef(yaml_string(value)?),
        "GPSMapDatum" => WritableExifTag::GPSMapDatum(yaml_string(value)?),
        "ISO" | "ISOSpeedRatings" => WritableExifTag::ISO(vec![yaml_u16(value)?]),
        "ISOSpeed" => WritableExifTag::ISOSpeed(vec![yaml_u32(value)?]),
        "ImageDescription" => WritableExifTag::ImageDescription(yaml_string(value)?),
        "LensMake" => WritableExifTag::LensMake(yaml_string(value)?),
        "LensModel" => WritableExifTag::LensModel(yaml_string(value)?),
        "LightSource" => WritableExifTag::LightSource(vec![yaml_u16(value)?]),
        "Make" => WritableExifTag::Make(yaml_string(value)?),
        "MaxApertureValue" => WritableExifTag::MaxApertureValue(vec![yaml_rational(value)?]),
        "MeteringMode" => WritableExifTag::MeteringMode(vec![yaml_u16(value)?]),
        "Model" => WritableExifTag::Model(yaml_string(value)?),
        "ModifyDate" => WritableExifTag::ModifyDate(yaml_datetime(value)?),
        "Orientation" => WritableExifTag::Orientation(vec![yaml_u16(value)?]),
        "Software" => WritableExifTag::Software(yaml_string(value)?),
        "WhiteBalance" => WritableExifTag::WhiteBalance(vec![yaml_u16(value)?]),
        _ => return None,
    };

    Some(tag)
}

fn writable_tag_name(tag: &WritableExifTag) -> Option<&'static str> {
    match tag {
        WritableExifTag::Artist(_) => Some("Artist"),
        WritableExifTag::Copyright(_) => Some("Copyright"),
        WritableExifTag::CreateDate(_) => Some("CreateDate"),
        WritableExifTag::DateTimeOriginal(_) => Some("DateTimeOriginal"),
        WritableExifTag::ExposureProgram(_) => Some("ExposureProgram"),
        WritableExifTag::ExposureTime(_) => Some("ExposureTime"),
        WritableExifTag::FNumber(_) => Some("FNumber"),
        WritableExifTag::FileSource(_) => Some("FileSource"),
        WritableExifTag::Flash(_) => Some("Flash"),
        WritableExifTag::FocalLength(_) => Some("FocalLength"),
        WritableExifTag::GPSAltitude(_) => Some("GPSAltitude"),
        WritableExifTag::GPSAltitudeRef(_) => Some("GPSAltitudeRef"),
        WritableExifTag::GPSLatitude(_) => Some("GPSLatitude"),
        WritableExifTag::GPSLatitudeRef(_) => Some("GPSLatitudeRef"),
        WritableExifTag::GPSLongitude(_) => Some("GPSLongitude"),
        WritableExifTag::GPSLongitudeRef(_) => Some("GPSLongitudeRef"),
        WritableExifTag::GPSMapDatum(_) => Some("GPSMapDatum"),
        WritableExifTag::ISO(_) => Some("ISO"),
        WritableExifTag::ISOSpeed(_) => Some("ISOSpeed"),
        WritableExifTag::ImageDescription(_) => Some("ImageDescription"),
        WritableExifTag::LensMake(_) => Some("LensMake"),
        WritableExifTag::LensModel(_) => Some("LensModel"),
        WritableExifTag::LightSource(_) => Some("LightSource"),
        WritableExifTag::Make(_) => Some("Make"),
        WritableExifTag::MaxApertureValue(_) => Some("MaxApertureValue"),
        WritableExifTag::MeteringMode(_) => Some("MeteringMode"),
        WritableExifTag::Model(_) => Some("Model"),
        WritableExifTag::ModifyDate(_) => Some("ModifyDate"),
        WritableExifTag::Orientation(_) => Some("Orientation"),
        WritableExifTag::Software(_) => Some("Software"),
        WritableExifTag::WhiteBalance(_) => Some("WhiteBalance"),
        _ => None,
    }
}

fn yaml_string(value: &YamlValue) -> Option<String> {
    match value {
        YamlValue::Null => None,
        YamlValue::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        }
        YamlValue::Number(value) => Some(value.to_string()),
        YamlValue::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn yaml_datetime(value: &YamlValue) -> Option<String> {
    let value = yaml_string(value)?;
    if value.len() == 10 && value.as_bytes().get(4) == Some(&b'-') {
        Some(format!(
            "{}:{}:{} 00:00:00",
            &value[0..4],
            &value[5..7],
            &value[8..10]
        ))
    } else {
        Some(value.replace('-', ":"))
    }
}

fn yaml_u8(value: &YamlValue) -> Option<u8> {
    u8::try_from(yaml_u32(value)?).ok()
}

fn yaml_u16(value: &YamlValue) -> Option<u16> {
    u16::try_from(yaml_u32(value)?).ok()
}

fn yaml_u32(value: &YamlValue) -> Option<u32> {
    match value {
        YamlValue::Number(number) => number.as_u64().and_then(|value| u32::try_from(value).ok()),
        YamlValue::String(value) => clean_numeric_string(value).parse::<u32>().ok(),
        YamlValue::Bool(value) => Some(u32::from(*value)),
        _ => None,
    }
}

fn yaml_f64(value: &YamlValue) -> Option<f64> {
    match value {
        YamlValue::Number(number) => number.as_f64(),
        YamlValue::String(value) => parse_number_or_fraction(value),
        _ => None,
    }
}

fn yaml_rational(value: &YamlValue) -> Option<uR64> {
    if let YamlValue::String(value) = value {
        let value = clean_numeric_string(value);
        if let Some((numerator, denominator)) = value.split_once('/') {
            let numerator = clean_numeric_string(numerator).parse::<u32>().ok()?;
            let denominator = clean_numeric_string(denominator).parse::<u32>().ok()?;
            return (denominator != 0).then_some(uR64 {
                nominator: numerator,
                denominator,
            });
        }
    }

    yaml_f64(value).map(rational)
}

fn clean_numeric_string(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix("f/")
        .or_else(|| value.strip_prefix("F/"))
        .unwrap_or(value);

    value
        .trim_end_matches("mm")
        .trim_end_matches("MM")
        .trim()
        .to_string()
}

fn parse_number_or_fraction(value: &str) -> Option<f64> {
    let value = clean_numeric_string(value);
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.trim().parse::<f64>().ok()?;
        let denominator = denominator.trim().parse::<f64>().ok()?;
        return (denominator != 0.0).then_some(numerator / denominator);
    }
    value.parse::<f64>().ok()
}

fn rational(value: f64) -> uR64 {
    rational_with_denominator(value, 1_000_000)
}

fn rational_with_denominator(value: f64, denominator: u32) -> uR64 {
    let value = value.max(0.0);
    uR64 {
        nominator: (value * f64::from(denominator)).round() as u32,
        denominator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use exif::{Context, Tag};

    #[test]
    fn interactive_entries_include_folders_and_supported_files_only() {
        let directory = temporary_test_directory("interactive-entries");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested directory should be created");
        std::fs::write(directory.join("a.txt"), "not an image")
            .expect("text file should be written");
        std::fs::write(directory.join("b.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");
        std::fs::write(directory.join("c.tif"), [0x49, 0x49, 0x2a, 0x00])
            .expect("tif should be written");

        let entries = interactive_entries(&directory).expect("entries should be listed");
        let labels = entries
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>();

        assert_eq!(labels, ["../", "nested/", "b.jpg", "c.tif"]);
        assert_eq!(entries[0].kind, InteractiveEntryKind::Parent);
        assert_eq!(entries[1].kind, InteractiveEntryKind::Directory);
        assert_eq!(entries[2].kind, InteractiveEntryKind::File);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_app_navigates_into_child_and_back_to_parent() {
        let directory = temporary_test_directory("interactive-navigation");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested directory should be created");
        std::fs::write(nested.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "nested/")
            .expect("nested entry should exist");
        app.open_selected().expect("selected folder should open");

        assert_eq!(app.current_dir, fs::canonicalize(&nested).unwrap());
        assert_eq!(app.selected, 0);
        assert!(app.entries.iter().any(|entry| entry.label == "image.jpg"));

        app.open_parent().expect("parent should open");

        assert_eq!(app.current_dir, fs::canonicalize(&directory).unwrap());
        assert_eq!(app.entries[app.selected].label, "nested/");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_app_selection_updates_preview() {
        let directory = temporary_test_directory("interactive-preview");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "image.jpg")
            .expect("image entry should exist");
        app.preview = InteractivePreviewContent::plain("previous preview");
        app.request_preview_for_selection();

        assert!(app.preview_loading);
        assert!(!app.preview_loading_visible);
        assert_eq!(app.preview.as_plain(), Some("previous preview"));

        let result = preview_result_for_selected(&app, "<No EXIF metadata found>");
        app.apply_preview_result(result);

        assert!(!app.preview_loading);
        assert!(!app.preview_loading_visible);
        assert!(
            app.preview
                .as_plain()
                .is_some_and(|preview| preview.contains("<No EXIF metadata found>"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_read_preview_reports_missing_file_errors() {
        let missing = temporary_test_path("interactive-missing.jpg");

        let preview = read_preview(&missing);
        let preview = preview
            .as_plain()
            .expect("missing image preview should be plain text");

        assert!(preview.contains("Failed to read"));
        assert!(preview.contains("failed to open"));
    }

    #[test]
    fn interactive_read_preview_preserves_read_colours() {
        let directory = temporary_test_directory("interactive-read-colours");
        let image = directory.join("image.jpg");
        std::fs::write(&image, [0xff, 0xd8, 0xff, 0xd9]).expect("jpg should be written");

        let preview = read_preview(&image);
        let lines = preview
            .as_lines()
            .expect("read preview should render styled lines");

        assert_eq!(line_text(&lines[0]), "<No EXIF metadata found>");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn ansi_preview_lines_preserve_read_styles() {
        let lines = ansi_to_preview_lines(
            "\u{1b}[94mblue\u{1b}[0m plain\n\u{1b}[96mcyan\u{1b}[0m \u{1b}[33myellow\u{1b}[0m",
        );

        assert_eq!(line_text(&lines[0]), "blue plain");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::LightBlue));
        assert_eq!(lines[0].spans[1].style, Style::default());
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::LightCyan));
        assert_eq!(lines[1].spans[1].style, Style::default());
        assert_eq!(lines[1].spans[2].style.fg, Some(Color::Yellow));
    }

    #[test]
    fn interactive_app_preserves_valid_selection_after_directory_refresh() {
        let directory = temporary_test_directory("interactive-selection");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested directory should be created");
        std::fs::write(directory.join("z.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = usize::MAX;
        app.open_directory(nested)
            .expect("nested directory should open");

        assert_eq!(app.selected, 0);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_key_press_moves_selection_once() {
        let directory = temporary_test_directory("interactive-key-press");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("first jpg should be written");
        std::fs::write(directory.join("b.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("second jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        let start = app.selected;

        app.handle_event(key_event(KeyCode::Down, KeyEventKind::Press))
            .expect("key press should be handled");

        assert_eq!(app.selected, start + 1);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_key_release_does_not_move_selection() {
        let directory = temporary_test_directory("interactive-key-release");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("first jpg should be written");
        std::fs::write(directory.join("b.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("second jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        let start = app.selected;

        app.handle_event(key_event(KeyCode::Down, KeyEventKind::Release))
            .expect("key release should be ignored");

        assert_eq!(app.selected, start);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_key_repeat_does_not_move_selection() {
        let directory = temporary_test_directory("interactive-key-repeat");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("first jpg should be written");
        std::fs::write(directory.join("b.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("second jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        let start = app.selected;

        app.handle_event(key_event(KeyCode::Down, KeyEventKind::Repeat))
            .expect("key repeat should be ignored");

        assert_eq!(app.selected, start);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_navigation_keys_still_open_child_and_parent_on_press() {
        let directory = temporary_test_directory("interactive-key-navigation");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested directory should be created");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "nested/")
            .expect("nested entry should exist");

        app.handle_event(key_event(KeyCode::Right, KeyEventKind::Press))
            .expect("right press should open child");
        assert_eq!(app.current_dir, fs::canonicalize(&nested).unwrap());

        app.handle_event(key_event(KeyCode::Left, KeyEventKind::Press))
            .expect("left press should open parent");
        assert_eq!(app.current_dir, fs::canonicalize(&directory).unwrap());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_right_on_file_focuses_preview_without_changing_directory() {
        let directory = temporary_test_directory("interactive-file-focus");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "image.jpg")
            .expect("image entry should exist");
        app.request_preview_for_selection();
        let start_dir = app.current_dir.clone();

        app.handle_event(key_event(KeyCode::Right, KeyEventKind::Press))
            .expect("right press should focus preview");

        assert_eq!(app.current_dir, start_dir);
        assert_eq!(app.focus, InteractiveFocus::Preview);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_preview_focus_scrolls_instead_of_moving_selection() {
        let directory = temporary_test_directory("interactive-preview-scroll");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.focus = InteractiveFocus::Preview;
        app.preview = InteractivePreviewContent::plain(
            (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.set_preview_viewport(80, 5);
        let selected = app.selected;

        app.handle_event(key_event(KeyCode::Down, KeyEventKind::Press))
            .expect("down press should scroll preview");

        assert_eq!(app.selected, selected);
        assert_eq!(app.preview_scroll, 1);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_left_returns_preview_focus_to_list() {
        let directory = temporary_test_directory("interactive-preview-left");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.focus = InteractiveFocus::Preview;

        app.handle_event(key_event(KeyCode::Left, KeyEventKind::Press))
            .expect("left press should return focus to list");

        assert_eq!(app.focus, InteractiveFocus::List);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_selection_change_resets_preview_focus_and_scroll() {
        let directory = temporary_test_directory("interactive-selection-focus-reset");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("first jpg should be written");
        std::fs::write(directory.join("b.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("second jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.focus = InteractiveFocus::Preview;
        app.preview_scroll = 4;

        app.select_next();

        assert_eq!(app.focus, InteractiveFocus::List);
        assert_eq!(app.preview_scroll, 0);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_preview_scroll_clamps_to_viewport() {
        let directory = temporary_test_directory("interactive-scroll-clamp");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.focus = InteractiveFocus::Preview;
        app.preview = InteractivePreviewContent::plain(
            (0..12)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        app.set_preview_viewport(80, 5);

        app.handle_event(key_event(KeyCode::PageDown, KeyEventKind::Press))
            .expect("page down should scroll preview");
        app.handle_event(key_event(KeyCode::PageDown, KeyEventKind::Press))
            .expect("page down should clamp preview");
        assert_eq!(app.preview_scroll, 7);

        app.handle_event(key_event(KeyCode::End, KeyEventKind::Press))
            .expect("end should scroll to bottom");
        assert_eq!(app.preview_scroll, 7);

        app.handle_event(key_event(KeyCode::PageUp, KeyEventKind::Press))
            .expect("page up should scroll preview");
        app.handle_event(key_event(KeyCode::PageUp, KeyEventKind::Press))
            .expect("page up should clamp preview");
        assert_eq!(app.preview_scroll, 0);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_display_path_removes_windows_verbatim_prefixes() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\photos")),
            r"C:\Users\photos"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\photos")),
            r"\\server\share\photos"
        );
    }

    #[test]
    fn interactive_matching_preview_result_updates_panel() {
        let directory = temporary_test_directory("interactive-preview-result");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "image.jpg")
            .expect("image entry should exist");
        app.request_preview_for_selection();

        app.apply_preview_result(preview_result_for_selected(&app, "ready preview"));

        assert!(!app.preview_loading);
        assert!(!app.preview_loading_visible);
        assert_eq!(app.preview.as_plain(), Some("ready preview"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_stale_preview_result_is_discarded() {
        let directory = temporary_test_directory("interactive-stale-preview");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("first jpg should be written");
        std::fs::write(directory.join("b.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("second jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "a.jpg")
            .expect("first image entry should exist");
        app.request_preview_for_selection();
        let stale = preview_result_for_selected(&app, "stale preview");

        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "b.jpg")
            .expect("second image entry should exist");
        app.preview = InteractivePreviewContent::plain("previous preview");
        app.request_preview_for_selection();
        let previous_preview = app.preview.clone();

        app.apply_preview_result(stale);

        assert!(app.preview_loading);
        assert!(!app.preview_loading_visible);
        assert_eq!(app.preview, previous_preview);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_directory_preview_renders_browsable_entries_only() {
        let directory = temporary_test_directory("interactive-directory-preview");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested directory should be created");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");
        std::fs::write(directory.join("notes.txt"), "not browsable")
            .expect("text file should be written");

        let preview = render_directory_preview(&directory);
        let lines = preview
            .as_lines()
            .expect("directory preview should render styled lines");

        assert!(line_text(&lines[0]).is_empty());
        assert_eq!(line_text(&lines[1]), "nested/");
        assert_eq!(
            lines[1].spans[0].style,
            interactive_entry_style(InteractiveEntryKind::Directory)
        );
        assert_eq!(line_text(&lines[2]), "image.jpg");
        assert_eq!(
            lines[2].spans[0].style,
            interactive_entry_style(InteractiveEntryKind::File)
        );
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(!rendered.contains(&display_path(&directory)));
        assert!(!rendered.contains("Contents:"));
        assert!(!rendered.contains("../"));
        assert!(!rendered.contains("Folder"));
        assert!(!rendered.contains("Parent directory"));
        assert!(!rendered.contains("notes.txt"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_empty_directory_preview_renders_empty_message_only() {
        let directory = temporary_test_directory("interactive-empty-directory-preview");

        let preview = render_directory_preview(&directory);
        let lines = preview
            .as_lines()
            .expect("directory preview should render styled lines");

        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).is_empty());
        assert_eq!(
            line_text(&lines[1]),
            "<No directories or photos in this directory>"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_preview_title_tracks_selected_entry_kind() {
        let directory = temporary_test_directory("interactive-preview-title");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested directory should be created");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.kind == InteractiveEntryKind::Parent)
            .expect("parent entry should exist");
        assert_eq!(selected_preview_title(&app), "Directory");

        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "nested/")
            .expect("nested entry should exist");
        assert_eq!(selected_preview_title(&app), "Directory");

        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "image.jpg")
            .expect("image entry should exist");
        assert_eq!(selected_preview_title(&app), "Read");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn interactive_preview_loading_appears_after_delay() {
        let directory = temporary_test_directory("interactive-loading-delay");
        std::fs::write(directory.join("image.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");

        let mut app = InteractiveApp::new(&directory).expect("app should initialize");
        app.selected = app
            .entries
            .iter()
            .position(|entry| entry.label == "image.jpg")
            .expect("image entry should exist");
        app.preview = InteractivePreviewContent::plain("previous preview");
        app.request_preview_for_selection();

        app.tick_preview_spinner();
        assert!(!app.preview_loading_visible);
        assert_eq!(app.preview.as_plain(), Some("previous preview"));

        app.preview_loading_deadline = Some(Instant::now());
        app.tick_preview_spinner();

        assert!(app.preview_loading_visible);
        let preview = app
            .preview
            .as_plain()
            .expect("loading preview should be plain text");
        assert!(preview.contains("Reading EXIF preview"));
        assert!(preview.starts_with(app.preview_spinner.frame()));
        assert!(!preview.contains("image.jpg"));
        assert!(!preview.contains(&display_path(&directory)));
        assert!(!preview.contains("\n\n"));

        let _ = std::fs::remove_dir_all(directory);
    }

    fn key_event(code: KeyCode, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, KeyModifiers::empty(), kind))
    }

    fn preview_result_for_selected(
        app: &InteractiveApp,
        preview: &str,
    ) -> InteractivePreviewResult {
        let entry = app
            .entries
            .get(app.selected)
            .expect("selected entry should exist");

        InteractivePreviewResult {
            request_id: app.preview_request_id,
            kind: entry.kind,
            path: entry.path.clone(),
            preview: InteractivePreviewContent::plain(preview),
        }
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn renders_metadata_template_values() {
        let image_files = vec![PathBuf::from("01.tif"), PathBuf::from("1_result.jpg")];
        let output = render_metadata_template("2026-05-22", &image_files);

        assert!(output.contains("date: 2026-05-22"));
        assert!(output.contains("date_end: 2026-05-22"));
        assert!(output.contains("frame_count: 2"));
        assert!(output.contains("frames:\n    \"01.tif\":\n    \"1_result.jpg\":"));
        assert!(!output.contains("image-file.tif"));
        assert!(!output.contains("ExposureTime:"));
        assert!(!output.contains("<today>"));
        assert!(!output.contains("<image-count-in-directory>"));
        assert!(!output.contains("<frames>"));
    }

    #[test]
    fn new_creates_metadata_file() {
        let directory = temporary_test_directory("new-creates");

        new_command(
            false,
            NewArgs {
                path: directory.clone(),
            },
        )
        .expect("new should create metadata file");

        let output = std::fs::read_to_string(directory.join(METADATA_FILE_NAME))
            .expect("metadata file should be readable");
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(output.contains(&format!("date: {today}")));
        assert!(output.contains("frame_count: 0"));
        assert!(output.contains("frames:\n\n"));
        assert!(!output.contains("frames: {}"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn new_creates_metadata_file_in_target_directory() {
        let parent = temporary_test_directory("new-target-parent");
        let directory = parent.join("nested");
        std::fs::create_dir(&directory).expect("nested test directory should be created");

        new_command(
            false,
            NewArgs {
                path: directory.clone(),
            },
        )
        .expect("new should create metadata file in target directory");

        assert!(directory.join(METADATA_FILE_NAME).exists());
        assert!(!parent.join(METADATA_FILE_NAME).exists());

        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn new_refuses_to_overwrite_existing_metadata_file() {
        let directory = temporary_test_directory("new-existing");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(&metadata, "existing").expect("metadata file should be written");

        let result = new_command(
            false,
            NewArgs {
                path: directory.clone(),
            },
        );

        assert!(
            matches!(result, Err(CliError::Warning(message)) if message.contains("metadata.yml already exists"))
        );
        assert_eq!(
            std::fs::read_to_string(&metadata).expect("metadata file should be readable"),
            "existing"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn new_counts_supported_images_non_recursively() {
        let directory = temporary_test_directory("new-image-count");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested test directory should be created");
        for file_name in [
            "a.JPG", "b.jpeg", "c.jxl", "d.heif", "e.hif", "f.heic", "g.avif", "h.png", "i.tiff",
            "j.webp",
        ] {
            std::fs::write(directory.join(file_name), []).expect("test image should be written");
        }
        std::fs::write(directory.join("notes.txt"), []).expect("test text file should be written");
        std::fs::write(nested.join("nested.jpg"), []).expect("nested image should be written");

        new_command(
            false,
            NewArgs {
                path: directory.clone(),
            },
        )
        .expect("new should create metadata file");

        let output = std::fs::read_to_string(directory.join(METADATA_FILE_NAME))
            .expect("metadata file should be readable");
        assert!(output.contains("frame_count: 10"));
        for file_name in [
            "a.JPG", "b.jpeg", "c.jxl", "d.heif", "e.hif", "f.heic", "g.avif", "h.png", "i.tiff",
            "j.webp",
        ] {
            assert!(output.contains(&format!("    \"{file_name}\":")));
        }
        assert!(!output.contains("notes.txt"));
        assert!(!output.contains("nested.jpg"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn new_dry_run_does_not_create_metadata_file() {
        let directory = temporary_test_directory("new-dry-run");

        new_command(
            true,
            NewArgs {
                path: directory.clone(),
            },
        )
        .expect("dry-run new should succeed");

        assert!(!directory.join(METADATA_FILE_NAME).exists());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolve_metadata_path_prefers_yml_over_legacy_yaml() {
        let directory = temporary_test_directory("check-prefers-yml");
        let yml = directory.join(METADATA_FILE_NAME);
        let yaml = directory.join(LEGACY_METADATA_FILE_NAME);
        std::fs::write(&yaml, "exif: {}").expect("yaml metadata should be written");
        std::fs::write(&yml, "exif: {}").expect("yml metadata should be written");

        let resolution =
            resolve_metadata_path(Some(&directory)).expect("metadata path should resolve");

        assert_eq!(resolution.path, yml);
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("metadata.yaml"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolve_metadata_path_falls_back_to_legacy_yaml() {
        let directory = temporary_test_directory("check-fallback-yaml");
        let yaml = directory.join(LEGACY_METADATA_FILE_NAME);
        std::fs::write(&yaml, "exif: {}").expect("yaml metadata should be written");

        let resolution =
            resolve_metadata_path(Some(&directory)).expect("metadata path should resolve");

        assert_eq!(resolution.path, yaml);
        assert!(resolution.warnings.is_empty());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolve_metadata_path_errors_when_missing() {
        let directory = temporary_test_directory("check-missing");

        let result = resolve_metadata_path(Some(&directory));

        assert!(matches!(result, Err(message) if message.contains("no metadata.yml")));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_request_treats_single_non_metadata_argument_as_targets() {
        let args = WriteArgs {
            metadata_or_targets: Some(PathBuf::from("*.jpg")),
            targets: None,
            strip: false,
            keep: Vec::new(),
            remove: Vec::new(),
            privacy: false,
            no_overwrite: false,
            extensions: Vec::new(),
            recursive: false,
        };

        let request = WriteRequest::from_args(&args);

        assert_eq!(request.metadata, None);
        assert_eq!(request.targets, Some("*.jpg".to_string()));
    }

    #[test]
    fn write_targets_filter_default_images_by_extension() {
        let directory = temporary_test_directory("write-target-extensions");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");
        std::fs::write(directory.join("b.tif"), [0x49, 0x49, 0x2a, 0x00])
            .expect("tif should be written");

        let targets = resolve_write_targets(&directory, None, false, &["jpg".to_string()])
            .expect("targets should resolve");

        assert_eq!(targets, [directory.join("a.jpg")]);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_dry_run_reports_without_modifying_file() {
        let directory = temporary_test_directory("strip-dry-run");
        let image = directory.join("image.jpg");
        let contents = [0xff, 0xd8, 0xff, 0xd9];
        std::fs::write(&image, contents).expect("jpg should be written");

        let result = strip_metadata_from_image(&image, true, true, &StripMode::all());

        assert!(result.stripped);
        assert!(!result.verified);
        assert!(result.warnings.is_empty());
        assert!(result.errors.is_empty());
        assert_eq!(
            std::fs::read(&image).expect("image should be readable"),
            contents
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_unsupported_clear_format_reports_per_file_error() {
        let directory = temporary_test_directory("strip-unsupported-clear-format");
        let image = directory.join("image.webp");
        std::fs::write(&image, b"RIFF\x04\0\0\0WEBP").expect("webp should be written");

        let result = strip_metadata_from_image(&image, false, false, &StripMode::all());

        assert!(!result.stripped);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("failed to strip EXIF metadata"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_output_renders_pretty_summary() {
        let output = StripOutput {
            dry_run: true,
            mode: "all",
            target_count: 1,
            files: vec![StripFileOutput {
                label: "image.jpg".to_string(),
                image: PathBuf::from("image.jpg"),
                result: StripFileResult {
                    stripped: true,
                    verified: false,
                    removed_tags: 0,
                    warnings: Vec::new(),
                    errors: Vec::new(),
                },
                elapsed_ms: 17,
                dry_run: true,
            }],
        };
        let mut summary = StripSummary::default();
        summary.add(&output.files[0].result);
        summary.elapsed_ms = 17;

        let rendered = strip_ansi_codes(&format_strip_output(&output, &summary));

        assert!(rendered.contains("strip "));
        assert!(rendered.contains("mode: all"));
        assert!(rendered.contains("targets: 1"));
        assert!(rendered.contains("image.jpg\nwould strip EXIF\nremoved 0 tags\ntook 17ms"));
        assert!(rendered.contains("would strip    1"));
        assert!(rendered.contains("status         success"));
    }

    #[test]
    fn strip_json_output_is_machine_readable() {
        let output = StripOutput {
            dry_run: false,
            mode: "all",
            target_count: 1,
            files: vec![StripFileOutput {
                label: "image.jpg".to_string(),
                image: PathBuf::from("image.jpg"),
                result: StripFileResult {
                    stripped: true,
                    verified: true,
                    removed_tags: 3,
                    warnings: Vec::new(),
                    errors: Vec::new(),
                },
                elapsed_ms: 12,
                dry_run: false,
            }],
        };
        let mut summary = StripSummary::default();
        summary.add(&output.files[0].result);
        summary.elapsed_ms = 12;

        let value: serde_json::Value =
            serde_json::from_str(&format_strip_json_output(&output, &summary))
                .expect("strip JSON should parse");

        assert_eq!(value["command"], "strip");
        assert_eq!(value["mode"], "all");
        assert_eq!(value["dry_run"], false);
        assert_eq!(value["target_count"], 1);
        assert_eq!(value["files"][0]["status"], "verified");
        assert_eq!(value["files"][0]["removed_tags"], 3);
        assert_eq!(value["summary"]["files_stripped"], 1);
        assert_eq!(value["summary"]["tags_removed"], 3);
        assert_eq!(value["summary"]["files_verified"], 1);
        assert_eq!(value["status"], "success");
    }

    #[test]
    fn strip_keep_preserves_named_tags_and_removes_others() {
        let directory = temporary_test_directory("strip-keep");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Model", "F3"),
                ("Artist", "Harry Merritt"),
            ],
        );

        let mode = StripMode::keep(
            TagSelectorSet::from_values(&["Make".to_string()]).expect("selector should parse"),
        );
        let result = strip_metadata_from_image(&image, false, true, &mode);

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert!(result.verified);
        assert_eq!(result.removed_tags, 2);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(!names.contains("Artist"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_keep_with_remove_prefers_explicit_remove() {
        let directory = temporary_test_directory("strip-keep-with-remove");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Model", "F3"),
                ("Artist", "Harry Merritt"),
            ],
        );

        let mode = StripMode::keep_with_remove(
            TagSelectorSet::from_values(&["Make".to_string()]).expect("keep selector should parse"),
            TagSelectorSet::from_values(&["Make".to_string()])
                .expect("remove selector should parse"),
        );
        let result = strip_metadata_from_image(&image, false, true, &mode);

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert!(result.verified);
        assert_eq!(result.removed_tags, 3);
        let names = exif_tag_names(&image);
        assert!(!names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(!names.contains("Artist"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_remove_only_removes_named_tags() {
        let directory = temporary_test_directory("strip-remove");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Model", "F3"),
                ("Artist", "Harry Merritt"),
            ],
        );

        let mode = StripMode::remove(
            TagSelectorSet::from_values(&["Model".to_string()]).expect("selector should parse"),
        );
        let result = strip_metadata_from_image(&image, false, true, &mode);

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert!(result.verified);
        assert_eq!(result.removed_tags, 1);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(names.contains("Artist"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_privacy_removes_sensitive_tags_and_keeps_technical_tags() {
        let directory = temporary_test_directory("strip-privacy");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Artist", "Harry Merritt"),
                ("DateTimeOriginal", "2026-05-22"),
                ("FNumber", "5.6"),
            ],
        );

        let result = strip_metadata_from_image(&image, false, true, &StripMode::privacy());

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert!(result.verified);
        assert_eq!(result.removed_tags, 2);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(names.contains("FNumber"));
        assert!(!names.contains("Artist"));
        assert!(!names.contains("DateTimeOriginal"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_privacy_with_remove_removes_extra_tag() {
        let directory = temporary_test_directory("strip-privacy-with-remove");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Artist", "Harry Merritt"),
                ("DateTimeOriginal", "2026-05-22"),
                ("FNumber", "5.6"),
            ],
        );

        let mode = StripMode::privacy_with_remove(
            TagSelectorSet::from_values(&["FNumber".to_string()])
                .expect("remove selector should parse"),
        );
        let result = strip_metadata_from_image(&image, false, true, &mode);

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert!(result.verified);
        assert_eq!(result.removed_tags, 3);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(!names.contains("FNumber"));
        assert!(!names.contains("Artist"));
        assert!(!names.contains("DateTimeOriginal"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_privacy_preserves_unknown_tags_and_required_tiff_tags() {
        let exif = parse_raw_exif(&[
            tiff_long_entry(0x0100, 640),
            tiff_ascii_entry(0x010f, b"N\0"),
            tiff_short_entry(0xfde8, 42),
        ]);
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        let targets = strip_removal_targets(
            &exif,
            &StripMode::privacy(),
            true,
            &mut warnings,
            &mut errors,
        );
        let tag_ids = targets
            .iter()
            .map(|target| target.tag_id)
            .collect::<HashSet<_>>();
        let names = targets
            .into_iter()
            .map(|target| target.name)
            .collect::<HashSet<_>>();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!tag_ids.contains(&0xfde8));
        assert!(!names.contains("ImageWidth"));
        assert!(!names.contains("Make"));
    }

    #[test]
    fn strip_selective_tiff_preserves_required_structural_tags() {
        let exif = parse_raw_exif(&[
            tiff_long_entry(0x0100, 640),
            tiff_long_entry(0x0101, 480),
            tiff_short_entry(0x0102, 8),
            tiff_ascii_entry(0x010e, b"A\0"),
            tiff_ascii_entry(0x013b, b"B\0"),
            tiff_short_entry(0x013d, 2),
        ]);
        let mode = StripMode::keep(
            TagSelectorSet::from_values(&["Make".to_string()]).expect("selector should parse"),
        );
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        let targets = strip_removal_targets(&exif, &mode, true, &mut warnings, &mut errors);
        let tag_ids = targets
            .iter()
            .map(|target| target.tag_id)
            .collect::<HashSet<_>>();
        let names = targets
            .into_iter()
            .map(|target| target.name)
            .collect::<HashSet<_>>();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!names.contains("ImageWidth"));
        assert!(!names.contains("ImageLength"));
        assert!(!names.contains("BitsPerSample"));
        assert!(names.contains("ImageDescription"));
        assert!(names.contains("Artist"));
        assert!(!tag_ids.contains(&0x013d));
    }

    #[test]
    fn strip_all_tiff_preserves_visual_structural_tags_and_removes_metadata_tags() {
        let (xres_entry, xres_data) = tiff_rational_entry_with_count(0x011a, &[(300, 1)], 200);
        let (yres_entry, yres_data) = tiff_rational_entry_with_count(0x011b, &[(300, 1)], 208);
        let exif = parse_raw_exif_with_offsets(
            &[
                tiff_long_entry(0x0100, 640),
                tiff_long_entry(0x0101, 480),
                tiff_short_entry(0x0102, 8),
                tiff_short_entry(0x0103, 5),
                tiff_short_entry(0x0106, 2),
                tiff_ascii_entry(0x010f, b"N\0"),
                tiff_ascii_entry(0x0110, b"F3\0"),
                tiff_long_entry(0x0111, 256),
                tiff_short_entry(0x0112, 1),
                tiff_short_entry(0x0115, 3),
                tiff_long_entry(0x0116, 1),
                tiff_long_entry(0x0117, 4),
                xres_entry,
                yres_entry,
                tiff_short_entry(0x011c, 1),
                tiff_short_entry(0x0128, 2),
                tiff_ascii_offset_entry(0x0131, 6, 216),
                tiff_ascii_offset_entry(0x0132, 20, 222),
                tiff_short_entry(0x013d, 2),
                tiff_undefined_entry(0x02bc, 4, 242),
                tiff_undefined_entry(0x83bb, 4, 246),
                tiff_long_entry(0x8769, 300),
                tiff_undefined_entry(0x8773, 4, 254),
            ],
            &[
                (200, xres_data),
                (208, yres_data),
                (216, b"soft\0\0".to_vec()),
                (222, b"2026:05:27 12:00:00\0".to_vec()),
                (242, b"xmp!".to_vec()),
                (246, b"iptc".to_vec()),
                (254, b"icc!".to_vec()),
                (300, vec![0, 0, 0, 0, 0, 0]),
            ],
        );
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        let targets =
            strip_removal_targets(&exif, &StripMode::all(), true, &mut warnings, &mut errors);
        let target_ids = targets
            .iter()
            .map(|target| target.tag_id)
            .collect::<HashSet<_>>();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(errors.is_empty(), "{errors:?}");
        for protected in [
            0x0100, 0x0101, 0x0102, 0x0103, 0x0106, 0x0111, 0x0112, 0x0115, 0x0116, 0x0117, 0x011a,
            0x011b, 0x011c, 0x0128, 0x013d, 0x8773,
        ] {
            assert!(
                !target_ids.contains(&protected),
                "protected TIFF tag 0x{protected:04x} should not be removed"
            );
        }
        for removable in [0x010f, 0x0110, 0x0131, 0x0132, 0x02bc, 0x83bb, 0x8769] {
            assert!(
                target_ids.contains(&removable),
                "metadata TIFF tag 0x{removable:04x} should be removed"
            );
        }
    }

    #[test]
    fn strip_remove_required_tiff_structural_tag_fails_before_write() {
        let exif = parse_raw_exif(&[
            tiff_long_entry(0x0100, 640),
            tiff_ascii_entry(0x010e, b"A\0"),
        ]);
        let mode = StripMode::remove(
            TagSelectorSet::from_values(&["ImageWidth".to_string()])
                .expect("selector should parse"),
        );
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        let targets = strip_removal_targets(&exif, &mode, true, &mut warnings, &mut errors);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(targets.is_empty());
        assert_eq!(
            errors,
            vec!["cannot remove required TIFF tag `ImageWidth`: TIFF cannot be written without it"]
        );
    }

    #[test]
    fn strip_remove_predictor_fails_before_write() {
        let exif = parse_raw_exif(&[
            tiff_short_entry(0x013d, 2),
            tiff_ascii_entry(0x010e, b"A\0"),
        ]);
        let mode = StripMode::remove(
            TagSelectorSet::from_values(&["Predictor".to_string()]).expect("selector should parse"),
        );
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        let targets = strip_removal_targets(&exif, &mode, true, &mut warnings, &mut errors);

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(targets.is_empty());
        assert_eq!(
            errors,
            vec!["cannot remove required TIFF tag `0x013D`: TIFF cannot be written without it"]
        );
    }

    #[test]
    fn strip_all_tiff_preserves_predictor_when_writing() {
        let directory = temporary_test_directory("strip-tiff-predictor");
        let image = directory.join("image.tif");
        write_synthetic_tiff_with_predictor(&image);

        let result = strip_metadata_from_image(&image, false, true, &StripMode::all());

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert!(result.verified);
        let ids = exif_tag_ids(&image);
        let names = exif_tag_names(&image);
        assert!(ids.contains(&0x013d));
        assert!(ids.contains(&0x8773));
        assert!(!names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(!names.contains("Software"));
        assert!(!names.contains("DateTime"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn strip_does_not_touch_xmp_sidecars() {
        let directory = temporary_test_directory("strip-sidecars-untouched");
        let image = directory.join("image.jpg");
        let sidecar_stem = directory.join("image.xmp");
        let sidecar_full = directory.join("image.jpg.xmp");
        write_test_exif_tags(&image, [("Make", "Nikon")]);
        std::fs::write(&sidecar_stem, "stem sidecar").expect("stem sidecar should be written");
        std::fs::write(&sidecar_full, "full sidecar").expect("full sidecar should be written");

        let result = strip_metadata_from_image(&image, false, false, &StripMode::all());

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(
            std::fs::read_to_string(&sidecar_stem).expect("stem sidecar should remain"),
            "stem sidecar"
        );
        assert_eq!(
            std::fs::read_to_string(&sidecar_full).expect("full sidecar should remain"),
            "full sidecar"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_strip_clears_existing_tags_before_writing() {
        let directory = temporary_test_directory("write-strip-all");
        let image = directory.join("image.jpg");
        write_test_exif_tags(&image, [("Make", "Nikon"), ("Model", "F3")]);
        let args = WriteArgs {
            strip: true,
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "Artist".to_string(),
            value: YamlValue::String("Harry Merritt".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            false,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert_eq!(result.written, 1);
        let names = exif_tag_names(&image);
        assert!(!names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(names.contains("Artist"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_keep_preserves_named_tags_before_writing() {
        let directory = temporary_test_directory("write-keep");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Model", "F3"),
                ("Artist", "Harry Merritt"),
            ],
        );
        let args = WriteArgs {
            keep: vec!["Make".to_string()],
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "FNumber".to_string(),
            value: YamlValue::String("5.6".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            false,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert_eq!(result.removed_tags, 2);
        assert_eq!(result.written, 1);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(!names.contains("Artist"));
        assert!(names.contains("FNumber"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_remove_removes_named_tags_before_writing() {
        let directory = temporary_test_directory("write-remove");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Model", "F3"),
                ("Artist", "Harry Merritt"),
            ],
        );
        let args = WriteArgs {
            remove: vec!["Model".to_string()],
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "FNumber".to_string(),
            value: YamlValue::String("5.6".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            false,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert_eq!(result.removed_tags, 1);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(!names.contains("Model"));
        assert!(names.contains("Artist"));
        assert!(names.contains("FNumber"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_privacy_removes_sensitive_tags_before_writing() {
        let directory = temporary_test_directory("write-privacy");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Artist", "Harry Merritt"),
                ("DateTimeOriginal", "2026-05-22"),
                ("FNumber", "5.6"),
            ],
        );
        let args = WriteArgs {
            privacy: true,
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "Model".to_string(),
            value: YamlValue::String("F3".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            false,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert_eq!(result.removed_tags, 2);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(names.contains("FNumber"));
        assert!(names.contains("Model"));
        assert!(!names.contains("Artist"));
        assert!(!names.contains("DateTimeOriginal"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_privacy_preserves_unknown_tags_unless_explicitly_removed() {
        let args = WriteArgs {
            privacy: true,
            ..default_write_args()
        };
        let mode = StripMode::from_write_args(&args)
            .expect("write privacy mode should parse")
            .expect("write privacy should create a strip mode");
        let exif = parse_raw_exif(&[
            tiff_ascii_entry(0x013b, b"A\0"),
            tiff_short_entry(0xfde8, 42),
        ]);
        let mut warnings = Vec::new();
        let mut errors = Vec::new();

        let targets = strip_removal_targets(&exif, &mode, true, &mut warnings, &mut errors);
        let tag_ids = targets
            .iter()
            .map(|target| target.tag_id)
            .collect::<HashSet<_>>();
        let names = targets
            .into_iter()
            .map(|target| target.name)
            .collect::<HashSet<_>>();

        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(errors.is_empty(), "{errors:?}");
        assert!(names.contains("Artist"));
        assert!(!tag_ids.contains(&0xfde8));
    }

    #[test]
    fn write_privacy_with_remove_removes_extra_tag_before_writing() {
        let directory = temporary_test_directory("write-privacy-remove");
        let image = directory.join("image.jpg");
        write_test_exif_tags(
            &image,
            [
                ("Make", "Nikon"),
                ("Artist", "Harry Merritt"),
                ("DateTimeOriginal", "2026-05-22"),
                ("FNumber", "5.6"),
            ],
        );
        let args = WriteArgs {
            privacy: true,
            remove: vec!["FNumber".to_string()],
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "Model".to_string(),
            value: YamlValue::String("F3".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            false,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert_eq!(result.removed_tags, 3);
        let names = exif_tag_names(&image);
        assert!(names.contains("Make"));
        assert!(names.contains("Model"));
        assert!(!names.contains("FNumber"));
        assert!(!names.contains("Artist"));
        assert!(!names.contains("DateTimeOriginal"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_remove_allows_no_overwrite_to_write_removed_tag() {
        let directory = temporary_test_directory("write-remove-no-overwrite");
        let image = directory.join("image.jpg");
        write_test_exif_tags(&image, [("Model", "F3")]);
        let args = WriteArgs {
            remove: vec!["Model".to_string()],
            no_overwrite: true,
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "Model".to_string(),
            value: YamlValue::String("FM2".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            false,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.skipped.is_empty(), "{:?}", result.skipped);
        assert!(result.stripped);
        assert_eq!(result.removed_tags, 1);
        assert_eq!(result.written, 1);
        assert!(exif_tag_names(&image).contains("Model"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_strip_dry_run_reports_without_modifying_file() {
        let directory = temporary_test_directory("write-strip-dry-run");
        let image = directory.join("image.jpg");
        write_test_exif_tags(&image, [("Make", "Nikon")]);
        let before = std::fs::read(&image).expect("image should be readable");
        let args = WriteArgs {
            remove: vec!["Make".to_string()],
            ..default_write_args()
        };
        let tags = vec![WriteTag {
            name: "Model".to_string(),
            value: YamlValue::String("F3".to_string()),
        }];

        let result = apply_tags_to_image(
            &image,
            &tags,
            true,
            &args,
            StripMode::from_write_args(&args)
                .expect("write strip mode should parse")
                .as_ref(),
        );

        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.stripped);
        assert_eq!(result.removed_tags, 1);
        assert_eq!(result.written, 1);
        assert_eq!(
            std::fs::read(&image).expect("image should remain readable"),
            before
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_plan_merges_global_and_frame_tags() {
        let directory = temporary_test_directory("write-plan-merge");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("one.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  Make: Nikon
  Model: F3
frames:
  1:
    - Model: FM2
    - ExposureTime: 1/500
"#,
        )
        .expect("test YAML should parse");

        let plan = build_write_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("write plan should build");
        let frame = plan
            .frame_for_image(&image)
            .expect("image should have merged tags");
        let tags = frame.tags;

        assert_eq!(tags.len(), 3);
        assert_eq!(frame.label, "one.jpg ← frame 1");
        assert!(
            tags.iter()
                .any(|tag| tag.name == "Make" && tag.value.as_str() == Some("Nikon"))
        );
        assert!(
            tags.iter()
                .any(|tag| tag.name == "Model" && tag.value.as_str() == Some("FM2"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_plan_expands_iso_aliases_after_frame_overrides() {
        let directory = temporary_test_directory("write-plan-iso");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("one.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  ISO: 400
frames:
  1:
    - ISOSpeed: 800
"#,
        )
        .expect("test YAML should parse");

        let plan = build_write_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("write plan should build");
        let tags = plan
            .frame_for_image(&image)
            .expect("image should have merged tags")
            .tags;

        for name in ["ISO", "ISOSpeed", "ISOSpeedRatings"] {
            assert!(
                tags.iter()
                    .any(|tag| tag.name == name && tag.value.as_i64() == Some(800)),
                "{name} should be expanded with the frame override"
            );
        }

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_plan_labels_filename_frame_keys_with_file_name() {
        let directory = temporary_test_directory("write-plan-filename-label");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("2.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  "2.jpg":
    Make: Nikon
"#,
        )
        .expect("test YAML should parse");

        let plan = build_write_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("write plan should build");
        let frame = plan
            .frame_for_image(&image)
            .expect("filename frame should match image");

        assert_eq!(frame.label, "2.jpg");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_plan_accepts_null_frames_and_null_frame_values() {
        let directory = temporary_test_directory("write-plan-null-frames");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("one.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  Make: Nikon
frames:
  "one.jpg":
"#,
        )
        .expect("test YAML should parse");

        let plan = build_write_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("write plan should build");
        let frame = plan
            .frame_for_image(&image)
            .expect("global tags should apply to the null frame");

        assert_eq!(frame.label, "one.jpg");
        assert!(
            frame
                .tags
                .iter()
                .any(|tag| tag.name == "Make" && tag.value.as_str() == Some("Nikon"))
        );

        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  Make: Nikon
frames:
"#,
        )
        .expect("test YAML should parse");
        let plan = build_write_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("write plan should build");
        assert!(plan.frame_for_image(&image).is_some());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn write_output_renders_file_groups_and_overview() {
        colored::control::set_override(true);
        let image = PathBuf::from("image.jpg");
        let output = WriteOutput {
            metadata_path: PathBuf::from("metadata.yml"),
            dry_run: true,
            target_count: 3,
            file_warnings: Vec::new(),
            files: vec![
                WriteFileOutput {
                    label: "frame 1 (image.jpg)".to_string(),
                    image: image.clone(),
                    result: WriteFileResult {
                        written: 2,
                        warnings: vec!["skipping unsupported writer tag `FilmRoll`".to_string()],
                        ..WriteFileResult::default()
                    },
                    elapsed_ms: 42,
                    dry_run: false,
                },
                WriteFileOutput {
                    label: "2.jpg".to_string(),
                    image: PathBuf::from("2.jpg"),
                    result: WriteFileResult {
                        written: 1,
                        ..WriteFileResult::default()
                    },
                    elapsed_ms: 1501,
                    dry_run: false,
                },
            ],
            skipped_files: vec![PathBuf::from("missing.jpg")],
        };
        let mut summary = WriteSummary::default();
        summary.add(&output.files[0].result);
        summary.add(&output.files[1].result);
        summary.skipped_files = output.skipped_files.len();
        summary.elapsed_ms = 42;

        let rendered = format_write_output(&output, &summary);
        let plain = strip_ansi_codes(&rendered);

        assert!(plain.starts_with("write "));
        assert!(plain.contains("mode: dry-run\n\nframes "));
        assert!(plain.contains("frames "));
        assert!(plain.contains("frame 1 (image.jpg)\nwrote 2 tags\ntook 42ms"));
        assert!(rendered.contains("\u{1b}[94mframes"));
        assert!(rendered.contains("\u{1b}[96mframe 1 (image.jpg)"));
        assert!(rendered.contains("\u{1b}[96m2.jpg"));
        assert!(!plain.contains("file: image.jpg"));
        assert!(!plain.contains("\ntags:"));
        assert!(!plain.contains("\nskipped: 0"));
        assert!(plain.contains("warning: skipping unsupported writer tag `FilmRoll`"));
        assert!(plain.contains(
            "warning: skipping unsupported writer tag `FilmRoll`\n2.jpg\nwrote 1 tags\ntook 1.5s"
        ));
        assert!(plain.contains("missing.jpg\nskipped: no metadata"));
        assert!(plain.contains("skipped: no metadata\n\noverview "));
        assert!(rendered.contains("\u{1b}[94moverview"));
        assert!(plain.contains("errors         0"));
        assert!(plain.contains("warnings       1"));
        assert!(plain.contains("written        3"));
        assert!(plain.contains("stripped       0"));
        assert!(plain.contains("removed tags   0"));
        assert!(plain.contains("skipped        0"));
        assert!(plain.contains("files skipped  1"));
        assert!(plain.contains("took           42ms"));
        assert!(plain.contains("status         success (with warnings)"));
        assert!(!plain.contains("write:"));
        assert!(rendered.contains("status         \u{1b}[32msuccess\u{1b}[0m (with warnings)"));
    }

    #[test]
    fn write_output_prints_path_for_non_current_directory_files() {
        colored::control::set_override(true);
        let output = WriteOutput {
            metadata_path: PathBuf::from("metadata.yml"),
            dry_run: true,
            target_count: 1,
            file_warnings: Vec::new(),
            files: vec![WriteFileOutput {
                label: "image.jpg".to_string(),
                image: PathBuf::from("nested").join("image.jpg"),
                result: WriteFileResult {
                    written: 1,
                    ..WriteFileResult::default()
                },
                elapsed_ms: 0,
                dry_run: false,
            }],
            skipped_files: Vec::new(),
        };
        let mut summary = WriteSummary::default();
        summary.add(&output.files[0].result);

        let rendered = format_write_output(&output, &summary);
        let plain = strip_ansi_codes(&rendered);

        assert!(rendered.contains("\u{1b}[96mimage.jpg"));
        assert!(
            plain.contains("file: nested\\image.jpg") || plain.contains("file: nested/image.jpg")
        );
    }

    #[test]
    fn write_file_output_renders_strip_status_before_write_status() {
        let file = WriteFileOutput {
            label: "image.jpg".to_string(),
            image: PathBuf::from("image.jpg"),
            result: WriteFileResult {
                written: 2,
                strip_attempted: true,
                stripped: true,
                removed_tags: 3,
                ..WriteFileResult::default()
            },
            elapsed_ms: 12,
            dry_run: false,
        };

        let rendered = strip_ansi_codes(&format_write_file_output(&file));

        assert!(rendered.contains("image.jpg\nstripped EXIF: 3 tags\nwrote 2 tags\ntook 12ms"));
    }

    #[test]
    fn write_file_output_renders_dry_run_strip_status() {
        let file = WriteFileOutput {
            label: "image.jpg".to_string(),
            image: PathBuf::from("image.jpg"),
            result: WriteFileResult {
                written: 1,
                strip_attempted: true,
                stripped: true,
                removed_tags: 1,
                ..WriteFileResult::default()
            },
            elapsed_ms: 12,
            dry_run: true,
        };

        let rendered = strip_ansi_codes(&format_write_file_output(&file));

        assert!(rendered.contains("image.jpg\nwould strip EXIF: 1 tags\nwould write 1 tags"));
    }

    #[test]
    fn write_overview_status_omits_warning_suffix_without_warnings() {
        colored::control::set_override(true);
        let summary = WriteSummary {
            written_tags: 1,
            ..WriteSummary::default()
        };

        let rendered = format_write_overview_output(&summary);
        let plain = strip_ansi_codes(&rendered);

        assert!(plain.contains("warnings       0"));
        assert!(plain.contains("status         success\n"));
        assert!(!plain.contains("(with warnings)"));
        assert!(rendered.contains("status         \u{1b}[32msuccess"));
    }

    #[test]
    fn write_overview_status_fail_takes_precedence_over_warning_suffix() {
        colored::control::set_override(true);
        let summary = WriteSummary {
            warnings: 1,
            errors: 1,
            ..WriteSummary::default()
        };

        let rendered = format_write_overview_output(&summary);
        let plain = strip_ansi_codes(&rendered);

        assert!(plain.contains("status         fail\n"));
        assert!(!plain.contains("(with warnings)"));
        assert!(rendered.contains("status         \u{1b}[31mfail"));
    }

    #[test]
    fn write_file_output_can_render_header_before_result() {
        colored::control::set_override(true);
        let file = WriteFileOutput {
            label: "frame 1 (image.jpg)".to_string(),
            image: PathBuf::from("image.jpg"),
            result: WriteFileResult {
                written: 2,
                skipped: vec!["ISO already exists".to_string()],
                ..WriteFileResult::default()
            },
            elapsed_ms: 1500,
            dry_run: false,
        };

        let header = format_write_file_header_output(&file);
        let result = format_write_file_result_output(&file);
        let combined = format_write_file_output(&file);

        assert_eq!(combined, format!("{header}{result}"));
        assert_eq!(strip_ansi_codes(&header), "frame 1 (image.jpg)\n");
        assert!(!strip_ansi_codes(&header).contains("tags:"));
        assert_eq!(
            strip_ansi_codes(&result),
            "wrote 2 tags\ntook 1500ms\nwarning: skipped ISO already exists\n"
        );
    }

    #[test]
    fn write_file_output_always_prints_zero_written_completion_line() {
        let file = WriteFileOutput {
            label: "frame 1 (image.jpg)".to_string(),
            image: PathBuf::from("image.jpg"),
            result: WriteFileResult {
                skipped: vec!["ISO already exists".to_string()],
                ..WriteFileResult::default()
            },
            elapsed_ms: 17,
            dry_run: false,
        };

        assert_eq!(
            strip_ansi_codes(&format_write_file_result_output(&file)),
            "wrote 0 tags\ntook 17ms\nwarning: skipped ISO already exists\n"
        );
    }

    #[test]
    fn write_file_output_labels_dry_run_completion_as_would_write() {
        let file = WriteFileOutput {
            label: "frame 1 (image.jpg)".to_string(),
            image: PathBuf::from("image.jpg"),
            result: WriteFileResult {
                written: 2,
                ..WriteFileResult::default()
            },
            elapsed_ms: 9,
            dry_run: true,
        };

        assert_eq!(
            strip_ansi_codes(&format_write_file_result_output(&file)),
            "would write 2 tags\ntook 9ms\n"
        );
    }

    #[test]
    fn write_duration_formats_milliseconds_until_threshold_then_seconds() {
        assert_eq!(format_write_duration(42), "42ms");
        assert_eq!(format_write_duration(1500), "1500ms");
        assert_eq!(format_write_duration(1501), "1.5s");
        assert_eq!(format_write_duration(2345), "2.3s");
    }

    #[test]
    fn spinner_presets_match_allowed_names() {
        let names = SPINNER_PRESETS
            .iter()
            .map(|preset| preset.name())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "dots",
                "pulse",
                "fillsweep",
                "diagswipe",
                "cascade",
                "columns",
                "sand",
                "waverows",
                "scan",
            ]
        );
    }

    #[test]
    fn custom_tag_payload_round_trips_yaml_values() {
        let tags = vec![
            CustomTag {
                name: "FilmRoll".to_string(),
                value: YamlValue::Number(35.into()),
            },
            CustomTag {
                name: "FilmName".to_string(),
                value: YamlValue::String("Kodak Double-X".to_string()),
            },
            CustomTag {
                name: "FilmNegative".to_string(),
                value: YamlValue::Bool(true),
            },
        ];

        let payload = encode_custom_tags(&tags).expect("custom tags should encode");
        let decoded = custom_tags_from_bytes(&payload).expect("custom tags should decode");
        let json = std::str::from_utf8(
            payload
                .strip_prefix(USER_COMMENT_ASCII_PREFIX)
                .expect("custom tags should use the EXIF ASCII UserComment prefix"),
        )
        .expect("custom tag JSON should be UTF-8");

        assert_eq!(decoded, tags);
        assert!(!payload.starts_with(LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX));
        assert!(json.starts_with(CUSTOM_TAG_PAYLOAD_MARKER));
        assert!(json.contains(&format!("exifmeta-v{}", env!("CARGO_PKG_VERSION"))));
        assert!(json.contains(r#""FilmRoll":35"#));
        assert!(json.contains(r#""FilmName":"Kodak Double-X""#));
        assert!(json.contains(r#""FilmNegative":true"#));
    }

    #[test]
    fn custom_tag_payload_decodes_marker_bare_json_and_legacy_yaml() {
        let bare_json = br#"{"FilmRoll":35,"FilmName":"Kodak Double-X"}"#;
        let mut marked_json = CUSTOM_TAG_PAYLOAD_MARKER.as_bytes().to_vec();
        marked_json.extend_from_slice(bare_json);
        let mut old_marked_json = b"exifmeta-v0.1.0\n".to_vec();
        old_marked_json.extend_from_slice(bare_json);
        let mut ascii_prefixed_json = USER_COMMENT_ASCII_PREFIX.to_vec();
        ascii_prefixed_json.extend_from_slice(bare_json);
        let mut ascii_prefixed_old_marked_json = USER_COMMENT_ASCII_PREFIX.to_vec();
        ascii_prefixed_old_marked_json.extend_from_slice(&old_marked_json);
        let mut legacy = LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX.to_vec();
        legacy.extend_from_slice(b"FilmRoll: 35\nFilmName: Kodak Double-X\n");

        let bare_decoded = custom_tags_from_bytes(bare_json).expect("bare JSON should decode");
        let marked_decoded =
            custom_tags_from_bytes(&marked_json).expect("marked JSON should decode");
        let old_marked_decoded =
            custom_tags_from_bytes(&old_marked_json).expect("old marked JSON should decode");
        let ascii_prefixed_decoded = custom_tags_from_bytes(&ascii_prefixed_json)
            .expect("ASCII-prefixed JSON should decode");
        let ascii_prefixed_old_marked_decoded =
            custom_tags_from_bytes(&ascii_prefixed_old_marked_json)
                .expect("ASCII-prefixed old marked JSON should decode");
        let legacy_decoded =
            custom_tags_from_bytes(&legacy).expect("legacy YAML payload should decode");

        assert_eq!(
            bare_decoded,
            vec![
                CustomTag {
                    name: "FilmRoll".to_string(),
                    value: YamlValue::Number(35.into()),
                },
                CustomTag {
                    name: "FilmName".to_string(),
                    value: YamlValue::String("Kodak Double-X".to_string()),
                },
            ]
        );
        assert_eq!(marked_decoded, bare_decoded);
        assert_eq!(old_marked_decoded, bare_decoded);
        assert_eq!(ascii_prefixed_decoded, bare_decoded);
        assert_eq!(ascii_prefixed_old_marked_decoded, bare_decoded);
        assert_eq!(legacy_decoded, bare_decoded);
        assert!(custom_tags_from_bytes(b"exifmeta-v0.1.0").is_none());
    }

    #[test]
    fn write_dry_run_counts_custom_tags_without_warning() {
        let args = WriteArgs {
            metadata_or_targets: None,
            targets: None,
            strip: false,
            keep: Vec::new(),
            remove: Vec::new(),
            privacy: false,
            no_overwrite: false,
            extensions: Vec::new(),
            recursive: false,
        };
        let tags = vec![
            WriteTag {
                name: "Make".to_string(),
                value: YamlValue::String("Nikon".to_string()),
            },
            WriteTag {
                name: "FilmRoll".to_string(),
                value: YamlValue::Number(35.into()),
            },
        ];

        let result = apply_tags_to_image(Path::new("image.jpg"), &tags, true, &args, None);

        assert_eq!(result.written, 2);
        assert!(result.warnings.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn read_decodes_custom_tags_from_user_comment() {
        colored::control::set_override(true);
        let payload = encode_custom_tags(&[
            CustomTag {
                name: "FilmRoll".to_string(),
                value: YamlValue::Number(35.into()),
            },
            CustomTag {
                name: "FilmName".to_string(),
                value: YamlValue::String("Kodak Double-X".to_string()),
            },
        ])
        .expect("custom tags should encode");
        let entry = tiff_undefined_entry(0x9286, payload.len(), 200);
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(&[entry], &[(200, payload)]),
            warnings: Vec::new(),
            file_info: ReadFileInfo {
                rows: vec![ReadInfoRow::new("Image Width", "100 px")],
            },
        };

        let pretty = strip_ansi_codes(&format_read_output(
            Path::new("image.jpg"),
            &metadata,
            ReadFormat::Pretty,
        ));
        let raw = strip_ansi_codes(&format_read_output(
            Path::new("image.jpg"),
            &metadata,
            ReadFormat::Raw,
        ));

        assert!(pretty.contains("custom "));
        assert!(pretty.contains("Film Roll  35"));
        assert!(pretty.contains("Film Name  Kodak Double-X"));
        assert!(pretty.contains("misc "));
        assert!(pretty.contains("Image Width  100 px"));
        assert!(pretty.find("custom ").unwrap() < pretty.find("misc ").unwrap());
        assert!(!pretty.contains("User Comment"));
        assert!(raw.contains("0x9286"));
        assert!(raw.contains("UserComment"));
        assert!(raw.contains(&format!("exifmeta-v{}", env!("CARGO_PKG_VERSION"))));
        assert!(raw.contains("FilmRoll"));
        assert!(raw.contains("Kodak Double-X"));
        assert!(!raw.contains("IFD exifmeta  Custom  0x0000"));
    }

    #[test]
    fn write_tag_parser_supports_fraction_and_unit_values() {
        let exposure = writable_exif_tag("ExposureTime", &YamlValue::String("1/500".to_string()))
            .expect("exposure tag should parse");
        let focal_length = writable_exif_tag("FocalLength", &YamlValue::String("75mm".to_string()))
            .expect("focal length tag should parse");
        let aperture = writable_exif_tag("FNumber", &YamlValue::String("f/5.6".to_string()))
            .expect("aperture tag should parse");

        assert!(
            matches!(exposure, WritableExifTag::ExposureTime(values) if values[0].nominator == 1 && values[0].denominator == 500)
        );
        assert!(
            matches!(focal_length, WritableExifTag::FocalLength(values) if values[0].nominator == 75_000_000 && values[0].denominator == 1_000_000)
        );
        assert!(
            matches!(aperture, WritableExifTag::FNumber(values) if values[0].nominator == 5_600_000 && values[0].denominator == 1_000_000)
        );
    }

    #[test]
    fn check_metadata_rejects_bad_structure() {
        let yaml = serde_yaml::from_str::<YamlValue>("- not\n- a mapping")
            .expect("test YAML should parse");

        let result = check_metadata_file(Path::new("metadata.yml"), &yaml);

        assert!(matches!(result, Err(message) if message.contains("root must be a mapping")));
    }

    #[test]
    fn check_metadata_counts_standard_and_unknown_exif_tags() {
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  Make: Nikon
  ISOSpeedRatings: 400
  CreateDate: 2026-05-22
  NotARealExifTag: value
"#,
        )
        .expect("test YAML should parse");

        let report = check_metadata_file(Path::new("metadata.yml"), &yaml)
            .expect("metadata should pass checks");

        assert_eq!(report.exif_tags.standard, 3);
        assert_eq!(report.exif_tags.unknown, 1);
        assert_eq!(report.exif_tags.unknown_names, ["NotARealExifTag"]);
        assert!(
            report
                .exif_warnings
                .iter()
                .any(|warning| warning == "exif tag is non-standard `NotARealExifTag`")
        );
    }

    #[test]
    fn check_metadata_counts_frame_tags_and_location_special_key() {
        let directory = temporary_test_directory("check-frame-tags");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - ExposureTime: 1/500
    - $Location: London
    - NotARealExifTag: value
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert_eq!(report.frame_tags.standard, 2);
        assert_eq!(report.frame_tags.unknown, 1);
        assert_eq!(report.frame_tags.unknown_names, ["NotARealExifTag"]);
        assert!(
            report
                .location_matches
                .iter()
                .any(|location_match| location_match.name == "London")
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("$Location"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_metadata_accepts_null_frames_and_null_frame_values() {
        let directory = temporary_test_directory("check-null-frames");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(directory.join("one.jpg"), []).expect("test image should be written");

        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  "one.jpg":
"#,
        )
        .expect("test YAML should parse");
        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert_eq!(report.frame_tags, TagCounts::default());
        assert_eq!(report.frames.frames.len(), 1);
        assert!(report.frames.frames[0].errors.is_empty());
        assert!(report.frames.frames[0].warnings.is_empty());

        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
"#,
        )
        .expect("test YAML should parse");
        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert_eq!(report.frames.file_count, 1);
        assert!(report.frames.frames.is_empty());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_metadata_warns_when_location_is_not_found() {
        let directory = temporary_test_directory("check-missing-location");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - $Location: DefinitelyNotARealPlace
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert!(report.location_matches.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("DefinitelyNotARealPlace"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn formats_missing_location_warning_in_red() {
        colored::control::set_override(true);

        let output = format_check_warning(
            "$Location: no match found in database [for <DefinitelyNotARealPlace>]",
        );

        assert!(output.starts_with("\u{1b}[31mwarning:"));
    }

    #[test]
    fn formats_missing_frame_file_error_with_error_label() {
        colored::control::set_override(true);

        let output = format_check_frame_error("file does not exist");

        assert_eq!(output, "\u{1b}[31merror: file does not exist\u{1b}[0m");
    }

    #[test]
    fn formats_other_check_warnings_with_yellow_label() {
        colored::control::set_override(true);

        let output = format_check_warning("ignored metadata.yml");

        assert!(output.starts_with("\u{1b}[33mwarning\u{1b}[0m:"));
    }

    #[test]
    fn check_metadata_ignores_blank_and_null_locations() {
        let directory = temporary_test_directory("check-empty-locations");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - $Location:
  2:
    - $Location: " "
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("one.jpg"), []).expect("test image should be written");
        std::fs::write(directory.join("two.jpg"), []).expect("test image should be written");

        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert!(report.location_matches.is_empty());
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("$Location"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_metadata_warns_when_location_value_is_not_a_string() {
        let directory = temporary_test_directory("check-invalid-location");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - $Location: [London]
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert!(report.location_matches.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("$Location value must be a string"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_metadata_errors_for_missing_frame_references() {
        let directory = temporary_test_directory("check-missing-frames");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  2:
    - ExposureTime: 1/500
  "missing.tif":
    - FNumber: 2.8
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = check_metadata_file(&metadata, &yaml).expect("metadata should pass checks");

        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("frame reference `2`"))
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning == "file does not exist")
        );
        assert!(report.frames.frames.iter().any(|frame| {
            frame
                .errors
                .iter()
                .any(|error| error.contains("frame reference `2`"))
        }));
        assert!(report.frames.frames.iter().any(|frame| {
            frame
                .errors
                .iter()
                .any(|error| error == "file does not exist")
        }));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_groups_are_rendered_in_order() {
        let directory = temporary_test_directory("check-group-order");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
exif:
  Make: Nikon
frames:
  "image.jpg":
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        let file = rendered.find("file ").expect("file group should render");
        let exif = rendered.find("exif ").expect("exif group should render");
        let frames = rendered
            .find("frames ")
            .expect("frames group should render");
        let overview = rendered
            .find("overview ")
            .expect("overview group should render");
        assert!(file < exif);
        assert!(exif < frames);
        assert!(frames < overview);
        assert!(rendered.starts_with("file "));
        assert!(rendered.contains("YAML format: ok\n\nexif "));
        assert!(rendered.contains("standard tags: 1\nunknown tags: 0\n\nframes "));
        assert!(rendered.contains("unknown tags: 0\n\noverview "));
        assert!(rendered.contains("metadata file: found "));
        assert!(rendered.contains("YAML format: ok"));
        assert!(rendered.contains("validation: success"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_overview_renders_success_without_warnings() {
        let rendered = strip_ansi_codes(&format_check_output(&CheckOutput::default()));

        assert!(rendered.contains("errors      0"));
        assert!(rendered.contains("warnings    0"));
        assert!(rendered.contains("validation: success"));
        assert!(!rendered.contains("(with warnings)"));
    }

    #[test]
    fn check_overview_renders_success_with_warnings() {
        let output = CheckOutput {
            file_warnings: vec!["metadata.yaml ignored because metadata.yml exists".to_string()],
            ..CheckOutput::default()
        };
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("errors      0"));
        assert!(rendered.contains("warnings    1"));
        assert!(rendered.contains("validation: success (with warnings)"));
    }

    #[test]
    fn check_overview_renders_error_when_errors_exist() {
        let output = CheckOutput {
            file_errors: vec!["failed to parse metadata.yml".to_string()],
            file_warnings: vec!["metadata.yaml ignored because metadata.yml exists".to_string()],
            ..CheckOutput::default()
        };
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("errors      1"));
        assert!(rendered.contains("warnings    1"));
        assert!(rendered.contains("validation: error"));
        assert!(!rendered.contains("validation: success"));
    }

    #[test]
    fn check_overview_counts_missing_frame_file_as_error() {
        let directory = temporary_test_directory("check-missing-frame-file-error");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  "missing.tif":
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("errors      1"));
        assert!(rendered.contains("warnings    0"));
        assert!(rendered.contains("error: file does not exist"));
        assert!(rendered.contains("validation: error"));
        assert!(!rendered.contains("warning: file does not exist"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_overview_colours_success_with_warnings() {
        colored::control::set_override(true);

        let output = CheckOutput {
            file_warnings: vec!["metadata.yaml ignored because metadata.yml exists".to_string()],
            ..CheckOutput::default()
        };
        let rendered = format_check_output(&output);

        assert!(rendered.contains("validation: \u{1b}[32msuccess\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[32msuccess\u{1b}[0m (with warnings)"));
    }

    #[test]
    fn check_overview_colours_nonzero_counts_only() {
        colored::control::set_override(true);

        let output = CheckOutput {
            file_errors: vec!["failed to parse metadata.yml".to_string()],
            file_warnings: vec!["metadata.yaml ignored because metadata.yml exists".to_string()],
            ..CheckOutput::default()
        };
        let rendered = format_check_output(&output);
        let clean = format_check_output(&CheckOutput::default());

        assert!(rendered.contains("errors      \u{1b}[31m1\u{1b}[0m"));
        assert!(rendered.contains("warnings    \u{1b}[33m1\u{1b}[0m"));
        assert!(clean.contains("errors      0"));
        assert!(clean.contains("warnings    0"));
        assert!(!clean.contains("\u{1b}[31m0\u{1b}[0m"));
        assert!(!clean.contains("\u{1b}[33m0\u{1b}[0m"));
    }

    #[test]
    fn check_output_renders_frame_blocks_in_yaml_order() {
        let directory = temporary_test_directory("check-frame-order");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  2:
    - FNumber: 2.8
  "image.jpg":
    - ExposureTime: 1/500
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("one.jpg"), []).expect("test image should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));
        let frame_two = rendered.find("← 2").expect("frame 2 should render");
        let frame_image = rendered
            .find("\nimage.jpg\n")
            .expect("frame image.jpg should render");

        assert!(frame_two < frame_image);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_prints_numeric_frame_summary_and_resolved_file() {
        let directory = temporary_test_directory("check-numeric-frame-file");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  1:
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        let first = directory.join("01.jpg");
        std::fs::write(&first, []).expect("first image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("frame numbers: 1"));
        assert!(rendered.contains("files: 1"));
        let frame = rendered.find("01.jpg ← 1").expect("frame should render");
        let standard_tags = rendered[frame..]
            .find("standard tags:")
            .map(|index| frame + index)
            .expect("standard tags should render");
        let unknown_tags = rendered[frame..]
            .find("unknown tags:")
            .map(|index| frame + index)
            .expect("unknown tags should render");

        assert!(frame < standard_tags);
        assert!(standard_tags < unknown_tags);
        assert!(!rendered.contains(&format!("file: {}", first.display())));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_warns_when_numeric_frames_exceed_files() {
        let directory = temporary_test_directory("check-more-frame-numbers");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  1:
    - FNumber: 2.8
  2:
    - ExposureTime: 1/500
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("01.jpg"), []).expect("image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("frame numbers: 2"));
        assert!(rendered.contains("files: 1"));
        assert!(
            rendered.contains("warning: there are more frame numbers (2) than image files (1)")
        );
        assert!(rendered.contains("\nframe 2\n"));
        assert!(rendered.contains("error: frame reference `2` does not match an image file"));
        assert!(!rendered.contains("warning: frame reference `2`"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_does_not_warn_when_files_exceed_numeric_frames() {
        let directory = temporary_test_directory("check-more-files");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  1:
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("01.jpg"), []).expect("first image should be written");
        std::fs::write(directory.join("02.jpg"), []).expect("second image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("frame numbers: 1"));
        assert!(rendered.contains("files: 2"));
        assert!(!rendered.contains("warning: supported image files"));
        assert!(!rendered.contains("warning: there are more frame numbers"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_omits_numeric_summary_for_filename_frames() {
        let directory = temporary_test_directory("check-filename-only");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  "image.jpg":
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("image should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(!rendered.contains("frame numbers:"));
        assert!(!rendered.contains("files:"));
        assert!(!rendered.contains("\nfile: "));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_skips_later_groups_after_parse_error() {
        let directory = temporary_test_directory("check-parse-error");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(&metadata, "exif: [").expect("metadata should be written");

        let output = build_check_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_check_output(&output));

        assert!(rendered.contains("YAML format: skipped"));
        assert!(rendered.contains("exif "));
        assert!(rendered.contains("frames "));
        assert_eq!(rendered.matches("skipped").count(), 3);
        assert!(rendered.contains("errors      1"));
        assert!(rendered.contains("validation: error"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_output_uses_expected_colours() {
        let directory = temporary_test_directory("check-colours");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  "image.jpg":
    - $Location: London
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");
        colored::control::set_override(true);

        let output = build_check_output(Some(&metadata));
        let rendered = format_check_output(&output);

        assert!(rendered.contains("\u{1b}[94mfile "));
        assert!(rendered.contains("\u{1b}[32mfound "));
        assert!(rendered.contains("\u{1b}[96mimage.jpg"));
        assert!(rendered.contains("location: \u{1b}[32mmatch found\u{1b}[0m [London"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn check_frame_titles_stay_light_blue_with_warnings_or_errors() {
        colored::control::set_override(true);

        let warning_frame = FrameReport {
            key: "image.jpg".to_string(),
            warnings: vec!["ignored malformed trailing field".to_string()],
            ..FrameReport::default()
        };
        let error_frame = FrameReport {
            key: "2".to_string(),
            is_numeric: true,
            warnings: vec!["ignored malformed trailing field".to_string()],
            errors: vec!["frame reference `2` does not match an image file".to_string()],
            ..FrameReport::default()
        };

        assert_eq!(
            check_frame_title(&warning_frame).bright_cyan().to_string(),
            "\u{1b}[96mimage.jpg\u{1b}[0m"
        );
        assert_eq!(
            check_frame_title(&error_frame).bright_cyan().to_string(),
            "\u{1b}[96mframe 2\u{1b}[0m"
        );
    }

    #[test]
    fn formats_empty_pretty_read_output() {
        colored::control::set_override(true);

        assert_eq!(
            format_read_output(
                Path::new("image.tif"),
                &ReadMetadata {
                    exif: parse_raw_exif(&[]),
                    warnings: Vec::new(),
                    file_info: ReadFileInfo::empty(),
                },
                ReadFormat::Pretty,
            ),
            "\u{1b}[33m<No EXIF metadata found>\u{1b}[0m"
        );
    }

    #[test]
    fn formats_empty_raw_read_output() {
        assert_eq!(
            format_read_output(
                Path::new("image.tif"),
                &ReadMetadata {
                    exif: parse_raw_exif(&[]),
                    warnings: Vec::new(),
                    file_info: test_file_info(),
                },
                ReadFormat::Raw,
            ),
            "<No EXIF metadata found>"
        );
    }

    #[test]
    fn formats_raw_read_output_with_unknown_rows_at_the_bottom() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif(&[
                tiff_ascii_entry(0x010f, b"Z\0"),
                tiff_short_entry(0xfde8, 42),
                tiff_ascii_entry(0x0110, b"E\0"),
            ]),
            warnings: vec!["ignored malformed trailing field".to_string()],
            file_info: test_file_info(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Raw);

        assert!(!output.contains("KNOWN"));
        assert!(!output.contains("UNKNOWN"));
        assert!(output.contains("IFD"));
        assert!(!output.contains("File Name"));
        assert!(output.contains("0x010F"));
        assert!(output.contains("0x0110"));
        assert!(output.contains("0xFDE8"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(output.find("0x0110").unwrap() < output.find("0xFDE8").unwrap());
    }

    #[test]
    fn formats_pretty_read_output_without_raw_columns() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif(&[
                tiff_ascii_entry(0x010f, b"Z\0"),
                tiff_short_entry(0xfde8, 42),
                tiff_ascii_entry(0x0110, b"E\0"),
            ]),
            warnings: vec!["ignored malformed trailing field".to_string()],
            file_info: test_file_info(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.contains("file "));
        assert!(plain_output.contains("File Name  image.tif"));
        assert!(plain_output.contains("camera "));
        assert!(plain_output.contains("Make   Z\nModel  E"));
        assert!(!plain_output.contains("unknown "));
        assert!(!plain_output.contains("Unknown Tiff Tag"));
        assert!(!plain_output.contains("tags were omitted for not being human-readable"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(plain_output.find("file ").unwrap() < plain_output.find("camera ").unwrap());
        assert!(plain_output.find("camera ").unwrap() < plain_output.find("Warnings:").unwrap());
        assert!(!output.contains("IFD"));
        assert!(!output.contains("Tiff  "));
        assert!(!output.contains("0x010F"));
        assert!(!output.contains("0x0110"));
        assert!(!output.contains("0xFDE8"));
    }

    #[test]
    fn pretty_read_output_omits_non_human_readable_tags_without_notice() {
        colored::control::set_override(true);
        let metadata = ReadMetadata {
            exif: parse_raw_exif(&[
                tiff_ascii_entry(0x010f, b"Z\0"),
                tiff_long_entry(0x0111, 12345),
                tiff_long_entry(0x0117, 67890),
            ]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let pretty = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);
        colored::control::set_override(false);
        let plain_pretty = strip_ansi_codes(&pretty);
        let raw = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Raw,
        ));

        assert!(plain_pretty.contains("Make  Z"));
        assert!(!plain_pretty.contains("Strip Offsets"));
        assert!(!plain_pretty.contains("Strip Byte Counts"));
        assert!(!plain_pretty.contains("12345"));
        assert!(!plain_pretty.contains("67890"));
        assert!(!plain_pretty.contains("Warnings:"));
        assert!(!plain_pretty.contains("warning:"));
        assert!(!plain_pretty.contains("tags were omitted for not being human-readable"));
        assert!(!pretty.contains("tags were omitted for not being human-readable"));
        assert!(raw.contains("StripOffsets"));
        assert!(raw.contains("StripByteCounts"));
        assert!(raw.contains("Make"));
        assert!(raw.contains("\"Z\""));
        assert!(raw.contains("12345"));
        assert!(raw.contains("67890"));
    }

    #[test]
    fn pretty_read_output_omits_unknown_values() {
        colored::control::set_override(true);
        let long_value = "x".repeat(160);
        let mut raw_value = long_value.clone().into_bytes();
        raw_value.push(0);
        let entry = tiff_ascii_offset_entry(0xfde8, raw_value.len(), 200);
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_offsets(&[entry], &[(200, raw_value)]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let pretty = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);
        let plain_pretty = strip_ansi_codes(&pretty);
        let raw = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Raw,
        ));
        let omitted_message = format!(
            "{}{}",
            PRETTY_UNKNOWN_VALUE_OMITTED_LABEL, PRETTY_UNKNOWN_VALUE_OMITTED_HINT
        );

        assert!(!plain_pretty.contains("unknown "));
        assert!(!plain_pretty.contains("Unknown Tiff Tag"));
        assert!(!plain_pretty.contains(&omitted_message));
        assert!(!plain_pretty.contains("tags were omitted for not being human-readable"));
        assert!(!pretty.contains("tags were omitted for not being human-readable"));
        assert!(!plain_pretty.contains(&long_value));
        assert!(raw.contains(&long_value));
        assert!(!raw.contains(&omitted_message));
    }

    #[test]
    fn pretty_read_output_keeps_unknown_values_at_display_limit() {
        let value = "x".repeat(PRETTY_UNKNOWN_VALUE_DISPLAY_LIMIT);
        let row = ReadRow {
            is_unknown: true,
            ifd: 0,
            context: "Tiff".to_string(),
            tag_id: 0xfde8,
            name: "Tag(Tiff, 0xFDE8)".to_string(),
            pretty_name: "Unknown Tiff Tag".to_string(),
            value: value.clone(),
        };

        assert_eq!(pretty_read_value(&row), value);
    }

    #[test]
    fn pretty_read_output_masks_long_known_exif_values() {
        colored::control::set_override(true);
        let long_value = "x".repeat(PRETTY_KNOWN_VALUE_DISPLAY_LIMIT + 1);
        let mut raw_value = USER_COMMENT_ASCII_PREFIX.to_vec();
        raw_value.extend_from_slice(long_value.as_bytes());
        let entry = tiff_undefined_entry(0x9286, raw_value.len(), 200);
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(&[entry], &[(200, raw_value)]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let pretty = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);
        let plain_pretty = strip_ansi_codes(&pretty);
        let raw = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Raw,
        ));
        let omitted_message = format!(
            "{}{}",
            PRETTY_UNKNOWN_VALUE_OMITTED_LABEL, PRETTY_UNKNOWN_VALUE_OMITTED_HINT
        );

        assert!(plain_pretty.contains("User Comment"));
        assert!(plain_pretty.contains(&omitted_message));
        assert!(pretty.contains(&format!(
            "\u{1b}[33m{}\u{1b}[0m{}",
            PRETTY_UNKNOWN_VALUE_OMITTED_LABEL, PRETTY_UNKNOWN_VALUE_OMITTED_HINT
        )));
        assert!(!plain_pretty.contains(&long_value));
        assert!(raw.contains(&long_value));
        assert!(!raw.contains(&omitted_message));
    }

    #[test]
    fn pretty_read_output_keeps_known_exif_values_at_display_limit() {
        let value = "x".repeat(PRETTY_KNOWN_VALUE_DISPLAY_LIMIT);
        let row = ReadRow {
            is_unknown: false,
            ifd: 0,
            context: "Exif".to_string(),
            tag_id: 0x9286,
            name: "UserComment".to_string(),
            pretty_name: "User Comment".to_string(),
            value: value.clone(),
        };

        assert_eq!(pretty_read_value(&row), value);
    }

    #[test]
    fn pretty_read_output_omits_empty_groups() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif(&[tiff_ascii_entry(0x010f, b"Z\0")]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.starts_with("camera "));
        assert!(!plain_output.contains("file "));
        assert!(!plain_output.contains("film "));
        assert!(!plain_output.contains("exposure "));
        assert!(!plain_output.contains("gps "));
        assert!(!plain_output.contains("misc "));
        assert!(!plain_output.contains("unknown "));
    }

    #[test]
    fn pretty_read_group_heading_uses_check_style_blue_rule() {
        colored::control::set_override(true);
        let metadata = ReadMetadata {
            exif: parse_raw_exif(&[tiff_ascii_entry(0x010f, b"Z\0")]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);
        colored::control::set_override(false);
        let expected = format!("\u{1b}[94mcamera {}\u{1b}[0m", "─".repeat(43));

        assert_eq!(output.lines().next(), Some(expected.as_str()));
    }

    #[test]
    fn classifies_extra_file_info_rows_as_file() {
        for name in [
            "File Access Date/Time",
            "fileaccessdate/time",
            "File Creation Date/Time",
            "File Permissions",
            "File Type",
            "File Type Extension",
            "file type extension",
            "MIME Type",
            "MIMEType",
        ] {
            let row = ReadInfoRow::new(name, "value");

            assert!(matches!(classify_info_row(&row), PrettyReadGroup::File));
        }
    }

    #[test]
    fn classifies_extra_camera_and_exposure_labels() {
        for label in [
            "FocalLengthIn35mmFilm",
            "Focal Length In 35mm Film",
            "focal length in 35mm film",
            "focallengthin35mmfilm",
        ] {
            assert!(is_camera_label(label));
        }

        for label in [
            "ExposureMode",
            "Exposure Mode",
            "exposure mode",
            "ExposureProgram",
            "Exposure Program",
            "exposureprogram",
            "PhotographicSensitivity",
            "Photographic Sensitivity",
            "SensitivityType",
            "Sensitivity Type",
            "sensitivity type",
        ] {
            assert!(is_exposure_label(label));
        }
    }

    #[test]
    fn normalized_label_classifiers_ignore_spacing_and_case() {
        assert!(is_file_label("file type"));
        assert!(!is_file_label("File Source"));
        assert!(is_camera_label("lens model"));
        assert!(is_camera_label("CAMERA SERIAL NUMBER"));
        assert!(is_film_label("Analogue Data Film Name"));
        assert!(is_film_label("analoguedatafilmname"));
        assert!(is_exposure_label("shutter speed value"));
        assert!(normalized_label_starts_with("G P S Latitude", &["gps"]));
    }

    #[test]
    fn pretty_read_output_deduplicates_identical_ifd_0_and_1_rows_only() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_ifd1(
                &[tiff_ascii_entry(0x010f, b"A\0")],
                &[
                    tiff_ascii_entry(0x010f, b"A\0"),
                    tiff_ascii_entry(0x0110, b"B\0"),
                ],
            ),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);

        assert_eq!(output.matches("Make").count(), 1);
        assert_eq!(output.matches("A").count(), 1);
        assert!(output.contains("Model  B"));
    }

    #[test]
    fn pretty_read_output_keeps_ifd_0_and_1_rows_with_different_values() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_ifd1(
                &[tiff_ascii_entry(0x010f, b"A\0")],
                &[tiff_ascii_entry(0x010f, b"B\0")],
            ),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);

        assert_eq!(output.matches("Make").count(), 2);
        assert!(output.contains("Make  A"));
        assert!(output.contains("Make  B"));
    }

    #[test]
    fn pretty_read_output_omits_photographic_sensitivity_duplicate_of_iso_speed() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(
                &[tiff_short_entry(0x8827, 400), tiff_long_entry(0x8833, 400)],
                &[],
            ),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let pretty = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Pretty,
        ));
        let raw = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Raw,
        ));

        assert!(pretty.contains("ISO Speed"));
        assert!(!pretty.contains("Photographic Sensitivity"));
        assert!(raw.contains("PhotographicSensitivity"));
        assert!(raw.contains("ISOSpeed"));
    }

    #[test]
    fn pretty_read_output_keeps_photographic_sensitivity_when_iso_speed_differs() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(
                &[tiff_short_entry(0x8827, 400), tiff_long_entry(0x8833, 800)],
                &[],
            ),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let pretty = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Pretty,
        ));

        assert!(pretty.contains("Photographic Sensitivity  400"));
        assert!(pretty.contains("ISO Speed                 800"));
    }

    #[test]
    fn pretty_read_output_keeps_photographic_sensitivity_without_iso_speed() {
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(&[tiff_short_entry(0x8827, 400)], &[]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let pretty = strip_ansi_codes(&format_read_output(
            Path::new("image.tif"),
            &metadata,
            ReadFormat::Pretty,
        ));

        assert!(pretty.contains("Photographic Sensitivity  400"));
    }

    #[test]
    fn de_camelcases_tag_names_for_pretty_output() {
        assert_eq!(title_case_tag_name("GPSLatitude"), "GPS Latitude");
        assert_eq!(
            title_case_tag_name("DateTimeOriginal"),
            "Date Time Original"
        );
        assert_eq!(
            title_case_tag_name("FocalLengthIn35mmFilm"),
            "Focal Length In 35mm Film"
        );
    }

    #[test]
    fn formats_exposure_time_for_pretty_output() {
        assert_eq!(
            pretty_exposure_time("1/1439.2133835330962 s"),
            Some("1/1439".to_string())
        );
        assert_eq!(
            pretty_exposure_time("1/1439.2133835330962"),
            Some("1/1439".to_string())
        );
        assert_eq!(pretty_exposure_time("1/500 s"), Some("1/500".to_string()));
        assert_eq!(pretty_exposure_time("0.5 s"), None);
    }

    #[test]
    fn raw_output_omits_exposure_time_unit() {
        let (exposure_entry, exposure_data) =
            tiff_rational_entry_with_count(0x829a, &[(1, 1439)], 200);
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(&[exposure_entry], &[(200, exposure_data)]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Raw);

        assert!(output.contains("ExposureTime"));
        assert!(output.contains("1/1439"));
        assert!(!output.contains("1/1439 s"));
    }

    #[test]
    fn pretty_output_rounds_exposure_time_reciprocal_denominator() {
        let (exposure_entry, exposure_data) =
            tiff_rational_entry_with_count(0x829a, &[(10_000, 14_392_134)], 200);
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_exif_entries(&[exposure_entry], &[(200, exposure_data)]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.tif"), &metadata, ReadFormat::Pretty);

        assert!(output.contains("Exposure Time"));
        assert!(output.contains("1/1439"));
        assert!(!output.contains("1/1439.2134"));
        assert!(!output.contains("1/1439 s"));
    }

    #[test]
    fn reads_unknown_tags_without_failing() {
        let exif = parse_raw_exif(&[tiff_short_entry(0xfde8, 42)]);
        let fields = exif.fields().collect::<Vec<_>>();

        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].tag, Tag(Context::Tiff, 0xfde8));
        assert!(fields[0].tag.description().is_none());
    }

    #[test]
    fn file_info_reports_file_and_exif_metadata() {
        let path = temporary_test_path("file-info.jpg");
        std::fs::write(&path, [0xff, 0xd8, 0xff]).expect("test image should be written");
        let exif = parse_raw_exif(&[]);
        let file_info =
            ReadFileInfo::from_path(&path, &exif).expect("file info should be collected");
        let rows = file_info.rows;

        assert!(info_row_value(&rows, "Exifmeta Version Number").is_none());
        let expected_file_name = path.file_name().and_then(|name| name.to_str());
        assert_eq!(info_row_value(&rows, "File Name"), expected_file_name);
        assert!(info_row_value(&rows, "Directory").is_some());
        assert_eq!(info_row_value(&rows, "File Size"), Some("3 bytes"));
        assert!(info_row_value(&rows, "File Permissions").is_some());
        assert_eq!(info_row_value(&rows, "File Type"), Some("JPEG"));
        assert_eq!(info_row_value(&rows, "File Type Extension"), Some("jpg"));
        assert_eq!(info_row_value(&rows, "MIME Type"), Some("image/jpeg"));
        assert_eq!(
            info_row_value(&rows, "Exif Byte Order"),
            Some("Big-endian (Motorola, MM)")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn appends_signed_decimal_gps_coordinates() {
        let metadata = test_gps_metadata();

        let output = format_read_output(Path::new("image.jpg"), &metadata, ReadFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert!(output.contains("GPS Latitude"));
        assert!(output.contains("(52.352832) 52 deg 21 min 10.1952 sec N"));
        assert!(output.contains("GPS Longitude"));
        assert!(output.contains("(-1.304089) 1 deg 18 min 14.71968 sec W"));
        assert!(!output.contains("GPS Latitude Ref"));
        assert!(!output.contains("GPS Longitude Ref"));
        assert!(!plain_output.contains("tags were omitted for not being human-readable"));
    }

    #[test]
    fn extracts_signed_gps_coordinates() {
        let metadata = test_gps_metadata();

        let (latitude, longitude) =
            gps_coordinates(&metadata.exif).expect("GPS coordinates should be extracted");
        assert!((latitude - 52.352832).abs() < 0.000001);
        assert!((longitude + 1.3040888).abs() < 0.000001);
    }

    #[test]
    fn pretty_read_output_appends_nearest_locations() {
        colored::control::set_override(true);
        let metadata = test_gps_metadata();

        let output = format_read_output(Path::new("image.jpg"), &metadata, ReadFormat::Pretty);
        colored::control::set_override(false);
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.contains("gps "));
        assert_eq!(plain_output.matches("GPS Nearest Location").count(), 5);
        assert_eq!(plain_output.matches("GPS Nearest City").count(), 1);
        assert_eq!(plain_output.matches("GPS Nearest Large City").count(), 0);
        assert!(plain_output.contains("GPS Nearest Location 1  (1.9 km) Dunchurch, GB"));
        assert!(plain_output.contains("GPS Nearest Location 2  (3.2 km) Long Lawford, GB"));
        assert!(plain_output.contains("GPS Nearest City        (15.3 km) Coventry, GB"));
        assert!(
            plain_output
                .find("GPS Nearest Location 5")
                .expect("nearest location rows should be present")
                < plain_output
                    .find("GPS Nearest City")
                    .expect("nearest city rows should be present")
        );
        assert!(output.contains("\u{1b}[32mGPS Nearest Location 1\u{1b}[0m"));
        assert!(output.contains("\u{1b}[32mGPS Nearest City\u{1b}[0m"));
        assert!(!output.contains("\u{1b}[32mGPS Nearest Large City\u{1b}[0m"));
    }

    #[test]
    fn pretty_read_output_omits_nearest_city_when_it_duplicates_nearest_location() {
        let metadata = test_coventry_gps_metadata();

        let output = format_read_output(Path::new("image.jpg"), &metadata, ReadFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert_eq!(plain_output.matches("GPS Nearest Location").count(), 5);
        assert_eq!(plain_output.matches("GPS Nearest City").count(), 0);
        assert_eq!(plain_output.matches("Coventry, GB").count(), 1);
        assert!(plain_output.contains("GPS Nearest Location 1  (0 m) Coventry, GB"));
    }

    #[test]
    fn raw_read_output_omits_nearest_locations() {
        let metadata = test_gps_metadata();

        let output = format_read_output(Path::new("image.jpg"), &metadata, ReadFormat::Raw);

        assert!(output.contains("GPSLatitude"));
        assert!(output.contains("GPSLatitudeRef"));
        assert!(output.contains("GPSLongitudeRef"));
        assert!(!output.contains("GPS Nearest Location"));
        assert!(!output.contains("GPS Nearest City"));
        assert!(!output.contains("GPS Nearest Large City"));
        assert!(!output.contains("Dunchurch"));
        assert!(!output.contains("Coventry"));
    }

    #[test]
    fn appends_unsigned_decimal_gps_coordinate_when_ref_is_missing() {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(10, 1), (30, 1), (0, 1)], 200);
        let metadata = ReadMetadata {
            exif: parse_raw_exif_with_gps_entries(&[latitude_entry], &[(200, latitude_data)]),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        };

        let output = format_read_output(Path::new("image.jpg"), &metadata, ReadFormat::Pretty);

        assert!(output.contains("GPS Latitude"));
        assert!(output.contains("[GPSLatitudeRef missing]"));
        assert!(output.contains("(10.5) 10 deg 30 min 0 sec [GPSLatitudeRef missing]"));
        assert!(!output.contains("GPS Nearest Location"));
    }

    #[test]
    fn formats_metric_distances() {
        assert_eq!(format_distance(0.0123), "12 m");
        assert_eq!(format_distance(1.234), "1.2 km");
    }

    #[test]
    fn candidate_locations_filters_by_strict_minimum_population() {
        let connection = Connection::open_in_memory().expect("test database should open");
        connection
            .execute_batch(
                "
                CREATE TABLE locations (
                    geoname_id INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    country_code TEXT NOT NULL,
                    latitude REAL NOT NULL,
                    longitude REAL NOT NULL,
                    population INTEGER NOT NULL,
                    elevation INTEGER
                );
                INSERT INTO locations VALUES
                    (1, 'Small Place', 'AA', 0.0, 0.0, 199999, NULL),
                    (2, 'Equal Place', 'AA', 0.0, 0.1, 200000, NULL),
                    (3, 'City', 'AA', 0.0, 0.2, 200001, NULL),
                    (4, 'Larger City', 'AA', 0.0, 0.3, 300000, NULL);
                ",
            )
            .expect("test locations should be inserted");

        let locations = candidate_locations(&connection, 0.0, 0.0, 50.0, Some(200_000))
            .expect("population-filtered query should succeed");

        let names = locations
            .iter()
            .map(|location| location.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["City", "Larger City"]);
        assert!(
            locations
                .iter()
                .all(|location| location.population > 200_000)
        );
    }

    #[test]
    fn locations_by_name_matches_case_insensitively() {
        let connection = test_geonames_connection();

        let locations =
            locations_by_name(&connection, "london").expect("location lookup should succeed");

        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].name, "London");
        assert_eq!(locations[0].country_code, "GB");
        assert_eq!(locations[0].latitude, 51.50853);
        assert_eq!(locations[0].longitude, -0.12574);
    }

    #[test]
    fn locations_by_name_sorts_matches_by_population_descending() {
        let connection = test_geonames_connection();

        let locations =
            locations_by_name(&connection, "London").expect("location lookup should succeed");
        let countries = locations
            .iter()
            .map(|location| location.country_code.as_str())
            .collect::<Vec<_>>();

        assert_eq!(countries, ["GB", "CA"]);
    }

    #[test]
    fn locations_by_name_returns_empty_list_when_no_match_exists() {
        let connection = test_geonames_connection();

        let locations =
            locations_by_name(&connection, "Nowhere").expect("location lookup should succeed");

        assert!(locations.is_empty());
    }

    #[test]
    fn reads_jpeg_without_exif_as_empty_metadata() {
        colored::control::set_override(true);

        let path = temporary_test_path("no-exif.jpg");
        std::fs::write(&path, [0xff, 0xd8, 0xff, 0xd9]).expect("test JPEG should be written");

        let metadata = read_metadata(&path).expect("missing EXIF should not fail read");

        let pretty = format_read_output(&path, &metadata, ReadFormat::Pretty);
        assert!(
            pretty == "\u{1b}[33m<No EXIF metadata found>\u{1b}[0m"
                || pretty == "<No EXIF metadata found>"
        );
        assert_eq!(
            format_read_output(&path, &metadata, ReadFormat::Raw),
            "<No EXIF metadata found>"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_missing_image_path() {
        let missing = Path::new("definitely-missing-image.tif");

        assert_eq!(
            check_image_path(missing),
            Err("image does not exist: definitely-missing-image.tif".to_string())
        );
    }

    #[test]
    fn rejects_directory_image_path() {
        assert_eq!(
            check_image_path(Path::new(".")),
            Err("image path is not a file: .".to_string())
        );
    }

    fn test_gps_metadata() -> ReadMetadata {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(52, 1), (21, 1), (101952, 10000)], 200);
        let (longitude_entry, longitude_data) =
            tiff_rational_entry(0x0004, [(1, 1), (18, 1), (1471968, 100000)], 224);

        ReadMetadata {
            exif: parse_raw_exif_with_gps_entries(
                &[
                    tiff_ascii_entry(0x0001, b"N\0"),
                    latitude_entry,
                    tiff_ascii_entry(0x0003, b"W\0"),
                    longitude_entry,
                ],
                &[(200, latitude_data), (224, longitude_data)],
            ),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        }
    }

    fn test_coventry_gps_metadata() -> ReadMetadata {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(52, 1), (24, 1), (23616, 1000)], 200);
        let (longitude_entry, longitude_data) =
            tiff_rational_entry(0x0004, [(1, 1), (30, 1), (43812, 1000)], 224);

        ReadMetadata {
            exif: parse_raw_exif_with_gps_entries(
                &[
                    tiff_ascii_entry(0x0001, b"N\0"),
                    latitude_entry,
                    tiff_ascii_entry(0x0003, b"W\0"),
                    longitude_entry,
                ],
                &[(200, latitude_data), (224, longitude_data)],
            ),
            warnings: Vec::new(),
            file_info: ReadFileInfo::empty(),
        }
    }

    fn test_file_info() -> ReadFileInfo {
        ReadFileInfo {
            rows: vec![ReadInfoRow::new("File Name", "image.tif")],
        }
    }

    fn info_row_value<'a>(rows: &'a [ReadInfoRow], name: &str) -> Option<&'a str> {
        rows.iter()
            .find(|row| row.name == name)
            .map(|row| row.value.as_str())
    }

    fn write_test_exif_tags<const N: usize>(image: &Path, tags: [(&str, &str); N]) {
        std::fs::write(image, [0xff, 0xd8, 0xff, 0xd9]).expect("test JPEG should be written");
        let tags = tags
            .into_iter()
            .map(|(name, value)| WriteTag {
                name: name.to_string(),
                value: YamlValue::String(value.to_string()),
            })
            .collect::<Vec<_>>();
        let args = WriteArgs {
            metadata_or_targets: None,
            targets: None,
            strip: false,
            keep: Vec::new(),
            remove: Vec::new(),
            privacy: false,
            no_overwrite: false,
            extensions: Vec::new(),
            recursive: false,
        };
        let result = apply_tags_to_image(image, &tags, false, &args, None);
        assert!(result.errors.is_empty(), "{:?}", result.errors);
    }

    fn default_write_args() -> WriteArgs {
        WriteArgs {
            metadata_or_targets: None,
            targets: None,
            strip: false,
            keep: Vec::new(),
            remove: Vec::new(),
            privacy: false,
            no_overwrite: false,
            extensions: Vec::new(),
            recursive: false,
        }
    }

    fn exif_tag_names(image: &Path) -> HashSet<String> {
        read_metadata(image)
            .expect("EXIF metadata should be readable")
            .exif
            .fields()
            .map(|field| field.tag.to_string())
            .collect()
    }

    fn exif_tag_ids(image: &Path) -> HashSet<u16> {
        read_metadata(image)
            .expect("EXIF metadata should be readable")
            .exif
            .fields()
            .map(|field| field.tag.number())
            .collect()
    }

    fn strip_ansi_codes(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();

        while let Some(char) = chars.next() {
            if char == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for char in chars.by_ref() {
                    if char == 'm' {
                        break;
                    }
                }
            } else {
                output.push(char);
            }
        }

        output
    }

    fn temporary_test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("exifmeta-{}-{name}", std::process::id()))
    }

    fn temporary_test_directory(name: &str) -> std::path::PathBuf {
        let path = temporary_test_path(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("test directory should be created");
        path
    }

    fn write_synthetic_tiff_with_predictor(path: &Path) {
        let mut entries = vec![
            test_tiff_long_entry_data(0x0100, 1),
            test_tiff_long_entry_data(0x0101, 1),
            test_tiff_short_values_entry_data(0x0102, &[8, 8, 8]),
            test_tiff_short_entry_data(0x0103, 5),
            test_tiff_short_entry_data(0x0106, 2),
            test_tiff_ascii_entry_data(0x010f, b"Nikon\0"),
            test_tiff_ascii_entry_data(0x0110, b"F3\0"),
            test_tiff_long_entry_data(0x0111, 0),
            test_tiff_short_entry_data(0x0112, 1),
            test_tiff_short_entry_data(0x0115, 3),
            test_tiff_long_entry_data(0x0116, 1),
            test_tiff_long_entry_data(0x0117, 4),
            test_tiff_rational_entry_data(0x011a, 300, 1),
            test_tiff_rational_entry_data(0x011b, 300, 1),
            test_tiff_short_entry_data(0x011c, 1),
            test_tiff_short_entry_data(0x0128, 2),
            test_tiff_ascii_entry_data(0x0131, b"test software\0"),
            test_tiff_ascii_entry_data(0x0132, b"2026:05:27 12:00:00\0"),
            test_tiff_short_entry_data(0x013d, 2),
            test_tiff_undefined_entry_data(0x8773, &[1, 2, 3, 4]),
        ];
        entries.sort_by_key(|entry| entry.0);

        let base_data_offset = 8 + 2 + entries.len() * 12 + 4;
        let offset_data_len = entries
            .iter()
            .filter(|entry| entry.3.len() > 4)
            .map(|entry| entry.3.len())
            .sum::<usize>();
        let pixel_offset = (base_data_offset + offset_data_len) as u32;
        for entry in &mut entries {
            if entry.0 == 0x0111 {
                entry.3 = pixel_offset.to_be_bytes().to_vec();
            }
        }

        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&(entries.len() as u16).to_be_bytes());

        let mut offset_chunks = Vec::new();
        let mut current_offset = base_data_offset as u32;
        for (tag, type_id, count, value) in entries {
            data.extend_from_slice(&tag.to_be_bytes());
            data.extend_from_slice(&type_id.to_be_bytes());
            data.extend_from_slice(&count.to_be_bytes());
            if value.len() <= 4 {
                data.extend_from_slice(&value);
                data.resize(data.len() + (4 - value.len()), 0);
            } else {
                data.extend_from_slice(&current_offset.to_be_bytes());
                current_offset += value.len() as u32;
                offset_chunks.push(value);
            }
        }
        data.extend_from_slice(&[0, 0, 0, 0]);
        for chunk in offset_chunks {
            data.extend_from_slice(&chunk);
        }
        assert_eq!(data.len(), pixel_offset as usize);
        data.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);

        std::fs::write(path, data).expect("synthetic TIFF should be written");
    }

    fn test_tiff_ascii_entry_data(tag: u16, value: &[u8]) -> (u16, u16, u32, Vec<u8>) {
        (tag, 2, value.len() as u32, value.to_vec())
    }

    fn test_tiff_short_entry_data(tag: u16, value: u16) -> (u16, u16, u32, Vec<u8>) {
        (tag, 3, 1, value.to_be_bytes().to_vec())
    }

    fn test_tiff_short_values_entry_data(tag: u16, values: &[u16]) -> (u16, u16, u32, Vec<u8>) {
        let mut bytes = Vec::new();
        for value in values {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        (tag, 3, values.len() as u32, bytes)
    }

    fn test_tiff_long_entry_data(tag: u16, value: u32) -> (u16, u16, u32, Vec<u8>) {
        (tag, 4, 1, value.to_be_bytes().to_vec())
    }

    fn test_tiff_rational_entry_data(
        tag: u16,
        numerator: u32,
        denominator: u32,
    ) -> (u16, u16, u32, Vec<u8>) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&numerator.to_be_bytes());
        bytes.extend_from_slice(&denominator.to_be_bytes());
        (tag, 5, 1, bytes)
    }

    fn test_tiff_undefined_entry_data(tag: u16, value: &[u8]) -> (u16, u16, u32, Vec<u8>) {
        (tag, 7, value.len() as u32, value.to_vec())
    }

    fn test_geonames_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("test database should open");
        connection
            .execute_batch(
                "
                CREATE TABLE locations (
                    geoname_id INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    country_code TEXT NOT NULL,
                    latitude REAL NOT NULL,
                    longitude REAL NOT NULL,
                    population INTEGER NOT NULL,
                    elevation INTEGER
                );
                INSERT INTO locations VALUES
                    (1, 'London', 'GB', 51.50853, -0.12574, 8961989, 25),
                    (2, 'London', 'CA', 42.98339, -81.23304, 383822, 251),
                    (3, 'Paris', 'FR', 48.85341, 2.3488, 2138551, 42);
                ",
            )
            .expect("test locations should be inserted");
        connection
    }

    fn parse_raw_exif(entries: &[[u8; 12]]) -> Exif {
        parse_raw_exif_with_offsets(entries, &[])
    }

    fn parse_raw_exif_with_offsets(entries: &[[u8; 12]], offset_data: &[(u32, Vec<u8>)]) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for entry in entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut offset_data = offset_data.to_vec();
        offset_data.sort_by_key(|(offset, _)| *offset);
        for (offset, bytes) in offset_data {
            let offset = offset as usize;
            if data.len() < offset {
                data.resize(offset, 0);
            }
            data.extend_from_slice(&bytes);
        }

        Reader::new()
            .continue_on_error(true)
            .read_raw(data)
            .or_else(|error| error.distill_partial_result(|_| {}))
            .expect("test EXIF should parse")
    }

    fn parse_raw_exif_with_ifd1(ifd0_entries: &[[u8; 12]], ifd1_entries: &[[u8; 12]]) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&(ifd0_entries.len() as u16).to_be_bytes());
        for entry in ifd0_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&100u32.to_be_bytes());
        data.resize(100, 0);
        data.extend_from_slice(&(ifd1_entries.len() as u16).to_be_bytes());
        for entry in ifd1_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        Reader::new()
            .continue_on_error(true)
            .read_raw(data)
            .or_else(|error| error.distill_partial_result(|_| {}))
            .expect("test EXIF should parse")
    }

    fn parse_raw_exif_with_gps_entries(
        gps_entries: &[[u8; 12]],
        offset_data: &[(u32, Vec<u8>)],
    ) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&tiff_long_entry(0x8825, 100));
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        data.resize(100, 0);
        data.extend_from_slice(&(gps_entries.len() as u16).to_be_bytes());
        for entry in gps_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut offset_data = offset_data.to_vec();
        offset_data.sort_by_key(|(offset, _)| *offset);
        for (offset, bytes) in offset_data {
            let offset = offset as usize;
            if data.len() < offset {
                data.resize(offset, 0);
            }
            data.extend_from_slice(&bytes);
        }

        Reader::new()
            .continue_on_error(true)
            .read_raw(data)
            .or_else(|error| error.distill_partial_result(|_| {}))
            .expect("test EXIF should parse")
    }

    fn parse_raw_exif_with_exif_entries(
        exif_entries: &[[u8; 12]],
        offset_data: &[(u32, Vec<u8>)],
    ) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&tiff_long_entry(0x8769, 100));
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        data.resize(100, 0);
        data.extend_from_slice(&(exif_entries.len() as u16).to_be_bytes());
        for entry in exif_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut offset_data = offset_data.to_vec();
        offset_data.sort_by_key(|(offset, _)| *offset);
        for (offset, bytes) in offset_data {
            let offset = offset as usize;
            if data.len() < offset {
                data.resize(offset, 0);
            }
            data.extend_from_slice(&bytes);
        }

        Reader::new()
            .continue_on_error(true)
            .read_raw(data)
            .or_else(|error| error.distill_partial_result(|_| {}))
            .expect("test EXIF should parse")
    }

    fn tiff_ascii_entry(tag: u16, value: &[u8]) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&2u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(value.len() as u32).to_be_bytes());
        entry[8..(8 + value.len())].copy_from_slice(value);
        entry
    }

    fn tiff_ascii_offset_entry(tag: u16, length: usize, offset: u32) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&2u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(length as u32).to_be_bytes());
        entry[8..12].copy_from_slice(&offset.to_be_bytes());
        entry
    }

    fn tiff_short_entry(tag: u16, value: u16) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&3u16.to_be_bytes());
        entry[4..8].copy_from_slice(&1u32.to_be_bytes());
        entry[8..10].copy_from_slice(&value.to_be_bytes());
        entry
    }

    fn tiff_long_entry(tag: u16, value: u32) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&4u16.to_be_bytes());
        entry[4..8].copy_from_slice(&1u32.to_be_bytes());
        entry[8..12].copy_from_slice(&value.to_be_bytes());
        entry
    }

    fn tiff_undefined_entry(tag: u16, length: usize, offset: u32) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&7u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(length as u32).to_be_bytes());
        entry[8..12].copy_from_slice(&offset.to_be_bytes());
        entry
    }

    fn tiff_rational_entry(tag: u16, values: [(u32, u32); 3], offset: u32) -> ([u8; 12], Vec<u8>) {
        let (entry, _) = tiff_rational_entry_with_count(tag, &values, offset);
        (entry, tiff_rational_data(&values))
    }

    fn tiff_rational_entry_with_count(
        tag: u16,
        values: &[(u32, u32)],
        offset: u32,
    ) -> ([u8; 12], Vec<u8>) {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&5u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(values.len() as u32).to_be_bytes());
        entry[8..12].copy_from_slice(&offset.to_be_bytes());

        (entry, tiff_rational_data(values))
    }

    fn tiff_rational_data(values: &[(u32, u32)]) -> Vec<u8> {
        let mut data = Vec::new();
        for &(numerator, denominator) in values {
            data.extend_from_slice(&numerator.to_be_bytes());
            data.extend_from_slice(&denominator.to_be_bytes());
        }

        data
    }
}
