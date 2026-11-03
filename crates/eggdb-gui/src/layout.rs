use eggdb_platform::window::{Key, KeyPress, Modifiers};

use crate::canvas::Rect;

/// A layout of the main window's regions (`docs/ux/gui-layouts.md`): EggDB's own, or one that
/// works like a client people already know. The names of other products are their owners'
/// trademarks, used only to describe compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// EggDB's own layout (`docs/ux/gui.md`).
    EggDb,
    /// Works like SQL Server Management Studio.
    Ssms,
    /// Works like pgAdmin 4.
    PgAdmin,
    /// Works like DBeaver.
    DBeaver,
    /// Works like DataGrip.
    DataGrip,
    /// Works like MySQL Workbench.
    Workbench,
    /// Works like MongoDB Compass.
    Compass,
    /// Works like Redis Insight.
    RedisInsight,
}

/// Where each region of the main window goes at 1280 × 800; `None` hides it.
#[derive(Debug, Clone, PartialEq)]
pub struct Regions {
    /// The tool bar.
    pub toolbar: Option<Rect>,
    /// The connection and schema tree.
    pub sidebar: Option<Rect>,
    /// The editor's tabs.
    pub tabs: Option<Rect>,
    /// The query editor.
    pub editor: Rect,
    /// The results.
    pub results: Rect,
    /// The status bar.
    pub statusbar: Option<Rect>,
    /// The layout's one extra region, with its name, when shown by default.
    pub extra: Option<(&'static str, Rect)>,
}

const fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::new(x, y, w, h)
}

/// The window width the layouts' rects are given at; a wider window widens the regions that
/// reach its right edge.
pub const DESIGN_WIDTH: f32 = 1280.0;

/// The short form of the trademark note, as the layout picker and the status bar item's
/// tooltip show it.
pub const TRADEMARK_NOTE: &str = "Product names are trademarks of their owners; EggDB is not affiliated with them. A layout copies an arrangement, shortcuts and workflows only.";

/// An action a layout gives keys of its own to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAction {
    /// Run the statement under the caret.
    RunStatement,
    /// Run the selection, or the whole tab when nothing is selected.
    RunAll,
    /// Explain the statement.
    Explain,
}

/// Which control key a shortcut holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// No control key.
    Nothing,
    /// The system's primary key: Command on macOS, Control elsewhere.
    Primary,
    /// Control on every system, as products that never remap it to Command.
    Control,
}

/// One key of a layout's shortcut table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    /// What it does.
    pub action: LayoutAction,
    /// The key, lower case for letters.
    pub key: Key,
    /// The control key it holds.
    pub held: Held,
    /// Whether it holds Shift.
    pub shift: bool,
    /// Whether it holds Alt (Option on macOS).
    pub alt: bool,
}

const fn shortcut(action: LayoutAction, key: Key, held: Held, shift: bool, alt: bool) -> Shortcut {
    Shortcut {
        action,
        key,
        held,
        shift,
        alt,
    }
}

fn primary_modifiers() -> Modifiers {
    crate::accessibility::primary()
}

impl Shortcut {
    /// Whether `press` is this shortcut.
    pub fn matches(&self, press: KeyPress) -> bool {
        let held = press.modifiers;
        let control_keys = match self.held {
            Held::Nothing => (false, false),
            Held::Primary => {
                let primary = primary_modifiers();
                (primary.control, primary.command)
            }
            Held::Control => (true, false),
        };
        let key = match press.key {
            Key::Character(typed) => Key::Character(typed.to_ascii_lowercase()),
            other => other,
        };
        key == self.key
            && (held.control, held.command) == control_keys
            && held.shift == self.shift
            && held.alt == self.alt
    }

