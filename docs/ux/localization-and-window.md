# Localization and right-to-left, small screens, the remembered window and the OS's theme

Designed by Claude Fable (D15) for roadmap M6e: the GUI localized, first English and Arabic
(mirrored), with what mirrors and what does not (D42); small screens; the remembered window
position; and following the OS's light and dark theme (D15, D256). Every token, rect and role
not restated is [gui.md](gui.md)'s, [first-screen.md](first-screen.md)'s and
[workspace.md](workspace.md)'s. Rects are at 1280 × 800 in the left-to-right frame; a mirrored
rect `(x, y, w, h)` becomes `(W − x − w, y, w, h)`.

## 1. Languages

### 1.1 The strings catalog
Every string a user reads (labels, menu items, dialog titles and bodies, status texts, toasts,
tooltips, accessible names, the `Try:` lines the client writes itself) comes from one catalog
per language built into the binary: `crates/eggdb-gui/strings/<tag>.txt`, one `key = value` line
each (`run = Run`, `rows_time = {rows} rows · {time}`), keys shared by every language, English
the source and the fallback for a missing key (a test fails when a language lacks a key or has
an extra one). Placeholders are `{name}`; plural forms are separate keys chosen by the
language's rules from Unicode CLDR's plural categories, copied into the catalog
(`rows_one = {rows} row`, `rows_other = {rows} rows`; Arabic has `zero`, `one`, `two`, `few`,
`many`, `other`). Not localized: the engine's words that are data or protocol (`SELECT`, `NULL`,
`Committed`'s tag line, SQLSTATE codes, `Error 42P01:`'s code, dialect names, product names,
key and table names), the editor's text, grid cells, the `eggdb` command lines the GUI shows,
and `Error:` / `Try:` messages that come from the engine (the engine's own localization is
D30's work; the GUI shows what it gets). The first two catalogs are `en` and `ar`; a language
is added by adding a file.

### 1.2 Choosing
`View > Language` (layouts-contents.md 8) lists the shipped languages by their own names (`English`,
`العربية`) as radios, plus `System` (the default): the OS's preferred language (macOS
`AppleLanguages`, Windows `GetUserPreferredUILanguages`, Linux `LANG` / `LC_MESSAGES` and the
settings portal, the browser `navigator.languages`), matched by tag then by base language, else
`en`. Stored as `language = ar` in `gui-settings`; a change applies at once to every open window
(strings re-read, layout re-run, no restart), keeping every tab, caret, result and connection.
`--language <tag>` overrides for one run.

