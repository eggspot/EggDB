use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, AccessState};

use super::server_segment::{
    DATABASE_INPUT, DEFAULT_HOST, DEFAULT_SERVER_USER, HOST_INPUT, PASSWORD_INPUT, PORT_INPUT,
    PROTOCOL_MENU, Segment, TLS_WARNING, TLS_WARNING_AT, TLS_WARNING_WIDTH, URL_INPUT,
    URL_PLACEHOLDER, USE_TLS_BOX, USE_TLS_LABEL, USER_INPUT, tls_warning_shown,
};
use super::{
    BODY, BROWSE, CANCEL, CARD, CHECK_LABEL_X, CONNECT_TO_A_SERVER, CONNECT_TO_A_SERVER_LABEL,
    CREATE_BOX, CREATE_LABEL, ConnectCard, ERROR_TOP, Focus, HEADLINE, OPEN, OPEN_A_FILE,
    OPEN_A_FILE_LABEL, PLACEHOLDER, READ_ONLY_BOX, READ_ONLY_LABEL, SEGMENTS, STATUS_LINE, TITLE,
    TextField, card_origin, down, file_input, link_rects, offset, panel_shift,
};
use crate::canvas::Rect;
use crate::main_window::Faces;

/// The window's node.
pub const NODE_WINDOW: u32 = 1;
/// The card, a group named by its title.
pub const NODE_CARD: u32 = 2;
/// The heading `Connect`.
pub const NODE_TITLE: u32 = 3;
/// The segmented control, a tab list.
pub const NODE_SEGMENTS: u32 = 4;
/// The tab `Open a file`.
pub const NODE_OPEN_A_FILE: u32 = 5;
/// The tab `Connect to a server`.
pub const NODE_CONNECT_TO_A_SERVER: u32 = 6;
/// The active segment's panel.
pub const NODE_PANEL: u32 = 7;
/// The `Error:` line, present only while an error is shown.
pub const NODE_ERROR: u32 = 8;
/// The File input.
pub const NODE_FILE: u32 = 9;
/// `Browse…`.
pub const NODE_BROWSE: u32 = 10;
/// The checkbox `Create the file if it does not exist`.
pub const NODE_CREATE_IF_MISSING: u32 = 11;
/// The checkbox `Open read-only`.
pub const NODE_READ_ONLY: u32 = 12;
/// `Cancel`.
pub const NODE_CANCEL: u32 = 13;
/// `Open`.
pub const NODE_OPEN: u32 = 14;
/// The link `Where are you coming from?`.
pub const NODE_WHERE_FROM: u32 = 15;
/// The link `Load a sample dataset`.
pub const NODE_SAMPLE: u32 = 16;
/// The progress or status line (`Opening shop.eggdb…`, `Cancelled`), present only while one
/// is shown; announced politely.
pub const NODE_STATUS_LINE: u32 = 17;
/// The server segment's `URL` input.
pub const NODE_URL: u32 = 18;
/// The protocol menu, its value the chosen protocol.
pub const NODE_PROTOCOL: u32 = 19;
/// The Host input.
pub const NODE_HOST: u32 = 20;
/// The Port input.
pub const NODE_PORT: u32 = 21;
/// The Database input.
pub const NODE_DATABASE: u32 = 22;
/// The User input.
pub const NODE_USER: u32 = 23;
/// The Password input, its value masked.
pub const NODE_PASSWORD: u32 = 24;
/// The checkbox `Use TLS`.
pub const NODE_USE_TLS: u32 = 25;
/// The warning under `Use TLS` while it is unchecked; announced politely.
pub const NODE_TLS_WARNING: u32 = 26;

const CARD_NAME: &str = "Connect";
const SEGMENTS_NAME: &str = "Connection kind";
const FILE_NAME: &str = "File";
const BROWSE_NAME: &str = "Browse";
const CANCEL_NAME: &str = "Cancel";
const OPEN_NAME: &str = "Open";
const CONNECT_NAME: &str = "Connect";
const MASK: char = '●';
const TITLE_AT: (f32, f32) = (48.0, 24.0);
const PANEL_LEFT: f32 = 24.0;
const PANEL_WIDTH: f32 = 512.0;
const STATUS_HEIGHT: f32 = 18.0;

fn access_rect(rect: Rect) -> AccessRect {
    AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    }
}

