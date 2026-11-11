//! The terminal view: a `TermSession` painted as a grid of cells, with the keyboard, mouse and IME
//! wired to it. Ported from syro's `pane/terminal.rs`, without syro's agent tracking and pane menu.

use std::cell::Cell as SharedCell;
use std::path::Path;
use std::process::ExitStatus;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::grid::Grid;
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Rgb as AnsiRgb};
use gpui_kit::base::{ColorTokens, Theme};
use gpui_kit::*;

use super::session::{
    EventProxy, MouseButton as Report, MouseModifiers, MouseReport, TermEvent, TermSession,
};

/// How often the view looks for terminal output.
const REPAINT_INTERVAL: Duration = Duration::from_millis(16);

/// alacritty's own minimums: a grid smaller than this cannot hold a cursor.
const MIN_COLUMNS: usize = 2;
const MIN_SCREEN_LINES: usize = 1;

/// The gap between the pane's edge and the first cell.
const PADDING_X: f32 = 10.;
const PADDING_Y: f32 = 8.;

static NEXT_TERMINAL: AtomicUsize = AtomicUsize::new(0);

/// What the owner of a terminal needs to hear about.
#[derive(Debug, Clone, PartialEq)]
pub enum TerminalEvent {
    /// The child exited; the screen stays as it left it.
    Exited(ExitStatus),
    /// Enter was pressed after the child exited.
    RestartRequested,
}

impl EventEmitter<TerminalEvent> for TerminalView {}

pub struct TerminalView {
    /// Held by `Rc` because the canvas closures paint from it. Dropping the last clone hangs up the child.
    session: Rc<TermSession>,
    focus: FocusHandle,
    metrics: CellMetrics,
    element_id: ElementId,
    /// Pixels of wheel scroll that did not add up to a line yet.
    scroll_pixels: f32,
    /// The IME's pre-edit text, painted at the cursor while composing.
    marked: Option<String>,
    exit: Option<ExitStatus>,
    /// Where the last painted frame put the grid.
    layout: Rc<SharedCell<GridLayout>>,
    /// A press is being dragged into a selection.
    dragging: bool,
    _repaint: Task<()>,
}

/// The pixel size of one cell, measured once.
#[derive(Clone)]
struct CellMetrics {
    font: Font,
    font_size: Pixels,
    cell: Size<Pixels>,
}

/// Where the grid sits and how big it is, decided while laying the frame out.
#[derive(Clone, Copy, PartialEq)]
struct GridLayout {
    columns: usize,
    screen_lines: usize,
    origin: Point<Pixels>,
}

impl CellMetrics {
    fn measure(text_system: &TextSystem, theme: &Theme) -> Self {
        let typography = &theme.tokens.typography;
        let font = font(typography.mono.clone());
        let font_size = typography.mono_md.size;
        let font_id = text_system.resolve_font(&font);
        // A monospace face advances `m` by exactly one cell.
        let width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .or_else(|_| text_system.em_advance(font_id, font_size))
            .unwrap_or_else(|_| font_size * 0.6);
        Self {
            font,
            font_size,
            cell: size(width, typography.mono_md.line_height),
        }
    }
}

impl GridLayout {
    /// The grid the painted bounds can hold, resizing the session only when that changed.
    fn resolve(bounds: Bounds<Pixels>, metrics: &CellMetrics, session: &TermSession) -> Self {
        let inner = Bounds::new(
            Point::new(
                bounds.origin.x + px(PADDING_X),
                bounds.origin.y + px(PADDING_Y),
            ),
            size(
                (bounds.size.width - px(PADDING_X * 2.)).max(px(0.)),
                (bounds.size.height - px(PADDING_Y * 2.)).max(px(0.)),
            ),
        );
        let cell_width = metrics.cell.width.as_f32().max(1.);
        let cell_height = metrics.cell.height.as_f32().max(1.);
        let columns = ((inner.size.width.as_f32() / cell_width).floor() as usize).max(MIN_COLUMNS);
        let screen_lines =
            ((inner.size.height.as_f32() / cell_height).floor() as usize).max(MIN_SCREEN_LINES);

        let current = session.size();
        if (current.columns != columns || current.screen_lines != screen_lines)
            && let Err(error) = session.resize(columns, screen_lines)
        {
            log::warn!("terminal resize failed: {error}");
        }

        Self {
            columns,
            screen_lines,
            origin: inner.origin,
        }
    }

    /// The cells `[column, column + span)` of `line`, in window coordinates.
    fn cells(
        &self,
        metrics: &CellMetrics,
        line: usize,
        column: usize,
        span: usize,
    ) -> Bounds<Pixels> {
        Bounds::new(
            Point::new(
                self.origin.x + metrics.cell.width * column as f32,
                self.origin.y + metrics.cell.height * line as f32,
            ),
            size(metrics.cell.width * span as f32, metrics.cell.height),
        )
    }
}

