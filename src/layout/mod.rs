//! This module handles creating the layout tree, which includes
//! arranging elements and performing text layout.

use crate::dom::{element::DynamicNode, node::AnyNodeData, Node, Window};
use crate::style::{BlockValues, ComputedValues, Direction, DisplayType};
use crate::util::word_break_iter;
use euclid::{point2, size2, Length, Point2D, SideOffsets2D, Size2D};
use font_kit::family_name::FamilyName;
use font_kit::properties::Properties;
use font_kit::source::SystemSource;
use moxie::embed::Runtime;
use moxie::*;
use skribo::{FontCollection, FontFamily, LayoutSession, TextStyle};
use std::ptr;
use std::rc::Rc;

pub struct LogicalPixel;
pub type LogicalPoint = Point2D<f32, LogicalPixel>;
pub type LogicalSize = Size2D<f32, LogicalPixel>;
pub type LogicalLength = Length<f32, LogicalPixel>;
pub type LogicalSideOffsets = SideOffsets2D<f32, LogicalPixel>;

/// Each edge of the layout tree contains information on the positions
/// of the child elements, since elements are positioned relative to
/// their parents, and the position is assigned by the parent.
pub struct LayoutChild {
    /// Child index of the DOM node this child is associated with.
    pub index: usize,
    pub position: LogicalPoint,
    pub layout: Rc<LayoutTreeNode>,
}

/// Information passed to the renderer for rendering text.
pub struct LayoutText {
    /// A piece of the text. This corresponds to roughly one line of text, but not always.
    pub text: String,
    /// The text size of the text.
    pub size: f32,
}

/// One node in the layout tree, which corresponds n:1 with DOM nodes.
pub struct LayoutTreeNode {
    /// The computed size of the node.
    pub size: LogicalSize,
    pub margin: LogicalSideOffsets,
    pub render_text: Option<LayoutText>,
    pub children: Vec<LayoutChild>,
}

#[derive(Clone)]
struct TextLayoutInfo {
    text: String,
    size: f32,
    max_width: f32,
}

impl TextLayoutInfo {
    fn advance_past_whitespace(&self, offset: usize) -> usize {
        let string = self.text[offset..].trim_start();
        string.as_ptr() as usize - self.text.as_ptr() as usize
    }

    #[topo::from_env(collection: &Rc<FontCollection>)]
    fn fill_line(&self, width: f32, offset: usize) -> (usize, f32, f32, f32) {
        let mut session =
            LayoutSession::create(&self.text, &TextStyle { size: self.size }, collection);

        let mut x = 0.0;
        let mut height = 0.0f32;
        let mut ascender = 0.0f32;
        let mut last_word_end = 0;
        let mut last_word_x = 0.0;
        let mut last_word_height = 0.0;
        let mut last_word_ascender = 0.0;
        for word in word_break_iter::WordBreakIterator::new(&self.text[offset..]) {
            let start = word.as_ptr() as usize - self.text.as_ptr() as usize;
            let end = start + word.len();
            for run in session.iter_substr(start..end) {
                let font = run.font();
                let metrics = font.font.metrics();
                let units_per_px = metrics.units_per_em as f32 / self.size;
                let line_height = (metrics.ascent - metrics.descent) / units_per_px;
                let line_ascent = metrics.ascent / units_per_px;
                for glyph in run.glyphs() {
                    let new_x = glyph.offset.x
                        + font.font.advance(glyph.glyph_id).unwrap().x / units_per_px;
                    if last_word_x + new_x > width {
                        return (
                            last_word_end,
                            last_word_x,
                            last_word_height,
                            last_word_ascender,
                        );
                    }
                    x = last_word_x + new_x;
                    height = height.max(line_height);
                    ascender = ascender.max(line_ascent);
                }
            }
            last_word_end = end - offset;
            last_word_x = x;
            last_word_height = height;
            last_word_ascender = ascender;
        }

        (
            last_word_end,
            last_word_x,
            last_word_height,
            last_word_ascender,
        )
    }
}

struct BlockLayoutInputs {
    values: BlockValues,
    children: Vec<Rc<LayoutTreeNode>>,
}

impl PartialEq for BlockLayoutInputs {
    fn eq(&self, other: &BlockLayoutInputs) -> bool {
        if self.values != other.values {
            return false;
        }
        if self.children.len() != other.children.len() {
            return false;
        }
        for (a, b) in self.children.iter().zip(other.children.iter()) {
            if !ptr::eq(a, b) {
                return false;
            }
        }
        true
    }
}

enum InlineLayoutItem {
    Block {
        index: usize,
        layout: Rc<LayoutTreeNode>,
    },
    Text {
        index: usize,
        text: TextLayoutInfo,
    },
}