struct TreeBuilder {
    nodes: Vec<AccessNode>,
    focused: Option<u32>,
}

impl TreeBuilder {
    fn add(&mut self, id: u32, parent: Option<u32>, role: AccessRole, name: &str, rect: Rect) {
        self.add_with(id, parent, role, name, rect, AccessState::default());
    }

    fn add_with(
        &mut self,
        id: u32,
        parent: Option<u32>,
        role: AccessRole,
        name: &str,
        rect: Rect,
        state: AccessState,
    ) {
        self.nodes.push(AccessNode {
            id,
            parent,
            role,
            name: name.to_owned(),
            state: AccessState {
                focused: self.focused == Some(id),
                ..state
            },
            bounds: access_rect(rect),
            ..AccessNode::default()
        });
    }
}

fn focused_node_id(card: &ConnectCard) -> u32 {
    match card.focus {
        Focus::Segments => match card.segment {
            Segment::File => NODE_OPEN_A_FILE,
            Segment::Server => NODE_CONNECT_TO_A_SERVER,
        },
        focus => focus_node_id(focus),
    }
}

fn focus_node_id(focus: Focus) -> u32 {
    match focus {
        Focus::Segments => NODE_SEGMENTS,
        Focus::FileInput => NODE_FILE,
        Focus::Browse => NODE_BROWSE,
        Focus::CreateIfMissing => NODE_CREATE_IF_MISSING,
        Focus::ReadOnly => NODE_READ_ONLY,
        Focus::Cancel => NODE_CANCEL,
        Focus::Open => NODE_OPEN,
        Focus::WhereFromLink => NODE_WHERE_FROM,
        Focus::SampleLink => NODE_SAMPLE,
        Focus::Url => NODE_URL,
        Focus::Protocol => NODE_PROTOCOL,
        Focus::Host => NODE_HOST,
        Focus::Port => NODE_PORT,
        Focus::Database => NODE_DATABASE,
        Focus::User => NODE_USER,
        Focus::Password => NODE_PASSWORD,
        Focus::UseTls => NODE_USE_TLS,
    }
}

/// The error lines as one announcement: the `Error` line, then the `Try:` line when there is
/// one.
pub fn error_announcement(card: &ConnectCard) -> Option<String> {
    card.error
        .as_ref()
        .map(|error| error.lines().collect::<Vec<_>>().join("\n"))
}

