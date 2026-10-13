//! The find bar: Cmd+F and Edit ▸ Find… search slide text, table cells and notes.

use serde_json::{Value, json};

use crate::SlideApp;
use crate::theme::Tokens;

#[derive(Clone, Debug)]
pub struct FindBar {
    pub query: String,
    pub replacement: String,
    pub match_case: bool,
    pub status: String,
    /// Query and case last sent to the engine, so typing searches once per change.
    searched: String,
    searched_case: bool,
    focus_find: bool,
}

impl Default for FindBar {
    fn default() -> Self {
        FindBar {
            query: String::new(),
            replacement: String::new(),
            match_case: false,
            status: "Type to find".into(),
            searched: String::new(),
            searched_case: false,
            focus_find: true,
        }
    }
}

enum Act {
    Next,
    Prev,
    Replace,
    ReplaceAll,
    Close,
}

/// Open the bar. A non-empty `query` selects the first match immediately.
pub fn open(app: &mut SlideApp, p: &Value) -> Result<Value, String> {
    if p.get("on").and_then(Value::as_bool) == Some(false) {
        app.find = None;
        return Ok(json!({"open": false}));
    }
    app.palette = None;
    let mut bar = app.find.take().unwrap_or_default();
    if let Some(q) = p.get("query").and_then(Value::as_str) {
        bar.query = q.to_string();
        bar.searched.clear();
    }
    if let Some(c) = p.get("matchCase").and_then(Value::as_bool) {
        bar.match_case = c;
        bar.searched.clear();
    }
    bar.focus_find = true;
    let pending = !bar.query.is_empty() && (bar.searched != bar.query || bar.searched_case != bar.match_case);
    app.find = Some(bar);
    if pending {
        search(app, "start");
    }
    Ok(json!({"open": true}))
}

pub fn bar(app: &mut SlideApp, ctx: &egui::Context) {
    let Some(mut bar) = app.find.take() else { return };
    let t = Tokens::get(ctx);
    let mut action = None;
    let (esc, enter, shift, prev_key, next_key) = ctx.input(|i| {
        let m = &i.modifiers;
        (
            i.key_pressed(egui::Key::Escape),
            i.key_pressed(egui::Key::Enter),
            m.shift,
            i.key_pressed(egui::Key::G) && (m.command || m.ctrl) && m.shift,
            i.key_pressed(egui::Key::G) && (m.command || m.ctrl) && !m.shift,
        )
    });
    let mut find_focused = false;
    let mut replace_focused = false;
    egui::Window::new("find_bar").title_bar(false).anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 108.0)).resizable(false).collapsible(false).show(
        ctx,
        |ui| {
            ui.set_min_width(560.0);
            ui.horizontal(|ui| {
                ui.add_sized(egui::vec2(72.0, 18.0), egui::Label::new("Find"));
                let resp =
                    ui.add(egui::TextEdit::singleline(&mut bar.query).id(egui::Id::new("find_query")).desired_width(220.0).hint_text("Text to find"));
                if bar.focus_find {
                    resp.request_focus();
                    bar.focus_find = false;
                }
                find_focused = resp.has_focus();
                if ui.button("Previous").clicked() {
                    action = Some(Act::Prev);
                }
                if ui.button("Next").clicked() {
                    action = Some(Act::Next);
                }
                if ui.button("Close").clicked() {
                    action = Some(Act::Close);
                }
            });
            ui.horizontal(|ui| {
                ui.add_sized(egui::vec2(72.0, 18.0), egui::Label::new("Replace"));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut bar.replacement)
                        .id(egui::Id::new("find_replacement"))
                        .desired_width(220.0)
                        .hint_text("Replacement"),
                );
                replace_focused = resp.has_focus();
                if ui.button("Replace").clicked() {
                    action = Some(Act::Replace);
                }
                if ui.button("Replace All").clicked() {
                    action = Some(Act::ReplaceAll);
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut bar.match_case, "Match case");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new(bar.status.clone()).color(t.text_dim));
                });
            });
        },
    );
    if esc {
        return;
    }
    if action.is_none() {
        if prev_key || (enter && shift && (find_focused || replace_focused)) {
            action = Some(Act::Prev);
        } else if next_key || (enter && find_focused) {
            action = Some(Act::Next);
        } else if enter && replace_focused {
            action = Some(Act::Replace);
        }
    }
    let changed = bar.query != bar.searched || bar.match_case != bar.searched_case;
    app.find = Some(bar);
    match action {
        Some(Act::Close) => app.find = None,
        Some(Act::Replace) => replace(app, false),
        Some(Act::ReplaceAll) => replace(app, true),
        Some(Act::Next | Act::Prev) if changed => search(app, "start"),
        Some(Act::Next) => search(app, "after"),
        Some(Act::Prev) => search(app, "before"),
        None if changed => search(app, "start"),
        None => {}
    }
}

