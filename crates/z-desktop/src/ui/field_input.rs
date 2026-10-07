//! Real GPUI text field: shaped line, painted caret/selection (see gpui examples/input.rs).
//! Caret is a paint quad — it does not take layout space.

use crate::shared::theme;
use crate::ui::actions::{NextField, PrevField, SubmitFocused};
use gpui::{
    actions, div, fill, point, prelude::*, px, relative, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    SharedString, Style, TextAlign, TextRun, UTF16Selection, Window, WrappedLine,
};
use std::ops::Range;
use std::rc::Rc;

actions!(
    field_input,
    [
        Backspace,
        Delete,
        MoveLeft,
        MoveRight,
        SelectLeft,
        SelectRight,
        SelectAll,
        LineStart,
        LineEnd,
        Paste,
        Cut,
        Copy
    ]
);

pub enum FieldEvent {
    Edited,
    Tab { back: bool },
    Submit,
}

pub struct FieldInput {
    id: SharedString,
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<WrappedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    last_line_height: Pixels,
    is_selecting: bool,
    masked: bool,
    multiline: bool,
    min_h: Pixels,
    disabled: bool,
    on_nav: Option<Rc<dyn Fn(FieldEvent, &mut Window, &mut App)>>,
}

impl EventEmitter<FieldEvent> for FieldInput {}

impl FieldInput {
    pub fn new(
        cx: &mut Context<Self>,
        id: impl Into<SharedString>,
        content: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        masked: bool,
        multiline: bool,
    ) -> Self {
        let content: SharedString = content.into();
        let len = content.len();
        Self {
            id: id.into(),
            focus_handle: cx.focus_handle(),
            content,
            placeholder: placeholder.into(),
            selected_range: len..len,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            last_line_height: px(20.0),
            is_selecting: false,
            masked,
            multiline,
            min_h: if multiline { px(96.0) } else { px(22.0) },
            disabled: false,
            on_nav: None,
        }
    }

    pub fn set_min_h(&mut self, h: Pixels) {
        self.min_h = h;
    }

    pub fn set_nav(&mut self, nav: Rc<dyn Fn(FieldEvent, &mut Window, &mut App)>) {
        self.on_nav = Some(nav);
    }

    pub fn text(&self) -> String {
        self.content.to_string()
    }

    pub fn set_text(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let content: SharedString = text.into();
        let len = content.len();
        self.content = content;
        self.selected_range = len..len;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        self.masked = masked;
        cx.notify();
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        self.disabled = disabled;
        cx.notify();
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn handle(&self) -> FocusHandle {
        self.focus_handle.clone()
    }

    fn display_text(&self) -> SharedString {
        if self.content.is_empty() {
            return self.placeholder.clone();
        }
        if self.masked {
            "*".repeat(self.content.chars().count()).into()
        } else {
            self.content.clone()
        }
    }

    fn to_display_index(&self, byte: usize) -> usize {
        if self.masked {
            self.content[..byte.min(self.content.len())].chars().count()
        } else {
            byte.min(self.display_text().len())
        }
    }

    fn from_display_index(&self, display_ix: usize) -> usize {
        if self.masked {
            self.content
                .char_indices()
                .nth(display_ix)
                .map(|(i, _)| i)
                .unwrap_or(self.content.len())
        } else {
            display_ix.min(self.content.len())
        }
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .rev()
            .find(|(i, _)| *i < offset)
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .char_indices()
            .find(|(i, _)| *i > offset)
            .map(|(i, _)| i)
            .unwrap_or(self.content.len())
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn index_for_mouse(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return self.content.len();
        };
        let local = point(position.x - bounds.origin.x, position.y - bounds.origin.y);
        let display_ix = line
            .closest_index_for_position(local, self.last_line_height)
            .unwrap_or_else(|i| i);
        self.from_display_index(display_ix)
    }

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn line_start(&mut self, _: &LineStart, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            self.move_to(0, cx);
        }
    }

