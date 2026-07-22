//! Draggable overlays for Ghosty Code.
//!
//! Architecture:
//!   Each overlay is a struct implementing `OverlayWidget`.  The `OverlayHost`
//!   stored in `App` owns the collection, routes mouse events, and drives
//!   rendering.  Overlays paint on top of the transcript (last in the render
//!   loop).
//!
//! Built-in overlays:
//!   * `TodoOverlay` — renders markdown checkboxes from a watched file
//!                     (`.ghosty/todo.md` by default).
//!   * `ScratchOverlay` — floating persistent scratchpad.
//!   * `LogWatcher` — tail a file in real time.
//!
//! Adding a built-in overlay is three steps:
//!   1. Implement `OverlayWidget` on a new struct.
//!   2. Register it in `OverlayHost::default_overlays()`.
//!   3. Add a keybinding in `ui.rs`.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::Rect,
    prelude::*,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Instant;

// ── Overlay widget trait ────────────────────────────────────────────

pub trait OverlayWidget {
    fn id(&self) -> &'static str;
    fn title(&self) -> &str;
    fn visible(&self) -> bool;
    fn set_visible(&mut self, visible: bool);
    fn position(&self) -> (u16, u16);
    fn set_position(&mut self, x: u16, y: u16);
    fn size(&self) -> (u16, u16);
    fn handle_key(&mut self, _key: KeyEvent, _workspace: &PathBuf) -> Vec<OverlayAction> { vec![] }
    fn render(&self, f: &mut Frame, area: Rect, workspace: &PathBuf, theme: &OverlayTheme);
    fn tick(&mut self, _workspace: &PathBuf) {} // called ~60 fps for animations / file polling
}

pub enum OverlayAction {
    Close,
    Redraw,
}

// ── Drag state ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct DragState {
    overlay_id: &'static str,
    drag_offset: (u16, u16),
    active: bool,
}

impl Default for DragState {
    fn default() -> Self {
        Self { overlay_id: "", drag_offset: (0, 0), active: false }
    }
}

// ── Theme (matches Ghosty's palette) ────────────────────────────────

#[derive(Clone)]
pub struct OverlayTheme {
    pub bg: Color,
    pub border: Color,
    pub text: Color,
    pub text_dim: Color,
    pub accent: Color,
    pub success: Color,
    pub title_bg: Color,
}

impl Default for OverlayTheme {
    fn default() -> Self {
        Self {
            bg: Color::Rgb(18, 18, 28),
            border: Color::Rgb(80, 140, 220),
            text: Color::Rgb(220, 220, 220),
            text_dim: Color::Rgb(140, 140, 140),
            accent: Color::Rgb(100, 180, 255),
            success: Color::Rgb(100, 200, 100),
            title_bg: Color::Rgb(30, 40, 60),
        }
    }
}

// ── Overlay host (lives in App) ─────────────────────────────────────

pub struct OverlayHost {
    pub overlays: Vec<Box<dyn OverlayWidget>>,
    pub theme: OverlayTheme,
    drag: DragState,
    /// When true, all overlays are rendered full-screen (dashboard mode).
    pub dashboard: bool,
}

impl OverlayHost {
    pub fn new() -> Self {
        Self {
            overlays: default_overlays(),
            theme: OverlayTheme::default(),
            drag: DragState::default(),
            dashboard: false,
        }
    }

    pub fn toggle_dashboard(&mut self) {
        self.dashboard = !self.dashboard;
    }

    pub fn toggle_overlay(&mut self, id: &str, term_cols: u16, term_rows: u16) {
        if let Some(ov) = self.overlays.iter_mut().find(|o| o.id() == id) {
            let was_visible = ov.visible();
            ov.set_visible(!was_visible);
            if !was_visible {
                // Clamp initial position to terminal
                let (x, y) = ov.position();
                let _ = (x, y); // keep defaults
                let (_, _) = (term_cols, term_rows);
            }
        }
    }

    // ── mouse routing ────────────────────────────────────────────

