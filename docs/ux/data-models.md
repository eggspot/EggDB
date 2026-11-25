# Every data model in the object browser: documents, search, series, graphs, vectors, queues and RESP aggregates

Designed by Claude Fable (D15) for roadmap M6e: every model the engine holds is a node in the
same tree (client-brief.md principle 1) and opens in a tab of its own kind, each with the
editor region as that model's query form and the results region as its result, so the chrome
never changes with the model (D276). Also here: the full-value view of a RESP aggregate (D329).
Every token, component, rect and role not restated is [gui.md](gui.md)'s and
[workspace.md](workspace.md)'s (the keys tab of 10.4 and 10.10 is the pattern every model tab
follows). Rects are at the 1280 × 800 golden frame, which is unchanged.

## 1. Model tabs: the shared shape
A **model tab** is 10.4's keys tab generalized: the `tabs` strip holds it beside query tabs; its
`editor` region holds the model's **query form** (never SQL; its own language or a bar of
controls); `Run` (`Ctrl/Cmd+Enter`, `F5`) runs the form; `Run all` and `Explain` are disabled
unless the section says otherwise; the dialect pill is disabled and reads the model's word
(`Documents`, `Search`, `Series`, `Graph`, `Vectors`, `Queue`, `Keys`); `Ln, Col` shows only for a
form with a text editor; the results header's segments are the model's views (first the one
named below); the summary (2.4) and the status bar's tag use the model's nouns; the grid, the
value view, the menus, copy and export are 3's, 9's and the Export dialog's unchanged. A model
tab keeps no SQL session (nothing to commit), so it closes without asking unless a run is in
flight (9.1's `Stop … and close it?`). `Enter` on the tree row opens or re-activates the tab and
runs its default form; `Ctrl/Cmd+T` from a model tab adds a SQL `Query n` as 10.4 says. The tree
row's inspector header reads the model's words (`Collection · 1,204 documents · 2.3 MB`).

The query form's text editor, where there is one, is gui.md's editor restricted to the form's
language: JSON (keys `textPrimary`, strings `syntaxString`, numbers `syntaxNumber`, `true`,
`false` and `null` `syntaxKeyword`, `$operators` `syntaxKeyword`), Cypher (its keywords
`syntaxKeyword`), or RESP command words. A form whose text does not parse shows 2.5's strip
with `Error: expected '}' at Ln 2, Col 14` and runs nothing.

The tree's groups (4.1), now all drawn when the connection reports the model, with their
children and right text:

| Group | Icon (1.5 px strokes, 12 × 12) | Children | Child's right text | Tab kind |
|-------|------|------|------|------|
| `Documents` | 4.1's document | `database.collection` names | document count | 2 |
| `Search` | a magnifier (4.1's search icon) | index names | document count | 3 |
| `Series` | a polyline (0, 10)–(3, 4)–(6, 7)–(9, 2)–(12, 5) | tables with a time column and a `time_bucket` use, as the engine's catalog lists them | row count | 4 |
| `Graphs` | three circles d 4 at (2, 2) (10, 2) (6, 10) joined by 1 px lines | property graphs (`graph0`) | node count · relationship count (`1,204 · 3,810`) | 5 |
| `Vectors` | three 2 px dots at (2, 9) (6, 4) (10, 7) with a 1 px arrow (1, 11)–(11, 1) | `table.column (dims)` for every vector column, with its index name as the description | row count | 6 |
| `Queues` | rect (0, 3, 12, 6) with lines at x 4 and x 8 | queues, then streams (badge `stream` as right text) | pending count / entry count | 7 |
| `Keys` | key | 4.1, 10.4, 10.10 | | 8 (the value view) |

A column of a table that is a vector column is also listed under the table's `Columns` with
the type `vector(768)`; a search index is also listed under its table's `Indexes` with the
badge `full-text`. The counts come from the catalog and refresh as 4.1 says.

## 2. Documents (`eggdb document get`, `document_get`)
Tab title `orders` with the model word after it in `small` `textSecondary` (`orders · documents`),
the strip's label cut as 9.1 says.

### 2.1 The filter and pipeline bar
The editor region is a text form of 1 to 8 lines (20 px each, growing with the text; the
results take the rest): placeholder `{ "status": "paid" }`, and a JSON array makes it a reading
pipeline (`[ { "$match": … }, { "$group": … } ]`); the first non-blank character decides and the
status bar's tag reads `FIND` or `AGGREGATE`. Right of the text, from the editor's top edge, a
row of controls at y 88: `Find` (`Ctrl/Cmd+Enter`; the primary button, label `Aggregate` for a
pipeline), `Explain` (secondary; runs the engine's `explain` for the filter and shows its plan in
the `Plan` segment with 5's tree, nodes named as the engine's document planner names them:
`COLLSCAN`, `IXSCAN orders_status`), `Reset` (ghost; clears to the default `{}` and runs),
`Limit 100 ▾` (dropdown: 20, 100, 1,000, `No limit`, the last showing 3.7's banner at 100,000),
`Sort ▾` (dropdown that inserts a `{ "$sort": … }` stage or a `sort` option; typing it is the
same). The default form on `Enter` in the tree is `{}` with `Limit 100`, so the first 100
documents show.

### 2.2 Views
Segments `Cards`, `Tree`, `Grid`, then `Messages` and `Plan`; `Cards` first.

- **Cards.** One card per document, `(272, y, 996, h)`, 8 px apart from y 426, `bg0`, 1 px
  `border`, `radiusCard`; header 28 px with `code` `_id: ObjectId("65a1…")` at (12, 4) and, right
  aligned, `small` `textSecondary` `12 fields · 1.2 KB`; body the document pretty-printed with
  2-space indent in `code` 20 px lines from (12, 36), the first 12 lines, then a ghost `Show all
  (24 more lines)` at the body's end that expands the card (and `Show less`); nested objects and
  arrays past depth 2 draw collapsed as `{ 3 fields }` / `[ 12 items ]` in `textSecondary`, and
  a click or `Enter` on that text expands it. The list is virtualized like the grid (card heights
  known from line counts). Hover `bg1` (fading). Keys: the cards list is one `Tab` stop; `Up` /
  `Down` move the current card (ring `focusRing` inset 1 px), `Right` / `Left` expand / collapse
  it, `Enter` opens the value view (9.3) on the whole document, `Ctrl/Cmd+C` copies the document
  as JSON, `PgUp` / `PgDn` a viewport, `Home` / `End` the ends. The right-click menu: `Copy as
  JSON`, `Open value`, `Copy _id`, then `Edit…` and `Delete…` (grid-editing.md section 7).
- **Tree.** One JSON tree for the whole result: rows 24 tall, indent 16, a chevron for objects
  and arrays, the key in `code` `textPrimary`, `: `, the value in its syntax colour, arrays'
  children numbered `[0]`; the first level is one row per document (`[0] { 12 fields }`). Keys
  as the object tree (4.2); `Enter` on a leaf opens the value view; `Ctrl/Cmd+C` copies the
  row's JSON; type-ahead on keys.
- **Grid.** 3's grid with a column per top-level field in the order of first appearance (`_id`
  first), nested values as JSON cells, a missing field as nothing (not `NULL`; the value view says
  `missing`), the summary `1,204 documents · 12 ms · 2.3 MB`.

The status line's cases are 2.4's with `documents` for `rows`; a pipeline's summary is `3
documents · 8 ms (aggregate)`. Errors (an unknown operator, a `$out` stage, a missing
collection) are the engine's words in 2.5's strip and `Messages`. Roles: the form a `textbox`
named `Filter` (`aria-multiline`); the cards a `list` named `Documents` of `listitem`s named by
`_id` with `expanded`; the JSON tree a `tree` named `Documents` whose rows are `treeitem`s named
`key: value`; the grid `Documents`.

## 3. Search indices (`eggdb search get`, `search_get`)
Tab `products_idx · search`. The editor is a text form (3 to 10 lines) holding the `_search`
body, default `{ "query": { "match_all": {} }, "size": 10 }`; controls at the right: `Search`
(`Ctrl/Cmd+Enter`, primary), `Size 10 ▾` (10, 50, 200), `Explain` (adds `"explain": true` and
shows each hit's scoring text in its card), `Reset`. A plain-text line (no `{`) is a shortcut:
the form runs `{ "query": { "multi_match": { "query": "<text>", "fields": ["*"] } } }` and shows
what it ran in the `Messages` segment, so a person can type `red shoes` and see results at once.

Segments `Hits`, `Grid`, `Aggregations`, `Messages`. **Hits**: cards as 2.2's, 8 px apart,
header `_id: 1042` left and `score 4.21` right (`body` `textPrimary`; the max score's card adds
`· max`), body: for each highlighted field one line `name: Red running <shoes> for trail`, the
highlighted words drawn `bodyBold` over a `selection` fill (bold and fill together, never colour
alone), then the `_source` fields not highlighted as `key: value` lines (first 8, then `Show
all`). `Grid`: `_id`, `_score` (right-aligned, 2 decimals), then the `_source` fields as
columns. `Aggregations`: one small grid per aggregation, titled by its name in `bodyBold`,
columns `key`, `doc_count` (and `value` for metrics). Summary `42 hits · 3 ms · max score 4.21`
(`1 hit`, `0 hits`, `10 hits shown of 42` with 3.7's banner reading `Showing the first 10 hits
of 42. Raise Size, or add "from".`); the status bar's tag `SEARCH 10 · 3 ms`. A kNN body
(`"knn": { "field": …, "query_vector": … }`) runs the same way and the score column reads
`similarity`. Roles as section 2 with `Hits` for the list; a highlighted span is exposed with
the `marked` text attribute (UIA `IsActive` annotation, AT-SPI `mark`, ARIA `<mark>`).

## 4. Time series
Tab `metrics · series`. The editor region is a **bar of controls** (no text editor; `Ln, Col`
absent), two rows of 28 at y 88 and y 124 from x 272, 12 px apart:

| Control | Kind | Choices |
|---------|------|---------|
| `Measure` | dropdown | each numeric column with an aggregate: `avg(value)`, `sum(value)`, `min`, `max`, `count(*)`, `last(value)` |
| `Bucket` | dropdown | `10 s`, `1 m`, `5 m`, `15 m`, `1 h`, `1 d`, `auto` (about 200 buckets over the range) |
| `Range` | dropdown | `Last 15 m`, `1 h`, `6 h`, `24 h`, `7 d`, `30 d`, `Custom…` (two inputs `From` / `To` in the engine's timestamp spelling, invalid text as 2.5's strip) |
| `Group by` | dropdown | `none`, then each text column (`host`, `region`); at most 6 groups drawn, the rest folded into `Other` |
| `Where` | input, 320 wide | a SQL condition, placeholder `host = 'web-1'` |
| `Run` | primary | `Ctrl/Cmd+Enter` |
| `Edit as SQL` | ghost | opens the generated statement in a new `Query n` tab |

Under the bar a read-only `code` line (`textSecondary`) shows the statement it runs:
`SELECT time_bucket('1 minute', ts) AS t, host, avg(value) FROM metrics WHERE ts >= now() -
interval '1 hour' AND host = 'web-1' GROUP BY 1, 2 ORDER BY 1` (the dialect's spelling). Segments
`Chart`, `Grid`, `Messages`; `Chart` first:

- **The chart** fills the results body with 24 px padding: a line chart, time on the x axis,
  the measure on the y axis from zero (or the data's minimum when all values are far from zero
  and the axis says so with a broken-axis mark), one 2 px line per group in `chart1` … `chart6`
  (section 9) in the group's first-seen order, never re-coloured when a group disappears;
  markers only when a bucket has fewer than 3 points per 100 px; gridlines 1 px `border`, axes
  1 px `borderStrong`, tick labels `small` `textSecondary` (time as `14:05`, `Nov 3`, `2026` by
  the range); a legend row above the chart (a 12 × 2 swatch, the group's name in `body`) with
  the values at the hovered time; the hovered time draws a 1 px `borderStrong` crosshair and a
  tooltip (polish.md section 5) listing each group's value `web-1 · 42.1`; a drag across the x
  axis zooms the range (the `Range` dropdown then reads `Custom…`), `Esc` or `Reset zoom` returns.
  Direct labels at each line's right end when 4 groups or fewer. Reduced motion changes nothing
  (no animation anyway).
- **Grid**: `t`, then one column per group (`web-1`, `web-2`) or `value`, numbers right-aligned.
  Summary `240 buckets · 12 ms`. Keys: the chart is one `Tab` stop; `Left` / `Right` move the
  hovered time one bucket, `Home` / `End` the ends, `+` / `−` zoom, `Ctrl/Cmd+C` copies the
  grid's TSV. Role: the chart an `img` named by the measure and range (`avg(value) over the last
  hour by host`) with the summary as its description, and the grid is the table view the chart
  always has beside it.

## 5. Property graphs (`eggdb graph get`, `graph run`, `graph_get`, `graph_run`)
Tab `graph0 · graph`. The editor is a Cypher text form (3 to 12 lines), default `MATCH (n)
RETURN n LIMIT 25`; `Run` (`Ctrl/Cmd+Enter`) runs a reading query through `graph_get`; a query
that writes (`CREATE`, `MERGE`, `SET`, `DELETE`, `REMOVE`, `DROP`) runs through `graph_run` after a
confirm dialog (9.1's shape, 420 × 192 with the statement box of 10.2): `Run a Cypher statement
that writes?`, body `It runs in one write transaction; a DELETE cannot be undone.`, `Run`
secondary in `error` text for `DELETE` / `DROP` and primary otherwise, `Cancel` default. A
read-only connection disables writes with 10.2's words. The dialect pill reads `Cypher`;
`Explain` shows the engine's Cypher plan in `Plan`.

Segments `Grid`, `Graph`, `Messages`; `Grid` first (the familiar rows: a node cell reads
`(:Person {name: "Ada"})`, a relationship `-[:KNOWS {since: 2020}]->`, a path its joined
text; the value view shows the node's properties as JSON). **Graph**: a canvas in the results
body: nodes are circles d 24, 1 px `borderStrong` on `bg0`, label `small` `textPrimary` under
the node (the first of `name`, `title`, `id`, else the element id), the primary label's name
(`Person`) as a `bg2` pill above on hover and in the tooltip; relationships 1 px `borderStrong`
lines with a 6 px arrowhead and the type in `small` `textSecondary` at the line's middle; the
layout is force-directed for at most 500 nodes (beyond, `Graph` reads `Too many nodes to draw
(1,204); add LIMIT or use the Grid.` and draws nothing), run to rest off the window's thread,
drawn once at rest (no animation, so reduced motion changes nothing); a drag moves a node (its
position pinned for the tab's life), the wheel zooms 25–400 % about the pointer, a drag on the
background pans, `Fit` (ghost, header right) and `Ctrl/Cmd+0` fit the whole graph; a click
selects a node or relationship (`selection` fill / 2 px `accent` line) and the inspector shows
its labels or type and properties as a two-column list with `Copy as JSON`; a double-click or
`Enter` runs `MATCH (n) WHERE id(n) = <id> MATCH (n)-[r]-(m) RETURN n, r, m LIMIT 50` in the same
tab, adding the neighbours. Keys: the canvas is one `Tab` stop; `Tab` into it lands on the
selected node or the first; arrows move the selection to the nearest node in that direction;
`Shift` + arrows move the selected node 8 px; `Esc` clears the selection. Roles: the canvas a
`group` named `Graph` whose children are the nodes (`button`s named `Person Ada`, described by
their properties) and relationships (`button`s named `Ada KNOWS Linus`), so the structure is
walkable without sight. Summary `25 nodes · 40 relationships · 4 ms`; the status bar's tag
`MATCH 25 · 4 ms`, `CREATE 2 · 1 ms` after a write.

## 6. Vectors (kNN)
Tab `embeddings.vec · vectors`. The editor region is a bar: `Query vector` input (480 wide,
`code`, placeholder `[0.12, -0.33, …] or a row id`: a JSON array of the column's dimension, or
`id = 42` meaning that row's vector), `k 10 ▾` (5, 10, 50, 100), `Metric cosine ▾` (`cosine`,
`euclidean`, `inner product`, the operators `<=>`, `<->`, `<#>` in the generated SQL), `Where`
input (a SQL condition on the table), `Run`, `Edit as SQL`. The generated statement line under
the bar: `SELECT id, name, vec <=> $1 AS distance FROM embeddings WHERE … ORDER BY distance LIMIT
10`, the vector bound as a parameter. Segments `Grid`, `Messages`: the grid shows the table's
non-vector columns, then `distance` (right-aligned, 4 decimals) with a 4 px `bg2` bar in the
cell's bottom 4 px filled `accent` by `1 − distance / max distance` (the plan view's share bar, so
the nearest reads longest; the number is always printed). `Enter` on a row's `distance` cell
opens the value view; the row's menu adds `Search from this row` (puts `id = <id>` in the input
and runs). A wrong dimension fails with `Error: the query vector has 3 values; vec has 768` on
the input (`error` border and strip). Summary `10 rows · 2 ms · cosine`.

## 7. Queues and streams
Tab `orders_q · queue` (or `events · stream`). The editor region is a bar: `Show ▾` (`Pending`,
`In flight`, `Dead`, `All`) for a queue, `From ▾` (`Newest 100`, `Oldest 100`, `After id…`) for a
stream; `Consumer group ▾` (`none`, then each group, showing its pending count); `Follow`
(toggle button, `pressed` while on: re-runs every 1 s, newest rows at the bottom and the grid
scrolled there unless the user scrolled up, the summary adding `· following`; reduced motion
changes nothing; `Esc` in the grid turns it off); `Run`; `Peek 100 ▾` (20, 100, 1,000). Grid
columns for a queue: `id`, `enqueued` (timestamp), `visible at`, `attempts` (right), `payload`
(JSON or text, cut, the value view shows it whole); for a stream: `id` (`1700000000000-0`),
`time` (from the id), then one column per field seen (`fields` as a JSON cell when more than
8). Row actions in the menu and the header's right (ghost buttons): `Ack` (`Ctrl/Cmd+Enter` on a
row), `Requeue`, `Move to dead letter`, `Delete…` (confirm), `Purge…` (confirm 9.1 with the count:
`Purge 1,204 pending messages from orders_q?`, `Purge` secondary in `error` text), each shown as
the statement or command it runs in the status bar's left group for 2 s (`SELECT
eggdb_queue.ack('orders_q', 1042)`; `XDEL events 1700000000000-0`). A read-only connection
disables them. Summary `100 messages shown of 1,204 pending · 3 ms` with 3.7's banner, or
`100 entries · 2 ms`; the status bar's tag `PEEK 100 · 3 ms`.

## 8. The full-value view of a RESP aggregate (D329)
On a RESP keys tab (10.10), `Enter`, `F2` or a double-click on the `value` cell of a `hash`,
`list`, `set`, `zset` or `stream` opens 9.3's view with these changes:

| Item | Drawing |
|------|---------|
| header | `value · hash · 12 fields` (`list · 3 items`, `set · 8 members`, `zset · 8 members`, `stream · 40 entries`), then a segmented control `JSON` / `Table` at `(300, 2, 104, 28)` (two 52-wide segments, 1.4's style), `Copy` as before |
| body while fetching | `Loading…` in `body` `textSecondary` at (12, 40), the summary unchanged; the fetch is `HGETALL`, `LRANGE 0 9999`, `SMEMBERS` (or `SSCAN` when `SCARD` is over 10,000), `ZRANGE 0 9999 WITHSCORES`, `XRANGE - + COUNT 10000`; `Esc` cancels and closes |
| `JSON` | 9.3's pretty-printed body: an object for a hash, arrays for a list and a set, an array of `[member, score]` for a zset, an array of `{"id": "…", "fields": {…}}` for a stream, cut at 1 MB as 9.3 says; past 10,000 elements the last line reads `… showing the first 10,000 of 40,128` in `textSecondary` |
| `Table` | a small grid (rows 20, `small`) in the body: hash `field`, `value`; list `#`, `value`; set `member`; zset `member`, `score` (right); stream `id`, then a column per field; the same cut and last line |
| `Copy` | copies the JSON form whole (what `JSON` shows), toasting `Copied 12 fields as JSON` |

