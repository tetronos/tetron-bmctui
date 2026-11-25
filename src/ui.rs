//! Layout and drawing. `draw` turns the app state into a `Canvas` and records
//! the clickable regions in `app.hits`, so what is drawn and what can be
//! clicked come from the same code and can not drift apart.
//!
//! Width rules:
//!   120 columns or more   navigation, main, details
//!   97 to 119             navigation, main; details on request
//!   below 97              one pane; navigation opens as an overlay

use crate::app::{App, Hit, Mode, NAV, Pane};
use crate::canvas::{Canvas, Rect};
use crate::ipmi::{self, Screen};
use crate::view::{self, Kind, wrap};
use crossterm::event::KeyCode;

pub const WIDE: usize = 120;
pub const MEDIUM: usize = 97;
/// Inner widths of the side panes. With these, an 80 column main pane (the
/// future console) fits at 97 columns, and with details at 120.
const NAV_W: usize = 14;
const DETAILS_W: usize = 22;
/// Columns taken by the navigation, the details pane and their borders.
pub const SIDE_PANES: usize = NAV_W + DETAILS_W + 4;
const MIN_W: usize = 40;
const MIN_H: usize = 12;

/// Outer rectangles (borders included) of the panes that are visible. Side by
/// side panes share a border column.
#[derive(Debug, Default)]
pub struct Layout {
    pub nav: Option<Rect>,
    pub main: Option<Rect>,
    pub details: Option<Rect>,
}

/// Decide which panes are visible and where. `nav_open` only matters on a
/// narrow terminal, where the navigation is an overlay on top of the others.
pub fn layout(w: usize, h: usize, details: bool, nav_open: bool) -> Layout {
    // Row 0 is the host bar, the last two rows are status and key hints.
    let (top, height) = (1, h.saturating_sub(3));
    let mut l = Layout::default();
    if w >= MEDIUM {
        l.nav = Some(Rect::new(0, top, NAV_W + 2, height));
        let left = NAV_W + 1;
        let right = if details {
            let d = Rect::new(w - DETAILS_W - 2, top, DETAILS_W + 2, height);
            l.details = Some(d);
            d.x + 1
        } else {
            w
        };
        l.main = Some(Rect::new(left, top, right - left, height));
    } else {
        let full = Rect::new(0, top, w, height);
        if details {
            l.details = Some(full);
        } else {
            l.main = Some(full);
        }
        if nav_open {
            l.nav = Some(Rect::new(0, top, NAV_W + 2, height));
        }
    }
    l
}

/// Area inside the border of a pane.
fn inner(r: Rect) -> Rect {
    Rect::new(r.x + 1, r.y + 1, r.w.saturating_sub(2), r.h.saturating_sub(2))
}

/// Draw a pane border with its title. The focused pane has a highlighted title.
/// The whole pane is registered first, so that regions drawn inside it later
/// take precedence for clicks.
fn pane(app: &mut App, c: &mut Canvas, r: Rect, p: Pane, title: &str) {
    c.clear(r);
    c.frame(r);
    let kind = if app.focus == p { Kind::Sel } else { Kind::Head };
    c.text(r.x + 2, r.y, r.w.saturating_sub(4), &format!(" {title} "), kind);
    app.hits.push((r, Hit::Pane(p)));
}

/// Draw a clickable label and return the column after it.
fn button(app: &mut App, c: &mut Canvas, x: usize, y: usize, label: &str, hit: Hit, kind: Kind) -> usize {
    let w = label.len();
    if x + w <= c.w {
        c.text(x, y, w, label, kind);
        app.hits.push((Rect::new(x, y, w, 1), hit));
    }
    x + w
}

/// Text of one host tab. Only a visited host has a known state.
fn host_tab(app: &App, i: usize) -> String {
    let state = match app.states[i].session_ok {
        Some(true) => "ok",
        Some(false) => "error",
        None => "unknown",
    };
    // Only the host in use is contacted, so each state carries its time.
    let state = match &app.states[i].checked {
        Some(at) => format!("{state} {at}"),
        None => state.to_string(),
    };
    // Only the first nine hosts have a number key.
    let key = if i < 9 { format!("{} ", i + 1) } else { String::new() };
    format!("[{key}{} {state}]", app.hosts[i].label())
}