fn search(app: &mut SlideApp, origin: &str) {
    let Some(bar) = app.find.as_ref() else { return };
    let query = bar.query.clone();
    let match_case = bar.match_case;
    if query.is_empty() {
        if let Some(bar) = app.find.as_mut() {
            bar.status = "Type to find".into();
            bar.searched.clear();
            bar.searched_case = match_case;
        }
        return;
    }
    let r = app.session.execute("edit.find", &json!({"query": query, "matchCase": match_case, "origin": origin}));
    if let Some(bar) = app.find.as_mut() {
        match &r {
            Ok(v) => bar.status = status_line(v),
            Err(e) => bar.status = e.to_string(),
        }
        bar.searched = query;
        bar.searched_case = match_case;
    }
}

fn replace(app: &mut SlideApp, all: bool) {
    let Some(bar) = app.find.as_ref() else { return };
    let query = bar.query.clone();
    let replacement = bar.replacement.clone();
    let match_case = bar.match_case;
    if query.is_empty() {
        if let Some(bar) = app.find.as_mut() {
            bar.status = "Type to find".into();
            bar.searched.clear();
            bar.searched_case = match_case;
        }
        return;
    }
    let r = app.session.execute("edit.replace", &json!({"query": query, "replacement": replacement, "matchCase": match_case, "all": all}));
    if let Some(bar) = app.find.as_mut() {
        match &r {
            Ok(v) => {
                let n = v.get("replaced").and_then(Value::as_u64).unwrap_or(0);
                bar.status = if all {
                    if n == 0 { "No matches".into() } else { format!("Replaced {n}") }
                } else if n == 0 {
                    "No matches".into()
                } else if v.get("found").and_then(Value::as_bool) == Some(true) {
                    status_line(v)
                } else {
                    "Replaced, no more matches".into()
                };
            }
            Err(e) => bar.status = e.to_string(),
        }
        bar.searched = query;
        bar.searched_case = match_case;
    }
}

fn status_line(v: &Value) -> String {
    if v.get("found").and_then(Value::as_bool) != Some(true) {
        return "No matches".into();
    }
    let count = v.get("count").and_then(Value::as_u64).unwrap_or(0);
    let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
    if v.get("wrapped").and_then(Value::as_bool) == Some(true) { format!("{index} of {count}, wrapped") } else { format!("{index} of {count}") }
}

#[cfg(test)]
mod tests {
    use deckcraft_engine::Session;
    use egui::{Key, Modifiers};
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use serde_json::json;

    use super::*;
    use crate::Services;

    fn harness(app: SlideApp) -> Harness<'static, SlideApp> {
        let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).build_ui_state(
            |ui, app: &mut SlideApp| {
                app.logic(ui.ctx());
                app.ui(ui);
            },
            app,
        );
        h.run_steps(3);
        h
    }

    #[test]
    fn command_f_opens_the_bar_and_escape_closes_it() {
        let mut h = harness(SlideApp::new(Session::with_new(), Services::default()));
        h.key_press_modifiers(Modifiers::COMMAND, Key::F);
        h.run_steps(2);
        assert!(h.state().find.is_some());
        assert!(h.query_all_by_label("Find").next().is_some());
        assert!(h.query_all_by_label("Replace").next().is_some());
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().find.is_none());
    }

    #[test]
    fn find_command_selects_the_match_and_close_dismisses_the_bar() {
        let mut session = Session::with_new();
        session.execute("shape.insert", &json!({"rect": [80.0, 160.0, 400.0, 80.0], "text": "Aurora over the bay"})).unwrap();
        let mut h = harness(SlideApp::new(session, Services::default()));
        h.state_mut().run("app.find", json!({"query": "aurora"})).unwrap();
        h.run_steps(2);
        let st = h.state().session.doc().unwrap();
        let t = st.selection.text.as_ref().unwrap();
        let body = deckcraft_engine::cmd::text::body_of(st, t).unwrap();
        let (a, b) = t.ordered();
        assert_eq!(deckcraft_model::edit::text_range(body, a, b), "Aurora");
        assert!(h.query_all_by_label("1 of 1").next().is_some());
        assert!(h.query_all_by_label("Match case").next().is_some());
        h.query_all_by_label("Close").next().unwrap().click();
        h.run_steps(2);
        assert!(h.state().find.is_none());
    }
}