fn report_modifiers(modifiers: &Modifiers) -> MouseModifiers {
    MouseModifiers {
        shift: modifiers.shift,
        alt: modifiers.alt,
        control: modifiers.control,
    }
}

impl TerminalView {
    pub fn new(cx: &mut Context<Self>, session: TermSession) -> Self {
        let session = Rc::new(session);
        let metrics = CellMetrics::measure(cx.text_system(), &Theme::global(cx));
        let repaint = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(REPAINT_INTERVAL).await;
                if this.update(cx, |view, cx| view.poll(cx)).is_err() {
                    break;
                }
            }
        });
        let id = NEXT_TERMINAL.fetch_add(1, Ordering::Relaxed);
        Self {
            session,
            focus: cx.focus_handle(),
            metrics,
            element_id: ElementId::Name(format!("terminal-{id}").into()),
            scroll_pixels: 0.,
            marked: None,
            exit: None,
            layout: Rc::new(SharedCell::new(GridLayout {
                columns: 0,
                screen_lines: 0,
                origin: Point::default(),
            })),
            dragging: false,
            _repaint: repaint,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit
    }

    /// Put `text` at the prompt as a paste: bracketed when the child asked, so it is not submitted.
    pub fn insert_text(&self, text: &str) {
        let bracketed = self
            .session
            .with_term(|term| term.mode().contains(TermMode::BRACKETED_PASTE));
        if let Err(error) = self.session.write(&paste_bytes(text, bracketed)) {
            log::warn!("terminal paste failed: {error}");
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.session.with_term(|term| term.selection_to_string())
    }

    fn marked_text_range(&self) -> Option<std::ops::Range<usize>> {
        self.marked
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn set_marked_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return self.clear_marked_text(cx);
        }
        self.marked = Some(text.to_string());
        cx.notify();
    }

    fn clear_marked_text(&mut self, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        self.clear_marked_text(cx);
        if text.is_empty() {
            return;
        }
        if let Err(error) = self.session.write(text.as_bytes()) {
            log::warn!("terminal write failed: {error}");
        }
    }

    /// Everything the terminal said since the last look; repaints only when something changed.
    fn poll(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        for event in self.session.drain_events() {
            match event {
                TermEvent::Wakeup => changed = true,
                TermEvent::Exit(status) => {
                    self.exit = Some(status);
                    cx.emit(TerminalEvent::Exited(status));
                    changed = true;
                }
                TermEvent::Error(error) => {
                    log::warn!("terminal pty error: {error}");
                    changed = true;
                }
                TermEvent::ClipboardStore(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                TermEvent::Title(_) | TermEvent::ResetTitle | TermEvent::Bell => {}
            }
        }
        if changed {
            cx.notify();
        }
    }

    /// Whether the child owns the mouse: it asked for reports, and alt is not held to take them back.
    fn child_has_the_mouse(&self, modifiers: &Modifiers) -> bool {
        self.session.wants_mouse() && !modifiers.alt
    }

    fn pointer_cell(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let (point, _) = self.grid_point(position)?;
        Some((point.column.0, point.line.0.max(0) as usize))
    }

    fn report_mouse(&mut self, event: MouseReport, position: Point<Pixels>, modifiers: &Modifiers) {
        let Some((column, line)) = self.pointer_cell(position) else {
            return;
        };
        let report = self
            .session
            .mouse_report(event, column, line, report_modifiers(modifiers));
        if let Err(error) = self.session.write(&report) {
            log::warn!("mouse report failed: {error}");
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exit.is_some() {
            if event.keystroke.key == "enter" {
                cx.emit(TerminalEvent::RestartRequested);
                cx.stop_propagation();
            }
            return;
        }
        if self.focus.is_focused(window) {
            match clipboard_key(&event.keystroke) {
                Some(ClipboardKey::Copy) => {
                    self.copy_selection(cx);
                    cx.stop_propagation();
                    return;
                }
                Some(ClipboardKey::Paste) => {
                    self.paste(cx);
                    cx.stop_propagation();
                    return;
                }
                None => {}
            }
        }
        let Some(bytes) = key_to_bytes(&event.keystroke) else {
            return;
        };
        // Typing jumps back to the live screen, as every terminal does.
        self.session.with_term(|term| {
            if term.grid().display_offset() != 0 {
                term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            }
            term.selection = None;
        });
        if let Err(error) = self.session.write(&bytes) {
            log::warn!("terminal write failed: {error}");
        }
        cx.stop_propagation();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self.metrics.cell.height.as_f32().max(1.);
        // A trackpad reports pixels and a wheel whole lines; the remainder of a line carries over.
        let pixels = match &event.delta {
            ScrollDelta::Pixels(pixels) => pixels.y.as_f32(),
            ScrollDelta::Lines(lines) => lines.y * line_height,
        };
        self.scroll_pixels += pixels;
        let lines = (self.scroll_pixels / line_height).trunc();
        self.scroll_pixels -= lines * line_height;
        let lines = lines as i32;
        if lines == 0 {
            return;
        }
        // Three targets, in the order a terminal tries them.
        if self.session.wants_mouse() {
            let Some((column, line)) = self.pointer_cell(event.position) else {
                return;
            };
            let report = self.session.wheel_report(
                lines > 0,
                column,
                line,
                report_modifiers(&event.modifiers),
            );
            for _ in 0..lines.abs() {
                if self.session.write(&report).is_err() {
                    break;
                }
            }
        } else if self.session.on_alternate_screen() {
            // xterm's "alternate scroll": a pager on the alternate screen has no history of ours.
            let key: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            for _ in 0..lines.abs() {
                if self.session.write(key).is_err() {
                    break;
                }
            }
        } else {
            self.session.scroll(lines);
        }
        cx.notify();
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        if !self.inside_grid(event.position) {
            return;
        }
        if self.child_has_the_mouse(&event.modifiers) && !event.modifiers.platform {
            self.report_mouse(
                MouseReport::Press(Report::Left),
                event.position,
                &event.modifiers,
            );
            return;
        }
        let Some((point, side)) = self.grid_point(event.position) else {
            return;
        };
        if event.modifiers.platform {
            let url = self.session.with_term(|term| terminal_url_at(term, point));
            if let Some(url) = url {
                cx.open_url(&url);
                return;
            }
        }
        let kind = match event.click_count {
            2 => SelectionType::Semantic,
            3.. => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.dragging = true;
        self.session
            .with_term(|term| term.selection = Some(Selection::new(kind, point, side)));
        cx.notify();
    }

    /// Autoscroll a drag that has left the grid, by how far out it is.
    fn autoscroll(&self, position: Point<Pixels>) {
        let layout = self.layout.get();
        if layout.screen_lines == 0 {
            return;
        }
        let height = self.metrics.cell.height.as_f32().max(1.);
        let bottom = layout.origin.y + self.metrics.cell.height * layout.screen_lines as f32;
        let outside = if position.y < layout.origin.y {
            position.y - layout.origin.y
        } else if position.y > bottom {
            position.y - bottom
        } else {
            return;
        };
        let cells = (outside.as_f32() / height).abs().ceil().clamp(1., 3.);
        let lines = if outside < px(0.) {
            cells as i32
        } else {
            -(cells as i32)
        };
        self.session.scroll(lines);
    }

    fn drag_to(&mut self, position: Point<Pixels>, modifiers: &Modifiers, cx: &mut Context<Self>) {
        if self.child_has_the_mouse(modifiers) {
            if self.session.mouse_reports_drag() {
                self.report_mouse(MouseReport::Motion(Some(Report::Left)), position, modifiers);
            }
            return;
        }
        if !self.dragging {
            return;
        }
        self.autoscroll(position);
        let Some((point, side)) = self.grid_point(position) else {
            return;
        };
        self.session.with_term(|term| {
            if let Some(selection) = term.selection.as_mut() {
                selection.update(point, side);
            }
        });
        cx.notify();
    }

    fn handle_mouse_up(
        &mut self,
        event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dragging = false;
        if self.child_has_the_mouse(&event.modifiers) {
            self.report_mouse(
                MouseReport::Release(Report::Left),
                event.position,
                &event.modifiers,
            );
            cx.notify();
        }
    }

    fn inside_grid(&self, position: Point<Pixels>) -> bool {
        let layout = self.layout.get();
        let x = position.x - layout.origin.x;
        let y = position.y - layout.origin.y;
        x >= px(0.)
            && y >= px(0.)
            && x < self.metrics.cell.width * layout.columns as f32
            && y < self.metrics.cell.height * layout.screen_lines as f32
    }

    /// The grid cell under `position`, clamped to the grid, and which half of it the pointer is in.
    fn grid_point(&self, position: Point<Pixels>) -> Option<(GridPoint, Side)> {
        let layout = self.layout.get();
        if layout.columns == 0 || layout.screen_lines == 0 {
            return None;
        }
        let width = self.metrics.cell.width.as_f32().max(1.);
        let height = self.metrics.cell.height.as_f32().max(1.);
        let x = (position.x - layout.origin.x).as_f32() / width;
        let y = (position.y - layout.origin.y).as_f32() / height;

        let column = (x.floor().max(0.) as usize).min(layout.columns - 1);
        let screen_line = (y.floor().max(0.) as usize).min(layout.screen_lines - 1);
        let display_offset = self.session.with_term(|term| term.grid().display_offset());
        let line = Line(screen_line as i32 - display_offset as i32);
        let side = if x - column as f32 > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        Some((GridPoint::new(line, Column(column)), side))
    }

    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = cx.read_from_clipboard() {
            self.paste_item(item);
        }
    }

    fn paste_item(&mut self, item: ClipboardItem) {
        if let Some(text) = clipboard_paste_text(&item) {
            self.insert_text(&text);
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prepaint_session = self.session.clone();
        let paint_session = self.session.clone();
        let metrics = self.metrics.clone();
        let prepaint_metrics = self.metrics.clone();
        let prepaint_layout = self.layout.clone();
        let focus = self.focus.clone();
        let marked = self.marked.clone();
        // Weak: the frame holds this closure, and a strong handle would keep the view (and child) alive.
        let handler_view = cx.entity().downgrade();
        let colors = Theme::global(cx).tokens.colors;

        div()
            .id(self.element_id.clone())
            .size_full()
            .bg(colors.background)
            .track_focus(&self.focus)
            .key_context("terminal")
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_scroll(cx.listener(Self::handle_scroll_wheel))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        let layout =
                            GridLayout::resolve(bounds, &prepaint_metrics, &prepaint_session);
                        prepaint_layout.set(layout);
                        layout
                    },
                    move |_bounds, layout, window, cx| {
                        let cursor_bounds = paint_grid(
                            layout,
                            &paint_session,
                            &metrics,
                            marked.as_deref(),
                            window,
                            cx,
                        );
                        window.handle_input(
                            &focus,
                            TerminalInputHandler {
                                view: handler_view.clone(),
                                cursor_bounds,
                            },
                            cx,
                        );
                        // The drag follows the pointer at window level, so autoscroll works past the edge.
                        let dragging_view = handler_view.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
                            if phase != DispatchPhase::Capture
                                || event.pressed_button != Some(MouseButton::Left)
                            {
                                return;
                            }
                            dragging_view
                                .update(cx, |view, cx| {
                                    view.drag_to(event.position, &event.modifiers, cx)
                                })
                                .ok();
                        });
                    },
                )
                .size_full(),
            )
    }
}