/// Top row: one clickable tab per host. When they do not all fit, the row
/// starts late enough that the tab of the host in use is visible.
fn host_bar(app: &mut App, c: &mut Canvas) {
    let mut x = 0;
    if app.width < MEDIUM {
        // Focusing the navigation pane is what opens the overlay.
        x = button(app, c, 0, 0, "[menu]", Hit::Pane(Pane::Nav), Kind::Head) + 1;
    }
    let tabs: Vec<String> = (0..app.hosts.len()).map(|i| host_tab(app, i)).collect();
    // Width needed to show the tabs from `first` up to the current one.
    let need = |first: usize| tabs[first..=app.current].iter().map(|t| t.len() + 1).sum::<usize>();
    let mut first = 0;
    while first < app.current && x + need(first) > c.w {
        first += 1;
    }
    for (i, tab) in tabs.iter().enumerate().skip(first) {
        let kind = if i == app.current { Kind::Head } else { Kind::Dim };
        // The current tab is cut off rather than left out on a tiny screen.
        let label = if i == app.current { &tab[..tab.len().min(c.w.saturating_sub(x))] } else { tab.as_str() };
        if label.is_empty() || x + label.len() > c.w {
            break;
        }
        x = button(app, c, x, 0, label, Hit::Host(i), kind) + 1;
    }
    let vendor = match app.vendor {
        Some(ipmi::SUPERMICRO_ID) => "Supermicro".to_string(),
        Some(id) => format!("vendor {id}"),
        None => "vendor ?".into(),
    };
    let right = c.w.saturating_sub(vendor.len() + 1);
    if right > x {
        c.text(right, 0, vendor.len(), &vendor, Kind::Dim);
    }
}

fn draw_nav(app: &mut App, c: &mut Canvas, r: Rect) {
    pane(app, c, r, Pane::Nav, "Screens");
    let a = inner(r);
    for (i, item) in NAV.iter().enumerate().take(a.h) {
        let current = *item == app.screen;
        let text = format!("{} {}", if current { ">" } else { " " }, item.title());
        // The highlight follows the keyboard while the pane has the focus, and
        // otherwise marks the screen that is open.
        let selected = if app.focus == Pane::Nav { app.nav_cursor == i } else { current };
        let kind = if selected { Kind::Sel } else { Kind::Normal };
        c.line(a.x, a.y + i, a.w, &text, kind);
        app.hits.push((Rect::new(a.x, a.y + i, a.w, 1), Hit::Nav(i)));
    }
}

/// Draw the emulator cells into `a`. When the area is smaller than the
/// emulated screen, the part around the cursor is shown.
fn draw_console_cells(app: &mut App, c: &mut Canvas, a: Rect) {
    app.view_rows = a.h.max(1);
    let Some(console) = &app.console else {
        let hint = [
            "Not connected.",
            "",
            "Enter or [ Connect ] opens serial over LAN.",
            "It needs unlock. After that, keys go to the host.",
        ];
        for (i, line) in hint.iter().enumerate().take(a.h) {
            c.text(a.x + 1, a.y + i, a.w.saturating_sub(2), line, Kind::Normal);
        }
        if a.h > hint.len() + 1 {
            let kind = connect_kind(app);
            button(app, c, a.x + 1, a.y + hint.len() + 1, "[ Connect (Enter) ]", Hit::Connect, kind);
        }
        return;
    };
    let typing = app.typing();
    let parser = console.screen();
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let (rows, cols) = (usize::from(rows.get()), usize::from(cols.get()));
    let (cur_row, cur_col) = screen.cursor_position();
    let (cur_row, cur_col) = (usize::from(cur_row), usize::from(cur_col));
    // While scrolled back the top is shown, so the oldest line can be reached.
    let top = if screen.scrollback() > 0 { 0 } else { (cur_row + 1).saturating_sub(a.h).min(rows.saturating_sub(a.h)) };
    let left = (cur_col + 1).saturating_sub(a.w).min(cols.saturating_sub(a.w));
    for y in 0..a.h.min(rows) {
        for x in 0..a.w.min(cols) {
            let Some(cell) = screen.cell((top + y) as u16, (left + x) as u16) else { continue };
            let mut ch = cell.contents().chars().next().unwrap_or(' ');
            // The second half of a wide character stays empty.
            if cell.is_wide_continuation() {
                ch = ' ';
            }
            let mut kind = if cell.inverse() {
                Kind::Sel
            } else if cell.bold() {
                Kind::Head
            } else {
                Kind::Normal
            };
            // The cursor is drawn only where typing would go.
            if typing && screen.scrollback() == 0 && !screen.hide_cursor() && (top + y, left + x) == (cur_row, cur_col) {
                kind = if kind == Kind::Sel { Kind::Normal } else { Kind::Sel };
            }
            c.term_cell(a.x + x, a.y + y, ch, kind);
        }
    }
}

