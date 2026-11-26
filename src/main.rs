//! tetron-bmctui: terminal UI for server BMCs around ipmitool (Supermicro first).

mod app;
mod canvas;
mod console;
mod export;
mod hosts;
mod ipmi;
mod ui;
mod view;

use app::{App, Msg};
use crossterm::{
    cursor,
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute, queue,
    terminal,
};
use ipmi::{Action, Screen, Target};
use std::io::{self, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Default idle time before writes lock again, in minutes.
const RELOCK_MINUTES: u64 = 15;

const USAGE: &str = "tetron-bmctui: terminal UI for server BMCs, using ipmitool (Supermicro first)

usage: tetron-bmctui [connection] [options]
       tetron-bmctui [connection] --dump SCREEN

Connection: one host from the command line, or a host list. Without -H,
BMC_HOST or --select the program starts on the host list and contacts nothing
until a host is chosen. With no saved hosts the list offers this machine's own
BMC (/dev/ipmi0).
  -H host             BMC address (env BMC_HOST)
  -U user             BMC user (env BMC_USER)
  -I iface            lanplus (default) or lan
  --pass-file FILE    file holding only the password, mode 0600
                      (or set IPMI_PASSWORD)
  --hosts FILE        host list, mode 0600. The default,
                      ~/.config/tetron-bmctui/hosts.toml, is used when it
                      exists. With -H or BMC_HOST that host comes first and
                      the saved ones can be picked too
  --select NAME       start with this host of the list
The password is never accepted on the command line.

Options:
  --relock MINUTES    lock writes after this idle time (default 15, 0 never)
  --refresh SECONDS   start with Live on, refreshing every SECONDS, 5 to 3600
                      (default 0: off. When turned on by hand, every 30)
  --console-size CxR  size of the emulated console (default 100x31)
  --console-mono      show the console without colors
  --scrollback N      console lines kept (default 50000, max 200000)
  --prefix CHAR       the console command key is Ctrl-CHAR (default ])
  --efi-boot          ask for UEFI with the \"Next boot\" actions
  --export-dir DIR    where e saves screens
                      (default ~/.local/state/tetron-bmctui/exports)
  --ipmitool BIN      ipmitool binary (default: ipmitool)
  --dump SCREEN       print dash, sensors, sel, fru, control, fan or hosts
                      as text and exit
  -h, --help          show this help
  -V, --version       show the version

Keys (? lists them in the program):
  Tab, Shift-Tab      next or previous pane
  arrows, j, k        move
  Enter               open, run or connect
  u                   unlock or lock writes
  p                   power menu
  1 to 9              switch host
  /  e  r  d          filter, export, refresh, details
  l                   Live: refresh Overview or Sensors repeatedly
  S                   full screen serial console
  a, w                add a host, change its saved password (Hosts screen)
  q                   quit

