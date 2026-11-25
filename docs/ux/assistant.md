# AI help in the client: the Assistant panel over the built-in MCP server

Designed by Claude Fable (D15) for roadmap M6e: an assistant panel that uses the built-in MCP
tools (`run_sql`, `question_get`, `explain_query`, `describe_table`, `sample_rows`, `write_sql`
and the model tools), with permissions, confirmation of destructive actions and a visible
action log (D11, D45, D119, D121, D255, D1356). client-brief.md principle 5 holds: AI is a
colleague in the room, not a wizard in the way; there is no chat box in the main window by
default, the agent is a presence chip in the status bar and an `Activity` panel, and the
assistant appears only when asked for. Every token, rect and role not restated is
[gui.md](gui.md)'s and [workspace.md](workspace.md)'s. Rects are at 1280 × 800, unchanged by this.

## 1. What the assistant is
The client holds one **MCP client** connected to EggDB's own MCP server for the connection: for
a file, `eggdb mcp <file>` run as a child process over stdio (D119), started at the first use;
for a server, the server's MCP endpoint (D1359's key grants per agent) over the same connection
options. The model is the one configured for the engine's `eggdb_ai` (D779: an endpoint and a
model name, set in `Settings > Assistant` as `assistant.endpoint`, `assistant.model`, the API key
in the OS credential store of connections.md 4; the engine's own `question_get` uses the same
settings, so text-to-SQL answers are the same in the GUI, the CLI and over MCP). Without an
endpoint the panel shows a one-card setup (`Choose a model endpoint to use the assistant`, with
the two fields and `Save`) and nothing else. The assistant never talks to the database except
through the MCP tools, with the grants of section 4, so what it can read and do is exactly what
any agent could with the same key; the panel is a window on that, not a back door.

