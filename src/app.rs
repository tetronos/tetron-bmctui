//! Application state and input handling. Nothing here touches the terminal.
//! Keyboard and mouse both end up in the same functions (`activate_nav`,
//! `pick`, `toggle_lock`, ...), which is what keeps the two equal: a click can
//! not do anything a key can not do, and the reverse.
//!
//! Actions are not run here either. They are put in `queue`, and the main loop
//! runs them. Tests can therefore check what would run without running it.

use crate::canvas::Rect;
use crate::ipmi::{self, Action, Risk, Screen, Target};
use crate::view;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// Rows moved by one mouse wheel step.
const WHEEL_STEP: usize = 3;

/// Entries of the navigation pane, in display order. The console is listed
/// first. It is greyed out until the console pane is built.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavItem {
    Console,
    Page(Screen),
}

pub const NAV: [NavItem; 7] = [
    NavItem::Console,
    NavItem::Page(Screen::Dash),
    NavItem::Page(Screen::Sensors),
    NavItem::Page(Screen::Sel),
    NavItem::Page(Screen::Fru),
    NavItem::Page(Screen::Control),
    NavItem::Page(Screen::Fan),
];

const CONSOLE_REASON: &str = "console pane is not built yet. S opens ipmitool serial over LAN full screen.";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pane {
    Nav,
    Main,
    Details,
}

/// What a click on a region of the screen means. The UI records one `Hit` per
/// clickable region each time it draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    /// Empty space of a pane: focuses it, and the wheel scrolls it.
    Pane(Pane),
    /// A navigation entry, by index in `NAV`.
    Nav(usize),
    /// A line of the main pane, by line number in the rendered screen.
    Row(usize),
    /// The run button in the details pane.
    Run,
    /// A key hint or button that stands for a key press.
    Key(KeyCode),
    /// The accept button of the open dialog.
    DialogOk,
    /// The cancel button of the open dialog.
    DialogCancel,
}

/// Dialog state. `anchor` is the pointer position when the dialog opened. The
/// UI uses it to keep the accept button away from the pointer, so that a second
/// click in the same spot can not confirm.
#[derive(Clone, Debug)]
pub enum Mode {
    Normal,
    /// "Unlock writes on HOST?"
    Unlock { anchor: Option<(usize, usize)> },
    /// "Run ACTION on HOST?" for sensitive actions.
    Confirm { act: Action, anchor: Option<(usize, usize)> },
}

/// Results coming back from worker threads.
pub enum Msg {
    Data(Screen, Result<String, String>),
    Done(String, Result<String, String>),
    Vendor(Option<u32>),
}

pub struct App {
    pub t: Target,
    pub screen: Screen,
    pub data: HashMap<Screen, Result<String, String>>,
    pub loading: HashSet<Screen>,
    last: HashMap<Screen, Instant>,
    /// First visible line of the main pane and of the details pane.
    pub scroll: usize,
    pub detail_scroll: usize,
    /// Selected row of the main pane. On screens with a menu this is the index
    /// of the action, elsewhere it is the line number.
    pub cursor: usize,
    /// Highlighted entry of the navigation pane (not yet opened).
    pub nav_cursor: usize,
    pub focus: Pane,
    /// `None` follows the width rule, `Some` is the choice made with `d`.
    pub details: Option<bool>,
    pub unlocked: bool,
    /// Idle time after which writes lock again. `None` never relocks.
    relock: Option<Duration>,
    last_input: Instant,
    pub mode: Mode,
    pub status: String,
    pub vendor: Option<u32>,
    vendor_pending: bool,
    /// Result of the latest fetch: `None` until the first one returns.
    pub session_ok: Option<bool>,
    halt_auto: bool,
    pub want_sol: bool,
    pub quit: bool,
    /// Confirmed or harmless actions waiting to be run by the main loop.
    pub queue: Vec<Action>,
    /// Clickable regions from the latest draw. Later entries lie on top.
    pub hits: Vec<(Rect, Hit)>,
    /// Latest mouse position.
    pointer: Option<(usize, usize)>,
    /// Facts about the latest draw that input handling depends on.
    pub view_rows: usize,
    pub detail_rows: usize,
    pub detail_len: usize,
    pub width: usize,
    tx: Sender<Msg>,
}

