//! Find and replace across slide text, table cells and speaker notes.
//!
//! Matches stay inside one paragraph. Case folding maps each folded character back to the original,
//! so a letter that lowercases to more than one character (İ → i plus a combining dot) still
//! selects the original text.

use deckcraft_model::edit::{self as ed, Pos};
use deckcraft_model::{Presentation, ShapeId, ShapeKind, Slide, SlideId, TextBody};
use serde_json::{Value, json};

use super::*;
use crate::{Result, Session, Target, TextSel};

const MAX_CHARS: usize = 2000;
const MAX_HITS: usize = 10_000;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "edit.find",
            "Find Text",
            ["Edit"],
            None,
            "{query, matchCase?: bool, origin?: start|after|before} → {found, count, index, slide, wrapped}",
            has_doc,
            find
        ),
        cmd!(
            "edit.replace",
            "Replace Text",
            ["Edit"],
            None,
            "{query, replacement, matchCase?: bool, all?: bool} → {replaced, found, count, index, slide, wrapped}",
            has_doc,
            replace
        ),
    ]
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Shape { id: ShapeId, cell: Option<(usize, usize)> },
    Notes,
}

#[derive(Clone, Copy)]
struct Hit {
    slide: usize,
    slide_id: SlideId,
    place: Place,
    seq: u32,
    para: usize,
    start: usize,
    end: usize,
}

struct Cursor {
    slide: usize,
    seq: u32,
    para: usize,
    ch: usize,
}

enum Origin {
    Start,
    After,
    Before,
}

struct Needle<'a> {
    query: &'a str,
    folded: String,
    match_case: bool,
}

impl<'a> Needle<'a> {
    fn new(query: &'a str, match_case: bool) -> Self {
        let folded = if match_case { String::new() } else { fold(query).0 };
        Needle { query, folded, match_case }
    }
}

fn find(s: &mut Session, p: &Value) -> Result<Value> {
    let (query, match_case) = words(p, "edit.find")?;
    let origin = origin_of(p)?;
    let needle = Needle::new(query, match_case);
    if !match_case && needle.folded.is_empty() {
        return Err(bad("edit.find", "find text is empty"));
    }
    let (hits, cursor) = {
        let st = s.doc()?;
        (collect(&st.doc, &needle), cursor_of(&st.doc, &st.selection))
    };
    let Some((index, wrapped)) = pick(&hits, origin, cursor.as_ref()) else {
        return Ok(miss());
    };
    let Some(hit) = hits.get(index).copied() else { return Ok(miss()) };
    select_hit(s, &hit)?;
    Ok(found_json(&hit, hits.len(), index, wrapped))
}

fn replace(s: &mut Session, p: &Value) -> Result<Value> {
    let (query, match_case) = words(p, "edit.replace")?;
    let replacement = p.get("replacement").and_then(Value::as_str).ok_or_else(|| bad("edit.replace", "`replacement` must be a string"))?;
    if replacement.chars().count() > MAX_CHARS {
        return Err(bad("edit.replace", "replacement is longer than 2000 characters"));
    }
    let needle = Needle::new(query, match_case);
    if !match_case && needle.folded.is_empty() {
        return Err(bad("edit.replace", "find text is empty"));
    }
    if bool_or(p, "all", false) { replace_all(s, &needle, replacement) } else { replace_one(s, &needle, replacement) }
}

