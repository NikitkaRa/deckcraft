//! Tables (graphic frames with `a:tbl`).

use serde::{Deserialize, Serialize};

use crate::style::{ColorRef, Fill, Line};
use crate::text::{Anchor, TextBody};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Cell {
    pub text: TextBody,
    pub fill: Option<Fill>,
    /// Left, right, top, bottom borders; `None` = from the table style.
    pub borders: [Option<Line>; 4],
    pub diag_down: Option<Line>,
    pub diag_up: Option<Line>,
    /// Margins (l, r, t, b) in points; `None` = defaults (7.2, 7.2, 3.6, 3.6).
    pub margins: Option<[f64; 4]>,
    pub anchor: Option<Anchor>,
    pub grid_span: u32,
    pub row_span: u32,
    /// Covered by a merge to the left / above.
    pub h_merge: bool,
    pub v_merge: bool,
    pub vertical: Option<crate::text::TextDir>,
}

impl Cell {
    pub fn new(text: &str) -> Self {
        Cell { text: TextBody::from_text(text), grid_span: 1, row_span: 1, ..Default::default() }
    }
    pub fn is_covered(&self) -> bool {
        self.h_merge || self.v_merge
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Row {
    pub height: f64,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Table {
    /// Column widths in points.
    pub cols: Vec<f64>,
    pub rows: Vec<Row>,
    /// Table style id (our built-in style names or a PPTX style GUID).
    pub style: String,
    pub first_row: bool,
    pub first_col: bool,
    pub last_row: bool,
    pub last_col: bool,
    pub band_row: bool,
    pub band_col: bool,
    /// `a:tblPr` fill: a fill behind the whole table (over the style's own background).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<Fill>,
}

impl Default for Table {
    fn default() -> Self {
        Table {
            cols: vec![],
            rows: vec![],
            style: "medium2-accent1".into(),
            first_row: true,
            first_col: false,
            last_row: false,
            last_col: false,
            band_row: true,
            band_col: false,
            background: None,
        }
    }
}

/// A merged block: its anchor cell and how many rows and columns it covers.
struct MergeRect {
    r: usize,
    c: usize,
    rows: usize,
    cols: usize,
}

/// How far a stored span actually reaches. Hostile spans stop at the grid.
fn fit_span(span: u32, index: usize, len: usize) -> usize {
    let room = len.saturating_sub(index);
    if room == 0 {
        return 1;
    }
    usize::try_from(span.max(1)).unwrap_or(room).min(room).max(1)
}

fn span_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX).max(1)
}

fn covered_by(rects: &[MergeRect], r: usize, c: usize) -> bool {
    rects.iter().any(|rect| r >= rect.r && c >= rect.c && r < rect.r.saturating_add(rect.rows) && c < rect.c.saturating_add(rect.cols))
}

/// `rows` selects the vertical axis; otherwise the column axis.
fn shift_inserted(rects: &mut [MergeRect], at: usize, rows: bool) {
    for rect in rects.iter_mut() {
        let (origin, span) = if rows { (rect.r, rect.rows) } else { (rect.c, rect.cols) };
        let (origin, span) = if origin >= at {
            (origin.saturating_add(1), span)
        } else if at < origin.saturating_add(span) {
            (origin, span.saturating_add(1))
        } else {
            (origin, span)
        };
        if rows {
            rect.r = origin;
            rect.rows = span;
        } else {
            rect.c = origin;
            rect.cols = span;
        }
    }
}

fn shift_deleted(rects: Vec<MergeRect>, at: usize, rows: bool) -> Vec<MergeRect> {
    let mut kept = Vec::new();
    for mut rect in rects {
        let (origin, span) = if rows { (rect.r, rect.rows) } else { (rect.c, rect.cols) };
        let (origin, span) = if origin == at {
            if span <= 1 {
                continue;
            }
            (origin, span.saturating_sub(1))
        } else if origin > at {
            (origin.saturating_sub(1), span)
        } else if at < origin.saturating_add(span) {
            (origin, span.saturating_sub(1))
        } else {
            (origin, span)
        };
        if rows {
            rect.r = origin;
            rect.rows = span;
        } else {
            rect.c = origin;
            rect.cols = span;
        }
        if rect.rows > 1 || rect.cols > 1 {
            kept.push(rect);
        }
    }
    kept
}

impl Table {
    pub fn new(rows: usize, cols: usize, width: f64, row_height: f64) -> Self {
        let rows = rows.clamp(1, 1000);
        let cols = cols.clamp(1, 1000);
        let cw = if cols > 0 { width / cols as f64 } else { width };
        Table {
            cols: vec![cw; cols],
            rows: (0..rows).map(|_| Row { height: row_height, cells: (0..cols).map(|_| Cell::new("")).collect() }).collect(),
            ..Default::default()
        }
    }
    pub fn n_rows(&self) -> usize {
        self.rows.len()
    }
    pub fn n_cols(&self) -> usize {
        self.cols.len()
    }
    pub fn cell(&self, r: usize, c: usize) -> Option<&Cell> {
        self.rows.get(r).and_then(|row| row.cells.get(c))
    }
    pub fn cell_mut(&mut self, r: usize, c: usize) -> Option<&mut Cell> {
        self.rows.get_mut(r).and_then(|row| row.cells.get_mut(c))
    }
    pub fn width(&self) -> f64 {
        self.cols.iter().sum()
    }
    pub fn height(&self) -> f64 {
        self.rows.iter().map(|r| r.height).sum()
    }
    /// Insert a row at `at` (clamped to the end). A merge the new row cuts through grows to
    /// include it. A merge that starts at or below `at` shifts down, unchanged.
    pub fn insert_row(&mut self, at: usize) {
        let at = at.min(self.rows.len());
        self.normalize();
        let mut rects = self.merge_rects();
        shift_inserted(&mut rects, at, true);
        let h = self.rows.get(at.saturating_sub(1)).map(|r| r.height).unwrap_or(30.0);
        let n = self.cols.len();
        self.rows.insert(at, Row { height: h, cells: (0..n).map(|_| Cell::new("")).collect() });
        self.paint_merges(&rects);
    }
    /// Insert a column at `at` (clamped to the end). A merge the new column cuts through grows
    /// to include it. A merge that starts at or to the right of `at` shifts right, unchanged.
    pub fn insert_col(&mut self, at: usize) {
        let at = at.min(self.cols.len());
        self.normalize();
        let mut rects = self.merge_rects();
        shift_inserted(&mut rects, at, false);
        let w = self.cols.get(at.saturating_sub(1)).copied().unwrap_or(80.0);
        self.cols.insert(at, w);
        for r in &mut self.rows {
            let i = at.min(r.cells.len());
            r.cells.insert(i, Cell::new(""));
        }
        self.paint_merges(&rects);
    }
    /// Delete a row. A merge that contained it shrinks. Deleting a merge's anchor row promotes
    /// the cell below, which keeps the merged text. Returns false when `at` is out of range or
    /// this is the last row.
    pub fn delete_row(&mut self, at: usize) -> bool {
        if self.rows.len() <= 1 || at >= self.rows.len() {
            return false;
        }
        self.normalize();
        let rects = self.merge_rects();
        for rect in &rects {
            if rect.r == at && rect.rows > 1 {
                self.promote(rect.r, rect.c, rect.r.saturating_add(1), rect.c);
            }
        }
        let rects = shift_deleted(rects, at, true);
        self.rows.remove(at);
        self.paint_merges(&rects);
        true
    }
    /// Delete a column. A merge that contained it shrinks. Deleting a merge's anchor column
    /// promotes the cell to its right, which keeps the merged text. Returns false when `at` is
    /// out of range or this is the last column.
    pub fn delete_col(&mut self, at: usize) -> bool {
        if self.cols.len() <= 1 || at >= self.cols.len() {
            return false;
        }
        self.normalize();
        let rects = self.merge_rects();
        for rect in &rects {
            if rect.c == at && rect.cols > 1 {
                self.promote(rect.r, rect.c, rect.r, rect.c.saturating_add(1));
            }
        }
        let rects = shift_deleted(rects, at, false);
        self.cols.remove(at);
        for r in &mut self.rows {
            if at < r.cells.len() {
                r.cells.remove(at);
            }
        }
        self.paint_merges(&rects);
        true
    }
    /// Merged rectangles, from anchors that are not themselves covered. A cell already inside an
    /// earlier rectangle is not a second anchor.
    fn merge_rects(&self) -> Vec<MergeRect> {
        let mut out = Vec::new();
        let n_rows = self.rows.len();
        let n_cols = self.cols.len();
        for (r, row) in self.rows.iter().enumerate() {
            for (c, cell) in row.cells.iter().enumerate().take(n_cols) {
                if cell.is_covered() || covered_by(&out, r, c) {
                    continue;
                }
                let rows = fit_span(cell.row_span, r, n_rows);
                let cols = fit_span(cell.grid_span, c, n_cols);
                if rows > 1 || cols > 1 {
                    out.push(MergeRect { r, c, rows, cols });
                }
            }
        }
        out
    }
    /// Move a merge's anchor onto the cell that survives when the anchor's row or column is
    /// deleted. The anchor's text wins; an empty anchor keeps text the survivor already had.
    fn promote(&mut self, from_r: usize, from_c: usize, to_r: usize, to_c: usize) {
        let Some(mut src) = self.cell(from_r, from_c).cloned() else { return };
        if src.text.is_empty()
            && let Some(kept) = self.cell(to_r, to_c).filter(|cell| !cell.text.is_empty()).map(|cell| cell.text.clone())
        {
            src.text = kept;
        }
        if let Some(dst) = self.cell_mut(to_r, to_c) {
            *dst = src;
        }
    }
    /// Clear merge flags, then mark each rectangle the way [`Table::merge`] does: the anchor
    /// carries the spans, and every other cell is covered from the left and/or from above.
    fn paint_merges(&mut self, rects: &[MergeRect]) {
        for row in &mut self.rows {
            for cell in &mut row.cells {
                cell.h_merge = false;
                cell.v_merge = false;
                cell.grid_span = 1;
                cell.row_span = 1;
            }
        }
        for rect in rects {
            for dr in 0..rect.rows {
                for dc in 0..rect.cols {
                    let (Some(r), Some(c)) = (rect.r.checked_add(dr), rect.c.checked_add(dc)) else { continue };
                    let Some(cell) = self.cell_mut(r, c) else { continue };
                    if dr == 0 && dc == 0 {
                        cell.grid_span = span_u32(rect.cols);
                        cell.row_span = span_u32(rect.rows);
                        cell.h_merge = false;
                        cell.v_merge = false;
                    } else {
                        cell.grid_span = 1;
                        cell.row_span = 1;
                        cell.h_merge = dc > 0;
                        cell.v_merge = dr > 0;
                    }
                }
            }
        }
    }
    /// Merge the rectangle (r0,c0)–(r1,c1) inclusive into its top-left cell.
    pub fn merge(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) -> bool {
        let (r0, r1) = (r0.min(r1), r0.max(r1));
        let (c0, c1) = (c0.min(c1), c0.max(c1));
        if r1 >= self.rows.len() || c1 >= self.cols.len() || (r0 == r1 && c0 == c1) {
            return false;
        }
        let mut text = vec![];
        for r in r0..=r1 {
            for c in c0..=c1 {
                if let Some(cell) = self.cell_mut(r, c) {
                    if !cell.text.is_empty() {
                        text.append(&mut cell.text.paragraphs);
                    }
                    cell.h_merge = c > c0;
                    cell.v_merge = r > r0;
                    cell.grid_span = 1;
                    cell.row_span = 1;
                }
            }
        }
        if let Some(cell) = self.cell_mut(r0, c0) {
            cell.grid_span = (c1 - c0 + 1) as u32;
            cell.row_span = (r1 - r0 + 1) as u32;
            cell.h_merge = false;
            cell.v_merge = false;
            if !text.is_empty() {
                cell.text.paragraphs = text;
            }
        }
        true
    }
    /// Undo a merge at (r, c).
    pub fn split(&mut self, r: usize, c: usize) -> bool {
        let Some(cell) = self.cell(r, c) else { return false };
        let (gs, rs) = (cell.grid_span.max(1) as usize, cell.row_span.max(1) as usize);
        if gs == 1 && rs == 1 {
            return false;
        }
        for rr in r..r + rs {
            for cc in c..c + gs {
                if let Some(x) = self.cell_mut(rr, cc) {
                    x.h_merge = false;
                    x.v_merge = false;
                    x.grid_span = 1;
                    x.row_span = 1;
                }
            }
        }
        true
    }
    /// Make every row have exactly `cols.len()` cells (repair after loading).
    pub fn normalize(&mut self) {
        let n = self.cols.len();
        for r in &mut self.rows {
            r.cells.resize_with(n, || Cell::new(""));
            for c in &mut r.cells {
                c.grid_span = c.grid_span.max(1);
                c.row_span = c.row_span.max(1);
            }
        }
    }
}

/// A table style defined in the file (`ppt/tableStyles.xml`), as PowerPoint writes the built-in
/// styles a deck uses: how each part of a table (whole table, banded rows and columns, first and
/// last rows and columns, corner cells) is filled, bordered and lettered.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TableStyleDef {
    /// The style's GUID (`a:tableStyleId` on tables).
    pub id: String,
    pub name: String,
    pub parts: Vec<(TablePart, TablePartStyle)>,
    /// `a:tblBg`: a fill behind the whole table (only `fill` and `fill_ref` are used).
    pub background: Option<TablePartStyle>,
    /// The definition as read, written back unchanged.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub raw: String,
}

