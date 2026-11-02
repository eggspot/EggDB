# The EggDB client: design brief

By Claude Fable (CLAUDE.md note 14, D15), recorded from issue #29. This is the brief every client
spec in `docs/ux/` builds on; [gui.md](gui.md) (the desktop window, its tokens and components),
[gui-layouts.md](gui-layouts.md) (the familiar layouts), [first-screen.md](first-screen.md) (the
connect card, the first slice), [cli.md](cli.md), [ui.md](ui.md) and [playground.md](playground.md)
are its specs. Where the brief and a spec once differed, the spec's figures stand and are used
here: the sidebar is 260 px (not 240), the type scale is 11, 13, 15 and 20 px, the token names are
gui.md's (`bg0`, `textPrimary`, `focusRing`, …, not `bg.base`, `fg.default`), the accent is the blue
`accent` token and the yolk `brandYolk` colours only the egg mark (yolk on white is 2.2:1, too
little for text or a focus ring), and the familiar layouts live in gui-layouts.md rather than one
TOML file each.

## 1. Design principles
1. **One object model, one grammar, every data model.** Every model (tables, keys, documents,
   indexes, series, graphs, vectors, geo, queues) is a *node* in the same explorer tree. It opens
   as a *tab* in the same editor and returns *rows* in the same grid, with cells typed by the
   model (a JSON cell, a vector cell, a TTL cell). The chrome never changes with the model; only
   cell renderers and the editor's language do.
2. **The query is the unit of work.** Every action the GUI performs (edit a cell, drop an index,
   grant a role) is shown as the statement it will run, in the tab's dialect, before it runs.
   Every GUI action can then be pasted into the CLI or given to an agent.
3. **Keyboard first, mouse welcome.** Every command has a shortcut and appears in the command
   palette (`Ctrl/Cmd+K`).
4. **Fast is a feature you can see.** Query time, row count and bytes are always visible, and the
   grid never blocks. Opening a 1 GB file and scrolling 10M rows are measured like the engine (D7).
5. **AI is a colleague in the room, not a wizard in the way.** An agent connected through the
   built-in MCP server appears as a presence chip in the status bar and in an **Activity** panel,
   which shows what it is reading and what it asks to write, with Allow/Deny on destructive
   actions (D11, D45). Agent-run statements open as read-only tabs in the same editor. There is
   no chat box in the main window by default.
6. **Familiar on request, ours by default.** Layout profiles change placement, shortcuts and
   words, never the underlying capability.
7. **Written for people and models alike.** Errors, explain output and metadata use the same
   structured text in the GUI, the CLI and MCP.

## 2. Main window
```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ⌂ EggDB   [▶ Run] [▶ Sel] [■] [Explain]   ⟨ shop.sql · Postgres ▾ ⟩  ⌕ ⌘K  ◉ agent │ toolbar
├───────────────┬──────────────────────────────────────────────┬───────────────┤
│ EXPLORER      │ TABS  shop.sql ×  orders(doc) ×  cache:* ×  + │ INSPECTOR     │
│ ▾ local.eggdb │──────────────────────────────────────────────│ orders        │
│   ▾ Tables    │  1  SELECT o.id, c.name                       │ Table · 2.1M  │
│     orders    │  2  FROM orders o JOIN customers c ...        │ Columns (9)   │
│     customers │  3  WHERE o.total > $1                        │ Indexes (3)   │
│   ▸ Documents │                                               │ Last analyzed │
│   ▸ Keys      │──────────────────────────────────────────────│ ─────────────│
│   ▸ Search    │ RESULTS  ▸ Grid  Text  JSON  Plan   ⤓ Export  │ Activity      │
│   ▸ Series    │ id   │ name      │ total   │ created_at       │ agent "ops"   │
│   ▸ Graphs    │ 1001 │ Ada       │ 120.00  │ 2026-10-26 09:12 │ read schema   │
│   ▸ Vectors   │ 1002 │ Linus     │  80.50  │ …                │ ? DROP idx…   │
│   ▸ Queues    │ ⚠ 2 pending changes   [Apply] [Discard]       │ [Allow][Deny] │
├───────────────┴──────────────────────────────────────────────┴───────────────┤
│ ● local.eggdb (embedded) │ Postgres dialect │ 1,204 rows · 38 ms │ Ln 3 Col 18 │ status
└──────────────────────────────────────────────────────────────────────────────┘
```
gui.md names the regions `toolbar`, `sidebar` (the explorer), `tabs`, `editor`, `results` and
`statusbar`; the inspector is the seventh region, added with the object browser (slice d).

- **Toolbar:** run controls, the current tab's dialect pill (click to switch), the palette and
  the agent chip.
- **Explorer** (`sidebar`, left, 260 px, 180–480, collapsible): connections, then model groups,
  then objects. A filter box at the top filters across all models.