    fn line_end(&mut self, _: &LineEnd, _: &mut Window, cx: &mut Context<Self>) {
        if !self.disabled {
            self.move_to(self.content.len(), cx);
        }
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled {
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace(['\n', '\r'], " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if self.disabled || self.selected_range.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(
            self.content[self.selected_range.clone()].to_string(),
        ));
        self.replace_text_in_range(None, "", window, cx);
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        window.focus(&self.focus_handle);
        if event.click_count >= 2 {
            self.move_to(0, cx);
            self.select_to(self.content.len(), cx);
            self.is_selecting = false;
            return;
        }
        self.is_selecting = true;
        let ix = self.index_for_mouse(event.position);
        if event.modifiers.shift {
            self.select_to(ix, cx);
        } else {
            self.move_to(ix, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting && !self.disabled {
            self.select_to(self.index_for_mouse(event.position), cx);
        }
    }

    fn on_next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(nav) = self.on_nav.clone() {
            nav(FieldEvent::Tab { back: false }, window, cx);
        }
    }

    fn on_prev_field(&mut self, _: &PrevField, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(nav) = self.on_nav.clone() {
            nav(FieldEvent::Tab { back: true }, window, cx);
        }
    }

    fn on_submit(&mut self, _: &SubmitFocused, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(nav) = self.on_nav.clone() {
            nav(FieldEvent::Submit, window, cx);
        }
    }
}

impl EntityInputHandler for FieldInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let cleaned = new_text.replace(['\n', '\r'], " ");
        self.content =
            (self.content[0..range.start].to_owned() + &cleaned + &self.content[range.end..])
                .into();
        let new_end = range.start + cleaned.len();
        self.selected_range = new_end..new_end;
        self.marked_range.take();
        cx.emit(FieldEvent::Edited);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.selected_range.clone());
        let cleaned = new_text.replace(['\n', '\r'], " ");
        self.content =
            (self.content[0..range.start].to_owned() + &cleaned + &self.content[range.end..])
                .into();
        if !cleaned.is_empty() {
            self.marked_range = Some(range.start..range.start + cleaned.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| {
                let end = range.start + cleaned.len();
                end..end
            });
        cx.emit(FieldEvent::Edited);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let line_height = self.last_line_height;
        let start =
            last_layout.position_for_index(self.to_display_index(range.start), line_height)?;
        let end = last_layout.position_for_index(self.to_display_index(range.end), line_height)?;
        Some(Bounds::from_corners(
            point(bounds.left() + start.x, bounds.top() + start.y),
            point(bounds.left() + end.x, bounds.top() + end.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(self.offset_to_utf16(self.index_for_mouse(point)))
    }
}

struct TextPaint {
    input: Entity<FieldInput>,
}

struct PrepaintState {
    line: Option<WrappedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextPaint {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextPaint {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (multiline, min_h) = {
            let input = self.input.read(cx);
            (input.multiline, input.min_h)
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = if multiline {
            min_h.into()
        } else {
            window.line_height().into()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let empty = input.content.is_empty();
        let display = input.display_text();
        let selected = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let multiline = input.multiline;
        let style = window.text_style();
        let color = if empty {
            theme::muted()
        } else {
            theme::text_color()
        };
        let run = TextRun {
            len: display.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let wrap = if multiline {
            Some(bounds.size.width.max(px(8.0)))
        } else {
            None
        };
        let line = window
            .text_system()
            .shape_text(display, font_size, &[run], wrap, None)
            .ok()
            .and_then(|mut lines| lines.pop())
            .unwrap_or_default();

        let line_height = window.line_height();
        let display_cursor = input.to_display_index(cursor);
        let (selection, cursor_quad) = if empty || selected.is_empty() {
            let pos = line
                .position_for_index(display_cursor, line_height)
                .unwrap_or_else(|| point(px(0.0), px(0.0)));
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + pos.x, bounds.top() + pos.y),
                        size(px(1.5), line_height),
                    ),
                    theme::plate(),
                )),
            )
        } else {
            let a = input.to_display_index(selected.start);
            let b = input.to_display_index(selected.end);
            let start = line
                .position_for_index(a, line_height)
                .unwrap_or_else(|| point(px(0.0), px(0.0)));
            let end = line
                .position_for_index(b, line_height)
                .unwrap_or_else(|| point(px(0.0), px(0.0)));
            let same_line = start.y == end.y;
            let quad = if same_line {
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left() + start.x, bounds.top() + start.y),
                        point(bounds.left() + end.x, bounds.top() + end.y + line_height),
                    ),
                    theme::plate().opacity(0.28),
                ))
            } else {
                // Full-width band covering wrapped selection; glyphs stay put.
                Some(fill(
                    Bounds::from_corners(
                        point(bounds.left(), bounds.top() + start.y),
                        point(bounds.right(), bounds.top() + end.y + line_height),
                    ),
                    theme::plate().opacity(0.22),
                ))
            };
            (quad, None)
        };
        PrepaintState {
            line: Some(line),
            cursor: cursor_quad,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        if let Some(line) = prepaint.line.take() {
            let _ = line.paint(
                bounds.origin,
                window.line_height(),
                TextAlign::Left,
                Some(bounds),
                window,
                cx,
            );
            let lh = window.line_height();
            self.input.update(cx, |input, _cx| {
                input.last_layout = Some(line);
                input.last_bounds = Some(bounds);
                input.last_line_height = lh;
            });
        }
        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }
    }
}

impl Render for FieldInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        div()
            .id(self.id.clone())
            .key_context("FieldInput")
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::on_next_field))
            .on_action(cx.listener(Self::on_prev_field))
            .on_action(cx.listener(Self::on_submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .px_3()
            .py_2()
            .bg(theme::field_bg())
            .border_1()
            .border_color(if self.disabled {
                theme::line()
            } else if focused {
                theme::plate()
            } else {
                theme::line()
            })
            .rounded(px(6.0))
            .overflow_hidden()
            .opacity(if self.disabled { 0.55 } else { 1.0 })
            .child(TextPaint { input: cx.entity() })
    }
}

impl Focusable for FieldInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