/// The connect button is highlighted while Enter would press it: in every pane
/// but the navigation.
fn connect_kind(app: &App) -> Kind {
    if app.focus == Pane::Nav { Kind::Head } else { Kind::Sel }
}

/// Short state of the console, for pane titles and the status line.
fn console_state(app: &App) -> String {
    match &app.console {
        None => "not connected".into(),
        Some(console) => {
            let back = console.scrolled();
            if back > 0 {
                format!("SCROLLED {back} back")
            } else if !console.live() {
                "ended".into()
            } else if app.typing() {
                "typing to host".into()
            } else {
                "connected".into()
            }
        }
    }
}

fn draw_console(app: &mut App, c: &mut Canvas, r: Rect) {
    let mut title = format!("Console: {}", console_state(app));
    let a = inner(r);
    let s = app.console_settings;
    if app.console.is_some() && (a.w < usize::from(s.cols.get()) || a.h < usize::from(s.rows.get())) {
        title = format!("{title}, cropped ({} z zooms)", app.prefix_name());
    }
    pane(app, c, r, Pane::Main, &title);
    draw_console_cells(app, c, inner(r));
}

/// The console on the whole terminal. The status line is shown when there is a
/// spare row for it, and otherwise only while a command is awaited.
fn draw_zoom(app: &mut App, c: &mut Canvas) {
    let rows = usize::from(app.console_settings.rows.get());
    let bar = c.h > rows || app.prefix_pending || app.scroll_mode;
    let area = Rect::new(0, 0, c.w, if bar { c.h - 1 } else { c.h });
    app.hits.push((area, Hit::Pane(Pane::Main)));
    draw_console_cells(app, c, area);
    if bar {
        let y = c.h - 1;
        c.line(0, y, c.w, "", Kind::Head);
        let x = button(app, c, 0, y, "[unzoom]", Hit::Command('z'), Kind::Head) + 1;
        let text = format!("{} | {} | {} then ? commands", app.t.label(), console_state(app), app.prefix_name());
        c.text(x, y, c.w.saturating_sub(x), &text, Kind::Head);
    }
}

/// Details of the console: session facts and its buttons.
fn draw_console_details(app: &mut App, c: &mut Canvas, r: Rect) {
    pane(app, c, r, Pane::Details, "Details");
    let a = inner(r);
    let width = a.w.saturating_sub(2);
    let s = app.console_settings;
    let mut lines = vec![
        ("state".to_string(), Kind::Dim),
        (console_state(app), Kind::Normal),
        ("size".to_string(), Kind::Dim),
        (format!("{}x{}", s.cols, s.rows), Kind::Normal),
        ("command key".to_string(), Kind::Dim),
        (app.prefix_name(), Kind::Normal),
        (String::new(), Kind::Normal),
    ];
    if app.console_live() {
        lines.extend(wrap("Keys go to the host while the console pane has the focus.", width).into_iter().map(|l| (l, Kind::Warn)));
    }
    app.detail_rows = a.h.max(1);
    app.detail_len = 0;
    let mut buttons = vec![];
    if app.console_live() {
        buttons.push(("[ Disconnect ]", Hit::Command('.')));
        buttons.push(("[ Zoom ]", Hit::Command('z')));
        buttons.push(("[ Scroll back ]", Hit::Command('[')));
    } else {
        buttons.push(("[ Connect (Enter) ]", Hit::Connect));
        if app.console.as_ref().is_some_and(|c| c.busy_elsewhere()) {
            buttons.push(("[ Take over ]", Hit::Key(KeyCode::Char('t'))));
        }
    }
    for (row, (text, kind)) in lines.iter().take(a.h.saturating_sub(buttons.len())).enumerate() {
        c.text(a.x + 1, a.y + row, width, text, *kind);
    }
    let first = a.h.saturating_sub(buttons.len());
    for (i, (label, hit)) in buttons.into_iter().enumerate().take(a.h) {
        let kind = if hit == Hit::Connect { connect_kind(app) } else { Kind::Head };
        button(app, c, a.x + 1, a.y + first + i, label, hit, kind);
    }
}

