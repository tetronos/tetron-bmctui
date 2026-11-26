//! Application state and input handling. Nothing here touches the terminal.
//! Keyboard and mouse both end up in the same functions (`activate_nav`,
//! `pick`, `toggle_lock`, ...), which is what keeps the two equal: a click can
//! not do anything a key can not do, and the reverse.
//!
//! Actions are not run here either. They are put in `queue`, and the main loop
//! runs them. Tests can therefore check what would run without running it.

use crate::bmcpw;
use crate::canvas::Rect;
use crate::console::{self, Console};
use crate::hosts::{self, NewHost};
use crate::ipmi::{self, Action, Risk, Screen, Target};
use crate::view;
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// Rows moved by one mouse wheel step.
const WHEEL_STEP: usize = 3;

/// Entries of the navigation pane, in display order (USER): choose a host,
/// look at it, then act on it.
pub const NAV: [Screen; 8] = [
    Screen::Hosts,
    Screen::Dash,
    Screen::Fru,
    Screen::Sensors,
    Screen::Fan,
    Screen::Sel,
    Screen::Console,
    Screen::Control,
];

/// Seconds between refreshes while Live is on, unless `--refresh` says else.
pub const LIVE_SECONDS: u64 = 30;

/// Screens that can be refreshed repeatedly with Live.
pub fn can_be_live(s: Screen) -> bool {
    matches!(s, Screen::Dash | Screen::Sensors)
}

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
    /// The power button in the top bar, and an entry of the power menu.
    /// The BMC password button, a field of its form and the arrows that choose
    /// the user.
    ChangeBmcPassword,
    PwField(usize),
    PwUser(isize),
    /// The Live button of the open screen.
    Live,
    /// The buttons below the host list, and a field of their forms.
    AddHost,
    HostPassword,
    SaveToList,
    UseHost,
    Field(usize),
    Power,
    PowerItem(usize),
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
    /// "Unlock writes on HOST?"
    Unlock { anchor: Option<(usize, usize)>, then: Then },
    /// "Switch host?" Asked when it would disconnect a live console.
    SwitchHost { to: usize, anchor: Option<(usize, usize)> },
    /// Changing a password on the BMC: the form, its confirmation, and the
    /// report of what happened.
    BmcPassword(bmcpw::Form),
    ConfirmBmcPassword { form: bmcpw::Form, anchor: Option<(usize, usize)> },
    Report(Vec<String>),
    /// The form that adds a host.
    AddHost(HostForm),
    /// "Save the password in a file?" Cancelling goes back to the form.
    SavePassword { form: HostForm, anchor: Option<(usize, usize)> },
    /// The power menu, on top of the open screen.
    Power,
    /// The list of keys.
    Help,
    /// "Run ACTION on HOST?" for sensitive actions.
    /// With `from_power`, cancelling goes back to the power menu.
    Confirm { act: Action, anchor: Option<(usize, usize)>, from_power: bool },
}

/// Longest filter text.
const FILTER_MAX: usize = 40;

/// What a text field accepts from a paste of `text` when it already holds
/// `have` characters. Line breaks at the end, as a copied line has, are left
/// off. Anything else that does not fit is refused as a whole: dropping or
/// cutting characters would silently save a wrong password.
fn field_text(text: &str, have: usize, max: usize) -> Result<&str, String> {
    let t = text.trim_end_matches(['\r', '\n']);
    if !t.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        Err("nothing pasted: only one line of plain ASCII is accepted".into())
    } else if have + t.len() > max {
        Err(format!("nothing pasted: this field takes at most {max} characters"))
    } else {
        Ok(t)
    }
}

/// Labels of the fields of the add host form, in order.
pub const FORM_FIELDS: [&str; 5] = ["Name", "Address", "User", "Password file", "or Password"];
/// The password field, which is shown as stars.
pub const FORM_PASSWORD: usize = 4;
/// Longest text a field takes.
const FIELD_MAX: usize = 128;

#[derive(Clone, Debug, Default)]
pub struct HostForm {
    /// With the index of a host the form changes its saved password, and only
    /// the last two fields are shown. Otherwise it adds a host.
    pub change: Option<usize>,
    /// Its `Debug` output hides the password.
    pub host: NewHost,
    /// Index into `FORM_FIELDS`.
    pub field: usize,
    pub error: String,
}

impl HostForm {
    /// Indexes into `FORM_FIELDS` that this form shows.
    pub fn fields(&self) -> std::ops::Range<usize> {
        if self.change.is_some() { FORM_PASSWORD - 1..FORM_FIELDS.len() } else { 0..FORM_FIELDS.len() }
    }

    pub fn value(&self, i: usize) -> &str {
        [&self.host.name, &self.host.address, &self.host.user, &self.host.pass_file, &self.host.password][i]
    }

    fn value_mut(&mut self) -> &mut String {
        match self.field {
            0 => &mut self.host.name,
            1 => &mut self.host.address,
            2 => &mut self.host.user,
            3 => &mut self.host.pass_file,
            _ => &mut self.host.password,
        }
    }
}

/// What follows the unlock dialog. Unlocking never runs an action by itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Then {
    Nothing,
    /// Open the console.
    Connect,
    /// Back to the power menu, also when cancelled.
    PowerMenu,
    /// On to the BMC password form.
    BmcPassword,
}

/// Results coming back from worker threads. Each carries the `epoch` it was
/// started in. The epoch changes on every host switch, so a late reply from
/// the previous host can not be shown as data of the new one.
pub enum Msg {
    Data(u64, Screen, Result<String, String>),
    /// Also carries the host name, to say where a late result belongs.
    Done(u64, String, String, Result<String, String>),
    Vendor(u64, Option<u32>),
    Lan(u64, Option<ipmi::Lan>),
    Users(u64, Result<Vec<bmcpw::User>, String>),
    /// The result of a change: the host index, the user and what happened. It is
    /// never dropped.
    PasswordChanged(usize, bmcpw::User, bmcpw::Outcome),
}

/// What is remembered about a host while another one is in use.
#[derive(Clone, Default)]
pub struct HostState {
    /// Result of the latest fetch: `None` until the first one returns.
    pub session_ok: Option<bool>,
    /// Local time of that result. A tab of another host shows how old it is.
    pub checked: Option<String>,
    /// The BMC's own address, once read.
    pub lan: Option<ipmi::Lan>,
    /// Requests the user did not ask for are stopped after a login failure.
    /// This is per host, so that switching away and back does not start
    /// retrying a wrong password.
    halt_auto: bool,
}

pub struct App {
    /// The host in use. Always a copy of `hosts[current]`.
    pub t: Target,
    pub hosts: Vec<Target>,
    pub current: usize,
    /// False until a host has been chosen. Until then nothing contacts a BMC
    /// and only the host list can be used.
    pub chosen: bool,
    /// The host row of the last click. A second click on it connects.
    clicked_host: Option<usize>,
    pub states: Vec<HostState>,
    pub epoch: u64,
    pub screen: Screen,
    pub data: HashMap<Screen, Result<String, String>>,
    pub loading: HashSet<Screen>,
    last: HashMap<Screen, Instant>,
    /// First visible entry of the navigation pane, which scrolls when the
    /// terminal is too short for all of them.
    pub nav_scroll: usize,
    /// Highlighted entry and pane height at the latest draw. The list follows
    /// the highlight only when one of them changes, so the wheel can scroll.
    pub nav_seen: Option<(usize, usize)>,
    pub nav_rows: usize,
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
    /// Show the console without the colors of the host.
    pub console_mono: bool,
    /// Byte of the console command key.
    pub prefix: u8,
    /// The prefix was pressed: the next key is a console command.
    pub prefix_pending: bool,
    /// Keys move through the scrollback instead of going to the host.
    pub scroll_mode: bool,
    /// The console takes the whole terminal.
    pub zoom: bool,
    was_live: bool,
    /// Text that lines of the open screen must contain. Empty shows all.
    pub filter: String,
    /// The filter is being typed.
    pub filtering: bool,
    /// Repeated refresh of the open screen. Off unless asked for.
    pub live: bool,
    pub live_every: Duration,
    /// When each screen last got data, for its title.
    pub updated: HashMap<Screen, String>,
    /// Screens that still show old data because the latest refresh failed.
    pub stale: HashSet<Screen>,
    pub help_scroll: usize,
    /// Selected entry of the power menu.
    pub power_cursor: usize,
    /// A checked form that the main loop writes to the host list.
    pub want_add: Option<HostForm>,
    /// Password work for the main loop.
    pub want_pw: Option<bmcpw::Job>,
    /// A change is running: nothing else may talk to a BMC or write.
    pub pw_busy: bool,
    /// A verified new password file that the host list has to point to.
    pub want_adopt: Option<(usize, bmcpw::User, std::path::PathBuf)>,
    /// The main loop writes the open screen to a file when set.
    pub want_export: bool,
    pub efi_boot: bool,
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
            chosen: true,
            clicked_host: None,
            epoch: 0,
            screen: Screen::Dash,
            data: HashMap::new(),
            loading: HashSet::new(),
            last: HashMap::new(),
            scroll: 0,
            detail_scroll: 0,
            cursor: 0,
            nav_cursor: 1,
            nav_scroll: 0,
            nav_seen: None,
            nav_rows: NAV.len(),
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
            console_mono: false,
            prefix: DEFAULT_PREFIX,
            prefix_pending: false,
            scroll_mode: false,
            zoom: false,
            was_live: false,
            filter: String::new(),
            filtering: false,
            live: false,
            live_every: Duration::from_secs(LIVE_SECONDS),
            updated: HashMap::new(),
            stale: HashSet::new(),
            help_scroll: 0,
            power_cursor: 0,
            want_add: None,
            want_pw: None,
            pw_busy: false,
            want_adopt: None,
            want_export: false,
            efi_boot: false,
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

    // ----- start ------------------------------------------------------------

    /// Start on the host list with no host in use.
    pub fn start_unchosen(&mut self) {
        self.chosen = false;
        self.screen = Screen::Hosts;
        self.cursor = 1;
        self.nav_cursor = NAV.iter().position(|s| *s == Screen::Hosts).unwrap_or(0);
        self.status = "choose a host: select it, then Enter or [ Connect ]. a adds one.".into();
    }

    /// Say why something is not available yet. True while no host is chosen.
    fn needs_host(&mut self) -> bool {
        if !self.chosen {
            self.status = "choose a host first: select it, then Enter or [ Connect ]".into();
        }
        !self.chosen
    }

    // ----- fetching ---------------------------------------------------------

