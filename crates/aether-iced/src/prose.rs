//! The prose layer: the view's [`aether_protocol::ui::Element::Prose`] nodes, drawn as real
//! typography over the editor's grid.
//!
//! The other two shells render prose in the same flow as the rows around it — the browser because
//! the DOM lays a block out beside a row, the terminal because a rendered row *is* a row. This
//! shell cannot: its editor is one widget that paints every row itself, from an absolute row
//! number under a pixel scroll. So prose is a second layer over that widget, each element placed
//! at the origin the shared grid gives it ([`crate::grid::element_origins`]) less the scroll — the
//! same arithmetic the editor uses for its own rows, so the two cannot drift apart.
//!
//! **The layer takes no input.** A reply is a record of what was said: it has no cursor, no focus
//! and no links out. Not forwarding events is what lets a click land on the editor underneath, and
//! is why the layer's own message type never matters.
//!
//! What it does forward is [`Widget::operate`], which is how the app measures it: proportional type
//! has no height until it has been laid out, and everything that scrolls or places this view
//! positions by that height.

use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{layout, mouse, overlay, renderer, Clipboard, Layout, Shell, Widget};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector};

/// One prose element and where its top sits, in pixels from the top of the layer. Negative for an
/// element scrolled off the top, which is ordinary: the layer clips.
pub struct Placed<'a, Message, Theme, Renderer> {
    pub y: f32,
    pub content: Element<'a, Message, Theme, Renderer>,
}

/// The prose elements of one view, each at its own offset. See the module docs.
pub struct ProseLayer<'a, Message, Theme, Renderer> {
    children: Vec<Placed<'a, Message, Theme, Renderer>>,
}

/// The prose layer over `children`.
pub fn prose_layer<'a, Message, Theme, Renderer>(
    children: Vec<Placed<'a, Message, Theme, Renderer>>,
) -> ProseLayer<'a, Message, Theme, Renderer> {
    ProseLayer { children }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for ProseLayer<'_, Message, Theme, Renderer>
where
    Renderer: renderer::Renderer,
{
    fn children(&self) -> Vec<Tree> {
        self.children
            .iter()
            .map(|c| Tree::new(&c.content))
            .collect()
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children_custom(
            &self.children,
            |state, child| child.content.as_widget().diff(state),
            |child| Tree::new(&child.content),
        );
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    /// Each element at its own offset, laid out to the layer's full width and as tall as it wants
    /// to be. The height limit is deliberately the layer's own rather than what is left below `y`:
    /// this is a measurement as much as a placement, and an element clipped by the bottom of the
    /// pane must still report the height it *would* take, or the view could never scroll to it.
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let bounds = limits.max();
        let inner = layout::Limits::new(Size::ZERO, Size::new(bounds.width, f32::INFINITY));
        let nodes = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(child, state)| {
                child
                    .content
                    .as_widget_mut()
                    .layout(state, renderer, &inner)
                    .move_to(Point::new(0.0, child.y))
            })
            .collect();
        layout::Node::with_children(bounds, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        for ((child, state), layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child
                .content
                .as_widget_mut()
                .operate(state, layout, renderer, operation);
        }
    }

    /// Nothing. See the module docs: the layer is a record, and an event it swallowed would be one
    /// the editor beneath never saw.
    fn update(
        &mut self,
        _tree: &mut Tree,
        _event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        _shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
    }

    /// None, for the same reason: the pointer belongs to the editor underneath, and a prose element
    /// claiming it would change the cursor over half the conversation.
    fn mouse_interaction(
        &self,
        _tree: &Tree,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::None
    }

    /// Clipped to the layer — an element is placed at an absolute offset and routinely reaches past
    /// both ends of it, and without the clip a scrolled reply paints over the status bar.
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
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        renderer.with_layer(clip, |renderer| {
            for ((child, state), layout) in self
                .children
                .iter()
                .zip(&tree.children)
                .zip(layout.children())
            {
                // Skip what is wholly outside the layer: a long conversation is mostly off screen,
                // and a block's own draw still walks its inlines.
                if layout.bounds().intersection(&clip).is_none() {
                    continue;
                }
                child
                    .content
                    .as_widget()
                    .draw(state, renderer, theme, style, layout, cursor, &clip);
            }
        });
    }

    fn overlay<'b>(
        &'b mut self,
        _tree: &'b mut Tree,
        _layout: Layout<'b>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        None
    }
}

impl<'a, Message, Theme, Renderer> From<ProseLayer<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: renderer::Renderer + 'a,
{
    fn from(layer: ProseLayer<'a, Message, Theme, Renderer>) -> Self {
        Element::new(layer)
    }
}