fn draw_main(app: &mut App, c: &mut Canvas, r: Rect) {
    let mut title = app.screen.title().to_string();
    if app.filtering {
        title = format!("{title}  /{}_", app.filter);
    } else if !app.filter.is_empty() {
        title = format!("{title}  /{}  (Esc clears)", app.filter);
    }
    pane(app, c, r, Pane::Main, &title);
    let a = inner(r);
    app.view_rows = a.h.max(1);
    app.scroll = app.scroll.min(app.max_scroll());
    let lines = app.lines();
    let selected = app.selected_line();
    // Menu screens mark their selection in `view`, other screens get the
    // highlight here.
    let plain = app.menu().is_empty() && matches!(app.data.get(&app.screen), Some(Ok(_)));
    for (row, (i, (text, kind))) in lines.iter().enumerate().skip(app.scroll).take(a.h).enumerate() {
        let kind = if plain && i == selected && app.focus != Pane::Nav { Kind::Sel } else { *kind };
        // One column of padding on each side of the text.
        c.line(a.x, a.y + row, a.w, &format!(" {text}"), kind);
        app.hits.push((Rect::new(a.x, a.y + row, a.w, 1), Hit::Row(i)));
    }
    // Show that there is more above or below.
    if app.scroll > 0 {
        c.text(r.x + r.w.saturating_sub(8), r.y, 6, " more ", Kind::Dim);
    }
    if app.scroll < app.max_scroll() {
        c.text(r.x + r.w.saturating_sub(8), r.y + r.h - 1, 6, " more ", Kind::Dim);
    }
}

fn draw_details(app: &mut App, c: &mut Canvas, r: Rect) {
    if app.screen == Screen::Console {
        return draw_console_details(app, c, r);
    }
    pane(app, c, r, Pane::Details, "Details");
    let a = inner(r);
    let width = a.w.saturating_sub(2);
    let lines = app.detail_lines(width);
    // The run button stays at the bottom, outside the scrolling text.
    let has_run = !app.menu().is_empty() && a.h > 2;
    app.detail_rows = if has_run { a.h - 2 } else { a.h.max(1) };
    app.detail_len = lines.len();
    app.detail_scroll = app.detail_scroll.min(lines.len().saturating_sub(app.detail_rows));
    for (row, (text, kind)) in lines.iter().skip(app.detail_scroll).take(app.detail_rows).enumerate() {
        c.text(a.x + 1, a.y + row, width, text, *kind);
    }
    if has_run {
        // The button is greyed while locked. It still reacts, with the reason.
        let kind = if app.unlocked { Kind::Sel } else { Kind::Dim };
        button(app, c, a.x + 1, a.y + a.h - 1, "[ Run ]", Hit::Run, kind);
    }
}

/// Status line: lock state first, because it must always be visible.
fn status_line(app: &App, c: &mut Canvas, y: usize) {
    let (lock, kind) = if app.unlocked { (" WRITES UNLOCKED ", Kind::Warn) } else { (" read-only ", Kind::Head) };
    c.text(0, y, c.w, lock, kind);
    let rest = format!("| {} | {}", app.t.label(), app.status);
    let mut x = lock.len();
    // A live console stays writable after a relock, so it is shown separately.
    if app.console_live() {
        let mark = " CONSOLE OPEN ";
        c.text(x, y, c.w.saturating_sub(x), mark, Kind::Sel);
        x += mark.len();
    }
    c.text(x, y, c.w.saturating_sub(x), &rest, if app.unlocked { Kind::Warn } else { Kind::Normal });
}

/// Bottom row: key hints. Each one can be clicked.
fn key_hints(app: &mut App, c: &mut Canvas, y: usize) {
    // While keys go to the host, only commands behind the prefix work.
    if app.screen == Screen::Console && (app.typing() || app.prefix_pending || app.scroll_mode) {
        let lead = if app.scroll_mode {
            "arrows PgUp PgDn Home scroll, other keys return".to_string()
        } else {
            format!("{} then:", app.prefix_name())
        };
        c.text(1, y, c.w.saturating_sub(1), &lead, Kind::Dim);
        let mut x = lead.len() + 3;
        if !app.scroll_mode {
            for (label, cmd) in [(". disconnect", '.'), ("z zoom", 'z'), ("[ scroll", '['), ("Tab pane", '\t'), ("? help", '?')] {
                x = button(app, c, x, y, label, Hit::Command(cmd), Kind::Dim) + 2;
            }
        }
        return;
    }
    let mut hints = vec![];
    if app.filtering {
        hints.push(("Enter keep filter", KeyCode::Enter));
        hints.push(("Esc clear", KeyCode::Esc));
    } else {
        // Help and quit come first: on a narrow terminal the hints at the end
        // are left out, and help lists all keys.
        hints.push(("? help", KeyCode::Char('?')));
        hints.push(("q quit", KeyCode::Char('q')));
        if app.screen == Screen::Console && !app.console_live() {
            hints.push(("Enter connect", KeyCode::Enter));
            hints.push(("S full screen", KeyCode::Char('S')));
        }
        if !app.menu().is_empty() {
            hints.push(("Enter run", KeyCode::Enter));
        }
        hints.push((if app.unlocked { "u lock" } else { "u unlock" }, KeyCode::Char('u')));
        if view::filterable(app.screen) {
            hints.push(("/ filter", KeyCode::Char('/')));
        }
        hints.push(("Tab pane", KeyCode::Tab));
        hints.push(("r refresh", KeyCode::Char('r')));
        hints.push(("d details", KeyCode::Char('d')));
        if app.screen != Screen::Console {
            hints.push(("e export", KeyCode::Char('e')));
        }
    }
    let mut x = 1;
    for (label, code) in hints {
        x = button(app, c, x, y, label, Hit::Key(code), Kind::Dim) + 2;
    }
}