Keys: `Tab` cycles segments → body → `Copy`; `Left` / `Right` on the segments switch (`Alt+J`
and `Alt+T` from anywhere in the view); the body's keys and selection are 9.3's and polish.md
2.1's; the table's keys are the grid's without editing. A key that is gone when the view opens
shows `(gone)` as the body and a disabled `Copy`. Role: the segmented control a `tablist` named
`Value view` with two `tab`s; the table a `table` named `Value`; the rest as 9.3. The RESP tab's
`type` cell for a string says `string` and its view stays 10.10's `GET`.

## 9. Chart tokens
Charts (here, in monitoring and in notebooks) use six categorical colours in fixed order,
assigned to series in first-seen order and never re-assigned while a tab lives; the seventh
series and beyond fold into `Other` in `textDisabled`. Validated for adjacent-pair
colour-vision separation and a normal-vision floor on both surfaces; three light values sit
under 3:1 against `bg0`, which is why every chart has a legend, direct labels and a `Grid`
segment beside it (text never takes a series colour: labels and values stay `textPrimary` /
`textSecondary`).

| Token | Light | Dark | High contrast |
|-------|-------|------|------|
| `chart1` | `#2A78D6` | `#3987E5` | `#8AB4F8` |
| `chart2` | `#EB6834` | `#D95926` | `#FFB38A` |
| `chart3` | `#1BAF7A` | `#199E70` | `#7CE38B` |
| `chart4` | `#EDA100` | `#C98500` | `#FFD75F` |
| `chart5` | `#E87BA4` | `#D55181` | `#FFA6C9` |
| `chart6` | `#008300` | `#008300` | `#A5D6A7` |

