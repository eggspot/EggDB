use eggdb_platform::window::{AccessNode, AccessRole, Key, Modifiers};

use crate::color::ThemeKind;
use crate::connect_screen::ConnectAction;

/// The modifier bit for Shift in [`modifiers_from_bits`].
pub const MODIFIER_SHIFT: u32 = 1;
/// The modifier bit for Control.
pub const MODIFIER_CONTROL: u32 = 2;
/// The modifier bit for Alt (Option).
pub const MODIFIER_ALT: u32 = 4;
/// The modifier bit for Meta (Command on macOS).
pub const MODIFIER_META: u32 = 8;

/// The GUI key for a DOM `KeyboardEvent.key` value (W3C UI Events), `None` for keys the card
/// does not use (so the browser keeps its own behaviour for them).
pub fn key_from_dom(key: &str) -> Option<Key> {
    let named = match key {
        "Tab" => Key::Tab,
        "Enter" => Key::Enter,
        "Escape" | "Esc" => Key::Escape,
        " " | "Spacebar" => Key::Space,
        "Backspace" => Key::Backspace,
        "Delete" | "Del" => Key::Delete,
        "ArrowLeft" | "Left" => Key::Left,
        "ArrowRight" | "Right" => Key::Right,
        "ArrowUp" | "Up" => Key::Up,
        "ArrowDown" | "Down" => Key::Down,
        "Home" => Key::Home,
        "End" => Key::End,
        "F4" => Key::F4,
        _ => {
            let mut characters = key.chars();
            let character = characters.next()?;
            if characters.next().is_some() || character.is_control() {
                return None;
            }
            Key::Character(character)
        }
    };
    Some(named)
}

/// The modifiers in `bits`, an OR of the `MODIFIER_*` values; Meta is Command.
pub fn modifiers_from_bits(bits: u32) -> Modifiers {
    Modifiers {
        shift: bits & MODIFIER_SHIFT != 0,
        control: bits & MODIFIER_CONTROL != 0,
        alt: bits & MODIFIER_ALT != 0,
        command: bits & MODIFIER_META != 0,
    }
}

/// The theme for a host's theme number: 0 light, 1 dark, 2 high contrast, anything else light.
pub fn theme_from_code(code: u32) -> ThemeKind {
    match code {
        1 => ThemeKind::Dark,
        2 => ThemeKind::HighContrast,
        _ => ThemeKind::Light,
    }
}

/// The number a host receives for an action: 0 none, 1 open, 2 browse, 3 cancel, 4 show the
/// layout picker, 5 load the sample dataset, 6 quit.
pub fn action_code(action: ConnectAction) -> u32 {
    match action {
        ConnectAction::None => 0,
        ConnectAction::Open => 1,
        ConnectAction::Browse => 2,
        ConnectAction::Cancel => 3,
        ConnectAction::ShowLayoutPicker => 4,
        ConnectAction::LoadSampleDataset => 5,
        ConnectAction::Quit => 6,
    }
}

/// The WAI-ARIA role of the DOM element that mirrors a node over the canvas.
pub fn aria_role(role: AccessRole) -> &'static str {
    match role {
        AccessRole::Window => "document",
        AccessRole::Group => "group",
        AccessRole::Heading => "heading",
        AccessRole::TabList => "tablist",
        AccessRole::Tab => "tab",
        AccessRole::TabPanel => "tabpanel",
        AccessRole::Status => "status",
        AccessRole::TextField => "textbox",
        AccessRole::Button => "button",
        AccessRole::CheckBox => "checkbox",
        AccessRole::Link => "link",
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json_optional(text: Option<&str>) -> String {
    text.map_or_else(|| "null".to_owned(), json_string)
}

fn json_number(value: f32) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "0".to_owned()
    }
}