/// Text of the help window.
fn help_text(app: &App) -> Vec<String> {
    let mut v: Vec<String> = [
        "Tab, Shift-Tab  next or previous pane",
        "arrows, j, k    move in the focused pane",
        "PgUp PgDn g G   move by page, to an end",
        "Enter           open, run or connect",
        "1 to 9          switch host (locks)",
        "u               unlock or lock writes",
        "/               filter, Esc clears it",
        "e               export screen to a file",
        "d               show or hide details",
        "r               refresh",
        "S               full screen console",
        "?               this help",
        "q               quit",
        "",
        "Mouse: click screens, hosts, rows and",
        "buttons. The wheel scrolls.",
        "",
    ]
    .map(String::from)
    .to_vec();
    let p = app.prefix_name();
    v.extend([
        "Connected console: keys go to the host.".into(),
        format!("{p} then:"),
        "  .      disconnect".into(),
        "  z      zoom".into(),
        "  [      scroll back".into(),
        "  Tab    next pane".into(),
        "  u d q ? and 1 to 9 as above".into(),
        format!("  {p} send it to the host"),
        "".into(),
        "Writes are recorded in the audit log.".into(),
    ]);
    v
}

fn draw_help(app: &mut App, c: &mut Canvas) {
    let lines = help_text(app);
    let w = 46.min(c.w);
    let h = (lines.len() + 4).min(c.h);
    let r = Rect::new((c.w - w) / 2, (c.h - h) / 2, w, h);
    app.hits.clear();
    c.clear(r);
    c.frame(r);
    c.text(r.x + 2, r.y, w - 4, " Help ", Kind::Sel);
    // Border, text, blank, button, border.
    let rows = h.saturating_sub(4).max(1);
    app.help_scroll = app.help_scroll.min(lines.len().saturating_sub(rows));
    for (i, line) in lines.iter().skip(app.help_scroll).take(rows).enumerate() {
        c.text(r.x + 2, r.y + 1 + i, w - 4, line, Kind::Normal);
    }
    let x = button(app, c, r.x + 2, r.y + h - 2, "[ Close (Esc) ]", Hit::DialogCancel, Kind::Head);
    if rows < lines.len() {
        c.text(x + 2, r.y + h - 2, (r.x + w).saturating_sub(x + 4), "arrows scroll", Kind::Dim);
    }
}

