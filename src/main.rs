//! tetron-bmctui: proof of concept terminal UI around ipmitool (Supermicro first).

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

const USAGE: &str = "tetron-bmctui: terminal UI around ipmitool (proof of concept, Supermicro first)

usage: tetron-bmctui [-H host] [-U user] [-I iface] [--pass-file FILE]
               [--hosts FILE] [--select NAME] [--ipmitool BIN] [--relock MINUTES] [--dump SCREEN]

  -H host           BMC address (env BMC_HOST). Without it ipmitool runs in-band (/dev/ipmi0).
  -U user           BMC user (env BMC_USER)
  -I iface          lanplus (default), lan
  --pass-file FILE  password file, must be mode 0600 (otherwise set IPMI_PASSWORD)
  --hosts FILE      host list, mode 0600 (default ~/.config/tetron-bmctui/hosts.toml,
                    used when no -H or BMC_HOST is given)
  --select NAME     start with this host of the list
  --ipmitool BIN    ipmitool binary (default: ipmitool)
  --relock MINUTES  lock writes again after this idle time (default 15, 0 never)
  --console-size CxR  emulated console size (default 100x31)
  --console-mono    show the console without colors
  --scrollback N    console lines kept (default 50000, max 200000)
  --prefix CHAR     console command key is Ctrl-CHAR (default ])
  --efi-boot        ask for UEFI with the \"Next boot\" actions
  --export-dir DIR  where e saves screens (default ~/.local/state/tetron-bmctui/exports)
  --dump SCREEN     print dash|sensors|sel|fru|control|fan|hosts as text and exit

The password is never accepted on the command line.
Mouse: click screens, rows, buttons and key hints. The wheel scrolls the pane
under the pointer. Hold Shift to select text with the terminal.
keys: 1-9 host, Tab pane, arrows or j/k move, Enter open or run, u unlock or lock,
      r refresh, p power menu, d details, / filter, e export, ? help, S serial console full screen, q quit.
Console: Enter connects (needs unlock). Then all keys go to the host, except
Ctrl-] followed by a command; Ctrl-] then ? lists them.
Harmless actions run at once after unlock. Sensitive ones ask for confirmation.
Writes are logged in ~/.local/state/tetron-bmctui/audit.log.";

struct Opts {
    /// At least one entry. `current` is the one to start with.
    hosts: Vec<Target>,
    current: usize,
    dump: Option<Screen>,
    relock: Option<Duration>,
    console: console::Settings,
    prefix: u8,
    efi_boot: bool,
    console_mono: bool,
    export_dir: Option<PathBuf>,
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
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
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
    // The default list applies only when no host was named in another way.
    if list.is_none() && t.host.is_none() {
        list = hosts::default_path().filter(|p| p.exists());
    }
    let (hosts, current) = match list {
        Some(path) => {
            if single_flags {
                return Err(format!("-U, -I and --pass-file do not apply to the host list {}", path.display()));
            }
            let all = hosts::load(&path, &t.bin)?;
            let current = match &select {
                Some(n) => all.iter().position(|h| h.name.as_ref() == Some(n)).ok_or(format!("no host named {n} in {}", path.display()))?,
                None => 0,
            };
            (all, current)
        }
        None => {
            if select.is_some() {
                return Err("--select needs a host list".into());
            }
            if let Some(f) = &t.pass_file {
                hosts::check_private(f)?;
            }
            if t.host.is_some() && t.pass_file.is_none() && std::env::var_os("IPMI_PASSWORD").is_none() {
                return Err("set IPMI_PASSWORD or pass --pass-file for LAN access".into());
            }
            (vec![t], 0)
        }
    };
    Ok(Opts { hosts, current, dump, relock, console, prefix, efi_boot, console_mono, export_dir })
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
    let Opts { hosts, current, relock, console, prefix, efi_boot, console_mono, export_dir, .. } = opts;
    let export_dir = export_dir.or_else(|| ipmi::state_dir().map(|d| d.join("exports")));
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();
    let mut app = App::new(hosts, current, relock, tx.clone());
    app.console_settings = console;
    app.prefix = prefix;
    app.efi_boot = efi_boot;
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
    for (l, _) in view::render(s, &view::Ctx { data: Some(&data), vendor, cursor: usize::MAX, loading: false, hosts, current, efi_boot: false }) {
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