/// Used to build the layout tree, with internal caching for
/// performance.
pub struct LayoutEngine {
    runtime: Runtime<fn() -> Rc<LayoutTreeNode>, Rc<LayoutTreeNode>>,
}

impl LayoutEngine {
    pub fn new() -> LayoutEngine {
        LayoutEngine {
            runtime: Runtime::new(LayoutEngine::run_layout),
        }
    }

    fn collect_inline_items(
        node: &dyn AnyNodeData,
        parent_values: &ComputedValues,
        max_size: LogicalSize,
        items: &mut Vec<InlineLayoutItem>,
    ) {
        for (index, child) in node.children().enumerate() {
            match child {
                DynamicNode::Node(node) => {
                    let values = node.computed_values().get().unwrap();
                    match values.display {
                        DisplayType::Block(ref block) => {
                            let layout = Self::layout_block(node, &values, block, max_size);
                            items.push(InlineLayoutItem::Block { index, layout });
                        }
                        DisplayType::Inline(_) => {
                            Self::collect_inline_items(node, &values, max_size, items);
                        }
                    }
                }
                DynamicNode::Text(text) => items.push(InlineLayoutItem::Text {
                    text: TextLayoutInfo {
                        text: text.to_owned(),
                        size: parent_values.text_size.get(),
                        max_width: max_size.width,
                    },
                    index,
                }),
            }
        }
    }

    fn layout_inline(
        node: &dyn AnyNodeData,
        values: &ComputedValues,
        max_size: LogicalSize,
    ) -> Rc<LayoutTreeNode> {
        let mut items = vec![];

        Self::collect_inline_items(node, values, max_size, &mut items);

        struct LineItem {
            ascender: f32,
            index: usize,
            x: f32,
            layout: Rc<LayoutTreeNode>,
        }

        struct LineState {
            children: Vec<LayoutChild>,
            max_width: f32,
            x: f32,
            height: f32,
            line_height: f32,
            line_ascender: f32,
            longest_line: f32,
            line_items: Vec<LineItem>,
        }

        impl LineState {
            fn carriage_return(&mut self) {
                for LineItem {
                    ascender,
                    index,
                    x,
                    layout,
                } in self.line_items.drain(..)
                {
                    self.children.push(LayoutChild {
                        position: point2(x, self.height + self.line_ascender - ascender),
                        index,
                        layout,
                    });
                }

                self.height += self.line_height;
                self.longest_line = self.longest_line.max(self.x);
                self.x = 0.0;
                self.line_height = 0.0;
                self.line_ascender = 0.0;
            }

            fn insert_block_item(&mut self, index: usize, layout: Rc<LayoutTreeNode>) {
                let size = layout.size;
                if self.x + size.width > self.max_width {
                    self.carriage_return();
                }
                self.line_items.push(LineItem {
                    x: self.x,
                    ascender: size.height,
                    index,
                    layout,
                });
                self.x += size.width;
                self.line_height = self.line_height.max(size.height);
            }

            fn insert_inline_item(&mut self, index: usize, text: TextLayoutInfo) {
                let mut offset = 0;
                while offset < text.text.len() {
                    let remaining = self.max_width - self.x;
                    let (end, mut width, mut this_line_height, mut ascender) =
                        text.fill_line(remaining, offset);
                    let mut start = offset;
                    offset += end;
                    if end == 0 {
                        self.carriage_return();
                        offset = text.advance_past_whitespace(offset);
                        start = offset;
                        let (end, new_width, new_line_height, new_ascender) =
                            text.fill_line(self.max_width, offset);
                        width = new_width;
                        this_line_height = new_line_height;
                        ascender = new_ascender;
                        offset += end;
                        if end == 0 {
                            // overflow
                            let (end, new_width, new_line_height, new_ascender) =
                                text.fill_line(99999999.0, offset);
                            offset += end;
                            width = new_width;
                            this_line_height = new_line_height;
                            ascender = new_ascender;
                        }
                    }

                    self.line_items.push(LineItem {
                        index,
                        ascender,
                        x: self.x,
                        layout: Rc::new(LayoutTreeNode {
                            render_text: Some(LayoutText {
                                text: text.text[start..offset].to_owned(),
                                size: text.size,
                            }),
                            size: size2(width, this_line_height),
                            margin: LogicalSideOffsets::default(),
                            children: vec![],
                        }),
                    });
                    self.x += width;
                    self.line_height = self.line_height.max(this_line_height);
                    self.line_ascender = self.line_ascender.max(ascender);
                }
            }
        }

        let mut state = LineState {
            children: vec![],
            max_width: max_size.width,
            x: 0.0f32,
            height: 0.0f32,
            line_height: 0.0f32,
            line_ascender: 0.0f32,
            longest_line: 0.0f32,
            line_items: vec![],
        };

        for item in items {
            match item {
                InlineLayoutItem::Block { index, layout } => state.insert_block_item(index, layout),
                InlineLayoutItem::Text { index, text } => state.insert_inline_item(index, text),
            }
        }
        state.carriage_return();
        let size = size2(state.longest_line, state.height);
        let children = state.children;

        Rc::new(LayoutTreeNode {
            render_text: None,
            margin: LogicalSideOffsets::default(),
            size,
            children,
        })
    }