/// Draw the open dialog on top of everything. It also replaces all clickable
/// regions, so nothing behind it can be clicked.
fn dialog(app: &mut App, c: &mut Canvas) {
    if matches!(app.mode, Mode::Help) {
        return draw_help(app, c);
    }
    let (title, text, ok, anchor) = match &app.mode {
        Mode::Normal | Mode::Help => return,
        Mode::Unlock { anchor, connect } => (
            "Unlock",
            vec![
                format!("Unlock writes on {}?", app.t.describe()),
                if *connect {
                    "The console opens and your keys are typed into the host, until you disconnect.".to_string()
                } else {
                    "Power, fan, event log and console input become possible.".to_string()
                },
            ],
            "[ Unlock (y) ]",
            *anchor,
        ),
        Mode::Confirm { act, anchor } => (
            "Confirm",
            vec![format!("Run on {}:", app.t.describe()), act.label.clone(), format!("ipmitool {}", act.args.join(" "))],
            "[ Run (y) ]",
            *anchor,
        ),
    };
    let cancel = "[ Cancel (Esc) ]";
    let w = 60.min(c.w.saturating_sub(2));
    let body: Vec<String> = text.iter().flat_map(|t| wrap(t, w - 4)).collect();
    // Border, blank, text, blank, buttons, border. The height is limited to the
    // screen, and the buttons stay inside it.
    let h = (body.len() + 5).min(c.h);
    let x = (c.w - w) / 2;
    let mut y = (c.h - h) / 2;
    // Move the box if the buttons would land on the row of the pointer. This is
    // what stops a double click from confirming.
    if let Some((_, py)) = anchor
        && py == y + h - 2
    {
        y = if y + h + 1 < c.h { y + 1 } else { y.saturating_sub(1) };
    }
    app.hits.clear();
    let r = Rect::new(x, y, w, h);
    c.clear(r);
    c.frame(r);
    c.text(x + 2, y, w - 4, &format!(" {title} "), Kind::Sel);
    for (i, line) in body.iter().take(h.saturating_sub(4)).enumerate() {
        c.text(x + 2, y + 2 + i, w - 4, line, Kind::Warn);
    }
    let by = y + h - 2;
    let mut bx = button(app, c, x + 2, by, ok, Hit::DialogOk, Kind::Head);
    bx += 3;
    button(app, c, bx, by, cancel, Hit::DialogCancel, Kind::Head);
    // A screen too short to move the box: swap the buttons, so that the accept
    // button is not under the pointer.
    if let Some((px, py)) = anchor
        && app.hits.iter().any(|(r, k)| *k == Hit::DialogOk && r.contains(px, py))
    {
        app.hits.clear();
        c.line(x + 2, by, w - 4, "", Kind::Normal);
        let bx = button(app, c, x + 2, by, cancel, Hit::DialogCancel, Kind::Head) + 3;
        button(app, c, bx, by, ok, Hit::DialogOk, Kind::Head);
    }
}

/// Draw the whole screen. Also stores the sizes and clickable regions that the
/// input handling needs.
pub fn draw(app: &mut App, w: usize, h: usize) -> Canvas {
    let mut c = Canvas::new(w, h);
    app.width = w;
    app.hits.clear();
    if w < MIN_W || h < MIN_H {
        // Nothing is clickable, and an open dialog closes: it can not be shown
        // completely, so it must not be possible to confirm it.
        app.mode = Mode::Normal;
        c.text(0, 0, w, "terminal too small", Kind::Warn);
        c.text(0, 1, w, &format!("need {MIN_W}x{MIN_H}, q quits"), Kind::Normal);
        return c;
    }
    app.fix_focus();
    if app.zoom && app.screen == Screen::Console && app.console.is_some() {
        draw_zoom(app, &mut c);
        dialog(app, &mut c);
        return c;
    }
    let l = layout(w, h, app.details_visible(), app.focus == Pane::Nav);
    host_bar(app, &mut c);
    // Right to left, so the shared border column belongs to the pane on the
    // right of it, and the navigation overlay is drawn last.
    if let Some(r) = l.main {
        if app.screen == Screen::Console {
            draw_console(app, &mut c, r);
        } else {
            draw_main(app, &mut c, r);
        }
    }
    if let Some(r) = l.details {
        draw_details(app, &mut c, r);
    }
    if let Some(r) = l.nav {
        // Under an open overlay nothing but the panes themselves reacts: a
        // click beside the overlay closes it and can not hit a menu item.
        if w < MEDIUM {
            app.hits.retain(|(_, h)| matches!(h, Hit::Pane(_)));
        }
        draw_nav(app, &mut c, r);
    }
    status_line(app, &mut c, h - 2);
    key_hints(app, &mut c, h - 1);
    dialog(app, &mut c);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_rules() {
        let l = layout(120, 28, true, false);
        let (n, m, d) = (l.nav.unwrap(), l.main.unwrap(), l.details.unwrap());
        assert_eq!((inner(n).w, inner(m).w, inner(d).w), (14, 80, 22));
        assert_eq!(d.x + d.w, 120);

        let l = layout(97, 28, false, false);
        assert_eq!(inner(l.main.unwrap()).w, 80);
        assert!(l.nav.is_some() && l.details.is_none());

        let l = layout(80, 24, false, false);
        assert!(l.nav.is_none() && l.details.is_none());
        assert_eq!(l.main.unwrap().w, 80);
        let l = layout(80, 24, true, true);
        assert!(l.nav.is_some() && l.main.is_none() && l.details.is_some());
    }
}