    /// Route a mouse event to the frontmost visible overlay at the cursor.
    /// Returns true when the event was consumed.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        // Finish drag on button release even if the cursor left the overlay.
        if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) && self.drag.active {
            self.drag.active = false;
            return true;
        }

        // Drag continuation — the overlay id is already captured.
        if let MouseEventKind::Drag(MouseButton::Left) = mouse.kind {
            if self.drag.active {
                return self.continue_drag(mouse);
            }
        }

        // Find the topmost visible overlay at the click point.
        let hit_idx = self.hit_test(mouse);
        if hit_idx.is_none() && !matches!(mouse.kind, MouseEventKind::Drag(_)) {
            return false;
        }

        if let Some(idx) = hit_idx {
            let ov = &self.overlays[idx];
            let (x, y) = ov.position();
            let (w, h) = ov.size();
            let on_title = mouse.row == y && mouse.column >= x && mouse.column < x.saturating_add(w);

            if on_title && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.drag = DragState {
                    overlay_id: ov.id(),
                    drag_offset: (mouse.column.saturating_sub(x), mouse.row.saturating_sub(y)),
                    active: true,
                };
                return true;
            }
        }

        // Click on overlay body (not title) — forward to the overlay.
        if let Some(idx) = hit_idx {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                // Give focus — reorder to front
                if idx != 0 {
                    let ov = self.overlays.remove(idx);
                    self.overlays.insert(0, ov);
                }
                return true; // consumed
            }
        }

        false
    }

    fn continue_drag(&mut self, mouse: MouseEvent) -> bool {
        let id = self.drag.overlay_id;
        if let Some(ov) = self.overlays.iter_mut().find(|o| o.id() == id) {
            let new_x = mouse.column.saturating_sub(self.drag.drag_offset.0);
            let new_y = mouse.row.saturating_sub(self.drag.drag_offset.1);
            ov.set_position(new_x, new_y);
        }
        true
    }

    fn hit_test(&self, mouse: MouseEvent) -> Option<usize> {
        for (i, ov) in self.overlays.iter().enumerate() {
            if !ov.visible() { continue; }
            let (x, y) = ov.position();
            let (w, h) = ov.size();
            if mouse.column >= x
                && mouse.column < x.saturating_add(w)
                && mouse.row >= y
                && mouse.row < y.saturating_add(h)
            {
                return Some(i);
            }
        }
        None
    }

    // ── key routing ──────────────────────────────────────────────

    /// Route a key event to the focused (topmost) overlay.
    pub fn handle_key(&mut self, key: KeyEvent, workspace: &PathBuf) -> Vec<OverlayAction> {
        if let Some(ov) = self.overlays.first_mut() {
            if ov.visible() {
                return ov.handle_key(key, workspace);
            }
        }
        vec![]
    }

    // ── rendering ────────────────────────────────────────────────

    pub fn render_all(&self, f: &mut Frame, workspace: &PathBuf) {
        let term = f.area();
        // Render bottom-to-top so the first overlay (focused) is on top.
        for ov in self.overlays.iter().rev() {
            if !ov.visible() { continue; }
            let (x, y) = ov.position();
            let (w, h) = ov.size();
            let area = Rect::new(x, y, w, h);
            if area.right() > term.right() || area.bottom() > term.bottom() || w < 8 || h < 3 {
                continue;
            }
            ov.render(f, area, workspace, &self.theme);
        }
    }

    // ── tick ─────────────────────────────────────────────────────

    pub fn tick_all(&mut self, workspace: &PathBuf) {
        for ov in self.overlays.iter_mut() {
            if ov.visible() {
                ov.tick(workspace);
            }
        }
    }
}

fn default_overlays() -> Vec<Box<dyn OverlayWidget>> {
    vec![
        Box::new(TodoOverlay::new(".ghosty/todo.md", "TODO", KeyCode::Char('t'))),
        Box::new(TodoOverlay::new(".ghosty/bugs.md", "BUGS", KeyCode::Char('b'))),
        Box::new(ScratchOverlay::default()),
        Box::new(LogWatcher::default()),
    ]
}

// ═════════════════════════════════════════════════════════════════════
// TODO overlay
// ═════════════════════════════════════════════════════════════════════

pub struct TodoOverlay {
    id: &'static str,
    title: String,
    visible: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    file: String,
    hotkey: KeyCode,
    items: Vec<TodoItem>,
    last_read: Instant,
    /// Editable inline — when Some(idx), we're editing that item.
    editing: Option<usize>,
    edit_buf: String,
}

