//! The macOS menu bar, built from [`deckcraft_ui_egui::menus::menu_tree`].
//!
//! Other desktops keep the ribbon only: PowerPoint on Windows hides its menus, and Wayland has no
//! global menu bar. Accelerators are the same shortcut strings the commands already use. A bare
//! letter is never an accelerator, so the menu cannot swallow typing, and Cmd+Space is left for
//! Spotlight.

use deckcraft_ui_egui::SlideApp;

const QUIT: &str = "menu.quit";

/// One shortcut chord, in the same spelling as `Cmd+Shift+]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyShortcut {
    pub cmd: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
}

/// Split `Cmd+Shift+]` the way the engine matches shortcuts. `+` itself is the key of `Cmd++`.
pub fn parse_shortcut(shortcut: &str) -> Option<KeyShortcut> {
    if shortcut.is_empty() {
        return None;
    }
    let parts: Vec<&str> = shortcut.split('+').collect();
    let (key, mods) = match parts.as_slice() {
        [.., "", ""] => ("+", &parts[..parts.len().saturating_sub(2)]),
        _ => {
            let (k, ms) = parts.split_last()?;
            (*k, ms)
        }
    };
    if key.is_empty() {
        return None;
    }
    let has = |m: &str| mods.iter().any(|x| x.eq_ignore_ascii_case(m));
    Some(KeyShortcut { cmd: has("cmd") || has("ctrl"), shift: has("shift"), alt: has("alt") || has("option"), key: key.to_string() })
}

/// A menu accelerator, or nothing when the chord would steal typing or Spotlight.
pub fn menu_accelerator(shortcut: &str) -> Option<KeyShortcut> {
    let s = parse_shortcut(shortcut)?;
    if s.key.eq_ignore_ascii_case("space") && s.cmd && !s.alt && !s.shift {
        return None;
    }
    let function = is_function_key(&s.key);
    if s.cmd || s.alt || function || (s.shift && s.key.eq_ignore_ascii_case("tab")) { Some(s) } else { None }
}

fn is_function_key(key: &str) -> bool {
    let Some(rest) = key.strip_prefix(['F', 'f']) else { return false };
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
}

/// `keyboard-types` name for a shortcut key (`S` → `KeyS`, `]` → `BracketRight`).
pub fn code_name(key: &str) -> Option<String> {
    let k = key.to_ascii_lowercase();
    if k.len() == 1 {
        let c = k.chars().next()?;
        if c.is_ascii_lowercase() {
            return Some(format!("Key{}", c.to_ascii_uppercase()));
        }
        if c.is_ascii_digit() {
            return Some(format!("Digit{c}"));
        }
        return Some(
            match c {
                '=' | '+' => "Equal",
                '-' => "Minus",
                ',' => "Comma",
                '.' => "Period",
                '[' => "BracketLeft",
                ']' => "BracketRight",
                '`' => "Backquote",
                ';' => "Semicolon",
                '\'' => "Quote",
                '/' => "Slash",
                '\\' => "Backslash",
                _ => return None,
            }
            .to_string(),
        );
    }
    match k.as_str() {
        "space" => Some("Space".to_string()),
        "tab" => Some("Tab".to_string()),
        "enter" | "return" => Some("Enter".to_string()),
        "escape" | "esc" => Some("Escape".to_string()),
        "backspace" => Some("Backspace".to_string()),
        "delete" => Some("Delete".to_string()),
        _ if is_function_key(&k) => Some(format!("F{}", &k[1..])),
        _ => None,
    }
}

fn command_shortcut(id: &str) -> Option<&'static str> {
    deckcraft_engine::find_command(id)
        .and_then(|c| c.shortcut)
        .or_else(|| deckcraft_ui_egui::UI_COMMANDS.iter().find(|c| c.0 == id).and_then(|c| c.2))
}

/// Installed menu. Empty on every platform except macOS.
pub struct MenuBar {
    #[cfg(target_os = "macos")]
    inner: Option<mac::Installed>,
}