fn replace_one(s: &mut Session, needle: &Needle<'_>, replacement: &str) -> Result<Value> {
    let (hits, cursor, matched) = {
        let st = s.doc()?;
        (collect(&st.doc, needle), cursor_of(&st.doc, &st.selection), selection_span(st, needle))
    };
    let (slide, place, para, start, end, slide_id) = if let Some((slide, place, para, start, end)) = matched {
        let slide_id = s.doc()?.doc.slides.get(slide).map(|sl| sl.id).ok_or_else(|| bad("edit.replace", "the slide is gone"))?;
        (slide, place, para, start, end, slide_id)
    } else if let Some((index, _)) = pick(&hits, Origin::After, cursor.as_ref()) {
        let Some(h) = hits.get(index).copied() else { return Ok(replaced_none()) };
        (h.slide, h.place, h.para, h.start, h.end, h.slide_id)
    } else {
        return Ok(replaced_none());
    };
    let end_pos = s.edit(|doc, _| {
        let body = body_at(doc, slide, &place).ok_or_else(|| bad("edit.replace", "the text is gone"))?;
        let at = ed::delete(body, (para, start), (para, end));
        Ok(ed::insert(body, at, replacement, None))
    })?;
    if let Place::Shape { id, cell: None } = place {
        text::fit_text_box(s, id)?;
    }
    let (hits, seq) = {
        let st = s.doc()?;
        let seq = st.doc.slides.get(slide).and_then(|sl| seq_of(sl, &place));
        (collect(&st.doc, needle), seq)
    };
    let cursor = seq.map(|seq| Cursor { slide, seq, para: end_pos.0, ch: end_pos.1 });
    if let Some((index, wrapped)) = pick(&hits, Origin::After, cursor.as_ref())
        && let Some(hit) = hits.get(index).copied()
    {
        select_hit(s, &hit)?;
        return Ok(json!({
            "replaced": 1,
            "found": true,
            "count": hits.len(),
            "index": index + 1,
            "slide": hit.slide,
            "wrapped": wrapped,
        }));
    }
    place_caret(s, slide, slide_id, place, end_pos)?;
    Ok(json!({"replaced": 1, "found": false, "count": 0, "index": 0, "wrapped": false}))
}

fn replace_all(s: &mut Session, needle: &Needle<'_>, replacement: &str) -> Result<Value> {
    let hits = {
        let st = s.doc()?;
        collect(&st.doc, needle)
    };
    if hits.is_empty() {
        return Ok(replaced_none());
    }
    let mut fit = Vec::new();
    for h in &hits {
        if let Place::Shape { id, cell: None } = h.place
            && !fit.contains(&id)
        {
            fit.push(id);
        }
    }
    let n = s.edit(|doc, sel| {
        let mut n = 0usize;
        for h in hits.iter().rev() {
            let Some(body) = body_at(doc, h.slide, &h.place) else { continue };
            let at = ed::delete(body, (h.para, h.start), (h.para, h.end));
            ed::insert(body, at, replacement, None);
            n += 1;
        }
        sel.text = None;
        Ok(n)
    })?;
    for id in fit {
        text::fit_text_box(s, id)?;
    }
    Ok(json!({"replaced": n, "found": false, "count": 0, "index": 0, "wrapped": false}))
}

fn words<'a>(p: &'a Value, cmd: &str) -> Result<(&'a str, bool)> {
    let q = p.get("query").and_then(Value::as_str).ok_or_else(|| bad(cmd, "`query` must be a string"))?;
    if q.is_empty() {
        return Err(bad(cmd, "find text is empty"));
    }
    if q.chars().count() > MAX_CHARS {
        return Err(bad(cmd, "find text is longer than 2000 characters"));
    }
    Ok((q, bool_or(p, "matchCase", false)))
}

fn origin_of(p: &Value) -> Result<Origin> {
    match str_param(p, "origin") {
        None | Some("start") => Ok(Origin::Start),
        Some("after") => Ok(Origin::After),
        Some("before") => Ok(Origin::Before),
        Some(o) => Err(bad("edit.find", format!("origin must be start, after or before, not `{o}`"))),
    }
}

fn miss() -> Value {
    json!({"found": false, "count": 0, "index": 0, "wrapped": false})
}

fn replaced_none() -> Value {
    json!({"replaced": 0, "found": false, "count": 0, "index": 0, "wrapped": false})
}

fn found_json(hit: &Hit, count: usize, index: usize, wrapped: bool) -> Value {
    json!({"found": true, "count": count, "index": index + 1, "slide": hit.slide, "wrapped": wrapped})
}

