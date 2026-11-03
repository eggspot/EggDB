use eggdb_pgwire::client::ConnectOptions;

use super::{
    BODY, CHECK_LABEL_X, ConnectCard, Focus, INPUT_TEXT_INSET, INPUT_TEXT_TOP, InputLook, Pen,
    RADIUS_CONTROL, TextField, checkbox, down, input_box, offset, panel_shift,
};
use crate::canvas::Rect;
use crate::connection::{
    ConnectFailure, FailureField, ssl_mode, unsupported_protocol_failure, url_failure,
};

/// The user `eggdb serve` logs in as when the User input is empty.
pub const DEFAULT_SERVER_USER: &str = "eggdb";
/// The host an empty Host input means.
pub const DEFAULT_HOST: &str = "localhost";
/// The `URL` input's placeholder.
pub const URL_PLACEHOLDER: &str = "postgresql://user@host:5432/shop";
/// The line under `Use TLS` while it is unchecked.
pub const TLS_WARNING: &str = "Warning: the password travels unencrypted";

pub(super) const URL_LABEL: (f32, f32) = (24.0, 120.0);
pub(super) const URL_INPUT: Rect = Rect::new(24.0, 142.0, 512.0, 28.0);
pub(super) const PROTOCOL_LABEL: (f32, f32) = (24.0, 186.0);
pub(super) const PROTOCOL_MENU: Rect = Rect::new(24.0, 208.0, 168.0, 28.0);
pub(super) const HOST_LABEL: (f32, f32) = (200.0, 186.0);
pub(super) const HOST_INPUT: Rect = Rect::new(200.0, 208.0, 224.0, 28.0);
pub(super) const PORT_LABEL: (f32, f32) = (432.0, 186.0);
pub(super) const PORT_INPUT: Rect = Rect::new(432.0, 208.0, 104.0, 28.0);
pub(super) const DATABASE_LABEL: (f32, f32) = (24.0, 252.0);
pub(super) const DATABASE_INPUT: Rect = Rect::new(24.0, 274.0, 512.0, 28.0);
pub(super) const USER_LABEL: (f32, f32) = (24.0, 318.0);
pub(super) const USER_INPUT: Rect = Rect::new(24.0, 340.0, 250.0, 28.0);
pub(super) const PASSWORD_LABEL: (f32, f32) = (286.0, 318.0);
pub(super) const PASSWORD_INPUT: Rect = Rect::new(286.0, 340.0, 250.0, 28.0);
pub(super) const USE_TLS_BOX: Rect = Rect::new(24.0, 384.0, 16.0, 16.0);
pub(super) const TLS_WARNING_AT: (f32, f32) = (24.0, 406.0);
pub(super) const TLS_WARNING_WIDTH: f32 = 332.0;
pub(super) const USE_TLS_LABEL: &str = "Use TLS";
pub(super) const MASK_ADVANCE: f32 = 10.0;
pub(super) const MASK_DIAMETER: f32 = 6.0;
const CHEVRON: (f32, f32) = (20.0, 4.0);

/// The connect card's two segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// `Open a file`.
    File,
    /// `Connect to a server`.
    Server,
}

/// The protocol menu's choices, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// The PostgreSQL protocol, which `eggdb serve` always speaks.
    PostgreSql,
    /// The MySQL protocol.
    MySql,
    /// SQL Server's TDS protocol.
    SqlServer,
    /// Redis's RESP protocol.
    Redis,
}

impl Protocol {
    /// Every protocol, in the menu's order.
    pub const ALL: [Self; 4] = [Self::PostgreSql, Self::MySql, Self::SqlServer, Self::Redis];

