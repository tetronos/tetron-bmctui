//! bmc-tui: proof of concept terminal UI around ipmitool (Supermicro first).

mod ipmi;
mod view;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};
use ipmi::{Action, Risk, Screen, Target};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use view::Kind;

const USAGE: &str = "bmc-tui: terminal UI around ipmitool (proof of concept, Supermicro first)

usage: bmc-tui [-H host] [-U user] [-I iface] [--pass-file FILE] [--ipmitool BIN] [--dump SCREEN]

  -H host           BMC address (env BMC_HOST). Without it ipmitool runs in-band (/dev/ipmi0).
  -U user           BMC user (env BMC_USER)
  -I iface          lanplus (default), lan
  --pass-file FILE  password file, must be mode 0600 (otherwise set IPMI_PASSWORD)
  --ipmitool BIN    ipmitool binary (default: ipmitool)
  --dump SCREEN     print dash|sensors|sel|fru|control|fan as text and exit

The password is never accepted on the command line.
keys: 1-6 or Tab screens, j/k scroll or move, Enter run item, r refresh,
      u unlock writes, S serial console, q quit. Writes are logged in
      ~/.local/state/bmc-tui/audit.log.";

struct Opts {
    t: Target,
    dump: Option<Screen>,
}

fn parse_args() -> Result<Opts, String> {
    let mut t = Target {
        bin: "ipmitool".into(),
        host: std::env::var("BMC_HOST").ok().filter(|s| !s.is_empty()),
        user: std::env::var("BMC_USER").ok().filter(|s| !s.is_empty()),
        iface: "lanplus".into(),
        pass_file: None,
    };
    let mut dump = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-H" => t.host = Some(val("-H")?),
            "-U" => t.user = Some(val("-U")?),
            "-I" => t.iface = val("-I")?,
            "--pass-file" => t.pass_file = Some(val("--pass-file")?),
            "--ipmitool" => t.bin = val("--ipmitool")?,
            "--dump" => {
                let n = val("--dump")?;
                dump = Some(Screen::from_name(&n).ok_or(format!("unknown screen {n}"))?);
            }
            "-P" | "-p" | "--password" => return Err("passwords are not accepted on the command line (use IPMI_PASSWORD or --pass-file)".into()),
            _ => return Err(format!("unknown argument {a}\n\n{USAGE}")),
        }
    }
    if let Some(f) = &t.pass_file {
        let m = std::fs::metadata(f).map_err(|e| format!("{f}: {e}"))?;
        if m.mode() & 0o077 != 0 {
            return Err(format!("{f}: password file must not be readable by group or others (chmod 600)"));
        }
    }
    if t.host.is_some() && t.pass_file.is_none() && std::env::var_os("IPMI_PASSWORD").is_none() {
        return Err("set IPMI_PASSWORD or pass --pass-file for LAN access".into());
    }
    Ok(Opts { t, dump })
}

enum Mode {
    Normal,
    Confirm { act: Action, typed: String, need: String },
}

enum Msg {
    Data(Screen, Result<String, String>),
    Done(String, Result<String, String>),
    Vendor(Option<u32>),
}

struct App {
    t: Target,
    screen: Screen,
    data: HashMap<Screen, Result<String, String>>,
    loading: HashSet<Screen>,
    last: HashMap<Screen, Instant>,
    scroll: usize,
    cursor: usize,
    unlocked: bool,
    mode: Mode,
    status: String,
    vendor: Option<u32>,
    halt_auto: bool,
    want_sol: bool,
    quit: bool,
    tx: Sender<Msg>,
}

impl App {
    fn new(t: Target, tx: Sender<Msg>) -> App {
        App {
            t,
            screen: Screen::Dash,
            data: HashMap::new(),
            loading: HashSet::new(),
            last: HashMap::new(),
            scroll: 0,
            cursor: 0,
            unlocked: false,
            mode: Mode::Normal,
            status: "read-only. u unlocks writes.".into(),
            vendor: None,
            halt_auto: false,
            want_sol: false,
            quit: false,
            tx,
        }
    }