fn collect(doc: &Presentation, needle: &Needle<'_>) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (si, slide) in doc.slides.iter().enumerate() {
        each_body(slide, |seq, place, body| {
            if hits.len() < MAX_HITS {
                push_body(&mut hits, si, slide.id, seq, place, body, needle);
            }
        });
        if hits.len() >= MAX_HITS {
            break;
        }
    }
    hits
}

fn each_body(slide: &Slide, mut f: impl FnMut(u32, Place, &TextBody)) {
    let mut seq = 0u32;
    deckcraft_model::walk(&slide.shapes, &mut |sh, _| {
        if let ShapeKind::Table(tb) = &sh.kind {
            for r in 0..tb.n_rows() {
                for c in 0..tb.n_cols() {
                    if let Some(cell) = tb.cell(r, c) {
                        f(seq, Place::Shape { id: sh.id, cell: Some((r, c)) }, &cell.text);
                        seq = seq.saturating_add(1);
                    }
                }
            }
        } else if let Some(body) = sh.text.as_ref() {
            f(seq, Place::Shape { id: sh.id, cell: None }, body);
            seq = seq.saturating_add(1);
        }
    });
    f(seq, Place::Notes, &slide.notes);
}

fn push_body(hits: &mut Vec<Hit>, slide: usize, slide_id: SlideId, seq: u32, place: Place, body: &TextBody, needle: &Needle<'_>) {
    for (pi, para) in body.paragraphs.iter().enumerate() {
        if hits.len() >= MAX_HITS {
            return;
        }
        let text = para.text();
        if text.is_empty() {
            continue;
        }
        for (start, end) in spans(&text, needle) {
            hits.push(Hit { slide, slide_id, place, seq, para: pi, start, end });
            if hits.len() >= MAX_HITS {
                return;
            }
        }
    }
}

fn spans(text: &str, needle: &Needle<'_>) -> Vec<(usize, usize)> {
    if needle.match_case {
        return text
            .match_indices(needle.query)
            .map(|(byte, m)| {
                let start = char_at(text, byte);
                (start, start + m.chars().count())
            })
            .collect();
    }
    if needle.folded.is_empty() {
        return Vec::new();
    }
    let (folded, map) = fold(text);
    let qlen = needle.folded.chars().count();
    if qlen == 0 {
        return Vec::new();
    }
    folded
        .match_indices(&needle.folded)
        .filter_map(|(byte, _)| {
            let fs = char_at(&folded, byte);
            let fe = fs + qlen;
            let start = *map.get(fs)?;
            let end = if fe >= map.len() { text.chars().count() } else { *map.get(fe)? };
            (end > start).then_some((start, end))
        })
        .collect()
}

/// Folded text, and for each folded character the original character index it came from.
fn fold(s: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut map = Vec::new();
    for (i, c) in s.chars().enumerate() {
        for f in c.to_lowercase() {
            out.push(f);
            map.push(i);
        }
    }
    (out, map)
}

fn char_at(s: &str, byte: usize) -> usize {
    s.get(..byte).map(|p| p.chars().count()).unwrap_or(0)
}

fn cursor_of(doc: &Presentation, sel: &crate::Selection) -> Option<Cursor> {
    if sel.target != Target::Slides {
        return None;
    }
    let t = sel.text.as_ref()?;
    let slide = doc.slides.get(sel.slide)?;
    let (a, _) = t.ordered();
    let place = if t.notes { Place::Notes } else { Place::Shape { id: t.shape, cell: t.cell } };
    seq_of(slide, &place).map(|seq| Cursor { slide: sel.slide, seq, para: a.0, ch: a.1 })
}

fn seq_of(slide: &Slide, place: &Place) -> Option<u32> {
    let mut found = None;
    each_body(slide, |seq, p, _| {
        if p == *place {
            found = Some(seq);
        }
    });
    found
}