    /// The menu's text.
    pub fn label(self) -> &'static str {
        match self {
            Self::PostgreSql => "PostgreSQL protocol",
            Self::MySql => "MySQL protocol",
            Self::SqlServer => "SQL Server (TDS)",
            Self::Redis => "Redis (RESP)",
        }
    }

    /// The port the Port input holds until the user changes it.
    pub fn default_port(self) -> u16 {
        match self {
            Self::PostgreSql => 5432,
            Self::MySql => 3306,
            Self::SqlServer => 1433,
            Self::Redis => 6379,
        }
    }

    /// The URL scheme written for it.
    pub fn scheme(self) -> &'static str {
        match self {
            Self::PostgreSql => "postgresql",
            Self::MySql => "mysql",
            Self::SqlServer => "sqlserver",
            Self::Redis => "redis",
        }
    }

    fn of_scheme(scheme: &str) -> Option<(Self, &'static str)> {
        Some(match scheme.to_ascii_lowercase().as_str() {
            "postgresql" => (Self::PostgreSql, "postgresql"),
            "postgres" => (Self::PostgreSql, "postgres"),
            "eggdb" => (Self::PostgreSql, "eggdb"),
            "mysql" => (Self::MySql, "mysql"),
            "sqlserver" => (Self::SqlServer, "sqlserver"),
            "redis" => (Self::Redis, "redis"),
            _ => return None,
        })
    }

    fn tls_off_parameter(self) -> &'static str {
        match self {
            Self::PostgreSql => "sslmode=disable",
            Self::MySql | Self::SqlServer | Self::Redis => "tls=false",
        }
    }

    fn position(self) -> usize {
        Self::ALL
            .iter()
            .position(|&protocol| protocol == self)
            .unwrap_or(0)
    }

    /// The next protocol in the menu, wrapping at the end.
    pub fn next(self) -> Self {
        Self::ALL[(self.position() + 1) % Self::ALL.len()]
    }

    /// The previous protocol in the menu, wrapping at the start.
    pub fn previous(self) -> Self {
        Self::ALL[(self.position() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// What a server URL says, read part by part so the fields fill while it is typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    /// The protocol its scheme names.
    pub protocol: Protocol,
    /// The scheme as written (`eggdb` stays `eggdb`).
    pub scheme: &'static str,
    /// The user, %-escapes decoded; empty when the URL names none.
    pub user: String,
    /// The password after `user:`, which the URL field never keeps.
    pub password: Option<String>,
    /// The host, without the brackets of an IPv6 address.
    pub host: String,
    /// The port as written, when there is one.
    pub port: Option<String>,
    /// The database, %-escapes decoded; empty when the URL names none.
    pub database: String,
    /// `Use TLS`: off only for `?sslmode=disable`, or `?tls=false` for the other protocols.
    pub use_tls: bool,
    /// Whether every part is whole: a host, a port that is a number from 1 to 65535, whole
    /// %-escapes and a known `sslmode` or `tls` value.
    pub well_formed: bool,
}

fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let pair = text.get(index + 1..index + 3)?;
            if !pair.bytes().all(|digit| digit.is_ascii_hexdigit()) {
                return None;
            }
            decoded.push(u8::from_str_radix(pair, 16).ok()?);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn percent_encoded(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn valid_port(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|&port| port > 0)
}

/// Reads `text` as a server URL: `scheme://[user[:password]@]host[:port][/database][?…]`
/// with the schemes `postgresql`, `postgres`, `eggdb`, `mysql`, `sqlserver` and `redis`.
/// `None` until the text starts with one of them and `://`; a URL with a missing or broken
/// part is read as far as it goes, with [`ServerUrl::well_formed`] false.
pub fn parse_server_url(text: &str) -> Option<ServerUrl> {
    let (scheme, rest) = text.trim().split_once("://")?;
    let (protocol, scheme) = Protocol::of_scheme(scheme)?;
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = main.split_once('/').unwrap_or((main, ""));
    let (user_info, host_port) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user_part, password_part) = match user_info.split_once(':') {
        Some((user, password)) => (user, Some(password)),
        None => (user_info, None),
    };
    let mut well_formed = true;
    let mut decoded = |part: &str| {
        percent_decoded(part).unwrap_or_else(|| {
            well_formed = false;
            part.to_owned()
        })
    };
    let user = decoded(user_part);
    let password = password_part.map(&mut decoded);
    let database = decoded(path);
    let (host, port) = match host_port.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((address, after)) => (address, after.strip_prefix(':')),
            None => {
                well_formed = false;
                (bracketed, None)
            }
        },
        None => match host_port.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (host_port, None),
        },
    };
    let port = port.filter(|port| !port.is_empty()).map(str::to_owned);
    if host.is_empty()
        || host.chars().any(char::is_whitespace)
        || database.contains('/')
        || port
            .as_deref()
            .is_some_and(|port| valid_port(port).is_none())
    {
        well_formed = false;
    }
    let mut use_tls = true;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = value.to_ascii_lowercase();
        let tls = match (protocol, name) {
            (Protocol::PostgreSql, "sslmode") => match value.as_str() {
                "disable" => Some(false),
                "allow" | "prefer" | "require" | "verify-ca" | "verify-full" => Some(true),
                _ => None,
            },
            (Protocol::MySql | Protocol::SqlServer | Protocol::Redis, "tls") => {
                match value.as_str() {
                    "false" | "0" | "no" | "off" => Some(false),
                    "true" | "1" | "yes" | "on" => Some(true),
                    _ => None,
                }
            }
            _ => continue,
        };
        match tls {
            Some(on) => use_tls = on,
            None => well_formed = false,
        }
    }
    Some(ServerUrl {
        protocol,
        scheme,
        user,
        password,
        host: host.to_owned(),
        port,
        database,
        use_tls,
        well_formed,
    })
}

