use crate::frame::{Color, Style};
use crate::terminal::{Capabilities, Colors};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Plain,
    Header,
    Title,
    FocusedTitle,
    Selected,
    JsonKey,
    JsonString,
    JsonNumber,
    JsonLiteral,
    Dim,
    Error,
    HexOffset,
}

const RED: u8 = 1;
const GREEN: u8 = 2;
const YELLOW: u8 = 3;
const MAGENTA: u8 = 5;
const CYAN: u8 = 6;
const BRIGHT_BLACK: u8 = 8;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Theme {
    capabilities: Capabilities,
}

impl Theme {
    pub(crate) fn new(capabilities: Capabilities) -> Self {
        Self { capabilities }
    }

    pub(crate) fn unicode(&self) -> bool {
        self.capabilities.unicode
    }

    pub(crate) fn plain(&self) -> bool {
        !self.capabilities.attributes
    }

    pub(crate) fn style(&self, role: Role) -> Style {
        if !self.capabilities.attributes {
            return Style::default();
        }
        let colors = self.capabilities.colors == Colors::Ansi16;
        let fg = |ansi: u8| {
            if colors {
                Color::Ansi(ansi)
            } else {
                Color::Default
            }
        };
        let inverse = Style {
            inverse: true,
            ..Style::default()
        };
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        match role {
            Role::Plain => Style::default(),
            Role::Header | Role::Title | Role::Selected => inverse,
            Role::FocusedTitle => Style {
                bold: true,
                inverse: true,
                fg: fg(CYAN),
                ..Style::default()
            },
            Role::JsonKey if colors => Style {
                fg: fg(CYAN),
                ..Style::default()
            },
            Role::JsonKey => bold,
            Role::JsonString => Style {
                fg: fg(GREEN),
                ..Style::default()
            },
            Role::JsonNumber => Style {
                fg: fg(YELLOW),
                ..Style::default()
            },
            Role::JsonLiteral => Style {
                fg: fg(MAGENTA),
                ..Style::default()
            },
            Role::Dim | Role::HexOffset => Style {
                fg: fg(BRIGHT_BLACK),
                ..Style::default()
            },
            Role::Error => Style {
                fg: fg(RED),
                bold: true,
                ..Style::default()
            },
        }
    }

    pub(crate) fn symbol(&self, symbol: Symbol) -> &'static str {
        symbol.spelling(self.capabilities.unicode)
    }

    pub(crate) fn ellipsis(&self) -> char {
        if self.capabilities.unicode {
            '…'
        } else {
            '~'
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Symbol {
    VerticalLine,
    HorizontalLine,
    Ellipsis,
    EnDash,
    MiddleDot,
    UpDown,
    LeftRight,
}

impl Symbol {
    pub(crate) fn spelling(self, unicode: bool) -> &'static str {
        let (fancy, ascii) = match self {
            Self::VerticalLine => ("│", "|"),
            Self::HorizontalLine => ("─", "-"),
            Self::Ellipsis => ("…", "~"),
            Self::EnDash => ("–", "-"),
            Self::MiddleDot => ("·", "|"),
            Self::UpDown => ("↑↓", "Up/Down"),
            Self::LeftRight => ("←→", "Left/Right"),
        };
        if unicode { fancy } else { ascii }
    }
}

#[cfg(test)]
mod tests {
    use super::{Role, Symbol, Theme};
    use crate::frame::{Color, Style};
    use crate::terminal::{Capabilities, Colors};

    #[test]
    fn every_symbol_has_an_ascii_fallback() {
        for symbol in [
            Symbol::VerticalLine,
            Symbol::HorizontalLine,
            Symbol::Ellipsis,
            Symbol::EnDash,
            Symbol::MiddleDot,
            Symbol::UpDown,
            Symbol::LeftRight,
        ] {
            assert!(symbol.spelling(false).is_ascii(), "{symbol:?}");
            assert!(!symbol.spelling(true).is_empty());
        }
        assert_eq!(Symbol::VerticalLine.spelling(false), "|");
        assert_eq!(Symbol::Ellipsis.spelling(false), "~");
        assert_eq!(Symbol::UpDown.spelling(false), "Up/Down");
    }

    #[test]
    fn monochrome_keeps_attributes_and_drops_colours() {
        let theme = Theme::new(Capabilities {
            colors: Colors::None,
            attributes: true,
            unicode: true,
        });
        assert_eq!(theme.style(Role::JsonString), Style::default());
        assert!(theme.style(Role::JsonKey).bold);
        assert!(theme.style(Role::Error).bold);
        assert_eq!(theme.style(Role::Error).fg, Color::Default);
        assert!(theme.style(Role::Selected).inverse);
    }

    #[test]
    fn the_plain_build_has_no_styles_at_all() {
        let theme = Theme::new(Capabilities {
            colors: Colors::None,
            attributes: false,
            unicode: false,
        });
        assert_eq!(theme.style(Role::Selected), Style::default());
        assert_eq!(theme.style(Role::Header), Style::default());
    }
}