fn selection_span(st: &crate::DocState, needle: &Needle<'_>) -> Option<(usize, Place, usize, usize, usize)> {
    if st.selection.target != Target::Slides {
        return None;
    }
    let t = st.selection.text.as_ref()?;
    let body = text::body_of(st, t)?;
    let (a, b) = t.ordered();
    if a.0 != b.0 {
        return None;
    }
    let text = ed::text_range(body, a, b);
    if !same_text(&text, needle) {
        return None;
    }
    let place = if t.notes { Place::Notes } else { Place::Shape { id: t.shape, cell: t.cell } };
    Some((st.selection.slide, place, a.0, a.1, b.1))
}

fn same_text(text: &str, needle: &Needle<'_>) -> bool {
    if needle.match_case { text == needle.query } else { fold(text).0 == needle.folded }
}

fn pick(hits: &[Hit], origin: Origin, cursor: Option<&Cursor>) -> Option<(usize, bool)> {
    if hits.is_empty() {
        return None;
    }
    match origin {
        Origin::Start => Some((0, false)),
        Origin::After => match cursor {
            Some(c) => {
                if let Some(i) = hits.iter().position(|h| after(h, c)) {
                    Some((i, false))
                } else {
                    Some((0, true))
                }
            }
            None => Some((0, false)),
        },
        Origin::Before => match cursor {
            Some(c) => {
                if let Some(i) = hits.iter().rposition(|h| before(h, c)) {
                    Some((i, false))
                } else {
                    Some((hits.len() - 1, true))
                }
            }
            None => Some((hits.len() - 1, true)),
        },
    }
}

fn after(h: &Hit, c: &Cursor) -> bool {
    (h.slide, h.seq, h.para, h.start) > (c.slide, c.seq, c.para, c.ch)
}

fn before(h: &Hit, c: &Cursor) -> bool {
    (h.slide, h.seq, h.para, h.start) < (c.slide, c.seq, c.para, c.ch)
}

fn select_hit(s: &mut Session, h: &Hit) -> Result<()> {
    let hit = *h;
    s.select(|_, sel| apply_range(sel, hit.slide, hit.slide_id, hit.place, (hit.para, hit.start), (hit.para, hit.end)))
}

fn place_caret(s: &mut Session, slide: usize, slide_id: SlideId, place: Place, pos: Pos) -> Result<()> {
    s.select(|_, sel| apply_range(sel, slide, slide_id, place, pos, pos))
}

fn apply_range(sel: &mut crate::Selection, slide: usize, slide_id: SlideId, place: Place, anchor: Pos, caret: Pos) {
    sel.target = Target::Slides;
    sel.slide = slide;
    sel.slides = vec![slide_id];
    match place {
        Place::Notes => {
            sel.shapes.clear();
            sel.cells = None;
            sel.text = Some(TextSel { notes: true, anchor, caret, ..Default::default() });
        }
        Place::Shape { id, cell } => {
            sel.shapes = vec![id];
            sel.cells = cell.map(|rc| (id, rc, rc));
            sel.text = Some(TextSel { shape: id, anchor, caret, cell, notes: false, line_end: None });
        }
    }
}