Serial console: Enter on the Console screen connects, after unlock. Keys then
go to the host. The command key (Ctrl-]) followed by . disconnects, z zooms,
[ scrolls back, p opens the power menu, Tab leaves the pane, ? shows help.
Serial over LAN does not replay output: if the screen is blank, press a key.

Mouse: click screens, hosts, rows, buttons and key hints. The wheel scrolls.
Hold Shift to select text with the terminal.

Safety: the program starts read-only. After unlock only the identify LED runs
without confirmation. A connected console stays usable after an idle relock.
Actions and console sessions are logged in
~/.local/state/tetron-bmctui/audit.log. Keystrokes are not logged.";

struct Opts {
    /// At least one entry. `current` is the one to start with.
    hosts: Vec<Target>,
    current: usize,
    dump: Option<Screen>,
    relock: Option<Duration>,
    console: console::Settings,
    prefix: u8,
    efi_boot: bool,
    /// Seconds between Live refreshes when asked for at startup. 0 is off.
    refresh: u64,
    /// Whether a host was named. If not, the program starts on the host list.
    chosen: bool,
    /// A problem to show once the program runs.
    warning: Option<String>,
    /// Where added hosts are saved: the list in use, or the default one.
    list_path: Option<PathBuf>,
    console_mono: bool,
    export_dir: Option<PathBuf>,
}

/// Linux device names of a local BMC, the ones ipmitool tries.
fn has_local_bmc() -> bool {
    ["/dev/ipmi0", "/dev/ipmi/0", "/dev/ipmidev/0"].iter().any(|p| Path::new(p).exists())
}

/// "80x24" -> (80, 24), within sizes a console can sensibly have.
fn parse_size(s: &str) -> Option<(NonZeroU16, NonZeroU16)> {
    let (c, r) = s.split_once('x')?;
    let (c, r): (u16, u16) = (c.parse().ok()?, r.parse().ok()?);
    if !(40..=500).contains(&c) || !(10..=200).contains(&r) {
        return None;
    }
    Some((NonZeroU16::new(c)?, NonZeroU16::new(r)?))
}

/// The command key must be a Ctrl combination that terminals can tell apart
/// from Tab, Enter, Backspace and Esc.
fn parse_prefix(s: &str) -> Option<u8> {
    let mut it = s.chars();
    let (c, None) = (it.next()?, it.next()) else { return None };
    console::ctrl_byte(c).filter(|b| !c.is_ascii_digit() && ![0, 8, 9, 10, 13, 27].contains(b))
}

fn parse_args() -> Result<Opts, String> {
    let mut t = Target {
        bin: "ipmitool".into(),
        name: None,
        host: std::env::var("BMC_HOST").ok().filter(|s| !s.is_empty()),
        user: std::env::var("BMC_USER").ok().filter(|s| !s.is_empty()),
        iface: "lanplus".into(),
        pass_file: None,
    };
    let mut dump = None;
    let (mut list, mut select, mut flag_h) = (None, None, false);
    // -U, -I and --pass-file describe the single host of the command line.
    let mut single_flags = false;
    let mut minutes = RELOCK_MINUTES;
    let mut console = console::Settings::default();
    let mut prefix = app::DEFAULT_PREFIX;
    let (mut efi_boot, mut export_dir) = (false, None);
    let mut console_mono = false;
    let mut refresh = 0;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("tetron-bmctui {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-H" => {
                t.host = Some(val("-H")?);
                flag_h = true;
            }
            "-U" => {
                t.user = Some(val("-U")?);
                single_flags = true;
            }
            "-I" => {
                t.iface = val("-I")?;
                single_flags = true;
            }
            "--pass-file" => {
                t.pass_file = Some(val("--pass-file")?);
                single_flags = true;
            }
            "--hosts" => list = Some(PathBuf::from(val("--hosts")?)),
            "--select" => select = Some(val("--select")?),
            "--ipmitool" => t.bin = val("--ipmitool")?,
            // Idle relock time. 0 turns it off.
            "--relock" => minutes = val("--relock")?.parse().ok().filter(|m| *m <= 100_000).ok_or("--relock needs a number of minutes")?,
            "--console-size" => (console.cols, console.rows) = parse_size(&val("--console-size")?).ok_or("--console-size needs COLSxROWS, 40x10 to 500x200")?,
            "--scrollback" => console.scrollback = val("--scrollback")?.parse().ok().filter(|n| *n <= 200_000).ok_or("--scrollback needs a number up to 200000")?,
            "--prefix" => prefix = parse_prefix(&val("--prefix")?).ok_or("--prefix needs one of a-z (not h i j m) or \\ ] ^ _")?,
            "--efi-boot" => efi_boot = true,
            // At least 5 s: a BMC answers slowly.
            "--refresh" => refresh = val("--refresh")?.parse().ok().filter(|n| *n == 0 || (5..=3600).contains(n)).ok_or("--refresh needs 0, or 5 to 3600 seconds")?,
            "--console-mono" => console_mono = true,
            "--export-dir" => export_dir = Some(PathBuf::from(val("--export-dir")?)),
            "--dump" => {
                let n = val("--dump")?;
                dump = Some(Screen::from_name(&n).ok_or(format!("unknown screen {n}"))?);
            }
            "-P" | "-p" | "--password" => return Err("passwords are not accepted on the command line (use IPMI_PASSWORD or --pass-file)".into()),
            _ => return Err(format!("unknown argument {a}\n\n{USAGE}")),
        }
    }
    let relock = (minutes > 0).then(|| Duration::from_secs(minutes * 60));
    if list.is_some() && flag_h {
        return Err("use either --hosts or -H".into());
    }
    // The default list applies when no host was named in another way. With a
    // host from the command line its hosts are added after that one.
    if list.is_none() && t.host.is_none() {
        list = hosts::default_path().filter(|p| p.exists());
    }
    let mut warning = None;
    let list_path = list.clone().or_else(hosts::default_path);
    // With no host named, the program asks which one before contacting any.
    // A host from the command line, or --select, is the choice already.
    let chosen = t.host.is_some() || select.is_some();
    let (hosts, current) = match list {
        Some(path) => {
            if single_flags {
                return Err(format!("-U, -I and --pass-file do not apply to the host list {}", path.display()));
            }
            let mut all = hosts::load(&path, &t.bin)?;
            // The BMC of this machine can be picked too, if there is one.
            if has_local_bmc() {
                all.push(t.clone());
            }
            let current = match &select {
                Some(n) => all.iter().position(|h| h.name.as_ref() == Some(n)).ok_or(format!("no host named {n} in {}", path.display()))?,
                None => 0,
            };
            (all, current)
        }
        None => {
            if let Some(f) = &t.pass_file {
                hosts::check_private(f)?;
            }
            if t.host.is_some() && t.pass_file.is_none() && std::env::var_os("IPMI_PASSWORD").is_none() {
                return Err("set IPMI_PASSWORD or pass --pass-file for LAN access".into());
            }
            let mut all = vec![t];
            if all[0].host.is_some()
                && let Some(path) = hosts::default_path().filter(|p| p.exists())
            {
                // The named host must work also when the list is broken.
                match hosts::load(&path, &all[0].bin) {
                    Ok(saved) => all.extend(saved),
                    Err(e) => warning = Some(format!("saved hosts not loaded: {e}")),
                }
            }
            let current = match &select {
                Some(n) => all.iter().position(|h| h.name.as_ref() == Some(n)).ok_or(format!("--select: no saved host named {n}"))?,
                None => 0,
            };
            (all, current)
        }
    };
    Ok(Opts { hosts, current, dump, relock, console, prefix, efi_boot, console_mono, export_dir, list_path, refresh, warning, chosen })
}