impl TableStyleDef {
    pub fn part(&self, p: TablePart) -> Option<&TablePartStyle> {
        self.parts.iter().find(|(k, _)| *k == p).map(|(_, s)| s)
    }
}

/// The parts of a table style, in the order they apply (later ones win).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TablePart {
    WholeTbl,
    Band1H,
    Band2H,
    Band1V,
    Band2V,
    LastCol,
    FirstCol,
    LastRow,
    FirstRow,
    SeCell,
    SwCell,
    NeCell,
    NwCell,
}

impl TablePart {
    pub const ALL: [TablePart; 13] = [
        TablePart::WholeTbl,
        TablePart::Band1H,
        TablePart::Band2H,
        TablePart::Band1V,
        TablePart::Band2V,
        TablePart::LastCol,
        TablePart::FirstCol,
        TablePart::LastRow,
        TablePart::FirstRow,
        TablePart::SeCell,
        TablePart::SwCell,
        TablePart::NeCell,
        TablePart::NwCell,
    ];

    pub fn xml(self) -> &'static str {
        match self {
            TablePart::WholeTbl => "wholeTbl",
            TablePart::Band1H => "band1H",
            TablePart::Band2H => "band2H",
            TablePart::Band1V => "band1V",
            TablePart::Band2V => "band2V",
            TablePart::LastCol => "lastCol",
            TablePart::FirstCol => "firstCol",
            TablePart::LastRow => "lastRow",
            TablePart::FirstRow => "firstRow",
            TablePart::SeCell => "seCell",
            TablePart::SwCell => "swCell",
            TablePart::NeCell => "neCell",
            TablePart::NwCell => "nwCell",
        }
    }

    pub fn from_xml(s: &str) -> Option<TablePart> {
        TablePart::ALL.into_iter().find(|p| p.xml() == s)
    }
}