    fn refresh(&mut self, s: Screen) {
        if !self.loading.insert(s) {
            return;
        }
        self.last.insert(s, Instant::now());
        let (t, tx) = (self.t.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Data(s, ipmi::fetch(&t, s)));
        });
    }

    fn start_vendor(&self) {
        let (t, tx) = (self.t.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Vendor(ipmi::fetch_vendor(&t)));
        });
    }

    fn lines(&self) -> Vec<view::Line> {
        view::render(
            self.screen,
            &view::Ctx { data: self.data.get(&self.screen), vendor: self.vendor, cursor: self.cursor, loading: self.loading.contains(&self.screen) },
        )
    }

    fn menu(&self) -> Vec<Action> {
        match self.screen {
            Screen::Control => ipmi::control_actions(),
            Screen::Fan => ipmi::fan_actions(),
            _ => vec![],
        }
    }

    fn switch(&mut self, s: Screen) {
        self.screen = s;
        self.scroll = 0;
        self.cursor = 0;
        let stale = self.last.get(&s).is_none_or(|t| t.elapsed() > Duration::from_secs(5));
        if stale && !self.halt_auto {
            self.refresh(s);
        }
    }

    fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Data(s, r) => {
                self.loading.remove(&s);
                if let Err(e) = &r
                    && ipmi::looks_like_auth_error(e)
                {
                    self.halt_auto = true;
                    self.status = "connection or login failed: auto refresh stopped (wrong password can lock the BMC account). r retries once.".into();
                }
                self.data.insert(s, r);
            }
            Msg::Vendor(v) => self.vendor = v,
            Msg::Done(label, r) => {
                self.status = match r {
                    Ok(o) => format!("{label}: ok {}", o.trim().lines().next().unwrap_or("")),
                    Err(e) => format!("{label}: FAILED {e}"),
                };
                for s in [Screen::Control, Screen::Fan, Screen::Dash, Screen::Sel] {
                    self.data.remove(&s);
                }
                let s = self.screen;
                self.refresh(s);
            }
        }
    }

    fn tick(&mut self) {
        if self.halt_auto || !matches!(self.screen, Screen::Dash | Screen::Sensors) {
            return;
        }
        let s = self.screen;
        if self.last.get(&s).is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
            self.refresh(s);
        }
    }

    fn pick(&mut self) {
        let items = self.menu();
        let Some(a) = items.get(self.cursor).cloned() else { return };
        if self.screen == Screen::Fan && self.vendor != Some(ipmi::SUPERMICRO_ID) {
            self.status = "fan control is only enabled for Supermicro BMCs".into();
            return;
        }
        if !self.unlocked {
            self.status = "locked: press u to unlock writes".into();
            return;
        }
        let need = if a.risk == Risk::High { self.t.label() } else { "y".into() };
        self.mode = Mode::Confirm { act: a, typed: String::new(), need };
    }

    fn execute(&mut self, a: Action) {
        let host = self.t.label();
        let line = format!("run {}", a.args.join(" "));
        if let Err(e) = ipmi::audit(&host, &line) {
            self.status = format!("refused, cannot write the audit log: {e}");
            return;
        }
        self.status = format!("running {} ...", a.label);
        let (t, tx) = (self.t.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let r = t.run(&a.args, Duration::from_secs(60));
            let res = match &r {
                Ok(_) => "result=ok".to_string(),
                Err(e) => format!("result=error {}", e.lines().next().unwrap_or("")),
            };
            let _ = ipmi::audit(&host, &format!("done {} {res}", a.args.join(" ")));
            let _ = tx.send(Msg::Done(a.label, r));
        });
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if let Mode::Confirm { act, typed, need } = &mut self.mode {
            match code {
                KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "cancelled".into();
                }
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Char(c) => typed.push(c),
                KeyCode::Enter => {
                    let ok = if need == "y" { typed.eq_ignore_ascii_case("y") } else { typed == need };
                    let a = act.clone();
                    self.mode = Mode::Normal;
                    if ok {
                        self.execute(a);
                    } else {
                        self.status = "confirmation did not match, nothing was run".into();
                    }
                }
                _ => {}
            }
            return;
        }
        let menu_len = self.menu().len();
        let max = self.lines().len().saturating_sub(1);
        match code {
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(d @ '1'..='6') => self.switch(Screen::ALL[d as usize - '1' as usize]),
            KeyCode::Tab => {
                let i = Screen::ALL.iter().position(|s| *s == self.screen).unwrap_or(0);
                self.switch(Screen::ALL[(i + 1) % Screen::ALL.len()]);
            }
            KeyCode::BackTab => {
                let i = Screen::ALL.iter().position(|s| *s == self.screen).unwrap_or(0);
                self.switch(Screen::ALL[(i + Screen::ALL.len() - 1) % Screen::ALL.len()]);
            }
            KeyCode::Char('r') => {
                self.halt_auto = false;
                let s = self.screen;
                self.refresh(s);
            }
            KeyCode::Char('u') => {
                self.unlocked = !self.unlocked;
                self.status = if self.unlocked { "WRITES UNLOCKED: power, fan and log changes are possible" } else { "read-only" }.into();
            }
            KeyCode::Char('S') => {
                if self.unlocked {
                    self.want_sol = true;
                } else {
                    self.status = "locked: press u to unlock, the console can type into the host".into();
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if menu_len > 0 {
                    self.cursor = (self.cursor + 1).min(menu_len - 1);
                } else {
                    self.scroll = (self.scroll + 1).min(max);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if menu_len > 0 {
                    self.cursor = self.cursor.saturating_sub(1);
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            KeyCode::PageDown => self.scroll = (self.scroll + 10).min(max),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => self.scroll = max,
            KeyCode::Enter => self.pick(),
            _ => {}
        }
    }

    fn draw(&self, out: &mut impl Write) -> io::Result<()> {
        let (w, h) = terminal::size()?;
        let (w, h) = (w as usize, h as usize);
        queue!(out, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
        let vendor = match self.vendor {
            Some(ipmi::SUPERMICRO_ID) => "Supermicro".to_string(),
            Some(id) => format!("vendor {id}"),
            None => "vendor ?".into(),
        };
        let lock = if self.unlocked { "WRITES UNLOCKED" } else { "read-only" };
        put(out, 0, &format!(" bmc-tui  {}  {}  [{}]", self.t.label(), vendor, lock), w, if self.unlocked { Kind::Warn } else { Kind::Head })?;
        queue!(out, cursor::MoveTo(0, 1))?;
        for (i, s) in Screen::ALL.iter().enumerate() {
            let tab = format!(" {} {} ", i + 1, s.title());
            if *s == self.screen {
                queue!(out, SetAttribute(Attribute::Reverse), Print(&tab), SetAttribute(Attribute::Reset))?;
            } else {
                queue!(out, Print(&tab))?;
            }
        }
        if h < 8 {
            return out.flush();
        }
        let body_h = h - 5;
        let lines = self.lines();
        for (i, (text, kind)) in lines.iter().skip(self.scroll).take(body_h).enumerate() {
            put(out, 3 + i, text, w, *kind)?;
        }
        let prompt = match &self.mode {
            Mode::Confirm { act, typed, need } => {
                if need == "y" {
                    format!(" {}: type y and Enter (Esc cancels): {typed}", act.label)
                } else {
                    format!(" {}: type the host name \"{need}\" and Enter (Esc cancels): {typed}", act.label)
                }
            }
            Mode::Normal => format!(" {}", self.status),
        };
        let pk = if matches!(self.mode, Mode::Confirm { .. }) { Kind::Warn } else { Kind::Normal };
        put(out, h - 2, &prompt, w, pk)?;
        put(out, h - 1, " 1-6 screens  j/k move  Enter run  r refresh  u unlock  S console  q quit", w, Kind::Dim)?;
        out.flush()
    }
}

fn put(out: &mut impl Write, row: usize, text: &str, w: usize, kind: Kind) -> io::Result<()> {
    let t: String = text.replace('\t', "    ").chars().take(w).collect();
    queue!(out, cursor::MoveTo(0, row as u16))?;
    match kind {
        Kind::Normal => queue!(out, Print(t))?,
        Kind::Head => queue!(out, SetAttribute(Attribute::Bold), Print(t), SetAttribute(Attribute::Reset))?,
        Kind::Good => queue!(out, SetForegroundColor(Color::Green), Print(t), ResetColor)?,
        Kind::Warn => queue!(out, SetForegroundColor(Color::Yellow), Print(t), ResetColor)?,
        Kind::Bad => queue!(out, SetForegroundColor(Color::Red), Print(t), ResetColor)?,
        Kind::Dim => queue!(out, SetAttribute(Attribute::Dim), Print(t), SetAttribute(Attribute::Reset))?,
        Kind::Sel => queue!(out, SetAttribute(Attribute::Reverse), Print(t), SetAttribute(Attribute::Reset))?,
    }
    Ok(())
}

struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<TermGuard> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn run_sol(t: &Target) {
    let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    let host = t.label();
    match ipmi::audit(&host, "sol activate") {
        Err(e) => println!("refused, cannot write the audit log: {e}"),
        Ok(()) => {
            println!("Serial over LAN to {host}. End the session with Enter, then ~ and . (tilde dot).");
            let st = Command::new(&t.bin).args(t.cli_args(&["sol".into(), "activate".into()])).status();
            let _ = ipmi::audit(&host, &format!("sol ended {:?}", st.map(|s| s.code())));
        }
    }
    let _ = terminal::enable_raw_mode();
    let _ = execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide);
}

fn run(t: Target) -> io::Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let mut app = App::new(t, tx);
    app.start_vendor();
    app.refresh(Screen::Dash);
    let _g = TermGuard::enter()?;
    let mut out = io::stdout();
    app.draw(&mut out)?;
    while !app.quit {
        let mut dirty = false;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    app.key(k.code, k.modifiers);
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
            dirty = true;
        }
        app.tick();
        if app.want_sol {
            app.want_sol = false;
            run_sol(&app.t);
            dirty = true;
        }
        if dirty {
            app.draw(&mut out)?;
        }
    }
    Ok(())
}

fn dump(t: &Target, s: Screen) -> i32 {
    let vendor = if s == Screen::Fan { ipmi::fetch_vendor(t) } else { None };
    let data = ipmi::fetch(t, s);
    let failed = data.is_err();
    for (l, _) in view::render(s, &view::Ctx { data: Some(&data), vendor, cursor: usize::MAX, loading: false }) {
        println!("{l}");
    }
    if failed { 1 } else { 0 }
}

fn main() {
    match parse_args() {
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
        Ok(Opts { t, dump: Some(s) }) => std::process::exit(dump(&t, s)),
        Ok(Opts { t, dump: None }) => {
            if let Err(e) = run(t) {
                eprintln!("bmc-tui: {e}");
                std::process::exit(1);
            }
        }
    }
}
