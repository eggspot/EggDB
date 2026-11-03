//! The platform-neutral side of the screens' accessibility (D278): the actions each role offers a
//! screen reader, a text dump of a tree for golden tests, and a screen reader's request turned
//! into the focus move and the key presses a person would make for the same result, so every
//! bridge (UI Automation, AT-SPI2, NSAccessibility, the browser's ARIA mirror) drives the GUI
//! through the paths the keyboard already tests.

use eggdb_platform::window::{
    AccessAction, AccessActions, AccessCaret, AccessNode, AccessRole, Key, KeyPress, Modifiers,
    MAX_ACCESS_VALUE_CHARS,
};

/// The actions a node of `role` offers by default, before its screen's own exceptions: buttons
/// and links invoke, checkboxes toggle, tabs and cells select, text fields take a value, tree
/// items invoke, select and (when they can) expand, and every one of them takes the focus.
/// Containers, headings and live text offer nothing.
pub fn default_actions(node: &AccessNode) -> AccessActions {
    let focus_only = AccessActions {
        focus: true,
        ..AccessActions::default()
    };
    match node.role {
        AccessRole::Window
        | AccessRole::Group
        | AccessRole::Heading
        | AccessRole::TabList
        | AccessRole::TabPanel
        | AccessRole::Status
        | AccessRole::Alert
        | AccessRole::Toolbar
        | AccessRole::Tree
        | AccessRole::Grid
        | AccessRole::ColumnHeader => AccessActions::default(),
        AccessRole::Button | AccessRole::Link => AccessActions {
            invoke: true,
            ..focus_only
        },
        AccessRole::CheckBox => AccessActions {
            toggle: true,
            ..focus_only
        },
        AccessRole::Tab | AccessRole::Cell => AccessActions {
            select: true,
            ..focus_only
        },
        AccessRole::TextField | AccessRole::TextArea => AccessActions {
            set_value: !node.state.read_only,
            ..focus_only
        },
        AccessRole::ComboBox => AccessActions {
            expand_collapse: true,
            ..focus_only
        },
        AccessRole::TreeItem => AccessActions {
            invoke: true,
            select: true,
            expand_collapse: node.state.expanded.is_some(),
            ..focus_only
        },
    }
}

/// Gives every node that offers no action yet its role's [`default_actions`].
pub fn fill_default_actions(nodes: &mut [AccessNode]) {
    for node in nodes {
        if node.actions == AccessActions::default() {
            node.actions = default_actions(node);
        }
    }
}

/// The caret and selection of a field whose caret is `caret` with the selection `selection`
/// (start before end), in characters.
pub fn field_caret(caret: usize, selection: Option<(usize, usize)>) -> AccessCaret {
    match selection {
        Some((start, end)) if start == caret => AccessCaret { anchor: end, caret },
        Some((start, _)) => AccessCaret {
            anchor: start,
            caret,
        },
        None => AccessCaret {
            anchor: caret,
            caret,
        },
    }
}

/// The character offset in `lines` joined with `\n` of `line` and `column`, clamped to the text.
pub fn char_offset(lines: &[String], line: usize, column: usize) -> usize {
    let line = line.min(lines.len().saturating_sub(1));
    let before: usize = lines
        .iter()
        .take(line)
        .map(|text| text.chars().count() + 1)
        .sum();
    let length = lines.get(line).map_or(0, |text| text.chars().count());
    before + column.min(length)
}

/// The line and column of character `offset` in `lines` joined with `\n`, clamped to the text.
pub fn line_and_column(lines: &[String], offset: usize) -> (usize, usize) {
    let mut left = offset;
    for (line, text) in lines.iter().enumerate() {
        let length = text.chars().count();
        if left <= length {
            return (line, left);
        }
        left -= length + 1;
    }
    let last = lines.len().saturating_sub(1);
    (last, lines.get(last).map_or(0, |text| text.chars().count()))
}

fn plain(key: Key) -> KeyPress {
    KeyPress::plain(key)
}

fn chord(key: Key, modifiers: Modifiers) -> KeyPress {
    KeyPress { key, modifiers }
}

/// The primary modifier of this system: Command on macOS, Control elsewhere.
pub fn primary() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers {
            command: true,
            ..Modifiers::default()
        }
    } else {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }
}

fn shifted() -> Modifiers {
    Modifiers {
        shift: true,
        ..Modifiers::default()
    }
}