/// The server segment's fields, kept in step: editing the `URL` input fills the fields below
/// it, and editing a field rewrites the URL (`docs/ux/workspace.md` 1.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerForm {
    /// The `URL` input; it never holds the password.
    pub url: TextField,
    /// The protocol menu's choice.
    pub protocol: Protocol,
    /// The Host input.
    pub host: TextField,
    /// The Port input.
    pub port: TextField,
    /// The Database input.
    pub database: TextField,
    /// The User input.
    pub user: TextField,
    /// The Password input, drawn masked.
    pub password: TextField,
    /// `Use TLS`.
    pub use_tls: bool,
    scheme: &'static str,
}

impl Default for ServerForm {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerForm {
    /// The segment as it first shows: an empty URL, the PostgreSQL protocol on port 5432 and
    /// `Use TLS` checked.
    pub fn new() -> Self {
        let mut port = TextField::default();
        port.set(&Protocol::PostgreSql.default_port().to_string());
        Self {
            url: TextField::default(),
            protocol: Protocol::PostgreSql,
            host: TextField::default(),
            port,
            database: TextField::default(),
            user: TextField::default(),
            password: TextField::default(),
            use_tls: true,
            scheme: Protocol::PostgreSql.scheme(),
        }
    }

    /// Takes an edit of the `URL` input: once it starts with a known scheme its parts fill
    /// the fields, a password typed into it included; [`ServerForm::forget_url_password`]
    /// takes it out of the URL once the input loses the focus.
    pub fn url_edited(&mut self) {
        let Some(read) = parse_server_url(self.url.value()) else {
            return;
        };
        self.protocol = read.protocol;
        self.scheme = read.scheme;
        self.host.set(&read.host);
        let port = read
            .port
            .unwrap_or_else(|| read.protocol.default_port().to_string());
        self.port.set(&port);
        self.database.set(&read.database);
        self.user.set(&read.user);
        self.use_tls = read.use_tls;
        if let Some(password) = read.password {
            self.password.set(&password);
        }
    }

    /// Writes the URL again without the password it holds, if any: the URL input never keeps
    /// one once the user leaves it or connects.
    pub fn forget_url_password(&mut self) {
        if parse_server_url(self.url.value()).is_some_and(|read| read.password.is_some()) {
            self.url.set(&self.written_url());
        }
    }

    /// Takes an edit of a field below the URL: the URL is written again from the fields.
    pub fn field_edited(&mut self) {
        self.url.set(&self.written_url());
    }

    /// Chooses `protocol`: a Port still holding the old protocol's default (or nothing) takes
    /// the new one's, and the URL is written again.
    pub fn choose_protocol(&mut self, protocol: Protocol) {
        let port = self.port.value().trim();
        if port.is_empty() || port == self.protocol.default_port().to_string() {
            self.port.set(&protocol.default_port().to_string());
        }
        if protocol != self.protocol {
            self.scheme = protocol.scheme();
        }
        self.protocol = protocol;
        self.field_edited();
    }

    /// Checks or unchecks `Use TLS`, and writes the URL again.
    pub fn toggle_tls(&mut self) {
        self.use_tls = !self.use_tls;
        self.field_edited();
    }