/// The connect card's accessibility tree for a window of `size` logical pixels, in the order
/// and with the roles, names and states of `docs/ux/first-screen.md`'s screen-reader table; the
/// egg mark is decorative and absent. Bounds are logical pixels from the window's top-left.
pub fn accessibility_tree(
    card: &ConnectCard,
    faces: Faces<'_>,
    size: (u32, u32),
) -> Vec<AccessNode> {
    let origin = card_origin(size.0, size.1);
    let shift = panel_shift(card);
    let at = |rect: Rect| offset(rect, origin);
    let mut tree = TreeBuilder {
        nodes: Vec::with_capacity(16),
        focused: Some(focused_node_id(card)),
    };
    let window = Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32);
    tree.add(NODE_WINDOW, None, AccessRole::Window, TITLE, window);
    tree.add(
        NODE_CARD,
        Some(NODE_WINDOW),
        AccessRole::Group,
        CARD_NAME,
        at(CARD),
    );
    let title_width = faces.ui_bold.width(CARD_NAME, HEADLINE.0);
    tree.add(
        NODE_TITLE,
        Some(NODE_CARD),
        AccessRole::Heading,
        CARD_NAME,
        at(Rect::new(TITLE_AT.0, TITLE_AT.1, title_width, HEADLINE.1)),
    );
    tree.add(
        NODE_SEGMENTS,
        Some(NODE_CARD),
        AccessRole::TabList,
        SEGMENTS_NAME,
        at(SEGMENTS),
    );
    let server = card.segment == Segment::Server;
    for (id, label, rect, selected) in [
        (NODE_OPEN_A_FILE, OPEN_A_FILE_LABEL, OPEN_A_FILE, !server),
        (
            NODE_CONNECT_TO_A_SERVER,
            CONNECT_TO_A_SERVER_LABEL,
            CONNECT_TO_A_SERVER,
            server,
        ),
    ] {
        let state = AccessState {
            selected,
            ..AccessState::default()
        };
        tree.add_with(
            id,
            Some(NODE_SEGMENTS),
            AccessRole::Tab,
            label,
            at(rect),
            state,
        );
    }
    let panel_bottom = if server {
        if tls_warning_shown(card) {
            TLS_WARNING_AT.1 + BODY.1 + shift
        } else {
            USE_TLS_BOX.y + USE_TLS_BOX.h + shift
        }
    } else {
        READ_ONLY_BOX.y + READ_ONLY_BOX.h + shift
    };
    let panel_name = if server {
        CONNECT_TO_A_SERVER_LABEL
    } else {
        OPEN_A_FILE_LABEL
    };
    tree.add(
        NODE_PANEL,
        Some(NODE_CARD),
        AccessRole::TabPanel,
        panel_name,
        at(Rect::new(
            PANEL_LEFT,
            ERROR_TOP,
            PANEL_WIDTH,
            panel_bottom - ERROR_TOP,
        )),
    );
    if let Some(announcement) = error_announcement(card) {
        tree.add(
            NODE_ERROR,
            Some(NODE_PANEL),
            AccessRole::Status,
            &announcement,
            at(Rect::new(PANEL_LEFT, ERROR_TOP, PANEL_WIDTH, shift)),
        );
    }
    if server {
        server_nodes(&mut tree, card, faces, &at, shift);
    } else {
        file_nodes(&mut tree, card, faces, &at, shift);
    }
    let cancel = AccessState {
        disabled: !card.can_cancel && !card.disabled(),
        ..AccessState::default()
    };
    tree.add_with(
        NODE_CANCEL,
        Some(NODE_CARD),
        AccessRole::Button,
        CANCEL_NAME,
        at(CANCEL),
        cancel,
    );
    let open = AccessState {
        default_button: true,
        ..AccessState::default()
    };
    tree.add_with(
        NODE_OPEN,
        Some(NODE_CARD),
        AccessRole::Button,
        if server { CONNECT_NAME } else { OPEN_NAME },
        at(OPEN),
        open,
    );
    if let Some(text) = card.progress.as_ref().or(card.status.as_ref()) {
        tree.add(
            NODE_STATUS_LINE,
            Some(NODE_CARD),
            AccessRole::Status,
            text,
            at(Rect::new(
                STATUS_LINE.0,
                STATUS_LINE.1,
                PANEL_WIDTH,
                STATUS_HEIGHT,
            )),
        );
    }
    for ((rect, label), id) in link_rects(faces, origin)
        .into_iter()
        .zip([NODE_WHERE_FROM, NODE_SAMPLE])
    {
        tree.add(id, Some(NODE_WINDOW), AccessRole::Link, label, rect);
    }
    if let Some(id) = card.failed_focus().map(focus_node_id) {
        for node in tree.nodes.iter_mut().filter(|node| node.id == id) {
            node.state.invalid = true;
        }
    }
    if let Some(node) = tree.nodes.iter_mut().find(|node| node.id == NODE_PROTOCOL) {
        node.state.expanded = Some(false);
    }
    crate::accessibility::fill_default_actions(&mut tree.nodes);
    if let Some(node) = tree.nodes.iter_mut().find(|node| node.id == NODE_OPEN) {
        node.shortcut = Some("Enter".to_owned());
    }
    if card.disabled() {
        for node in &mut tree.nodes {
            let control = matches!(
                node.role,
                AccessRole::Tab
                    | AccessRole::TextField
                    | AccessRole::ComboBox
                    | AccessRole::Button
                    | AccessRole::CheckBox
                    | AccessRole::Link
            );
            if control && node.id != NODE_CANCEL {
                node.state.disabled = true;
            }
        }
    }
    tree.nodes
}

fn text_field(
    tree: &mut TreeBuilder,
    id: u32,
    name: &str,
    rect: Rect,
    field: (String, &TextField),
    placeholder: &str,
) {
    let (value, text) = field;
    tree.add(id, Some(NODE_PANEL), AccessRole::TextField, name, rect);
    if let Some(node) = tree.nodes.last_mut() {
        node.value = Some(value);
        node.placeholder = (!placeholder.is_empty()).then(|| placeholder.to_owned());
        node.caret = Some(crate::accessibility::field_caret(
            text.caret(),
            text.selection(),
        ));
    }
}

