use super::*;
use pretty_assertions::assert_eq;

#[test]
fn tall_paragraph_stays_visible_in_column_and_inset_layout() {
    let paragraph = Paragraph::new(vec![Line::from("body"); 65_536]);
    let mut column = ColumnRenderable::new();
    column.push(paragraph.inset(Insets::vh(1, 0)));
    column.push("tail");

    assert_eq!(column.desired_height(4), 65_535);
    let area = Rect::new(0, 0, 4, 4);
    let mut buffer = Buffer::empty(area);
    column.render(area, &mut buffer);
    assert_eq!(buffer, Buffer::with_lines(["    ", "body", "body", "    "]));
}

#[test]
fn inset_layout_handles_width_smaller_than_horizontal_padding() {
    let inset = Line::from("body").inset(Insets::vh(1, 2));

    assert_eq!(inset.desired_height(1), 3);
    let area = Rect::new(0, 0, 1, 3);
    let mut buffer = Buffer::empty(area);
    inset.render(area, &mut buffer);
    assert_eq!(buffer, Buffer::with_lines([" ", " ", " "]));
}

struct HeightRenderable(u16);

impl HeightRenderable {
    fn with_height(height: u16) -> Self {
        Self(height)
    }
}

impl Renderable for HeightRenderable {
    fn render(&self, _area: Rect, _buf: &mut Buffer) {}

    fn desired_height(&self, _width: u16) -> u16 {
        self.0
    }
}

#[test]
fn flex_redistributes_space_unused_by_short_children() {
    let mut flex = FlexRenderable::new();
    flex.push(
        /*flex*/ 1,
        RenderableItem::Owned(Box::new(HeightRenderable::with_height(/*height*/ 20))),
    );
    flex.push(
        /*flex*/ 1,
        RenderableItem::Owned(Box::new(HeightRenderable::with_height(/*height*/ 2))),
    );

    let allocated = flex.allocate(Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 10,
    ));

    assert_eq!(
        allocated
            .into_iter()
            .map(|area| area.height)
            .collect::<Vec<_>>(),
        vec![8, 2],
    );
}

#[test]
fn flex_reserves_non_flex_space_before_flexible_children() {
    let mut flex = FlexRenderable::new();
    flex.push(
        /*flex*/ 1,
        RenderableItem::Owned(Box::new(HeightRenderable::with_height(/*height*/ 20))),
    );
    flex.push(
        /*flex*/ 0,
        RenderableItem::Owned(Box::new(HeightRenderable::with_height(/*height*/ 2))),
    );
    flex.push(
        /*flex*/ 1,
        RenderableItem::Owned(Box::new(HeightRenderable::with_height(/*height*/ 20))),
    );

    let allocated = flex.allocate(Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 10,
    ));

    assert_eq!(
        allocated
            .into_iter()
            .map(|area| area.height)
            .collect::<Vec<_>>(),
        vec![4, 2, 4],
    );
}

#[test]
fn flex_preserves_large_weight_proportions_when_rendering() {
    let mut flex = FlexRenderable::new();
    flex.push(
        65_536,
        RenderableItem::Owned(Box::new(Paragraph::new(vec![Line::from("A"); 6]))),
    );
    flex.push(
        32_768,
        RenderableItem::Owned(Box::new(Paragraph::new(vec![Line::from("B"); 6]))),
    );
    let area = Rect::new(0, 0, 1, 6);
    let mut buffer = Buffer::empty(area);
    flex.render(area, &mut buffer);
    assert_eq!(buffer, Buffer::with_lines(["A", "A", "A", "A", "B", "B"]));
}