/// One part of a table style. A theme reference (`a:fillRef`, `a:lnRef`) is kept as the theme's
/// style index with its colour.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TablePartStyle {
    pub fill: Option<Fill>,
    pub fill_ref: Option<(u32, ColorRef)>,
    /// Left, right, top, bottom, inside horizontal, inside vertical.
    pub borders: [Option<TableBorder>; 6],
    pub text_color: Option<ColorRef>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
}

/// A border of a table style part: a line, or a theme line style reference.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum TableBorder {
    Line { line: Line },
    Ref { idx: u32, color: ColorRef },
}

/// Built-in table styles: (id, label). Styles are generated from the theme accents.
pub fn table_styles() -> Vec<(String, String)> {
    let mut v = vec![("none".to_string(), "No Style, No Grid".to_string()), ("grid".to_string(), "No Style, Table Grid".to_string())];
    for (kind, label) in [
        ("light1", "Light Style 1"),
        ("light2", "Light Style 2"),
        ("medium2", "Medium Style 2"),
        ("medium4", "Medium Style 4"),
        ("dark1", "Dark Style 1"),
    ] {
        v.push((format!("{kind}-tx1"), label.to_string()));
        for a in 1..=6 {
            v.push((format!("{kind}-accent{a}"), format!("{label} - Accent {a}")));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_delete_merge_split() {
        let mut t = Table::new(3, 3, 300.0, 30.0);
        assert_eq!((t.n_rows(), t.n_cols()), (3, 3));
        t.insert_row(1);
        t.insert_col(3);
        assert_eq!((t.n_rows(), t.n_cols()), (4, 4));
        assert!(t.delete_row(0) && t.delete_col(0));
        assert_eq!((t.n_rows(), t.n_cols()), (3, 3));
        assert!(t.merge(0, 0, 1, 1));
        assert_eq!(t.cell(0, 0).map(|c| (c.grid_span, c.row_span)), Some((2, 2)));
        assert!(t.cell(1, 1).is_some_and(|c| c.is_covered()));
        assert!(t.split(0, 0));
        assert!(!t.cell(1, 1).is_some_and(|c| c.is_covered()));
        assert!(!t.merge(5, 5, 6, 6));
        assert!(!t.delete_row(99));
        let mut one = Table::new(1, 1, 10.0, 10.0);
        assert!(!one.delete_row(0) && !one.delete_col(0));
        assert!(table_styles().len() > 30);
    }

    /// Every covered cell belongs to exactly one anchor, and that anchor's span is the block.
    fn assert_sound(t: &Table) {
        let mut claimed = std::collections::HashSet::new();
        for (r, row) in t.rows.iter().enumerate() {
            for (c, cell) in row.cells.iter().enumerate().take(t.n_cols()) {
                if cell.is_covered() {
                    continue;
                }
                let rows = cell.row_span.max(1) as usize;
                let cols = cell.grid_span.max(1) as usize;
                assert!(r.saturating_add(rows) <= t.n_rows() && c.saturating_add(cols) <= t.n_cols(), "span leaves the grid at {r},{c}");
                for rr in r..r.saturating_add(rows) {
                    for cc in c..c.saturating_add(cols) {
                        assert!(claimed.insert((rr, cc)), "cell {rr},{cc} is in two merges");
                        let other = t.cell(rr, cc).expect("cell");
                        if rr == r && cc == c {
                            assert!(!other.h_merge && !other.v_merge);
                        } else {
                            assert_eq!(other.h_merge, cc > c, "h_merge at {rr},{cc}");
                            assert_eq!(other.v_merge, rr > r, "v_merge at {rr},{cc}");
                            assert_eq!((other.grid_span, other.row_span), (1, 1));
                        }
                    }
                }
            }
        }
        for (r, row) in t.rows.iter().enumerate() {
            for (c, cell) in row.cells.iter().enumerate().take(t.n_cols()) {
                if cell.is_covered() {
                    assert!(claimed.contains(&(r, c)), "covered cell {r},{c} has no anchor");
                }
            }
        }
    }

    fn head(rows: usize, cols: usize) -> Table {
        let mut t = Table::new(rows, cols, 400.0, 20.0);
        assert!(t.merge(0, 1, 2, 2));
        let anchor = t.cell_mut(0, 1).expect("anchor");
        anchor.text = TextBody::from_text("Head");
        anchor.margins = Some([1.0, 2.0, 3.0, 4.0]);
        t
    }

    #[test]
    fn inserting_through_a_merge_grows_it_and_keeps_the_text() {
        let mut t = head(4, 4);
        t.insert_row(0);
        assert_sound(&t);
        assert_eq!(t.cell(1, 1).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((2, 3, "Head".into())));
        assert!(!t.cell(0, 1).is_some_and(|c| c.is_covered()), "the new row is above the merge");

        let mut t = head(4, 4);
        t.insert_row(1);
        assert_sound(&t);
        let anchor = t.cell(0, 1).expect("anchor");
        assert_eq!((anchor.grid_span, anchor.row_span, anchor.text.text()), (2, 4, "Head".into()));
        assert_eq!(anchor.margins, Some([1.0, 2.0, 3.0, 4.0]));
        assert!(t.cell(1, 1).is_some_and(|c| c.v_merge && !c.h_merge && c.text.is_empty()));
        assert!(t.cell(1, 2).is_some_and(|c| c.v_merge && c.h_merge));
        assert!(!t.cell(4, 1).is_some_and(|c| c.is_covered()), "the row below the merge stays free");

        let mut t = head(4, 4);
        t.insert_row(3);
        assert_sound(&t);
        assert_eq!(t.cell(0, 1).map(|c| (c.grid_span, c.row_span)), Some((2, 3)));

        let mut t = head(4, 4);
        t.insert_col(1);
        assert_sound(&t);
        assert_eq!(t.cell(0, 2).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((2, 3, "Head".into())));

        let mut t = head(4, 4);
        t.insert_col(2);
        assert_sound(&t);
        let anchor = t.cell(0, 1).expect("anchor");
        assert_eq!((anchor.grid_span, anchor.row_span), (3, 3));
        assert!(t.cell(0, 2).is_some_and(|c| c.h_merge && !c.v_merge && c.text.is_empty()));
        assert!(t.cell(2, 2).is_some_and(|c| c.h_merge && c.v_merge));

        let mut t = head(4, 4);
        t.insert_col(3);
        assert_sound(&t);
        assert_eq!(t.cell(0, 1).map(|c| (c.grid_span, c.row_span)), Some((2, 3)));
    }

    #[test]
    fn deleting_a_merged_row_or_column_shrinks_or_promotes_the_anchor() {
        let mut t = head(4, 4);
        assert!(t.delete_row(0));
        assert_sound(&t);
        let anchor = t.cell(0, 1).expect("promoted");
        assert_eq!((anchor.grid_span, anchor.row_span, anchor.text.text()), (2, 2, "Head".into()));
        assert_eq!(anchor.margins, Some([1.0, 2.0, 3.0, 4.0]));
        assert!(t.cell(0, 2).is_some_and(|c| c.h_merge && !c.v_merge));
        assert!(t.cell(1, 1).is_some_and(|c| c.v_merge && !c.h_merge));
        assert!(t.cell(1, 2).is_some_and(|c| c.h_merge && c.v_merge));

        let mut t = head(4, 4);
        assert!(t.delete_row(1));
        assert_sound(&t);
        assert_eq!(t.cell(0, 1).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((2, 2, "Head".into())));
        assert!(t.cell(1, 2).is_some_and(|c| c.h_merge && c.v_merge));
        assert!(!t.cell(2, 1).is_some_and(|c| c.is_covered()));

        let mut t = head(4, 4);
        assert!(t.delete_row(3));
        assert_sound(&t);
        assert_eq!(t.cell(0, 1).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((2, 3, "Head".into())));

        let mut t = head(4, 4);
        assert!(t.delete_col(1));
        assert_sound(&t);
        let anchor = t.cell(0, 1).expect("promoted");
        assert_eq!((anchor.grid_span, anchor.row_span, anchor.text.text()), (1, 3, "Head".into()));
        assert_eq!(anchor.margins, Some([1.0, 2.0, 3.0, 4.0]));
        assert!(t.cell(1, 1).is_some_and(|c| c.v_merge && !c.h_merge));
        assert!(!t.cell(0, 1).is_some_and(|c| c.h_merge));

        let mut t = head(4, 4);
        assert!(t.delete_col(2));
        assert_sound(&t);
        assert_eq!(t.cell(0, 1).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((1, 3, "Head".into())));

        let mut t = head(4, 4);
        assert!(t.delete_col(0));
        assert_sound(&t);
        assert_eq!(t.cell(0, 0).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((2, 3, "Head".into())));
    }

    #[test]
    fn deleting_the_anchor_keeps_visible_text_and_a_one_cell_remainder() {
        let mut t = Table::new(2, 3, 200.0, 20.0);
        assert!(t.merge(0, 0, 0, 2));
        t.cell_mut(0, 0).expect("anchor").text = TextBody::from_text("visible");
        t.cell_mut(0, 1).expect("covered").text = TextBody::from_text("hidden");
        assert!(t.delete_col(0));
        assert_sound(&t);
        assert_eq!(t.cell(0, 0).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((2, 1, "visible".into())));
        assert!(t.delete_col(0));
        assert_sound(&t);
        assert_eq!(t.cell(0, 0).map(|c| (c.grid_span, c.row_span, c.text.text(), c.is_covered())), Some((1, 1, "visible".into(), false)));
        assert!(t.rows.iter().flat_map(|row| &row.cells).all(|c| !c.is_covered()));

        let mut t = Table::new(2, 1, 40.0, 20.0);
        assert!(t.merge(0, 0, 1, 0));
        t.cell_mut(1, 0).expect("covered").text = TextBody::from_text("kept");
        assert!(t.delete_row(0));
        assert_sound(&t);
        assert_eq!(t.cell(0, 0).map(|c| c.text.text()), Some("kept".into()));
    }

    #[test]
    fn a_second_merge_is_left_intact_and_the_edit_round_trips() {
        let mut t = Table::new(4, 4, 400.0, 20.0);
        assert!(t.merge(0, 0, 1, 0));
        assert!(t.merge(2, 2, 3, 3));
        t.cell_mut(0, 0).expect("v").text = TextBody::from_text("V");
        t.cell_mut(2, 2).expect("h").text = TextBody::from_text("H");
        t.insert_row(1);
        assert!(t.delete_col(3));
        assert_sound(&t);
        assert_eq!(t.cell(0, 0).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((1, 3, "V".into())));
        assert_eq!(t.cell(3, 2).map(|c| (c.grid_span, c.row_span, c.text.text())), Some((1, 2, "H".into())));
        let saved = serde_json::to_string(&t).expect("serialize");
        let back: Table = serde_json::from_str(&saved).expect("deserialize");
        assert_eq!(back, t);
        assert_sound(&back);
    }
}
