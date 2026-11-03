use std::time::Instant;

use eggdb_platform::window::{
    AccessAction, AccessNode, AccessRole, Key, Modifiers, PointerButton,
};

use crate::accessibility::action_keys;
use crate::color::ThemeKind;
use crate::connect_screen::{self, ConnectAction, ConnectCard};
use crate::main_window::Faces;
use crate::pointer::{self, ClickCounter};

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
        "F2" => Key::F2,
        "F5" => Key::F5,
        "F10" => Key::F10,
        "ContextMenu" | "Apps" => Key::ContextMenu,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
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
/// layout picker, 5 load the sample dataset, 6 quit, 7 stop the open in progress, 8 copy the
/// focused input's selection, 9 paste into the focused input.
pub fn action_code(action: ConnectAction) -> u32 {
    match action {
        ConnectAction::None => 0,
        ConnectAction::Open => 1,
        ConnectAction::Browse => 2,
        ConnectAction::Cancel => 3,
        ConnectAction::ShowLayoutPicker => 4,
        ConnectAction::LoadSampleDataset => 5,
        ConnectAction::Quit => 6,
        ConnectAction::StopOpening => 7,
        ConnectAction::CopySelection => 8,
        ConnectAction::Paste => 9,
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
        AccessRole::Toolbar => "toolbar",
        AccessRole::ComboBox => "combobox",
        AccessRole::TextArea => "textbox",
        AccessRole::Alert => "alert",
        AccessRole::Tree => "tree",
        AccessRole::TreeItem => "treeitem",
        AccessRole::Grid => "grid",
        AccessRole::ColumnHeader => "columnheader",
        AccessRole::Cell => "gridcell",
    }
}

/// DOM `WheelEvent.deltaMode` for deltas in CSS pixels.
pub const DOM_DELTA_PIXEL: u32 = 0;
/// DOM `WheelEvent.deltaMode` for deltas in lines.
pub const DOM_DELTA_LINE: u32 = 1;
/// DOM `WheelEvent.deltaMode` for deltas in pages.
pub const DOM_DELTA_PAGE: u32 = 2;
/// The CSS pixels browsers report for one wheel notch in [`DOM_DELTA_PIXEL`] mode.
pub const DOM_PIXELS_PER_NOTCH: f32 = 100.0;
/// The lines browsers report for one wheel notch in [`DOM_DELTA_LINE`] mode.
pub const DOM_LINES_PER_NOTCH: f32 = 3.0;
/// The notches one page of a [`DOM_DELTA_PAGE`] delta counts as.
pub const NOTCHES_PER_DOM_PAGE: f32 = 10.0;

/// The pointer button of a DOM `MouseEvent.button` value (0 main, 1 auxiliary, 2 secondary),
/// `None` for the back and forward buttons and anything else.
pub fn pointer_button_from_dom(button: u32) -> Option<PointerButton> {
    match button {
        0 => Some(PointerButton::Primary),
        1 => Some(PointerButton::Middle),
        2 => Some(PointerButton::Secondary),
        _ => None,
    }
}

/// A DOM wheel `delta` in `mode` (`WheelEvent.deltaMode`) as wheel notches, the platform
/// layer's lines (one line a notch, D279); a delta that is not finite or a mode the DOM does not
/// define counts as none.
pub fn wheel_notches(delta: f32, mode: u32) -> f32 {
    if !delta.is_finite() {
        return 0.0;
    }
    match mode {
        DOM_DELTA_PIXEL => delta / DOM_PIXELS_PER_NOTCH,
        DOM_DELTA_LINE => delta / DOM_LINES_PER_NOTCH,
        DOM_DELTA_PAGE => delta * NOTCHES_PER_DOM_PAGE,
        _ => 0.0,
    }
}

/// What a pointer event did to the browser card: whether its frame changed, and the action
/// for the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerAnswer {
    /// Whether the card must be drawn again.
    pub changed: bool,
    /// The action the host acts on (opening, browsing, the layout picker, ...).
    pub action: ConnectAction,
}

impl PointerAnswer {
    /// The card did not use the event, so the browser keeps its own behaviour for it.
    pub const UNUSED: Self = Self {
        changed: false,
        action: ConnectAction::None,
    };
    /// The card changed and asks nothing of the host.
    pub const CHANGED: Self = Self {
        changed: true,
        action: ConnectAction::None,
    };

    /// Whether the card used the event.
    pub fn used(self) -> bool {
        self.changed || self.action != ConnectAction::None
    }
}