fn terminal_url_at(term: &Term<EventProxy>, point: GridPoint) -> Option<String> {
    let grid = term.grid();
    if let Some(url) = grid[point].hyperlink().map(|link| link.uri().to_owned())
        && (url.starts_with("https://") || url.starts_with("http://"))
    {
        return Some(url);
    }
    let row = &grid[point.line];
    let chars: Vec<char> = (0..grid.columns())
        .map(|column| row[Column(column)].c)
        .collect();
    url_in_chars(&chars, point.column.0)
}

fn url_in_chars(chars: &[char], column: usize) -> Option<String> {
    if column >= chars.len() || chars[column].is_whitespace() {
        return None;
    }
    let mut start = column;
    let mut end = column + 1;
    while start > 0 && !chars[start - 1].is_whitespace() && !"<>\"'".contains(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && !chars[end].is_whitespace() && !"<>\"'".contains(chars[end]) {
        end += 1;
    }
    while start < end && "([{".contains(chars[start]) {
        start += 1;
    }
    while end > start && ".,;:!?)]}".contains(chars[end - 1]) {
        end -= 1;
    }
    if column < start || column >= end {
        return None;
    }
    let url: String = chars[start..end].iter().collect();
    (url.starts_with("https://") || url.starts_with("http://")).then_some(url)
}

/// The terminal's side of the platform's text-input protocol: IME composition and committed text.
struct TerminalInputHandler {
    view: WeakEntity<TerminalView>,
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        self.view
            .update(cx, |view, _| view.marked_text_range())
            .ok()
            .flatten()
    }

    fn text_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view
            .update(cx, |view, cx| view.commit_text(text, cx))
            .ok();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view
            .update(cx, |view, cx| view.set_marked_text(new_text, cx))
            .ok();
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view
            .update(cx, |view, cx| view.clear_marked_text(cx))
            .ok();
    }

    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, _| view.paste_item(item)).ok();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn prefers_ime_for_printable_keys(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        true
    }
}

