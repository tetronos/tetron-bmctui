//! Application state and input handling. Nothing here touches the terminal.
//! Keyboard and mouse both end up in the same functions (`activate_nav`,
//! `pick`, `toggle_lock`, ...), which is what keeps the two equal: a click can
//! not do anything a key can not do, and the reverse.
//!
//! Actions are not run here either. They are put in `queue`, and the main loop
//! runs them. Tests can therefore check what would run without running it.

use crate::canvas::Rect;
use crate::console::{self, Console};
use crate::ipmi::{self, Action, Risk, Screen, Target};
use crate::view;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// Rows moved by one mouse wheel step.
const WHEEL_STEP: usize = 3;

/// Entries of the navigation pane, in display order. The console is first.
pub const NAV: [Screen; 8] = [
    Screen::Console,
    Screen::Dash,
    Screen::Sensors,
    Screen::Sel,
    Screen::Fru,
    Screen::Control,
    Screen::Fan,
    Screen::Hosts,
];

/// Default console command key: Ctrl-], as in telnet.
pub const DEFAULT_PREFIX: u8 = 0x1d;

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
    /// A host tab in the top bar, by index in `App::hosts`.
    Host(usize),
    /// Console buttons: connect, and the commands that otherwise follow the
    /// prefix key.
    Connect,
    Command(char),
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
    /// "Unlock writes on HOST?" With `connect` the console is opened next.
    Unlock { anchor: Option<(usize, usize)>, connect: bool },
    /// "Run ACTION on HOST?" for sensitive actions.
    Confirm { act: Action, anchor: Option<(usize, usize)> },
}

/// Results coming back from worker threads. Each carries the `epoch` it was
/// started in. The epoch changes on every host switch, so a late reply from
/// the previous host can not be shown as data of the new one.
pub enum Msg {
    Data(u64, Screen, Result<String, String>),
    /// Also carries the host name, to say where a late result belongs.
    Done(u64, String, String, Result<String, String>),
    Vendor(u64, Option<u32>),
}

/// What is remembered about a host while another one is in use.
#[derive(Clone, Copy, Default)]
pub struct HostState {
    /// Result of the latest fetch: `None` until the first one returns.
    pub session_ok: Option<bool>,
    /// Auto refresh is stopped after a login failure. This is per host, so that
    /// switching away and back does not start retrying a wrong password.
    halt_auto: bool,
}

