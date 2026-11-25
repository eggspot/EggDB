# SQL notebooks with charts, and the visual query builder

Designed by Claude Fable (D15) for roadmap M6e: SQL notebooks with charts and a visual query
builder (D42). Both are page tabs ([admin-screens.md](admin-screens.md) section 1) that keep the
query as the unit of work (client-brief.md principle 2: every builder action is the statement it
makes). Every token, rect and role not restated is [gui.md](gui.md)'s and
[workspace.md](workspace.md)'s; charts use [data-models.md](data-models.md) section 9's tokens and
section 4's chart rules. Rects are at 1280 × 800.

## 1. Notebooks

### 1.1 The file
A notebook is a Markdown file (`.md`, suggested in `~/eggdb/notebooks/`), readable anywhere:
prose is Markdown; a query cell is a fenced block ```` ```sql ```` (with the dialect after a
space when not the notebook's default: ```` ```sql mysql ````); a chart is a fenced ```` ```chart ````
block of `key = value` lines (`type = line`, `x = t`, `y = avg`, `series = host`, `title = …`)
that applies to the query cell above it; a parameter cell is ```` ```params ```` with `name =
value` lines. Results are never stored in the file (a notebook is text; re-run to see them);
`Export › HTML` writes a static page with the results and charts inlined (our own HTML, D16's
exception), and `Export › Markdown with results` writes a copy with each result as a Markdown
table of at most 100 rows. The front matter (`---` lines) holds `connection = shop.eggdb`,
`dialect = postgresql`, `title = …`; a notebook opened without a matching connection asks to
pick one in a dialog listing the recent connections. `File > New notebook` (`Ctrl/Cmd+Shift+T`),
`File > Open notebook…`, a dropped `.md` file whose first fence is ```` ```sql ````, or DBeaver's
`Projects › Scripts` open one in a page tab `Notebook · sales` (`Notebook 1` until saved;
`Ctrl/Cmd+S` saves, the unsaved dot as a query tab's).

### 1.2 Layout
| Region | Rect | Contents |
|--------|------|----------|
| header | (260, 80, 1020, 44) | the title (an input when clicked; the front matter's `title`); right: `Run all` (primary, `Ctrl/Cmd+Shift+Enter`), `Stop`, `Add ▾` (`Query cell` `Ctrl/Cmd+Shift+Q`, `Text cell` `Ctrl/Cmd+Shift+M`, `Chart`, `Parameters`), `Clear results`, `Export ▾` (`HTML`, `Markdown with results`, `PDF` through the OS's print where it exists) |
| cells | (260, 124, 1020, 652), scrolling as one (9.6's scrollbar), 24 px padding, cells 16 px apart, each 972 wide from x 284 | section 1.3 |
| footer | the status bar | `Cell 3 of 8 · 12 rows · 4 ms`, `Running cell 3…` |

### 1.3 Cells
A cell is a card: `bg0`, 1 px `border` (the current cell `borderControl`; the running cell's left
edge a 2 px `accent` stripe; a cell with an error a 2 px `error` stripe), `radiusCard`. A gutter
32 px wide at the card's left holds the cell's number (`[3]` `small` `textSecondary`; `[*]` while
running) and, on hover or focus, ghost icon buttons 24 × 24 stacked: `Run` (`Ctrl/Cmd+Enter`),
`Move up` / `Move down` (`Alt+Up` / `Alt+Down`), `Delete` (`Ctrl/Cmd+Shift+Backspace`, asks when the
cell has text: 9.1's `Delete cell 3?`), `Add below` (`Ctrl/Cmd+Shift+Q`).

- **Query cell**: gui.md's editor without the 48 px gutter (line numbers in `small` at the
  card's left inside the gutter's 32 px), growing with its lines from 3 to 20 (then scrolling
  inside), completion and errors as the query tab's (editor-and-plan.md); a dialect pill at the
  card's top-right (`small`, the notebook's default unless the fence names one). Under the
  editor, once run, the **result**: the results header (32 px: `Results` / `Messages` / `Plan`
  segments, the summary, `Export…`, and `Chart` as a toggle that adds a chart block) and the grid
  at most 10 rows tall (240 px; a `Show all` ghost in the header expands to 30 rows; the grid
  scrolls beyond), 3's grid in full (keys, copy, the value view, column widths kept per cell).
  A cell's `Run` runs its statement(s) on the notebook's one kept session (9.2: a `BEGIN` in cell
  2 holds for cell 3); `Run all` runs the cells top to bottom, stopping at the first error with
  that cell scrolled into view. Parameters from a `params` cell are bound as `:name` in every
  query (`WHERE region = :region`); the cell shows them as labelled inputs, and changing one
  marks the cells below stale (their result header reads `stale · Run` in `textSecondary`).
- **Text cell**: Markdown rendered when not focused (headings in `title` / `bodyBold`,
  paragraphs `body` wrapped, lists with `•` and numbers, bold in `bodyBold`, inline code and
  fences in `code` on `bg1`, links in `accent` opened with `Ctrl/Cmd` + click, tables as a small
  grid, images not loaded: the alt text in `textSecondary`); a click, `Enter` or `F2` edits it as
  plain text in gui.md's editor (no syntax), `Esc` or a click outside renders again. The renderer
  is ours (CommonMark's core: ATX headings, emphasis, lists, code, links, tables; raw HTML shown
  as text).
- **Chart block**: under a query cell's result (the `Chart` toggle, or the file's block): a
  controls row (`Type ▾` `line`, `bar`, `stacked bar`, `area`, `scatter`, `stat` (one number);
  `X ▾` and `Y ▾` column dropdowns (`Y` multi-select up to 6), `Series by ▾` (a column, folding
  past 6 values into `Other`), `Title` input) and the chart `(x, y, 940, 280)`: data-models.md 4's
  rules (one axis, a legend for 2 or more series, direct end labels up to 4, thin marks: bars 2
  px apart with 4 px rounded tops on the baseline, 2 px lines, markers 8 px when sparse, gridlines
  `border`, axes `borderStrong`, labels `textSecondary`, series `chart1`…`chart6` in first-seen
  order, a crosshair tooltip on hover with every series' value, `Grid` toggle to the table); a
  `stat` is the first row's first `Y` value in `headline` with the title in `small` above and,
  with two rows, the change `+12 %` after it with an arrow drawn from lines (up or down) and the
  word `up` / `down` in the tooltip. The chart's choices write back to the ```` ```chart ```` block
  on save. A chart with no numeric `Y` reads `Choose a numeric column for Y` in its place.