/// One frame: backgrounds, selection, text, then the cursor. Returns where the cursor went (for the IME).
fn paint_grid(
    layout: GridLayout,
    session: &TermSession,
    metrics: &CellMetrics,
    marked: Option<&str>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    let palette = Palette::new(Theme::global(cx).tokens.colors);
    let text_system = window.text_system().clone();

    session.with_term(|term| {
        paint_backgrounds(term, layout, metrics, &palette, window);
        paint_selection(term, layout, metrics, &palette, window);
        paint_rows(term, layout, metrics, &palette, &text_system, window, cx);
        if let Some(marked) = marked {
            let bounds = paint_marked(
                term,
                layout,
                metrics,
                &palette,
                marked,
                &text_system,
                window,
                cx,
            );
            return bounds.or_else(|| {
                paint_cursor(term, layout, metrics, &palette, &text_system, window, cx)
            });
        }
        paint_cursor(term, layout, metrics, &palette, &text_system, window, cx)
    })
}

#[allow(clippy::too_many_arguments)]
fn paint_marked(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    marked: &str,
    text_system: &WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    let content = term.renderable_content();
    if content.display_offset != 0 {
        return None;
    }
    let point = content.cursor.point;
    let grid = term.grid();
    if point.line.0 < 0
        || point.line.0 as usize >= grid.screen_lines()
        || point.column.0 >= grid.columns()
    {
        return None;
    }
    let run = TextRun {
        len: marked.len(),
        font: metrics.font.clone(),
        color: palette.foreground,
        underline: Some(UnderlineStyle {
            color: None,
            thickness: px(1.),
            wavy: false,
        }),
        ..Default::default()
    };
    let shaped = text_system.shape_line(marked.to_string().into(), metrics.font_size, &[run], None);
    let width = shaped.width().max(metrics.cell.width);
    // shortcut: one row, clipped at the grid edge; wrap it if long compositions ever matter.
    let bounds = Bounds::new(
        layout
            .cells(metrics, point.line.0 as usize, point.column.0, 1)
            .origin,
        size(width, metrics.cell.height),
    );
    window.paint_quad(fill(bounds, palette.background));
    if let Err(error) = shaped.paint(
        bounds.origin,
        metrics.cell.height,
        TextAlign::Left,
        None,
        window,
        cx,
    ) {
        log::warn!("terminal composition paint failed: {error}");
    }
    Some(bounds)
}