impl App {
    pub fn new(t: Target, relock: Option<Duration>, tx: Sender<Msg>) -> App {
        App {
            t,
            screen: Screen::Dash,
            data: HashMap::new(),
            loading: HashSet::new(),
            last: HashMap::new(),
            scroll: 0,
            detail_scroll: 0,
            cursor: 0,
            nav_cursor: 1,
            focus: Pane::Main,
            details: None,
            unlocked: false,
            relock,
            last_input: Instant::now(),
            mode: Mode::Normal,
            status: "u unlocks writes".into(),
            vendor: None,
            vendor_pending: false,
            session_ok: None,
            halt_auto: false,
            want_sol: false,
            quit: false,
            queue: vec![],
            hits: vec![],
            pointer: None,
            view_rows: 1,
            detail_rows: 1,
            detail_len: 0,
            width: 80,
            tx,
        }
    }

    // ----- fetching ---------------------------------------------------------

    /// Fetch a screen in a worker thread. At most one fetch per screen runs.
    pub fn refresh(&mut self, s: Screen) {
        if !self.loading.insert(s) {
            return;
        }
        self.last.insert(s, Instant::now());
        let (t, tx) = (self.t.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Data(s, ipmi::fetch(&t, s)));
        });
    }

    /// Ask the BMC for its manufacturer id. The fan screen depends on it.
    pub fn start_vendor(&mut self) {
        if self.vendor_pending {
            return;
        }
        self.vendor_pending = true;
        let (t, tx) = (self.t.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Vendor(ipmi::fetch_vendor(&t)));
        });
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Data(s, r) => {
                self.loading.remove(&s);
                self.session_ok = Some(r.is_ok());
                if let Err(e) = &r
                    && ipmi::looks_like_auth_error(e)
                {
                    self.halt_auto = true;
                    self.status = "connection or login failed: auto refresh stopped (wrong password can lock the BMC account). r retries once.".into();
                }
                self.data.insert(s, r);
            }
            Msg::Vendor(v) => {
                self.vendor_pending = false;
                self.vendor = v;
            }
            Msg::Done(label, r) => {
                self.status = match r {
                    Ok(o) => format!("{label}: ok {}", o.trim().lines().next().unwrap_or("")),
                    Err(e) => format!("{label}: FAILED {e}"),
                };
                // A write can change these, so cached copies are dropped.
                for s in [Screen::Control, Screen::Fan, Screen::Dash, Screen::Sel] {
                    self.data.remove(&s);
                }
                let s = self.screen;
                self.refresh(s);
            }
        }
    }

    /// Periodic work: idle relock and auto refresh of the live screens.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if let Some(limit) = self.relock
            && self.unlocked
            && now.saturating_duration_since(self.last_input) >= limit
        {
            self.lock(&format!("locked after {} min without input", limit.as_secs() / 60));
            changed = true;
        }
        if !self.halt_auto && matches!(self.screen, Screen::Dash | Screen::Sensors) {
            let s = self.screen;
            if self.last.get(&s).is_none_or(|t| now.saturating_duration_since(*t) > Duration::from_secs(10)) {
                self.refresh(s);
                changed = true;
            }
        }
        changed
    }

    /// Restart the idle timer. Also called after the full screen console, so
    /// that time spent watching it does not count as idle.
    pub fn touch(&mut self) {
        self.last_input = Instant::now();
    }

    // ----- screen content ---------------------------------------------------

    fn ctx(&self) -> view::Ctx<'_> {
        view::Ctx { data: self.data.get(&self.screen), vendor: self.vendor, cursor: self.cursor, loading: self.loading.contains(&self.screen) }
    }

    pub fn lines(&self) -> Vec<view::Line> {
        view::render(self.screen, &self.ctx())
    }

    pub fn detail_lines(&self, width: usize) -> Vec<view::Line> {
        view::details(self.screen, &self.ctx(), width)
    }

    pub fn menu(&self) -> Vec<Action> {
        view::actions(self.screen, self.vendor)
    }

    /// Line number of the selected row in the main pane.
    pub fn selected_line(&self) -> usize {
        if self.menu().is_empty() { self.cursor } else { view::MENU_FIRST_LINE + self.cursor }
    }

    /// First line that can be selected. The sensor table starts with a header.
    pub fn first_row(&self) -> usize {
        if self.screen == Screen::Sensors && matches!(self.data.get(&self.screen), Some(Ok(_))) { 1 } else { 0 }
    }

    pub fn max_scroll(&self) -> usize {
        self.lines().len().saturating_sub(self.view_rows)
    }

    /// Scroll just far enough to make the selected row visible.
    fn follow_cursor(&mut self) {
        let line = self.selected_line();
        if line < self.scroll {
            self.scroll = line;
        } else if line >= self.scroll + self.view_rows {
            self.scroll = line + 1 - self.view_rows;
        }
        // Keep headers above the first row on screen.
        if line <= self.first_row() {
            self.scroll = 0;
        }
    }

    /// Move the selection of the main pane by `delta` rows.
    fn move_cursor(&mut self, delta: isize) {
        let items = self.menu().len();
        let (low, high) = if items > 0 { (0, items - 1) } else { (self.first_row(), self.lines().len().saturating_sub(1)) };
        self.cursor = self.cursor.saturating_add_signed(delta).clamp(low, high.max(low));
        self.detail_scroll = 0;
        self.follow_cursor();
    }

    // ----- panes ------------------------------------------------------------

    /// Details are shown by default only on wide terminals.
    pub fn details_visible(&self) -> bool {
        self.details.unwrap_or(self.width >= crate::ui::WIDE)
    }

    /// Panes that Tab visits. On a narrow terminal the navigation is an overlay
    /// that is open while it has the focus, and details replace the main pane.
    fn pane_cycle(&self) -> Vec<Pane> {
        let mut v = vec![Pane::Nav];
        let narrow = self.width < crate::ui::MEDIUM;
        if !(narrow && self.details_visible()) {
            v.push(Pane::Main);
        }
        if self.details_visible() {
            v.push(Pane::Details);
        }
        v
    }

    pub fn focus_pane(&mut self, p: Pane) {
        if p == Pane::Nav && self.focus != Pane::Nav {
            self.nav_cursor = NAV.iter().position(|n| *n == NavItem::Page(self.screen)).unwrap_or(1);
        }
        self.focus = p;
    }

    fn cycle_focus(&mut self, delta: isize) {
        let panes = self.pane_cycle();
        let n = panes.len() as isize;
        let i = panes.iter().position(|p| *p == self.focus).unwrap_or(0) as isize;
        self.focus_pane(panes[(i + delta).rem_euclid(n) as usize]);
    }

    /// Keep the focus on a pane that exists, for example after a resize.
    pub fn fix_focus(&mut self) {
        let panes = self.pane_cycle();
        if !panes.contains(&self.focus) {
            self.focus = panes[panes.len() - 1];
        }
    }

    fn toggle_details(&mut self) {
        let show = !self.details_visible();
        self.details = Some(show);
        // On a narrow terminal the new pane covers the old one: move there.
        if self.width < crate::ui::MEDIUM {
            self.focus = if show { Pane::Details } else { Pane::Main };
        }
        self.fix_focus();
    }

    pub fn switch(&mut self, s: Screen) {
        self.screen = s;
        self.scroll = 0;
        self.detail_scroll = 0;
        self.cursor = 0;
        self.cursor = if self.menu().is_empty() { self.first_row() } else { 0 };
        let stale = self.last.get(&s).is_none_or(|t| t.elapsed() > Duration::from_secs(5));
        if stale && !self.halt_auto {
            self.refresh(s);
        }
    }

    /// Open a navigation entry (Enter in the navigation pane, or a click).
    fn activate_nav(&mut self, i: usize) {
        let Some(item) = NAV.get(i) else { return };
        self.nav_cursor = i;
        match *item {
            NavItem::Console => self.status = CONSOLE_REASON.into(),
            NavItem::Page(s) => {
                self.switch(s);
                self.focus = if self.pane_cycle().contains(&Pane::Main) { Pane::Main } else { Pane::Details };
            }
        }
    }

    // ----- writes -----------------------------------------------------------

    fn lock(&mut self, why: &str) {
        self.unlocked = false;
        // An open confirmation belongs to the unlocked state, so it closes too.
        self.mode = Mode::Normal;
        self.status = why.into();
    }

    fn toggle_lock(&mut self) {
        if self.unlocked {
            self.lock("locked: read-only");
        } else {
            self.mode = Mode::Unlock { anchor: self.pointer };
        }
    }

    /// Run the selected menu item. Harmless actions run at once, sensitive ones
    /// open a confirmation dialog.
    pub fn pick(&mut self) {
        let items = self.menu();
        let Some(a) = items.get(self.cursor).cloned() else { return };
        if !self.unlocked {
            self.status = "locked: press u to unlock writes".into();
            return;
        }
        match a.risk {
            Risk::Low => self.queue.push(a),
            Risk::High => self.mode = Mode::Confirm { act: a, anchor: self.pointer },
        }
    }

    fn open_sol(&mut self) {
        if self.unlocked {
            self.want_sol = true;
        } else {
            self.status = "locked: press u to unlock, the console can type into the host".into();
        }
    }

    fn dialog_ok(&mut self) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Unlock { .. } => {
                self.unlocked = true;
                self.status = "writes unlocked".into();
            }
            Mode::Confirm { act, .. } => {
                if self.unlocked {
                    self.queue.push(act);
                }
            }
            Mode::Normal => {}
        }
    }

    fn dialog_cancel(&mut self) {
        self.mode = Mode::Normal;
        self.status = "cancelled, nothing was run".into();
    }

    // ----- keyboard ---------------------------------------------------------

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.touch();
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        // A dialog takes all other keys. Enter alone deliberately does nothing.
        if !matches!(self.mode, Mode::Normal) {
            match code {
                KeyCode::Char('y' | 'Y') if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => self.dialog_ok(),
                KeyCode::Esc => self.dialog_cancel(),
                _ => {}
            }
            return;
        }
        let page = self.view_rows.max(2) as isize - 1;
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Char('d') => self.toggle_details(),
            KeyCode::Char('u') => self.toggle_lock(),
            KeyCode::Char('S') => self.open_sol(),
            KeyCode::Char('r') => {
                self.halt_auto = false;
                if self.vendor.is_none() {
                    self.start_vendor();
                }
                let s = self.screen;
                self.refresh(s);
            }
            // Keys 1 to 9 select hosts. There is one host until the host list
            // is built.
            KeyCode::Char('1'..='9') => self.status = "only one host: the host list is not built yet".into(),
            KeyCode::Esc => self.focus = self.pane_cycle().into_iter().find(|p| *p != Pane::Nav).unwrap_or(Pane::Main),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::PageDown => self.step(page),
            KeyCode::PageUp => self.step(-page),
            KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX / 2),
            KeyCode::Enter => match self.focus {
                Pane::Nav => self.activate_nav(self.nav_cursor),
                Pane::Main | Pane::Details => self.pick(),
            },
            _ => {}
        }
    }

    /// Up and down act on whichever pane has the focus.
    fn step(&mut self, delta: isize) {
        match self.focus {
            Pane::Nav => self.nav_cursor = self.nav_cursor.saturating_add_signed(delta).min(NAV.len() - 1),
            Pane::Main => self.move_cursor(delta),
            Pane::Details => self.scroll_details(delta),
        }
    }

    fn scroll_details(&mut self, delta: isize) {
        let max = self.detail_len.saturating_sub(self.detail_rows);
        self.detail_scroll = self.detail_scroll.saturating_add_signed(delta).min(max);
    }

    // ----- mouse ------------------------------------------------------------

    /// Topmost clickable region under a cell.
    fn hit_at(&self, x: usize, y: usize) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    /// Handle a mouse event. Returns true when the screen has to be redrawn,
    /// which plain pointer movement does not require.
    pub fn mouse(&mut self, m: MouseEvent) -> bool {
        let (x, y) = (m.column as usize, m.row as usize);
        self.pointer = Some((x, y));
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.touch();
                if let Some(h) = self.hit_at(x, y) {
                    self.click(h);
                }
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.touch();
                let down = m.kind == MouseEventKind::ScrollDown;
                self.wheel(x, y, if down { WHEEL_STEP as isize } else { -(WHEEL_STEP as isize) });
                true
            }
            _ => false,
        }
    }

    fn click(&mut self, h: Hit) {
        // With a dialog open only its two buttons react.
        if !matches!(self.mode, Mode::Normal) {
            match h {
                Hit::DialogOk => self.dialog_ok(),
                Hit::DialogCancel => self.dialog_cancel(),
                _ => {}
            }
            return;
        }
        match h {
            Hit::Pane(p) => self.focus_pane(p),
            Hit::Nav(i) => self.activate_nav(i),
            Hit::Row(line) => {
                self.focus = Pane::Main;
                self.detail_scroll = 0;
                if self.menu().is_empty() {
                    if line >= self.first_row() {
                        self.cursor = line;
                    }
                } else if let Some(i) = line.checked_sub(view::MENU_FIRST_LINE)
                    && i < self.menu().len()
                {
                    // Clicking a menu item is the same as selecting it and
                    // pressing Enter.
                    self.cursor = i;
                    self.pick();
                }
            }
            Hit::Run => self.pick(),
            Hit::Key(k) => self.key(k, KeyModifiers::NONE),
            Hit::DialogOk | Hit::DialogCancel => {}
        }
    }

    /// The wheel scrolls the pane under the pointer, without moving the
    /// selection or the focus.
    fn wheel(&mut self, x: usize, y: usize, delta: isize) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let pane = self.hits.iter().rev().find_map(|(r, h)| match h {
            Hit::Pane(p) if r.contains(x, y) => Some(*p),
            _ => None,
        });
        match pane {
            Some(Pane::Main) => self.scroll = self.scroll.saturating_add_signed(delta).min(self.max_scroll()),
            Some(Pane::Details) => self.scroll_details(delta),
            // The navigation list is short and does not scroll.
            Some(Pane::Nav) | None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui;
    use std::sync::mpsc;

    /// An app whose ipmitool does not exist: fetches fail harmlessly, and
    /// actions stay in the queue because no main loop runs them.
    fn app(w: usize, h: usize) -> App {
        let t = Target { bin: "/nonexistent/ipmitool".into(), host: Some("testbox".into()), user: None, iface: "lanplus".into(), pass_file: None };
        let (tx, rx) = mpsc::channel();
        std::mem::forget(rx);
        let mut a = App::new(t, Some(Duration::from_secs(900)), tx);
        ui::draw(&mut a, w, h);
        a
    }

    fn press(a: &mut App, c: KeyCode) {
        a.key(c, KeyModifiers::NONE);
        ui::draw(a, 120, 28);
    }

    fn click_at(a: &mut App, x: usize, y: usize) {
        a.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: x as u16, row: y as u16, modifiers: KeyModifiers::NONE });
        ui::draw(a, 120, 28);
    }

    /// Click the middle of the region that carries `h`.
    fn click(a: &mut App, h: Hit) {
        let r = a.hits.iter().rev().find(|(_, x)| *x == h).unwrap_or_else(|| panic!("no region for {h:?}")).0;
        click_at(a, r.x + r.w / 2, r.y);
    }

    fn unlock(a: &mut App) {
        press(a, KeyCode::Char('u'));
        press(a, KeyCode::Char('y'));
        assert!(a.unlocked);
    }

    fn open_power(a: &mut App) {
        let i = NAV.iter().position(|n| *n == NavItem::Page(Screen::Control)).unwrap();
        click(a, Hit::Nav(i));
        assert_eq!(a.screen, Screen::Control);
    }

    #[test]
    fn unlock_needs_y_and_enter_does_nothing() {
        let mut a = app(120, 28);
        press(&mut a, KeyCode::Char('u'));
        press(&mut a, KeyCode::Enter);
        assert!(!a.unlocked);
        press(&mut a, KeyCode::Char('y'));
        assert!(a.unlocked);
        press(&mut a, KeyCode::Char('u'));
        assert!(!a.unlocked, "u locks again without asking");
    }

    #[test]
    fn locked_runs_nothing() {
        let mut a = app(120, 28);
        open_power(&mut a);
        press(&mut a, KeyCode::Enter);
        click(&mut a, Hit::Run);
        press(&mut a, KeyCode::Char('S'));
        assert!(a.queue.is_empty() && !a.want_sol);
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn harmless_runs_at_once() {
        let mut a = app(120, 28);
        open_power(&mut a);
        unlock(&mut a);
        press(&mut a, KeyCode::Enter); // first item: Power on
        assert_eq!(a.queue.len(), 1);
        assert_eq!(a.queue[0].label, "Power on");
    }

    #[test]
    fn sensitive_needs_the_dialog() {
        let mut a = app(120, 28);
        open_power(&mut a);
        unlock(&mut a);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Down); // Power off (hard)
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Enter);
        assert!(a.queue.is_empty(), "Enter alone must not confirm");
        press(&mut a, KeyCode::Esc);
        assert!(a.queue.is_empty() && matches!(a.mode, Mode::Normal));
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(a.queue.len(), 1);
        assert_eq!(a.queue[0].args, ["chassis", "power", "off"]);
    }

    #[test]
    fn double_click_can_not_confirm() {
        for (w, h) in [(120, 28), (80, 24), (60, 12)] {
            let mut a = app(w, h);
            a.switch(Screen::Control);
            a.unlocked = true;
            // Try a pointer on every cell: the accept button must never be
            // drawn under the click that opened the dialog.
            for y in 0..h {
                for x in 0..w {
                    a.mode = Mode::Confirm { act: ipmi::control_actions()[2].clone(), anchor: Some((x, y)) };
                    ui::draw(&mut a, w, h);
                    assert!(a.hits.iter().any(|(_, k)| *k == Hit::DialogOk), "button missing at {w}x{h}");
                    assert_ne!(a.hit_at(x, y), Some(Hit::DialogOk), "{w}x{h} pointer {x},{y}");
                }
            }
        }
    }

    #[test]
    fn mouse_can_do_the_whole_sequence() {
        let mut a = app(120, 28);
        open_power(&mut a);
        click(&mut a, Hit::Key(KeyCode::Char('u')));
        click(&mut a, Hit::DialogOk);
        assert!(a.unlocked);
        click(&mut a, Hit::Row(view::MENU_FIRST_LINE + 2));
        assert!(a.queue.is_empty());
        // Clicks outside the dialog are ignored.
        click_at(&mut a, 2, 3);
        assert!(matches!(a.mode, Mode::Confirm { .. }));
        click(&mut a, Hit::DialogOk);
        assert_eq!(a.queue[0].label, "Power off (hard)");
    }

    #[test]
    fn idle_relocks_and_closes_the_dialog() {
        let mut a = app(120, 28);
        open_power(&mut a);
        unlock(&mut a);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        a.tick(Instant::now() + Duration::from_secs(60));
        assert!(a.unlocked);
        a.tick(Instant::now() + Duration::from_secs(901));
        assert!(!a.unlocked && matches!(a.mode, Mode::Normal));
        press(&mut a, KeyCode::Char('y'));
        assert!(a.queue.is_empty());
    }

    #[test]
    fn tab_and_enter_choose_screens() {
        let mut a = app(120, 28);
        press(&mut a, KeyCode::BackTab);
        assert_eq!(a.focus, Pane::Nav);
        press(&mut a, KeyCode::Down);
        assert_eq!(a.screen, Screen::Dash, "moving does not fetch");
        press(&mut a, KeyCode::Enter);
        assert_eq!((a.screen, a.focus), (Screen::Sensors, Pane::Main));
    }

    #[test]
    fn console_is_greyed_with_a_reason() {
        let mut a = app(120, 28);
        click(&mut a, Hit::Nav(0));
        assert_eq!(a.screen, Screen::Dash);
        assert!(a.status.contains("not built"));
    }

    #[test]
    fn wheel_scrolls_without_moving_the_selection() {
        let mut a = app(120, 28);
        a.switch(Screen::Fru);
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        a.data.insert(Screen::Fru, Ok(text));
        ui::draw(&mut a, 120, 28);
        for _ in 0..100 {
            a.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 40, row: 10, modifiers: KeyModifiers::NONE });
        }
        assert_eq!((a.scroll, a.cursor), (a.max_scroll(), 0));
        assert!(a.scroll > 0);
        a.mouse(MouseEvent { kind: MouseEventKind::ScrollUp, column: 40, row: 10, modifiers: KeyModifiers::NONE });
        assert_eq!(a.scroll, a.max_scroll() - WHEEL_STEP);
        // A key press brings the view back to the selection.
        ui::draw(&mut a, 120, 28);
        a.key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!((a.scroll, a.cursor), (1, 1));
    }
}