- **Editor tabs** (centre): one tab is one dialect plus one connection.
- **Results** (bottom of the centre, draggable split, 40 % by default): Grid, Text, JSON and
  Plan views, with the pending-changes bar docked here.
- **Inspector** (right, 280 px, collapsible): the selected object's details plus the agent
  Activity log.
- **Status bar:** connection, dialect, last result stats, cursor position and the density toggle.

**Adaptation by width:**
- 1440 px and wider: all three columns.
- Laptop, 1024–1439 px: the inspector collapses to an icon rail and reopens as an overlay
  (`Ctrl/Cmd+I`).
- Browser, or under 1024 px: the explorer becomes a slide-over drawer, and results become a tab
  beside the editor.

Panel sizes persist per layout profile.

## 3. First slices, in order
- **(a) First run and connect** ([first-screen.md](first-screen.md), and gui.md's "The connect
  screen").
  - A single centred card with two choices:
    - "Open a file" creates `~/eggdb/quickstart.eggdb` if none exists, and accepts drag-and-drop.
    - "Connect to a server" takes an `eggdb://host:port/db` URL, with TLS on by default and
      credentials never stored unencrypted.
  - Below the card: "Where are you coming from?" (the layout picker of gui-layouts.md) and "Load
    a sample dataset" (D42).
  - Enter opens the main window with one tab focused.
- **(b) Query editor.**
  - **Dialect pill** on every tab (Postgres, T-SQL, MySQL, Oracle, SQLite, Redis, Mongo, ES DSL,
    …). Switching re-lexes without losing text.
  - **Autocomplete** after 1 character, with schema-aware ranking, plus a docs pane for functions.
  - **Shortcuts:** `Ctrl/Cmd+Enter` runs the selection or the statement under the cursor, and
    `Ctrl/Cmd+Shift+Enter` runs all.
  - **Plan view** (`Ctrl/Cmd+E`): a visual tree with hot nodes tinted and cost, rows and time per
    node. Clicking a node highlights its SQL.
  - **Errors** show as an inline squiggle with a "Fix" action when one exists.
- **(c) Results grid.**
  - **Virtualized** rows and columns, with only visible pages fetched (cursor-backed).
  - **Sort and filter** run client-side up to 100k rows, then are pushed into the query, with a
    banner saying so.
  - **Copy and export.** `Ctrl/Cmd+C` copies TSV, and the Shift variant copies INSERT statements.
    Export to CSV, JSON or Parquet streams.
  - **Editing.** Double-click edits a cell with a typed editor. Edits accumulate in a
    **pending-changes bar** showing the generated statements: `Apply` (`Ctrl/Cmd+S`) runs them in
    one transaction with a row-by-row conflict report, and `Discard` is `Esc`.
- **(d) Object browser.**
  - The explorer tree has per-model icons.
  - Right-click shows contextual commands, each previewing its statement.
  - The inspector shows DDL, stats, dependencies and sample rows.
  - `Ctrl/Cmd+P` searches across models.
- **(e) CLI (`eggdb`)** ([cli.md](cli.md) has the one-shot commands that exist today).
  - Prompt `db=> ` in the active dialect, `\dialect tsql` to switch, and `->` for continuation
    lines.
  - Box tables on a TTY, TSV when piped, `--json` for machines.
  - Structured errors on stderr, for example `error[E2104]: column "nmae" does not exist … hint:
    did you mean "name"? fix: …`, with the same code shown in the GUI.
  - Commands: `\explain`, `\watch`, `\export`, `\agent log`.

## 4. Familiar layouts
At first run, and later in Settings → Layout, the user picks "EggDB" or "Works like …". The
choices are SSMS, pgAdmin, DBeaver, DataGrip, MySQL Workbench, Compass and Redis Insight
(Oracle SQL Developer follows when its layout is recorded), with a trademark note shown once.

A **layout profile** is a tested arrangement, recorded in [gui-layouts.md](gui-layouts.md). It
sets panel placement and default sizes, the shortcut map, terminology ("Object Explorer" vs
"Database Navigator", "Collection" vs "Table"), the toolbar order and which panels start open. It
never changes capabilities or the design system, and users can mix profiles (for example
DBeaver's layout with EggDB's keys).

| Action | EggDB default | SSMS | pgAdmin | DBeaver / DataGrip | Compass |
|---|---|---|---|---|---|
| Run selection/statement | Cmd/Ctrl+Enter | F5 | F5 | Ctrl+Enter | Enter (filter bar) |
| Explain | Cmd/Ctrl+E | Ctrl+L | F7 | Ctrl+Shift+E | "Explain Plan" tab |
| New query tab | Cmd/Ctrl+T | Ctrl+N | Alt+Shift+Q | Ctrl+] / Ctrl+Shift+F10 | — |
| Cancel query | Esc | Alt+Break | Esc (stop) | Ctrl+Shift+X | Stop |
| Object/command search | Cmd/Ctrl+P / K | Ctrl+Alt+T | Ctrl+F | Ctrl+Shift+N / Shift Shift | Ctrl+F |
| Object tree placement | Left, collapsible | Left, results bottom | Left, Query Tool as tab | Left, results bottom, right outline | Left, docs as cards |