    /// How the shortcut reads on this system, as `Ctrl+Shift+E` or `F5`.
    pub fn label(&self) -> String {
        let mac = cfg!(target_os = "macos");
        let mut parts: Vec<String> = Vec::new();
        match self.held {
            Held::Nothing => {}
            Held::Primary if mac => parts.push("Cmd".to_owned()),
            Held::Primary | Held::Control => parts.push("Ctrl".to_owned()),
        }
        if self.alt {
            parts.push(if mac { "Option" } else { "Alt" }.to_owned());
        }
        if self.shift {
            parts.push("Shift".to_owned());
        }
        parts.push(match self.key {
            Key::Enter => "Enter".to_owned(),
            Key::F5 => "F5".to_owned(),
            Key::F4 => "F4".to_owned(),
            Key::F2 => "F2".to_owned(),
            Key::Character(typed) => typed.to_ascii_uppercase().to_string(),
            other => format!("{other:?}"),
        });
        parts.join("+")
    }
}

/// EggDB's own key for `action`, which the main window answers.
pub fn eggdb_key(action: LayoutAction) -> KeyPress {
    let primary = primary_modifiers();
    let (key, shift) = match action {
        LayoutAction::RunStatement => (Key::Enter, false),
        LayoutAction::RunAll => (Key::Enter, true),
        LayoutAction::Explain => (Key::Character('e'), false),
    };
    KeyPress {
        key,
        modifiers: Modifiers { shift, ..primary },
    }
}

const SSMS_KEYS: [Shortcut; 4] = [
    shortcut(LayoutAction::RunAll, Key::F5, Held::Nothing, false, false),
    shortcut(LayoutAction::RunAll, Key::Character('e'), Held::Primary, false, false),
    shortcut(LayoutAction::RunAll, Key::Character('x'), Held::Nothing, false, true),
    shortcut(LayoutAction::Explain, Key::Character('l'), Held::Primary, false, false),
];
const PGADMIN_KEYS: [Shortcut; 1] = [shortcut(
    LayoutAction::RunAll,
    Key::F5,
    Held::Nothing,
    false,
    false,
)];
const DBEAVER_KEYS: [Shortcut; 3] = [
    shortcut(LayoutAction::RunStatement, Key::Enter, Held::Primary, false, false),
    shortcut(LayoutAction::RunAll, Key::Character('x'), Held::Nothing, false, true),
    shortcut(LayoutAction::Explain, Key::Character('e'), Held::Primary, true, false),
];
const DATAGRIP_KEYS: [Shortcut; 1] = [shortcut(
    LayoutAction::RunStatement,
    Key::Enter,
    Held::Primary,
    false,
    false,
)];
const WORKBENCH_KEYS: [Shortcut; 3] = [
    shortcut(LayoutAction::RunStatement, Key::Enter, Held::Primary, false, false),
    shortcut(LayoutAction::RunAll, Key::Enter, Held::Primary, true, false),
    shortcut(LayoutAction::Explain, Key::Character('x'), Held::Primary, false, true),
];
const COMPASS_KEYS: [Shortcut; 1] = [shortcut(
    LayoutAction::RunAll,
    Key::Enter,
    Held::Nothing,
    false,
    false,
)];
const REDIS_INSIGHT_KEYS: [Shortcut; 1] = [shortcut(
    LayoutAction::RunAll,
    Key::Enter,
    Held::Primary,
    false,
    false,
)];

impl Layout {
    /// Every layout, EggDB's first.
    pub const ALL: [Self; 8] = [
        Self::EggDb,
        Self::Ssms,
        Self::PgAdmin,
        Self::DBeaver,
        Self::DataGrip,
        Self::Workbench,
        Self::Compass,
        Self::RedisInsight,
    ];