/// The keys that replace a field's text with `text` once it has the focus: select all, then
/// type it (`Backspace` when it is empty). Line breaks become `Enter` in a text area and are
/// dropped in a single-line field, as are other control characters.
pub fn typing(text: &str, multi_line: bool) -> Vec<KeyPress> {
    let mut keys = vec![chord(Key::Character('a'), primary())];
    if text.is_empty() {
        keys.push(plain(Key::Backspace));
    }
    for character in text.chars().take(MAX_ACCESS_VALUE_CHARS) {
        match character {
            '\n' if multi_line => keys.push(plain(Key::Enter)),
            character if character.is_control() => {}
            character => keys.push(plain(Key::Character(character))),
        }
    }
    keys
}

/// The keys that put a single-line field's caret at `caret.caret` with the selection from
/// `caret.anchor`, once it has the focus and holds `length` characters.
pub fn caret_keys(caret: AccessCaret, length: usize) -> Vec<KeyPress> {
    let anchor = caret.anchor.min(length);
    let to = caret.caret.min(length);
    let mut keys = vec![plain(Key::Home)];
    keys.extend(std::iter::repeat_n(plain(Key::Right), anchor));
    if to > anchor {
        keys.extend(std::iter::repeat_n(chord(Key::Right, shifted()), to - anchor));
    } else {
        keys.extend(std::iter::repeat_n(chord(Key::Left, shifted()), anchor - to));
    }
    keys
}

/// The keys that do `action` on `node` once it has the focus, as a person would press them:
/// `Space` toggles and selects, `Enter` invokes (`Space` on a checkbox), `Right` and `Left`
/// expand and collapse a tree item, `Space` and `Esc` open and close a combo box's menu.
/// Focusing needs no key; values and carets come from [`typing`] and [`caret_keys`].
pub fn action_keys(node: &AccessNode, action: &AccessAction) -> Vec<KeyPress> {
    match (action, node.role) {
        (AccessAction::Focus, _) => Vec::new(),
        (AccessAction::Invoke | AccessAction::Toggle, AccessRole::CheckBox) => {
            vec![plain(Key::Space)]
        }
        (AccessAction::Invoke, AccessRole::ComboBox) => vec![plain(Key::Space)],
        (AccessAction::Invoke, _) => vec![plain(Key::Enter)],
        (AccessAction::Toggle, _) => vec![plain(Key::Space)],
        (AccessAction::Expand, AccessRole::ComboBox) if node.state.expanded != Some(true) => {
            vec![plain(Key::Space)]
        }
        (AccessAction::Collapse, AccessRole::ComboBox) if node.state.expanded == Some(true) => {
            vec![plain(Key::Escape)]
        }
        (AccessAction::Expand, AccessRole::TreeItem) if node.state.expanded == Some(false) => {
            vec![plain(Key::Right)]
        }
        (AccessAction::Collapse, AccessRole::TreeItem) if node.state.expanded == Some(true) => {
            vec![plain(Key::Left)]
        }
        (AccessAction::Expand | AccessAction::Collapse, _) => Vec::new(),
        (AccessAction::Select, AccessRole::TreeItem) => vec![plain(Key::Space)],
        (AccessAction::Select, _) => Vec::new(),
        (AccessAction::SetValue(text), role) => typing(text, role == AccessRole::TextArea),
        (AccessAction::SetCaret(caret), _) => caret_keys(
            *caret,
            node.value.as_deref().map_or(0, |value| value.chars().count()),
        ),
    }
}

fn role_word(role: AccessRole) -> &'static str {
    match role {
        AccessRole::Window => "window",
        AccessRole::Group => "group",
        AccessRole::Heading => "heading",
        AccessRole::TabList => "tablist",
        AccessRole::Tab => "tab",
        AccessRole::TabPanel => "tabpanel",
        AccessRole::Status => "status",
        AccessRole::TextField => "textfield",
        AccessRole::Button => "button",
        AccessRole::CheckBox => "checkbox",
        AccessRole::Link => "link",
        AccessRole::Toolbar => "toolbar",
        AccessRole::ComboBox => "combobox",
        AccessRole::TextArea => "textarea",
        AccessRole::Alert => "alert",
        AccessRole::Tree => "tree",
        AccessRole::TreeItem => "treeitem",
        AccessRole::Grid => "grid",
        AccessRole::ColumnHeader => "columnheader",
        AccessRole::Cell => "cell",
    }
}