struct CheckBoxRow<'l> {
    id: u32,
    label: &'l str,
    rect: Rect,
    checked: bool,
}

fn check_box_row(
    tree: &mut TreeBuilder,
    faces: Faces<'_>,
    at: &dyn Fn(Rect) -> Rect,
    row: CheckBoxRow<'_>,
) {
    let label_width = faces.ui.width(row.label, BODY.0);
    let rect = row.rect;
    let bounds = Rect::new(rect.x, rect.y, CHECK_LABEL_X - rect.x + label_width, rect.h);
    let state = AccessState {
        checked: Some(row.checked),
        ..AccessState::default()
    };
    tree.add_with(
        row.id,
        Some(NODE_PANEL),
        AccessRole::CheckBox,
        row.label,
        at(bounds),
        state,
    );
}

fn server_nodes(
    tree: &mut TreeBuilder,
    card: &ConnectCard,
    faces: Faces<'_>,
    at: &dyn Fn(Rect) -> Rect,
    shift: f32,
) {
    let form = &card.server;
    let plain = |field: &TextField| field.value().to_owned();
    text_field(
        tree,
        NODE_URL,
        "URL",
        at(down(URL_INPUT, shift)),
        (plain(&form.url), &form.url),
        URL_PLACEHOLDER,
    );
    tree.add(
        NODE_PROTOCOL,
        Some(NODE_PANEL),
        AccessRole::ComboBox,
        "Protocol",
        at(down(PROTOCOL_MENU, shift)),
    );
    if let Some(node) = tree.nodes.last_mut() {
        node.value = Some(form.protocol.label().to_owned());
    }
    for (id, name, rect, field, placeholder) in [
        (NODE_HOST, "Host", HOST_INPUT, &form.host, DEFAULT_HOST),
        (NODE_PORT, "Port", PORT_INPUT, &form.port, ""),
        (
            NODE_DATABASE,
            "Database",
            DATABASE_INPUT,
            &form.database,
            "",
        ),
        (
            NODE_USER,
            "User",
            USER_INPUT,
            &form.user,
            DEFAULT_SERVER_USER,
        ),
        (
            NODE_PASSWORD,
            "Password",
            PASSWORD_INPUT,
            &form.password,
            "",
        ),
    ] {
        let value = if id == NODE_PASSWORD {
            std::iter::repeat_n(MASK, field.value().chars().count()).collect()
        } else {
            plain(field)
        };
        text_field(
            tree,
            id,
            name,
            at(down(rect, shift)),
            (value, field),
            placeholder,
        );
    }
    let tls_row = CheckBoxRow {
        id: NODE_USE_TLS,
        label: USE_TLS_LABEL,
        rect: down(USE_TLS_BOX, shift),
        checked: form.use_tls,
    };
    check_box_row(tree, faces, at, tls_row);
    if tls_warning_shown(card) {
        let width = faces.ui.width(TLS_WARNING, BODY.0).min(TLS_WARNING_WIDTH);
        let rect = Rect::new(TLS_WARNING_AT.0, TLS_WARNING_AT.1 + shift, width, BODY.1);
        tree.add(
            NODE_TLS_WARNING,
            Some(NODE_PANEL),
            AccessRole::Status,
            TLS_WARNING,
            at(rect),
        );
    }
}

fn file_nodes(
    tree: &mut TreeBuilder,
    card: &ConnectCard,
    faces: Faces<'_>,
    at: &dyn Fn(Rect) -> Rect,
    shift: f32,
) {
    text_field(
        tree,
        NODE_FILE,
        FILE_NAME,
        at(down(file_input(card), shift)),
        (card.file.value().to_owned(), &card.file),
        PLACEHOLDER,
    );
    if card.file_dialog {
        tree.add(
            NODE_BROWSE,
            Some(NODE_PANEL),
            AccessRole::Button,
            BROWSE_NAME,
            at(down(BROWSE, shift)),
        );
    }
    for (id, label, rect, checked) in [
        (
            NODE_CREATE_IF_MISSING,
            CREATE_LABEL,
            CREATE_BOX,
            card.create_if_missing,
        ),
        (
            NODE_READ_ONLY,
            READ_ONLY_LABEL,
            READ_ONLY_BOX,
            card.read_only,
        ),
    ] {
        let row = CheckBoxRow {
            id,
            label,
            rect: down(rect, shift),
            checked,
        };
        check_box_row(tree, faces, at, row);
    }
}