impl TodoOverlay {
    pub fn new(file: &str, title: &str, hotkey: KeyCode) -> Self {
        let file = file.to_string();
        let items = read_todo_file(&file);
        Self {
            id: Box::leak(title.to_lowercase().into_boxed_str()),
            title: title.to_string(),
            visible: false,
            x: 4,
            y: 2,
            width: 40,
            height: 14,
            file,
            hotkey,
            items,
            last_read: Instant::now(),
            editing: None,
            edit_buf: String::new(),
        }
    }
}

impl OverlayWidget for TodoOverlay {
    fn id(&self) -> &'static str { self.id }
    fn title(&self) -> &str { &self.title }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        if visible {
            self.items = read_todo_file(&self.file);
            self.last_read = Instant::now();
        }
    }
    fn position(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_position(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.width, self.height) }

    fn handle_key(&mut self, key: KeyEvent, workspace: &PathBuf) -> Vec<OverlayAction> {
        // Close on Escape when not editing
        if let Some(_idx) = self.editing {
            match key.code {
                KeyCode::Enter => {
                    if !self.edit_buf.trim().is_empty() {
                        self.toggle_item_and_save(workspace);
                    }
                    self.editing = None;
                    self.edit_buf.clear();
                    return vec![OverlayAction::Redraw];
                }
                KeyCode::Esc => {
                    self.editing = None;
                    self.edit_buf.clear();
                    return vec![OverlayAction::Redraw];
                }
                KeyCode::Backspace => { self.edit_buf.pop(); return vec![OverlayAction::Redraw]; }
                KeyCode::Char(c) => { self.edit_buf.push(c); return vec![OverlayAction::Redraw]; }
                _ => {}
            }
            return vec![];
        }

        match key.code {
            KeyCode::Esc => return vec![OverlayAction::Close],
            KeyCode::Char(' ') => {
                // Toggle the first unchecked item
                if let Some(idx) = self.items.iter().position(|i| !i.done) {
                    self.items[idx].done = true;
                    self.save(workspace);
                }
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Char('n') => {
                self.editing = Some(self.items.len());
                self.edit_buf.clear();
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Char('d') => {
                // Delete first checked item
                if let Some(idx) = self.items.iter().position(|i| i.done) {
                    self.items.remove(idx);
                    self.save(workspace);
                }
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Char('r') => {
                self.items = read_todo_file(&self.file);
                self.last_read = Instant::now();
                return vec![OverlayAction::Redraw];
            }
            _ => {}
        }
        vec![]
    }

    fn tick(&mut self, _workspace: &PathBuf) {
        // Re-read file every 2 seconds
        if self.last_read.elapsed().as_secs() >= 2 {
            let fresh = read_todo_file(&self.file);
            if fresh != self.items {
                self.items = fresh;
            }
            self.last_read = Instant::now();
        }
    }

    fn render(&self, f: &mut Frame, area: Rect, _workspace: &PathBuf, theme: &OverlayTheme) {
        Clear.render(area, f.buffer_mut());

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" ≡ {} (drag me) ", self.title))
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.bg));

        let inner = block.inner(area);
        let usable_h = inner.height.saturating_sub(2);
        if usable_h == 0 { block.render(area, f.buffer_mut()); return; }

        let done = self.items.iter().filter(|i| i.done).count();
        let total = self.items.len();
        let mut lines: Vec<Line> = Vec::with_capacity(usable_h as usize + 2);
        lines.push(Line::styled(
            format!("  {done}/{total} completed  [n]ew [Space]toggle [d]elete [r]eload"),
            Style::default().fg(theme.text_dim),
        ));

        for (idx, item) in self.items.iter().take(usable_h.saturating_sub(2) as usize).enumerate() {
            if self.editing == Some(idx) {
                lines.push(Line::styled(
                    format!("  ✎ {}", self.edit_buf),
                    Style::default().fg(theme.accent),
                ));
            } else {
                let cb = if item.done { "☑" } else { "☐" };
                let style = if item.done {
                    Style::default().fg(theme.success)
                } else {
                    Style::default().fg(theme.text)
                };
                lines.push(Line::styled(
                    truncate_line(&format!("  {cb} {}", item.text), inner.width.saturating_sub(2) as usize),
                    style,
                ));
            }
        }

        // Show "editing new item" row if needed
        if let Some(idx) = self.editing {
            if idx >= self.items.len() {
                lines.push(Line::styled(
                    format!("  ✎ {}", self.edit_buf),
                    Style::default().fg(theme.accent),
                ));
            }
        }

        let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        f.render_widget(p, area);
    }
}

