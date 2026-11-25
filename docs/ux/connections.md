# Connections: MySQL and TDS, RESP over TLS, certificate checks, the password's Show button and recent connections

Designed by Claude Fable (D15) for roadmap M6e: connecting over the MySQL protocol and SQL
Server's TDS (D274), `rediss://` with the pill `RESP · TLS` (D329), `verify-ca` / `verify-full`
certificate checks, the password field's `Show` button and the recent connections in the
sidebar. Every token, rect and role not restated is [gui.md](gui.md)'s connect screen,
[first-screen.md](first-screen.md)'s card and [workspace.md](workspace.md)'s section 1 (the
`URL` row and the packed fields of 1.4). Card-relative positions are `(x, y)` from `cx, cy`.

## 1. The protocols

### 1.1 The dropdown and the URL
The `Protocol` dropdown (1.4: `(24, 208, 168, 28)`) lists `PostgreSQL protocol`, `MySQL
protocol`, `SQL Server (TDS)`, `Redis (RESP)`, now all four connectable; its menu is the dialect
pill's popover (2.2) opened by click, `Space`, `Enter` or `Alt+Down`, with the check icon on the
current one. The URL field and the dropdown agree both ways:

| Scheme | Protocol | Port | TLS switch in the URL | The tab's dialect after connecting |
|--------|----------|------|------|------|
| `postgresql://`, `postgres://`, `eggdb://` | PostgreSQL | 5432 | `?sslmode=disable` off; `require` (the default), `verify-ca`, `verify-full` set the check (section 2) | `PostgreSQL` |
| `mysql://` | MySQL | 3306 | `?tls=false` off; `?ssl-mode=VERIFY_CA` / `VERIFY_IDENTITY` set the check | `MySQL` |
| `sqlserver://`, `mssql://` | SQL Server (TDS) | 1433 | `?encrypt=false` off; `?trustServerCertificate=true` means `Encrypt only` | `SQL Server` |
| `redis://` | Redis (RESP) | 6379 | TLS off (the scheme says plain) | none (a keys tab, 10.10) |
| `rediss://` | Redis (RESP) | 6379 | TLS on, `Verify server (full)` unless `?insecure=true` | none |

A URL in another scheme fails with 1.6's `Error: the URL is not a database URL`. The `Database`
field is MySQL's schema, SQL Server's initial database and RESP's `db` index (`0` to `15`, from
the URL's path `/3`); `User` and `Password` are MySQL's account, SQL Server's SQL login (Windows
authentication is not offered: the client has no SSPI) and RESP's `AUTH` (user optional, the
`default` user when empty).