    /// The URL the fields say, without the password: `postgresql://ada@db:5433/shop`, with
    /// `?sslmode=disable` (`?tls=false`) while `Use TLS` is off.
    pub fn written_url(&self) -> String {
        let mut url = format!("{}://", self.scheme);
        let user = self.user.value().trim();
        if !user.is_empty() {
            url.push_str(&percent_encoded(user));
            url.push('@');
        }
        let host = self.host.value().trim();
        if host.contains(':') {
            url.push_str(&format!("[{host}]"));
        } else {
            url.push_str(host);
        }
        let port = self.port.value().trim();
        if !port.is_empty() {
            url.push(':');
            url.push_str(port);
        }
        let database = self.database.value().trim();
        if !database.is_empty() {
            url.push('/');
            url.push_str(&percent_encoded(database));
        }
        if !self.use_tls {
            url.push('?');
            url.push_str(self.protocol.tls_off_parameter());
        }
        url
    }

    /// `host:port` as the progress line and the error lines show it.
    pub fn place(&self) -> String {
        let host = self.host.value().trim();
        let host = if host.is_empty() { DEFAULT_HOST } else { host };
        let port = self.port.value().trim();
        let port = if port.is_empty() {
            self.protocol.default_port().to_string()
        } else {
            port.to_owned()
        };
        format!("{host}:{port}")
    }

    /// Where `Connect` goes: the fields as connect options, with libpq's defaults for what is
    /// left empty (host `localhost`, user `eggdb`, the user's name as the database). Fails
    /// with the URL's lines when a typed URL (one the fields did not write) does not parse or
    /// the port is not a number, and
    /// with the protocol's when the client cannot speak it yet.
    pub fn connect_options(&self) -> Result<ConnectOptions, ConnectFailure> {
        let typed = self.url.value().trim();
        if !typed.is_empty()
            && typed != self.written_url()
            && !parse_server_url(typed).is_some_and(|url| url.well_formed)
        {
            return Err(url_failure());
        }
        if self.protocol != Protocol::PostgreSql {
            return Err(unsupported_protocol_failure(self.protocol.label()));
        }
        let host = self.host.value().trim();
        if host.chars().any(char::is_whitespace) {
            return Err(url_failure());
        }
        let port = self.port.value().trim();
        let port = if port.is_empty() {
            self.protocol.default_port()
        } else {
            valid_port(port).ok_or_else(url_failure)?
        };
        let user = match self.user.value().trim() {
            "" => DEFAULT_SERVER_USER,
            user => user,
        };
        let database = match self.database.value().trim() {
            "" => user,
            database => database,
        };
        let password = self.password.value();
        Ok(ConnectOptions {
            host: if host.is_empty() { DEFAULT_HOST } else { host }.to_owned(),
            port,
            user: user.to_owned(),
            database: database.to_owned(),
            password: (!password.is_empty()).then(|| password.to_owned()),
            ssl_mode: ssl_mode(self.use_tls),
            ssl_root_cert: None,
            replication: false,
        })
    }

