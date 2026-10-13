//! Trackpad pinch zooms the editor around the pointer, and the Slide Sorter's thumbnails.

use deckcraft_engine::Session;
use egui::Pos2;
use egui_kittest::Harness;

use crate::{Services, SlideApp, ViewMode};

fn harness() -> Harness<'static, SlideApp> {
    let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        |ui, app: &mut SlideApp| {
            app.logic(ui.ctx());
            app.ui(ui);
        },
        SlideApp::new(Session::with_new(), Services::default()),
    );
    h.run_steps(2);
    h
}

fn slide_local(app: &SlideApp, at: Pos2) -> egui::Vec2 {
    let slide = app.slide_rect.expect("the canvas laid out a slide");
    (at - slide.min) / app.canvas_scale
}

#[test]
fn pinch_zooms_the_slide_around_the_pointer() {
    let mut h = harness();
    h.state_mut().ui.zoom = Some(1.5);
    h.run_steps(2);
    let canvas = h.state().canvas_rect.expect("canvas");
    let slide = h.state().slide_rect.expect("slide");
    let anchor = slide.intersect(canvas).center();
    h.hover_at(anchor);
    h.run_steps(1);
    let before = slide_local(h.state(), anchor);
    let scale = h.state().canvas_scale;
    h.event(egui::Event::Zoom(1.4));
    h.run_steps(1);
    let after_scale = h.state().canvas_scale;
    assert!((after_scale - scale * 1.4).abs() < 0.02, "scale {scale} → {after_scale}");
    let after = slide_local(h.state(), anchor);
    assert!((after.x - before.x).abs() < 1.5 && (after.y - before.y).abs() < 1.5, "slide point {before:?} moved to {after:?}");

    // Another pinch at the ceiling stays there and does not jump the slide.
    h.state_mut().ui.zoom = Some(4.0);
    h.run_steps(1);
    let canvas = h.state().canvas_rect.expect("canvas");
    let slide = h.state().slide_rect.expect("slide");
    let anchor = slide.intersect(canvas).center();
    h.hover_at(anchor);
    h.run_steps(1);
    let stuck = slide_local(h.state(), anchor);
    h.event(egui::Event::Zoom(2.0));
    h.run_steps(1);
    assert!((h.state().canvas_scale - 4.0).abs() < 0.01);
    let still = slide_local(h.state(), anchor);
    assert!((still.x - stuck.x).abs() < 1.5 && (still.y - stuck.y).abs() < 1.5, "{stuck:?} → {still:?}");
}

#[test]
fn two_finger_scroll_pans_a_zoomed_slide_and_does_not_change_slides() {
    let mut h = harness();
    h.state_mut().ui.zoom = Some(2.0);
    h.run_steps(2);
    let canvas = h.state().canvas_rect.expect("canvas");
    h.hover_at(canvas.center());
    h.run_steps(1);
    let before = h.state().slide_rect.expect("slide").min;
    let slide = h.state().session.active().expect("doc").selection.slide;
    // A trackpad sends small point deltas. Negative Y scrolls the content up, revealing the rest of the slide.
    for _ in 0..4 {
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -6.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::default(),
        });
    }
    h.run_steps(4);
    let after = h.state().slide_rect.expect("slide").min;
    assert!(after.y < before.y - 15.0, "the slide should move up, {before:?} → {after:?}");
    assert_eq!(h.state().session.active().expect("doc").selection.slide, slide);
}

#[test]
fn pinch_away_from_the_slide_does_not_zoom_it() {
    let mut h = harness();
    h.hover_at(Pos2::new(40.0, 20.0));
    h.run_steps(1);
    h.event(egui::Event::Zoom(1.8));
    h.run_steps(1);
    assert!(h.state().ui.zoom.is_none(), "a pinch on the title bar leaves Fit alone");
}

#[test]
fn pinch_zooms_sorter_thumbnails() {
    let mut h = harness();
    h.state_mut().ui.view = ViewMode::Sorter;
    h.run_steps(2);
    h.hover_at(Pos2::new(600.0, 450.0));
    h.run_steps(1);
    h.event(egui::Event::Zoom(1.5));
    h.run_steps(1);
    let z = h.state().ui.sorter_zoom;
    assert!((z - 1.5).abs() < 0.02, "sorter zoom {z}");
    h.hover_at(Pos2::new(40.0, 20.0));
    h.run_steps(1);
    h.event(egui::Event::Zoom(2.0));
    h.run_steps(1);
    assert!((h.state().ui.sorter_zoom - z).abs() < 0.02, "a pinch on the title bar does not resize the sorter");
}