impl MenuBar {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self { inner: mac::install() }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {}
        }
    }

    pub fn dispatch(&self, app: &mut SlideApp, ctx: &egui::Context) {
        #[cfg(target_os = "macos")]
        if let Some(inner) = &self.inner {
            inner.dispatch(app, ctx);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (app, ctx);
        }
    }

    pub fn sync(&self, app: &SlideApp) {
        #[cfg(target_os = "macos")]
        if let Some(inner) = &self.inner {
            inner.sync(app);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = app;
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::str::FromStr;

    use super::{QUIT, code_name, command_shortcut, menu_accelerator};
    use crate::SlideApp;
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
    use serde_json::json;

    pub struct Installed {
        _menu: Menu,
        _submenus: Vec<Submenu>,
        items: Vec<(String, MenuItem)>,
    }

    pub fn install() -> Option<Installed> {
        match build() {
            Ok(installed) => Some(installed),
            Err(e) => {
                log::warn!("menu bar was not installed: {e}");
                None
            }
        }
    }

    fn build() -> Result<Installed, muda::Error> {
        let menu = Menu::new();
        let mut submenus = Vec::new();
        let mut items = Vec::new();

        let app_menu = Submenu::new("DeckCraft", true);
        let about = tracked(&mut items, "app.about", "About DeckCraft");
        let prefs = tracked(&mut items, "app.preferences", "Preferences…");
        let quit = MenuItem::with_id(QUIT, "Quit DeckCraft", true, accelerator_for("Q", true, false, false));
        app_menu.append_items(&[
            &about,
            &PredefinedMenuItem::separator(),
            &prefs,
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &quit,
        ])?;
        menu.append(&app_menu)?;
        submenus.push(app_menu);

        for (title, entries) in deckcraft_ui_egui::menus::menu_tree() {
            let submenu = Submenu::new(title, true);
            let mut owned: Vec<MenuItem> = Vec::new();
            let mut seps: Vec<PredefinedMenuItem> = Vec::new();
            for (label, id) in &entries {
                if id.is_empty() {
                    seps.push(PredefinedMenuItem::separator());
                } else {
                    owned.push(tracked(&mut items, id, label));
                }
            }
            let mut sep_i = 0;
            let mut item_i = 0;
            for (_, id) in &entries {
                if id.is_empty() {
                    submenu.append(&seps[sep_i])?;
                    sep_i += 1;
                } else {
                    submenu.append(&owned[item_i])?;
                    item_i += 1;
                }
            }
            menu.append(&submenu)?;
            submenus.push(submenu);
        }

        menu.init_for_nsapp();
        Ok(Installed { _menu: menu, _submenus: submenus, items })
    }

    fn tracked(items: &mut Vec<(String, MenuItem)>, id: &str, label: &str) -> MenuItem {
        let accel = command_shortcut(id)
            .filter(|shortcut| accelerator_follows_the_key(id, shortcut))
            .and_then(menu_accelerator)
            .as_ref()
            .and_then(to_accelerator);
        let item = MenuItem::with_id(id, escape_mnemonic(label), true, accel);
        items.push((id.to_string(), item.clone()));
        item
    }

    /// The menu item's shortcut is shown only when pressing it does that command.
    /// Cmd+P opens the print page; the Print item itself prints, so it must not take the key.
    fn accelerator_follows_the_key(id: &str, _shortcut: &str) -> bool {
        id != "app.print"
    }

    /// `&` starts a mnemonic. Labels are shown as written.
    fn escape_mnemonic(label: &str) -> String {
        label.replace('&', "&&")
    }

    fn accelerator_for(key: &str, cmd: bool, shift: bool, alt: bool) -> Option<Accelerator> {
        to_accelerator(&super::KeyShortcut { cmd, shift, alt, key: key.to_string() })
    }

    fn to_accelerator(s: &super::KeyShortcut) -> Option<Accelerator> {
        let name = code_name(&s.key)?;
        let code = Code::from_str(&name).ok()?;
        let mut mods = Modifiers::empty();
        if s.cmd {
            mods.insert(Modifiers::SUPER);
        }
        if s.shift {
            mods.insert(Modifiers::SHIFT);
        }
        if s.alt {
            mods.insert(Modifiers::ALT);
        }
        Some(Accelerator::new((!mods.is_empty()).then_some(mods), code))
    }

    impl Installed {
        pub fn dispatch(&self, app: &mut SlideApp, ctx: &egui::Context) {
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let id = event.id.0.as_str();
                if id == QUIT {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else if id == "file.save" {
                    // Same path as Cmd+S: save, or open Save As when the presentation has no file yet.
                    app.save();
                } else if let Err(e) = app.run(id, json!({})) {
                    log::warn!("menu {id}: {e}");
                }
            }
        }

        pub fn sync(&self, app: &SlideApp) {
            for (id, item) in &self.items {
                let on = deckcraft_ui_egui::ribbon::enabled(app, id);
                if item.is_enabled() != on {
                    item.set_enabled(on);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_split_the_way_the_engine_matches_them() {
        assert_eq!(parse_shortcut("Cmd+Shift+]"), Some(KeyShortcut { cmd: true, shift: true, alt: false, key: "]".into() }));
        assert_eq!(parse_shortcut("F5"), Some(KeyShortcut { cmd: false, shift: false, alt: false, key: "F5".into() }));
        assert_eq!(parse_shortcut("Cmd++"), Some(KeyShortcut { cmd: true, shift: false, alt: false, key: "+".into() }));
        assert_eq!(parse_shortcut("Alt+F9"), Some(KeyShortcut { cmd: false, shift: false, alt: true, key: "F9".into() }));
    }

    #[test]
    fn a_menu_accelerator_keeps_typing_and_spotlight() {
        assert!(menu_accelerator("Cmd+S").is_some());
        assert!(menu_accelerator("F5").is_some());
        assert!(menu_accelerator("Shift+Tab").is_some());
        assert!(menu_accelerator("b").is_none());
        assert!(menu_accelerator("Cmd+Space").is_none());
    }

    #[test]
    fn code_names_cover_the_chords_the_menus_use() {
        assert_eq!(code_name("S"), Some("KeyS".into()));
        assert_eq!(code_name("]"), Some("BracketRight".into()));
        assert_eq!(code_name("["), Some("BracketLeft".into()));
        assert_eq!(code_name("="), Some("Equal".into()));
        assert_eq!(code_name("+"), Some("Equal".into()));
        assert_eq!(code_name("`"), Some("Backquote".into()));
        assert_eq!(code_name(","), Some("Comma".into()));
        assert_eq!(code_name("."), Some("Period".into()));
        assert_eq!(code_name("F5"), Some("F5".into()));
        assert_eq!(code_name("Tab"), Some("Tab".into()));
        assert_eq!(code_name("Space"), Some("Space".into()));
    }

    #[test]
    fn every_menu_shortcut_can_be_an_accelerator() {
        for (_, entries) in deckcraft_ui_egui::menus::menu_tree() {
            for (_, id) in entries {
                let Some(shortcut) = command_shortcut(id) else { continue };
                assert!(menu_accelerator(shortcut).is_some(), "{id} {shortcut}");
                let parsed = menu_accelerator(shortcut).expect("just checked");
                assert!(code_name(&parsed.key).is_some(), "{id} {shortcut}");
            }
        }
    }
}
