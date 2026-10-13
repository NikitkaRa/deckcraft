//! Save As: PowerPoint is offered and a bare name gets the chosen format (#208).

#[test]
fn save_dialog_offers_powerpoint_and_preselects_the_asked_type() {
    let f = crate::save_filters("Talk.pptx");
    assert_eq!(f[0].1, &["pptx"]);
    assert!(f.iter().any(|x| x.1 == ["deckcraft"]));
    let f = crate::save_filters("Talk.deckcraft");
    assert_eq!(f[0].1, &["deckcraft"]);
    assert!(f.iter().any(|x| x.1 == ["pptx"]), "PowerPoint is always offered (#208)");
    assert_eq!(crate::save_filters("weird")[0].1, &["deckcraft"]);
}

#[test]
fn saving_as_powerpoint_writes_a_pptx_powerpoint_can_open() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let dir = std::env::temp_dir().join(format!("deckcraft-208-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let asked: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = asked.clone();
    let d2 = dir.clone();
    // The dialog hands back a bare name, as a GTK dialog does when no type is typed.
    let services = crate::Services {
        pick_save: Some(Box::new(move |suggested: &str| {
            log.borrow_mut().push(suggested.to_string());
            Some(d2.join("Deck").to_string_lossy().to_string())
        })),
        ..Default::default()
    };
    let mut app = crate::SlideApp::new(deckcraft_engine::Session::with_new(), services);
    app.save_as_dialog("pptx");
    let path = dir.join("Deck.pptx");
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.windows(20).any(|w| w == b"ppt/presentation.xml"), "a PowerPoint package, not native");
    assert!(deckcraft_engine::cmd::file::open_presentation("Deck.pptx", &bytes).is_ok());
    assert!(asked.borrow()[0].ends_with(".pptx"));
    // Save As on a deck that is a .pptx suggests .pptx again.
    assert_eq!(app.current_save_format(), "pptx");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_bare_save_name_gets_the_chosen_format() {
    assert_eq!(crate::with_save_extension("/tmp/Talk", "pptx"), "/tmp/Talk.pptx");
    assert_eq!(crate::with_save_extension("/tmp/Talk.PPTX", "deckcraft"), "/tmp/Talk.PPTX");
    assert_eq!(crate::with_save_extension("/tmp/Talk.deckcraft", "pptx"), "/tmp/Talk.deckcraft");
    assert_eq!(crate::with_save_extension("/tmp/v1.2", "pptx"), "/tmp/v1.2.pptx");
}