### 1.2 What a connection speaks
- **MySQL:** the public client/server protocol with `caching_sha2_password` and
  `mysql_native_password`, `CLIENT_SSL` when TLS is on, `utf8mb4`; the tab's dialect is `MySQL`,
  the tree reads `information_schema` (D276's server path), and the status bar reads `Connected
  · db.example.com:3306 · MySQL` (`· MySQL · TLS`).
- **SQL Server (TDS):** TDS 7.4 `PRELOGIN` with encryption negotiated, `LOGIN7` with the SQL
  login, `SQL_BATCH` for runs; the dialect is `SQL Server`, the tree reads `sys.tables` and
  `INFORMATION_SCHEMA`, the status bar `· TDS`.
- **RESP:** 10.10's `HELLO 3` / RESP2 fallback and `AUTH`; the tree's level 0 row's right text is
  `RESP`, `RESP · TLS` over `rediss://`, and the status bar `Connected · cache.example.com:6379 ·
  RESP · TLS`.

The toolbar's connection text is `host:port`; the status bar's third item names the protocol
only when it is not PostgreSQL (a PostgreSQL server reads as 1.7 says). The sidebar's tree
groups follow the catalog each protocol answers (4.1).

### 1.3 Failures by protocol
1.6's table grows; line 1 is the server's text with its code first, line 2 ours:

| Cause | Line 1 | Line 2 (`Try:`) |
|-------|--------|------|
| MySQL access denied | `Error 1045: Access denied for user 'ada'@'10.0.0.5' (using password: YES)` | `check the user name and password` |
| MySQL unknown database | `Error 1049: Unknown database 'shop'` | `check the database name, or leave it empty` |
| MySQL TLS refused by the server | `Error 2026: the server does not offer TLS` | `uncheck "Use TLS" only on a network you trust` |
| SQL Server login failed | `Error 18456: Login failed for user 'ada'.` | `check the user name and password` |
| SQL Server database missing | `Error 4060: Cannot open database "shop" requested by the login.` | `check the database name` |
| SQL Server encryption refused | `Error: the server requires an encrypted connection` | `check "Use TLS"` |
| RESP `NOAUTH` / `WRONGPASS` | `Error: NOAUTH Authentication required.` / `Error: WRONGPASS invalid username-password pair` | `enter the password` / `check the user name and password` |
| RESP `rediss://` to a plain port | `Error: cannot connect to cache:6379: the server does not speak TLS` | `use redis:// if the server has no TLS listener` |
| certificate not trusted (any protocol) | `Error: cannot verify db.example.com:5432: the certificate is not signed by a trusted authority` | `choose a CA file below, or "Encrypt only" on a network you trust` |
| host name mismatch | `Error: cannot verify db.example.com:5432: the certificate is for "db-internal"` | `connect with the name on the certificate, or choose "Verify CA only"` |
| certificate expired | `Error: cannot verify db.example.com:5432: the certificate expired on 2026-10-01` | `renew the server's certificate` |

Focus goes to Password for the authentication rows, Database for the database rows, the
`Certificate check` dropdown for the certificate rows, `Use TLS` for the TLS rows.

## 2. TLS and the certificate check
Under `Use TLS` (1.4: `(24, 384)`) the card gains one row while TLS is on: label `Certificate
check` at `(24, 408)`, dropdown `(24, 430, 250, 28)` and, to its right, `CA file` input `(286,
430, 166, 28)` with `Browse…` `(460, 430, 76, 28)` where the platform has a file dialog (the
input is `(286, 430, 250, 28)` elsewhere). The card's packed fields of 1.4 move up 20 px to
make room (`URL` at y 100, the last control ends at 458, the status line at 476, the buttons as
before), and the TLS warning line of 1.4 takes the dropdown row's place while TLS is off.

| Choice | Means | libpq name / MySQL / TDS |
|--------|-------|------|
| `Verify server (full)` (default) | the chain to a trusted CA and the host name | `verify-full` / `VERIFY_IDENTITY` / `Encrypt=true;TrustServerCertificate=false` |
| `Verify CA only` | the chain only | `verify-ca` / `VERIFY_CA` / the same with the host name check skipped |
| `Encrypt only` | no check; the warning `Warning: anyone on the network can impersonate the server` in `body` `warning` at `(24, 462)`, 332 px, elided | `require` / `REQUIRED` / `TrustServerCertificate=true` |

Trusted CAs are the OS's store (macOS the system keychain's trust settings through Security
framework, Windows `CertOpenSystemStore("ROOT")`, Linux and the BSDs the first of
`/etc/ssl/certs/ca-certificates.crt`, `/etc/pki/tls/certs/ca-bundle.crt`,
`/etc/ssl/cert.pem`, the browser its own) plus the `CA file` (PEM, one or more certificates)
when given; the chain, names and dates are checked by our own X.509 reader (D1, the one
`eggdb serve` already uses, D23). The dropdown is one focus stop stepping with `Up` / `Down`;
`CA file` and `Browse…` are stops after it; the URL gains `?sslmode=verify-ca` /
`&sslrootcert=<path>` (and each protocol's spelling) as the fields change. The successful
connection's status bar shows `· TLS` for the first two choices and `· TLS (unverified)` in
`warning` for `Encrypt only`, and the tree's level 0 right text matches.

## 3. The password field's Show button
Inside the right edge of the Password input (1.4: `(286, 340, 250, 28)`): a ghost button
`(486, 342, 48, 24)` with the label `Show` (`Hide` while shown) in `small` `textSecondary`,
`radiusControl`; the input's text area ends 8 px before it. Shown means the characters draw in
`code` instead of the filled circles; the caret and selection work the same either way. The
button is a `Tab` stop right after the Password input, a `button` named `Show password` with
`pressed` true while shown (`aria-pressed`), the input's role a password field (`AXSecureTextField`
/ `Edit` with `IsPassword` / `ATSPI_ROLE_PASSWORD_TEXT` / `type=password`) while masked and a plain
text field while shown. `Alt+S` toggles it from the input. The masking returns on `Connect`, on a
change of segment and when the card is shown again, never persisting; the value is never put in
the URL field either way. Hover `bg2`, pressed `bg2` with `borderStrong`, focused `focusRing`.

## 4. Recent connections in the sidebar
The connect screen's sidebar (gui.md: `(0, 44, 260, 732)`, `bg1`) lists what was opened before:

| Item | Rect | Drawing |
|------|------|---------|
| heading | text at (12, 60) | `small` `textSecondary` upper-case: `RECENT` |
| rows | from y 80, 24 tall, label at x 36, icon 12 × 12 at (16, y + 6) | gui.md's tree rows (level 0, no chevron): a file row's icon is the database icon and its label the file name, right text the directory (`~/data`); a server row's icon is a 12 × 12 server (rect (0, 1, 12, 4) and (0, 7, 12, 4) r 1 with a 2 px dot at (9, 3) and (9, 9)) and its label `host:port`, right text the protocol (`PostgreSQL`, `MySQL`, `TDS`, `RESP`), `· TLS` when so |
| empty | text at (12, 80) | `body` `textSecondary`: `Nothing opened yet` |
| `Coming from another client? Pick a layout` | (0, 744, 260, 32) | layouts-contents.md section 7 |

