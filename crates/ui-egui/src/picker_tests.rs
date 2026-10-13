//! Asynchronous file picks (web) land on the document and slide they were started from (#414).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use deckcraft_engine::Session;
use std::sync::{Arc, Mutex};

const SVG: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#;

/// An app whose async picker records `(purpose, ticket)` requests; answers go through the inbox.
fn app() -> (SlideApp, Arc<Mutex<Vec<(String, u64)>>>, Inbox) {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let inbox = Inbox::default();
    let a = asked.clone();
    let services = Services {
        open_async: Some(Box::new(move |purpose: &str, ticket: u64| a.lock().unwrap().push((purpose.to_string(), ticket)))),
        inbox: Some(inbox.clone()),
        ..Default::default()
    };
    let mut app = SlideApp::new(Session::with_new(), services);
    app.run("slide.new", json!({})).unwrap();
    app.run("slide.go", json!({"index": 0})).unwrap();
    (app, asked, inbox)
}

fn answer(app: &mut SlideApp, inbox: &Inbox, name: &str, bytes: &[u8], ticket: u64) {
    inbox.lock().unwrap().push(InboxFile { name: name.into(), bytes: bytes.to_vec(), ticket: Some(ticket) });
    app.drain_inbox();
}

fn pictures(app: &SlideApp, doc: usize, slide: usize) -> usize {
    let d = &app.session.documents()[doc];
    d.doc.slides[slide].shapes.iter().filter(|s| matches!(s.kind, deckcraft_model::ShapeKind::Picture { .. })).count()
}

fn last_ticket(asked: &Arc<Mutex<Vec<(String, u64)>>>) -> u64 {
    asked.lock().unwrap().last().unwrap().1
}

#[test]
fn picture_lands_on_the_slide_it_was_requested_from() {
    let (mut app, asked, inbox) = app();
    app.run("app.insertPictureDialog", json!({})).unwrap();
    assert_eq!(asked.lock().unwrap()[0].0, "picture");
    let t = last_ticket(&asked);
    // The user moves to slide 2 while the browser reads the file.
    app.run("slide.go", json!({"index": 1})).unwrap();
    answer(&mut app, &inbox, "a.svg", SVG, t);
    assert_eq!((pictures(&app, 0, 0), pictures(&app, 0, 1)), (1, 0));
    // The view stays where the user is.
    assert_eq!(app.session.active().unwrap().selection.slide, 1);
}

#[test]
fn picture_lands_in_the_document_it_was_requested_from() {
    let (mut app, asked, inbox) = app();
    app.run("app.insertPictureDialog", json!({})).unwrap();
    let t = last_ticket(&asked);
    app.run("file.new", json!({})).unwrap();
    let second = app.session.active().unwrap().uid;
    answer(&mut app, &inbox, "a.svg", SVG, t);
    assert_eq!(pictures(&app, 0, 0), 1);
    assert_eq!(pictures(&app, 1, 0), 0);
    assert_eq!(app.session.active().unwrap().uid, second);
}

#[test]
fn answers_in_reverse_order_each_reach_their_own_target() {
    let (mut app, asked, inbox) = app();
    app.run("app.insertPictureDialog", json!({})).unwrap();
    let first = last_ticket(&asked);
    app.run("slide.go", json!({"index": 1})).unwrap();
    app.run("app.insertPictureDialog", json!({})).unwrap();
    let second = last_ticket(&asked);
    assert_ne!(first, second);
    answer(&mut app, &inbox, "b.svg", SVG, second);
    answer(&mut app, &inbox, "a.svg", SVG, first);
    assert_eq!((pictures(&app, 0, 0), pictures(&app, 0, 1)), (1, 1));
}

#[test]
fn a_closed_document_or_deleted_slide_cancels_the_insert() {
    let (mut app, asked, inbox) = app();
    app.run("app.insertPictureDialog", json!({})).unwrap();
    let t = last_ticket(&asked);
    app.run("slide.delete", json!({})).unwrap();
    let before: usize = (0..app.session.active().unwrap().doc.slides.len()).map(|i| pictures(&app, 0, i)).sum();
    answer(&mut app, &inbox, "a.svg", SVG, t);
    let after: usize = (0..app.session.active().unwrap().doc.slides.len()).map(|i| pictures(&app, 0, i)).sum();
    assert_eq!(before, after, "nothing lands on another slide");

    app.run("app.insertPictureDialog", json!({})).unwrap();
    let t = last_ticket(&asked);
    app.session.close_document(0);
    app.run("file.new", json!({})).unwrap();
    answer(&mut app, &inbox, "a.svg", SVG, t);
    assert_eq!(pictures(&app, 0, 0), 0, "nothing lands in the presentation opened since");
}

#[test]
fn the_requested_command_is_kept_and_stale_tickets_are_ignored() {
    let (mut app, asked, inbox) = app();
    // An outline-looking file answering a picture request is not turned into slides.
    app.run("app.insertPictureDialog", json!({})).unwrap();
    let t = last_ticket(&asked);
    let slides = app.session.active().unwrap().doc.slides.len();
    answer(&mut app, &inbox, "notes.txt", b"One\nTwo\nThree", t);
    assert_eq!(app.session.active().unwrap().doc.slides.len(), slides);
    // A ticket answered twice, or never issued, does nothing.
    answer(&mut app, &inbox, "a.svg", SVG, t);
    answer(&mut app, &inbox, "a.svg", SVG, 999);
    assert_eq!(pictures(&app, 0, 0), 0);
}