fn body_at<'a>(doc: &'a mut Presentation, slide: usize, place: &Place) -> Option<&'a mut TextBody> {
    let sl = std::sync::Arc::make_mut(doc.slides.get_mut(slide)?);
    match place {
        Place::Notes => Some(&mut sl.notes),
        Place::Shape { id, cell: None } => deckcraft_model::find_shape_mut(&mut sl.shapes, *id).and_then(|sh| sh.text.as_mut()),
        Place::Shape { id, cell: Some((r, c)) } => {
            let sh = deckcraft_model::find_shape_mut(&mut sl.shapes, *id)?;
            match &mut sh.kind {
                ShapeKind::Table(tb) => tb.cell_mut(*r, *c).map(|cell| &mut cell.text),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use serde_json::json;

    fn deck() -> Session {
        let mut s = Session::with_new();
        s.execute("shape.insert", &json!({"rect": [40.0, 40.0, 420.0, 60.0], "text": "Aurora over the bay"})).unwrap();
        s.execute("slide.notes", &json!({"text": "Mention the aurora once"})).unwrap();
        s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
        s.execute("shape.insert", &json!({"rect": [40.0, 40.0, 420.0, 60.0], "text": "The aurora returns"})).unwrap();
        s.execute("slide.go", &json!({"index": 1})).unwrap();
        s
    }

    fn selected(s: &Session) -> String {
        let st = s.doc().unwrap();
        let t = st.selection.text.as_ref().unwrap();
        let body = text::body_of(st, t).unwrap();
        let (a, b) = t.ordered();
        deckcraft_model::edit::text_range(body, a, b)
    }

    #[test]
    fn find_walks_matches_wraps_and_respects_case() {
        let mut s = deck();
        let undo = s.doc().unwrap().history.undo.len();
        let journal = s.journal.len();
        let v = s.execute("edit.find", &json!({"query": "aurora"})).unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(v["count"], 3);
        assert_eq!(v["index"], 1);
        assert_eq!(v["slide"], 0);
        assert_eq!(v["wrapped"], false);
        assert_eq!(selected(&s), "Aurora");
        assert!(!s.doc().unwrap().selection.text.as_ref().unwrap().notes);
        assert_eq!(s.doc().unwrap().history.undo.len(), undo);
        assert_eq!(s.journal.len(), journal);

        let v = s.execute("edit.find", &json!({"query": "aurora", "origin": "after"})).unwrap();
        assert_eq!(v["index"], 2);
        assert_eq!(v["slide"], 0);
        assert!(s.doc().unwrap().selection.text.as_ref().unwrap().notes);
        assert_eq!(selected(&s), "aurora");

        let v = s.execute("edit.find", &json!({"query": "aurora", "origin": "after"})).unwrap();
        assert_eq!(v["index"], 3);
        assert_eq!(v["slide"], 1);
        assert_eq!(selected(&s), "aurora");

        let v = s.execute("edit.find", &json!({"query": "aurora", "origin": "after"})).unwrap();
        assert_eq!(v["wrapped"], true);
        assert_eq!(v["index"], 1);
        assert_eq!(v["slide"], 0);

        let v = s.execute("edit.find", &json!({"query": "aurora", "origin": "before"})).unwrap();
        assert_eq!(v["wrapped"], true);
        assert_eq!(v["index"], 3);

        let v = s.execute("edit.find", &json!({"query": "aurora", "matchCase": true})).unwrap();
        assert_eq!(v["count"], 2);
        assert!(s.doc().unwrap().selection.text.as_ref().unwrap().notes);

        let before = s.doc().unwrap().selection.clone();
        let v = s.execute("edit.find", &json!({"query": "AURORA", "matchCase": true})).unwrap();
        assert_eq!(v["found"], false);
        assert_eq!(v["count"], 0);
        assert_eq!(s.doc().unwrap().selection, before);
    }

    #[test]
    fn find_rejects_empty_and_huge_queries() {
        let mut s = deck();
        let e = s.execute("edit.find", &json!({"query": ""})).unwrap_err();
        assert!(e.to_string().contains("empty"), "{e}");
        let e = s.execute("edit.find", &json!({"query": 4})).unwrap_err();
        assert!(e.to_string().contains("string"), "{e}");
        let e = s.execute("edit.find", &json!({"query": "a".repeat(2001)})).unwrap_err();
        assert!(e.to_string().contains("2000"), "{e}");
        let e = s.execute("edit.find", &json!({"query": "a", "origin": "sideways"})).unwrap_err();
        assert!(e.to_string().contains("origin"), "{e}");
    }

    #[test]
    fn find_matches_a_cell_a_group_and_a_letter_that_grows_when_lowercased() {
        let mut s = Session::with_new();
        s.execute("shape.insert", &json!({"rect": [40.0, 40.0, 300.0, 80.0], "text": "hello\nworld"})).unwrap();
        let v = s.execute("edit.find", &json!({"query": "o\nw"})).unwrap();
        assert_eq!(v["found"], false);
        let v = s.execute("edit.find", &json!({"query": "world"})).unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(selected(&s), "world");

        s.execute("insert.table", &json!({"rows": 1, "cols": 2, "rect": [40.0, 200.0, 300.0, 40.0], "data": [["no", "cellword"]]})).unwrap();
        let v = s.execute("edit.find", &json!({"query": "cellword"})).unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(s.doc().unwrap().selection.text.as_ref().unwrap().cell, Some((0, 1)));
        assert_eq!(selected(&s), "cellword");

        let a = s.execute("shape.insert", &json!({"rect": [40.0, 280.0, 120.0, 40.0], "text": "inside-group"})).unwrap()["id"].as_u64().unwrap();
        let b = s.execute("shape.insert", &json!({"rect": [180.0, 280.0, 120.0, 40.0], "text": "other"})).unwrap()["id"].as_u64().unwrap();
        s.execute("edit.select", &json!({"ids": [a, b]})).unwrap();
        s.execute("arrange.group", &json!({})).unwrap();
        let v = s.execute("edit.find", &json!({"query": "inside-group"})).unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(selected(&s), "inside-group");

        // İ lowercases to two characters; the selection is still that one letter.
        s.execute("shape.insert", &json!({"rect": [40.0, 360.0, 200.0, 40.0], "text": "İ"})).unwrap();
        let folded: String = 'İ'.to_lowercase().collect();
        let v = s.execute("edit.find", &json!({"query": folded})).unwrap();
        assert_eq!(v["found"], true);
        assert_eq!(selected(&s), "İ");
    }

    #[test]
    fn replace_one_then_all_and_undo() {
        let mut s = Session::with_new();
        let id = s.execute("shape.insert", &json!({"rect": [40.0, 40.0, 400.0, 60.0], "text": "cat and cat"})).unwrap()["id"].as_u64().unwrap();
        s.execute("slide.new", &json!({"layout": "blank"})).unwrap();
        s.execute("shape.insert", &json!({"rect": [40.0, 40.0, 300.0, 40.0], "text": "cat"})).unwrap();
        s.execute("slide.go", &json!({"index": 1})).unwrap();
        let undo = s.doc().unwrap().history.undo.len();

        let v = s.execute("edit.replace", &json!({"query": "cat", "replacement": "dog"})).unwrap();
        assert_eq!(v["replaced"], 1);
        assert_eq!(v["found"], true);
        assert_eq!(s.doc().unwrap().selection.slide, 0);
        assert_eq!(selected(&s), "cat");
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "dog and cat");
        assert_eq!(s.doc().unwrap().history.undo.len(), undo + 1);
        s.execute("edit.undo", &json!({})).unwrap();
        s.execute("slide.go", &json!({"index": 0})).unwrap();
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "cat and cat");

        s.execute("edit.find", &json!({"query": "cat"})).unwrap();
        assert_eq!(selected(&s), "cat");
        let v = s.execute("edit.replace", &json!({"query": "cat", "replacement": "dog"})).unwrap();
        assert_eq!(v["replaced"], 1);
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "dog and cat");

        let undo = s.doc().unwrap().history.undo.len();
        let v = s.execute("edit.replace", &json!({"query": "cat", "replacement": "lynx", "all": true})).unwrap();
        assert_eq!(v["replaced"], 2);
        assert_eq!(s.doc().unwrap().history.undo.len(), undo + 1);
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "dog and lynx");
        s.execute("slide.go", &json!({"index": 1})).unwrap();
        let second = s.doc().unwrap().current_slide().unwrap().shapes.iter().find_map(|sh| sh.text.as_ref()).unwrap().text();
        assert_eq!(second, "lynx");
        s.execute("edit.undo", &json!({})).unwrap();
        s.execute("slide.go", &json!({"index": 0})).unwrap();
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "dog and cat");

        s.execute("edit.find", &json!({"query": "dog"})).unwrap();
        let v = s.execute("edit.replace", &json!({"query": "dog", "replacement": "north\nlights"})).unwrap();
        assert_eq!(v["replaced"], 1);
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "north\nlights and cat");
        s.execute("edit.undo", &json!({})).unwrap();
        let first = s.execute("text.get", &json!({"id": id})).unwrap();
        assert_eq!(first["text"], "dog and cat");
    }
}