/// The pointer on the browser card, as `Workspace` drives it on the desktop (D279, D313): the
/// clicks counted into double and triple clicks, and the input a drag extends the selection in.
/// Positions are CSS pixels from the card canvas's top-left corner, the card's logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CardPointer {
    clicks: ClickCounter,
    field_drag: Option<f32>,
}

fn finite(x: f32, y: f32) -> bool {
    x.is_finite() && y.is_finite()
}

fn is_text_input(role: AccessRole) -> bool {
    matches!(role, AccessRole::TextField | AccessRole::TextArea)
}

fn caret_state(card: &ConnectCard) -> Option<(usize, Option<(usize, usize)>)> {
    card.focused_field()
        .map(|field| (field.caret(), field.selection()))
}

fn activate(card: &mut ConnectCard, node: &AccessNode, action: &AccessAction) -> ConnectAction {
    if !node.allows(action) || !card.focus_node(node.id) {
        return ConnectAction::None;
    }
    let mut answer = ConnectAction::None;
    for press in action_keys(node, action) {
        let (next, action) = connect_screen::handle_key(card.clone(), press);
        *card = next;
        if action != ConnectAction::None {
            answer = action;
        }
    }
    answer
}

impl CardPointer {
    /// A button went down at (`x`, `y`) at `now`: the primary button presses the control under
    /// it and focuses an input with the caret at the press (the word on a double-click, the
    /// value on a triple-click, extending with `Shift`). Other buttons, a busy card and the
    /// layout picker leave the event to the browser.
    #[allow(clippy::too_many_arguments)]
    pub fn pressed(
        &mut self,
        card: &mut ConnectCard,
        faces: Faces<'_>,
        size: (u32, u32),
        (x, y): (f32, f32),
        button: PointerButton,
        modifiers: Modifiers,
        now: Instant,
    ) -> PointerAnswer {
        if button != PointerButton::Primary
            || card.busy
            || card.layout_picker.is_some()
            || !finite(x, y)
        {
            return PointerAnswer::UNUSED;
        }
        let clicks = self.clicks.press(now, x, y);
        let tree = connect_screen::accessibility_tree(card, faces, size);
        let Some(node) = pointer::hit_test(&tree, x, y).cloned() else {
            return PointerAnswer::UNUSED;
        };
        card.pointer.pressed = Some(node.id);
        card.pointer.hovered = Some(node.id);
        if is_text_input(node.role) && card.focus_node(node.id) {
            card.place_caret_at(faces, node.bounds.x, x, clicks, modifiers.shift);
            self.field_drag = Some(node.bounds.x);
        }
        card.pointer.focus_by_pointer = true;
        PointerAnswer::CHANGED
    }

    /// The pointer moved to (`x`, `y`): the hover follows it and a drag that started in an
    /// input extends its selection. Returns whether the frame changed.
    pub fn moved(
        &mut self,
        card: &mut ConnectCard,
        faces: Faces<'_>,
        size: (u32, u32),
        (x, y): (f32, f32),
    ) -> bool {
        if !finite(x, y) {
            return false;
        }
        let mut changed = false;
        if let Some(box_left) = self.field_drag {
            let before = caret_state(card);
            card.place_caret_at(faces, box_left, x, 1, true);
            changed = caret_state(card) != before;
        }
        let tree = connect_screen::accessibility_tree(card, faces, size);
        let hovered = pointer::hit_test(&tree, x, y).map(|node| node.id);
        if card.pointer.hovered != hovered {
            card.pointer.hovered = hovered;
            changed = true;
        }
        changed
    }

    /// A button went up at (`x`, `y`): a press that started on a button, link, checkbox,
    /// combo box or tab and ends on it acts; a release elsewhere cancels it, and a drag ends.
    pub fn released(
        &mut self,
        card: &mut ConnectCard,
        faces: Faces<'_>,
        size: (u32, u32),
        (x, y): (f32, f32),
        button: PointerButton,
    ) -> PointerAnswer {
        if button != PointerButton::Primary {
            return PointerAnswer::UNUSED;
        }
        self.field_drag = None;
        let Some(pressed) = card.pointer.pressed.take() else {
            return PointerAnswer::UNUSED;
        };
        let tree = connect_screen::accessibility_tree(card, faces, size);
        let Some(node) = pointer::hit_test(&tree, x, y)
            .filter(|node| finite(x, y) && node.id == pressed)
            .cloned()
        else {
            return PointerAnswer::CHANGED;
        };
        let action = match node.role {
            AccessRole::Button | AccessRole::Link | AccessRole::ComboBox => AccessAction::Invoke,
            AccessRole::CheckBox => AccessAction::Toggle,
            AccessRole::Tab => AccessAction::Select,
            _ => return PointerAnswer::CHANGED,
        };
        let action = activate(card, &node, &action);
        card.pointer.focus_by_pointer = true;
        PointerAnswer {
            changed: true,
            action,
        }
    }