    /// The label in the layout picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::EggDb => "EggDB",
            Self::Ssms => "Works like SQL Server Management Studio",
            Self::PgAdmin => "Works like pgAdmin 4",
            Self::DBeaver => "Works like DBeaver",
            Self::DataGrip => "Works like DataGrip",
            Self::Workbench => "Works like MySQL Workbench",
            Self::Compass => "Works like MongoDB Compass",
            Self::RedisInsight => "Works like Redis Insight",
        }
    }

    /// The layout's name in the client's settings (`layout = ssms`).
    pub fn key(self) -> &'static str {
        match self {
            Self::EggDb => "eggdb",
            Self::Ssms => "ssms",
            Self::PgAdmin => "pgadmin",
            Self::DBeaver => "dbeaver",
            Self::DataGrip => "datagrip",
            Self::Workbench => "mysql-workbench",
            Self::Compass => "mongodb-compass",
            Self::RedisInsight => "redis-insight",
        }
    }

    /// The layout named `key` in the client's settings, ignoring case and surrounding space.
    pub fn from_key(key: &str) -> Option<Self> {
        let key = key.trim();
        Self::ALL
            .into_iter()
            .find(|layout| layout.key().eq_ignore_ascii_case(key))
    }

    /// The picker row's one-line description (`docs/ux/gui-layouts.md`, the layout picker).
    pub fn description(self) -> &'static str {
        match self {
            Self::EggDb => {
                "Objects on the left, the editor above the results grid, EggDB's own keys."
            }
            Self::Ssms => "Object Explorer left, editor above Results and Messages, F5 executes.",
            Self::PgAdmin => {
                "Object Explorer left, a Query Tool tab with Data Output below, F5 executes."
            }
            Self::DBeaver => {
                "Navigator over Projects, run toolbar beside the script, Grid or Text results."
            }
            Self::DataGrip => {
                "Database Explorer left, a console per data source, results in Services."
            }
            Self::Workbench => {
                "Navigator with SCHEMAS left, Result Grid with side tabs, Output log below."
            }
            Self::Compass => {
                "Collections in a full-height sidebar, Filter bar above documents, shell below."
            }
            Self::RedisInsight => {
                "Icon rail, a Workbench editor, one result card per command, CLI at the bottom."
            }
        }
    }

    /// The status bar's item while the layout is active, `Works like <product>`; `None` for
    /// EggDB's own.
    pub fn status_item(self) -> Option<&'static str> {
        (self != Self::EggDb).then(|| self.label())
    }

    /// The keys the layout gives its actions; every other key keeps EggDB's meaning. A key
    /// the product has that EggDB has no action for yet is left out.
    pub fn shortcuts(self) -> &'static [Shortcut] {
        match self {
            Self::EggDb => &[],
            Self::Ssms => &SSMS_KEYS,
            Self::PgAdmin => &PGADMIN_KEYS,
            Self::DBeaver => &DBEAVER_KEYS,
            Self::DataGrip => &DATAGRIP_KEYS,
            Self::Workbench => &WORKBENCH_KEYS,
            Self::Compass => &COMPASS_KEYS,
            Self::RedisInsight => &REDIS_INSIGHT_KEYS,
        }
    }

    /// The key EggDB answers for `press` in this layout's editor: the EggDB key of the
    /// action the layout's table gives `press`, else `press` itself.
    pub fn translate(self, press: KeyPress) -> KeyPress {
        self.shortcuts()
            .iter()
            .find(|shortcut| shortcut.matches(press))
            .map_or(press, |found| eggdb_key(found.action))
    }

    fn moves_to_palette(self, action: LayoutAction) -> bool {
        matches!(
            (self, action),
            (Self::Ssms, LayoutAction::Explain) | (Self::RedisInsight, LayoutAction::RunStatement)
        )
    }

    /// The key the layout shows for `action`: its own first key, else EggDB's, or `None`
    /// when the layout gave EggDB's key to another action and this one is reached from the
    /// palette.
    pub fn shortcut_label(self, action: LayoutAction) -> Option<String> {
        if let Some(own) = self
            .shortcuts()
            .iter()
            .find(|shortcut| shortcut.action == action)
        {
            return Some(own.label());
        }
        if self.moves_to_palette(action) {
            return None;
        }
        let (shift, key) = match action {
            LayoutAction::RunStatement => (false, Key::Enter),
            LayoutAction::RunAll => (true, Key::Enter),
            LayoutAction::Explain => (false, Key::Character('e')),
        };
        Some(shortcut(action, key, Held::Primary, shift, false).label())
    }

    /// The label of the toolbar's button that runs the statement.
    pub fn run_label(self) -> &'static str {
        match self {
            Self::EggDb | Self::RedisInsight => "Run",
            Self::Ssms | Self::PgAdmin | Self::DBeaver | Self::DataGrip | Self::Workbench => {
                "Execute"
            }
            Self::Compass => "Find",
        }
    }

    /// The names of EggDB's three result views in this layout: the grid, the messages and
    /// the plan.
    pub fn view_labels(self) -> [&'static str; 3] {
        match self {
            Self::EggDb => ["Results", "Messages", "Plan"],
            Self::Ssms => ["Results", "Messages", "Execution plan"],
            Self::PgAdmin => ["Data Output", "Messages", "Explain"],
            Self::DBeaver => ["Results", "Output", "Execution plan"],
            Self::DataGrip => ["Result", "Output", "Plan"],
            Self::Workbench => ["Result Grid", "Output", "Execution Plan"],
            Self::Compass => ["Documents", "Messages", "Explain"],
            Self::RedisInsight => ["Table", "Text", "Plan"],
        }
    }

    /// The header of the sidebar in this layout.
    pub fn sidebar_title(self) -> &'static str {
        match self {
            Self::EggDb => "Objects",
            Self::Ssms | Self::PgAdmin => "Object Explorer",
            Self::DBeaver => "Database Navigator",
            Self::DataGrip => "Database Explorer",
            Self::Workbench => "Navigator",
            Self::Compass => "Databases",
            Self::RedisInsight => "Workbench",
        }
    }

    /// The regions' places in a window `width` wide (at least [`DESIGN_WIDTH`]): the rects
    /// of [`Layout::regions`], those reaching the right edge widened by the extra width.
    pub fn regions_at(self, width: f32) -> Regions {
        let extra = (width - DESIGN_WIDTH).max(0.0);
        let widen = |rect: Rect| {
            if (rect.x + rect.w - DESIGN_WIDTH).abs() < 0.5 {
                Rect::new(rect.x, rect.y, rect.w + extra, rect.h)
            } else {
                rect
            }
        };
        let regions = self.regions();
        Regions {
            toolbar: regions.toolbar.map(widen),
            sidebar: regions.sidebar.map(widen),
            tabs: regions.tabs.map(widen),
            editor: widen(regions.editor),
            results: widen(regions.results),
            statusbar: regions.statusbar.map(widen),
            extra: regions.extra.map(|(name, rect)| (name, widen(rect))),
        }
    }

    /// The names of the results' tabs.
    pub fn result_tabs(self) -> &'static [&'static str] {
        match self {
            Self::EggDb => &["Results", "Messages"],
            Self::Ssms => &["Results", "Messages", "Execution plan"],
            Self::PgAdmin => &[
                "Data Output",
                "Messages",
                "Notifications",
                "Explain",
                "Query History",
            ],
            Self::DBeaver => &["Statistics", "Output", "Execution Log"],
            Self::DataGrip => &["Output"],
            Self::Workbench => &[
                "Result Grid",
                "Form Editor",
                "Field Types",
                "Query Stats",
                "Execution Plan",
            ],
            Self::Compass => &["List", "JSON", "Table"],
            Self::RedisInsight => &[],
        }
    }

    /// The regions' places.
    pub fn regions(self) -> Regions {
        let bar = Some(rect(0.0, 776.0, 1280.0, 24.0));
        let top = Some(rect(0.0, 0.0, 1280.0, 44.0));
        match self {
            Self::EggDb => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 260.0, 732.0)),
                tabs: Some(rect(260.0, 44.0, 1020.0, 36.0)),
                editor: rect(260.0, 80.0, 1020.0, 300.0),
                results: rect(260.0, 386.0, 1020.0, 390.0),
                statusbar: bar,
                extra: None,
            },
            Self::Ssms => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 300.0, 732.0)),
                tabs: Some(rect(300.0, 44.0, 980.0, 36.0)),
                editor: rect(300.0, 80.0, 980.0, 340.0),
                results: rect(300.0, 420.0, 980.0, 356.0),
                statusbar: bar,
                extra: None,
            },
            Self::PgAdmin => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 300.0, 732.0)),
                tabs: Some(rect(300.0, 44.0, 980.0, 36.0)),
                editor: rect(300.0, 80.0, 980.0, 336.0),
                results: rect(300.0, 416.0, 980.0, 360.0),
                statusbar: bar,
                extra: None,
            },
            Self::DBeaver => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 280.0, 440.0)),
                tabs: Some(rect(280.0, 44.0, 1000.0, 36.0)),
                editor: rect(280.0, 80.0, 1000.0, 320.0),
                results: rect(280.0, 400.0, 1000.0, 376.0),
                statusbar: bar,
                extra: Some(("projects", rect(0.0, 484.0, 280.0, 292.0))),
            },
            Self::DataGrip => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 300.0, 732.0)),
                tabs: Some(rect(300.0, 44.0, 980.0, 36.0)),
                editor: rect(300.0, 80.0, 980.0, 340.0),
                results: rect(300.0, 420.0, 980.0, 356.0),
                statusbar: bar,
                extra: None,
            },
            Self::Workbench => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 260.0, 732.0)),
                tabs: Some(rect(260.0, 44.0, 1020.0, 36.0)),
                editor: rect(260.0, 80.0, 1020.0, 280.0),
                results: rect(260.0, 360.0, 1020.0, 266.0),
                statusbar: bar,
                extra: Some(("output", rect(260.0, 626.0, 1020.0, 150.0))),
            },
            Self::Compass => Regions {
                toolbar: Some(rect(300.0, 36.0, 980.0, 72.0)),
                sidebar: Some(rect(0.0, 0.0, 300.0, 800.0)),
                tabs: Some(rect(300.0, 0.0, 980.0, 36.0)),
                editor: rect(300.0, 108.0, 980.0, 92.0),
                results: rect(300.0, 200.0, 980.0, 568.0),
                statusbar: None,
                extra: Some(("shell", rect(300.0, 768.0, 980.0, 32.0))),
            },
            Self::RedisInsight => Regions {
                toolbar: top,
                sidebar: Some(rect(0.0, 44.0, 60.0, 732.0)),
                tabs: None,
                editor: rect(60.0, 44.0, 1220.0, 180.0),
                results: rect(60.0, 224.0, 1220.0, 552.0),
                statusbar: bar,
                extra: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(rect: &Rect) -> f32 {
        rect.w * rect.h
    }

    fn overlap(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn every_layout_keeps_its_regions_inside_the_window_without_overlap() {
        for layout in Layout::ALL {
            let regions = layout.regions();
            let mut shown: Vec<Rect> = [
                regions.toolbar,
                regions.sidebar,
                regions.tabs,
                regions.statusbar,
            ]
            .into_iter()
            .flatten()
            .collect();
            shown.push(regions.editor);
            shown.push(regions.results);
            if let Some((_, extra)) = regions.extra {
                shown.push(extra);
            }
            for rect in &shown {
                assert!(rect.x >= 0.0 && rect.y >= 0.0, "{layout:?}");
                assert!(
                    rect.x + rect.w <= 1280.0 && rect.y + rect.h <= 800.0,
                    "{layout:?} {rect:?}"
                );
            }
            for (index, first) in shown.iter().enumerate() {
                for second in &shown[index + 1..] {
                    assert!(
                        !overlap(first, second),
                        "{layout:?}: {first:?} and {second:?}"
                    );
                }
            }
            let covered: f32 = shown.iter().map(area).sum();
            assert!(
                covered <= 1280.0 * 800.0 && covered > 1280.0 * 800.0 * 0.95,
                "{layout:?} covers {covered}"
            );
            assert!(layout == Layout::EggDb || layout.label().starts_with("Works like "));
        }
    }

    fn press(key: Key, primary: bool, shift: bool, alt: bool) -> KeyPress {
        let base = if primary {
            crate::accessibility::primary()
        } else {
            Modifiers::default()
        };
        KeyPress {
            key,
            modifiers: Modifiers { shift, alt, ..base },
        }
    }

    #[test]
    fn every_layout_has_a_settings_key_that_reads_back() {
        let mut keys: Vec<&str> = Layout::ALL.iter().map(|layout| layout.key()).collect();
        for layout in Layout::ALL {
            assert_eq!(Layout::from_key(layout.key()), Some(layout));
            assert!(!layout.description().is_empty() && layout.description().ends_with('.'));
        }
        assert_eq!(Layout::from_key(" SSMS "), Some(Layout::Ssms));
        assert_eq!(Layout::from_key("pgadmin3"), None);
        assert_eq!(Layout::from_key(""), None);
        keys.dedup();
        assert_eq!(keys.len(), 8);
        assert_eq!(Layout::EggDb.status_item(), None);
        assert_eq!(
            Layout::Ssms.status_item(),
            Some("Works like SQL Server Management Studio")
        );
    }

    #[test]
    fn ssms_keys_execute_the_tab_and_explain_and_its_ctrl_e_no_longer_explains() {
        let run_all = eggdb_key(LayoutAction::RunAll);
        let explain = eggdb_key(LayoutAction::Explain);
        let ssms = Layout::Ssms;
        assert_eq!(ssms.translate(press(Key::F5, false, false, false)), run_all);
        assert_eq!(ssms.translate(press(Key::Character('E'), true, false, false)), run_all);
        assert_eq!(ssms.translate(explain), run_all);
        assert_eq!(ssms.translate(press(Key::Character('x'), false, false, true)), run_all);
        assert_eq!(ssms.translate(press(Key::Character('l'), true, false, false)), explain);
        let typed = press(Key::Character('l'), false, false, false);
        assert_eq!(ssms.translate(typed), typed);
        let run_statement = eggdb_key(LayoutAction::RunStatement);
        assert_eq!(ssms.translate(run_statement), run_statement);
        assert_eq!(ssms.shortcut_label(LayoutAction::RunAll), Some("F5".to_owned()));
        assert!(
            ssms.shortcut_label(LayoutAction::Explain)
                .is_some_and(|label| label.ends_with("+L"))
        );
        assert_eq!(
            Layout::RedisInsight.shortcut_label(LayoutAction::RunStatement),
            None
        );
    }

    #[test]
    fn each_familiar_layout_routes_its_own_keys_and_eggdb_routes_none() {
        let run_all = eggdb_key(LayoutAction::RunAll);
        let run_statement = eggdb_key(LayoutAction::RunStatement);
        let explain = eggdb_key(LayoutAction::Explain);
        let analyze = press(Key::Character('e'), true, true, false);
        assert_eq!(Layout::DBeaver.translate(analyze), explain);
        assert_eq!(
            Layout::DBeaver.translate(press(Key::Character('x'), false, false, true)),
            run_all
        );
        assert_eq!(
            Layout::Workbench.translate(press(Key::Character('x'), true, false, true)),
            explain
        );
        assert_eq!(Layout::PgAdmin.translate(press(Key::F5, false, false, false)), run_all);
        let enter = KeyPress::plain(Key::Enter);
        assert_eq!(Layout::Compass.translate(enter), run_all);
        assert_eq!(Layout::DataGrip.translate(enter), enter);
        assert_eq!(Layout::RedisInsight.translate(run_statement), run_all);
        for key in [Key::F5, Key::Enter, Key::Character('e'), Key::Character('l')] {
            for (primary, shift, alt) in [
                (false, false, false),
                (true, false, false),
                (true, true, false),
                (false, false, true),
            ] {
                let typed = press(key, primary, shift, alt);
                assert_eq!(Layout::EggDb.translate(typed), typed);
            }
        }
    }

    #[test]
    fn wider_windows_widen_the_regions_at_the_right_edge_only() {
        let wide = Layout::Ssms.regions_at(1600.0);
        assert_eq!(wide.editor, Rect::new(300.0, 80.0, 1300.0, 340.0));
        assert_eq!(wide.sidebar, Some(Rect::new(0.0, 44.0, 300.0, 732.0)));
        assert_eq!(wide.toolbar, Some(Rect::new(0.0, 0.0, 1600.0, 44.0)));
        assert_eq!(Layout::Ssms.regions_at(900.0), Layout::Ssms.regions());
        let labels = Layout::Workbench.view_labels();
        assert_eq!(labels[0], "Result Grid");
        assert_eq!(Layout::EggDb.view_labels(), ["Results", "Messages", "Plan"]);
        assert_eq!(Layout::Compass.run_label(), "Find");
    }
}