    pub(super) fn field_mut(&mut self, focus: Focus) -> Option<&mut TextField> {
        Some(match focus {
            Focus::Url => &mut self.url,
            Focus::Host => &mut self.host,
            Focus::Port => &mut self.port,
            Focus::Database => &mut self.database,
            Focus::User => &mut self.user,
            Focus::Password => &mut self.password,
            _ => return None,
        })
    }
}

/// The focus stop a failure about `field` moves to on the server segment.
pub(super) fn failure_focus(field: FailureField) -> Option<Focus> {
    Some(match field {
        FailureField::Url => Focus::Url,
        FailureField::Protocol => Focus::Protocol,
        FailureField::Host => Focus::Host,
        FailureField::Database => Focus::Database,
        FailureField::User => Focus::User,
        FailureField::Password => Focus::Password,
        FailureField::File => return None,
    })
}

/// The rect of a server segment's focus stop, card-relative before the error lines' shift.
pub(super) fn stop_rect(focus: Focus) -> Option<Rect> {
    Some(match focus {
        Focus::Url => URL_INPUT,
        Focus::Protocol => PROTOCOL_MENU,
        Focus::Host => HOST_INPUT,
        Focus::Port => PORT_INPUT,
        Focus::Database => DATABASE_INPUT,
        Focus::User => USER_INPUT,
        Focus::Password => PASSWORD_INPUT,
        Focus::UseTls => USE_TLS_BOX,
        _ => return None,
    })
}

pub(super) fn tls_warning_shown(card: &ConnectCard) -> bool {
    !card.server.use_tls && !card.disabled()
}

pub(super) fn draw_panel(pen: &mut Pen<'_, '_>, card: &ConnectCard, origin: (f32, f32)) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let shift = panel_shift(card);
    let at = |rect: Rect| offset(down(rect, shift), origin);
    let label_color = if card.disabled() {
        theme.text_disabled
    } else {
        theme.text_primary
    };
    for (text, (x, y)) in [
        ("URL", URL_LABEL),
        ("Protocol", PROTOCOL_LABEL),
        ("Host", HOST_LABEL),
        ("Port", PORT_LABEL),
        ("Database", DATABASE_LABEL),
        ("User", USER_LABEL),
        ("Password", PASSWORD_LABEL),
    ] {
        pen.text(
            face,
            text,
            (origin.0 + x, origin.1 + y + shift),
            BODY,
            label_color,
        );
    }
    let form = &card.server;
    for (focus, field, rect, placeholder, masked) in [
        (Focus::Url, &form.url, URL_INPUT, URL_PLACEHOLDER, false),
        (Focus::Host, &form.host, HOST_INPUT, DEFAULT_HOST, false),
        (Focus::Port, &form.port, PORT_INPUT, "", false),
        (Focus::Database, &form.database, DATABASE_INPUT, "", false),
        (
            Focus::User,
            &form.user,
            USER_INPUT,
            DEFAULT_SERVER_USER,
            false,
        ),
        (Focus::Password, &form.password, PASSWORD_INPUT, "", true),
    ] {
        let look = InputLook {
            focused: card.focus == focus && !card.disabled(),
            failed: card.error.is_some() && card.failed_focus() == Some(focus),
            placeholder,
            masked,
            caret: card.caret,
        };
        input_box(pen, field, at(rect), card.disabled(), look);
    }
    protocol_menu(pen, card, at(PROTOCOL_MENU));
    let tls_box = at(USE_TLS_BOX);
    checkbox(pen, tls_box, form.use_tls);
    pen.text(
        face,
        USE_TLS_LABEL,
        (origin.0 + CHECK_LABEL_X, tls_box.y - 1.0),
        BODY,
        label_color,
    );
    if tls_warning_shown(card) {
        let shown = pen.elided(face, TLS_WARNING, TLS_WARNING_WIDTH);
        let top = origin.1 + TLS_WARNING_AT.1 + shift;
        pen.text(
            face,
            &shown,
            (origin.0 + TLS_WARNING_AT.0, top),
            BODY,
            theme.warning,
        );
    }
}