A sequential scale (a heat cell in monitoring) is `chart1` stepped by lightness from `bg1` to
`chart1`; a diverging one is `chart1` ↔ `chart2` through `bg2`. `success`, `warning` and `error`
never colour a series. In high contrast the lines are 3 px and every series also gets a marker
shape (circle, square, triangle, diamond, cross, plus) in fixed order.

## 10. Accessibility summary
| Element | Role | Name | Value / states |
|---------|------|------|------|
| model tab | `tab` | `orders, documents` (the title and the model word) | selected; `running` |
| text form | `textbox` `aria-multiline` | `Filter`, `Search body`, `Cypher` | the text; invalid with the strip as its error message |
| bar controls | `combobox`, `textbox`, `button`, toggle `button` (`Follow`, `pressed`) | their labels | |
| generated statement line | read-only `textbox` | `Statement` | the SQL |
| cards list | `list` of `listitem` | `Documents`, `Hits` | `expanded`; position and set size; the current card focused |
| JSON tree | `tree` of `treeitem` | `Documents` | level, expanded |
| chart | `img` | the measure and range | the summary as the description |
| graph canvas | `group` of `button`s | `Graph` | each node and relationship named and described |
| value view segments | `tablist` of two `tab`s | `Value view` | selected |

Announcements are 7's: the status line after every run (`1,204 documents, 12 milliseconds`;
`42 hits, 3 milliseconds, max score 4.21`; `25 nodes, 40 relationships`), the strip as an
`alert`, the banners politely; `Follow` reads `Following; 3 new messages` politely at most every
5 s.