    fn calc_max_size(values: &BlockValues, parent_size: LogicalSize) -> LogicalSize {
        let mut outer = parent_size;
        if let Some(width) = values.width {
            outer.width = width.get();
        }
        if let Some(height) = values.height {
            outer.height = height.get();
        }
        outer - size2(values.padding.horizontal(), values.padding.vertical())
    }

    fn calc_block_layout(input: &BlockLayoutInputs) -> Rc<LayoutTreeNode> {
        let values = &input.values;
        let children = &input.children;

        let mut width = 0.0f32;
        let mut height = 0.0f32;
        let mut child_positions = vec![];
        for (index, child) in children.iter().enumerate() {
            let child = child.clone();
            let size = child.size + size2(child.margin.horizontal(), child.margin.vertical());
            if values.direction == Direction::Vertical {
                width = width.max(size.width);
                child_positions.push(LayoutChild {
                    index,
                    position: point2(values.padding.left, height + values.padding.top),
                    layout: child,
                });
                height += size.height;
            } else {
                height = height.max(size.height);
                child_positions.push(LayoutChild {
                    index,
                    position: point2(width + values.padding.left, values.padding.top),
                    layout: child,
                });
                width += size.width;
            }
        }

        let mut size =
            size2(width, height) + size2(values.padding.horizontal(), values.padding.vertical());

        if let Some(width) = values.width {
            size.width = width.get();
        }
        if let Some(height) = values.height {
            size.height = height.get();
        }

        let margin = values.margin;

        Rc::new(LayoutTreeNode {
            size,
            margin,
            children: child_positions,
            render_text: None,
        })
    }

    fn layout_block(
        node: &dyn AnyNodeData,
        values: &ComputedValues,
        block_values: &BlockValues,
        parent_max_size: LogicalSize,
    ) -> Rc<LayoutTreeNode> {
        topo::call! {
            {
                let max_size = Self::calc_max_size(block_values, parent_max_size);

                let mut children = vec![];
                for child in node.children() {
                    match child {
                        DynamicNode::Node(node) => {
                            let values = node.computed_values().get().unwrap();
                            match values.display {
                                DisplayType::Block(ref block) => {
                                    children.push(Self::layout_block(node, &values, block, max_size));
                                }
                                DisplayType::Inline(_) => {
                                    children.push(Self::layout_inline(node, &values, max_size));
                                }
                            }
                        }
                        DynamicNode::Text(text) => {
                            let text = TextLayoutInfo {
                                text: text.to_owned(),
                                size: values.text_size.get(),
                                max_width: max_size.width,
                            };
                            let (_, width, height, _) = text.fill_line(999999.0, 0);
                            children.push(Rc::new(LayoutTreeNode {
                                size: size2(width, height),
                                margin: LogicalSideOffsets::default(),
                                render_text: Some(LayoutText {
                                    text: text.text,
                                    size: text.size,
                                }),
                                children: vec![],
                            }))
                        }
                    }
                }

                moxie::memo!(
                    BlockLayoutInputs {
                        values: block_values.clone(),
                        children
                    },
                    Self::calc_block_layout
                )
            }
        }
    }

    #[topo::from_env(node: &Node<Window>, size: &LogicalSize)]
    fn run_layout() -> Rc<LayoutTreeNode> {
        let collection = once!(|| {
            let mut collection = FontCollection::new();
            let source = SystemSource::new();
            let font = source
                .select_best_match(&[FamilyName::SansSerif], &Properties::new())
                .unwrap()
                .load()
                .unwrap();
            collection.add_family(FontFamily::new_from_font(font));

            Rc::new(collection)
        });

        topo::call!(
            {
                let values = node.computed_values().get().unwrap();
                match values.display {
                    DisplayType::Block(ref block) => {
                        Self::layout_block(&**node, &values, block, *size)
                    }
                    DisplayType::Inline(_) => Self::layout_inline(&**node, &values, *size),
                }
            },
            env! {
                Rc<FontCollection> => collection,
            }
        )
    }

    /// Perform a layout step based on the new DOM and content size, and
    /// return a fresh layout tree.
    pub fn layout(&mut self, node: Node<Window>, size: LogicalSize) -> Rc<LayoutTreeNode> {
        topo::call!(
            { self.runtime.run_once() },
            env! {
                Node<Window> => node,
                LogicalSize => size,
            }
        )
    }
}