fn protocol_menu(pen: &mut Pen<'_, '_>, card: &ConnectCard, rect: Rect) {
    let theme = pen.theme;
    let face = pen.faces.ui;
    let failed = card.error.is_some() && card.failed_focus() == Some(Focus::Protocol);
    let border = if failed {
        theme.error
    } else {
        theme.border_control
    };
    let (fill, text) = if card.disabled() {
        (theme.bg1, theme.text_disabled)
    } else {
        (theme.bg0, theme.text_primary)
    };
    pen.outlined(rect, RADIUS_CONTROL, fill, border);
    let label = pen.elided(
        face,
        card.server.protocol.label(),
        rect.w - INPUT_TEXT_INSET - CHEVRON.0 - INPUT_TEXT_INSET,
    );
    pen.text(
        face,
        &label,
        (rect.x + INPUT_TEXT_INSET, rect.y + INPUT_TEXT_TOP),
        BODY,
        text,
    );
    let s = pen.scale;
    let middle = rect.x + rect.w - CHEVRON.0 / 2.0 - INPUT_TEXT_INSET / 2.0;
    let centre = rect.y + rect.h / 2.0;
    let point = |x: f32, y: f32| (x * s, y * s);
    let width = 1.5 * s;
    let half = CHEVRON.1;
    pen.canvas.line(
        point(middle - half, centre - half / 2.0),
        point(middle, centre + half / 2.0),
        width,
        text,
    );
    pen.canvas.line(
        point(middle, centre + half / 2.0),
        point(middle + half, centre - half / 2.0),
        width,
        text,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_pgwire::client::SslMode;

    fn read(text: &str) -> ServerUrl {
        parse_server_url(text).unwrap_or_else(|| panic!("{text} reads as a server URL"))
    }

    fn typed(form: &mut ServerForm, text: &str) {
        form.url.set(text);
        form.url_edited();
    }

    #[test]
    fn the_checklist_url_fills_host_port_database_and_user() {
        let url = read("postgresql://ada@db:5433/shop");
        assert_eq!(url.protocol, Protocol::PostgreSql);
        assert_eq!(
            (
                url.user.as_str(),
                url.host.as_str(),
                url.port.as_deref(),
                url.database.as_str()
            ),
            ("ada", "db", Some("5433"), "shop")
        );
        assert!(url.use_tls);
        assert!(url.well_formed);
        assert!(!read("postgresql://ada@db:5433/shop?sslmode=disable").use_tls);
        assert!(read("postgresql://ada@db/shop?sslmode=require").use_tls);
    }

    #[test]
    fn every_scheme_names_its_protocol_and_eggdb_is_an_alias_of_postgresql() {
        for (text, protocol) in [
            ("postgres://h", Protocol::PostgreSql),
            ("EGGDB://h", Protocol::PostgreSql),
            ("mysql://h", Protocol::MySql),
            ("sqlserver://h", Protocol::SqlServer),
            ("redis://h", Protocol::Redis),
        ] {
            assert_eq!(read(text).protocol, protocol, "{text}");
        }
        assert_eq!(read("eggdb://h").scheme, "eggdb");
        assert!(!read("mysql://h?tls=false").use_tls);
        assert!(read("mysql://h?sslmode=disable").use_tls);
        assert!(parse_server_url("http://example.com").is_none());
        assert!(parse_server_url("postgresql:/").is_none());
        assert!(parse_server_url("").is_none());
    }

    #[test]
    fn partial_and_broken_urls_read_as_far_as_they_go() {
        let partial = read("postgresql://ada@d");
        assert_eq!((partial.user.as_str(), partial.host.as_str()), ("ada", "d"));
        assert!(partial.well_formed);
        assert!(!read("postgresql://").well_formed);
        assert!(!read("postgresql://db:port/shop").well_formed);
        assert!(!read("postgresql://db:0").well_formed);
        assert!(!read("postgresql://db:70000").well_formed);
        assert!(!read("postgresql://a%2@db").well_formed);
        assert!(!read("postgresql://db?sslmode=sometimes").well_formed);
        assert!(!read("postgresql://[::1/shop").well_formed);
        let ipv6 = read("postgresql://ada@[::1]:6000/shop");
        assert_eq!(
            (ipv6.host.as_str(), ipv6.port.as_deref()),
            ("::1", Some("6000"))
        );
        let escaped = read("postgresql://a%40b@db/my%20shop");
        assert_eq!(
            (escaped.user.as_str(), escaped.database.as_str()),
            ("a@b", "my shop")
        );
    }

    #[test]
    fn typing_the_url_fills_the_fields_and_a_password_moves_out_of_it() {
        let mut form = ServerForm::new();
        typed(&mut form, "postgresql://ada@db:5433/shop?sslmode=disable");
        assert_eq!(form.host.value(), "db");
        assert_eq!(form.port.value(), "5433");
        assert_eq!(form.database.value(), "shop");
        assert_eq!(form.user.value(), "ada");
        assert!(!form.use_tls);
        typed(&mut form, "postgresql://ada:s3cret@db/shop");
        assert_eq!(form.password.value(), "s3cret");
        form.forget_url_password();
        assert_eq!(form.url.value(), "postgresql://ada@db:5432/shop");
        assert!(!form.url.value().contains("s3cret"));
        typed(&mut form, "redis://cache");
        assert_eq!(form.protocol, Protocol::Redis);
        assert_eq!(form.port.value(), "6379");
        typed(&mut form, "not a url");
        assert_eq!(
            form.host.value(),
            "cache",
            "an unknown scheme leaves the fields"
        );
    }

    #[test]
    fn editing_a_field_rewrites_the_url_without_the_password() {
        let mut form = ServerForm::new();
        form.host.set("db.example.com");
        form.user.set("a@b");
        form.database.set("shop");
        form.password.set("hunter2");
        form.field_edited();
        assert_eq!(
            form.url.value(),
            "postgresql://a%40b@db.example.com:5432/shop"
        );
        form.toggle_tls();
        assert_eq!(
            form.url.value(),
            "postgresql://a%40b@db.example.com:5432/shop?sslmode=disable"
        );
        form.choose_protocol(Protocol::MySql);
        assert_eq!(form.port.value(), "3306");
        assert_eq!(
            form.url.value(),
            "mysql://a%40b@db.example.com:3306/shop?tls=false"
        );
        form.port.set("4000");
        form.choose_protocol(Protocol::SqlServer);
        assert_eq!(form.port.value(), "4000", "a port the user chose stays");
        form.host.set("::1");
        form.field_edited();
        assert!(
            form.url
                .value()
                .starts_with("sqlserver://a%40b@[::1]:4000/")
        );
        let mut aliased = ServerForm::new();
        typed(&mut aliased, "eggdb://ada@db/shop");
        aliased.toggle_tls();
        assert_eq!(
            aliased.url.value(),
            "eggdb://ada@db:5432/shop?sslmode=disable"
        );
    }

    #[test]
    fn protocols_cycle_in_the_menu_order() {
        assert_eq!(Protocol::PostgreSql.next(), Protocol::MySql);
        assert_eq!(Protocol::Redis.next(), Protocol::PostgreSql);
        assert_eq!(Protocol::PostgreSql.previous(), Protocol::Redis);
        let ports: Vec<u16> = Protocol::ALL.iter().map(|p| p.default_port()).collect();
        assert_eq!(ports, [5432, 3306, 1433, 6379]);
    }

    #[test]
    fn connect_options_take_the_fields_with_libpq_defaults() {
        let mut form = ServerForm::new();
        let options = form.connect_options().unwrap();
        assert_eq!(
            (
                options.host.as_str(),
                options.port,
                options.user.as_str(),
                options.database.as_str()
            ),
            ("localhost", 5432, "eggdb", "eggdb")
        );
        assert_eq!(options.ssl_mode, SslMode::Require);
        assert_eq!(options.password, None);
        typed(&mut form, "postgresql://ada@db:5433/shop?sslmode=disable");
        form.password.set("pw");
        let options = form.connect_options().unwrap();
        assert_eq!(
            (
                options.host.as_str(),
                options.port,
                options.user.as_str(),
                options.database.as_str()
            ),
            ("db", 5433, "ada", "shop")
        );
        assert_eq!(options.ssl_mode, SslMode::Disable);
        assert_eq!(options.password.as_deref(), Some("pw"));
        assert_eq!(form.place(), "db:5433");
    }

    #[test]
    fn a_url_that_does_not_parse_and_other_protocols_fail_with_their_lines() {
        let mut form = ServerForm::new();
        form.url.set("postgresql://db:port");
        let failure = form.connect_options().unwrap_err();
        assert_eq!(failure.error_line(), "Error: the URL is not a database URL");
        assert_eq!(failure.field, FailureField::Url);
        form.url.set("ftp://db");
        assert_eq!(form.connect_options().unwrap_err().field, FailureField::Url);
        let mut other = ServerForm::new();
        other.choose_protocol(Protocol::MySql);
        let failure = other.connect_options().unwrap_err();
        assert_eq!(failure.field, FailureField::Protocol);
        assert!(failure.error_line().contains("MySQL protocol"));
        let mut port = ServerForm::new();
        port.port.set("99999");
        assert_eq!(port.connect_options().unwrap_err().field, FailureField::Url);
        let mut written = ServerForm::new();
        written.toggle_tls();
        assert_eq!(written.url.value(), "postgresql://:5432?sslmode=disable");
        let options = written
            .connect_options()
            .expect("a URL the fields wrote is theirs");
        assert_eq!(options.host, "localhost");
    }
}