fn paint_selection(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    window: &mut Window,
) {
    for (screen_line, from, to) in selection_spans(term) {
        window.paint_quad(fill(
            layout.cells(metrics, screen_line, from, to - from + 1),
            palette.selection,
        ));
    }
}

/// The selected columns of each screen row, inclusive, top to bottom.
fn selection_spans(term: &Term<EventProxy>) -> Vec<(usize, usize, usize)> {
    let Some(range) = term
        .selection
        .as_ref()
        .and_then(|selection| selection.to_range(term))
    else {
        return Vec::new();
    };
    let grid = term.grid();
    let columns = grid.columns();
    let offset = grid.display_offset() as i32;
    let mut spans = Vec::new();
    for screen_line in 0..grid.screen_lines() {
        let line = screen_line as i32 - offset;
        if line < range.start.line.0 || line > range.end.line.0 {
            continue;
        }
        let (from, to) = if range.is_block {
            (range.start.column.0, range.end.column.0)
        } else {
            (
                if line == range.start.line.0 {
                    range.start.column.0
                } else {
                    0
                },
                if line == range.end.line.0 {
                    range.end.column.0
                } else {
                    columns - 1
                },
            )
        };
        if from <= to {
            spans.push((screen_line, from, to));
        }
    }
    spans
}

/// One quad per run of same-coloured cells, not one per cell.
fn paint_backgrounds(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    window: &mut Window,
) {
    let grid = term.grid();
    let offset = grid.display_offset();
    let columns = grid.columns();
    for screen_line in 0..grid.screen_lines() {
        let row = &grid[Line(screen_line as i32 - offset as i32)];
        let mut column = 0;
        while column < columns {
            let background = palette.resolve(&row[Column(column)]).1;
            let start = column;
            column += 1;
            while column < columns && palette.resolve(&row[Column(column)]).1 == background {
                column += 1;
            }
            if background == palette.background {
                continue;
            }
            window.paint_quad(fill(
                layout.cells(metrics, screen_line, start, column - start),
                background,
            ));
        }
    }
}

/// One shaped line per row, up to the last cell that has something in it.
#[allow(clippy::too_many_arguments)]
fn paint_rows(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    text_system: &WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) {
    let grid = term.grid();
    let offset = grid.display_offset();
    let columns = grid.columns();

    for screen_line in 0..grid.screen_lines() {
        let line = Line(screen_line as i32 - offset as i32);
        let row = &grid[line];
        let Some(end_column) = last_inked_column(grid, line, columns) else {
            continue;
        };

        let mut text = String::new();
        let mut runs: Vec<TextRun> = Vec::new();
        let mut style: Option<RunStyle> = None;
        let mut run_len = 0usize;

        for column in 0..=end_column {
            let cell = &row[Column(column)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let (color, _) = palette.resolve(cell);
            let next = RunStyle::of(cell, color);
            if style.as_ref() != Some(&next)
                && let Some(previous) = style.replace(next)
            {
                runs.push(previous.run(run_len, metrics));
                run_len = 0;
            }
            let glyph = if cell.flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };
            text.push(glyph);
            run_len += glyph.len_utf8();
            // `force_width` advances one slot per glyph, so a wide glyph needs an empty slot for its second cell.
            if cell.flags.contains(Flags::WIDE_CHAR) {
                text.push(' ');
                run_len += 1;
            }
            if let Some(zerowidth) = cell.zerowidth() {
                for mark in zerowidth {
                    text.push(*mark);
                    run_len += mark.len_utf8();
                }
            }
        }

        let Some(last) = style else { continue };
        runs.push(last.run(run_len, metrics));

        // Snap every glyph to its own cell, as Zed's terminal does.
        let shaped = text_system.shape_line(
            text.into(),
            metrics.font_size,
            &runs,
            Some(metrics.cell.width),
        );
        let origin = Point::new(
            layout.origin.x,
            layout.origin.y + metrics.cell.height * screen_line as f32,
        );
        if let Err(error) = shaped.paint(
            origin,
            metrics.cell.height,
            TextAlign::Left,
            None,
            window,
            cx,
        ) {
            log::warn!("terminal row paint failed: {error}");
        }
    }
}