    /// The browser's own `dblclick` at (`x`, `y`): when the press count missed it (the user's
    /// double-click interval is longer than 500 ms), the word under it in an input is
    /// selected. Returns whether the frame changed.
    pub fn double_clicked(
        &mut self,
        card: &mut ConnectCard,
        faces: Faces<'_>,
        size: (u32, u32),
        (x, y): (f32, f32),
    ) -> bool {
        if card.busy || card.layout_picker.is_some() || !finite(x, y) {
            return false;
        }
        let tree = connect_screen::accessibility_tree(card, faces, size);
        let Some(node) = pointer::hit_test(&tree, x, y)
            .filter(|node| is_text_input(node.role))
            .cloned()
        else {
            return false;
        };
        let selected = card
            .focused_field()
            .is_some_and(|field| field.selection().is_some());
        if selected || !card.focus_node(node.id) {
            return false;
        }
        card.place_caret_at(faces, node.bounds.x, x, 2, false);
        true
    }

    /// The pointer left the card: nothing is hovered. Returns whether the frame changed.
    pub fn left(&mut self, card: &mut ConnectCard) -> bool {
        card.pointer.hovered.take().is_some()
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
/// `focused`, `checked`, `disabled`, `selected`, `default`, `x`, `y`, `width`, `height` in
/// CSS pixels, then `expanded`, `invalid`, `readonly`, `description` and `shortcut` (D278).
pub fn accessibility_json(nodes: &[AccessNode]) -> String {
    let objects: Vec<String> = nodes
        .iter()
        .map(|node| {
            let state = node.state;
            format!(
                "{{\"id\":{},\"parent\":{},\"role\":{},\"name\":{},\"value\":{},\"placeholder\":{},\"focused\":{},\"checked\":{},\"disabled\":{},\"selected\":{},\"default\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"expanded\":{},\"invalid\":{},\"readonly\":{},\"description\":{},\"shortcut\":{}}}",
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
                state
                    .expanded
                    .map_or_else(|| "null".to_owned(), |expanded| expanded.to_string()),
                state.invalid,
                state.read_only,
                json_optional(node.description.as_deref()),
                json_optional(node.shortcut.as_deref()),
            )
        })
        .collect();
    format!("[{}]", objects.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect_screen::{
        DEFAULT_HEIGHT, DEFAULT_WIDTH, NODE_FILE, NODE_OPEN, NODE_READ_ONLY, accessibility_tree,
    };
    use crate::font::BoxFace;
    use std::time::Duration;

    #[test]
    fn dom_key_values_become_gui_keys() {
        assert_eq!(key_from_dom("Tab"), Some(Key::Tab));
        assert_eq!(key_from_dom(" "), Some(Key::Space));
        assert_eq!(key_from_dom("ArrowLeft"), Some(Key::Left));
        assert_eq!(key_from_dom("Escape"), Some(Key::Escape));
        assert_eq!(key_from_dom("q"), Some(Key::Character('q')));
        assert_eq!(key_from_dom("é"), Some(Key::Character('é')));
        assert_eq!(key_from_dom("Shift"), None);
        assert_eq!(key_from_dom("F5"), Some(Key::F5));
        assert_eq!(key_from_dom("F10"), Some(Key::F10));
        assert_eq!(key_from_dom("ContextMenu"), Some(Key::ContextMenu));
        assert_eq!(key_from_dom("F6"), None);
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
        assert_eq!(action_code(ConnectAction::CopySelection), 8);
        assert_eq!(action_code(ConnectAction::Paste), 9);
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
            "{\"id\":9,\"parent\":7,\"role\":\"textbox\",\"name\":\"File\",\"value\":\"~/eggdb/quickstart.eggdb\",\"placeholder\":\"/path/to/shop.eggdb\",\"focused\":true,\"checked\":null,\"disabled\":false,\"selected\":false,\"default\":false,\"x\":384,\"y\":282,\"width\":420,\"height\":28,\"expanded\":null,\"invalid\":false,\"readonly\":false,\"description\":null,\"shortcut\":null}"
        ), "{json}");
        assert!(json.contains("\"role\":\"checkbox\",\"name\":\"Open read-only\",\"value\":null,\"placeholder\":null,\"focused\":false,\"checked\":false"));
        assert!(json.contains("\"role\":\"button\",\"name\":\"Cancel\",\"value\":null,\"placeholder\":null,\"focused\":false,\"checked\":null,\"disabled\":true"));
        assert_eq!(json.matches("\"id\":").count(), nodes.len());
        assert!(json.ends_with("}]"));
    }

    const SIZE: (u32, u32) = (DEFAULT_WIDTH, DEFAULT_HEIGHT);

    fn box_faces() -> Faces<'static> {
        Faces {
            ui: &BoxFace,
            ui_bold: &BoxFace,
            mono: &BoxFace,
        }
    }