fn states_text(node: &AccessNode) -> Vec<&'static str> {
    let state = node.state;
    let mut words = Vec::new();
    for (on, word) in [
        (state.focused, "focused"),
        (state.selected, "selected"),
        (state.checked == Some(true), "checked"),
        (state.checked == Some(false), "unchecked"),
        (state.expanded == Some(true), "expanded"),
        (state.expanded == Some(false), "collapsed"),
        (state.disabled, "disabled"),
        (state.invalid, "invalid"),
        (state.read_only, "read-only"),
        (state.default_button, "default"),
    ] {
        if on {
            words.push(word);
        }
    }
    words
}

fn actions_text(actions: AccessActions) -> Vec<&'static str> {
    [
        (actions.invoke, "invoke"),
        (actions.toggle, "toggle"),
        (actions.expand_collapse, "expand"),
        (actions.set_value, "set-value"),
        (actions.select, "select"),
        (actions.focus, "focus"),
    ]
    .into_iter()
    .filter_map(|(on, word)| on.then_some(word))
    .collect()
}

fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

fn depth(nodes: &[AccessNode], node: &AccessNode) -> usize {
    let mut depth = 0;
    let mut parent = node.parent;
    while let Some(id) = parent {
        if depth > nodes.len() {
            break;
        }
        depth += 1;
        parent = nodes.iter().find(|other| other.id == id).and_then(|other| other.parent);
    }
    depth
}

