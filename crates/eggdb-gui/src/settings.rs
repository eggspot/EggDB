use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::layout::Layout;

/// The variable that moves the client's settings file, for scripted runs and tests.
pub const SETTINGS_VARIABLE: &str = "EGGDB_GUI_SETTINGS";

const FILE_NAME: &str = "gui-settings";

/// The largest settings file the client reads; a larger one is taken as no settings, so a
/// stray or hostile file is never read whole.
pub const MOST_SETTINGS_BYTES: u64 = 1 << 20;

/// What the client keeps per user between starts: one `name = value` line each.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClientSettings {
    /// The main window's layout (`layout = ssms`).
    pub layout: Option<Layout>,
    other_lines: Vec<String>,
}

impl ClientSettings {
    /// The settings in `text`, after a byte order mark if it starts with one. Lines the client
    /// does not know and values it cannot read are kept as they are, so a newer client's
    /// settings survive an older one; blank lines are dropped.
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut settings = Self::default();
        for line in text.lines() {
            let known = line
                .split_once('=')
                .filter(|(name, _)| name.trim() == "layout")
                .and_then(|(_, value)| Layout::from_key(value));
            match known {
                Some(layout) => settings.layout = Some(layout),
                None if line.trim().is_empty() => {}
                None => settings.other_lines.push(line.to_owned()),
            }
        }
        settings
    }

    /// The settings as the file holds them. With a layout set, a kept `layout` line whose
    /// value this client does not know is left out, so the file has one `layout` line.
    pub fn render(&self) -> String {
        let mut text = String::new();
        if let Some(layout) = self.layout {
            text.push_str("layout = ");
            text.push_str(layout.key());
            text.push('\n');
        }
        let lines = self
            .other_lines
            .iter()
            .filter(|line| self.layout.is_none() || !names(line, "layout"));
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        text
    }

    /// The value of the `name = value` line named `name`, when the file has one.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.other_lines.iter().find_map(|line| {
            line.split_once('=')
                .filter(|(key, _)| key.trim() == name)
                .map(|(_, value)| value.trim())
        })
    }

    /// Sets the `name = value` line named `name`, adding it when the file has none.
    pub fn set(&mut self, name: &str, value: &str) {
        let line = format!("{name} = {value}");
        let at = self.other_lines.iter().position(|kept| names(kept, name));
        match at {
            Some(at) => self.other_lines[at] = line,
            None => self.other_lines.push(line),
        }
    }

    /// The settings in the file at `path`; none when it is missing, unreadable or larger
    /// than [`MOST_SETTINGS_BYTES`].
    pub fn load(path: &Path) -> Self {
        let Ok(file) = std::fs::File::open(path) else {
            return Self::default();
        };
        let mut bytes = Vec::new();
        let read = file.take(MOST_SETTINGS_BYTES + 1).read_to_end(&mut bytes);
        if read.is_err() || bytes.len() as u64 > MOST_SETTINGS_BYTES {
            return Self::default();
        }
        Self::parse(&String::from_utf8_lossy(&bytes))
    }

    /// Writes the settings to `path`, creating its directory; only the user can read it. They
    /// are written to a file beside it first and then renamed over it, so a crash or a full
    /// disk mid-write leaves the old file whole.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(directory) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            eggdb_platform::user_files::create_private_directory(directory)?;
        }
        let written = written_beside(path);
        let saved = write_whole(&written, self.render().as_bytes())
            .and_then(|()| std::fs::rename(&written, path));
        if saved.is_err() {
            let _ = std::fs::remove_file(&written);
        }
        saved
    }
}

fn names(line: &str, name: &str) -> bool {
    line.split_once('=')
        .is_some_and(|(key, _)| key.trim() == name)
}

fn written_beside(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| FILE_NAME.into(), |name| name.to_string_lossy());
    path.with_file_name(format!(".{name}.{}.new", std::process::id()))
}

fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = eggdb_platform::user_files::open_private_rewrite(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

/// Where the client's settings live: [`SETTINGS_VARIABLE`] when it is set (empty turns
/// keeping them off), else `gui-settings` in EggDB's per-user state directory.
pub fn settings_path() -> Option<PathBuf> {
    match std::env::var_os(SETTINGS_VARIABLE) {
        Some(path) if path.is_empty() => None,
        Some(path) => Some(PathBuf::from(path)),
        None => eggdb_platform::user_files::state_directory().map(|base| base.join(FILE_NAME)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_layout_line_reads_and_writes_and_other_lines_are_kept() {
        let settings = ClientSettings::parse("theme = dark\nlayout = pgadmin\n\n");
        assert_eq!(settings.layout, Some(Layout::PgAdmin));
        assert_eq!(settings.render(), "layout = pgadmin\ntheme = dark\n");
        assert_eq!(ClientSettings::parse(&settings.render()), settings);
        let unknown = ClientSettings::parse("layout = toad\n");
        assert_eq!(unknown.layout, None);
        assert_eq!(unknown.render(), "layout = toad\n");
        let picked = ClientSettings {
            layout: Some(Layout::Ssms),
            ..unknown
        };
        assert_eq!(picked.render(), "layout = ssms\n");
        assert_eq!(
            ClientSettings::parse("\u{feff}layout = ssms\r\ntheme = dark\r\n").layout,
            Some(Layout::Ssms)
        );
        assert_eq!(ClientSettings::parse("").render(), "");
        let changed = ClientSettings {
            layout: Some(Layout::Ssms),
            ..ClientSettings::parse("layout=dbeaver")
        };
        assert_eq!(changed.render(), "layout = ssms\n");
    }

    #[test]
    fn a_named_line_is_read_replaced_or_added_and_the_others_are_kept() {
        let mut settings = ClientSettings::parse("theme = dark\neggdb.sidebar_width = 300\n");
        assert_eq!(settings.value("eggdb.sidebar_width"), Some("300"));
        assert_eq!(settings.value("eggdb.results_ratio"), None);
        settings.set("eggdb.sidebar_width", "268");
        settings.set("eggdb.results_ratio", "0.4000");
        assert_eq!(
            settings.render(),
            "theme = dark\neggdb.sidebar_width = 268\neggdb.results_ratio = 0.4000\n"
        );
    }

    #[test]
    fn hostile_bytes_read_as_no_layout_without_failing() {
        for text in [
            "=",
            "layout",
            "layout = \u{0}",
            "\u{feff}\u{feff}layout = ssms",
            "layout = = ssms",
        ] {
            let settings = ClientSettings::parse(text);
            assert_eq!(settings.layout, None, "{text:?}");
        }
    }

    #[test]
    fn settings_are_saved_and_loaded_from_a_file() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-gui-settings-{}", std::process::id()));
        let path = directory.join("nested").join(FILE_NAME);
        assert_eq!(ClientSettings::load(&path), ClientSettings::default());
        let settings = ClientSettings {
            layout: Some(Layout::RedisInsight),
            ..ClientSettings::default()
        };
        settings.save(&path).expect("settings are written");
        assert_eq!(
            ClientSettings::load(&path).layout,
            Some(Layout::RedisInsight)
        );
        let replaced = ClientSettings {
            layout: Some(Layout::DataGrip),
            ..ClientSettings::default()
        };
        replaced.save(&path).expect("settings are written again");
        assert_eq!(ClientSettings::load(&path).layout, Some(Layout::DataGrip));
        assert!(!written_beside(&path).exists());
        let entries = std::fs::read_dir(path.parent().expect("a directory"))
            .expect("the directory lists")
            .count();
        assert_eq!(entries, 1, "only the settings file is left");
    }

    #[test]
    fn a_settings_file_past_one_megabyte_reads_as_no_settings() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-gui-settings-big-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("a directory");
        let path = directory.join(FILE_NAME);
        let mut text = "layout = ssms\n".to_owned();
        let fits = usize::try_from(MOST_SETTINGS_BYTES).expect("1 MB fits");
        text.push_str(&"#".repeat(fits - text.len()));
        std::fs::write(&path, &text).expect("the file is written");
        assert_eq!(ClientSettings::load(&path).layout, Some(Layout::Ssms));
        text.push('#');
        std::fs::write(&path, &text).expect("the file is written");
        assert_eq!(ClientSettings::load(&path), ClientSettings::default());
    }
}