    fn centre_of(card: &ConnectCard, id: u32) -> (f32, f32) {
        let tree = accessibility_tree(card, box_faces(), SIZE);
        let node = tree.iter().find(|node| node.id == id).unwrap();
        (
            node.bounds.x + node.bounds.width / 2.0,
            node.bounds.y + node.bounds.height / 2.0,
        )
    }

    fn click(
        pointer: &mut CardPointer,
        card: &mut ConnectCard,
        at: (f32, f32),
        now: Instant,
    ) -> PointerAnswer {
        let down = pointer.pressed(
            card,
            box_faces(),
            SIZE,
            at,
            PointerButton::Primary,
            Modifiers::default(),
            now,
        );
        assert!(down.used());
        pointer.released(card, box_faces(), SIZE, at, PointerButton::Primary)
    }

    #[test]
    fn dom_buttons_and_wheel_deltas_have_fixed_meanings() {
        assert_eq!(pointer_button_from_dom(0), Some(PointerButton::Primary));
        assert_eq!(pointer_button_from_dom(1), Some(PointerButton::Middle));
        assert_eq!(pointer_button_from_dom(2), Some(PointerButton::Secondary));
        assert_eq!(pointer_button_from_dom(3), None);
        assert_eq!(wheel_notches(100.0, DOM_DELTA_PIXEL), 1.0);
        assert_eq!(wheel_notches(-3.0, DOM_DELTA_LINE), -1.0);
        assert_eq!(wheel_notches(1.0, DOM_DELTA_PAGE), NOTCHES_PER_DOM_PAGE);
        assert_eq!(wheel_notches(f32::INFINITY, DOM_DELTA_PIXEL), 0.0);
        assert_eq!(wheel_notches(5.0, 7), 0.0);
    }

    #[test]
    fn a_click_on_a_checkbox_toggles_it_and_on_open_asks_the_host_to_open() {
        let mut card = ConnectCard::first_start();
        let mut pointer = CardPointer::default();
        let now = Instant::now();
        let read_only = centre_of(&card, NODE_READ_ONLY);
        let answer = click(&mut pointer, &mut card, read_only, now);
        assert_eq!(answer, PointerAnswer::CHANGED);
        assert!(card.read_only);
        assert!(card.pointer.focus_by_pointer);
        assert_eq!(card.pointer.pressed, None);
        let mut fresh = ConnectCard::first_start();
        let open = centre_of(&fresh, NODE_OPEN);
        let later = now + Duration::from_secs(1);
        let answer = click(&mut pointer, &mut fresh, open, later);
        assert_eq!(answer.action, ConnectAction::Open);
    }

    #[test]
    fn a_release_off_the_pressed_button_cancels_it() {
        let mut card = ConnectCard::first_start();
        let mut pointer = CardPointer::default();
        let read_only = centre_of(&card, NODE_READ_ONLY);
        let down = pointer.pressed(
            &mut card,
            box_faces(),
            SIZE,
            read_only,
            PointerButton::Primary,
            Modifiers::default(),
            Instant::now(),
        );
        assert_eq!(down, PointerAnswer::CHANGED);
        assert_eq!(card.pointer.pressed, Some(NODE_READ_ONLY));
        let answer = pointer.released(
            &mut card,
            box_faces(),
            SIZE,
            (2.0, 2.0),
            PointerButton::Primary,
        );
        assert_eq!(answer, PointerAnswer::CHANGED);
        assert!(!card.read_only);
        assert_eq!(card.pointer.pressed, None);
    }