/// One line per node, indented two spaces per level: the role, the quoted name, then what a
/// screen reader reads beyond it (value, caret, description, states, actions, grid size, cell
/// and tree position, shortcut). Bounds are left out so layout changes do not move it.
pub fn describe(nodes: &[AccessNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        out.push_str(&"  ".repeat(depth(nodes, node)));
        out.push_str(role_word(node.role));
        out.push(' ');
        out.push_str(&quoted(&node.name));
        if let Some(value) = &node.value {
            out.push_str(" value=");
            out.push_str(&quoted(value));
        }
        if let Some(caret) = node.caret {
            out.push_str(&format!(" caret={}..{}", caret.anchor, caret.caret));
        }
        if let Some(description) = &node.description {
            out.push_str(" description=");
            out.push_str(&quoted(description));
        }
        let states = states_text(node);
        if !states.is_empty() {
            out.push_str(&format!(" [{}]", states.join(" ")));
        }
        let actions = actions_text(node.actions);
        if !actions.is_empty() {
            out.push_str(&format!(" actions={}", actions.join(",")));
        }
        if let Some(table) = node.table {
            out.push_str(&format!(" table={}x{}", table.rows, table.columns));
        }
        if let Some(cell) = node.cell {
            out.push_str(&format!(" cell={},{}", cell.row, cell.column));
        }
        if let Some(at) = node.tree_position {
            out.push_str(&format!(" level={} {}/{}", at.level, at.position, at.set_size));
        }
        if let Some(shortcut) = &node.shortcut {
            out.push_str(&format!(" shortcut={shortcut}"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_platform::window::AccessState;

    fn node(id: u32, role: AccessRole) -> AccessNode {
        AccessNode {
            id,
            parent: (id > 1).then_some(1),
            role,
            name: format!("node {id}"),
            ..AccessNode::default()
        }
    }

    #[test]
    fn each_role_offers_the_actions_its_keys_do() {
        let button = default_actions(&node(2, AccessRole::Button));
        assert!(button.invoke && button.focus && !button.toggle && !button.set_value);
        assert!(default_actions(&node(2, AccessRole::CheckBox)).toggle);
        assert!(default_actions(&node(2, AccessRole::Tab)).select);
        assert_eq!(default_actions(&node(2, AccessRole::Grid)), AccessActions::default());
        let mut ddl = node(2, AccessRole::TextArea);
        ddl.state.read_only = true;
        assert!(!default_actions(&ddl).set_value && default_actions(&ddl).focus);
        let mut leaf = node(3, AccessRole::TreeItem);
        assert!(!default_actions(&leaf).expand_collapse);
        leaf.state.expanded = Some(false);
        assert!(default_actions(&leaf).expand_collapse);
        let mut nodes = vec![node(1, AccessRole::Window), node(2, AccessRole::Link), leaf];
        nodes[1].actions.focus = true;
        fill_default_actions(&mut nodes);
        assert!(!nodes[1].actions.invoke, "a screen's own choice is kept");
        assert!(nodes[2].actions.invoke && nodes[2].actions.expand_collapse);
    }

    #[test]
    fn actions_become_the_keys_a_person_would_press() {
        let enter = vec![KeyPress::plain(Key::Enter)];
        let space = vec![KeyPress::plain(Key::Space)];
        assert_eq!(action_keys(&node(2, AccessRole::Button), &AccessAction::Invoke), enter);
        assert_eq!(action_keys(&node(2, AccessRole::CheckBox), &AccessAction::Invoke), space);
        assert_eq!(action_keys(&node(2, AccessRole::CheckBox), &AccessAction::Toggle), space);
        assert!(action_keys(&node(2, AccessRole::Button), &AccessAction::Focus).is_empty());
        let mut row = node(3, AccessRole::TreeItem);
        row.state.expanded = Some(false);
        assert_eq!(action_keys(&row, &AccessAction::Expand), vec![KeyPress::plain(Key::Right)]);
        assert!(action_keys(&row, &AccessAction::Collapse).is_empty(), "already collapsed");
        row.state.expanded = Some(true);
        assert_eq!(action_keys(&row, &AccessAction::Collapse), vec![KeyPress::plain(Key::Left)]);
        assert_eq!(action_keys(&row, &AccessAction::Select), space);
        let mut pill = node(4, AccessRole::ComboBox);
        assert_eq!(action_keys(&pill, &AccessAction::Expand), space);
        pill.state.expanded = Some(true);
        assert_eq!(action_keys(&pill, &AccessAction::Collapse), vec![KeyPress::plain(Key::Escape)]);
    }

    #[test]
    fn values_are_typed_over_a_select_all() {
        let keys = typing("a\nb\t", false);
        assert_eq!(keys[0], KeyPress { key: Key::Character('a'), modifiers: primary() });
        assert_eq!(
            keys[1..],
            [KeyPress::plain(Key::Character('a')), KeyPress::plain(Key::Character('b'))]
        );
        let area = typing("a\nb", true);
        assert_eq!(area[2], KeyPress::plain(Key::Enter));
        assert_eq!(typing("", false)[1], KeyPress::plain(Key::Backspace));
        let mut field = node(5, AccessRole::TextField);
        field.value = Some("shop".to_owned());
        let keys = action_keys(&field, &AccessAction::SetCaret(AccessCaret { anchor: 1, caret: 9 }));
        assert_eq!(keys.len(), 1 + 1 + 3, "Home, one Right, then Shift+Right up to the end");
        assert_eq!(keys[2].modifiers, shifted());
        let back = caret_keys(AccessCaret { anchor: 3, caret: 1 }, 4);
        assert_eq!(back.iter().filter(|key| key.key == Key::Left).count(), 2);
    }

    #[test]
    fn offsets_and_carets_count_characters() {
        let lines = vec!["SELECT é".to_owned(), "FROM t".to_owned()];
        assert_eq!(char_offset(&lines, 1, 4), 13);
        assert_eq!(char_offset(&lines, 9, 99), 15);
        assert_eq!(line_and_column(&lines, 13), (1, 4));
        assert_eq!(line_and_column(&lines, 8), (0, 8));
        assert_eq!(line_and_column(&lines, 500), (1, 6));
        assert_eq!(field_caret(4, None), AccessCaret { anchor: 4, caret: 4 });
        assert_eq!(field_caret(0, Some((0, 4))), AccessCaret { anchor: 4, caret: 0 });
        assert_eq!(field_caret(4, Some((0, 4))), AccessCaret { anchor: 0, caret: 4 });
    }

    #[test]
    fn the_dump_shows_values_states_and_structure() {
        let mut field = node(2, AccessRole::TextField);
        field.name = "File".to_owned();
        field.value = Some("say \"hi\"".to_owned());
        field.caret = Some(AccessCaret { anchor: 0, caret: 3 });
        field.state = AccessState {
            focused: true,
            invalid: true,
            ..AccessState::default()
        };
        let mut grid = node(3, AccessRole::Grid);
        grid.table = Some(eggdb_platform::window::AccessTable { rows: 3, columns: 2 });
        let mut cell = node(4, AccessRole::Cell);
        cell.parent = Some(3);
        cell.cell = Some(eggdb_platform::window::AccessCell { row: 1, column: 0 });
        let mut nodes = vec![node(1, AccessRole::Window), field, grid, cell];
        fill_default_actions(&mut nodes);
        assert_eq!(
            describe(&nodes),
            "window \"node 1\"\n\
             \x20 textfield \"File\" value=\"say \\\"hi\\\"\" caret=0..3 [focused invalid] actions=set-value,focus\n\
             \x20 grid \"node 3\" table=3x2\n\
             \x20   cell \"node 4\" actions=select,focus cell=1,0\n"
        );
    }
}