- **Parameters cell**: inputs from its `name = value` lines, one per row (`Region` label and a
  text input; a value starting with `[` makes a dropdown of its items; a `date` suffix an input
  with the engine's date form), `Apply` (`Ctrl/Cmd+Enter` in the cell) re-runs the cells below.

### 1.4 Keys
| Key | Does |
|-----|------|
| `Ctrl/Cmd+Enter` | run the current cell; `Shift+Enter` run it and move to the next (adding a query cell at the end) |
| `Ctrl/Cmd+Shift+Enter` | run all |
| `Esc` in a cell's editor | leave edit mode: the cell is current, the card focused (then `Up` / `Down` move between cells, `Enter` edits, `Alt+Up` / `Alt+Down` move the cell, `Ctrl/Cmd+Shift+Backspace` deletes, `Ctrl/Cmd+Shift+Q` / `M` add a query / text cell below) |
| `Ctrl/Cmd+Shift+C` | add or focus the cell's chart |
| `Ctrl/Cmd+3` | focus the current cell's grid |
| `Ctrl/Cmd+S` | save; `Ctrl/Cmd+Shift+S` save as |
| `Ctrl/Cmd+W` | close (asks `Close Notebook sales without saving?` when unsaved; a running cell asks 9.1's stop dialog) |

### 1.5 Roles
The cells region a `list` named `Cells` of `listitem`s named `Cell 3, query` (`text`, `chart`,
`parameters`) with `current`, each holding the editor's `textbox` (named `Cell 3`), the result's
`grid`, the chart's `img` (named by the title, the summary as its description) and the gutter's
`button`s; `[*]` reads `running` in the cell's name; the result's summary a polite `status`. A
text cell's rendered form is a `document` region with headings at their levels and links as
`link`s; while editing, a `textbox`. The parameters cell's inputs by their names. `Run all`'s end
announces `Ran 8 cells, 1 error in cell 5`.

## 2. The visual query builder

### 2.1 Opening
`Build query…` in a table's row menu, `File > Query builder` (`Ctrl/Cmd+Shift+B`), or `Open in
builder` in a query tab's editor menu (10.9; enabled when the statement is a `SELECT` the
builder can represent: joins, filters, groups, sorts, a limit; else disabled with the tooltip
`This statement uses features the builder does not represent`). A page tab `Builder 1` (`Builder
2`…), the page's pill reading `Builder`. The builder is one-way: it writes SQL; `Edit as SQL`
(or `Run`) opens a query tab with the text, and edits there do not flow back (the SQL is the
truth; the builder is a way to write it).

### 2.2 Layout
| Region | Rect | Contents |
|--------|------|----------|
| header | (260, 80, 1020, 44) | the dialect pill (enabled: the builder writes that dialect), `Run` (primary, `Ctrl/Cmd+Enter`: runs in a new query tab and activates it; a second `Run` reuses that tab), `Edit as SQL` (secondary), `Add table…` (secondary, `Ctrl/Cmd+Shift+A`), `Distinct` (toggle), `Limit 100 ▾` |
| tables canvas | (260, 124, 1020, 300), `bg0`, scrollable | er-view.md 3's cards (header, columns with checkboxes at the left of each column name, 16 × 16, `pk` and link icons), joins as er-view.md 4's lines labelled with the join kind at the midpoint in a `bg2` pill (`INNER`, `LEFT`, `RIGHT`, `FULL`; click or `Enter` cycles; the tooltip the `ON` condition) |
| canvas/grid splitter | (260, 424, 1020, 6) | polish.md 6.2's, key `builder_ratio` |
| columns grid | (260, 430, 1020, 258) | section 2.3 |
| SQL box | (260, 688, 1020, 88), `bg1`, 1 px `border` above | the generated statement in `code` (read-only, the DDL box's style with `Copy`), 4 lines then scrolling; updated on every change; a statement the dialect refuses (the engine's parse check runs after 150 ms of quiet) draws 2.5's error line under it |

### 2.3 The columns grid
3's grid with editable cells (grid-editing.md 2's editors), one row per chosen column, in the
`SELECT` list's order:

| Column | Cell | Writes |
|--------|------|--------|
| `#` | the gutter; drag the gutter to reorder (`Alt+Up` / `Alt+Down`) | the select list's order |
| `Column` | `orders.total` (read-only; `*` for a whole table's checkbox) | |
| `Output` | checkbox (default on); off keeps the row for filtering or sorting only | the select list |
| `Alias` | text | `AS revenue` |
| `Aggregate` | dropdown `none`, `count`, `sum`, `avg`, `min`, `max`, `count distinct`, `string_agg` (the dialect's names: `GROUP_CONCAT`, `LISTAGG`) | `sum(orders.total)`; any aggregate puts every non-aggregated output column in `GROUP BY` |
| `Filter` | text: an operator and a value (`> 100`, `= 'paid'`, `in ('a', 'b')`, `like 'A%'`, `is null`, `between 1 and 9`; a bare value means `=`) | `WHERE` (or `HAVING` for an aggregated row), joined by `AND`; `Or…` in the cell's menu adds a second filter column `Or` for an `OR` group |
| `Sort` | dropdown `none`, `asc`, `desc`, with the sort order number after it (`asc 1`) | `ORDER BY` |
| `Group` | checkbox, set automatically with an aggregate; explicit on for grouping without an aggregate | `GROUP BY` |

`Add table…` opens a popover (the completion list's shape, 360 wide) of the catalog's tables
with type-ahead; a table added joins the canvas and, when a foreign key links it to a table
already there, the join line and its `ON` are made from the key (`INNER` by default; the
status bar says `Joined orders to customers on customer_id`); without a key the line is drawn
dashed with `ON ?` as its pill, the SQL box shows `ON <set the join condition>` in
`textSecondary` in the condition's place (never a comment), and `Run` is disabled
until the condition is set by dragging a column onto another card's column (the line snaps to
them) or by `Edit join…` in the line's menu (a dialog with two column dropdowns and the kind).
A whole-table checkbox in a card header selects every column. Removing a table (`Delete` on
the card, the card's menu) removes its rows and joins. A subquery, a `UNION`, window functions
and expressions are not built here; the `Alias` cell accepts an expression when it starts with
`=` (`= total * 1.2`), written verbatim as a select item, so the grid is not a wall.

### 2.4 Keys
The canvas has er-view.md 6's keys (arrows between cards, `Shift` + arrows move, `Space` on a
column row toggles its checkbox, `Enter` on a join line cycles its kind, `Delete` removes the
current card or join); the grid has grid-editing.md 8's editing keys; `F6` cycles header → canvas
→ grid → SQL box; `Ctrl/Cmd+Enter` runs from anywhere in the page; `Ctrl/Cmd+C` in the SQL box
copies the statement.

### 2.5 Roles
The canvas a `group` named `Tables` whose cards are er-view.md 8's `button`s with each column a
`checkbox` (`orders.total, checked`); join lines `button`s named `orders inner join customers
on orders.customer_id = customers.id` with `Enter` cycling (announced `left join`); the grid a
`grid` named `Columns` with its editable cells; the SQL box a read-only `textbox` named
`Statement`, a polite `status` reading `Statement updated` at most every 2 s while editing.

## 3. Acceptance checklist
- [ ] file: a notebook with a `params` cell, two `sql` cells (one with `mysql` after the fence),
      a `chart` block and a text cell round-trips through save unchanged except the front matter's
      `title`; a `chart` block's `type = bar` is read into the chart's `Type`.
- [ ] png: `Notebook · sales` with three cells: cell 1 a rendered text cell with a heading and a
      list, cell 2 a query cell after `Ctrl+Enter` with a 3-row grid and the summary, cell 3 the
      same cell's chart (`line`, `x = t`, `y = total`, two series with a legend and end labels);
      the running frame shows `[*]` and the `accent` stripe; a stub error shows the `error` stripe
      and the strip inside the card.
- [ ] png: changing `region` in the parameters cell marks cells 2 and 3 `stale · Run`; `Apply`
      re-runs them; `Run all` with an error in cell 2 stops there with the status text.
- [ ] export stub: `HTML` writes one file with the grid as a `<table>` and the chart as inline
      SVG paths from our renderer; `Markdown with results` appends a 3-row table under cell 2.
- [ ] png: `Builder 1` with `orders` and `customers` added: the join line `INNER` pill on the
      foreign key, four checked columns, `sum` on `total` with `Group` set on `customers.name`,
      a filter `> 100`, `desc 1` on the sum, and the SQL box reading the statement with `GROUP
      BY`, `HAVING`, `ORDER BY`, `LIMIT 100`; `Run` opens `Query 2` with it run.
- [ ] png: adding `products` with no key draws the dashed line with `ON <set the join
      condition>` in the box and `Run` disabled; dragging `products.id` onto `orders.product_id`
      sets it; `Enter` on the line cycles to `LEFT`.
- [ ] tree: cells are `listitem`s with `current`; the chart an `img` with its description; the
      builder's column rows carry `checkbox`es and the join line's name states the condition;
      the SQL box is a read-only `textbox`.
- [ ] builder from SQL: `Open in builder` on `SELECT c.name, sum(o.total) FROM orders o JOIN
      customers c ON o.customer_id = c.id GROUP BY c.name` rebuilds the two cards, the join and
      the grid rows; on a statement with a window function the item is disabled.

## Decided by this design
- A notebook is a Markdown file with `sql`, `chart` and `params` fences; results are never
  stored; exports inline them. One kept session per notebook, cells run in order, parameters as
  `:name` with stale marking below.
- Cells are cards with a 32 px gutter of actions; a query cell's result is the full grid capped
  at 10 rows; the chart block is data-models.md's chart with a `Grid` toggle and writes its
  choices back to the file.
- Our own CommonMark-core renderer draws text cells and the assistant's answers; raw HTML is text.
- The builder is one-way and writes the dialect's SQL live into a read-only box; tables are ER
  cards with checkboxes, joins are lines whose kind cycles, the columns grid holds alias,
  aggregate, filter, sort and group; an unjoined table disables `Run` until its condition is set;
  `=` in `Alias` admits an expression.