/// Raw mode, alternate screen and mouse capture. Dropping the guard restores
/// the terminal, also when the program panics.
struct TermGuard;

/// Switch the terminal to the state the UI needs.
fn term_setup() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide, EnableMouseCapture)
}

/// Give the terminal back, for exit and for the full screen console.
fn term_restore() {
    let _ = execute!(io::stdout(), DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

impl TermGuard {
    fn enter() -> io::Result<TermGuard> {
        // The guard exists before setup, so a partial setup is undone too.
        let g = TermGuard;
        term_setup()?;
        Ok(g)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        term_restore();
    }
}

/// Run one action in a worker thread. Nothing is run without an audit record.
fn execute_action(app: &mut App, tx: &Sender<Msg>, a: Action) {
    let host = app.t.label();
    let who = app.t.describe();
    let line = format!("run {}", a.args.join(" "));
    if let Err(e) = ipmi::audit(&who, &line) {
        app.status = format!("refused, cannot write the audit log: {e}");
        return;
    }
    app.status = format!("running {} ...", a.label);
    let (t, tx, epoch) = (app.t.clone(), tx.clone(), app.epoch);
    std::thread::spawn(move || {
        let r = t.run(&a.args, Duration::from_secs(60));
        let res = match &r {
            Ok(_) => "result=ok".to_string(),
            Err(e) => format!("result=error {}", e.lines().next().unwrap_or("")),
        };
        let _ = ipmi::audit(&who, &format!("done {} {res}", a.args.join(" ")));
        let _ = tx.send(Msg::Done(epoch, host, a.label, r));
    });
}

/// Write a new host to the list. Typed passwords always go below the default
/// configuration directory, not beside a list kept somewhere else.
fn add_host(app: &mut App, form: app::HostForm, list: Option<&Path>) {
    let pass_dir = hosts::default_path().and_then(|p| p.parent().map(|d| d.join("pass")));
    let (Some(list), Some(pass_dir)) = (list, pass_dir) else {
        app.host_added(form, Err("no HOME for the host list".into()));
        return;
    };
    if form.change.is_some() {
        let r = hosts::set_password(list, &pass_dir, &form.host, &app.t.bin);
        let changed = r.as_ref().ok().map(|c| c.target.describe());
        app.password_changed(form, r);
        if let Some(who) = changed {
            let _ = ipmi::audit(&who, "saved password changed in the host list");
        }
        return;
    }
    let r = hosts::add(list, &pass_dir, &form.host, &app.t.bin);
    let added = r.as_ref().ok().map(|t| t.describe());
    app.host_added(form, r);
    if let Some(who) = added {
        // Only the fact is logged, never what was typed besides name and address.
        let _ = ipmi::audit(&who, "added to the host list");
        app.status = format!("added {who} to {}", list.display());
    }
}

/// Save the open screen, as filtered, and say where.
fn export_screen(app: &mut App, dir: Option<&Path>) {
    let Some(dir) = dir else {
        app.status = "export: no HOME, use --export-dir".into();
        return;
    };
    let lines: Vec<String> = app.lines().into_iter().map(|l| l.0).collect();
    app.status = match export::write(dir, &app.t.label(), app.screen.title(), &lines) {
        // The name comes first: the status line can cut off a long path.
        Ok(p) => format!("saved {} in {}", p.file_name().unwrap_or_default().to_string_lossy(), p.parent().unwrap_or(dir).display()),
        Err(e) => format!("export failed: {e}"),
    };
}

/// Record and start the session of the Console pane.
fn connect_console(app: &mut App) {
    let r = ipmi::audit(&app.t.describe(), "sol activate (pane)")
        .map_err(|e| format!("refused, cannot write the audit log: {e}"))
        .and_then(|()| console::Console::spawn(&app.t, &app.console_settings).map_err(|e| format!("{}: {e}", app.t.bin)));
    app.connected(r);
}

/// Leave the UI and hand the terminal to `ipmitool sol activate`.
fn run_sol(t: &Target) -> io::Result<()> {
    term_restore();
    let host = t.describe();
    match ipmi::audit(&host, "sol activate") {
        Err(e) => println!("refused, cannot write the audit log: {e}"),
        Ok(()) => {
            println!("Serial over LAN to {host}. End the session with Enter, then ~ and . (tilde dot).");
            let st = Command::new(&t.bin).args(t.cli_args(&["sol".into(), "activate".into()])).status();
            let _ = ipmi::audit(&host, &format!("sol ended {:?}", st.map(|s| s.code())));
        }
    }
    term_setup()
}

/// Draw one frame. Synchronized update keeps terminals that support it from
/// showing a half drawn screen.
fn redraw(app: &mut App, out: &mut impl Write) -> io::Result<()> {
    let (w, h) = terminal::size()?;
    let frame = ui::draw(app, w as usize, h as usize);
    queue!(out, terminal::BeginSynchronizedUpdate)?;
    frame.flush(out)?;
    execute!(out, terminal::EndSynchronizedUpdate)
}

fn run(opts: Opts) -> io::Result<()> {
    let Opts { hosts, current, relock, console, prefix, efi_boot, console_mono, export_dir, list_path, refresh, warning, chosen, .. } = opts;
    let export_dir = export_dir.or_else(|| ipmi::state_dir().map(|d| d.join("exports")));
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let mut app = App::new(hosts, current, relock, tx.clone());
    app.console_settings = console;
    app.prefix = prefix;
    app.efi_boot = efi_boot;
    if let Some(w) = warning {
        app.status = w;
    }
    if refresh > 0 {
        app.live_every = Duration::from_secs(refresh);
        app.live = chosen;
    }
    if !chosen {
        app.start_unchosen();
    }
    app.console_mono = console_mono;
    // Threads that are ending console sessions.
    let mut closers: Vec<JoinHandle<()>> = vec![];
    app.start_vendor();
    app.refresh(Screen::Dash);
    let _g = TermGuard::enter()?;
    let mut out = io::stdout();
    redraw(&mut app, &mut out)?;
    while !app.quit {
        let mut dirty = false;
        // The idle check comes before input: a key pressed after the limit must
        // find the writes locked.
        dirty |= app.tick(Instant::now());
        // Echo of typed keys has to show quickly while a console is open.
        let wait = if app.console_live() { 20 } else { 200 };
        if event::poll(Duration::from_millis(wait))? {
            dirty |= app.tick(Instant::now());
            // One event per frame: the clickable regions always belong to what
            // is on screen. Pointer movement does not cause a redraw.
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    app.key(k.code, k.modifiers);
                    dirty = true;
                }
                Event::Mouse(m) => dirty |= app.mouse(m),
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        while let Ok(m) = rx.try_recv() {
            app.on_msg(m);
            dirty = true;
        }
        for a in std::mem::take(&mut app.queue) {
            execute_action(&mut app, &tx, a);
            dirty = true;
        }
        if let Some(form) = app.want_add.take() {
            add_host(&mut app, form, list_path.as_deref());
            dirty = true;
        }
        if app.want_export {
            app.want_export = false;
            export_screen(&mut app, export_dir.as_deref());
            dirty = true;
        }
        for c in std::mem::take(&mut app.closing) {
            closers.push(std::thread::spawn(move || c.close()));
        }
        closers.retain(|c| !c.is_finished());
        // A new session waits for the old one: its deactivate would otherwise
        // end the new session.
        if app.want_connect && closers.is_empty() {
            app.want_connect = false;
            connect_console(&mut app);
            dirty = true;
        }
        if app.want_sol && closers.is_empty() {
            app.want_sol = false;
            run_sol(&app.t)?;
            // Time spent in the console does not count as idle.
            app.touch();
            dirty = true;
        }
        if dirty {
            redraw(&mut app, &mut out)?;
        }
    }
    // Leave no session behind on the BMC.
    app.disconnect();
    for c in std::mem::take(&mut app.closing) {
        closers.push(std::thread::spawn(move || c.close()));
    }
    drop(_g);
    if !closers.is_empty() {
        println!("closing the console session ...");
    }
    for c in closers {
        let _ = c.join();
    }
    Ok(())
}

fn dump(hosts: &[Target], current: usize, s: Screen) -> i32 {
    let t = &hosts[current];
    let vendor = if s == Screen::Fan { ipmi::fetch_vendor(t) } else { None };
    let data = ipmi::fetch(t, s);
    let failed = data.is_err();
    for (l, _) in view::render(s, &view::Ctx { data: Some(&data), vendor, cursor: usize::MAX, loading: false, hosts, current, lan: None, efi_boot: false }) {
        // The same filter as on screen: BMC text never reaches the terminal raw.
        println!("{}", l.chars().map(canvas::safe_char).collect::<String>());
    }
    if failed { 1 } else { 0 }
}

fn main() {
    match parse_args() {
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
        Ok(Opts { hosts, current, dump: Some(s), .. }) => std::process::exit(dump(&hosts, current, s)),
        Ok(opts) => {
            if let Err(e) = run(opts) {
                eprintln!("tetron-bmctui: {e}");
                std::process::exit(1);
            }
        }
    }
}