At most 20 rows, most recent first; the same file or `protocol://user@host:port/db` moves to the
top rather than repeating. `Enter` or a double-click connects at once (a server row with a
remembered password connects; without one it fills the card's server segment and focuses
Password); a single click fills the card without connecting; `Delete` forgets the row with the
toast `Forgot db.example.com:5432` and an `Undo` ghost button in the toast for 8 s; the row's
menu (`Shift+F10`, right-click) has `Connect`, `Fill the form`, `Copy URL` (never the password),
`Forget`. Typing a letter jumps. The list is a `tree` named `Recent connections` of `treeitem`s
with the right text as the description; the sidebar's filter field filters it as it filters
objects.

Stored in `gui-settings` as `recent.<n>.kind = file|server`, `recent.<n>.path` or
`recent.<n>.url` (the URL without a password), `recent.<n>.tls`, `recent.<n>.check`,
`recent.<n>.ca_file`, `recent.<n>.read_only`, `recent.<n>.opened = <unix seconds>`. A password is
stored only when the card's `Remember password` checkbox (added under the Password input at
`(286, 372)`, 16 × 16, label at x 310, unchecked by default) was checked, and then in the OS's
credential store, never in `gui-settings`: macOS the keychain (`SecItemAdd`, service `EggDB`,
account the URL), Windows the Credential Manager (`CredWriteW`, target `EggDB:<url>`), Linux and
the BSDs the Secret Service over D-Bus (`org.freedesktop.secrets`, our own message encoding, D1)
when it answers, else the checkbox is disabled with the tooltip `No credential store is
available`; the browser never stores one. `Forget` removes the credential too.

## 5. Accessibility additions
| Element | Role | Name | Value / states |
|---------|------|------|------|
| `Protocol` dropdown | combo box | `Protocol` | the choice; `expanded` with its `menu` of `menuitemradio` |
| `Certificate check` | combo box | `Certificate check` | the choice; the warning line is its description under `Encrypt only` |
| `CA file`, `Browse…` | text field, button | `CA file`, `Browse for a CA file` | |
| `Show` | toggle button | `Show password` | `pressed` |
| Password | password text field, or text field while shown | `Password` | |
| `Remember password` | checkbox | `Remember password` | checked; disabled with the description `No credential store is available` |
| recent list | tree | `Recent connections` | |
| recent row | tree item | `shop.eggdb`, `db.example.com:5432` | description `~/data` / `PostgreSQL, TLS`; position and set size |
| toast with `Undo` | status with a child button | the text | `Undo` is a `button` |

Announcements: the card's error lines as 1.6 (polite); `Forgot …` (polite); the card's `Layout:`
line is unchanged.

## 6. Acceptance checklist
- [ ] png: typing `mysql://ada@db:3306/shop` sets the dropdown to `MySQL protocol`, Port 3306
      and the `Certificate check` row; a stub server refusing the password shows `Error 1045: …`
      with the focus on Password; after connecting the status bar reads `Connected · db:3306 ·
      MySQL · TLS` and the dialect pill `MySQL`.
- [ ] png: `sqlserver://sa@db/shop` connects over a stub TDS login; `?encrypt=false` unchecks
      `Use TLS` and shows the warning; the tree reads `dbo.customers` from the stub's `sys.tables`.
- [ ] png: `rediss://cache:6379/3` checks `Use TLS`, sets Database `3`, and after connecting the
      tree's level 0 right text is `RESP · TLS` and the keys tab opens on `resp3`.
- [ ] png: the card with TLS on shows `Certificate check` at `(cx + 24, cy + 430)`; `Encrypt
      only` draws the warning; a stub certificate for another host fails with the host-name line
      and focuses the dropdown; `Verify CA only` then connects and the status bar reads `· TLS`.
- [ ] png: `Show` at `(cx + 486, cy + 342)` turns the circles into `code` text; tree: the field's
      role changes from the password role to a text field and the button reports `pressed`.
- [ ] png: the sidebar with three recent rows (a file, a PostgreSQL server with TLS, a RESP
      server) draws the icons, labels and right texts; `Delete` on the second shows the toast with
      `Undo`; `Undo` restores it in place; tree: the list is a `tree` of three `treeitem`s.
- [ ] settings stub: after a connect with `Remember password` the `gui-settings` file holds the
      URL without the password and the credential-store stub holds it under the URL; `Forget`
      removes both.

## Decided by this design
- Four protocols connect from one card; the dialect follows the protocol and the status bar
  names the protocol only when it is not PostgreSQL.
- Certificate checks are a dropdown under `Use TLS` with `Verify server (full)` as the default,
  a `CA file` beside it, and `Encrypt only` always carrying a warning line; the OS's trust store
  is read through our own declarations.
- Windows authentication for SQL Server is not offered; SQL logins only.
- `Show` is a toggle button inside the password field, a `Tab` stop with `Alt+S`, that never
  persists.
- Recent connections live in the connect screen's sidebar as a tree, 20 at most, with `Undo`
  on forget; passwords only in the OS credential store and only when asked.
