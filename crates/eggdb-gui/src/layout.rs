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

    /// The names of the results' tabs.
    pub fn result_tabs(self) -> &'static [&'static str] {
        match self {
            Self::EggDb => &["Results", "Messages"],
            Self::Ssms => &["Results", "Messages", "Execution plan"],
            Self::PgAdmin => &["Data Output", "Messages", "Notifications", "Explain", "Query History"],
            Self::DBeaver => &["Statistics", "Output", "Execution Log"],
            Self::DataGrip => &["Output"],
            Self::Workbench => &["Result Grid", "Form Editor", "Field Types", "Query Stats", "Execution Plan"],
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
            let mut shown: Vec<Rect> = [regions.toolbar, regions.sidebar, regions.tabs, regions.statusbar]
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
                assert!(rect.x + rect.w <= 1280.0 && rect.y + rect.h <= 800.0, "{layout:?} {rect:?}");
            }
            for (index, first) in shown.iter().enumerate() {
                for second in &shown[index + 1..] {
                    assert!(!overlap(first, second), "{layout:?}: {first:?} and {second:?}");
                }
            }
            let covered: f32 = shown.iter().map(area).sum();
            assert!(covered <= 1280.0 * 800.0 && covered > 1280.0 * 800.0 * 0.95, "{layout:?} covers {covered}");
            assert!(layout == Layout::EggDb || layout.label().starts_with("Works like "));
        }
    }
}
