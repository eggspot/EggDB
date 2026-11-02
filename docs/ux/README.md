# docs/ux: the EggDB client's design

Every screen a user sees is designed by Claude Fable before it is built (CLAUDE.md note 14, D15).
One file per surface or slice; a new slice gets its own file beside these and a line here.

| File | What it holds |
|------|---------------|
| [client-brief.md](client-brief.md) | Fable's brief for the whole client: principles, the main window, the slices in order, the familiar layouts, the design system summary, motion, errors, acceptance criteria |
| [first-screen.md](first-screen.md) | The first slice to build: a native window (macOS, Windows, Linux, browser) with the static connect card, every rect and token, the focus ring, keyboard order, the accessibility tree, 2× rendering, tests (built in `eggdb_gui::connect_screen`, shown by the `eggdb-gui` binary in an X11 window on Linux and the BSDs, a Win32 window on Windows and a Cocoa window on macOS, D256) |
| [gui.md](gui.md) | The desktop GUI: design tokens (colours, spacing, radii, type), the main window at 1280 × 800 as a golden image, components and states, the connect screen, shortcuts, motion, accessibility |
| [gui-layouts.md](gui-layouts.md) | The familiar layouts (works like SSMS, pgAdmin, DBeaver, DataGrip, MySQL Workbench, Compass, Redis Insight): regions, shortcuts, labels, trademark note, switching |
| [cli.md](cli.md) | The `eggdb` command-line tool: one-shot commands, output for a terminal and for a pipe, errors |
| [ui.md](ui.md) | `eggdb ui`: the full-screen terminal UI for browsing and editing a database file |
| [playground.md](playground.md) | The browser playground: the GUI's query and result alone on one static page |
| [command-model.md](command-model.md) | The Egg command model shared with EggOS: verbs, answers, errors and safety fields for people and agents (D143) |