## 11. Acceptance checklist
- [ ] png: a `shop` with a collection, an index, a series table, a graph, a vector column and a
      queue draws all seven groups in section 1's order with icons and counts; the rail (10.6)
      gains an item for each group that has its own icon.
- [ ] png: `Enter` on `orders` opens `orders · documents` with `{}` in the form, `Find` primary,
      `Limit 100`, three cards from a stub of three documents with `_id` headers and pretty JSON;
      `Right` on the first card expands a collapsed `{ 3 fields }`; `Tree` shows `[0] { 12
      fields }` rows; `Grid` shows `_id` first; a pipeline `[{"$group": …}]` relabels the button
      `Aggregate` and the tag `AGGREGATE`.
- [ ] png: the search tab with `red shoes` typed as plain text runs the `multi_match` form,
      shows two hit cards with `score` and bold highlighted words over `selection`, and
      `Messages` names the body it ran; `Size 10` with 42 hits shows the banner.
- [ ] png: the series tab's bar at y 88 and 124, the statement line, a chart of two groups in
      `chart1` and `chart2` with a legend and end labels, and the crosshair tooltip at a hovered
      bucket; `Grid` shows `t`, `web-1`, `web-2`; high contrast draws 3 px lines with markers.
- [ ] png: the graph tab with 5 stub nodes and 4 relationships draws circles, arrows and labels
      at rest; a `DELETE` query shows the confirm dialog with the statement; tree: the canvas's
      children are 9 buttons with names.