impl ConnectCard {
    /// Moves the keyboard focus to the element of accessibility node `id`, as when a screen
    /// reader or a click focuses it; returns whether the node can take the focus (a disabled
    /// `Cancel`, a group or a heading cannot).
    pub fn focus_node(&mut self, id: u32) -> bool {
        let file = self.segment == Segment::File;
        let focus = match id {
            NODE_OPEN_A_FILE => {
                self.show_segment(Segment::File);
                Focus::Segments
            }
            NODE_CONNECT_TO_A_SERVER => {
                self.show_segment(Segment::Server);
                Focus::Segments
            }
            NODE_SEGMENTS => Focus::Segments,
            NODE_FILE if file => Focus::FileInput,
            NODE_BROWSE if file && self.file_dialog => Focus::Browse,
            NODE_CREATE_IF_MISSING if file => Focus::CreateIfMissing,
            NODE_READ_ONLY if file => Focus::ReadOnly,
            NODE_URL if !file => Focus::Url,
            NODE_PROTOCOL if !file => Focus::Protocol,
            NODE_HOST if !file => Focus::Host,
            NODE_PORT if !file => Focus::Port,
            NODE_DATABASE if !file => Focus::Database,
            NODE_USER if !file => Focus::User,
            NODE_PASSWORD if !file => Focus::Password,
            NODE_USE_TLS if !file => Focus::UseTls,
            NODE_CANCEL if self.can_cancel => Focus::Cancel,
            NODE_OPEN => Focus::Open,
            NODE_WHERE_FROM => Focus::WhereFromLink,
            NODE_SAMPLE => Focus::SampleLink,
            _ => return false,
        };
        self.focus = focus;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect_screen::{
        CardError, ConnectAction, DEFAULT_HEIGHT, DEFAULT_WIDTH, handle_key,
    };
    use crate::font::BoxFace;
    use eggdb_platform::window::{Key, KeyPress, focused_node};

    const BOXES: Faces<'static> = Faces {
        ui: &BoxFace,
        ui_bold: &BoxFace,
        mono: &BoxFace,
    };
    const SIZE: (u32, u32) = (DEFAULT_WIDTH, DEFAULT_HEIGHT);

    fn tree(card: &ConnectCard) -> Vec<AccessNode> {
        accessibility_tree(card, BOXES, SIZE)
    }

    fn node(nodes: &[AccessNode], id: u32) -> &AccessNode {
        match nodes.iter().find(|node| node.id == id) {
            Some(node) => node,
            None => panic!("node {id} is missing"),
        }
    }

