# Streaming rows, the value view's selection, scrollbars, cursors, splitters, the rail and motion

Designed by Claude Fable (D15) for roadmap M6e's leftovers of D275, D310 and D311: rows streamed
into the grid with pages fetched on scroll; the value view's text selection and SQL type names;
the menu, dialog and scroll bar roles in the platform layer; scrollbars that hide and fade; the
editor's draggable and horizontal scrollbars; the pointer cursors; the results grid filling a
taller results region; every splitter as a `Tab` stop, in every layout; the rail's hover, tooltips
and roles; and the fades, the display-rate redraw, the spinner, the sweeping bar and reduced
motion. Every token, component, rect and role not restated is [gui.md](gui.md)'s and
[workspace.md](workspace.md)'s (sections 3, 9 and 10). Rects are `(x, y, w, h)` at the 1280 × 800
golden frame, which none of this changes.

## 1. Rows streamed into the grid (D275)

### 1.1 Two counts
A result has `fetched` (rows the client holds) and `total` (rows the engine says exist, or
`unknown` while a cursor is open). The grid's row count for the scrollbar, the gutter and the
accessibility tree is `total` when known, else `fetched + 1,000` (one page of room), so the thumb
never jumps to the end and then back.

| Phase | Summary (2.4) | Grid |
|-------|------|------|
| running, rows arriving | `Running · 1.2 s · 12,400 rows so far` | rows appended every 100 ms or 1,000 rows; scroll kept at the top unless the user scrolled, then kept on the first visible row |
| finished, all rows held | `12,400 rows · 1.4 s · 8.2 MB` | |
| finished, more rows on the cursor (over 1,000 rows on a server; the engine keeps the cursor open on the tab's session) | `1,000+ rows · 0.3 s` until the count is known; `12,400 rows · 1.4 s` once the last page came | pages of 1,000 fetched as 3.1 says, two pages ahead of the scroll |
| a page in flight | the summary adds ` · fetching rows 2,001–3,000` in `textSecondary` | the page's rows draw 3.1's placeholder cells (60 × 12 `bg2` rounded rects at the cell's text position), the gutter still numbered; the page's first row announces `Loading rows 2,001 to 3,000` politely once |
| `Ctrl/Cmd+End` | `fetching the last page` while the engine counts and fetches | the current cell lands on the last row when it arrives; `Esc` meanwhile cancels the jump and stays |
| stopped | `Stopped · 12,400 rows (partial)` | the rows stay, no more pages |

A page takes the pointer's wheel, `PgDn` or the thumb drag past `fetched − 2,000` as its trigger;
pages are fetched in order, one at a time, and a page requested while another is in flight waits.
The placeholder is never a spinner: it is still, and the summary carries the words.

### 1.2 Errors mid-stream
When the engine or the connection fails after rows arrived, the rows stay (a result is a
snapshot), the summary reads `Error`, `Messages` shows the engine's text, and 3.7's banner sits
between the results header and the grid header with the error icon (`error`, 12 px) at x 272
and `body` `textPrimary` at x 292: `Error after 12,400 rows: connection to db:5432 lost` (the
first line of the engine's message, cut with `…` at x 1120), a ghost button `Retry` right-aligned
12 px from the right edge (runs the statement again from the start; `F5` in the grid does the
same) and the close icon after it. The banner is a polite `status`. A failed page fetch (not the
run) keeps the result and its summary, shows the same banner with `Error fetching rows
2,001–3,000: …`, and the placeholder cells read `unavailable` in `textDisabled` until `Retry`.

### 1.3 Keyboard and announcements
Nothing moves the current cell while rows stream. The status line is 7's polite live region and
speaks once per phase change, never per page: `Running, 12,400 rows so far` at most every 5 s,
`12,400 rows, 1.4 seconds` at the end, `Error after 12,400 rows` (assertive, through the banner).
The grid's `aria-rowcount` / `RowCount` is `total` or `−1` (`unknown`) while a cursor is open, and
each fetched row carries its index.

## 2. The value view's selection and type names (D310)

### 2.1 Selection
9.3's body gains text selection: a drag selects by character across the wrapped lines;
`Shift` + arrows, `Shift+Home` / `Shift+End` (line ends), `Ctrl/Cmd+Shift+Home` / `End` (the
whole text's ends) extend it from the caret; `Ctrl/Cmd+A` selects everything; a double-click
selects the word, a triple-click the line. The caret is 9.3's body's own (2 px `accent`, solid
for 500 ms after a key, never blinking in a read-only body under reduced motion, as 6.2 says);
selection is `selection` behind the glyphs, `selectionInactive` while the body is not focused.
`Ctrl/Cmd+C` and `Copy` copy the selection when one exists, else the whole value as 9.3 says;
the toast says `Copied 48 characters` for a selection. The body's role stays a read-only
`textbox` named `Value`, whose selection range is exposed through the text pattern (UIA
`TextPattern`, AT-SPI `Text`, NSAccessibility selected text range, ARIA the DOM selection).

### 2.2 The type name
9.3's header reads `<column> · <type>` with the type as the engine names it in the tab's dialect:

| Dialect | Examples |
|---------|----------|
| PostgreSQL (and a file) | `integer`, `text`, `numeric(12,2)`, `timestamp with time zone`, `jsonb`, `bytea · 184 bytes`, `vector(768)` |
| MySQL | `int`, `varchar(255)`, `decimal(12,2)`, `datetime`, `json`, `blob · 184 bytes` |
| SQL Server | `int`, `nvarchar(255)`, `decimal(12,2)`, `datetime2`, `varbinary(max) · 184 bytes` |
| Oracle | `NUMBER(12,2)`, `VARCHAR2(255)`, `TIMESTAMP WITH TIME ZONE`, `BLOB · 184 bytes` |
| SQLite | the declared type (`INTEGER`, `TEXT`), else the storage class in lower case |

A keys tab's cells keep 10.4's `value · 1,204 bytes` and 10.10's `value · hash · 12 fields`. The
grid's column header exposes the same type as its description (`id, integer, width 80 px`), and a
tooltip (section 5) over a header shows `id · integer` after 600 ms.

## 3. Scrollbars (D310)

### 3.1 Showing, hiding, fading
9.6 gives the geometry; this is the state machine, one per scrollbar:

| State | Drawn | Leaves on |
|-------|-------|-----------|
| `hidden` | nothing | a wheel notch, a key that scrolls, a drag, or the pointer entering the region → `shown` |
| `shown` | thumb `textDisabled` at 60 % alpha | 800 ms after the last scroll, key, drag or pointer movement in the region → `fading`; the pointer over the track → `hovered` |
| `hovered` | thumb `textSecondary`, 8 px wide | the pointer leaves the track → `shown` (timer restarted); a press → `dragging` |
| `dragging` | as `hovered` | release → `shown` |
| `fading` | alpha from 60 % to 0 over 200 ms linear | the fade's end → `hidden`; any trigger of `hidden` → `shown` from the alpha drawn |

The timer is one deadline the frame scheduler (section 7) wakes for. Under reduced motion the
states are `hidden` (content fits) and `shown` only: no timer, no fade. Content that fits the
viewport has no scrollbar in any state, and a region that gains a scrollbar while the pointer is
inside it starts `shown`.

### 3.2 The editor's own scrollbars
The editor keeps a scroll offset of its own, `(ox, oy)`, in pixels: `oy` is a multiple of 20
(whole lines), `ox` any value. Content height is `lines × 20 + 4 × 20` (four lines of margin
below the last, so the caret is never at the bottom edge); content width is `max line advance +
48` from the text origin. Vertical track 9.6's `(1270, 80, 10, 300)`, horizontal `(308, 370,
962, 10)`, each shown only when its content exceeds its viewport; the thumb is draggable (9.6's
formula), the track pages, the wheel moves 60 px, `Shift` + wheel 60 px horizontally. The caret
is scrolled into view with four lines of vertical margin and 48 px of horizontal margin on
every key, and a horizontal scroll never happens while the caret's column fits. The gutter
never scrolls horizontally; the current-line highlight spans the viewport. The error strip (2.5)
takes the editor's last 28 px and shortens both tracks by 28.

### 3.3 Roles in the platform layer
Each platform bridge gets these three roles, which D329's `menu` and `menuitem` work completed
in part; the exact values the engineer maps:

| Generic | macOS | UI Automation | AT-SPI2 | ARIA |
|---------|-------|---------------|---------|------|
| `scrollbar` | `AXScrollBar`, `AXOrientation`, `AXValue` 0–1 | `ScrollBar` control type, `RangeValue` pattern (0–100, `IsReadOnly` false) | `ATSPI_ROLE_SCROLL_BAR`, `Value` interface | `role=scrollbar`, `aria-orientation`, `aria-valuenow/min/max`, `aria-controls` |
| `dialog` (non-modal: the value view, the drawer, popovers with focus) | `AXPopover` | `Window` with `IsDialog` true, `IsModal` false | `ATSPI_ROLE_DIALOG` | `role=dialog`, `aria-modal=false` |
| `alertdialog` (modal: the picker, confirm and Drop dialogs) | `AXSheet` | `Window`, `IsDialog` and `IsModal` true, `WindowPattern` | `ATSPI_ROLE_ALERT` with `ATSPI_STATE_MODAL` | `role=alertdialog`, `aria-modal=true` |
| `menu` / `menuitem` / `menuitemradio` / `menuitemcheckbox` | `AXMenu` / `AXMenuItem` (`AXMenuItemMarkChar` for the check) | `Menu` / `MenuItem` with `Invoke`, `Toggle` for check items | `ATSPI_ROLE_MENU` / `MENU_ITEM` / `RADIO_MENU_ITEM` / `CHECK_MENU_ITEM` | the ARIA role of the same name, `aria-checked` |
| `tooltip` | `AXHelp` on the described node and a transient `AXGroup` role `AXHelpTag` | `ToolTip` control type; the described node's `HelpText` | `ATSPI_ROLE_TOOL_TIP` | `role=tooltip`, `aria-describedby` on the described node |

A modal dialog moves focus in and raises the platform's window-opened event; a non-modal one
raises focus only. A scrollbar's value change raises the property-changed event at most every
100 ms while dragging.

## 4. Pointer cursors (D310)
The platform layer sets the OS cursor (`NSCursor`, `SetCursor` with the system `IDC_*` cursors,
X11's cursor font and the Wayland cursor theme through the shell's `wp_cursor_shape`, the CSS
cursor name in the browser) from one enum, chosen on every pointer move by what is under it:

| Cursor | Where |
|--------|-------|
| `arrow` | everywhere else |
| `text` (I-beam) | the editor's text area and gutter, inputs, the value view's body, the DDL box |
| `column-resize` (↔ with a bar) | a grid header separator's 6 px band (9.5), the sidebar and inspector splitters' bands |
| `row-resize` (↕ with a bar) | the editor/results splitter, every horizontal splitter of section 6 |
| `pointer` (hand) | links (`Where are you coming from?`, `Try:` lines that name a command), the status bar's clickable items, the `Works like` item |
| `not-allowed` | a drag over the window that is not a file (1.3) |
| `grabbing` | while a splitter or a scrollbar thumb is being dragged, and a column separator; kept until release, wherever the pointer goes |

No cursor is drawn by the toolkit itself; the browser build sets `canvas.style.cursor`.

## 5. Tooltips
One tooltip at a time: popover `bg0`, 1 px `border`, `radiusPopover`, shadow 0 4 12 at 16 %
black, `small` `textPrimary`, padding 4 8, at most 320 wide (wrapped at words) and 6 lines; placed
below the control with a 4 px gap, left-aligned to it, moved left to end at x `W − 8` and flipped
above when it would cross y `H − 8`; the rail's tooltips sit to the right of the item (10.6).
Shown 600 ms after the pointer rests over a control that has one; moving to another such control
within 300 ms of hiding shows the new one at once (no wait); hidden on leave, on any key, on a
press, and when the window deactivates. `Ctrl/Cmd+Shift+Space` shows the focused control's
tooltip at once and `Esc` hides it (the plan's rule, now everywhere). Controls with tooltips:
toolbar buttons (the label plus the shortcut, `Run · Ctrl+Enter`), the dialect pill (`Dialect ·
PostgreSQL`), rail items, grid headers (`id · integer`), tree rows whose label is cut (the full
label), status bar items (`Works like pgAdmin 4` carries the trademark note), the tab strip's
cut labels, the splitters (`Drag or use the arrow keys · 260 px`), plan nodes (5.4). A tooltip
is never the only place a thing is said. Role `tooltip` (3.3), and the described node gets it as
its description while shown; no live region.

## 6. Splitters and regions (D311)

### 6.1 The results grid in a taller results region
The results body is `results` minus its 32 px header, minus 28 for each banner shown; the grid
header takes its first 28 px and the body rows the rest: `visible = floor((body_h − 28) / 24)`
whole rows, the last partial row clipped. At 1280 × 800 that is 13 rows; after the editor/results
splitter moves to y 200 it is 20. The horizontal scrollbar's track sits at the region's bottom
edge (9.6) wherever that is; the vertical one spans the body under the grid header. The empty
part of the body below the last row is `bg0`, no zebra. The plan view and `Messages` fill the
same body. `Ctrl/Cmd+J` (toggle the results panel) collapses `results` to its 32 px header
(the splitter at the bottom) and back to the remembered ratio.

### 6.2 Every splitter a `Tab` stop, in every layout
10.5's three splitters and their keys hold for every layout; each layout's splitter list, in
`Tab` order (right after the region on its left or above it, `F6` skips them), with the settings
keys written as `<layout>.<key>`:

| Layout | Splitters (band at the layout's rects; line; key; range, default) |
|--------|------|
| EggDB | sidebar (256, 44, 6, 732) x 259 `sidebar_width` 180–480 (260); editor/results (260, 380, 1020, 6) y 382 `results_ratio` (0.40); inspector (1157, 44, 6, 732) at 1440 x 1160 `inspector_width` 200–480 (280) |
| SSMS | sidebar x 299 `sidebar_width` (300); editor/results y 419 `results_ratio` (0.47); properties (977, 44, 6, 732) x 980 `properties_width` 200–480 (300), while shown |
| pgAdmin 4 | sidebar x 299 (300); editor/results y 415 (0.48); scratchpad (997, 80, 6, 336) x 1000 `scratchpad_width` 200–400 (280), while shown |
| DBeaver | sidebar x 279 (280); navigator/projects (0, 481, 280, 6) y 483 `projects_ratio` 0.2–0.6 (0.40); editor/results y 399 (0.50) |
| DataGrip | sidebar x 299 (300); editor/results y 419 (0.47) |
| MySQL Workbench | sidebar x 259 (260); editor/results y 359 (0.36); results/output (260, 623, 1020, 6) y 625 `output_height` 80–400 (150), while shown |
| MongoDB Compass | sidebar x 299 (300); results/shell (300, 557, 980, 6) y 559 `shell_height` 120–480 (240), while the shell is expanded |
| Redis Insight | editor/results (60, 221, 1220, 6) y 223 `results_ratio` (0.72); results/cli (60, 533, 1220, 6) y 535 `cli_height` 120–480 (240), while shown; the rail is fixed at 60 |

Names for the accessibility tree and the status bar follow the layout's words: `Object Explorer
width`, `Properties width`, `Data Output height`, `Output height`, `Shell height`, `CLI height`;
the status bar's drag text uses the same (`Properties · 300 px`). A hidden extra region's
splitter is not in the tree and not a `Tab` stop. Ranges are clamped at every window size so
the editor keeps 80 px and the results 120, as 10.5 says; a splitter at its limit draws its line
in `borderStrong` instead of `accent` while dragged, and the status bar text adds ` (min)` or
` (max)`.

### 6.3 The rail, the drawer and their roles
10.6's rail: hover `bg2` fading as section 7 says; the tooltip of section 5 to the right of the
item after 600 ms, with the item's name and, for `Objects`, `Ctrl/Cmd+B`; the drawer opened from
an item keeps that item `pressed`. Roles: the rail `toolbar` named `Objects rail`
(`aria-orientation=vertical`), one `Tab` stop, its items `button`s (`pressed` for the one the
drawer was opened from, the tooltip as each item's description); the drawer a non-modal `dialog`
named `Objects` (3.3) holding the filter `searchbox` and the `tree`, focus moved in on open and
returned on close to the node that had it (the rail item, the editor, the grid), announced by the
focus event only. The splitters: `separator` / `AXSplitter` / `Thumb` with `Transform` /
`ATSPI_ROLE_SEPARATOR` with `Value`, named as 6.2, value the size in px with min and max,
`aria-orientation` `vertical` for a left/right splitter (it separates columns), `horizontal` for
an up/down one; the drag end and every key raise the value change, read as `Sidebar width,
268 pixels`.

## 7. Motion (D311)

### 7.1 What fades
10.8's list, as the one table the renderer implements, each a fill (never a border, text or
icon) blended `lerp(from, to, t / 80 ms)` per sRGB channel with alpha:

| Surface | From → to on hover | Pressed |
|---------|------|------|
| primary button | `accent` → `accentHover` | `accentPressed` |
| secondary, ghost, dropdown, dialog buttons | none or `bg0` → `bg2` | `bg2` (border `borderStrong` at once) |
| tree row, rail item, grid row, inspector block heading | none → `bg2` | none |
| inactive tab | `bg2` → `bg1` | |
| status bar item pill | none → `bg1` | |
| menu and popover highlighted row, the picker's rows, completion rows | none → `bg2` for hover; `selection` for the highlighted row changes at once | |
| scrollbar thumb | `textDisabled` 60 % → `textSecondary` 100 % (and the width 6 → 8 at once) | |
| document and result cards (data-models.md) | `bg0` → `bg1` | |

A fade that reverses restarts from the colour currently drawn. Selection fills, focus rings,
the `accent` cell rect, splitter lines, tab switches, tree expansion, dialog and menu opening and
closing, layout switches and resizes change at once, in every mode.

### 7.2 The frame scheduler
The window redraws on an event (key, pointer, resize, a result page, a toast) and otherwise
only while an animation is registered: each animation has a start, a duration and a deadline
(a fade's end, the caret's next toggle, the scrollbar's 800 ms timer and its fade, the running
bar's next frame, the spinner's next frame, a tooltip's 600 ms wait, a toast's 8 s). The
platform layer's `next_event(timeout)` is given the nearest deadline; while a continuous
animation runs (the bar, the spinner, a fade) the timeout is the display's frame interval (from
`CVDisplayLink`'s nominal rate, `DwmGetCompositionTimingInfo`, the X11 RandR mode or the Wayland
output's refresh, the browser's `requestAnimationFrame`; 60 Hz when unknown), capped at 120 Hz
and never faster than the compositor presents. With nothing registered the window sleeps until
an event. A frame presents only the regions whose pixels changed when the platform supports a
damage rect, else the whole frame. Under reduced motion no fade, blink, sweep or spin is ever
registered, so the window redraws on events alone.

### 7.3 The spinner and the sweeping bar
- **Spinner** (a running tab's close slot, a job in the tray): a 12 px ring, 1.5 px `accent`,
  270° arc starting at the top and turning one full turn per 1,000 ms (angle = `elapsed /
  1000 × 360`), drawn from 24 line segments; reduced motion: the static ring with a 3 px
  `accent` dot at the top, plus the word `running` in the tab's accessible name.
- **Sweeping bar** (the toolbar's bottom edge, the connect card's bottom edge, a wizard's
  footer): a 2 px `accent` bar 120 px wide whose left edge moves from `x0 − 120` to `x1` over
  1,000 ms linear and repeats, clipped to the edge; reduced motion: the full-width static bar and
  `Running…` in the status bar (a wizard: in its footer).

### 7.4 Reduced motion, every source
`reduced_motion = os || flag || menu`. The OS value is read at start and on change: macOS
`accessibilityDisplayShouldReduceMotion` and `NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification`;
Windows `SystemParametersInfo(SPI_GETCLIENTAREAANIMATION)` and `WM_SETTINGCHANGE`; X11 the
XSETTINGS `Gtk/EnableAnimations` property (false means reduced) and its `PropertyNotify`;
Wayland the settings portal (`org.freedesktop.portal.Settings`, key
`org.gnome.desktop.interface` / `enable-animations`, read with `ReadOne` and followed through
`SettingChanged` over D-Bus with our own message encoding) and, failing the portal, XSETTINGS
when Xwayland is present; the browser `matchMedia('(prefers-reduced-motion: reduce)')` and its
`change` event. `View > Reduce motion` is a check item in the `View` menu (layouts-contents.md
section 6) stored as `reduce_motion = true` in `gui-settings`; it and `--reduced-motion` force
the value on and the menu item shows checked and disabled (`Reduced motion is on in the system`)
when the OS forces it. A change applies at the next frame: running fades finish at once, the
caret turns solid, scrollbars show, the bar and spinner become their static forms.

## 8. Acceptance checklist
- [ ] png: a stub cursor result of 12,400 rows at 1,000 per page: at 300 ms the summary reads
      `Running · 0.3 s · 1,000 rows so far`; after the run `1,000+ rows · 0.3 s`; `PgDn` × 3
      draws placeholder cells for rows 2,001–3,000 and the summary's `fetching rows 2,001–3,000`;
      a stub failure on page 3 shows the banner `Error fetching rows 2,001–3,000: …` with
      `Retry`; tree: the grid's row count is `unknown` then `12,400`.
- [ ] png: the value view with a drag over 12 characters draws `selection` behind them and
      `Ctrl+C` copies exactly those, toasting `Copied 12 characters`; the header reads
      `total · numeric(12,2)`; in a MySQL tab `total · decimal(12,2)`.
- [ ] png: frames at 0, 700, 900 and 1,100 ms after a wheel notch in the grid show the thumb at
      60 %, 60 %, about 30 % (halfway through the fade) and gone; under reduced motion all four
      show it; a 400-line editor draws both tracks, a thumb drag to the end shows line 400, and
      `Shift` + wheel moves the text 60 px left with the gutter still.
- [ ] tree: the platform dump of an open grid menu has a `menu` with `menuitem`s; the Drop
      dialog an `alertdialog` with `modal`; the value view a `dialog` not modal; each scrollbar
      a `scrollbar` with orientation and value; the rail's tooltip a `tooltip` referenced by the
      item's description.
- [ ] cursor stub: moving over a header separator asks for `column-resize`, over the
      editor/results splitter `row-resize`, over the editor `text`, during a thumb drag
      `grabbing` even outside the track.
- [ ] png: with the editor/results splitter at y 200 the grid draws 20 body rows and its
      horizontal track at y 766; in the SSMS layout with `F4` the properties splitter is a `Tab`
      stop after the results and reports `Properties width, 300 pixels`; `Home` on it reads
      ` (min)` in the status bar.
- [ ] png: hover frames at 0, 40 and 80 ms over a tree row show `bg2` at 0 %, 50 % and 100 %;
      a leave at 40 ms reverses from 50 %; under reduced motion the first frame is 100 %.
- [ ] scheduler stub: with no animation the window waits with no timeout; a running tab makes it
      wake at the display interval; a scrollbar's hide wakes it once at 800 ms and then at the
      frame interval for 200 ms; `View > Reduce motion` checked stops every wake.
- [ ] png: the spinner at 0, 250 and 500 ms shows the arc's start at 0°, 90° and 180°; the bar at
      500 ms has its left edge at `x0 + 500 / 1000 × (x1 − x0 + 120) − 120`; a stub Wayland portal
      `enable-animations = false` makes both static at the next frame.

## Decided by this design
- A streaming result has two counts; the scrollbar and the tree use `total` when known, else
  `fetched` plus one page, and the summary says `1,000+ rows` until the engine counts.
- A page in flight is still placeholder cells plus words in the summary; one page at a time, in
  order; a mid-stream error keeps the rows and offers `Retry` in the banner.
- The value view gets a caret and a selection but stays read-only; `Copy` copies the selection
  when one exists.
- Type names are the engine's own in the tab's dialect, never a generic name, so the header
  matches what `\d` or `DESCRIBE` would say.
- Scrollbars are a five-state machine with one timer; the editor scrolls by whole lines
  vertically and keeps 48 px of horizontal margin for the caret.
- Seven cursors cover every control; `grabbing` holds through a drag wherever the pointer goes.
- One tooltip component, 600 ms, with a 300 ms grace between neighbours, shown on request with
  `Ctrl/Cmd+Shift+Space`, and never the only place a thing is said.
- Every layout's splitters, including the extra regions', are named in the layout's words and
  keyed `<layout>.<key>` in `gui-settings`; a splitter at its limit says so.
- Only fills fade; the frame scheduler wakes for deadlines and sleeps otherwise; reduced motion
  comes from the OS (including Wayland's portal and the browser), the flag or the menu, and is
  only ever forced on.