### 1.3 Numbers, dates and digits
Counts and times the GUI writes (`1,204 rows · 0.4 ms`) use the language's separators from
CLDR (`1٬204` in Arabic with the Arabic thousands separator, `0٫4`) and the language's digits
by its default numbering system (Arabic's `arab` digits: `١٬٢٠٤`) in labels, status texts and
tooltips only; grid cells, the editor, the value view and anything copied keep the engine's
ASCII digits, since they are data. Timestamps the GUI writes (`taken 2026-11-25 14:12`) keep
ISO order in every language (what the engine shows), with the language's digits. The
`Time:` figure and the CLI's agree in value.

## 2. Right-to-left

### 2.1 Text
The toolkit shapes text with the Unicode Bidirectional Algorithm (UAX #9, implemented from the
standard with the Unicode data tables embedded, D1) per line, with the paragraph direction
from the UI language for UI strings and from the text's first strong character for data
(cells, the editor, the value view); Arabic letters take their contextual forms (isolated,
initial, medial, final) through the font's `cmap` and the Arabic joining rules of UAX #9's
companion data, with ligatures `lam-alef` and the font's `GSUB` `init`, `medi`, `fina`, `rlig`
features read by our own OpenType table reader (D256 named this work); kashida and
justification are not done. The font chains gain an Arabic-capable face per OS (macOS `Geeza
Pro`, `Noto Sans Arabic`; Windows `Segoe UI` already covers Arabic; Linux `Noto Sans Arabic`,
`DejaVu Sans`), used by fallback per glyph run. The caret in bidi text moves logically (by
character in the text's order) and is drawn at the visual position of that boundary; a
selection is drawn as the visual runs it covers (possibly two rects). Mixed text in a cell
(`Ada لوفليس 2026`) lays out by the algorithm; numbers are always left-to-right runs.

### 2.2 What mirrors
When the UI language is right-to-left, the layout tree mirrors: every region's x becomes
`W − x − w`, and inside each region the horizontal order of items reverses. In the golden
frame's words:

| Mirrors | How |
|---------|-----|
| the sidebar and inspector | the sidebar at the right `(1020, 44, 260, 732)` with its splitter at x 1020 … 1026; the inspector and assistant at the left; the rail at the right with the drawer sliding from the right |
| the toolbar | the egg mark and connection name at the right from x 1268 leftwards; `Run`, `Run all`, `Explain`, `Stop` right to left; the dialect pill and `more` at the left |
| the tab strip | tabs from the right edge leftwards; `+` after the last tab to the left; the scroll offset's sign |
| the tree | indent from the right (chevron slot at `W − 8 − 16 × level − 16`), chevrons pointing left for collapsed, down for expanded; the right text at the left edge; type-ahead unchanged |
| the grid's chrome | the row gutter at the right edge, columns from the right, the horizontal scrollbar's origin at the right, the header's resize bands, the current-cell keys `Left` / `Right` swapped so `Right` moves toward the start (visual arrows move visually: `Right` goes right on screen); `Home` / `End` stay logical (first / last column) |
| cell alignment | text cells align to the start edge (the right); numbers, money, dates, times, booleans and `NULL` keep their LTR alignment rule applied visually: numbers stay right-aligned (their digits are LTR runs and the column's digit edge must line up), dates and booleans go to the start edge |
| the results header, the status bar | segments from the right; the status bar's left group at the right and vice versa |
| dialogs | labels and inputs right-aligned, buttons mirrored (the default button leftmost), the title at the right, the check icon at the right of a menu row, chevrons mirrored; `Tab` order unchanged (the logical order) |
| the pending bar, banners, strips | icon at the right, text after it leftwards, buttons at the left |
| the running bar's sweep, the spinner's start, progress bars | the sweep runs right to left; the spinner starts at the top and turns the same way (a clock is not mirrored); progress bars fill from the right |
| the ER view, plan view, graph canvas | the plan tree stays top-down with children laid out right to left; ER cards' column lists right-aligned; the graph is not mirrored (its layout has no reading order) |
| directional icons | chevrons, `Back` / `Next` arrows, `←` `→` pagers mirror; the egg mark, the key, the check, the warning and error icons, the database, table and folder icons do not |

### 2.3 What does not mirror
- The editor: SQL and every query language stay left-to-right with the gutter at the left, the
  text starting at the left, `Ln, Col` logical; Arabic string literals inside the SQL are shaped
  and ordered by the algorithm within the line, so `'لوفليس'` reads right-to-left inside an LTR line.
- Grid cells, the value view's body, the DDL box, `Messages`, the plan's text form, code boxes
  in cards and notebooks: their content keeps the engine's order with per-line bidi; the
  container's chrome mirrors.
- Numbers' digit order; timestamps' field order; key and file paths; URLs; command lines.
- Keyboard shortcuts: `Ctrl/Cmd+Enter` and the rest are the same keys; `Alt+Left` / `Alt+Right`
  on a column (9.5) act visually (`Alt+Right` widens toward the right edge).
- Splitter keys act visually: `Left` moves the splitter left on screen.

### 2.4 Checking
The golden tests render the main window, the connect screen, the picker, a dialog, a menu, the
value view, the plan and the Users page in `ar`, light, as separate expected PNGs
(`main_ar_light.png` and so on), with every rect mirrored by the formula and the Arabic
strings from the catalog; a test asserts that the mirrored frame's pixel at `(W − 1 − x, y)`
equals the LTR frame's at `(x, y)` for the regions that only reverse (the sidebar's fill, the
splitter lines, the grid's zebra rows), so the mirroring is a rule and not a second layout. The
accessibility tree carries `aria-orientation` and the reading direction (`AXLanguage` /
`Culture` / the `locale` attribute / `lang` and `dir` on the root) and the same logical order.

## 3. Following the OS's theme
`View > Theme` has `System` (the default, stored `theme = system`), `Light`, `Dark`, `High
contrast`. Under `System` the theme is the OS's and follows it live: macOS
`NSApp.effectiveAppearance` and `NSAppearanceDidChangeNotification` (and `accessibilityDisplayShouldIncreaseContrast`);
Windows `AppsUseLightTheme` under `HKCU\…\Themes\Personalize` with `WM_SETTINGCHANGE`
(`ImmersiveColorSet`) and `SPI_GETHIGHCONTRAST` with its change; Linux the settings portal's
`org.freedesktop.appearance` / `color-scheme` (and `contrast`) through `ReadOne` and
`SettingChanged`, falling back to XSETTINGS `Net/ThemeName` ending in `-dark` and GTK's
`gtk-application-prefer-dark-theme`; the browser `prefers-color-scheme` and `prefers-contrast`
with their `change` events. A change redraws at the next frame with the new tokens and no
motion, keeping everything else; the status bar's left group shows `Theme: dark (system)` for 2
s. `--theme` overrides for one run as first-screen.md says. High contrast from the OS wins over
dark; an explicit `Light` or `Dark` choice ignores the OS's contrast setting but keeps
first-screen.md's rule that nothing is said by colour alone.

## 4. The remembered window
At close (and every 5 s while moved or resized, so a crash loses little) the client writes to
`gui-settings`: `window.x`, `window.y`, `window.width`, `window.height` (the normal, unmaximized
frame in logical pixels of the display), `window.maximized`, `window.fullscreen`, `window.display`
(the display's name or id the frame's centre was on). At start the window opens at that frame
when the display exists and the frame's visible part is at least 200 × 100 inside it; a frame
on a display that is gone, or off every display, is moved to the main display's work area,
centred, at the same size clamped to the work area; a saved size under the minimum takes the
minimum. Without a saved frame the first window is 1280 × 800 centred on the main display (or
the work area's size minus 80 px each way when smaller). `window.maximized` restores maximized
(on macOS zoomed), `window.fullscreen` restores full screen; a second window (`File > New
Window`) opens offset 24 px down and right from the first. The browser build ignores all of it
(the viewport is the window). Each layout's splitter sizes (polish.md 6.2) stay separate from
the window frame.

## 5. Small screens
The client's minimum is 800 × 560 (first-screen.md); gui.md gives the rail under 800 and
client-brief.md the browser's drawer under 1024. The rules in full, by the main area's width:

| Width | Sidebar | Inspector | Results | Toolbar |
|-------|---------|-----------|---------|---------|
| 1440 and wider | docked | docked by default | below the editor | every button with its label |
| 1024–1439 | docked | overlay on `Ctrl/Cmd+I` | below | as above |
| 800–1023 | the rail and its drawer (10.6) | overlay | below | `Run all` and `Explain` become icon-only (28 × 28, tooltips), the dialect pill 96 wide |
| 600–799 (browser only) | the drawer, opened from the rail's `Objects` | overlay, full width | **a segment beside the editor**: the tab strip's row gains `Editor` / `Results` segments at its right `(W − 180, 48, 168, 28)` and only one of the two shows, `Ctrl/Cmd+J` toggling them; a run switches to `Results` | icon-only buttons; the connection name cut with `…` |
| under 600 (browser only) | as above | as above | as above; the status bar keeps the connection dot, the summary and `Ln, Col`, dropping the rest into a `…` button that opens them as a menu | the toolbar wraps to two rows (44 + 36): buttons on the first, the dialect pill and `more` on the second |

The connect card under 600 px wide becomes the viewport's width minus 24 with its fields stacked
one per row (`Host` over `Port`, `User` over `Password`), scrolling inside the viewport. Dialogs
clamp to the viewport minus 24 and scroll their body. The grid and tree are unchanged (they
scroll); the plan view fits by scrolling; model tabs' control bars wrap to two rows under 800.
Touch (D313): a tap is a click, a long press (500 ms) the right-click menu, a drag on a region
scrolls it, a drag on a splitter moves it with a 24 px hit band under touch, a pinch zooms the
ER view and the graph canvas, and hit sizes under touch are at least 32 px (rows stay 24 tall
but a tap lands on the nearest row).

## 6. Accessibility
The root node carries `lang` and the reading direction; every string in the tree is the
catalog's in the chosen language, so a screen reader in Arabic reads Arabic names; the grid's
and tree's column and position indices are logical and unchanged by mirroring; `aria-orientation`
of splitters is unchanged (a vertical splitter is still vertical). Theme changes raise no
announcement; a language change announces the window's new name (`EggDB · shop`) through the
focus event on the focused control. The window's frame restore raises nothing.

## 7. Acceptance checklist
- [ ] catalog: `ar.txt` has exactly `en.txt`'s keys; a missing key fails the test; `{rows}`
      placeholders match between the two.
- [ ] png: `main_ar_light.png` at 1280 × 800 with the sidebar at `(1020, 44, 260, 732)`, the tree's
      chevrons at the right, the toolbar's `Run` (Arabic label) at the right, the grid's gutter at
      the right with the row numbers `١ ٢ ٣` (gutter numbers are the GUI's, so they take the
      language's digits), the cells `1`, `2`, `3` in ASCII, `Ada Lovelace` at the cell's right (start) edge,
      `id` right-aligned at its digit edge; the mirror-pixel assertion holds on the sidebar fill.
- [ ] png: the editor in `ar` keeps its gutter at x 260 and `SELECT` at x 320; a literal
      `'لوفليس'` draws shaped with its letters joined and ordered right-to-left inside the line;
      the caret after the literal sits at the literal's visual left edge.
- [ ] png: the Drop dialog in `ar` with the title at the right, `Cancel` (default) leftmost, the
      statement box LTR; the dialect menu with the check icon at the right of the row.
- [ ] theme stub: `System` with the OS reporting dark draws the dark golden frame; a change to
      light redraws light at the next frame with the status text `Theme: light (system)`; the OS
      reporting high contrast draws the high-contrast tokens.
- [ ] settings stub: closing at `(100, 80, 1280, 800)` on display `A` writes the six keys; a
      start with display `A` absent opens centred on the main display at 1280 × 800; a saved
      `640 × 400` opens at 800 × 560.
- [ ] png at 700 × 600 in the browser build: the `Editor` / `Results` segments at the strip's
      right, only the editor shown, `Ctrl+J` showing the grid; at 560 × 600 the two-row toolbar
      and the status bar's `…` button; the connect card with stacked fields.
- [ ] tree: the root carries `lang=ar` and `dir=rtl`; the tree rows' positions and the grid's
      column indices equal the LTR dump's.

## Decided by this design
- One `key = value` catalog per language built into the binary, English the fallback, CLDR
  plural categories; the engine's words, data, SQL and command lines are never translated by
  the GUI.
- UI numbers take the language's digits and separators; data keeps ASCII.
- Bidi and Arabic shaping are our own implementations of the public standards; kashida is out.
- Mirroring is a rule over the layout tree (`x → W − x − w`), not a second layout; editors,
  cells, code and numbers keep their order; numbers stay right-aligned; arrows and splitter keys
  act visually, `Home` / `End` logically.
- The theme follows the OS live under `System`, with high contrast winning; the window frame is
  remembered per display and clamped to a visible display.
- Under 800 px (the browser) results become a segment beside the editor, and under 600 the
  toolbar wraps and the status bar folds into `…`.
