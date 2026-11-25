//! Layout and drawing. `draw` turns the app state into a `Canvas` and records
//! the clickable regions in `app.hits`, so what is drawn and what can be
//! clicked come from the same code and can not drift apart.
//!
//! Width rules:
//!   120 columns or more   navigation, main, details
//!   97 to 119             navigation, main; details on request
//!   below 97              one pane; navigation opens as an overlay

use crate::app::{App, Hit, Mode, NAV, NavItem, Pane};
use crate::canvas::{Canvas, Rect};
use crate::ipmi;
use crate::view::{Kind, wrap};
use crossterm::event::KeyCode;

pub const WIDE: usize = 120;
pub const MEDIUM: usize = 97;
/// Inner widths of the side panes. With these, an 80 column main pane (the
/// future console) fits at 97 columns, and with details at 120.
const NAV_W: usize = 14;
const DETAILS_W: usize = 22;
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

fn nav_title(item: NavItem) -> &'static str {
    match item {
        NavItem::Console => "Console",
        NavItem::Page(s) => s.title(),
    }
}

/// Top row: the host tab. The host list will add more tabs here.
fn host_bar(app: &mut App, c: &mut Canvas) {
    let mut x = 0;
    if app.width < MEDIUM {
        // Focusing the navigation pane is what opens the overlay.
        x = button(app, c, 0, 0, "[menu]", Hit::Pane(Pane::Nav), Kind::Head);
    }
    let state = match app.session_ok {
        Some(true) => "ok",
        Some(false) => "error",
        None => "unknown",
    };
    let tab = format!(" [1 {} {state}] ", app.t.label());
    c.text(x, 0, c.w, &tab, Kind::Head);
    let vendor = match app.vendor {
        Some(ipmi::SUPERMICRO_ID) => "Supermicro".to_string(),
        Some(id) => format!("vendor {id}"),
        None => "vendor ?".into(),
    };
    let right = c.w.saturating_sub(vendor.len() + 1);
    if right > x + tab.len() {
        c.text(right, 0, vendor.len(), &vendor, Kind::Dim);
    }
}

fn draw_nav(app: &mut App, c: &mut Canvas, r: Rect) {
    pane(app, c, r, Pane::Nav, "Screens");
    let a = inner(r);
    for (i, item) in NAV.iter().enumerate().take(a.h) {
        let current = *item == NavItem::Page(app.screen);
        let text = format!("{} {}", if current { ">" } else { " " }, nav_title(*item));
        // The highlight follows the keyboard while the pane has the focus, and
        // otherwise marks the screen that is open.
        let selected = if app.focus == Pane::Nav { app.nav_cursor == i } else { current };
        let kind = if selected {
            Kind::Sel
        } else if *item == NavItem::Console {
            Kind::Dim
        } else {
            Kind::Normal
        };
        c.line(a.x, a.y + i, a.w, &text, kind);
        app.hits.push((Rect::new(a.x, a.y + i, a.w, 1), Hit::Nav(i)));
    }
}

fn draw_main(app: &mut App, c: &mut Canvas, r: Rect) {
    let title = app.screen.title();
    pane(app, c, r, Pane::Main, title);
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
    c.text(lock.len(), y, c.w, &rest, if app.unlocked { Kind::Warn } else { Kind::Normal });
}

/// Bottom row: key hints. Each one can be clicked.
fn key_hints(app: &mut App, c: &mut Canvas, y: usize) {
    let mut hints = vec![("Tab pane", KeyCode::Tab)];
    if !app.menu().is_empty() {
        hints.push(("Enter run", KeyCode::Enter));
    }
    hints.push((if app.unlocked { "u lock" } else { "u unlock" }, KeyCode::Char('u')));
    hints.push(("r refresh", KeyCode::Char('r')));
    hints.push(("d details", KeyCode::Char('d')));
    hints.push(("S console", KeyCode::Char('S')));
    hints.push(("q quit", KeyCode::Char('q')));
    let mut x = 1;
    for (label, code) in hints {
        x = button(app, c, x, y, label, Hit::Key(code), Kind::Dim) + 2;
    }
}

/// Draw the open dialog on top of everything. It also replaces all clickable
/// regions, so nothing behind it can be clicked.
fn dialog(app: &mut App, c: &mut Canvas) {
    let (title, text, ok, anchor) = match &app.mode {
        Mode::Normal => return,
        Mode::Unlock { anchor } => (
            "Unlock",
            vec![format!("Unlock writes on {}?", app.t.label()), "Power, fan, event log and console input become possible.".to_string()],
            "[ Unlock (y) ]",
            *anchor,
        ),
        Mode::Confirm { act, anchor } => (
            "Confirm",
            vec![format!("Run on {}:", app.t.label()), act.label.clone(), format!("ipmitool {}", act.args.join(" "))],
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
    let l = layout(w, h, app.details_visible(), app.focus == Pane::Nav);
    host_bar(app, &mut c);
    // Right to left, so the shared border column belongs to the pane on the
    // right of it, and the navigation overlay is drawn last.
    if let Some(r) = l.main {
        draw_main(app, &mut c, r);
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