/// The cursor, over the text it sits on; a block cursor re-draws its glyph in the background colour.
fn paint_cursor(
    term: &Term<EventProxy>,
    layout: GridLayout,
    metrics: &CellMetrics,
    palette: &Palette,
    text_system: &WindowTextSystem,
    window: &mut Window,
    cx: &mut App,
) -> Option<Bounds<Pixels>> {
    let content = term.renderable_content();
    if content.display_offset != 0 {
        return None;
    }
    let cursor = content.cursor;
    let row = cursor.point.line.0;
    let column = cursor.point.column.0;
    let grid = term.grid();
    if row < 0 || row as usize >= grid.screen_lines() || column >= grid.columns() {
        return None;
    }
    let cell = layout.cells(metrics, row as usize, column, 1);
    // A hidden cursor still answers where the IME goes: TUIs park it at their own insertion point.
    if cursor.shape == CursorShape::Hidden {
        return Some(cell);
    }
    let block = match cursor.shape {
        CursorShape::Beam => Bounds::new(cell.origin, size(px(2.), metrics.cell.height)),
        CursorShape::Underline => Bounds::new(
            Point::new(cell.origin.x, cell.origin.y + metrics.cell.height - px(2.)),
            size(metrics.cell.width, px(2.)),
        ),
        _ => cell,
    };
    window.paint_quad(fill(block, palette.cursor));
    if !matches!(cursor.shape, CursorShape::Block) {
        return Some(block);
    }
    let glyph = grid[Line(row)][Column(column)].c;
    if glyph == ' ' {
        return Some(block);
    }
    let run = TextRun {
        len: glyph.len_utf8(),
        font: metrics.font.clone(),
        color: palette.background,
        ..Default::default()
    };
    let shaped = text_system.shape_line(glyph.to_string().into(), metrics.font_size, &[run], None);
    if let Err(error) = shaped.paint(
        cell.origin,
        metrics.cell.height,
        TextAlign::Left,
        None,
        window,
        cx,
    ) {
        log::warn!("terminal cursor paint failed: {error}");
    }
    Some(block)
}

fn last_inked_column(grid: &Grid<Cell>, line: Line, columns: usize) -> Option<usize> {
    (0..columns).rev().find(|&column| {
        let cell = &grid[line][Column(column)];
        !(cell.c == ' '
            && cell.zerowidth().is_none()
            && cell.flags.is_empty()
            && cell.fg == AnsiColor::Named(NamedColor::Foreground)
            && cell.bg == AnsiColor::Named(NamedColor::Background))
    })
}

#[derive(Clone, PartialEq)]
struct RunStyle {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strikeout: bool,
}

impl RunStyle {
    fn of(cell: &Cell, color: Hsla) -> Self {
        Self {
            color,
            bold: cell.flags.intersects(Flags::BOLD | Flags::DIM_BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            strikeout: cell.flags.contains(Flags::STRIKEOUT),
        }
    }

    fn run(&self, len: usize, metrics: &CellMetrics) -> TextRun {
        let mut font = metrics.font.clone();
        if self.bold {
            font = font.bold();
        }
        if self.italic {
            font = font.italic();
        }
        TextRun {
            len,
            font,
            color: self.color,
            underline: self.underline.then(|| UnderlineStyle {
                color: Some(self.color),
                thickness: px(1.),
                wavy: false,
            }),
            strikethrough: self.strikeout.then(|| StrikethroughStyle {
                color: Some(self.color),
                thickness: px(1.),
            }),
            ..Default::default()
        }
    }
}

/// The theme's own roles, plus the xterm table for everything the child names.
#[derive(Clone, Copy)]
struct Palette {
    background: Hsla,
    foreground: Hsla,
    cursor: Hsla,
    selection: Hsla,
    /// A light background swaps the named sixteen for ones that read on it.
    light: bool,
}

impl Palette {
    fn new(colors: ColorTokens) -> Self {
        Self {
            background: colors.background,
            foreground: colors.foreground,
            cursor: colors.foreground,
            selection: colors.selection,
            light: colors.background.l > 0.5,
        }
    }

