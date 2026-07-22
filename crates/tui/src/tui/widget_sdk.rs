//! Ghosty Widget SDK — schema-driven overlay & plugin system.
//!
//! Architecture (inspired by Ghostty's apprt + Ghostty Config's registry):
//!   `WidgetManifest`  — declarative widget definition (like Ghostty Config settings)
//!   `WidgetRegistry`  — auto-discovers widgets from `~/.ghosty/widgets/`
//!   `OverlayHost`     — owns instances, routes mouse/key events, renders
//!
//! ## Creating a widget plugin
//!
//! Drop a TOML manifest in `~/.ghosty/widgets/<name>/manifest.toml`:
//!
//! ```toml
//! [widget]
//! id = "my-panel"
//! title = "My Panel"
//! kind = "script"                # "script" | "file-watch" | "mcp-view"
//! hotkey = "m"                    # Alt-<key> to toggle
//! width = 40
//! height = 12
//!
//! [widget.script]
//! command = "bash"
//! args = ["-c", "echo 'hello from plugin'"]
//! interval_secs = 5              # re-run every N seconds
//! ```
//!
//! ## Built-in widget kinds
//!
//! | Kind | Description |
//! |------|-------------|
//! | `script` | Runs a command; stdout becomes widget content |
//! | `file-watch` | Watches a file; renders markdown checkboxes |
//! | `mcp-view` | Queries an MCP server tool; renders JSON result |
//! | `log-tail` | Tails a file in real-time |
//! | `static` | Displays static markdown text |

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

// ═════════════════════════════════════════════════════════════════════
// 1. Widget Manifest — declarative schema (like Ghostty Config registry)
// ═════════════════════════════════════════════════════════════════════

/// A widget plugin definition discovered from `~/.ghosty/widgets/`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WidgetManifest {
    pub widget: WidgetDef,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WidgetDef {
    pub id: String,
    pub title: String,
    #[serde(rename = "kind")]
    pub kind: WidgetKind,
    pub hotkey: Option<String>,
    #[serde(default = "default_width")]
    pub width: u16,
    #[serde(default = "default_height")]
    pub height: u16,
    #[serde(default)]
    pub script: Option<ScriptDef>,
    #[serde(default)]
    pub file_watch: Option<FileWatchDef>,
    #[serde(default)]
    pub mcp_view: Option<McpViewDef>,
    #[serde(default)]
    pub log_tail: Option<LogTailDef>,
    #[serde(default)]
    pub static_content: Option<String>,
}