    /// Fetch a screen in a worker thread. At most one fetch per screen runs.
    pub fn refresh(&mut self, s: Screen) {
        if !self.chosen || self.pw_busy || matches!(s, Screen::Hosts | Screen::Console) || !self.loading.insert(s) {
            return;
        }
        self.last.insert(s, Instant::now());
        let (t, tx, epoch) = (self.t.clone(), self.tx.clone(), self.epoch);
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Data(epoch, s, ipmi::fetch(&t, s)));
        });
    }

    /// Ask the BMC for its manufacturer id, which the fan screen depends on,
    /// and then for its own address.
    pub fn start_vendor(&mut self) {
        if !self.chosen || self.pw_busy || self.vendor_pending {
            return;
        }
        self.vendor_pending = true;
        let (t, tx, epoch) = (self.t.clone(), self.tx.clone(), self.epoch);
        std::thread::spawn(move || {
            let vendor = ipmi::fetch_vendor(&t);
            let _ = tx.send(Msg::Vendor(epoch, vendor));
            // One after the other, and only after a success: a wrong password
            // is then not tried twice.
            if vendor.is_some() {
                let _ = tx.send(Msg::Lan(epoch, ipmi::fetch_lan(&t)));
            }
        });
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            // Replies for a host that is no longer in use are dropped.
            Msg::Data(e, ..) | Msg::Vendor(e, _) | Msg::Lan(e, _) if e != self.epoch => {}
            Msg::Users(e, r) => {
                if e != self.epoch {
                    return;
                }
                if let Mode::BmcPassword(form) = &mut self.mode
                    && form.step == bmcpw::Step::Loading
                {
                    match r {
                        Ok(users) => {
                            // The login account is selected to begin with.
                            form.user = users.iter().position(|u| bmcpw::is_login(&self.t, u)).unwrap_or(0);
                            form.users = users;
                            form.step = bmcpw::Step::Pick;
                            form.field = bmcpw::PICK_USER;
                        }
                        Err(e) => {
                            let why = e.lines().next().unwrap_or("").to_string();
                            self.mode = Mode::Report(vec!["The BMC user list could not be read.".into(), why, "Nothing was changed.".into()]);
                        }
                    }
                }
            }
            Msg::PasswordChanged(host, user, out) => self.password_result(host, user, out),
            Msg::Lan(_, lan) => {
                // A failed read keeps what is already known.
                if lan.is_some() {
                    self.states[self.current].lan = lan;
                }
            }
            Msg::Data(_, s, r) => {
                self.loading.remove(&s);
                self.states[self.current].session_ok = Some(r.is_ok());
                self.states[self.current].checked = Some(crate::export::clock());
                match r {
                    Ok(text) => {
                        self.updated.insert(s, crate::export::clock_seconds());
                        self.stale.remove(&s);
                        self.data.insert(s, Ok(text));
                    }
                    Err(e) => {
                        // A failure ends Live: it must not repeat by itself.
                        let was_live = self.live && s == self.screen;
                        if was_live {
                            self.live = false;
                        }
                        let first = e.lines().next().unwrap_or("").to_string();
                        // Old readings stay, marked as old. Power and fan state
                        // do not: an old state must not pass for the current one.
                        if !matches!(s, Screen::Control | Screen::Fan) && matches!(self.data.get(&s), Some(Ok(_))) {
                            self.stale.insert(s);
                            self.status = format!("{} not updated{}: {first}", s.title(), if was_live { ", Live off" } else { "" });
                        } else {
                            if was_live {
                                self.status = format!("refresh failed, Live off: {first}");
                            }
                            self.updated.remove(&s);
                            self.data.insert(s, Err(e.clone()));
                        }
                        if ipmi::looks_like_auth_error(&e) {
                            self.states[self.current].halt_auto = true;
                            self.status = "connection or login failed: no more requests by themselves (a wrong password can lock the BMC account). r retries once.".into();
                        }
                    }
                }
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

    /// Periodic work: console output, idle relock and Live.
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        // New console output needs a redraw. The idle relock below does not
        // touch the console: once connected, typing has no further gate.
        if let Some(c) = &mut self.console {
            if !c.live() {
                c.reap();
            }
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
        if self.live && !self.states[self.current].halt_auto && can_be_live(self.screen) {
            let s = self.screen;
            if self.last.get(&s).is_none_or(|t| now.saturating_duration_since(*t) >= self.live_every) {
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

    /// A host switch asked for by key or click. It asks first when a console
    /// would be disconnected.
    fn request_host(&mut self, i: usize) {
        if i < self.hosts.len() && i != self.current && self.console_live() {
            self.prefix_pending = false;
            self.mode = Mode::SwitchHost { to: i, anchor: self.pointer };
        } else {
            self.switch_host(i);
        }
    }

    /// Make another host the one in use. Everything that belongs to the old
    /// host is dropped, and writes lock, so that an unlock never carries over.
    pub fn switch_host(&mut self, i: usize) {
        let Some(t) = self.hosts.get(i).cloned() else {
            self.status = format!("there is no host {}", i + 1);
            return;
        };
        if self.pw_busy {
            self.status = "wait: a password change is running".into();
            return;
        }
        let first = !self.chosen;
        if i == self.current && !first {
            return;
        }
        self.chosen = true;
        self.lock("writes locked");
        self.queue.clear();
        self.want_sol = false;
        self.disconnect();
        self.current = i;
        self.t = t;
        self.epoch += 1;
        self.data.clear();
        self.updated.clear();
        self.stale.clear();
        self.live = false;
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
        self.filter.clear();
        self.filtering = false;
        // The first choice opens the overview. Later the host screen stays as
        // it is, and other screens are fetched again.
        if first {
            self.switch(Screen::Dash);
            self.focus = self.pane_cycle().into_iter().find(|p| *p != Pane::Nav).unwrap_or(Pane::Main);
        } else if self.screen != Screen::Hosts {
            self.switch(self.screen);
        }
    }

    // ----- the BMC password -------------------------------------------------

    /// Start changing a password on the BMC of the host in use. It needs
    /// unlock, and then the current password as an extra check.
    pub fn open_bmc_password(&mut self) {
        let why = if self.selected_host().is_some_and(|i| i != self.current) || !self.chosen {
            "connect to that host first, then change its BMC password"
        } else if self.pw_busy {
            "wait: a password change is running"
        } else if self.console_live() {
            "disconnect the console first"
        } else if !self.loading.is_empty() || self.vendor_pending {
            "wait: the BMC is still being read, then try again"
        } else if !self.unlocked {
            self.mode = Mode::Unlock { anchor: self.pointer, then: Then::BmcPassword };
            return;
        } else {
            self.live = false;
            self.mode = Mode::BmcPassword(bmcpw::Form::default());
            return;
        };
        self.status = why.into();
    }

    /// Next or Change in the form.
    fn pw_submit(&mut self) {
        let Mode::BmcPassword(form) = &mut self.mode else { return };
        match form.step {
            bmcpw::Step::Current if self.t.host.is_none() => {
                // The local BMC has no login password to compare with.
                if form.current == "localhost" {
                    self.pw_checked(Ok(true));
                } else {
                    form.error = "type localhost to continue".into();
                }
            }
            bmcpw::Step::Current if !form.current.is_empty() => {
                self.want_pw = Some(bmcpw::Job::Check(form.current.clone()));
            }
            bmcpw::Step::Current => form.error = "type the current password".into(),
            bmcpw::Step::Loading => {}
            bmcpw::Step::Pick => match form.check_new() {
                Ok(()) => {
                    let form = form.clone();
                    self.mode = Mode::ConfirmBmcPassword { form, anchor: self.pointer };
                }
                Err(e) => form.error = e,
            },
        }
    }

    /// The result of the safety check. True means the user list is wanted.
    pub fn pw_checked(&mut self, r: Result<bool, String>) -> bool {
        let Mode::BmcPassword(form) = &mut self.mode else { return false };
        // Not kept longer than needed.
        form.current.clear();
        match r {
            Ok(true) => {
                form.step = bmcpw::Step::Loading;
                form.error.clear();
                true
            }
            Ok(false) => {
                form.wrong_tries += 1;
                form.error = "that is not the current password".into();
                if form.wrong_tries >= 3 {
                    self.lock("three wrong entries: password change closed, writes locked");
                }
                false
            }
            Err(e) => {
                form.error = e;
                false
            }
        }
    }

    fn pw_move_user(&mut self, delta: isize) {
        if let Mode::BmcPassword(form) = &mut self.mode
            && !form.users.is_empty()
        {
            let n = form.users.len() as isize;
            form.user = (form.user as isize + delta).rem_euclid(n) as usize;
            // What was typed was meant for the other user.
            form.new.clear();
            form.again.clear();
            form.error.clear();
        }
    }

    fn pw_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Mode::BmcPassword(form) = &mut self.mode else { return };
        let picking = form.step == bmcpw::Step::Pick;
        let fields = if picking { bmcpw::PICK_AGAIN + 1 } else { 1 };
        match code {
            KeyCode::Esc => self.dialog_cancel(),
            KeyCode::Tab | KeyCode::Down => form.field = (form.field + 1) % fields,
            KeyCode::BackTab | KeyCode::Up => form.field = (form.field + fields - 1) % fields,
            KeyCode::Left if picking && form.field == bmcpw::PICK_USER => self.pw_move_user(-1),
            KeyCode::Right if picking && form.field == bmcpw::PICK_USER => self.pw_move_user(1),
            KeyCode::Enter if picking && form.field < bmcpw::PICK_AGAIN => form.field += 1,
            KeyCode::Enter => self.pw_submit(),
            KeyCode::Backspace => {
                if let Some(t) = form.text_mut() {
                    t.pop();
                }
                form.error.clear();
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) && (c == ' ' || c.is_ascii_graphic()) => {
                if let Some(t) = form.text_mut()
                    && t.len() < bmcpw::MAX
                {
                    t.push(c);
                    form.error.clear();
                }
            }
            _ => {}
        }
    }

    /// Tell what happened and decide what the program does next.
    fn password_result(&mut self, host: usize, user: bmcpw::User, out: bmcpw::Outcome) {
        self.pw_busy = false;
        let login = bmcpw::is_login(&self.t, &user);
        let who = format!("user {} {}", user.id, user.name);
        let saved = out.saved.as_ref().map(|p| p.display().to_string());
        let mut lines: Vec<String> = vec![];
        match (out.changed, login, out.verified) {
            (true, false, _) => lines.push(format!("The BMC password of {who} was changed. How this program logs in is not affected.")),
            (true, true, true) => lines.push(format!("The BMC password of {who} was changed and a login with it worked.")),
            (true, true, false) => lines.push(format!("The BMC reported the password of {who} as changed, but it is not verified.")),
            (false, ..) => lines.push(format!("The change for {who} was NOT confirmed. It may or may not have happened.")),
        }
        if let Some(p) = &out.problem {
            lines.push(format!("Reason: {p}"));
        }
        match saved {
            // The host is pointed at the new file by the main loop.
            Some(_) if out.verified => {
                if let Some(p) = out.saved.clone() {
                    self.want_adopt = Some((host, user, p));
                }
            }
            Some(p) => {
                lines.push(format!("The new password is saved in {p}"));
                lines.push("The host still uses the old one. Nothing is retried. Check which password works before trying again: repeated wrong logins can lock the account.".into());
                // Requests with a password that may now be wrong must stop.
                if let Some(s) = self.states.get_mut(host) {
                    s.halt_auto = true;
                }
            }
            None => {}
        }
        self.status = lines[0].clone();
        self.mode = Mode::Report(lines);
    }

    /// Use the verified new password file for a host from now on. `old_kept`
    /// says what happened in the host list.
    pub fn adopt_password(&mut self, host: usize, target: Target, note: String) {
        if host >= self.hosts.len() {
            return;
        }
        self.hosts[host] = target.clone();
        self.states[host] = HostState { lan: self.states[host].lan.take(), ..HostState::default() };
        if host == self.current {
            self.t = target;
            self.epoch += 1;
            self.loading.clear();
            self.vendor_pending = false;
        }
        if let Mode::Report(lines) = &mut self.mode {
            lines.push(note);
        }
    }

    // ----- adding a host ----------------------------------------------------

    pub fn open_add_host(&mut self) {
        self.mode = Mode::AddHost(HostForm::default());
    }

    /// The host selected on the Hosts screen.
    pub fn selected_host(&self) -> Option<usize> {
        if self.screen != Screen::Hosts {
            return None;
        }
        self.cursor.checked_sub(1).filter(|i| *i < self.hosts.len())
    }

    /// Open the right form for the selected host. A host of the list gets its
    /// saved password changed. A host from the command line has no entry yet,
    /// so it is offered to be saved. Nothing here contacts or changes a BMC.
    pub fn open_host_password(&mut self) {
        let Some(i) = self.selected_host() else { return };
        let h = &self.hosts[i];
        if h.name.is_some() {
            self.mode = Mode::AddHost(HostForm { change: Some(i), field: FORM_PASSWORD - 1, ..Default::default() });
        } else if let Some(address) = &h.host {
            let host = NewHost {
                address: address.clone(),
                user: h.user.clone().unwrap_or_default(),
                pass_file: h.pass_file.clone().unwrap_or_default(),
                ..Default::default()
            };
            self.mode = Mode::AddHost(HostForm { host, ..Default::default() });
        } else {
            self.status = "the local BMC needs no password".into();
        }
    }

    /// Save was chosen. A typed password is written to disk only after a
    /// warning and a confirmation.
    fn submit_host(&mut self) {
        let Mode::AddHost(mut form) = std::mem::replace(&mut self.mode, Mode::Normal) else { return };
        let checked = match form.change {
            Some(i) => {
                form.host.name = self.hosts.get(i).and_then(|h| h.name.clone()).unwrap_or_default();
                hosts::validate_password(&form.host)
            }
            None => {
                let taken = self.hosts.iter().any(|h| h.name.as_ref() == Some(&form.host.name));
                hosts::validate(&form.host).and_then(|()| if taken { Err("that name is already in use".into()) } else { Ok(()) })
            }
        };
        match checked {
            Err(e) => {
                form.error = e;
                self.mode = Mode::AddHost(form);
            }
            Ok(()) if form.host.password.is_empty() => self.want_add = Some(form),
            Ok(()) => self.mode = Mode::SavePassword { form, anchor: self.pointer },
        }
    }

    /// The main loop reports the result of writing the list. The new host is
    /// not contacted until it is selected.
    pub fn host_added(&mut self, mut form: HostForm, r: Result<Target, String>) {
        match r {
            Ok(t) => {
                self.status = format!("added host {}", t.describe());
                self.hosts.push(t);
                self.states.push(HostState::default());
                if self.screen == Screen::Hosts {
                    self.cursor = self.hosts.len();
                    self.follow_cursor();
                }
            }
            Err(e) => {
                form.error = e;
                self.mode = Mode::AddHost(form);
            }
        }
    }

    /// The main loop reports the result of changing a saved password.
    pub fn password_changed(&mut self, mut form: HostForm, r: Result<hosts::Changed, String>) {
        let Some(i) = form.change.filter(|i| *i < self.hosts.len()) else { return };
        match r {
            Ok(c) => {
                self.hosts[i] = c.target.clone();
                // What was known came from the old password, also a stop after
                // a failed login. Nothing logs in until the user asks.
                self.states[i] = HostState { lan: self.states[i].lan.take(), ..HostState::default() };
                if i == self.current {
                    self.t = c.target;
                    // Late replies to requests with the old password are dropped.
                    self.epoch += 1;
                    self.loading.clear();
                    self.vendor_pending = false;
                }
                // No path here: the status line would cut it off.
                let gone = if c.removed.is_some() { " Old password file deleted." } else { "" };
                let next = if i == self.current && self.chosen { " Open a screen and press r to try it." } else { "" };
                self.status = format!("saved password changed for {}.{next}{gone}", self.hosts[i].label());
            }
            Err(e) => {
                form.error = e;
                self.mode = Mode::AddHost(form);
            }
        }
    }

    fn form_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Mode::AddHost(form) = &mut self.mode else { return };
        let (first, last) = (form.fields().start, form.fields().end - 1);
        match code {
            KeyCode::Esc => self.dialog_cancel(),
            KeyCode::Tab | KeyCode::Down => form.field = if form.field >= last { first } else { form.field + 1 },
            KeyCode::BackTab | KeyCode::Up => form.field = if form.field <= first { last } else { form.field - 1 },
            // Enter walks through the fields and saves from the last one.
            KeyCode::Enter if form.field < last => form.field += 1,
            KeyCode::Enter => self.submit_host(),
            KeyCode::Backspace => {
                form.value_mut().pop();
                form.error.clear();
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                let max = if form.field == FORM_PASSWORD { hosts::PASSWORD_MAX } else { FIELD_MAX };
                if (c == ' ' || c.is_ascii_graphic()) && form.value_mut().len() < max {
                    form.value_mut().push(c);
                    form.error.clear();
                }
            }
            _ => {}
        }
    }

    // ----- power menu -------------------------------------------------------

    pub fn power_actions(&self) -> Vec<Action> {
        view::actions(Screen::Control, self.vendor, self.efi_boot)
    }

    /// The power state for the menu, as one line.
    pub fn power_state(&self) -> view::Line {
        view::power_state(self.data.get(&Screen::Control), self.loading.contains(&Screen::Control))
    }

    /// Read the power state again. After a login failure only `r` retries, as
    /// on the other screens.
    fn read_power(&mut self) {
        if !self.states[self.current].halt_auto {
            // An old answer must not pass for the current state.
            self.data.remove(&Screen::Control);
            self.refresh(Screen::Control);
        }
    }

    pub fn open_power_menu(&mut self) {
        if self.needs_host() {
            return;
        }
        self.prefix_pending = false;
        self.filtering = false;
        self.power_cursor = 0;
        self.mode = Mode::Power;
        self.read_power();
    }

    /// Close the menu. On the console the keys go to the host again.
    fn leave_power_menu(&mut self) {
        self.mode = Mode::Normal;
        if self.screen == Screen::Console && self.pane_cycle().contains(&Pane::Main) {
            self.focus = Pane::Main;
            self.scroll_mode = false;
        }
    }

    /// Choose an entry of the power menu. The rules are those of the Power
    /// screen.
    fn pick_power(&mut self, i: usize) {
        let Some(a) = self.power_actions().get(i).cloned() else { return };
        self.power_cursor = i;
        if !self.unlocked {
            self.mode = Mode::Unlock { anchor: self.pointer, then: Then::PowerMenu };
            return;
        }
        match a.risk {
            Risk::Low => {
                self.queue.push(a);
                self.leave_power_menu();
            }
            Risk::High => self.mode = Mode::Confirm { act: a, anchor: self.pointer, from_power: true },
        }
    }

    fn power_menu_key(&mut self, code: KeyCode) {
        let last = self.power_actions().len().saturating_sub(1);
        match code {
            KeyCode::Esc => {
                self.leave_power_menu();
                self.status = "power menu closed, nothing was run".into();
            }
            KeyCode::Down | KeyCode::Char('j') => self.power_cursor = (self.power_cursor + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.power_cursor = self.power_cursor.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.power_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.power_cursor = last,
            KeyCode::Enter => self.pick_power(self.power_cursor),
            KeyCode::Char('r') => {
                self.states[self.current].halt_auto = false;
                self.read_power();
            }
            _ => {}
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
            self.mode = Mode::Unlock { anchor: self.pointer, then: Then::Connect };
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
                self.status = format!("connected. Only new output shows: if blank, press an arrow key. {} then ? for commands.", self.prefix_name());
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
        self.mode = Mode::Confirm { act, anchor: self.pointer, from_power: false };
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
            'p' => self.open_power_menu(),
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
            '?' => self.open_help(),
            '1'..='9' => self.request_host(c as usize - '1' as usize),
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

    /// Call `f` with what the view needs, the data narrowed by the filter.
    fn with_ctx<R>(&self, f: impl FnOnce(&view::Ctx) -> R) -> R {
        let all = self.data.get(&self.screen);
        let narrowed = match all {
            Some(Ok(text)) if !self.filter.is_empty() => Some(Ok(view::filter(self.screen, text, &self.filter))),
            _ => None,
        };
        f(&view::Ctx {
            data: narrowed.as_ref().or(all),
            vendor: self.vendor,
            cursor: self.cursor,
            loading: self.loading.contains(&self.screen),
            hosts: &self.hosts,
            // No row is marked as in use before a choice.
            current: if self.chosen { self.current } else { usize::MAX },
            lan: self.states[self.current].lan.as_ref(),
            efi_boot: self.efi_boot,
        })
    }

    pub fn lines(&self) -> Vec<view::Line> {
        self.with_ctx(|c| view::render(self.screen, c))
    }

    pub fn detail_lines(&self, width: usize) -> Vec<view::Line> {
        self.with_ctx(|c| view::details(self.screen, c, width))
    }

    pub fn menu(&self) -> Vec<Action> {
        view::actions(self.screen, self.vendor, self.efi_boot)
    }

    // ----- live -------------------------------------------------------------

    pub fn toggle_live(&mut self) {
        if !can_be_live(self.screen) {
            self.status = "Live works on Overview and Sensors".into();
        } else if self.live {
            self.live = false;
            self.status = "Live off. r refreshes.".into();
        } else if self.states[self.current].halt_auto {
            self.status = "the last connection or login failed: refresh with r first".into();
        } else {
            self.live = true;
            self.status = format!("Live on: refreshes every {} s while this screen is open", self.live_every.as_secs());
            let s = self.screen;
            self.refresh(s);
        }
    }

    // ----- filter, help, export ---------------------------------------------

    fn start_filter(&mut self) {
        if view::filterable(self.screen) {
            self.filtering = true;
            self.focus = self.pane_cycle().into_iter().find(|p| *p == Pane::Main).unwrap_or(self.focus);
        } else {
            self.status = "the filter works on Sensors, Events and Inventory".into();
        }
    }

    /// The filter changed: the old row numbers no longer mean anything.
    fn filter_changed(&mut self) {
        self.scroll = 0;
        self.detail_scroll = 0;
        self.cursor = self.first_row();
    }

    fn clear_filter(&mut self) {
        self.filtering = false;
        if !self.filter.is_empty() {
            self.filter.clear();
            self.filter_changed();
        }
    }

    /// Keys while the filter is typed. False leaves moving and the rest to the
    /// normal handling.
    fn filter_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        match code {
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                if self.filter.len() < FILTER_MAX {
                    self.filter.push(c);
                    self.filter_changed();
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.filter_changed();
            }
            KeyCode::Enter => self.filtering = false,
            KeyCode::Esc => self.clear_filter(),
            _ => return false,
        }
        true
    }

    fn open_help(&mut self) {
        self.help_scroll = 0;
        self.mode = Mode::Help;
    }

    fn export(&mut self) {
        if self.screen == Screen::Console {
            self.status = "the console can not be exported yet".into();
        } else if !matches!(self.screen, Screen::Hosts) && !matches!(self.data.get(&self.screen), Some(Ok(_))) {
            self.status = "nothing to export: no data".into();
        } else {
            self.want_export = true;
        }
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
        // On the console they are only shown by default when the emulated
        // screen still fits beside them.
        let wide = if self.screen == Screen::Console {
            usize::from(self.console_settings.cols.get()) + crate::ui::SIDE_PANES
        } else {
            crate::ui::WIDE
        };
        self.details.unwrap_or(self.width >= wide)
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
        // Live belongs to the screen it was turned on for.
        if s != self.screen {
            self.live = false;
        }
        self.screen = s;
        self.clicked_host = None;
        self.filter.clear();
        self.filtering = false;
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
        if screen != Screen::Hosts && self.needs_host() {
            return;
        }
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
        if self.needs_host() {
            return;
        }
        if self.pw_busy {
            self.status = "wait: a password change is running".into();
            return;
        }
        if self.unlocked {
            self.lock("locked: read-only");
        } else {
            self.mode = Mode::Unlock { anchor: self.pointer, then: Then::Nothing };
        }
    }

    /// Run the selected menu item. Harmless actions run at once, sensitive ones
    /// open a confirmation dialog.
    pub fn pick(&mut self) {
        // Selecting a host is not a write and works while locked.
        if self.screen == Screen::Hosts {
            if let Some(i) = self.cursor.checked_sub(1) {
                self.request_host(i);
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
            Risk::High => self.mode = Mode::Confirm { act: a, anchor: self.pointer, from_power: false },
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
            // No unlock while a change runs, so nothing else can be written.
            Mode::Unlock { .. } if self.pw_busy => self.status = "wait: a password change is running".into(),
            Mode::Unlock { then, .. } => {
                self.unlocked = true;
                self.status = "writes unlocked".into();
                match then {
                    Then::Nothing => {}
                    Then::Connect => self.want_connect = true,
                    // The entry stays selected and has to be chosen again.
                    Then::PowerMenu => self.mode = Mode::Power,
                    Then::BmcPassword => self.open_bmc_password(),
                }
            }
            Mode::Confirm { act, from_power, .. } => {
                if self.unlocked {
                    self.queue.push(act);
                }
                if from_power {
                    self.leave_power_menu();
                }
            }
            Mode::SwitchHost { to, .. } => self.switch_host(to),
            // The form's own button.
            Mode::AddHost(form) => {
                self.mode = Mode::AddHost(form);
                self.submit_host();
            }
            Mode::SavePassword { form, .. } => self.want_add = Some(form),
            Mode::BmcPassword(form) => {
                self.mode = Mode::BmcPassword(form);
                self.pw_submit();
            }
            Mode::ConfirmBmcPassword { form, .. } => {
                let Some(user) = form.selected().cloned() else { return };
                if self.unlocked {
                    self.want_pw = Some(bmcpw::Job::Change(user, form.new));
                    self.pw_busy = true;
                    self.live = false;
                    // Also keeps every other write out while this runs.
                    self.lock("changing the BMC password ...");
                }
            }
            Mode::Report(lines) => self.mode = Mode::Report(lines),
            Mode::Help => self.mode = Mode::Help,
            Mode::Power => self.mode = Mode::Power,
            Mode::Normal => {}
        }
    }

    fn dialog_cancel(&mut self) {
        self.mode = match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Help | Mode::Report(_) => return,
            // One step back, to the entries.
            Mode::ConfirmBmcPassword { form, .. } => Mode::BmcPassword(form),
            Mode::Power => {
                self.mode = Mode::Power;
                return self.power_menu_key(KeyCode::Esc);
            }
            Mode::SavePassword { form, .. } => Mode::AddHost(form),
            // One step back, to the menu the dialog was opened from.
            Mode::Confirm { from_power: true, .. } | Mode::Unlock { then: Then::PowerMenu, .. } => Mode::Power,
            _ => Mode::Normal,
        };
        self.status = "cancelled, nothing was run".into();
    }

    // ----- paste ------------------------------------------------------------

    /// Text pasted into the terminal. It arrives in one piece, apart from typed
    /// keys, so it can never act as a command or confirm a dialog. It goes into
    /// the open text field or to the console, and is ignored anywhere else.
    pub fn paste(&mut self, text: &str) {
        self.touch();
        self.clicked_host = None;
        self.prefix_pending = false;
        let typing = self.typing();
        match &mut self.mode {
            Mode::BmcPassword(form) => {
                let have = form.text_mut().map(|t| t.len());
                if let Some(have) = have {
                    match field_text(text, have, bmcpw::MAX) {
                        Ok(t) => {
                            if let Some(field) = form.text_mut() {
                                field.push_str(t);
                            }
                            form.error.clear();
                        }
                        Err(why) => form.error = why,
                    }
                }
            }
            Mode::AddHost(form) => {
                let password = form.field == FORM_PASSWORD;
                let max = if password { hosts::PASSWORD_MAX } else { FIELD_MAX };
                match field_text(text, form.value(form.field).len(), max) {
                    Ok(t) => {
                        form.value_mut().push_str(t);
                        form.error.clear();
                    }
                    // The pasted text is not repeated: it may be a password.
                    Err(why) => form.error = why,
                }
            }
            Mode::Normal if self.filtering => match field_text(text, self.filter.len(), FILTER_MAX) {
                Ok(t) => {
                    self.filter.push_str(t);
                    self.filter_changed();
                }
                Err(why) => self.status = why,
            },
            Mode::Normal if typing => {
                // A terminal sends Return for each line break.
                let bytes = text.replace("\r\n", "\r").replace('\n', "\r").into_bytes();
                if let Some(c) = &self.console {
                    c.scroll_to_live();
                    c.send(bytes);
                }
            }
            Mode::Normal => self.status = "paste ignored: no text field is open".into(),
            // Dialogs and menus take no text.
            _ => {}
        }
    }

    // ----- keyboard ---------------------------------------------------------

    pub fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.touch();
        self.clicked_host = None;
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
        if matches!(self.mode, Mode::Help) {
            match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?' | 'q') => self.dialog_cancel(),
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => self.help_scroll = self.help_scroll.saturating_sub(1),
                _ => {}
            }
            return;
        }
        if matches!(self.mode, Mode::Power) {
            return self.power_menu_key(code);
        }
        if matches!(self.mode, Mode::AddHost(_)) {
            return self.form_key(code, mods);
        }
        if matches!(self.mode, Mode::BmcPassword(_)) {
            return self.pw_key(code, mods);
        }
        if matches!(self.mode, Mode::Report(_)) {
            if matches!(code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                self.dialog_cancel();
            }
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
        if self.filtering && self.filter_key(code, mods) {
            return;
        }
        let page = self.view_rows.max(2) as isize - 1;
        match code {
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('/') => self.start_filter(),
            KeyCode::Char('e') => self.export(),
            KeyCode::Char('l') => self.toggle_live(),
            KeyCode::Char('p') => self.open_power_menu(),
            KeyCode::Char('a') if self.screen == Screen::Hosts => self.open_add_host(),
            KeyCode::Char('w') if self.screen == Screen::Hosts => self.open_host_password(),
            KeyCode::Char('c') if self.screen == Screen::Hosts => self.open_bmc_password(),
            // Esc first removes a filter that is in effect.
            KeyCode::Esc if !self.filter.is_empty() => self.clear_filter(),
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Tab => self.cycle_focus(1),
            KeyCode::BackTab => self.cycle_focus(-1),
            KeyCode::Char('d') => self.toggle_details(),
            KeyCode::Char('u') => self.toggle_lock(),
            KeyCode::Char('S') => self.open_sol(),
            KeyCode::Char('r') if !self.chosen => {
                self.needs_host();
            }
            KeyCode::Char('r') => {
                self.states[self.current].halt_auto = false;
                if self.vendor.is_none() || self.states[self.current].lan.is_none() {
                    self.start_vendor();
                }
                let s = self.screen;
                self.refresh(s);
            }
            // Keys 1 to 9 select the first nine hosts.
            KeyCode::Char(c @ '1'..='9') => self.request_host(c as usize - '1' as usize),
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
                Hit::PowerItem(i) if matches!(self.mode, Mode::Power) => self.pick_power(i),
                Hit::PwField(i) => {
                    if let Mode::BmcPassword(form) = &mut self.mode {
                        form.field = i;
                    }
                }
                Hit::PwUser(d) => self.pw_move_user(d),
                Hit::Field(i) => {
                    if let Mode::AddHost(form) = &mut self.mode {
                        form.field = i;
                    }
                }
                Hit::DialogOk => self.dialog_ok(),
                Hit::DialogCancel => self.dialog_cancel(),
                _ => {}
            }
            return;
        }
        // A click anywhere else ends typing the filter, and keeps it.
        if !matches!(h, Hit::Key(_)) {
            self.filtering = false;
        }
        match h {
            Hit::Pane(p) => self.focus_pane(p),
            Hit::Nav(i) => self.activate_nav(i),
            Hit::Row(line) => {
                self.focus = Pane::Main;
                self.detail_scroll = 0;
                if self.screen == Screen::Hosts {
                    // The first click only selects, so that a host's password
                    // can be fixed without first logging in with it. A second
                    // click on the same row connects.
                    if (1..=self.hosts.len()).contains(&line) {
                        let again = self.clicked_host == Some(line) && self.cursor == line;
                        self.cursor = line;
                        self.clicked_host = Some(line);
                        if again {
                            self.clicked_host = None;
                            self.pick();
                        }
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
            Hit::Host(i) => self.request_host(i),
            Hit::Power => self.open_power_menu(),
            Hit::Live => self.toggle_live(),
            Hit::AddHost => self.open_add_host(),
            Hit::HostPassword | Hit::SaveToList => self.open_host_password(),
            Hit::UseHost => self.pick(),
            Hit::Field(_) | Hit::PwField(_) | Hit::PwUser(_) => {}
            Hit::ChangeBmcPassword => self.open_bmc_password(),
            // Only drawn while the menu is open, which is handled above.
            Hit::PowerItem(_) => {}
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
        if matches!(self.mode, Mode::Help) {
            self.help_scroll = self.help_scroll.saturating_add_signed(delta);
            return;
        }
        if matches!(self.mode, Mode::Power) {
            let last = self.power_actions().len().saturating_sub(1);
            self.power_cursor = self.power_cursor.saturating_add_signed(delta.signum()).min(last);
            return;
        }
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
            Some(Pane::Nav) => {
                let max = NAV.len().saturating_sub(self.nav_rows);
                self.nav_scroll = self.nav_scroll.saturating_add_signed(delta.signum()).min(max);
            }
            None => {}
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

    /// The clickable navigation entry of a screen.
    fn nav(s: Screen) -> Hit {
        Hit::Nav(NAV.iter().position(|n| *n == s).unwrap())
    }

    fn open_power(a: &mut App) {
        click(a, nav(Screen::Control));
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
        a.cursor = a.menu().iter().position(|x| x.label.starts_with("Identify")).unwrap();
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.queue.len(), 1);
        assert_eq!(a.queue[0].args, ["chassis", "identify", "15"]);
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
                    a.mode = Mode::Confirm { act: ipmi::control_actions()[2].clone(), anchor: Some((x, y)), from_power: false };
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
        assert_eq!((a.screen, a.focus), (Screen::Fru, Pane::Main));
    }

    /// Open the Console screen with a fake session that has printed `output`.
    fn with_console(a: &mut App, output: &[u8]) -> std::sync::mpsc::Receiver<Vec<u8>> {
        click(a, nav(Screen::Console));
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
        click(&mut a, nav(Screen::Console));
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Unlock { then: Then::Connect, .. }) && !a.want_connect);
        press(&mut a, KeyCode::Enter);
        assert!(!a.want_connect, "Enter alone must not unlock");
        press(&mut a, KeyCode::Esc);
        assert!(!a.want_connect && !a.unlocked);
        click(&mut a, Hit::Connect);
        click(&mut a, Hit::DialogOk);
        assert!(a.want_connect && a.unlocked);
    }

    #[test]
    fn connect_by_keyboard_only() {
        let mut a = app(120, 28);
        press(&mut a, KeyCode::BackTab);
        // From Overview down to Console.
        for _ in 0..5 {
            press(&mut a, KeyCode::Down);
        }
        press(&mut a, KeyCode::Enter); // open the Console screen
        assert_eq!((a.screen, a.focus), (Screen::Console, Pane::Main));
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('y'));
        assert!(a.want_connect);
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
        assert_eq!(a.console.as_ref().unwrap().scrolled(), 70);
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
        press(&mut a, KeyCode::Enter);
        assert!(a.current == 0 && a.console_live(), "Enter alone must not switch");
        press(&mut a, KeyCode::Esc);
        assert!(a.current == 0 && a.typing() && sent(&rx).is_empty(), "cancel resumes typing");
        // A click on the tab asks as well.
        click(&mut a, Hit::Host(1));
        assert!(matches!(a.mode, Mode::SwitchHost { to: 1, .. }));
        click(&mut a, Hit::DialogOk);
        assert_eq!(a.current, 1);
        assert!(a.console.is_none() && a.closing.len() == 1);
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::Unlock { then: Then::Connect, .. }), "the new host has its own gate");
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
    fn console_details_do_not_crop_by_default() {
        let mut a = app(120, 28);
        assert!(a.details_visible());
        a.switch(Screen::Console);
        assert!(!a.details_visible());
        ui::draw(&mut a, 140, 36);
        assert!(a.details_visible());
        let _rx = with_console(&mut a, b"");
        assert!(ui::draw(&mut a, 120, 36).row(1).contains("typing to host"));
        assert!(ui::draw(&mut a, 120, 36).row(1).find("cropped").is_none());
        assert!(ui::draw(&mut a, 100, 24).row(1).contains("cropped"));
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
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('y')); // queued for host one
        assert_eq!(a.queue.len(), 1);
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
        assert_eq!((a.current, a.cursor), (1, 1), "a click only selects");
        click(&mut a, Hit::UseHost);
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
    fn bmc_address_is_shown_when_it_fits() {
        let mut a = two_hosts();
        let lan = ipmi::Lan { ip: "192.0.2.10".into(), mac: "02:00:00:aa:bb:cc".into() };
        let old = a.epoch;
        a.on_msg(Msg::Lan(old, Some(lan.clone())));
        let top = ui::draw(&mut a, 120, 28).row(0);
        assert!(top.contains("192.0.2.10 02:00:00:aa:bb:cc") && top.contains("[ Power ]"), "{top}");
        // Too narrow: tabs and the button win, and nothing is cut in half.
        for w in 40..120 {
            let top = ui::draw(&mut a, w, 28).row(0);
            assert!(top.contains("[ Power ]"));
            assert!(top.contains("02:00:00:aa:bb:cc") || !top.contains("02:00"), "{w}: {top}");
        }
        // Another host does not inherit it, also not from a late reply.
        a.switch_host(1);
        a.on_msg(Msg::Lan(old, Some(lan)));
        assert!(!ui::draw(&mut a, 120, 28).row(0).contains("192.0.2.10"));
        a.switch_host(0);
        assert!(ui::draw(&mut a, 120, 28).row(0).contains("192.0.2.10"), "kept per host");
    }

    fn type_text(a: &mut App, text: &str) {
        for c in text.chars() {
            press(a, KeyCode::Char(c));
        }
    }

    /// Fill the form with the keyboard, from the name down to the password.
    fn fill(a: &mut App, values: [&str; 5]) {
        for (i, v) in values.iter().enumerate() {
            type_text(a, v);
            if i < 4 {
                press(a, KeyCode::Tab);
            }
        }
    }

    #[test]
    fn add_host_with_a_password_warns_and_confirms() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        ui::draw(&mut a, 120, 28);
        press(&mut a, KeyCode::Char('a'));
        // Letters that are commands elsewhere are text in the form.
        fill(&mut a, ["quip", "192.0.2.9", "ADMIN", "", "hunter2y"]);
        assert!(!a.quit && !a.unlocked);
        let c = ui::draw(&mut a, 120, 28);
        let screen: String = (0..28).map(|y| c.row(y)).collect();
        assert!(!screen.contains("hunter2y") && screen.contains("********"), "hidden while typed");
        assert!(!format!("{:?}", a.mode).contains("hunter2y"));
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::SavePassword { .. }) && a.want_add.is_none());
        press(&mut a, KeyCode::Enter);
        assert!(a.want_add.is_none(), "Enter alone must not save a password");
        let c = ui::draw(&mut a, 120, 28);
        assert!((0..28).any(|y| c.row(y).contains("NOT encrypted")));
        press(&mut a, KeyCode::Esc);
        assert!(matches!(a.mode, Mode::AddHost(_)), "back to the form");
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('y'));
        let form = a.want_add.take().unwrap();
        assert_eq!((form.host.name.as_str(), form.host.password.as_str()), ("quip", "hunter2y"));
        // A failed write brings the form back with the reason.
        a.host_added(form.clone(), Err("disk full".into()));
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.error == "disk full" && f.host.name == "quip"));
        a.mode = Mode::Normal;
        let t = Target { name: Some("quip".into()), ..a.t.clone() };
        a.host_added(form, Ok(t));
        assert_eq!((a.hosts.len(), a.states.len(), a.current, a.cursor), (3, 3, 0, 3));
    }

    #[test]
    fn add_host_by_mouse_with_a_password_file() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        ui::draw(&mut a, 120, 28);
        click(&mut a, Hit::AddHost);
        click(&mut a, Hit::DialogOk);
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.error.contains("needed")));
        for (i, v) in [(0, "three"), (1, "h3"), (3, "~/pw")] {
            click(&mut a, Hit::Field(i));
            type_text(&mut a, v);
        }
        click(&mut a, Hit::DialogOk);
        assert!(a.want_add.is_some() && matches!(a.mode, Mode::Normal), "no password, no warning");
        // Cancel leaves everything as it was. A name in use is refused.
        a.want_add = None;
        click(&mut a, Hit::AddHost);
        fill(&mut a, ["two", "h", "", "/p", ""]);
        press(&mut a, KeyCode::Enter);
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.error.contains("already in use")));
        click(&mut a, Hit::DialogCancel);
        assert!(matches!(a.mode, Mode::Normal) && a.want_add.is_none());
    }

    #[test]
    fn change_the_saved_password_of_a_host_not_in_use() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        a.states[1].halt_auto = true;
        ui::draw(&mut a, 120, 28);
        let before = a.epoch;
        click(&mut a, Hit::Row(2));
        click(&mut a, Hit::HostPassword);
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.change == Some(1)));
        assert_eq!((a.current, a.epoch), (0, before), "nothing was switched or contacted");
        let c = ui::draw(&mut a, 120, 28);
        let screen: String = (0..28).map(|y| c.row(y)).collect();
        assert!(screen.contains("Saved password: two") && screen.contains("not change the BMC") && !screen.contains("Address "));
        // Tab stays within the two fields. Letters are text.
        press(&mut a, KeyCode::Tab);
        type_text(&mut a, "q-secret");
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Tab);
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::SavePassword { .. }) && a.want_add.is_none());
        press(&mut a, KeyCode::Enter);
        assert!(a.want_add.is_none(), "Enter alone must not save");
        press(&mut a, KeyCode::Char('y'));
        let form = a.want_add.take().unwrap();
        assert_eq!((form.change, form.host.name.as_str(), form.host.password.as_str()), (Some(1), "two", "q-secret"));
        let t = Target { pass_file: Some("/new".into()), ..a.hosts[1].clone() };
        a.password_changed(form, Ok(hosts::Changed { target: t, removed: None }));
        assert_eq!(a.hosts[1].pass_file.as_deref(), Some("/new"));
        assert!(!a.states[1].halt_auto && a.current == 0 && a.epoch == before && a.loading.is_empty());
        assert!(!a.status.contains("q-secret"));
    }

    #[test]
    fn new_password_for_the_host_in_use_waits_for_the_user() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        a.states[0].halt_auto = true;
        let old = a.epoch;
        press(&mut a, KeyCode::Char('w'));
        let Mode::AddHost(mut form) = std::mem::replace(&mut a.mode, Mode::Normal) else { panic!("no form") };
        form.host.pass_file = "/new".into();
        let t = Target { pass_file: Some("/new".into()), ..a.hosts[0].clone() };
        a.password_changed(form, Ok(hosts::Changed { target: t, removed: None }));
        assert_eq!(a.t.pass_file.as_deref(), Some("/new"));
        assert!(a.epoch > old && !a.states[0].halt_auto && a.loading.is_empty(), "no login by itself");
    }

    #[test]
    fn command_line_host_can_be_saved() {
        let mut a = app(120, 28);
        a.t.user = Some("ADMIN".into());
        a.t.pass_file = Some("/home/x/pw".into());
        a.hosts[0] = a.t.clone();
        a.switch(Screen::Hosts);
        ui::draw(&mut a, 120, 28);
        click(&mut a, Hit::SaveToList);
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.change.is_none() && f.host.address == "testbox" && f.host.user == "ADMIN" && f.host.pass_file == "/home/x/pw"));
    }

    /// The buttons belong to the list, not to the details of one host.
    #[test]
    fn nothing_is_contacted_until_a_host_is_chosen() {
        let mut a = two_hosts();
        a.loading.clear();
        a.start_unchosen();
        ui::draw(&mut a, 120, 28);
        let untouched = |a: &App| a.loading.is_empty() && !a.unlocked && !a.want_connect && !a.want_sol && a.queue.is_empty();
        for k in ['r', 'u', 'p', 'l', 'S', 't', '/'] {
            press(&mut a, KeyCode::Char(k));
            assert!(matches!(a.mode, Mode::Normal) && untouched(&a), "{k}");
        }
        press(&mut a, KeyCode::Down);
        for i in 0..NAV.len() {
            click(&mut a, Hit::Nav(i));
            assert!(a.screen == Screen::Hosts && untouched(&a), "nav {i}");
        }
        click(&mut a, Hit::Power);
        click(&mut a, Hit::Row(2));
        a.tick(Instant::now() + Duration::from_secs(3600));
        assert!(!a.chosen && untouched(&a));
        let c = ui::draw(&mut a, 120, 28);
        let text: String = (0..28).map(|y| c.row(y)).collect();
        assert!(!text.contains("* one") && !text.contains("* two") && text.contains("no host chosen"), "{text}");
    }

    #[test]
    fn choosing_the_first_or_another_host_connects_to_it() {
        // Enter on the first row, which has the same index as the unused default.
        for (how, want) in [(0, 0), (1, 1), (2, 1), (3, 0)] {
            let mut a = two_hosts();
            a.loading.clear();
            a.start_unchosen();
            ui::draw(&mut a, 120, 28);
            match how {
                0 => press(&mut a, KeyCode::Enter),
                1 => {
                    click(&mut a, Hit::Row(2));
                    click(&mut a, Hit::UseHost);
                }
                2 => press(&mut a, KeyCode::Char('2')),
                _ => click(&mut a, Hit::Host(0)),
            }
            assert!(a.chosen && a.current == want && a.t == a.hosts[want], "way {how}");
            assert!(a.screen == Screen::Dash && a.loading.contains(&Screen::Dash), "way {how}");
        }
    }

    #[test]
    fn hosts_can_be_managed_before_choosing() {
        let mut a = two_hosts();
        a.loading.clear();
        a.start_unchosen();
        press(&mut a, KeyCode::Char('w'));
        assert!(matches!(a.mode, Mode::AddHost(_)));
        press(&mut a, KeyCode::Esc);
        press(&mut a, KeyCode::Char('a'));
        assert!(matches!(a.mode, Mode::AddHost(_)) && !a.chosen && a.loading.is_empty());
    }

    /// Which host row is drawn highlighted.
    fn highlighted_rows(a: &mut App) -> Vec<usize> {
        let c = ui::draw(a, 120, 28);
        let rows: Vec<(Rect, usize)> = a.hits.iter().filter_map(|(r, h)| if let Hit::Row(i) = h { Some((*r, *i)) } else { None }).collect();
        rows.iter().filter(|(r, _)| c.kind(r.x + 3, r.y) == view::Kind::Sel).map(|(_, i)| *i).collect()
    }

    #[test]
    fn the_selected_host_is_highlighted() {
        let mut a = two_hosts();
        a.loading.clear();
        a.start_unchosen();
        assert_eq!(highlighted_rows(&mut a), [1]);
        press(&mut a, KeyCode::Down);
        assert_eq!(highlighted_rows(&mut a), [2]);
        click(&mut a, Hit::Row(1));
        assert_eq!(highlighted_rows(&mut a), [1]);
    }

    /// Two hosts, connected to the first, nothing loading, on the Hosts screen.
    fn ready_for_password() -> App {
        let mut a = two_hosts();
        a.t.user = Some("admin".into());
        a.hosts[0] = a.t.clone();
        a.switch(Screen::Hosts);
        a.loading.clear();
        ui::draw(&mut a, 120, 28);
        a
    }

    fn pw_form(a: &App) -> &bmcpw::Form {
        let Mode::BmcPassword(f) = &a.mode else { panic!("no password form: {:?}", a.mode) };
        f
    }

    fn users() -> Vec<bmcpw::User> {
        bmcpw::parse_users("2   admin   false false true ADMINISTRATOR\n3   monitor true true true USER\n")
    }

    /// Pass the safety check as the main loop would, and deliver the user list.
    fn to_pick(a: &mut App) {
        click(a, Hit::ChangeBmcPassword);
        click(a, Hit::DialogOk); // unlock
        type_text(a, "current-pw");
        press(a, KeyCode::Enter);
        assert!(matches!(a.want_pw.take(), Some(bmcpw::Job::Check(p)) if p == "current-pw"));
        assert!(a.pw_checked(Ok(true)));
        let e = a.epoch;
        a.on_msg(Msg::Users(e, Ok(users())));
        ui::draw(a, 120, 28);
        assert_eq!(pw_form(a).step, bmcpw::Step::Pick);
    }

    #[test]
    fn password_change_needs_unlock_current_password_and_confirmation() {
        let mut a = ready_for_password();
        press(&mut a, KeyCode::Char('c'));
        assert!(matches!(a.mode, Mode::Unlock { then: Then::BmcPassword, .. }));
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(pw_form(&a).step, bmcpw::Step::Current);
        // A wrong current password goes nowhere, three close the form and lock.
        for n in 1..=3 {
            type_text(&mut a, "wrong");
            press(&mut a, KeyCode::Enter);
            a.want_pw = None;
            assert!(!a.pw_checked(Ok(false)));
            if n < 3 {
                assert!(pw_form(&a).current.is_empty() && pw_form(&a).step == bmcpw::Step::Current);
            }
        }
        assert!(matches!(a.mode, Mode::Normal) && !a.unlocked && !a.pw_busy);

        press(&mut a, KeyCode::Char('c'));
        press(&mut a, KeyCode::Char('y'));
        type_text(&mut a, "current-pw");
        press(&mut a, KeyCode::Enter);
        a.want_pw = None;
        a.pw_checked(Ok(true));
        let e = a.epoch;
        a.on_msg(Msg::Users(e, Ok(users())));
        assert_eq!(pw_form(&a).selected().unwrap().name, "admin", "the login user is preselected");
        press(&mut a, KeyCode::Tab);
        type_text(&mut a, "y q");
        press(&mut a, KeyCode::Tab);
        type_text(&mut a, "different");
        press(&mut a, KeyCode::Enter);
        assert!(pw_form(&a).error.contains("do not match") && a.want_pw.is_none());
        for _ in 0.."different".len() {
            press(&mut a, KeyCode::Backspace);
        }
        type_text(&mut a, "y q");
        press(&mut a, KeyCode::Enter);
        assert!(matches!(a.mode, Mode::ConfirmBmcPassword { .. }) && a.want_pw.is_none());
        let c = ui::draw(&mut a, 120, 28);
        let text: String = (0..28).map(|y| c.row(y)).collect();
        assert!(text.contains("user 2 admin") && text.contains("otherbox") | text.contains("testbox") && !text.contains("y q"), "{text}");
        press(&mut a, KeyCode::Enter);
        assert!(a.want_pw.is_none() && !a.pw_busy, "Enter alone must not change a password");
        press(&mut a, KeyCode::Esc);
        assert_eq!(pw_form(&a).new, "y q", "cancel goes back to the entries");
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('y'));
        assert!(matches!(a.want_pw.take(), Some(bmcpw::Job::Change(u, p)) if u.id == 2 && p == "y q"));
        assert!(a.pw_busy && !a.unlocked);
    }

    #[test]
    fn nothing_else_happens_while_a_password_change_runs() {
        let mut a = ready_for_password();
        to_pick(&mut a);
        click(&mut a, Hit::PwField(bmcpw::PICK_NEW));
        a.paste("pasted new");
        click(&mut a, Hit::PwField(bmcpw::PICK_AGAIN));
        a.paste("pasted new\n");
        click(&mut a, Hit::DialogOk);
        a.paste("y");
        assert!(a.want_pw.is_none(), "paste can not confirm");
        click(&mut a, Hit::DialogOk);
        assert!(a.pw_busy);
        for k in ['2', 'r', 'u', 'y', 'c', 'l', 'p', 'S'] {
            press(&mut a, KeyCode::Char(k));
        }
        press(&mut a, KeyCode::Esc);
        a.switch(Screen::Sensors);
        a.tick(Instant::now() + Duration::from_secs(3600));
        assert_eq!(a.current, 0, "no host switch");
        assert!(!a.unlocked && a.loading.is_empty() && !a.want_sol && !a.want_connect && a.queue.is_empty());
    }

    #[test]
    fn choosing_another_user_clears_what_was_typed() {
        let mut a = ready_for_password();
        to_pick(&mut a);
        press(&mut a, KeyCode::Tab);
        type_text(&mut a, "for-admin");
        click(&mut a, Hit::PwUser(1));
        assert!(pw_form(&a).selected().unwrap().name == "monitor" && pw_form(&a).new.is_empty());
        click(&mut a, Hit::PwUser(1));
        assert_eq!(pw_form(&a).selected().unwrap().name, "admin", "wraps around");
        click(&mut a, Hit::DialogCancel);
        assert!(matches!(a.mode, Mode::Normal) && a.want_pw.is_none());
    }

    #[test]
    fn password_change_only_for_the_connected_idle_host() {
        let mut a = ready_for_password();
        press(&mut a, KeyCode::Down); // the other host is selected
        press(&mut a, KeyCode::Char('c'));
        assert!(matches!(a.mode, Mode::Normal) && a.status.contains("connect to that host"), "{}", a.status);
        ui::draw(&mut a, 120, 28);
        assert!(!a.hits.iter().any(|(_, h)| *h == Hit::ChangeBmcPassword));
        press(&mut a, KeyCode::Up);
        a.loading.insert(Screen::Dash);
        press(&mut a, KeyCode::Char('c'));
        assert!(a.status.contains("still being read"));
        a.loading.clear();
        let _rx = with_console(&mut a, b"");
        a.switch(Screen::Hosts);
        press(&mut a, KeyCode::Char('c'));
        assert!(a.status.contains("disconnect the console"), "{}", a.status);
        let mut b = ready_for_password();
        b.start_unchosen();
        press(&mut b, KeyCode::Char('c'));
        assert!(matches!(b.mode, Mode::Normal));
    }

    #[test]
    fn password_results_are_reported_and_only_verified_ones_adopted() {
        for (changed, verified, adopt) in [(true, true, true), (true, false, false), (false, false, false)] {
            let mut a = ready_for_password();
            a.pw_busy = true;
            let out = bmcpw::Outcome { changed, verified, saved: Some("/tmp/new-file".into()), problem: (!verified).then(|| "why".into()) };
            a.on_msg(Msg::PasswordChanged(0, users()[0].clone(), out));
            let Mode::Report(lines) = &a.mode else { panic!() };
            assert!(!a.pw_busy);
            assert_eq!(a.want_adopt.is_some(), adopt);
            assert_eq!(a.states[0].halt_auto, !verified);
            if !verified {
                assert!(lines.iter().any(|l| l.contains("/tmp/new-file")) && lines.iter().any(|l| l.contains("Nothing is retried")));
            }
            press(&mut a, KeyCode::Char('y'));
            assert!(matches!(a.mode, Mode::Report(_)));
            press(&mut a, KeyCode::Esc);
            assert!(matches!(a.mode, Mode::Normal));
        }
    }

    #[test]
    fn password_form_fits_the_smallest_terminal() {
        let mut a = ready_for_password();
        for (w, h) in [(40, 12), (80, 24)] {
            a.mode = Mode::Normal;
            ui::draw(&mut a, w, h);
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::ChangeBmcPassword), "button at {w}x{h}");
            let mut form = bmcpw::Form::default();
            for step in [bmcpw::Step::Current, bmcpw::Step::Pick] {
                form.step = step;
                form.users = users();
                form.error = "the two entries do not match".into();
                a.mode = Mode::BmcPassword(form.clone());
                let c = ui::draw(&mut a, w, h);
                let text: String = (0..h).map(|y| c.row(y)).collect();
                assert!(text.contains("do not match"), "{step:?} {w}x{h}");
                assert!(a.hits.iter().any(|(_, k)| *k == Hit::DialogOk) && a.hits.iter().any(|(_, k)| *k == Hit::DialogCancel));
                let fields = if step == bmcpw::Step::Pick { 3 } else { 1 };
                for i in 0..fields {
                    assert!(a.hits.iter().any(|(_, k)| *k == Hit::PwField(i)), "field {i} {step:?} {w}x{h}");
                }
            }
        }
    }

    #[test]
    fn paste_fills_a_field_exactly_and_never_submits() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        a.open_add_host();
        let secret = r#"P@ss w0rd!$'"\;|&y{}#`"#.get(..20).unwrap();
        if let Mode::AddHost(f) = &mut a.mode {
            f.field = FORM_PASSWORD;
        }
        // With the line break a copied line often carries.
        a.paste(&format!("{secret}\r\n"));
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.host.password == secret && f.error.is_empty()));
        assert!(a.want_add.is_none());
    }

    #[test]
    fn paste_that_does_not_fit_changes_nothing() {
        let mut a = two_hosts();
        a.open_add_host();
        if let Mode::AddHost(f) = &mut a.mode {
            f.field = FORM_PASSWORD;
        }
        a.paste("keep");
        for (text, why) in [("x".repeat(17), "at most 20"), ("one\ny".into(), "one line"), ("tab\there".into(), "plain ASCII"), ("p\u{e4}ss".into(), "plain ASCII")] {
            a.paste(&text);
            let Mode::AddHost(f) = &a.mode else { panic!("form closed by {text:?}") };
            assert!(f.host.password == "keep" && f.error.contains(why), "{}", f.error);
            assert!(!f.error.contains(&text), "the text is not echoed");
        }
        // Other fields are longer: a path can be pasted whole.
        if let Mode::AddHost(f) = &mut a.mode {
            f.field = 3;
        }
        let path = format!("/home/someone/{}", "d".repeat(80));
        a.paste(&path);
        assert!(matches!(&a.mode, Mode::AddHost(f) if f.host.pass_file == path));
    }

    #[test]
    fn paste_never_runs_commands_or_confirms() {
        let mut a = app(120, 28);
        open_power(&mut a);
        a.paste("uyq\ny\n");
        assert!(!a.unlocked && !a.quit && a.queue.is_empty() && matches!(a.mode, Mode::Normal));
        unlock(&mut a);
        press(&mut a, KeyCode::Enter); // confirm dialog for the first action
        assert!(matches!(a.mode, Mode::Confirm { .. }));
        a.paste("y");
        assert!(matches!(a.mode, Mode::Confirm { .. }) && a.queue.is_empty());
        a.mode = Mode::Normal;
        a.switch(Screen::Sensors);
        press(&mut a, KeyCode::Char('/'));
        a.paste("CPU1 Temp\n");
        assert_eq!(a.filter, "CPU1 Temp");
    }

    #[test]
    fn paste_reaches_the_console_only_while_typing() {
        let mut a = app(120, 28);
        let rx = with_console(&mut a, b"");
        a.paste("ls -l\r\ncd /tmp\n");
        assert_eq!(sent(&rx), b"ls -l\rcd /tmp\r");
        a.focus = Pane::Details;
        a.paste("reboot\n");
        assert!(sent(&rx).is_empty());
    }

    #[test]
    fn screens_are_in_the_users_order() {
        let titles: Vec<&str> = NAV.iter().map(|s| s.title()).collect();
        assert_eq!(titles, ["Hosts", "Overview", "Inventory", "Sensors", "Fans", "Events", "Console", "Power"]);
    }

    /// Screens whose navigation entry is on screen and clickable.
    fn nav_shown(a: &App) -> Vec<Screen> {
        a.hits.iter().filter_map(|(_, h)| if let Hit::Nav(i) = h { Some(NAV[*i]) } else { None }).collect()
    }

    #[test]
    fn every_screen_is_reachable_on_the_smallest_terminal() {
        // Narrow: the list is an overlay. Short and wide: it is a pane.
        for w in [40, 80, 120] {
            let mut a = app(w, 12);
            a.focus_pane(Pane::Nav);
            ui::draw(&mut a, w, 12);
            assert!(nav_shown(&a).len() < NAV.len(), "the list does not fit at 12 rows");
            // The keyboard walks to the last entry and back to the first.
            for (key, want) in [(KeyCode::Down, Screen::Control), (KeyCode::Up, Screen::Hosts)] {
                for _ in 0..NAV.len() {
                    a.key(key, KeyModifiers::NONE);
                    let c = ui::draw(&mut a, w, 12);
                    assert!(nav_shown(&a).contains(&NAV[a.nav_cursor]), "highlight visible at width {w}");
                    drop(c);
                }
                assert_eq!(NAV[a.nav_cursor], want);
            }
            // The wheel scrolls without moving the highlight, and a click on
            // the entry that came into view opens it.
            let r = a.hits.iter().find(|(_, h)| matches!(h, Hit::Nav(_))).unwrap().0;
            for _ in 0..NAV.len() {
                a.mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: r.x as u16, row: r.y as u16, modifiers: KeyModifiers::NONE });
                ui::draw(&mut a, w, 12);
            }
            assert!(nav_shown(&a).contains(&Screen::Control), "wheel at width {w}");
            assert_eq!(NAV[a.nav_cursor], Screen::Hosts);
            let i = NAV.len() - 1;
            let r = a.hits.iter().find(|(_, h)| *h == Hit::Nav(i)).unwrap().0;
            a.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: r.x as u16, row: r.y as u16, modifiers: KeyModifiers::NONE });
            assert_eq!(a.screen, Screen::Control);
        }
    }

    #[test]
    fn the_open_screen_stays_visible_in_a_short_list() {
        let mut a = app(120, 12);
        for s in NAV {
            a.switch(s);
            ui::draw(&mut a, 120, 12);
            assert!(nav_shown(&a).contains(&s), "{s:?}");
        }
        // With room for everything the list does not stay scrolled.
        ui::draw(&mut a, 120, 28);
        assert_eq!((nav_shown(&a).len(), a.nav_scroll), (NAV.len(), 0));
    }

    #[test]
    fn the_host_in_use_stands_out() {
        let mut a = two_hosts();
        a.switch_host(1);
        a.switch(Screen::Hosts);
        press(&mut a, KeyCode::Up);
        let c = ui::draw(&mut a, 120, 28);
        let at = |a: &App, h: Hit| a.hits.iter().rev().find(|(_, k)| *k == h).unwrap().0;
        let (tab, other) = (at(&a, Hit::Host(1)), at(&a, Hit::Host(0)));
        assert_eq!((c.kind(tab.x + 1, tab.y), c.kind(other.x + 1, other.y)), (view::Kind::Sel, view::Kind::Dim));
        let (selected, in_use) = (at(&a, Hit::Row(1)), at(&a, Hit::Row(2)));
        assert_eq!((c.kind(selected.x + 3, selected.y), c.kind(in_use.x + 3, in_use.y)), (view::Kind::Sel, view::Kind::Head));
    }

    #[test]
    fn second_click_on_a_host_connects() {
        let mut a = two_hosts();
        a.loading.clear();
        a.start_unchosen();
        ui::draw(&mut a, 120, 28);
        // The first row starts selected. One click must not connect.
        click(&mut a, Hit::Row(1));
        assert!(!a.chosen);
        // Clicks on two different rows do not add up, nor does a key between.
        click(&mut a, Hit::Row(2));
        click(&mut a, Hit::Row(1));
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Up);
        click(&mut a, Hit::Row(1));
        assert!(!a.chosen);
        click(&mut a, Hit::Row(2));
        click(&mut a, Hit::Row(2));
        assert!(a.chosen && a.current == 1 && a.screen == Screen::Dash);
    }

    #[test]
    fn connect_button_names_the_host_when_there_is_room() {
        let mut a = two_hosts();
        a.start_unchosen();
        let text = |a: &mut App, w, h| {
            let c = ui::draw(a, w, h);
            (0..h).map(|y| c.row(y)).collect::<String>()
        };
        press(&mut a, KeyCode::Down);
        assert!(text(&mut a, 120, 28).contains("[ Connect to two ]"));
        let narrow = text(&mut a, 40, 12);
        assert!(narrow.contains("[ Connect ]") && narrow.contains("[ Saved pw ]"), "{narrow}");
    }

    #[test]
    fn host_buttons_are_in_the_list_pane_at_every_size() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        for (w, h) in [(40, 12), (80, 24), (97, 24), (120, 28), (160, 40)] {
            ui::draw(&mut a, w, h);
            let main = ui::layout(w, h, a.details_visible(), false).main;
            for k in [Hit::AddHost, Hit::HostPassword, Hit::UseHost] {
                let r = a.hits.iter().find(|(_, x)| *x == k).unwrap_or_else(|| panic!("{k:?} at {w}x{h}")).0;
                if let Some(m) = main {
                    assert!(r.x > m.x && r.x + r.w < m.x + m.w, "{k:?} outside the list at {w}x{h}");
                }
            }
        }
        // With more hosts than rows the buttons stay on screen.
        let base = a.hosts[0].clone();
        a.hosts = (0..40).map(|i| Target { name: Some(format!("h{i}")), ..base.clone() }).collect();
        a.states = vec![HostState::default(); 40];
        a.switch(Screen::Hosts);
        for _ in 0..39 {
            press(&mut a, KeyCode::Down);
        }
        ui::draw(&mut a, 80, 14);
        a.key(KeyCode::Up, KeyModifiers::NONE);
        a.key(KeyCode::Down, KeyModifiers::NONE);
        ui::draw(&mut a, 80, 14);
        assert!(a.hits.iter().any(|(_, k)| *k == Hit::Row(40)) && a.hits.iter().any(|(_, k)| *k == Hit::AddHost));
    }

    #[test]
    fn add_host_fits_small_terminals() {
        let mut a = two_hosts();
        a.switch(Screen::Hosts);
        for (w, h) in [(40, 12), (80, 24)] {
            a.mode = Mode::Normal;
            ui::draw(&mut a, w, h);
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::Key(KeyCode::Char('a'))), "hint at {w}x{h}");
            a.open_add_host();
            ui::draw(&mut a, w, h);
            for i in 0..FORM_FIELDS.len() {
                assert!(a.hits.iter().any(|(_, k)| *k == Hit::Field(i)), "field {i} at {w}x{h}");
            }
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::DialogOk) && a.hits.iter().any(|(_, k)| *k == Hit::DialogCancel));
            // The whole warning is readable, down to its last words.
            let mut host = NewHost { name: "a".into(), address: "h".into(), password: "p".into(), ..Default::default() };
            host.user = "u".into();
            a.mode = Mode::SavePassword { form: HostForm { host, ..Default::default() }, anchor: None };
            let c = ui::draw(&mut a, w, h);
            let text: String = (0..h).map(|y| c.row(y)).collect();
            assert!(text.contains("NOT encrypted") && text.contains("advised"), "{w}x{h}");
        }
    }

    /// Has a refresh of the open screen been started by the clock?
    fn ticks_refresh(a: &mut App, after: u64) -> bool {
        a.loading.clear();
        a.tick(Instant::now() + Duration::from_secs(after));
        a.loading.contains(&a.screen)
    }

    #[test]
    fn nothing_refreshes_by_itself_unless_live() {
        let mut a = app(120, 28);
        for s in NAV {
            a.switch(s);
            assert!(!ticks_refresh(&mut a, 3600), "{s:?}");
        }
        a.switch(Screen::Sensors);
        ui::draw(&mut a, 120, 28);
        click(&mut a, Hit::Live);
        assert!(a.live && a.loading.contains(&Screen::Sensors), "on refreshes at once");
        a.live_every = Duration::from_secs(30);
        assert!(!ticks_refresh(&mut a, 20) && ticks_refresh(&mut a, 31));
        press(&mut a, KeyCode::Char('l'));
        assert!(!a.live && !ticks_refresh(&mut a, 3600));
        // Leaving the screen or the host ends it. Reopening the same one does not.
        press(&mut a, KeyCode::Char('l'));
        a.switch(Screen::Sensors);
        assert!(a.live);
        a.switch(Screen::Dash);
        assert!(!a.live);
        a.switch(Screen::Sel);
        press(&mut a, KeyCode::Char('l'));
        assert!(!a.live && a.status.contains("Overview and Sensors"));
        let mut b = two_hosts();
        press(&mut b, KeyCode::Char('l'));
        press(&mut b, KeyCode::Char('2'));
        assert!(!b.live);
    }

    #[test]
    fn failed_refresh_keeps_old_data_and_says_so() {
        let mut a = app(120, 28);
        a.switch(Screen::Sensors);
        let e = a.epoch;
        a.on_msg(Msg::Data(e, Screen::Sensors, Ok("CPU1 Temp | 01h | ok | 3.1 | 38 C\n".into())));
        let title = ui::draw(&mut a, 120, 28).row(1);
        assert!(title.contains("updated ") && title.contains("[ Live: off ]"), "{title}");
        press(&mut a, KeyCode::Char('l'));
        a.on_msg(Msg::Data(e, Screen::Sensors, Err("timeout after 20 s".into())));
        assert!(!a.live, "a failure ends Live");
        assert!(a.status.contains("not updated") && a.status.contains("timeout"), "{}", a.status);
        let c = ui::draw(&mut a, 120, 28);
        assert!(c.row(1).contains("NOT updated since "), "{}", c.row(1));
        assert!((0..28).any(|y| c.row(y).contains("CPU1 Temp")), "old readings stay");
        a.on_msg(Msg::Data(e, Screen::Sensors, Ok("x | 01h | ok | 3.1 | 1 C\n".into())));
        assert!(!ui::draw(&mut a, 120, 28).row(1).contains("NOT"));
        // An old power state is never kept.
        a.on_msg(Msg::Data(e, Screen::Control, Ok("Chassis Power is on\n".into())));
        a.on_msg(Msg::Data(e, Screen::Control, Err("boom".into())));
        assert!(a.power_state().0.contains("could not be read"));
        // After a login failure Live stays off until r has been used.
        a.on_msg(Msg::Data(e, Screen::Sensors, Err("Unable to establish RMCP+ session".into())));
        press(&mut a, KeyCode::Char('l'));
        assert!(!a.live && a.status.contains("with r first"), "{}", a.status);
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
    fn filter_by_keys_and_mouse() {
        let mut a = app(120, 28);
        a.switch(Screen::Sensors);
        a.data.insert(Screen::Sensors, Ok("CPU1 Temp | 01h | ok | 3.1 | 38 C\nFAN1 | 41h | ns | 7.1 | No Reading\nSystem Temp | 02h | ok | 7.1 | 30 C\n".into()));
        ui::draw(&mut a, 120, 28);
        click(&mut a, Hit::Key(KeyCode::Char('/')));
        // Letters that are commands elsewhere are text here.
        for c in "uq".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert!(!a.quit && matches!(a.mode, Mode::Normal));
        assert_eq!(a.lines().len(), 1, "only the header is left");
        press(&mut a, KeyCode::Backspace);
        press(&mut a, KeyCode::Backspace);
        for c in "temp".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        press(&mut a, KeyCode::Down);
        assert_eq!((a.lines().len(), a.cursor), (3, 2));
        assert!(a.detail_lines(30)[0].0.contains("System Temp"), "details follow the filter");
        press(&mut a, KeyCode::Enter);
        assert!(!a.filtering && a.filter == "temp");
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.lines().len(), 4);
        press(&mut a, KeyCode::Char('/'));
        press(&mut a, KeyCode::Char('x'));
        a.switch(Screen::Fru);
        assert!(a.filter.is_empty() && !a.filtering);
    }

    #[test]
    fn help_opens_and_runs_nothing() {
        let mut a = app(120, 28);
        open_power(&mut a);
        unlock(&mut a);
        click(&mut a, Hit::Key(KeyCode::Char('?')));
        for c in ['y', 'u'] {
            press(&mut a, KeyCode::Char(c));
        }
        assert!(matches!(a.mode, Mode::Help) && a.unlocked && a.queue.is_empty());
        click(&mut a, Hit::DialogCancel);
        assert!(matches!(a.mode, Mode::Normal));
        for (w, h) in [(40, 12), (80, 24), (120, 28)] {
            a.mode = Mode::Help;
            a.help_scroll = 1000;
            let c = ui::draw(&mut a, w, h);
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::DialogCancel));
            assert!((0..h).any(|y| c.row(y).contains("audit log")), "the end is reachable at {w}x{h}");
        }
    }

    #[test]
    fn power_menu_over_the_console() {
        let mut a = app(120, 28);
        a.unlocked = true;
        let rx = with_console(&mut a, b"boot messages");
        ctrl(&mut a, '5');
        press(&mut a, KeyCode::Char('p'));
        assert!(matches!(a.mode, Mode::Power) && a.screen == Screen::Console && a.console_live());
        // Nothing reaches the host while the menu or its dialog is open.
        press(&mut a, KeyCode::Char('x'));
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Down); // Power off (hard)
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Enter);
        assert!(a.queue.is_empty(), "Enter alone must not confirm");
        press(&mut a, KeyCode::Esc);
        assert!(matches!(a.mode, Mode::Power) && a.power_cursor == 2, "back to the menu");
        press(&mut a, KeyCode::Esc);
        assert!(matches!(a.mode, Mode::Normal) && a.queue.is_empty());
        assert!(sent(&rx).is_empty());
        // Closed: Esc belongs to the host again.
        press(&mut a, KeyCode::Esc);
        assert_eq!(sent(&rx), [0x1b]);
    }

    #[test]
    fn power_menu_by_mouse() {
        let mut a = app(120, 28);
        let rx = with_console(&mut a, b"");
        a.focus = Pane::Details;
        ui::draw(&mut a, 120, 28);
        click(&mut a, Hit::Power);
        click(&mut a, Hit::DialogCancel);
        assert!(matches!(a.mode, Mode::Normal));
        click(&mut a, Hit::Power);
        // Clicks outside the menu do nothing.
        click_at(&mut a, 1, 3);
        assert!(matches!(a.mode, Mode::Power));
        // Locked: unlocking returns to the menu and runs nothing.
        click(&mut a, Hit::PowerItem(0));
        click(&mut a, Hit::DialogOk);
        assert!(matches!(a.mode, Mode::Power) && a.unlocked && a.queue.is_empty() && !a.want_connect);
        click(&mut a, Hit::PowerItem(2));
        click(&mut a, Hit::DialogCancel);
        assert!(matches!(a.mode, Mode::Power));
        click(&mut a, Hit::PowerItem(2));
        click(&mut a, Hit::DialogOk);
        assert!(matches!(a.mode, Mode::Normal) && a.focus == Pane::Main && a.typing());
        assert_eq!(a.queue[0].args, ["chassis", "power", "off"]);
        click(&mut a, Hit::Power);
        click(&mut a, Hit::PowerItem(0));
        assert!(matches!(a.mode, Mode::Confirm { .. }) && a.queue.len() == 1, "power on asks too");
        click(&mut a, Hit::DialogOk);
        assert_eq!(a.queue[1].args, ["chassis", "power", "on"]);
        let led = a.power_actions().iter().position(|x| x.label.starts_with("Identify")).unwrap();
        click(&mut a, Hit::Power);
        click(&mut a, Hit::PowerItem(led));
        assert!(matches!(a.mode, Mode::Normal) && a.queue.len() == 3, "the LED runs at once");
        assert!(sent(&rx).is_empty());
    }

    #[test]
    fn power_menu_fits_and_stays_reachable() {
        let mut a = two_hosts();
        let _rx = with_console(&mut a, b"");
        for (w, h) in [(40, 12), (80, 24), (130, 36)] {
            ui::draw(&mut a, w, h);
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::Power), "button at {w}x{h}");
            a.open_power_menu();
            let last = a.power_actions().len() - 1;
            for _ in 0..last {
                a.key(KeyCode::Down, KeyModifiers::NONE);
            }
            ui::draw(&mut a, w, h);
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::PowerItem(last)), "last entry at {w}x{h}");
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::DialogCancel));
            a.mode = Mode::Normal;
        }
        // Zoom has no top bar, so its own line carries the button.
        a.command('z');
        ui::draw(&mut a, 120, 40);
        assert!(a.hits.iter().any(|(_, k)| *k == Hit::Power));
        // The console key hints still show all commands at 80 columns.
        a.command('z');
        ui::draw(&mut a, 80, 24);
        for c in ['.', 'z', '[', 'p', '?'] {
            assert!(a.hits.iter().any(|(_, k)| *k == Hit::Command(c)), "hint {c}");
        }
    }

    #[test]
    fn fan_modes_need_confirmation() {
        assert!(ipmi::fan_actions().iter().all(|x| x.risk == Risk::High));
    }

    #[test]
    fn next_boot_needs_confirmation() {
        let mut a = app(120, 28);
        a.efi_boot = true;
        open_power(&mut a);
        unlock(&mut a);
        a.cursor = a.menu().iter().position(|x| x.label.starts_with("Next boot: CD")).unwrap();
        press(&mut a, KeyCode::Enter);
        assert!(a.queue.is_empty());
        press(&mut a, KeyCode::Char('y'));
        assert_eq!(a.queue[0].args, ["chassis", "bootdev", "cdrom", "options=efiboot"]);
    }

    #[test]
    fn key_hints_keep_help_and_quit() {
        let mut a = app(120, 28);
        for s in NAV {
            a.switch(s);
            for w in [40, 80] {
                ui::draw(&mut a, w, 28);
                for k in ['?', 'q'] {
                    assert!(a.hits.iter().any(|(_, h)| *h == Hit::Key(KeyCode::Char(k))), "{k} on {s:?} at {w}");
                }
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