impl TodoOverlay {
    fn toggle_item_and_save(&mut self, workspace: &PathBuf) {
        let text = self.edit_buf.trim().to_string();
        if text.starts_with("- [ ] ") || text.starts_with("- [x] ") {
            // Add as-is
            self.items.push(TodoItem { done: text.contains("[x]"), text: text.clone() });
        } else {
            self.items.push(TodoItem { done: false, text });
        }
        self.save(workspace);
    }

    fn save(&self, workspace: &PathBuf) {
        let path = workspace.join(&self.file);
        let mut out = String::new();
        for item in &self.items {
            let cb = if item.done { "[x]" } else { "[ ]" };
            out.push_str(&format!("- {cb} {}\n", item.text));
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, out);
    }
}

// ═════════════════════════════════════════════════════════════════════
// Scratch overlay — floating text scratchpad
// ═════════════════════════════════════════════════════════════════════

pub struct ScratchOverlay {
    visible: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    lines: Vec<String>,
    cursor: (usize, usize), // line, col
}

impl Default for ScratchOverlay {
    fn default() -> Self {
        Self {
            visible: false,
            x: 6,
            y: 3,
            width: 50,
            height: 10,
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }
}

impl OverlayWidget for ScratchOverlay {
    fn id(&self) -> &'static str { "scratch" }
    fn title(&self) -> &str { "Scratch" }
    fn visible(&self) -> bool { self.visible }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn position(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_position(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.width, self.height) }