    /// A cell's colours, as `(foreground, background)`.
    fn resolve(&self, cell: &Cell) -> (Hsla, Hsla) {
        let mut foreground = self.foreground_of(cell.fg);
        let mut background = self.background_of(cell.bg);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut foreground, &mut background);
        }
        if cell.flags.contains(Flags::DIM) {
            foreground = foreground.opacity(0.6);
        }
        (foreground, background)
    }

    fn foreground_of(&self, color: AnsiColor) -> Hsla {
        match color {
            AnsiColor::Named(NamedColor::Foreground) => self.foreground,
            AnsiColor::Named(NamedColor::Background) => self.background,
            AnsiColor::Named(NamedColor::Cursor) => self.cursor,
            AnsiColor::Named(named) => self.indexed(named_index(named)),
            AnsiColor::Spec(rgb) => ansi_rgb(rgb),
            AnsiColor::Indexed(index) => self.indexed(index),
        }
    }

    fn background_of(&self, color: AnsiColor) -> Hsla {
        match color {
            AnsiColor::Named(NamedColor::Background) => self.background,
            AnsiColor::Named(NamedColor::Foreground) => self.foreground,
            other => self.foreground_of(other),
        }
    }

    fn indexed(&self, index: u8) -> Hsla {
        match LIGHT_BASE.get(index as usize) {
            Some(&rgb) if self.light => rgb_hsla(rgb),
            _ => xterm256(index),
        }
    }
}

/// The named sixteen for a light background (GitHub Light's terminal colours).
const LIGHT_BASE: [(u8, u8, u8); 16] = [
    (0x24, 0x29, 0x2f),
    (0xcf, 0x22, 0x2e),
    (0x11, 0x63, 0x29),
    (0x4d, 0x2d, 0x00),
    (0x09, 0x69, 0xda),
    (0x82, 0x50, 0xdf),
    (0x1b, 0x7c, 0x83),
    (0x6e, 0x77, 0x81),
    (0x57, 0x60, 0x6a),
    (0xa4, 0x0e, 0x26),
    (0x1a, 0x7f, 0x37),
    (0x63, 0x3c, 0x01),
    (0x21, 0x8b, 0xff),
    (0xa4, 0x75, 0xf9),
    (0x31, 0x92, 0xaa),
    (0x8c, 0x95, 0x9f),
];

/// A `NamedColor` as an index into the xterm table; the dim set (259+) maps back to the base sixteen.
fn named_index(named: NamedColor) -> u8 {
    let index = named as usize;
    if index <= 15 {
        return index as u8;
    }
    if index >= 259 {
        return ((index - 259) % 16) as u8;
    }
    7
}

fn rgb_hsla((r, g, b): (u8, u8, u8)) -> Hsla {
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a: 1.,
    }
    .into()
}

fn ansi_rgb(rgb: AnsiRgb) -> Hsla {
    rgb_hsla((rgb.r, rgb.g, rgb.b))
}

/// The xterm 256-colour table: sixteen named, a 6×6×6 cube, then twenty-four greys.
fn xterm256(index: u8) -> Hsla {
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xcd, 0x00, 0x00),
        (0x00, 0xcd, 0x00),
        (0xcd, 0xcd, 0x00),
        (0x00, 0x00, 0xee),
        (0xcd, 0x00, 0xcd),
        (0x00, 0xcd, 0xcd),
        (0xe5, 0xe5, 0xe5),
        (0x7f, 0x7f, 0x7f),
        (0xff, 0x00, 0x00),
        (0x00, 0xff, 0x00),
        (0xff, 0xff, 0x00),
        (0x5c, 0x5c, 0xff),
        (0xff, 0x00, 0xff),
        (0x00, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let rgb = match index {
        0..=15 => BASE[index as usize],
        16..=231 => {
            let offset = index as u16 - 16;
            let step = |part: u16| STEPS[(part % 6) as usize];
            (step(offset / 36), step(offset / 6), step(offset))
        }
        _ => {
            let grey = (8 + 10 * (index as u16 - 232)) as u8;
            (grey, grey, grey)
        }
    };
    rgb_hsla(rgb)
}

/// One keystroke as the bytes a shell expects, or `None` for a platform-modifier key (the app's).
pub fn key_to_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let modifiers = keystroke.modifiers;
    if modifiers.platform {
        return None;
    }
    let key = keystroke.key.as_str();
    // Control characters first: `ctrl-c` must not fall through to the letter.
    if modifiers.control
        && let Some(control) = control_byte(key, keystroke.key_char.as_deref())
    {
        return Some(with_alt(vec![control], modifiers.alt));
    }
    if let Some(encoded) = named_key(key, modifiers.shift) {
        return Some(with_alt(encoded, modifiers.alt));
    }
    // What the keyboard actually typed, with shift and layouts already applied.
    if let Some(typed) = keystroke.key_char.as_deref()
        && !typed.is_empty()
    {
        return Some(with_alt(typed.as_bytes().to_vec(), modifiers.alt));
    }
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(with_alt(ch.to_string().into_bytes(), modifiers.alt))
}