gui-layouts.md verified each product's shortcuts against its public documentation; where this
table and gui-layouts.md differ, gui-layouts.md is right.

## 5. Minimal design system (toolkit v1)
gui.md holds the token tables; this is the summary.
- **Type scale:** 11 (`small`), 13 (`body`, `bodyBold`, `code`: body, grid and editor), 15
  (`title`) and 20 px (`headline`). The editor, grid and CLI use the system monospace font. Line
  height is about 1.4 in the UI (13 / 18) and 1.5 in the editor (13 / 20).
- **Spacing:** a 4 px base, with 8, 12, 16 and 24 px padding (`space2` to `space6`). Panels have
  1 px borders, and shadows appear only on overlays.
- **Colour tokens** (semantic): `bg0`, `bg1`, `bg2`, `textPrimary`, `textSecondary`,
  `textDisabled`, `border`, `borderStrong`, `accent` (blue), `onAccent`, `success`, `warning`,
  `error`, `selection`, `focusRing`, `brandYolk` (the egg mark's yolk, `#F5B700`) and
  `syntax{Keyword,String,Number,Comment}`.
  - Three themes come from the same tokens: light, dark and high contrast.
  - High contrast means 7:1 text, a 3 px focus ring and no state shown by colour alone.
- **Density:** comfortable (32 px rows, 13 px text) and compact (24 px rows, 12 px text).
  **Compact is the default**, with the toggle in the status bar; gui.md's rects are compact.
- **Focus and keyboard:**
  - Every focusable element has a visible 2 px `focusRing` (3 px in high contrast).
  - `Tab` cycles regions, arrows move inside grids and trees, `F6` cycles panels, and `Esc`
    always closes or cancels.
  - The grid and tree use a roving tabindex.
- **Accessibility from slice 1.** The accessibility tree is exposed natively on desktop and
  through ARIA in wasm. Every icon has a label, reduced motion is honoured, and RTL mirroring is
  built into the layout engine (D42).

## 6. Motion and feedback
- **Running:**
  - The tab shows a spinner and a live elapsed-time counter.
  - Run becomes **Stop** (`Esc`).
  - Rows stream into the results as they arrive.
  - After 2 s, a progress line shows the rows fetched so far.
- **Cancel:** the UI acknowledges at once ("Stopping…"), and partial results stay, labelled
  "partial (cancelled at 1.2M rows)".
- **Long jobs** (import, backup, index build): a Jobs tray in the status bar. Closing the tab
  does not cancel the job, and a completion toast has an "Open" action.
- **Motion:** 120 ms panel reveals and 80 ms fades, with no bounce. Nothing moves unless the
  user acted. Reduced motion means 0 ms.
- **Errors** contain:
  - a stable code;
  - one plain sentence on what failed;
  - where it failed (line and column, highlighted);
  - why, from the engine;
  - a clickable fix when one is known.

  Errors never present stored data as guidance (D45), and the text is the same in the GUI, the
  CLI and MCP.

## 7. Acceptance criteria for slices (a) and (b)

**(a) First run and connect**
- [ ] A fresh start shows the connect card within 300 ms (release build, measured).
- [ ] "Open a file" creates or opens an embedded `.eggdb` file, and drag-and-drop works on
      desktop and in the browser.
- [ ] A server URL connects over TLS, and a failure shows a structured error (code, reason, fix).
- [ ] The layout picker lists EggDB and every D15 product with the trademark note, and the choice
      persists.
- [ ] The whole flow works keyboard-only, and a screen reader announces each field and error.
- [ ] Behaviour is identical on Windows, macOS, Linux and the browser from one codebase, and
      `eggdb <file>` reaches a prompt in under 1 s.

**(b) Query editor**
- [ ] A dialect pill shows on every tab, and switching keeps the text and re-highlights it.
- [ ] Run selection, run statement, run all and cancel work with the listed shortcuts, in EggDB
      and every shipped profile (automated tests).
- [ ] Autocomplete appears within 50 ms with schema-aware ranking.
- [ ] Explain renders the plan tree with per-node cost, rows and time, and clicking a node
      highlights its text.
- [ ] Errors show code, message, position and fix inline, with the same text from the CLI.
- [ ] Light, dark and high-contrast themes pass contrast checks, and both densities render
      correctly.
- [ ] Keystroke-to-paint takes 16 ms or less on a 10k-line file (benchmarked, baseline recorded).