pub struct App {
    /// The host in use. Always a copy of `hosts[current]`.
    pub t: Target,
    pub hosts: Vec<Target>,
    pub current: usize,
    pub states: Vec<HostState>,
    pub epoch: u64,
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
    pub want_sol: bool,
    /// The session in the Console pane, live or ended.
    pub console: Option<Console>,
    /// The main loop starts a session when set.
    pub want_connect: bool,
    /// Sessions to be ended by the main loop, which does not block on them.
    pub closing: Vec<Console>,
    pub console_settings: console::Settings,
    /// Byte of the console command key.
    pub prefix: u8,
    /// The prefix was pressed: the next key is a console command.
    pub prefix_pending: bool,
    /// Keys move through the scrollback instead of going to the host.
    pub scroll_mode: bool,
    /// The console takes the whole terminal.
    pub zoom: bool,
    was_live: bool,
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
    /// `hosts` must not be empty and `current` must be an index into it.
    pub fn new(hosts: Vec<Target>, current: usize, relock: Option<Duration>, tx: Sender<Msg>) -> App {
        App {
            t: hosts[current].clone(),
            states: vec![HostState::default(); hosts.len()],
            hosts,
            current,
            epoch: 0,
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
            want_sol: false,
            console: None,
            want_connect: false,
            closing: vec![],
            console_settings: console::Settings::default(),
            prefix: DEFAULT_PREFIX,
            prefix_pending: false,
            scroll_mode: false,
            zoom: false,
            was_live: false,
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
        if matches!(s, Screen::Hosts | Screen::Console) || !self.loading.insert(s) {
            return;
        }
        self.last.insert(s, Instant::now());
        let (t, tx, epoch) = (self.t.clone(), self.tx.clone(), self.epoch);
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Data(epoch, s, ipmi::fetch(&t, s)));
        });
    }

    /// Ask the BMC for its manufacturer id. The fan screen depends on it.
    pub fn start_vendor(&mut self) {
        if self.vendor_pending {
            return;
        }
        self.vendor_pending = true;
        let (t, tx, epoch) = (self.t.clone(), self.tx.clone(), self.epoch);
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Vendor(epoch, ipmi::fetch_vendor(&t)));
        });
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            // Replies for a host that is no longer in use are dropped.
            Msg::Data(e, ..) | Msg::Vendor(e, _) if e != self.epoch => {}
            Msg::Data(_, s, r) => {
                self.loading.remove(&s);
                self.states[self.current].session_ok = Some(r.is_ok());
                if let Err(e) = &r
                    && ipmi::looks_like_auth_error(e)
                {
                    self.states[self.current].halt_auto = true;
                    self.status = "connection or login failed: auto refresh stopped (wrong password can lock the BMC account). r retries once.".into();
                }
                self.data.insert(s, r);
            }
            Msg::Vendor(_, v) => {
                self.vendor_pending = false;
                self.vendor = v;
            }
            // The outcome of an action is always reported, also after a switch.
            Msg::Done(epoch, host, label, r) => {
                let label = if epoch == self.epoch { label } else { format!("{host}: {label}") };
                self.status = match r {
                    Ok(o) => format!("{label}: ok {}", o.trim().lines().next().unwrap_or("")),
                    Err(e) => format!("{label}: FAILED {e}"),
                };
                if epoch != self.epoch {
                    return;
                }
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
        // New console output needs a redraw. The idle relock below does not
        // touch the console: once connected, typing has no further gate.
        if let Some(c) = &self.console {
            changed |= c.take_dirty();
            let live = c.live();
            if self.was_live && !live {
                self.status = if c.busy_elsewhere() {
                    "console is in use by another session. t takes it over.".into()
                } else {
                    "console session ended. Enter connects again.".into()
                };
                self.scroll_mode = false;
                changed = true;
            }
            self.was_live = live;
        }
        if let Some(limit) = self.relock
            && self.unlocked
            && now.saturating_duration_since(self.last_input) >= limit
        {
            self.lock(&format!("locked after {} min without input", limit.as_secs() / 60));
            changed = true;
        }
        if !self.states[self.current].halt_auto && matches!(self.screen, Screen::Dash | Screen::Sensors) {
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

    // ----- hosts ------------------------------------------------------------

    /// Make another host the one in use. Everything that belongs to the old
    /// host is dropped, and writes lock, so that an unlock never carries over.
    pub fn switch_host(&mut self, i: usize) {
        let Some(t) = self.hosts.get(i).cloned() else {
            self.status = format!("there is no host {}", i + 1);
            return;
        };
        if i == self.current {
            return;
        }
        self.lock("writes locked");
        self.queue.clear();
        self.want_sol = false;
        self.disconnect();
        self.current = i;
        self.t = t;
        self.epoch += 1;
        self.data.clear();
        self.loading.clear();
        self.last.clear();
        self.vendor = None;
        self.vendor_pending = false;
        self.status = format!("host {}: read-only", self.t.describe());
        if !self.states[i].halt_auto {
            self.start_vendor();
        } else {
            self.status = format!("host {}: auto refresh is stopped after a failure. r retries once.", self.t.describe());
        }
        // The host screen stays as it is. Other screens are fetched again.
        if self.screen != Screen::Hosts {
            self.switch(self.screen);
        }
    }

    // ----- console ----------------------------------------------------------

    pub fn console_live(&self) -> bool {
        self.console.as_ref().is_some_and(|c| c.live())
    }

    /// True while key presses go to the host: a live console on screen, with
    /// the keyboard focus and nothing else asking for the keys.
    pub fn typing(&self) -> bool {
        matches!(self.mode, Mode::Normal) && self.screen == Screen::Console && self.focus == Pane::Main && !self.scroll_mode && self.console_live()
    }

    /// Open the console. This is the one gate: it needs unlock, and after that
    /// typing is not asked about again.
    pub fn connect(&mut self) {
        if self.console_live() {
            return;
        }
        if !self.unlocked {
            self.mode = Mode::Unlock { anchor: self.pointer, connect: true };
            return;
        }
        self.want_connect = true;
    }

    /// The main loop reports the result of starting a session.
    pub fn connected(&mut self, r: Result<Console, String>) {
        match r {
            Ok(c) => {
                self.disconnect();
                self.console = Some(c);
                self.was_live = true;
                self.focus = Pane::Main;
                self.status = format!("console connected. Keys go to the host. {} then ? lists commands.", self.prefix_name());
            }
            Err(e) => self.status = format!("console: {e}"),
        }
    }

    /// Drop the session. The main loop ends it.
    pub fn disconnect(&mut self) {
        self.scroll_mode = false;
        self.prefix_pending = false;
        self.zoom = false;
        self.was_live = false;
        if let Some(c) = self.console.take() {
            self.closing.push(c);
        }
    }

    /// "Ctrl-]" and the like.
    pub fn prefix_name(&self) -> String {
        let c = (self.prefix | 0x40) as char;
        format!("Ctrl-{}", c.to_ascii_lowercase())
    }

    /// Offer to end the session that blocks ours. This is a write, so it goes
    /// through the same unlock and confirmation as other sensitive actions.
    pub fn take_over(&mut self) {
        if !self.console.as_ref().is_some_and(|c| c.busy_elsewhere()) {
            return;
        }
        if !self.unlocked {
            self.status = "locked: press u to unlock writes".into();
            return;
        }
        let act = Action {
            label: "End the other console session".into(),
            args: vec!["sol".into(), "deactivate".into()],
            risk: Risk::High,
        };
        self.mode = Mode::Confirm { act, anchor: self.pointer };
    }

    fn scroll_console(&mut self, delta: isize) {
        if let Some(c) = &self.console {
            c.scroll(delta);
        }
    }

    fn live_view(&mut self) {
        self.scroll_mode = false;
        if let Some(c) = &self.console {
            c.scroll_to_live();
        }
    }

    /// A command: the key after the prefix, or the matching button.
    pub fn command(&mut self, c: char) {
        self.prefix_pending = false;
        match c {
            '.' => {
                self.disconnect();
                self.status = "console disconnected".into();
            }
            'z' => {
                self.zoom = !self.zoom;
                if self.zoom {
                    self.focus = Pane::Main;
                }
            }
            '[' => {
                self.scroll_mode = self.console.is_some();
                self.focus = Pane::Main;
            }
            '\t' => {
                self.zoom = false;
                self.cycle_focus(1);
            }
            'q' => self.quit = true,
            'u' => self.toggle_lock(),
            'd' => self.toggle_details(),
            '?' => self.status = format!("after {}: . disconnect, z zoom, [ scroll, Tab pane, u lock, d details, 1-9 host, q quit; twice sends it", self.prefix_name()),
            '1'..='9' => self.switch_host(c as usize - '1' as usize),
            _ => self.status = format!("no console command {c:?}. {} then ? lists them.", self.prefix_name()),
        }
    }

    /// Handle a key on the Console screen. False leaves it to the normal key
    /// handling.
    fn console_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        if self.screen != Screen::Console {
            return false;
        }
        let is_prefix = mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char(c) if console::ctrl_byte(c) == Some(self.prefix));
        if self.prefix_pending {
            self.prefix_pending = false;
            if is_prefix {
                self.send_to_host(code, mods);
                return true;
            }
            match code {
                KeyCode::Char(c) => self.command(c),
                KeyCode::Tab => self.command('\t'),
                _ => self.status = "console command cancelled".into(),
            }
            return true;
        }
        if is_prefix {
            self.prefix_pending = true;
            return true;
        }
        let page = self.view_rows.max(2) as isize - 1;
        if self.scroll_mode {
            match code {
                KeyCode::Up | KeyCode::Char('k') => self.scroll_console(1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_console(-1),
                KeyCode::PageUp => self.scroll_console(page),
                KeyCode::PageDown => self.scroll_console(-page),
                KeyCode::Home | KeyCode::Char('g') => self.scroll_console(isize::MAX / 2),
                // Any other key returns to the live screen and is not sent.
                _ => self.live_view(),
            }
            return true;
        }
        if self.typing() {
            self.send_to_host(code, mods);
            return true;
        }
        false
    }

    fn send_to_host(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(c) = &self.console else { return };
        let app_cursor = c.screen().screen().application_cursor();
        if let Some(bytes) = console::encode_key(code, mods, app_cursor) {
            // Typing shows what is being typed at.
            c.scroll_to_live();
            c.send(bytes);
        }
    }

    // ----- screen content ---------------------------------------------------

    fn ctx(&self) -> view::Ctx<'_> {
        view::Ctx {
            data: self.data.get(&self.screen),
            vendor: self.vendor,
            cursor: self.cursor,
            loading: self.loading.contains(&self.screen),
            hosts: &self.hosts,
            current: self.current,
        }
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

    /// First line that can be selected. The sensor and host tables start with
    /// a header.
    pub fn first_row(&self) -> usize {
        match self.screen {
            Screen::Hosts => 1,
            Screen::Sensors if matches!(self.data.get(&self.screen), Some(Ok(_))) => 1,
            _ => 0,
        }
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
        let (low, high) = match self.screen {
            // Only the host rows, not the note below them.
            Screen::Hosts => (1, self.hosts.len()),
            _ if items > 0 => (0, items - 1),
            _ => (self.first_row(), self.lines().len().saturating_sub(1)),
        };
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
            self.nav_cursor = NAV.iter().position(|n| *n == self.screen).unwrap_or(1);
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
        self.scroll_mode = false;
        self.prefix_pending = false;
        self.zoom = false;
        self.scroll = 0;
        self.detail_scroll = 0;
        self.cursor = 0;
        self.cursor = if self.menu().is_empty() { self.first_row() } else { 0 };
        if s == Screen::Hosts {
            self.cursor = self.current + 1;
        }
        let stale = self.last.get(&s).is_none_or(|t| t.elapsed() > Duration::from_secs(5));
        if stale && !self.states[self.current].halt_auto {
            self.refresh(s);
        }
    }

    /// Open a navigation entry (Enter in the navigation pane, or a click).
    fn activate_nav(&mut self, i: usize) {
        let Some(&screen) = NAV.get(i) else { return };
        self.nav_cursor = i;
        self.switch(screen);
        self.focus = if self.pane_cycle().contains(&Pane::Main) { Pane::Main } else { Pane::Details };
    }

    // ----- writes -----------------------------------------------------------

    fn lock(&mut self, why: &str) {
        self.unlocked = false;
        // A console that is still waiting to be opened passed the gate before.
        self.want_connect = false;
        // An open confirmation belongs to the unlocked state, so it closes too.
        self.mode = Mode::Normal;
        self.status = why.into();
    }

    fn toggle_lock(&mut self) {
        if self.unlocked {
            self.lock("locked: read-only");
        } else {
            self.mode = Mode::Unlock { anchor: self.pointer, connect: false };
        }
    }

    /// Run the selected menu item. Harmless actions run at once, sensitive ones
    /// open a confirmation dialog.
    pub fn pick(&mut self) {
        // Selecting a host is not a write and works while locked.
        if self.screen == Screen::Hosts {
            if let Some(i) = self.cursor.checked_sub(1) {
                self.switch_host(i);
            }
            return;
        }
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
        if self.console_live() {
            self.status = "the console pane is connected: disconnect it first".into();
        } else if self.unlocked {
            self.want_sol = true;
        } else {
            self.status = "locked: press u to unlock, the console can type into the host".into();
        }
    }

    fn dialog_ok(&mut self) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Unlock { connect, .. } => {
                self.unlocked = true;
                self.status = "writes unlocked".into();
                self.want_connect |= connect;
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
        // The console sees keys first, so that Ctrl-C reaches the host.
        if matches!(self.mode, Mode::Normal) && self.console_key(code, mods) {
            return;
        }
        self.app_key(code, mods);
    }

    /// A key meant for the program itself. Clickable key hints come here
    /// directly, so a click is never typed into the host.
    fn app_key(&mut self, code: KeyCode, mods: KeyModifiers) {
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
                self.states[self.current].halt_auto = false;
                if self.vendor.is_none() {
                    self.start_vendor();
                }
                let s = self.screen;
                self.refresh(s);
            }
            // Keys 1 to 9 select the first nine hosts.
            KeyCode::Char(c @ '1'..='9') => self.switch_host(c as usize - '1' as usize),
            KeyCode::Esc => self.focus = self.pane_cycle().into_iter().find(|p| *p != Pane::Nav).unwrap_or(Pane::Main),
            KeyCode::Down | KeyCode::Char('j') => self.step(1),
            KeyCode::Up | KeyCode::Char('k') => self.step(-1),
            KeyCode::PageDown => self.step(page),
            KeyCode::PageUp => self.step(-page),
            KeyCode::Home | KeyCode::Char('g') => self.step(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.step(isize::MAX / 2),
            KeyCode::Char('t') if self.screen == Screen::Console => self.take_over(),
            KeyCode::Enter => match self.focus {
                Pane::Nav => self.activate_nav(self.nav_cursor),
                _ if self.screen == Screen::Console => self.connect(),
                Pane::Main | Pane::Details => self.pick(),
            },
            _ => {}
        }
    }

    /// Up and down act on whichever pane has the focus.
    fn step(&mut self, delta: isize) {
        match self.focus {
            // Up is back in time on the console.
            Pane::Main if self.screen == Screen::Console => self.scroll_console(delta.saturating_neg()),
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
                if self.screen == Screen::Hosts {
                    if (1..=self.hosts.len()).contains(&line) {
                        self.cursor = line;
                        self.pick();
                    }
                } else if self.menu().is_empty() {
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
            Hit::Host(i) => self.switch_host(i),
            Hit::Run => self.pick(),
            Hit::Key(k) => self.app_key(k, KeyModifiers::NONE),
            Hit::Connect => self.connect(),
            Hit::Command(c) => self.command(c),
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
            Some(Pane::Main) if self.screen == Screen::Console => self.scroll_console(-delta),
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
        let t = Target { bin: "/nonexistent/ipmitool".into(), name: None, host: Some("testbox".into()), user: None, iface: "lanplus".into(), pass_file: None };
        let (tx, rx) = mpsc::channel();
        std::mem::forget(rx);
        let mut a = App::new(vec![t], 0, Some(Duration::from_secs(900)), tx);
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
        let i = NAV.iter().position(|n| *n == Screen::Control).unwrap();
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

    /// Open the Console screen with a fake session that has printed `output`.
    fn with_console(a: &mut App, output: &[u8]) -> std::sync::mpsc::Receiver<Vec<u8>> {
        click(a, Hit::Nav(0));
        let (c, rx) = Console::fake(&a.t, output);
        a.connected(Ok(c));
        ui::draw(a, 120, 28);
        rx
    }

    fn ctrl(a: &mut App, c: char) {
        a.key(KeyCode::Char(c), KeyModifiers::CONTROL);
        ui::draw(a, 120, 28);
    }

    fn sent(rx: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<u8> {
        rx.try_iter().flatten().collect()
    }

    #[test]
    fn connecting_needs_unlock_once() {
        let mut a = app(120, 28);
        click(&mut a, Hit::Nav(0));
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Unlock { connect: true, .. }) && !a.want_connect);
        press(&mut a, KeyCode::Enter);
        assert!(!a.want_connect, "Enter alone must not unlock");
        press(&mut a, KeyCode::Esc);
        assert!(!a.want_connect && !a.unlocked);
        click(&mut a, Hit::Connect);
        click(&mut a, Hit::DialogOk);
        assert!(a.want_connect && a.unlocked);
    }

    #[test]
    fn typed_keys_go_to_the_host() {
        let mut a = app(120, 28);
        let rx = with_console(&mut a, b"");
        for c in ['q', 'u', 'r', '1', 'S', 'y'] {
            press(&mut a, KeyCode::Char(c));
        }
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Enter);
        ctrl(&mut a, 'c');
        assert_eq!(sent(&rx), b"qur1Sy\t\r\x03");
        assert!(!a.quit && !a.unlocked);
    }

    #[test]
    fn typing_survives_the_idle_relock() {
        let mut a = app(120, 28);
        a.unlocked = true;
        let rx = with_console(&mut a, b"");
        a.tick(Instant::now() + Duration::from_secs(901));
        assert!(!a.unlocked, "other writes lock");
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(sent(&rx), b"x");
    }

    #[test]
    fn prefix_commands_and_literal_prefix() {
        let mut a = app(120, 28);
        let rx = with_console(&mut a, b"");
        // Ctrl-] arrives from the terminal as Ctrl-5.
        ctrl(&mut a, '5');
        ctrl(&mut a, '5');
        assert_eq!(sent(&rx), [0x1d]);
        ctrl(&mut a, '5');
        press(&mut a, KeyCode::Tab);
        assert!(a.focus != Pane::Main && sent(&rx).is_empty());
        press(&mut a, KeyCode::Down);
        assert!(sent(&rx).is_empty(), "without focus nothing is typed");
        a.focus = Pane::Main;
        ctrl(&mut a, '5');
        press(&mut a, KeyCode::Char('.'));
        assert!(a.console.is_none() && a.closing.len() == 1 && sent(&rx).is_empty());
    }

    #[test]
    fn clicks_are_never_typed() {
        let mut a = app(120, 28);
        let rx = with_console(&mut a, b"");
        let regions: Vec<(Rect, Hit)> = a.hits.clone();
        for (r, h) in regions {
            if matches!(h, Hit::Key(_)) {
                click_at(&mut a, r.x, r.y);
                a.mode = Mode::Normal;
                a.quit = false;
            }
        }
        click(&mut a, Hit::Command('z'));
        assert!(a.zoom && sent(&rx).is_empty());
    }

    #[test]
    fn scrollback_by_keys_and_wheel() {
        let mut a = app(120, 28);
        let output: String = (0..100).map(|i| format!("line {i}\r\n")).collect();
        let rx = with_console(&mut a, output.as_bytes());
        a.mouse(MouseEvent { kind: MouseEventKind::ScrollUp, column: 40, row: 10, modifiers: KeyModifiers::NONE });
        assert_eq!(a.console.as_ref().unwrap().scrolled(), WHEEL_STEP);
        press(&mut a, KeyCode::Char('a'));
        assert_eq!((a.console.as_ref().unwrap().scrolled(), sent(&rx)), (0, b"a".to_vec()));
        ctrl(&mut a, '5');
        press(&mut a, KeyCode::Char('['));
        press(&mut a, KeyCode::Home);
        assert_eq!(a.console.as_ref().unwrap().scrolled(), 77);
        let c = ui::draw(&mut a, 120, 28);
        assert!(c.row(2).contains("line 0"), "{}", c.row(2));
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(a.console.as_ref().unwrap().scrolled(), 0);
        assert!(!a.scroll_mode && sent(&rx).is_empty(), "the key that leaves scrolling is not sent");
    }

    #[test]
    fn host_switch_disconnects_the_console() {
        let mut a = two_hosts();
        let rx = with_console(&mut a, b"");
        ctrl(&mut a, '5');
        press(&mut a, KeyCode::Char('2'));
        assert_eq!(a.current, 1);
        assert!(a.console.is_none() && a.closing.len() == 1);
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Unlock { connect: true, .. }), "the new host has its own gate");
        assert!(sent(&rx).is_empty());
    }

    #[test]
    fn a_waiting_connect_does_not_survive_a_lock_or_host_switch() {
        let mut a = two_hosts();
        a.switch(Screen::Console);
        a.unlocked = true;
        a.connect();
        assert!(a.want_connect);
        a.switch_host(1);
        assert!(!a.want_connect && !a.unlocked);
    }

    #[test]
    fn take_over_needs_unlock_and_confirmation() {
        let mut a = app(120, 28);
        let _rx = with_console(&mut a, b"Info: SOL payload already active on another session\r\n");
        a.console.as_ref().unwrap().end();
        a.tick(Instant::now());
        assert!(a.status.contains("another session"), "{}", a.status);
        press(&mut a, KeyCode::Char('t'));
        assert!(a.queue.is_empty() && matches!(a.mode, Mode::Normal));
        unlock(&mut a);
        press(&mut a, KeyCode::Char('t'));
        assert!(a.queue.is_empty());
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(a.queue[0].args, ["sol", "deactivate"]);
    }

    /// An app with two named hosts, "one" and "two".
    fn two_hosts() -> App {
        let mut a = app(120, 28);
        let mut one = a.t.clone();
        one.name = Some("one".into());
        let two = Target { name: Some("two".into()), host: Some("otherbox".into()), ..one.clone() };
        a.t = one.clone();
        a.hosts = vec![one, two];
        a.states = vec![HostState::default(); 2];
        ui::draw(&mut a, 120, 28);
        a
    }

    #[test]
    fn host_switch_locks_and_drops_the_old_host() {
        let mut a = two_hosts();
        open_power(&mut a);
        unlock(&mut a);
        press(&mut a, KeyCode::Enter); // queued for host one
        a.data.insert(Screen::Fru, Ok("fru of one".into()));
        a.vendor = Some(ipmi::SUPERMICRO_ID);
        let old = a.epoch;
        press(&mut a, KeyCode::Char('2'));
        assert_eq!(a.t.label(), "two");
        assert!(!a.unlocked && a.queue.is_empty() && a.vendor.is_none());
        assert!(!a.data.contains_key(&Screen::Fru));
        // Late replies from host one are ignored.
        a.on_msg(Msg::Data(old, Screen::Fru, Ok("late".into())));
        a.on_msg(Msg::Vendor(old, Some(1)));
        assert!(!a.data.contains_key(&Screen::Fru) && a.vendor.is_none());
        // A late action result is still reported, with the host name.
        a.on_msg(Msg::Done(old, "one".into(), "Power on".into(), Ok(String::new())));
        assert!(a.status.starts_with("one: Power on: ok"), "{}", a.status);
    }

    #[test]
    fn host_switch_closes_the_dialog_by_key_and_mouse() {
        let mut a = two_hosts();
        click(&mut a, Hit::Host(1));
        assert_eq!(a.current, 1);
        // With a dialog open neither digits nor tabs switch the host.
        press(&mut a, KeyCode::Char('u'));
        press(&mut a, KeyCode::Char('1'));
        assert_eq!(a.current, 1);
        press(&mut a, KeyCode::Esc);
        press(&mut a, KeyCode::Char('9'));
        assert_eq!(a.current, 1);
    }

    #[test]
    fn hosts_screen_selects_with_keys_and_mouse() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        ui::draw(&mut a, 120, 28);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Down);
        assert_eq!((a.cursor, a.current), (2, 0), "moving does not switch");
        press(&mut a, KeyCode::Enter);
        assert_eq!((a.current, a.screen), (1, Screen::Hosts));
        click(&mut a, Hit::Row(1));
        assert_eq!(a.current, 0);
    }

    #[test]
    fn login_failure_stays_halted_across_a_switch() {
        let mut a = two_hosts();
        let e = a.epoch;
        a.on_msg(Msg::Data(e, Screen::Dash, Err("Unable to establish RMCP+ session".into())));
        press(&mut a, KeyCode::Char('2'));
        press(&mut a, KeyCode::Char('1'));
        assert!(!a.loading.contains(&Screen::Dash), "no automatic retry");
        a.tick(Instant::now() + Duration::from_secs(60));
        assert!(!a.loading.contains(&Screen::Dash));
        press(&mut a, KeyCode::Char('r'));
        assert!(a.loading.contains(&Screen::Dash));
    }

    #[test]
    fn host_tabs_keep_the_current_one_visible() {
        let mut a = two_hosts();
        let base = a.hosts[0].clone();
        a.hosts = (0..12).map(|i| Target { name: Some(format!("host-number-{i}")), ..base.clone() }).collect();
        a.states = vec![HostState::default(); 12];
        for i in [0, 5, 11] {
            a.switch_host(i);
            for w in [40, 80, 120] {
                ui::draw(&mut a, w, 28);
                assert!(a.hits.iter().any(|(_, h)| *h == Hit::Host(i)), "host {i} at width {w}");
            }
        }
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