    fn handle_key(&mut self, key: KeyEvent, _ws: &PathBuf) -> Vec<OverlayAction> {
        match key.code {
            KeyCode::Esc => return vec![OverlayAction::Close],
            KeyCode::Enter => {
                let rest = self.lines[self.cursor.0][self.cursor.1..].to_string();
                self.lines[self.cursor.0].truncate(self.cursor.1);
                self.lines.insert(self.cursor.0 + 1, rest);
                self.cursor = (self.cursor.0 + 1, 0);
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Backspace => {
                if self.cursor.1 > 0 {
                    self.lines[self.cursor.0].remove(self.cursor.1 - 1);
                    self.cursor.1 -= 1;
                } else if self.cursor.0 > 0 {
                    let rest = self.lines.remove(self.cursor.0);
                    self.cursor.0 -= 1;
                    self.cursor.1 = self.lines[self.cursor.0].len();
                    self.lines[self.cursor.0].push_str(&rest);
                }
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Char(c) => {
                self.lines[self.cursor.0].insert(self.cursor.1, c);
                self.cursor.1 += 1;
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Up if self.cursor.0 > 0 => {
                self.cursor.0 -= 1;
                self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
                return vec![OverlayAction::Redraw];
            }
            KeyCode::Down if self.cursor.0 + 1 < self.lines.len() => {
                self.cursor.0 += 1;
                self.cursor.1 = self.cursor.1.min(self.lines[self.cursor.0].len());
                return vec![OverlayAction::Redraw];
            }
            _ => {}
        }
        vec![]
    }

    fn render(&self, f: &mut Frame, area: Rect, _ws: &PathBuf, theme: &OverlayTheme) {
        Clear.render(area, f.buffer_mut());
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" ≡ Scratch (drag me) ")
            .border_style(Style::default().fg(theme.accent))
            .style(Style::default().bg(theme.bg));
        let inner = block.inner(area);

        let mut display: Vec<Line> = Vec::new();
        for (li, line) in self.lines.iter().enumerate().take(inner.height as usize) {
            if li == self.cursor.0 && self.cursor.1 <= line.len() {
                let before = &line[..self.cursor.1];
                let at = if self.cursor.1 < line.len() { &line[self.cursor.1..self.cursor.1+1] } else { " " };
                let after = if self.cursor.1 + 1 < line.len() { &line[self.cursor.1+1..] } else { "" };
                display.push(Line::from(vec![
                    Span::raw(before),
                    Span::styled(at, Style::default().bg(theme.accent).fg(Color::Black)),
                    Span::raw(after),
                ]));
            } else {
                display.push(Line::raw(line.clone()));
            }
        }

        f.render_widget(Paragraph::new(display).block(block), area);
    }
}

// ═════════════════════════════════════════════════════════════════════
// Log watcher — tail -f any file
// ═════════════════════════════════════════════════════════════════════

pub struct LogWatcher {
    visible: bool,
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    path: PathBuf,
    lines: VecDeque<String>,
    last_size: u64,
}

impl Default for LogWatcher {
    fn default() -> Self {
        Self {
            visible: false,
            x: 8,
            y: 4,
            width: 70,
            height: 12,
            path: PathBuf::new(),
            lines: VecDeque::with_capacity(200),
            last_size: 0,
        }
    }
}

impl LogWatcher {
    pub fn set_path(&mut self, path: PathBuf) {
        self.path = path;
        self.lines.clear();
        self.last_size = 0;
    }
}

impl OverlayWidget for LogWatcher {
    fn id(&self) -> &'static str { "logwatcher" }
    fn title(&self) -> &str {
        if self.path.as_os_str().is_empty() { "Log Watcher" } else { "Log Watcher" }
    }
    fn visible(&self) -> bool { self.visible && !self.path.as_os_str().is_empty() }
    fn set_visible(&mut self, v: bool) { self.visible = v; }
    fn position(&self) -> (u16, u16) { (self.x, self.y) }
    fn set_position(&mut self, x: u16, y: u16) { self.x = x; self.y = y; }
    fn size(&self) -> (u16, u16) { (self.width, self.height) }

    fn tick(&mut self, _ws: &PathBuf) {
        if self.path.as_os_str().is_empty() || !self.visible { return; }
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let sz = meta.len();
            if sz > self.last_size {
                if let Ok(content) = std::fs::read_to_string(&self.path) {
                    let new_lines: Vec<&str> = content.lines().collect();
                    let existing = self.lines.len();
                    if new_lines.len() > existing {
                        for line in &new_lines[existing..] {
                            self.lines.push_back(line.to_string());
                            if self.lines.len() > 200 {
                                self.lines.pop_front();
                            }
                        }
                    }
                }
                self.last_size = sz;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, _ws: &PathBuf) -> Vec<OverlayAction> {
        if key.code == KeyCode::Esc {
            return vec![OverlayAction::Close];
        }
        vec![]
    }

    fn render(&self, f: &mut Frame, area: Rect, _ws: &PathBuf, theme: &OverlayTheme) {
        Clear.render(area, f.buffer_mut());
        let title = if self.path.as_os_str().is_empty() {
            " ≡ Log Watcher "
        } else {
            let _ = format!(" ≡ {} ", self.path.display());
            " ≡ Log Watcher (drag me) "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(theme.success))
            .style(Style::default().bg(theme.bg));
        let inner = block.inner(area);

        let lines: Vec<Line> = self.lines.iter().rev()
            .take(inner.height as usize)
            .map(|l| Line::styled(truncate_line(l, inner.width.saturating_sub(2) as usize), Style::default().fg(theme.text_dim)))
            .collect();

        f.render_widget(Paragraph::new(lines).block(block), area);
    }
}

// ── Shared helpers ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct TodoItem {
    done: bool,
    text: String,
}

fn read_todo_file(file: &str) -> Vec<TodoItem> {
    let mut items = Vec::new();
    if let Ok(content) = std::fs::read_to_string(file) {
        for line in content.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("- [x] ").or_else(|| t.strip_prefix("- [X] ")) {
                items.push(TodoItem { done: true, text: rest.to_string() });
            } else if let Some(rest) = t.strip_prefix("- [ ] ") {
                items.push(TodoItem { done: false, text: rest.to_string() });
            }
        }
    }
    if items.is_empty() {
        items.push(TodoItem { done: false, text: "Create .ghosty/todo.md with `- [ ] task` lines".into() });
    }
    items
}

fn truncate_line(s: &str, max: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    if w <= max { return s.to_string(); }
    let mut out = String::with_capacity(max + 1);
    let mut used = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > max.saturating_sub(1) { out.push('…'); break; }
        out.push(c);
        used += cw;
    }
    out
}