fn control_byte(key: &str, typed: Option<&str>) -> Option<u8> {
    if key == "space" {
        return Some(0x00);
    }
    let key = typed.unwrap_or(key);
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match ch {
        'a'..='z' => Some(ch as u8 - b'a' + 1),
        'A'..='Z' => Some(ch as u8 - b'A' + 1),
        ' ' | '@' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

fn named_key(key: &str, shift: bool) -> Option<Vec<u8>> {
    let bytes: &[u8] = match key {
        "enter" => b"\r",
        "tab" if shift => b"\x1b[Z",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "escape" => b"\x1b",
        "space" => b" ",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
}

fn with_alt(mut bytes: Vec<u8>, alt: bool) -> Vec<u8> {
    if alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardKey {
    Copy,
    Paste,
}

/// ⌘C / ⌘V (or ctrl-shift-C/V) are the app's, not the child's.
pub fn clipboard_key(keystroke: &Keystroke) -> Option<ClipboardKey> {
    let modifiers = keystroke.modifiers;
    let clipboard = (modifiers.platform || (modifiers.control && modifiers.shift))
        && !modifiers.alt
        && !modifiers.function;
    if !clipboard {
        return None;
    }
    match keystroke.key.as_str() {
        "c" => Some(ClipboardKey::Copy),
        "v" => Some(ClipboardKey::Paste),
        _ => None,
    }
}

/// Pasted text as the bytes to hand the child; ESC is stripped inside brackets so a paste cannot end itself.
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend(text.bytes().filter(|byte| *byte != 0x1b));
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Clipboard images need a file path before a terminal agent can read them.
fn clipboard_paste_text(item: &ClipboardItem) -> Option<String> {
    let text = match item.entries().first() {
        Some(ClipboardEntry::Image(image)) => {
            let path = std::env::temp_dir().join(format!(
                "lumireel-paste-{}.{}",
                image.id(),
                image.format.extension()
            ));
            if let Err(error) = std::fs::write(&path, &image.bytes) {
                log::warn!("saving pasted image failed: {error}");
                return None;
            }
            shell_escape(&path)
        }
        Some(ClipboardEntry::ExternalPaths(paths)) => paths
            .0
            .iter()
            .map(|path| shell_escape(path))
            .collect::<Vec<_>>()
            .join(" "),
        _ => item.text()?,
    };
    (!text.is_empty()).then_some(text)
}

/// A path as a shell will read it back whole.
pub fn shell_escape(path: &Path) -> String {
    let text = path.to_string_lossy();
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
    {
        return text.into_owned();
    }
    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('\'');
    for c in text.chars() {
        if c == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(c);
        }
    }
    escaped.push('\'');
    escaped
}

#[cfg(test)]
mod tests {
    // Not `use super::*`: gpui's glob brings its own `test` attribute, which would shadow the built-in one.
    use gpui_kit::Keystroke;
    use std::path::Path;

    use super::{
        ClipboardKey, clipboard_key, key_to_bytes, paste_bytes, shell_escape, url_in_chars,
        xterm256,
    };

    fn key(spec: &'static str) -> Option<Vec<u8>> {
        key_to_bytes(&Keystroke::parse(spec).unwrap())
    }

    #[test]
    fn keys_become_the_bytes_a_shell_expects() {
        assert_eq!(key("enter"), Some(b"\r".to_vec()));
        assert_eq!(key("ctrl-c"), Some(vec![0x03]));
        assert_eq!(key("shift-tab"), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key("alt-b"), Some(b"\x1bb".to_vec()));
        assert_eq!(key("cmd-t"), None);
        assert_eq!(
            clipboard_key(&Keystroke::parse("cmd-v").unwrap()),
            Some(ClipboardKey::Paste)
        );
    }

    #[test]
    fn bracketed_paste_wraps_and_strips_escapes() {
        assert_eq!(paste_bytes("a\x1bb", false), b"a\x1bb");
        assert_eq!(paste_bytes("a\x1bb", true), b"\x1b[200~ab\x1b[201~");
    }

    #[test]
    fn urls_open_only_inside_the_url() {
        let chars: Vec<char> = "(https://pi.dev/changelog).".chars().collect();
        assert_eq!(
            url_in_chars(&chars, 10),
            Some("https://pi.dev/changelog".into())
        );
        assert_eq!(url_in_chars(&chars, 0), None);
    }

    #[test]
    fn paths_are_shell_escaped() {
        assert_eq!(shell_escape(Path::new("/tmp/a.png")), "/tmp/a.png");
        assert_eq!(shell_escape(Path::new("/tmp/it's")), "'/tmp/it'\\''s'");
    }

    #[test]
    fn the_256_colour_cube_and_greys() {
        let white = xterm256(231).to_rgb();
        assert!((white.r - 1.).abs() < 0.01 && (white.g - 1.).abs() < 0.01);
        let grey = xterm256(232).to_rgb();
        assert!((grey.r - 8. / 255.).abs() < 0.01);
    }
}
