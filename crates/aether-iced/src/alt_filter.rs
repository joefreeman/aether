//! A transparent decorator widget that hides `Alt`-modified key presses from its child.
//!
//! Overlay text fields use a real `iced::widget::text_input`, but on platforms where winit delivers
//! `Alt+letter` as text (e.g. Linux), a focused `text_input` *inserts* that character — yet
//! `Alt-j/k/l` are the app's navigation chords, not text. Rather than undo the insert afterward
//! (which races the query filter), this wraps the input and drops `Alt`-modified `KeyPressed`
//! events *before* the inner widget sees them: nothing is inserted, and — since the event is left
//! uncaptured — it bubbles to the application's key subscription, which routes the chord to the
//! core's keymap. Every other event is delegated unchanged, so focus/caret/click/selection/typing
//! all behave exactly like a plain `text_input`. Mirrors iced's own `opaque` decorator pattern.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{tree, Operation, Tree};
use iced::advanced::{mouse, overlay, renderer, Clipboard, Shell, Widget};
use iced::{Element, Event, Length, Rectangle, Size, Vector};

/// Decides whether an unmodified key press at the inner input's boundary should be intercepted.
/// Called with the iced key and whether the caret sits at the start of the field; a returned
/// message is published and the event consumed (the inner `text_input` never sees it).
type Intercept<'a, Message> = Box<dyn Fn(&iced::keyboard::Key, bool) -> Option<Message> + 'a>;

struct AltFilter<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
    /// When set, each unmodified key press is offered to this closure (with the key and a
    /// caret-at-start flag); if it returns a message, that's published and the event consumed
    /// instead of reaching the inner input — for the chip-boundary gestures a focused `text_input`
    /// would otherwise swallow (step into the chip row from the query; `:` confirm-root / Backspace
    /// step-to-root in the chip editor). Publishing directly (rather than letting the event bubble
    /// to the app's key subscription) is deterministic: the subscription only forwards `Ignored`
    /// keys, and a declined-but-uncaptured key doesn't reliably read as `Ignored` there.
    intercept: Option<Intercept<'a, Message>>,
    /// The inner input's current text — lets [`Widget::update`] resolve the caret to a char index
    /// (the cursor in the widget state is a position; `Value` gives it length to clamp against).
    /// Only read when `intercept` is set.
    value: String,
}

/// Wrap `content` so `Alt`-modified key presses never reach it (they bubble to the app instead).
pub fn alt_passthrough<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::text::Renderer + 'a,
{
    Element::new(AltFilter {
        content: content.into(),
        intercept: None,
        value: String::new(),
    })
}

/// Like [`alt_passthrough`], but consults `intercept` for each unmodified key press (see
/// [`AltFilter::intercept`]). `value` is the inner input's current text, for the caret-at-start
/// check the closure receives.
pub fn alt_passthrough_intercept<'a, Message, Theme, Renderer>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
    value: String,
    intercept: impl Fn(&iced::keyboard::Key, bool) -> Option<Message> + 'a,
) -> Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: iced::advanced::text::Renderer + 'a,
{
    Element::new(AltFilter {
        content: content.into(),
        intercept: Some(Box::new(intercept)),
        value,
    })
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for AltFilter<'_, Message, Theme, Renderer>
where
    Renderer: iced::advanced::text::Renderer,
{
    fn tag(&self) -> tree::Tag {
        self.content.as_widget().tag()
    }

    fn state(&self) -> tree::State {
        self.content.as_widget().state()
    }

    fn children(&self) -> Vec<Tree> {
        self.content.as_widget().children()
    }

    fn diff(&self, tree: &mut Tree) {
        self.content.as_widget().diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content.as_widget_mut().layout(tree, renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content
            .as_widget()
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Swallow `Alt`-modified key presses (the app's command/nav chords): don't delegate to the
        // inner widget, and don't capture — leaving the event `Ignored` so it bubbles to the key
        // subscription. Everything else passes through untouched.
        if let Event::Keyboard(iced::keyboard::Event::KeyPressed {
            key, modifiers, ..
        }) = event
        {
            if modifiers.alt() {
                return;
            }
            // At a field boundary, an unmodified key may be a chip gesture rather than an edit —
            // offer it to `intercept`; if it claims the key, publish the message and consume the
            // event so the inner input never sees it. Otherwise fall through and edit as usual.
            if let Some(intercept) = &self.intercept {
                if !modifiers.control() && !modifiers.command() {
                    use iced::widget::text_input;
                    // `state()` is delegated to the inner `text_input`, so its tree state is a
                    // `text_input::State`; resolve the caret to a char index against the value.
                    let at_start = matches!(
                        tree.state
                            .downcast_ref::<text_input::State<Renderer::Paragraph>>()
                            .cursor()
                            .state(&text_input::Value::new(&self.value)),
                        text_input::cursor::State::Index(0)
                    );
                    if let Some(msg) = intercept(key, at_start) {
                        shell.publish(msg);
                        shell.capture_event();
                        return;
                    }
                }
            }
        }
        self.content.as_widget_mut().update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_widget()
            .mouse_interaction(tree, layout, cursor, viewport, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(tree, layout, renderer, viewport, translation)
    }
}