    #[test]
    fn the_first_start_tree_has_the_spec_table_nodes_in_order() {
        let nodes = tree(&ConnectCard::first_start());
        let summary: Vec<(u32, Option<u32>, AccessRole, &str)> = nodes
            .iter()
            .map(|node| (node.id, node.parent, node.role, node.name.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (NODE_WINDOW, None, AccessRole::Window, "EggDB"),
                (NODE_CARD, Some(NODE_WINDOW), AccessRole::Group, "Connect"),
                (NODE_TITLE, Some(NODE_CARD), AccessRole::Heading, "Connect"),
                (
                    NODE_SEGMENTS,
                    Some(NODE_CARD),
                    AccessRole::TabList,
                    "Connection kind"
                ),
                (
                    NODE_OPEN_A_FILE,
                    Some(NODE_SEGMENTS),
                    AccessRole::Tab,
                    "Open a file"
                ),
                (
                    NODE_CONNECT_TO_A_SERVER,
                    Some(NODE_SEGMENTS),
                    AccessRole::Tab,
                    "Connect to a server"
                ),
                (
                    NODE_PANEL,
                    Some(NODE_CARD),
                    AccessRole::TabPanel,
                    "Open a file"
                ),
                (NODE_FILE, Some(NODE_PANEL), AccessRole::TextField, "File"),
                (NODE_BROWSE, Some(NODE_PANEL), AccessRole::Button, "Browse"),
                (
                    NODE_CREATE_IF_MISSING,
                    Some(NODE_PANEL),
                    AccessRole::CheckBox,
                    "Create the file if it does not exist"
                ),
                (
                    NODE_READ_ONLY,
                    Some(NODE_PANEL),
                    AccessRole::CheckBox,
                    "Open read-only"
                ),
                (NODE_CANCEL, Some(NODE_CARD), AccessRole::Button, "Cancel"),
                (NODE_OPEN, Some(NODE_CARD), AccessRole::Button, "Open"),
                (
                    NODE_WHERE_FROM,
                    Some(NODE_WINDOW),
                    AccessRole::Link,
                    "Where are you coming from?"
                ),
                (
                    NODE_SAMPLE,
                    Some(NODE_WINDOW),
                    AccessRole::Link,
                    "Load a sample dataset"
                ),
            ]
        );
        let file = node(&nodes, NODE_FILE);
        assert_eq!(file.value.as_deref(), Some("~/eggdb/quickstart.eggdb"));
        assert_eq!(file.placeholder.as_deref(), Some("/path/to/shop.eggdb"));
        assert!(file.state.focused);
        assert_eq!(focused_node(&nodes), Some(NODE_FILE));
        assert_eq!(nodes.iter().filter(|node| node.state.focused).count(), 1);
        assert!(node(&nodes, NODE_OPEN_A_FILE).state.selected);
        assert!(!node(&nodes, NODE_CONNECT_TO_A_SERVER).state.selected);
        assert_eq!(
            node(&nodes, NODE_CREATE_IF_MISSING).state.checked,
            Some(true)
        );
        assert_eq!(node(&nodes, NODE_READ_ONLY).state.checked, Some(false));
        assert_eq!(node(&nodes, NODE_BROWSE).state.checked, None);
        assert!(node(&nodes, NODE_CANCEL).state.disabled);
        assert!(node(&nodes, NODE_OPEN).state.default_button);
        let ids: Vec<u32> = nodes.iter().map(|node| node.id).collect();
        for node in &nodes {
            if let Some(parent) = node.parent {
                let parent_at = ids.iter().position(|&id| id == parent);
                let own_at = ids.iter().position(|&id| id == node.id);
                assert!(
                    parent_at < own_at,
                    "node {} comes after its parent",
                    node.id
                );
            }
        }
    }

    #[test]
    fn the_first_start_dump_shows_values_carets_states_and_actions() {
        let expected = "\
window \"EggDB\"
  group \"Connect\"
    heading \"Connect\"
    tablist \"Connection kind\"
      tab \"Open a file\" [selected] actions=select,focus
      tab \"Connect to a server\" actions=select,focus
    tabpanel \"Open a file\"
      textfield \"File\" value=\"~/eggdb/quickstart.eggdb\" caret=0..24 [focused] actions=set-value,focus
      button \"Browse\" actions=invoke,focus
      checkbox \"Create the file if it does not exist\" [checked] actions=toggle,focus
      checkbox \"Open read-only\" [unchecked] actions=toggle,focus
    button \"Cancel\" [disabled] actions=invoke,focus
    button \"Open\" [default] actions=invoke,focus shortcut=Enter
  link \"Where are you coming from?\" actions=invoke,focus
  link \"Load a sample dataset\" actions=invoke,focus
";
        assert_eq!(
            crate::accessibility::describe(&tree(&ConnectCard::first_start())),
            expected
        );
    }

    #[test]
    fn the_server_segment_dump_shows_the_combo_box_and_the_failed_field() {
        let mut card = ConnectCard::first_start();
        assert!(card.focus_node(NODE_CONNECT_TO_A_SERVER));
        let nodes = tree(&card);
        let protocol = node(&nodes, NODE_PROTOCOL);
        assert_eq!(protocol.role, AccessRole::ComboBox);
        assert_eq!(protocol.state.expanded, Some(false));
        assert!(protocol.actions.expand_collapse && protocol.actions.focus);
        let url = node(&nodes, NODE_URL);
        assert!(url.actions.set_value);
        assert!(url.caret.is_some());
        let password = node(&nodes, NODE_PASSWORD);
        assert!(
            password
                .value
                .as_deref()
                .is_some_and(|value| value.chars().all(|letter| letter == MASK))
        );
        card.failed_field = Some(crate::connection::FailureField::Host);
        let failed = tree(&card);
        assert!(node(&failed, NODE_HOST).state.invalid);
        assert!(!node(&failed, NODE_URL).state.invalid);
        assert!(crate::accessibility::describe(&failed).contains("textfield \"Host\""));
    }