## 2. The panel
`Ctrl/Cmd+Shift+A`, the toolbar's agent chip (section 5) or `View > Assistant` open it in the
inspector's place: docked at the right, 360 wide (240–560, splitter as 10.5's, key
`<layout>.assistant_width`), from 1280 px; an overlay like the inspector's (4.3) under that, 360
of the window's width, `Esc` closes. Opening it while the inspector is docked replaces the
inspector (`Ctrl/Cmd+I` brings the inspector back and hides the assistant; the two never show
together, the brief's Activity being part of this panel). Rect at 1280 × 800: `(920, 44, 360,
732)`, `bg1`, 1 px `border` at x 920; the tabs, editor and results shrink to 660 wide.

| Region | Rect | Drawing |
|--------|------|---------|
| header | (920, 44, 360, 36), `bg1`, 1 px `border` below | `bodyBold` `Assistant` at (932, 53); right: the grants pill (section 4) at `(1096, 48, 120, 28)` reading `read-only ▾` / `can write ▾`; `New` ghost (clears the conversation) and the close icon (24 × 24 hit) |
| conversation | (920, 80, 360, 580), scrolling as one (9.6's scrollbar at x 1270) | cards of section 3, 8 px apart, 12 px from the edges, newest at the bottom, scrolled to the newest unless the user scrolled up (then a `↓ New` pill at the bottom) |
| question box | (920, 660, 360, 116), `bg1`, 1 px `border` above | a multi-line input `(932, 668, 336, 68)` (gui.md's input, 3 lines of 20, growing to 6 with the region, placeholder `Ask about this database…`), under it a row: `Send` primary `(1196, 744, 72, 28)` (`Ctrl/Cmd+Enter`; `Enter` alone is a newline), `Stop` while a turn runs, and a `small` `textSecondary` line at (932, 748) `uses run_sql, describe_table, explain_query… as ada` |

Focus on open goes to the question box; `Ctrl/Cmd+Shift+A` again closes and returns focus. The
panel is one `Tab` region after the results (before the status bar): header controls, the
conversation (a `Tab` stop with the cards' keys), the question box, `Send`.

## 3. Cards
Each turn draws cards `(932, y, 336, h)`, `bg0`, 1 px `border`, `radiusCard`, body `body`
`textPrimary` wrapped at words with 12 px padding; code in `code` in a `bg1` box; nothing in
the panel is ever drawn as a button the model asked for (D45: the model's text is text).

| Card | Drawing | Keys and actions |
|------|---------|------|
| the question | `bg2`, right-aligned to the panel, the text | `Edit` on hover re-fills the box |
| an answer | the model's text, Markdown's headings, lists, bold and code rendered (notebooks.md's renderer), links shown as text with the URL in the tooltip and opened only by `Ctrl/Cmd` + click | `Copy` ghost (the whole text) |
| a statement card (an answer that holds SQL, or a `question_get` result) | header `small` `textSecondary` `SQL · PostgreSQL · checked` (`question_get`'s `checked` flag; `not checked: <problem>` in `warning` when it reports one), the statement in a `code` box of up to 12 lines (then scroll), buttons under it: `Open in tab` (ghost: a new `Query n` with the text, not run), `Run` (secondary: runs it through `run_sql` as a tool call, section 4's rules; disabled while `not checked`), `Explain` (ghost: `explain_query`) | `Enter` on the card is `Open in tab` |
| a tool call | one line while running: a 12 px spinner, `code` `run_sql(SELECT count(*) FROM orders)` cut at the card's width; when done, the line gets the check icon (or the error icon) and `· 12 ms · 1 row`, and a chevron that expands the result as a small grid (rows 20, `small`, at most 10 rows then `Open in results` which puts the rows in the active tab's grid as a read-only result named by the tool) or the tool's text | `Right` / `Left` expand and collapse; `Esc` while running stops the turn |
| a confirmation (section 4) | `warningFill` card: `The assistant wants to run a statement that changes data`, the statement box, the classification line (`UPDATE orders · 1,204 rows match · no key pinned`), buttons `Allow once` (secondary), `Deny` (primary and default), `Open in tab instead` (ghost) | focus moves to the card; `Enter` is `Deny`; `Alt+A` allows |
| an error | 2.5's strip look inside a card: `Error 42703: column "nmae" does not exist` and the model's next attempt follows as its own tool card | |

A turn is: the question is sent with the conversation's previous turns, the connection's
schema resource (`eggdb://schema`) and the tool list; each tool call the model makes is a card;
the final text is the answer. `Stop` ends the turn after the current tool call. The model's
text can contain anything, so it is drawn as text and never interpreted: a statement in it runs
only when the user presses `Run`, and tool calls run only as section 4 allows.

## 4. Permissions and confirmation
The grants pill in the header opens a popover (9.4's style, 280 wide, rows 28) that is the
agent's grant set for this connection, the same model `eggdb mcp` enforces (D121, D328, D1358,
D1359), stored per connection in `gui-settings` (`assistant.<connection>.grants`) and applied
when the MCP client starts (a change restarts it):

| Row | Kind | Means |
|-----|------|-------|
| `Read` | check, always on | `run_sql`, `describe_table`, `sample_rows`, `explain_query`, `list_tables`, `question_get` and the model `*_get` tools |
| `Write` | check, off by default | adds `write_sql`, `graph_run`, `document_run`, `resp_run` (`--allow-write`); turning it on asks 9.1's `Let the assistant write to shop?` with the body `Statements that change data still ask you first.` (`Allow writes` secondary / `Keep read-only` default) |
| `Tables ▸` | submenu | `All tables` or a checklist of tables (`--tables`); a table unchecked is invisible to the model |
| `Keys ▸` | submenu | key prefixes (`--keys`) as an input of comma-separated prefixes |
| `Confirm every write` | check, on | every `write_sql` asks, not only destructive ones |
| `Time limit 30 s ▾` | dropdown | `--time-limit` |