- [ ] png: the vectors tab with `id = 42`, `k 10`, `cosine` draws the statement line and a grid
      whose `distance` cells carry the share bar; a 3-value vector on a 768 column shows the
      dimension error.
- [ ] png: the queue tab shows `Pending`, `Follow` pressed with `· following` in the summary and a
      new row appended at the bottom after a stub tick; `Purge…` shows the dialog with the count.
- [ ] png: the RESP value view on a `hash` cell shows `value · hash · 12 fields`, the `JSON` /
      `Table` control, `Loading…` at 100 ms with a slow stub, then the JSON object; `Alt+T` shows
      the two-column table; a stub of 40,128 members ends with the `… showing the first 10,000`
      line; tree: a `tablist` with two `tab`s.
- [ ] contrast: `chart1` … `chart6` on `bg0` and `bg1` in all three themes pass the categorical
      checks recorded in section 9 (a test recomputes the adjacent-pair separations).

## Decided by this design
- Every model opens as a model tab shaped like the keys tab: the editor region is the model's
  own form, the results its views, the dialect pill the model's word; no model becomes a dialect
  of the SQL editor.
- Documents offer `Cards`, `Tree` and `Grid`; the filter and the pipeline share one form told
  apart by the first character; `Limit 100` is the default.
- Plain text in the search form is a `multi_match` shortcut, shown in `Messages`.
- Series and vectors are bars of controls that always show the SQL they run, with `Edit as SQL`;
  the chart is a line chart with a legend, end labels and a `Grid` beside it, never alone.
- The graph canvas draws at most 500 nodes at rest with no animation; writes go through
  `graph_run` after a confirm dialog that shows the statement.
- Queues follow with a 1 s re-run under a `Follow` toggle; every action is shown as the
  statement or command it runs.
- A RESP aggregate's value view fetches on open, offers `JSON` and `Table`, and cuts at 10,000
  elements or 1 MB with a line that says so.
- Six chart tokens in fixed order, validated on both surfaces, with high-contrast values; the
  seventh series folds into `Other`.