    #[test]
    fn bounds_are_the_drawn_rects_in_logical_pixels() {
        let nodes = tree(&ConnectCard::first_start());
        let rect = |id: u32| node(&nodes, id).bounds;
        assert_eq!(
            rect(NODE_WINDOW),
            AccessRect {
                x: 0.0,
                y: 0.0,
                width: 1280.0,
                height: 800.0
            }
        );
        assert_eq!(
            rect(NODE_CARD),
            AccessRect {
                x: 360.0,
                y: 140.0,
                width: 560.0,
                height: 480.0
            }
        );
        assert_eq!(
            rect(NODE_FILE),
            AccessRect {
                x: 384.0,
                y: 282.0,
                width: 420.0,
                height: 28.0
            }
        );
        assert_eq!(
            rect(NODE_OPEN),
            AccessRect {
                x: 816.0,
                y: 576.0,
                width: 80.0,
                height: 28.0
            }
        );
        let create = rect(NODE_CREATE_IF_MISSING);
        assert_eq!((create.x, create.y, create.height), (384.0, 326.0, 16.0));
        assert!(create.width > 24.0);
    }

    #[test]
    fn an_error_adds_a_status_node_and_moves_the_panel_down() {
        let mut card = ConnectCard::first_start();
        card.error = Some(CardError {
            line: "Error: the file is locked".to_owned(),
            fix: Some("Try: close the other EggDB".to_owned()),
        });
        let nodes = tree(&card);
        let error = node(&nodes, NODE_ERROR);
        assert_eq!(error.role, AccessRole::Status);
        assert_eq!(error.parent, Some(NODE_PANEL));
        assert_eq!(
            error.name,
            "Error: the file is locked\nTry: close the other EggDB"
        );
        let order: Vec<u32> = nodes.iter().map(|node| node.id).collect();
        assert_eq!(order.get(7..9), Some(&[NODE_ERROR, NODE_FILE][..]));
        assert_eq!(node(&nodes, NODE_FILE).bounds.y, 318.0);
        assert!(
            tree(&ConnectCard::first_start())
                .iter()
                .all(|node| node.id != NODE_ERROR)
        );
    }

    #[test]
    fn the_tree_follows_the_keyboard_focus_and_the_checkboxes() {
        let mut card = ConnectCard::first_start();
        let mut visited = Vec::new();
        for _ in 0..card.focus_order().len() {
            let (next, action) = handle_key(card, KeyPress::plain(Key::Tab));
            assert_eq!(action, ConnectAction::None);
            card = next;
            visited.push(focused_node(&tree(&card)));
        }
        assert_eq!(
            visited,
            [
                NODE_BROWSE,
                NODE_CREATE_IF_MISSING,
                NODE_READ_ONLY,
                NODE_OPEN,
                NODE_WHERE_FROM,
                NODE_SAMPLE,
                NODE_OPEN_A_FILE,
                NODE_FILE
            ]
            .map(Some)
        );
        card.focus = Focus::ReadOnly;
        let (card, _) = handle_key(card, KeyPress::plain(Key::Space));
        assert_eq!(node(&tree(&card), NODE_READ_ONLY).state.checked, Some(true));
    }

    #[test]
    fn focusing_a_node_moves_the_card_focus_except_where_it_cannot_go() {
        let mut card = ConnectCard::first_start();
        for stop in card.focus_order() {
            let id = focused_node_id(&ConnectCard {
                focus: stop,
                ..ConnectCard::first_start()
            });
            assert!(card.focus_node(id), "node {id}");
            assert_eq!(card.focus, stop);
            assert_eq!(focused_node(&tree(&card)), Some(id));
        }
        card.focus = Focus::Open;
        for unfocusable in [
            NODE_WINDOW,
            NODE_CARD,
            NODE_TITLE,
            NODE_PANEL,
            NODE_ERROR,
            NODE_CANCEL,
            99,
        ] {
            assert!(!card.focus_node(unfocusable), "node {unfocusable}");
            assert_eq!(card.focus, Focus::Open);
        }
        card.can_cancel = true;
        assert!(card.focus_node(NODE_CANCEL));
        assert_eq!(card.focus, Focus::Cancel);
        assert!(!node(&tree(&card), NODE_CANCEL).state.disabled);
    }
}