With `Write` off, a statement that writes answers the model `read_only` with the fix `Send it
with write_sql` (D121) and the card shows that error; the model may ask for the grant in words,
which the user grants only through the pill. With `Write` on, a `write_sql` call that D255
classifies as destructive (or any write under `Confirm every write`) pauses the turn with the
confirmation card: `Allow once` runs it (`confirm: true`), `Deny` answers the model `denied by
the user`, `Open in tab instead` puts it in a new tab and denies. The pill reads `read-only`
(`textSecondary`) or `can write` (`warning` text) and the status bar's agent chip (section 5)
says the same. A `DROP`, `TRUNCATE` or `DELETE` without a key is also refused outright when the
connection is read-only, with 10.2's words. Every tool call, allowed or denied, is written to
the engine's audit log (`eggdb audit`, D121) with the agent name `gui-assistant` and the
connection's user.

## 5. The agent chip and the Activity log
The toolbar's right end (before `more`) gains a chip `(1128, 8, 104, 28)` (the dialect pill
moves left by 112 when the chip shows; the golden frame has no chip, since no agent has
connected): a secondary button with a filled circle d 8 (`success` while an agent is connected
and idle, `accent` while a call runs, `textDisabled` after it disconnected) at x 1140 and the
label `agent` / `2 agents` at x 1154. The agents are every MCP session the engine reports on
this connection (the assistant's, and any outside agent using `eggdb mcp` on the file or the
server's endpoint, D1359), so a person sees an agent at work even when the panel is closed. Click,
`Space` or `Enter` open the panel on its **Activity** view: a segmented control in the panel's
header (`Chat` / `Activity`, 1.4's style at `(932, 48, 160, 28)` when any outside agent exists;
without one the header has no segments and `Activity` is a block at the conversation's end):
a list of every agent action on this connection from the audit log, newest first, rows 44:
`bodyBold` the tool and its first argument (`run_sql · SELECT count(*) FROM orders`), `small`
`textSecondary` `ops-agent · 14:02:11 · 12 ms · allowed` (`denied`, `error: …`, `confirmed by
you`), a chevron expanding the full arguments and result text; a filter field (`Filter
actions`) and an `Agents ▾` dropdown of the agent names; `Revoke…` on an outside agent's row
(runs the engine's revoke of that key after 10.2's dialog, D1359). Rows are a `list` of
`listitem`s with `expanded`; a new row from an outside agent is announced politely (`ops-agent
ran run_sql`) at most every 10 s, and a pending confirmation for an outside agent (the engine's
confirmation queue, D121) shows the confirmation card at the list's top with `Allow once` /
`Deny`, the chip's dot `warning` and a toast `ops-agent asks to run UPDATE orders` with `Show`.

## 6. Keys
| Key | Does |
|-----|------|
| `Ctrl/Cmd+Shift+A` | open / close the panel (focus to the box / back) |
| `Ctrl/Cmd+Enter` in the box | send; `Enter` newline; `Up` in an empty box recalls the last question |
| `Esc` | stop the running turn; else close the overlay (docked: return focus to the editor) |
| `Ctrl/Cmd+Shift+L` | focus the conversation; `Up` / `Down` move between cards, `Right` / `Left` expand / collapse a tool card, `Enter` the card's first action, `Ctrl/Cmd+C` copies the card's text |
| `Alt+A` / `Alt+D` | allow / deny the focused confirmation |
| `Ctrl/Cmd+Shift+G` | open the grants pill |
| from the editor: `Ctrl/Cmd+Shift+?` | `Ask about this` sends the selection (or the statement under the caret) with the question box pre-filled `Explain this statement` |
| from the error strip: `Ask the assistant` (a second ghost button when the panel is configured) | sends the error's text and the statement |

## 7. Accessibility
| Element | Role | Name | Value / states |
|---------|------|------|------|
| panel | `complementary` (docked) / `dialog` not modal (overlay) | `Assistant` | |
| grants pill | `button` `aria-haspopup=menu` | `Assistant permissions, read-only` | the popover a `menu` of `menuitemcheckbox`es |
| conversation | `list` | `Conversation` | one `Tab` stop; the current card focused |
| question card, answer card | `listitem` | `You:` / `Assistant:` + the first line | the text as the content |
| statement card | `listitem` with child `button`s | `Statement` | `Open in tab`, `Run`, `Explain`; `Run` disabled with the reason in its description |
| tool card | `listitem` `aria-busy` while running | `run_sql, running` → `run_sql, done, 12 milliseconds, 1 row` | `expanded`; the result grid a `table` |
| confirmation card | `alertdialog` region inside the list (focus moved in; `Tab` cycles its three buttons) | `Confirm a write` | `Deny` default |
| question box | `textbox` `aria-multiline` | `Question` | |
| agent chip | `button` | `Agents: 1 connected, idle` | |
| Activity rows | `listitem` | the tool and argument | description the agent, time, outcome |

Announcements: an answer's arrival (`Assistant answered`, polite, then the reader reads the card
on focus); a tool card's completion (polite, `run_sql done, 1 row`) at most every 2 s; a
confirmation (assertive, the card's title); an outside agent's action (polite, throttled).

## 8. Acceptance checklist
- [ ] png: `Ctrl+Shift+A` at 1280 × 800 draws the panel at `(920, 44, 360, 732)` with the header,
      an empty conversation reading `Ask about this database…` in the box, the grants pill
      `read-only`, the centre 660 wide; without an endpoint the setup card instead.
- [ ] png: a stub turn `How many orders?` draws the question card, a tool card `run_sql(SELECT
      count(*) FROM orders)` with the spinner at 100 ms, then done with `· 12 ms · 1 row`,
      expanded to a one-row grid, then the answer card; `Enter` on the statement card opens
      `Query 2` with the SQL, not run.
- [ ] png: with `Write` off a stub `write_sql` call shows the `read_only` error card; turning
      `Write` on shows the `Let the assistant write to shop?` dialog; with it on, a stub `UPDATE
      orders SET …` without a key shows the confirmation card on `warningFill` with `Deny` focused;
      `Alt+A` runs it and the tool card reads `confirmed by you`.
- [ ] png: the chip at `(1128, 8, 104, 28)` with a `success` dot and `agent` once the MCP client
      connected; with a stub outside agent `2 agents`; its `Activity` view lists both agents'
      rows with the `Agents ▾` filter; an outside agent's pending confirmation shows the card at
      the top and the toast with `Show`.
- [ ] audit stub: every tool call of the turn, including the denied one, is in the audit log
      with `gui-assistant` and the user.
- [ ] tree: the panel is a `complementary` region; the confirmation card an `alertdialog` with
      focus inside; the grants popover a `menu` of `menuitemcheckbox`es with `Read` checked and
      disabled; the chip's name carries the count and state.
- [ ] png: `Ctrl+Shift+?` in the editor on `SELECT nmae FROM customers` opens the panel with the
      statement quoted in the question card; the error strip shows `Ask the assistant` when the
      endpoint is set.

## Decided by this design
- The assistant is an MCP client to EggDB's own MCP server, with the same grants any agent
  gets, configured in the engine's `eggdb_ai` settings; it has no other way to the data.
- It lives in the inspector's place (360 wide) and never beside the inspector; the main window
  keeps no chat box until asked.
- The model's text is drawn as text: statements run only by the user's `Run`, tool calls only
  under the grants, destructive ones only after `Allow once` on a card whose default is `Deny`.
- `Write` is off by default and turned on through a dialog; `Confirm every write` is on.
- The toolbar's agent chip shows every agent on the connection, and the panel's `Activity` view
  is the audit log with filters and `Revoke…`; outside agents' confirmations arrive as cards and
  toasts.
- Every call is audited under `gui-assistant`.