    #[test]
    fn a_press_in_the_file_input_places_the_caret_and_a_drag_selects() {
        let mut card = ConnectCard::first_start();
        card.move_focus(crate::connect_screen::Focus::Open);
        let mut pointer = CardPointer::default();
        let tree = accessibility_tree(&card, box_faces(), SIZE);
        let file = tree.iter().find(|node| node.id == NODE_FILE).unwrap().bounds;
        let middle = file.y + file.height / 2.0;
        let start = (file.x + 1.0, middle);
        pointer.pressed(
            &mut card,
            box_faces(),
            SIZE,
            start,
            PointerButton::Primary,
            Modifiers::default(),
            Instant::now(),
        );
        assert_eq!(card.focused_field().map(|field| field.caret()), Some(0));
        assert_eq!(card.focused_field().and_then(|field| field.selection()), None);
        let end = (file.x + file.width - 2.0, middle);
        assert!(pointer.moved(&mut card, box_faces(), SIZE, end));
        let value_length = card.file.value().chars().count();
        assert_eq!(
            card.focused_field().and_then(|field| field.selection()),
            Some((0, value_length))
        );
        pointer.released(&mut card, box_faces(), SIZE, end, PointerButton::Primary);
        assert!(!pointer.moved(&mut card, box_faces(), SIZE, end));
        assert_eq!(card.copied_text().as_deref(), Some(card.file.value()));
    }

    #[test]
    fn a_double_press_selects_a_word_and_the_browsers_dblclick_only_fills_in() {
        let mut card = ConnectCard::first_start();
        let mut pointer = CardPointer::default();
        let tree = accessibility_tree(&card, box_faces(), SIZE);
        let file = tree.iter().find(|node| node.id == NODE_FILE).unwrap().bounds;
        let at = (file.x + 1.0, file.y + file.height / 2.0);
        let now = Instant::now();
        for clicks in 0..2 {
            pointer.pressed(
                &mut card,
                box_faces(),
                SIZE,
                at,
                PointerButton::Primary,
                Modifiers::default(),
                now + Duration::from_millis(clicks * 100),
            );
            pointer.released(&mut card, box_faces(), SIZE, at, PointerButton::Primary);
        }
        let word = card.copied_text();
        assert!(word.is_some());
        assert!(!pointer.double_clicked(&mut card, box_faces(), SIZE, at));
        let mut slow = CardPointer::default();
        let mut other = ConnectCard::first_start();
        for clicks in 0..2 {
            slow.pressed(
                &mut other,
                box_faces(),
                SIZE,
                at,
                PointerButton::Primary,
                Modifiers::default(),
                now + Duration::from_millis(clicks * 700),
            );
            slow.released(&mut other, box_faces(), SIZE, at, PointerButton::Primary);
        }
        assert_eq!(other.copied_text(), None);
        assert!(slow.double_clicked(&mut other, box_faces(), SIZE, at));
        assert_eq!(other.copied_text(), word);
    }

    #[test]
    fn hover_follows_the_pointer_and_other_buttons_are_the_browsers() {
        let mut card = ConnectCard::first_start();
        let mut pointer = CardPointer::default();
        let open = centre_of(&card, NODE_OPEN);
        assert!(pointer.moved(&mut card, box_faces(), SIZE, open));
        assert_eq!(card.pointer.hovered, Some(NODE_OPEN));
        assert!(!pointer.moved(&mut card, box_faces(), SIZE, open));
        assert!(pointer.left(&mut card));
        assert_eq!(card.pointer.hovered, None);
        let secondary = pointer.pressed(
            &mut card,
            box_faces(),
            SIZE,
            open,
            PointerButton::Secondary,
            Modifiers::default(),
            Instant::now(),
        );
        assert_eq!(secondary, PointerAnswer::UNUSED);
        let nowhere = pointer.pressed(
            &mut card,
            box_faces(),
            SIZE,
            (f32::NAN, 3.0),
            PointerButton::Primary,
            Modifiers::default(),
            Instant::now(),
        );
        assert_eq!(nowhere, PointerAnswer::UNUSED);
        card.busy = true;
        let busy = pointer.pressed(
            &mut card,
            box_faces(),
            SIZE,
            open,
            PointerButton::Primary,
            Modifiers::default(),
            Instant::now(),
        );
        assert_eq!(busy, PointerAnswer::UNUSED);
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(json_number(f32::NAN), "0");
        assert_eq!(json_number(1.5), "1.5");
    }
}