/// The accessibility tree as the JSON array the browser loader mirrors into ARIA elements:
/// one object per node with `id`, `parent`, `role` (ARIA), `name`, `value`, `placeholder`,
/// `focused`, `checked`, `disabled`, `selected`, `default` and `x`, `y`, `width`, `height` in
/// CSS pixels.
pub fn accessibility_json(nodes: &[AccessNode]) -> String {
    let objects: Vec<String> = nodes
        .iter()
        .map(|node| {
            let state = node.state;
            format!(
                "{{\"id\":{},\"parent\":{},\"role\":{},\"name\":{},\"value\":{},\"placeholder\":{},\"focused\":{},\"checked\":{},\"disabled\":{},\"selected\":{},\"default\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{}}}",
                node.id,
                node.parent
                    .map_or_else(|| "null".to_owned(), |parent| parent.to_string()),
                json_string(aria_role(node.role)),
                json_string(&node.name),
                json_optional(node.value.as_deref()),
                json_optional(node.placeholder.as_deref()),
                state.focused,
                state
                    .checked
                    .map_or_else(|| "null".to_owned(), |checked| checked.to_string()),
                state.disabled,
                state.selected,
                state.default_button,
                json_number(node.bounds.x),
                json_number(node.bounds.y),
                json_number(node.bounds.width),
                json_number(node.bounds.height),
            )
        })
        .collect();
    format!("[{}]", objects.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect_screen::{ConnectCard, DEFAULT_HEIGHT, DEFAULT_WIDTH, accessibility_tree};
    use crate::font::BoxFace;
    use crate::main_window::Faces;

    #[test]
    fn dom_key_values_become_gui_keys() {
        assert_eq!(key_from_dom("Tab"), Some(Key::Tab));
        assert_eq!(key_from_dom(" "), Some(Key::Space));
        assert_eq!(key_from_dom("ArrowLeft"), Some(Key::Left));
        assert_eq!(key_from_dom("Escape"), Some(Key::Escape));
        assert_eq!(key_from_dom("q"), Some(Key::Character('q')));
        assert_eq!(key_from_dom("é"), Some(Key::Character('é')));
        assert_eq!(key_from_dom("Shift"), None);
        assert_eq!(key_from_dom("F5"), None);
        assert_eq!(key_from_dom(""), None);
        assert_eq!(key_from_dom("\u{7}"), None);
    }

    #[test]
    fn modifier_bits_themes_and_actions_have_fixed_numbers() {
        assert_eq!(modifiers_from_bits(0), Modifiers::default());
        assert_eq!(
            modifiers_from_bits(MODIFIER_SHIFT | MODIFIER_META),
            Modifiers {
                shift: true,
                control: false,
                alt: false,
                command: true
            }
        );
        assert!(modifiers_from_bits(MODIFIER_CONTROL | MODIFIER_ALT).control);
        assert_eq!(theme_from_code(0), ThemeKind::Light);
        assert_eq!(theme_from_code(1), ThemeKind::Dark);
        assert_eq!(theme_from_code(2), ThemeKind::HighContrast);
        assert_eq!(theme_from_code(9), ThemeKind::Light);
        assert_eq!(action_code(ConnectAction::None), 0);
        assert_eq!(action_code(ConnectAction::Open), 1);
        assert_eq!(action_code(ConnectAction::Quit), 6);
    }

    #[test]
    fn the_tree_json_carries_aria_roles_names_and_states() {
        let faces = Faces {
            ui: &BoxFace,
            ui_bold: &BoxFace,
            mono: &BoxFace,
        };
        let nodes = accessibility_tree(
            &ConnectCard::first_start(),
            faces,
            (DEFAULT_WIDTH, DEFAULT_HEIGHT),
        );
        let json = accessibility_json(&nodes);
        assert!(
            json.starts_with("[{\"id\":1,\"parent\":null,\"role\":\"document\",\"name\":\"EggDB\"")
        );
        assert!(json.contains(
            "{\"id\":9,\"parent\":7,\"role\":\"textbox\",\"name\":\"File\",\"value\":\"~/eggdb/quickstart.eggdb\",\"placeholder\":\"/path/to/shop.eggdb\",\"focused\":true,\"checked\":null,\"disabled\":false,\"selected\":false,\"default\":false,\"x\":384,\"y\":282,\"width\":420,\"height\":28}"
        ), "{json}");
        assert!(json.contains("\"role\":\"checkbox\",\"name\":\"Open read-only\",\"value\":null,\"placeholder\":null,\"focused\":false,\"checked\":false"));
        assert!(json.contains("\"role\":\"button\",\"name\":\"Cancel\",\"value\":null,\"placeholder\":null,\"focused\":false,\"checked\":null,\"disabled\":true"));
        assert_eq!(json.matches("\"id\":").count(), nodes.len());
        assert!(json.ends_with("}]"));
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(json_number(f32::NAN), "0");
        assert_eq!(json_number(1.5), "1.5");
    }
}