fn default_width() -> u16 { 40 }
fn default_height() -> u16 { 14 }

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WidgetKind {
    Script,
    FileWatch,
    McpView,
    LogTail,
    Static,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ScriptDef {
    pub command: String,
    pub args: Option<Vec<String>>,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

fn default_interval() -> u64 { 5 }

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FileWatchDef {
    pub path: String,
    #[serde(default)]
    pub checkbox_format: bool,  // render as - [ ] checklist
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct McpViewDef {
    pub server: String,
    pub tool: String,
    pub args_json: Option<String>,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct LogTailDef {
    pub path: String,
    #[serde(default)]
    pub max_lines: usize,
}

// ═════════════════════════════════════════════════════════════════════
// 2. Widget runtime trait
// ═════════════════════════════════════════════════════════════════════

pub trait Widget {
    fn id(&self) -> &str;
    fn title(&self) -> &str;
    fn visible(&self) -> bool;
    fn set_visible(&mut self, v: bool);
    fn pos(&self) -> (u16, u16);
    fn set_pos(&mut self, x: u16, y: u16);
    fn size(&self) -> (u16, u16);
    fn handle_key(&mut self, _key: KeyEvent) -> Vec<WidgetAction> { vec![] }
    fn handle_mouse(&mut self, _mouse: MouseEvent) -> bool { false }
    fn tick(&mut self) {}
    fn render(&self, f: &mut Frame, area: Rect, theme: &WidgetTheme);
}

pub enum WidgetAction {
    Close,
    Redraw,
}

// ═════════════════════════════════════════════════════════════════════
// 3. WidgetFactory — builds Widget from manifest
// ═════════════════════════════════════════════════════════════════════

pub struct WidgetFactory;

impl WidgetFactory {
    pub fn build(manifest: &WidgetManifest) -> Box<dyn Widget> {
        match manifest.widget.kind {
            WidgetKind::Script => Box::new(ScriptWidget::from_manifest(manifest)),
            WidgetKind::FileWatch => Box::new(FileWatchWidget::from_manifest(manifest)),
            WidgetKind::LogTail => Box::new(LogTailWidget::from_manifest(manifest)),
            WidgetKind::Static => Box::new(StaticWidget::from_manifest(manifest)),
            WidgetKind::McpView => Box::new(McpViewWidget::from_manifest(manifest)),
        }
    }
}

// ═════════════════════════════════════════════════════════════════════
// 4. Widget Registry — auto-discovery
// ═════════════════════════════════════════════════════════════════════

pub struct WidgetRegistry {
    manifests: HashMap<String, WidgetManifest>,
    widgets_dir: PathBuf,
}

impl WidgetRegistry {
    pub fn new() -> Self {
        let home = dirs_next().unwrap_or_else(|| PathBuf::from("."));
        let dir = home.join(".ghosty").join("widgets");
        let mut reg = Self { manifests: HashMap::new(), widgets_dir: dir };
        reg.discover();
        reg
    }

    fn discover(&mut self) {
        if !self.widgets_dir.exists() {
            let _ = fs::create_dir_all(&self.widgets_dir);
            self.write_example_widgets();
        }
        if let Ok(entries) = fs::read_dir(&self.widgets_dir) {
            for entry in entries.flatten() {
                let manifest_path = entry.path().join("manifest.toml");
                if manifest_path.exists() {
                    if let Ok(content) = fs::read_to_string(&manifest_path) {
                        if let Ok(m) = toml::from_str::<WidgetManifest>(&content) {
                            self.manifests.insert(m.widget.id.clone(), m);
                        }
                    }
                }
            }
        }
    }

    fn write_example_widgets(&self) {
        let examples_dir = self.widgets_dir.join("_examples");
        let _ = fs::create_dir_all(&examples_dir);
        let example = r##"[widget]
id = "example-clock"
title = "Clock"
kind = "script"
hotkey = "c"
width = 30
height = 6

[widget.script]
command = "bash"
args = ["-c", "echo '🕐'; date '+%H:%M:%S'; echo '---'; cal"]
interval_secs = 1
"##;
        let _ = fs::write(examples_dir.join("clock.manifest.toml"), example);
    }

    pub fn manifests(&self) -> impl Iterator<Item = &WidgetManifest> {
        self.manifests.values()
    }

    pub fn hotkey_map(&self) -> HashMap<String, String> {
        self.manifests.iter()
            .filter_map(|(id, m)| m.widget.hotkey.as_ref().map(|k| (k.clone(), id.clone())))
            .collect()
    }
}

// ═════════════════════════════════════════════════════════════════════
// 5. Built-in widget implementations
// ═════════════════════════════════════════════════════════════════════

// -- Script widget ------------------------------------------------

struct ScriptWidget {
    manifest: WidgetManifest,
    visible: bool,
    x: u16, y: u16,
    output: String,
    last_run: Instant,
}

impl ScriptWidget {
    fn from_manifest(m: &WidgetManifest) -> Self {
        Self {
            manifest: m.clone(),
            visible: false, x: 4, y: 2,
            output: String::new(),
            last_run: Instant::now(),
        }
    }
}

impl Widget for ScriptWidget {
    fn id(&self) -> &str { &self.manifest.widget.id }
    fn title(&self) -> &str { &self.manifest.widget.title }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn pos(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_pos(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.manifest.widget.width, self.manifest.widget.height) }

    fn tick(&mut self) {
        if !self.visible { return; }
        let interval = self.manifest.widget.script.as_ref()
            .map(|s| s.interval_secs).unwrap_or(5);
        if self.last_run.elapsed().as_secs() >= interval {
            self.output = self.run_script();
            self.last_run = Instant::now();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<WidgetAction> {
        match key.code {
            KeyCode::Esc => vec![WidgetAction::Close],
            _ => vec![],
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, theme: &WidgetTheme) {
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL)
            .title(format!(" ≡ {} (drag) ", self.title()))
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.bg));
        let inner = block.inner(area);
        let lines: Vec<Line> = self.output.lines()
            .take(inner.height as usize)
            .map(|l| Line::styled(trunc(l, inner.width as usize), Style::default().fg(theme.text)))
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

impl ScriptWidget {
    fn run_script(&self) -> String {
        let def = match &self.manifest.widget.script {
            Some(d) => d, None => return "no script defined".into(),
        };
        Command::new(&def.command)
            .args(def.args.as_deref().unwrap_or(&[]))
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_else(|e| format!("error: {e}"))
    }
}

// -- FileWatch widget ----------------------------------------------

struct FileWatchWidget {
    manifest: WidgetManifest,
    visible: bool, x: u16, y: u16,
    items: Vec<CheckItem>,
    last_read: Instant,
}

#[derive(Clone)]
struct CheckItem { done: bool, text: String }

impl FileWatchWidget {
    fn from_manifest(m: &WidgetManifest) -> Self {
        let items = m.widget.file_watch.as_ref()
            .map(|fw| read_checklist(&fw.path, fw.checkbox_format))
            .unwrap_or_default();
        Self { manifest: m.clone(), visible: false, x: 4, y: 2, items, last_read: Instant::now() }
    }
}

impl Widget for FileWatchWidget {
    fn id(&self) -> &str { &self.manifest.widget.id }
    fn title(&self) -> &str { &self.manifest.widget.title }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn pos(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_pos(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.manifest.widget.width, self.manifest.widget.height) }

    fn tick(&mut self) {
        if !self.visible { return; }
        if self.last_read.elapsed().as_secs() >= 2 {
            if let Some(fw) = &self.manifest.widget.file_watch {
                self.items = read_checklist(&fw.path, fw.checkbox_format);
            }
            self.last_read = Instant::now();
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<WidgetAction> {
        match key.code {
            KeyCode::Esc => vec![WidgetAction::Close],
            _ => vec![],
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, theme: &WidgetTheme) {
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL)
            .title(format!(" ≡ {} (drag) ", self.title()))
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.bg));
        let inner = block.inner(area);
        let done = self.items.iter().filter(|i| i.done).count();
        let total = self.items.len();
        let mut lines: Vec<Line> = vec![
            Line::styled(format!("  {done}/{total}"), Style::default().fg(theme.text_dim))
        ];
        for item in self.items.iter().take(inner.height.saturating_sub(2) as usize) {
            let cb = if item.done { "☑" } else { "☐" };
            let style = if item.done { Style::default().fg(theme.success) }
                        else { Style::default().fg(theme.text) };
            lines.push(Line::styled(
                format!("  {cb} {}", trunc(&item.text, inner.width.saturating_sub(4) as usize)),
                style,
            ));
        }
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

// -- LogTail widget ------------------------------------------------

struct LogTailWidget {
    manifest: WidgetManifest,
    visible: bool, x: u16, y: u16,
    lines: VecDeque<String>,
    last_size: u64,
}

impl LogTailWidget {
    fn from_manifest(m: &WidgetManifest) -> Self {
        Self { manifest: m.clone(), visible: false, x: 8, y: 4,
               lines: VecDeque::with_capacity(200), last_size: 0 }
    }
}

impl Widget for LogTailWidget {
    fn id(&self) -> &str { &self.manifest.widget.id }
    fn title(&self) -> &str { &self.manifest.widget.title }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn pos(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_pos(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.manifest.widget.width, self.manifest.widget.height) }

    fn tick(&mut self) {
        if !self.visible { return; }
        let path = self.manifest.widget.log_tail.as_ref()
            .map(|lt| PathBuf::from(&lt.path)).unwrap_or_default();
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > self.last_size {
                if let Ok(content) = fs::read_to_string(&path) {
                    let new: Vec<&str> = content.lines().collect();
                    let skip = self.lines.len();
                    for line in &new[skip.min(new.len())..] {
                        self.lines.push_back(line.to_string());
                        let max = self.manifest.widget.log_tail.as_ref()
                            .map(|lt| lt.max_lines).unwrap_or(200);
                        while self.lines.len() > max { self.lines.pop_front(); }
                    }
                    self.last_size = meta.len();
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Vec<WidgetAction> {
        if key.code == KeyCode::Esc { vec![WidgetAction::Close] } else { vec![] }
    }

    fn render(&self, f: &mut Frame, area: Rect, theme: &WidgetTheme) {
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL)
            .title(format!(" ≡ {} (drag) ", self.title()))
            .border_style(Style::default().fg(theme.success))
            .style(Style::default().bg(theme.bg));
        let inner = block.inner(area);
        let lines: Vec<Line> = self.lines.iter().rev()
            .take(inner.height as usize)
            .map(|l| Line::styled(trunc(l, inner.width as usize), Style::default().fg(theme.text_dim)))
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

// -- Static widget ------------------------------------------------

struct StaticWidget {
    manifest: WidgetManifest,
    visible: bool, x: u16, y: u16,
}

impl StaticWidget {
    fn from_manifest(m: &WidgetManifest) -> Self {
        Self { manifest: m.clone(), visible: false, x: 4, y: 2 }
    }
}

impl Widget for StaticWidget {
    fn id(&self) -> &str { &self.manifest.widget.id }
    fn title(&self) -> &str { &self.manifest.widget.title }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn pos(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_pos(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.manifest.widget.width, self.manifest.widget.height) }
    fn handle_key(&mut self, key: KeyEvent) -> Vec<WidgetAction> {
        if key.code == KeyCode::Esc { vec![WidgetAction::Close] } else { vec![] }
    }
    fn render(&self, f: &mut Frame, area: Rect, theme: &WidgetTheme) {
        f.render_widget(Clear, area);
        let content = self.manifest.widget.static_content.as_deref().unwrap_or("");
        let block = Block::default().borders(Borders::ALL)
            .title(format!(" ≡ {} (drag) ", self.title()))
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.bg));
        let inner = block.inner(area);
        let lines: Vec<Line> = content.lines()
            .take(inner.height as usize)
            .map(|l| Line::styled(trunc(l, inner.width as usize), Style::default().fg(theme.text)))
            .collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

// -- MCP View widget (stub) ---------------------------------------

struct McpViewWidget {
    manifest: WidgetManifest,
    visible: bool, x: u16, y: u16,
    content: String, last_run: Instant,
}

impl McpViewWidget {
    fn from_manifest(m: &WidgetManifest) -> Self {
        Self { manifest: m.clone(), visible: false, x: 4, y: 2,
               content: "MCP view — connect to server to see data".into(),
               last_run: Instant::now() }
    }
}

impl Widget for McpViewWidget {
    fn id(&self) -> &str { &self.manifest.widget.id }
    fn title(&self) -> &str { &self.manifest.widget.title }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn pos(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_pos(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.manifest.widget.width, self.manifest.widget.height) }
    fn handle_key(&mut self, key: KeyEvent) -> Vec<WidgetAction> {
        if key.code == KeyCode::Esc { vec![WidgetAction::Close] } else { vec![] }
    }
    fn render(&self, f: &mut Frame, area: Rect, theme: &WidgetTheme) {
        f.render_widget(Clear, area);
        let block = Block::default().borders(Borders::ALL)
            .title(format!(" ≡ {} (drag) ", self.title()))
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.bg));
        f.render_widget(Paragraph::new(self.content.clone()).block(block), area);
    }
}

// ═════════════════════════════════════════════════════════════════════
// 6. Overlay Host — manages all widget instances
// ═════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub struct WidgetTheme {
    pub bg: Color, pub border: Color, pub text: Color,
    pub text_dim: Color, pub accent: Color, pub success: Color,
}

impl Default for WidgetTheme {
    fn default() -> Self {
        Self {
            bg: Color::Rgb(18, 18, 28), border: Color::Rgb(80, 140, 220),
            text: Color::Rgb(220, 220, 220), text_dim: Color::Rgb(140, 140, 140),
            accent: Color::Rgb(100, 180, 255), success: Color::Rgb(100, 200, 100),
        }
    }
}

pub struct OverlayHost {
    pub widgets: Vec<Box<dyn Widget>>,
    pub theme: WidgetTheme,
    pub dashboard: bool,
    drag: DragState,
    registry: WidgetRegistry,
}

#[derive(Default)]
struct DragState {
    active: bool,
    widget_id: String,
    offset: (u16, u16),
}

impl OverlayHost {
    pub fn new() -> Self {
        let registry = WidgetRegistry::new();
        let widgets: Vec<Box<dyn Widget>> = registry.manifests()
            .map(|m| WidgetFactory::build(m))
            .collect();
        Self {
            widgets, registry, theme: WidgetTheme::default(),
            dashboard: false, drag: DragState::default(),
        }
    }

    pub fn toggle_dashboard(&mut self) { self.dashboard = !self.dashboard; }

    pub fn toggle(&mut self, id: &str) {
        if let Some(w) = self.widgets.iter_mut().find(|w| w.id() == id) {
            w.set_visible(!w.visible());
        }
    }

    pub fn toggle_by_hotkey(&mut self, key: &str) -> bool {
        let map = self.registry.hotkey_map();
        if let Some(id) = map.get(key) {
            self.toggle(id);
            return true;
        }
        false
    }

    // -- mouse ----------------------------------------------------

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) && self.drag.active {
            self.drag.active = false; return true;
        }
        if let MouseEventKind::Drag(MouseButton::Left) = mouse.kind {
            if self.drag.active { return self.continue_drag(mouse); }
        }
        // Find topmost hit
        for (i, w) in self.widgets.iter().enumerate() {
            if !w.visible() { continue; }
            let (x, y) = w.pos(); let (ww, wh) = w.size();
            if mouse.column >= x && mouse.column < x + ww
               && mouse.row >= y && mouse.row < y + wh {
                // Title bar hit → start drag
                if mouse.row == y && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.drag = DragState {
                        active: true,
                        widget_id: w.id().to_string(),
                        offset: (mouse.column - x, mouse.row - y),
                    };
                    return true;
                }
                // Body click → bring to front
                if i != 0 && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                    let w = self.widgets.remove(i);
                    self.widgets.insert(0, w);
                    return true;
                }
                return true; // consumed
            }
        }
        false
    }

    fn continue_drag(&mut self, mouse: MouseEvent) -> bool {
        let id = self.drag.widget_id.clone();
        if let Some(w) = self.widgets.iter_mut().find(|w| w.id() == &id) {
            w.set_pos(
                mouse.column.saturating_sub(self.drag.offset.0),
                mouse.row.saturating_sub(self.drag.offset.1),
            );
        }
        true
    }

    // -- keys -----------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<WidgetAction> {
        if let Some(w) = self.widgets.first_mut() {
            if w.visible() { return w.handle_key(key); }
        }
        vec![]
    }

    // -- render ---------------------------------------------------

    pub fn render_all(&self, f: &mut Frame) {
        // Bottom-to-top so first widget is on top
        for w in self.widgets.iter().rev() {
            if !w.visible() { continue; }
            let (x, y) = w.pos(); let (ww, wh) = w.size();
            let area = Rect::new(x, y, ww, wh);
            let term = f.area();
            if area.right() > term.right() || area.bottom() > term.bottom() { continue; }
            w.render(f, area, &self.theme);
        }
    }

    // -- tick -----------------------------------------------------

    pub fn tick_all(&mut self) {
        for w in self.widgets.iter_mut() {
            if w.visible() { w.tick(); }
        }
    }

    /// Scan for new/updated plugins on disk.
    pub fn reload_plugins(&mut self) {
        self.registry = WidgetRegistry::new();
        let old_ids: Vec<String> = self.widgets.iter().map(|w| w.id().to_string()).collect();
        self.widgets = self.registry.manifests()
            .map(|m| {
                let was_visible = old_ids.contains(&m.widget.id);
                let mut w = WidgetFactory::build(m);
                if was_visible { w.set_visible(true); }
                w
            })
            .collect();
    }
}

// ═════════════════════════════════════════════════════════════════════
// 7. Helpers
// ═════════════════════════════════════════════════════════════════════

fn trunc(s: &str, max: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w <= max { return s.to_string(); }
    let mut out = String::with_capacity(max + 1);
    let mut used = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > max.saturating_sub(1) { out.push('…'); break; }
        out.push(c); used += cw;
    }
    out
}

fn read_checklist(path: &str, _checkbox: bool) -> Vec<CheckItem> {
    let mut items = Vec::new();
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("- [x] ").or_else(|| t.strip_prefix("- [X] ")) {
                items.push(CheckItem { done: true, text: rest.to_string() });
            } else if let Some(rest) = t.strip_prefix("- [ ] ") {
                items.push(CheckItem { done: false, text: rest.to_string() });
            }
        }
    }
    if items.is_empty() { items.push(CheckItem { done: false, text: "Create a file with `- [ ] task` lines".into() }); }
    items
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}
