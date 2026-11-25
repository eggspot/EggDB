# The interactive ER view

Designed by Claude Fable (D15) for roadmap M6e: the GUI's interactive entity-relationship view,
with diagrams across schemas, views, indexes and check constraints (D237: the same catalog
query `eggdb_console.er_diagram` renders as Mermaid or DOT; the GUI draws it itself). A page
tab ([admin-screens.md](admin-screens.md) section 1). Every token, rect and role not restated
is [gui.md](gui.md)'s and [workspace.md](workspace.md)'s. Rects are at 1280 × 800.

## 1. Opening
`Diagram…` in the tree's connection, schema or `Tables` row menu, `Ctrl/Cmd+Shift+D`, `File >
Diagram…`, or `Diagrams` in DBeaver's Projects panel. The page tab is titled `Diagram · shop`
(`· public` for one schema), one per connection; a second open activates it and re-reads the
catalog. The page's dialect pill reads `Diagram`.

## 2. Layout
| Region | Rect | Contents |
|--------|------|----------|
| header | (260, 80, 1020, 44) | title; segments `Tables` / `Tables and views`; right: `Schemas ▾` (a checklist of schemas, all checked at first), `Show ▾` (check items `Columns`, `Types`, `Indexes`, `Checks`, `Views`, `Column names only` ), `Auto layout` (ghost), `Fit` (ghost, `Ctrl/Cmd+0`), zoom `100 % ▾` (`25`, `50`, `75`, `100`, `150`, `200`, `Fit`), `Export ▾` (`PNG`, `Mermaid`, `DOT`, `SQL (DDL)`) |
| canvas | (260, 124, 1020, 652), `bg0` | the diagram, scrollable in both axes (9.6's scrollbars) and zoomable |
| minimap | (1116, 616, 152, 148) over the canvas's bottom-right, `bg1` at 90 % alpha, 1 px `border`, `radiusControl` | the whole diagram as 1 px `borderStrong` rects with the viewport as a 1 px `accent` rect; a drag moves the viewport; hidden under 1024 px and by `View > Minimap` |

Zoom scales the canvas's drawing (text sizes with it, clamped: under 50 % cards draw only
their names in `small`, under 35 % only their outline); `Ctrl/Cmd` + wheel zooms 10 % per notch
about the pointer, `Ctrl/Cmd+=` / `−` the same about the viewport's centre, a pinch on touch;
the wheel scrolls, `Shift` + wheel horizontally; a drag on empty canvas pans; `Space` + drag pans
too. Positions and zoom are kept per connection and schema set in `gui-settings`
(`diagram.<connection>.<table> = x,y`, `diagram.<connection>.zoom`), so a hand-laid diagram
returns; `Auto layout` re-lays everything and forgets them after 9.1's `Replace your layout?`
(`Auto layout` / `Keep`, default `Keep`) when any position was hand-set.

## 3. Cards
A table is a card: `bg0`, 1 px `borderControl`, `radiusCard`, width `max(180, widest line + 24)`
clamped at 320, 4 px shadow only while dragged.

| Row | Height | Drawing |
|-----|--------|---------|
| header | 28, `bg1`, 1 px `border` below | the table icon at (8, 8) `textSecondary`; `bodyBold` `textPrimary` the name at (24, 5) (`schema.name` when more than one schema shows, the schema in `textSecondary`); right text `small` `textSecondary` the row count (`2.1M`) when analyzed |
| column | 20 each, `code` | the key icon (`pk`), a 12 × 12 link icon (two rings) for a foreign key column, or nothing, at (8, 4); the name at (24, 0) `textPrimary`; the type right-aligned at `w − 8` in `textSecondary` (hidden with `Column names only`); `not null` columns draw the name in the mono bold face |
| `Indexes (3)` | 20, `bg1`, collapsible group row with a chevron | then one 20 px row per index: the name, `(columns)`, `unique` as a `small` badge; shown with `Show › Indexes` |
| `Checks (1)` | 20, as above | one row per check constraint: the name, then the expression in `textSecondary` cut with `…` (the tooltip shows it whole); shown with `Show › Checks` |

A view is a card with a 1 px dashed border (dashes 4 on 4), the folder-with-eye icon (the folder
plus a 2 px dot) and the badge `view` in its header; its columns list as a table's; its
relationships (section 4) come from the view's definition's base tables, drawn dashed. A
foreign table or a collection shows as a card with its model's icon and no relationship lines.
Hover `bg1` on the header (fading); a selected card (click, `Enter` from the keyboard) draws a
2 px `accent` border and the inspector (4.3) shows the table; a double-click or `Ctrl/Cmd+Enter`
opens its `LIMIT 100` tab. A card's column under the pointer draws `bg2` and its relationships
highlight (section 4). Cards never overlap after auto layout; a dragged card may.

## 4. Relationships
A foreign key is a 1 px `borderStrong` orthogonal line from the referencing column's row (its
card's nearest side edge, at the column's vertical centre) to the referenced table's key row,
routed horizontally, then vertically, then horizontally, with 12 px stubs at both ends and lines
rounded by 4 px at the turns; crossings are not avoided but lines never pass through cards after
auto layout. Ends: the **one** side (the referenced key) a 1 px bar 8 px tall across the line at
the stub; the **many** side (the referencing column) a crow's foot of three 1 px lines spreading
to 8 px at the card's edge; a nullable foreign key adds a circle d 6 before its crow's foot
(zero or one); a unique foreign key (one-to-one) draws a bar instead of the foot. The constraint's
name draws at the line's midpoint in `small` `textSecondary` only with `Show › Names` (off by
default); the tooltip over a line reads `orders.customer_id → customers.id · fk_orders_customer ·
ON DELETE CASCADE`. Hovering or selecting a card draws its lines 2 px `accent` and the linked
cards' borders `accent` 1 px; a click on a line selects it (2 px `accent`) and the inspector shows
the constraint (its DDL, the two columns, the actions). A self-reference loops out of the card's
right edge and back in, 24 px out. Relationships across schemas draw the same; a schema's cards
sit inside a group: a 1 px dashed `border` rounded rect (`radiusCard`) 16 px around its cards
with the schema's name in `small` `textSecondary` at its top-left, drawn only when more than one
schema shows.

## 5. Auto layout
Layered left to right by the foreign-key direction (referenced tables to the left of referencing
ones): tables with no outgoing keys form layer 0; each other table's layer is one more than the
highest layer it references (cycles broken at the edge whose source has the most columns);
inside a layer cards are ordered to shorten crossings by the barycentre of their neighbours
(two passes down and up), then stacked top to bottom with 24 px gaps; layers are 80 px apart
(measured from the widest card of the left layer); unconnected tables go to a last layer, sorted
by name. Views sit in the layer after their deepest base table. The result is centred in the
canvas and `Fit` zooms to it. Layout runs off the window's thread for over 200 tables with the
sweeping bar in the header and `Laying out 412 tables…`; over 2,000 tables the page asks for a
schema or a subset first (`Choose schemas to draw`, the `Schemas ▾` checklist opened as a
dialog). No animation: cards appear at their places (reduced motion changes nothing).

## 6. Keyboard
The canvas is one `Tab` stop with a current card (the first by name at open):

| Key | Does |
|-----|------|
| `Tab` / `Shift+Tab` | leaves the canvas (to the minimap, then the status bar) |
| arrows | move the current card to the nearest card in that direction (by centre), scrolling it into view |
| `Ctrl/Cmd` + arrows | follow a relationship: `Right` to the first referenced table, `Left` to the first referencing one, `Up` / `Down` between several (the status bar names the constraint) |
| `Shift` + arrows | move the current card 8 px (`Ctrl/Cmd+Shift` 32) |
| `Enter` | select (the inspector), `Ctrl/Cmd+Enter` open the `LIMIT 100` tab, `Shift+Enter` insert the name at the active tab's caret |
| `Space` | expand / collapse the card's `Indexes` and `Checks` groups |
| `Ctrl/Cmd+F` | focuses a `Find table` input that appears in the header (type-ahead; `Enter` jumps and selects, `Esc` closes) |
| `Ctrl/Cmd+A` | selects every card (a drag then moves them together); `Esc` clears |
| `Ctrl/Cmd+0`, `=`, `−` | fit, zoom in, zoom out |
| `Ctrl/Cmd+C` | copies the current card's DDL; with everything selected, the Mermaid text |
| `Delete` | nothing (the diagram never changes the schema; `Drop…` is in the inspector's and the tree's menus) |
| `Shift+F10`, right-click on a card | the tree's row menu (10.1) for that table |

The status bar's left group shows `customers · 3 columns · 2 relationships` for the current
card and `orders.customer_id → customers.id` for a selected line.

## 7. Export
`Export ▾`: `PNG` writes the whole diagram at the current zoom (or `Fit` at 100 % when larger than
8,192 px on a side, then scaled to fit) through our own PNG encoder (D191) to a chosen file (the
Export dialog's file row, data-transfer.md 3); `Mermaid` and `DOT` copy the text
`eggdb_console.er_diagram(schema, 'mermaid' | 'dot')` returns for the shown schemas, with the
shown options (`Columns` off gives the names-only form), to the clipboard and offer `Save…`; `SQL
(DDL)` opens a new tab with every shown table's `CREATE` statements in dependency order. The
toasts: `Exported diagram.png`, `Copied Mermaid (1,204 lines)`.

## 8. Accessibility
| Element | Role | Name | Value / states |
|---------|------|------|------|
| canvas | `group` (`aria-roledescription` `diagram`) | `Diagram of shop` | the summary `12 tables, 14 relationships` as its description |
| card | `button` inside the group (focusable, the current one focused) | `customers, table` (`orders_v, view`) | description `3 columns, 2 relationships, 2.1 million rows`; `selected`; `expanded` for its groups |
| column row | `listitem` under the card's `list` | `id, integer, primary key` | |
| relationship | `button` | `orders.customer_id references customers.id` | description the constraint's name and actions; `selected` |
| schema group | `group` | `Schema public` | |
| minimap | `img` | `Overview` | the viewport as its value (`showing 40 percent`) |
| header controls | `tablist`, `combobox`es, `button`s | their labels | `Show ▾` a `menu` of `menuitemcheckbox`es |
| `Find table` | `searchbox` | `Find table` | |

Moving the current card raises the focus event with its name and description; following a
relationship announces the constraint politely; a layout's end announces `Laid out 412 tables`.

## 9. Acceptance checklist
- [ ] png: `Diagram · shop` for the sample (`customers`, `orders`, `products`, one view) after
      `Auto layout` and `Fit`: `customers` and `products` in layer 0 at the left, `orders` in
      layer 1 with two crow's-foot lines, the view dashed in layer 2, header texts and column
      rows as section 3, the minimap at `(1116, 616, 152, 148)`.
- [ ] png: `Show › Indexes` and `Checks` add the group rows to `orders` with its index and a
      check expression cut with `…`; `Column names only` hides the types; hovering `orders` draws
      its lines 2 px `accent` and `customers`' border `accent`.
- [ ] png: a two-schema stub draws two dashed schema groups with names and a line between them;
      a nullable foreign key draws the circle; a unique one the bar; a self-reference the loop.
- [ ] tree: `Right` from `customers` moves to `orders` (the nearest right), `Ctrl+Right` from
      `orders` follows to `customers` with the status text; `Enter` selects and the inspector
      shows `customers`; the card is a `button` with its description; the canvas's description
      counts tables and relationships.
- [ ] settings stub: dragging `orders` to `(400, 300)` writes `diagram.shop.orders = 400,300`;
      reopening the page draws it there; `Auto layout` asks `Replace your layout?`.
- [ ] export stub: `Mermaid` copies exactly `eggdb_console.er_diagram('public', 'mermaid')`'s
      text; `PNG` writes a file whose size matches the diagram's bounds at 100 %.
- [ ] png: a 2,500-table stub opens the `Choose schemas to draw` dialog instead of laying out.

## Decided by this design
- The diagram is a page tab per connection, drawn by the client from the same catalog query as
  `eggdb_console.er_diagram`, with Mermaid and DOT exports coming from that function so every
  client agrees.
- Cards carry columns, keys, types, and optional index and check groups; views are dashed
  cards; schemas are dashed groups; relationships are orthogonal lines with crow's-foot,
  bar and circle ends, and the constraint's tooltip.
- Auto layout is layered by foreign-key direction with barycentre ordering, off-thread over
  200 tables, refused over 2,000 without a subset; hand positions persist per connection.
- The canvas is keyboard-complete: arrows between cards, `Ctrl/Cmd` + arrows along
  relationships, `Shift` + arrows to move, `Ctrl/Cmd+F` to find; the diagram never alters the schema.
