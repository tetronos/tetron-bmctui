//! One serial over LAN session shown in a pane: `ipmitool sol activate` runs in
//! a pseudo-terminal, and a terminal emulator turns its output into a grid of
//! cells plus scrollback.
//!
//! Host output is untrusted, since anything on the host can print. It never
//! reaches the real terminal as bytes: the emulator parses it, and the UI draws
//! only the resulting cells. The emulator also never sends replies back, so
//! output can not make the program type into the host.
//!
//! Keystrokes are never logged.

use crate::ipmi::{self, Target};
use crossterm::event::{KeyCode, KeyModifiers};
use std::fs::File;
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// What ipmitool prints once the BMC has accepted the session. Only a session
/// that got this far is ours to deactivate.
const OPERATIONAL: &[u8] = b"SOL Session operational";
/// What ipmitool prints when another session holds the console.
const BUSY: &str = "already active";
/// Keystroke packets that may wait for a stalled ipmitool before more are
/// dropped. The UI must never block on a write.
const INPUT_QUEUE: usize = 1024;

/// Emulated screen size and scrollback length.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub cols: NonZeroU16,
    pub rows: NonZeroU16,
    pub scrollback: usize,
}

impl Default for Settings {
    /// 80x24 is what BIOS console redirection expects.
    fn default() -> Settings {
        Settings { cols: vt100::Parser::DEFAULT_COLS, rows: vt100::Parser::DEFAULT_ROWS, scrollback: 50_000 }
    }
}

/// State shared with the reader thread.
#[derive(Default)]
struct Flags {
    /// Output arrived since the last redraw.
    dirty: AtomicBool,
    /// ipmitool closed the terminal.
    ended: AtomicBool,
    operational: AtomicBool,
}

pub struct Console {
    parser: Arc<Mutex<vt100::Parser>>,
    flags: Arc<Flags>,
    input: SyncSender<Vec<u8>>,
    /// `None` only in tests.
    child: Option<Child>,
    target: Target,
}

/// Open a pseudo-terminal pair of the given size: (our side, ipmitool side).
fn open_pty(s: &Settings) -> io::Result<(File, File)> {
    let (mut master, mut slave) = (-1, -1);
    let size = libc::winsize { ws_row: s.rows.get(), ws_col: s.cols.get(), ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: openpty writes two descriptors into the given integers and reads
    // `size`. The name and termios arguments may be null.
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors are open and owned by nothing else.
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // Children must not inherit either one. The copies made for stdin, stdout
    // and stderr of ipmitool do not carry this flag.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: sets a flag on a descriptor we own.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((master, slave))
}

/// A poisoned lock only means the reader thread panicked. The screen content is
/// still usable.
fn lock(p: &Mutex<vt100::Parser>) -> MutexGuard<'_, vt100::Parser> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

fn new_parser(s: &Settings) -> Arc<Mutex<vt100::Parser>> {
    Arc::new(Mutex::new(vt100::Parser::new(s.rows, s.cols, s.scrollback)))
}

/// Feed everything ipmitool prints into the emulator until the terminal closes.
fn read_output(mut from: File, parser: Arc<Mutex<vt100::Parser>>, flags: Arc<Flags>) {
    let mut buf = [0u8; 4096];
    // The first bytes are kept to find the "operational" line, which may be
    // split over several reads.
    let mut head: Vec<u8> = vec![];
    loop {
        match from.read(&mut buf) {
            // Linux reports EIO once the other side is closed.
            Ok(0) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(n) => {
                if !flags.operational.load(Ordering::SeqCst) && head.len() < 16 * 1024 {
                    head.extend_from_slice(&buf[..n]);
                    if head.windows(OPERATIONAL.len()).any(|w| w == OPERATIONAL) {
                        flags.operational.store(true, Ordering::SeqCst);
                    }
                }
                lock(&parser).process(&buf[..n]);
                flags.dirty.store(true, Ordering::SeqCst);
            }
        }
    }
    flags.ended.store(true, Ordering::SeqCst);
    flags.dirty.store(true, Ordering::SeqCst);
}

/// Pass keystrokes on from their own thread, so that a full terminal buffer can
/// not freeze the UI.
fn write_input(mut to: File, from: Receiver<Vec<u8>>) {
    for bytes in from {
        if to.write_all(&bytes).is_err() {
            break;
        }
    }
}

impl Console {
    /// Start `ipmitool sol activate` for a host. The caller writes the audit
    /// record first.
    pub fn spawn(t: &Target, s: &Settings) -> io::Result<Console> {
        let (master, slave) = open_pty(s)?;
        let mut cmd = Command::new(&t.bin);
        cmd.args(t.cli_args(&["sol".into(), "activate".into()]))
            .env("TERM", "vt100")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe. They make the pty the
        // controlling terminal of the child, which then also gets SIGHUP if we
        // die without cleaning up.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // `cmd` still holds the child side. It must close here, or the end of
        // output would never be seen.
        drop(cmd);
        let (parser, flags) = (new_parser(s), Arc::new(Flags::default()));
        let (input, rx) = sync_channel(INPUT_QUEUE);
        let writer = master.try_clone()?;
        let (p, f) = (parser.clone(), flags.clone());
        std::thread::spawn(move || read_output(master, p, f));
        std::thread::spawn(move || write_input(writer, rx));
        Ok(Console { parser, flags, input, child: Some(child), target: t.clone() })
    }

    pub fn live(&self) -> bool {
        !self.flags.ended.load(Ordering::SeqCst)
    }

    /// True once after new output.
    pub fn take_dirty(&self) -> bool {
        self.flags.dirty.swap(false, Ordering::SeqCst)
    }

    /// The session ended because someone else holds the console.
    pub fn busy_elsewhere(&self) -> bool {
        !self.live() && !self.flags.operational.load(Ordering::SeqCst) && self.screen().screen().contents().contains(BUSY)
    }

    pub fn screen(&self) -> MutexGuard<'_, vt100::Parser> {
        lock(&self.parser)
    }

    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.input.try_send(bytes);
    }

    /// Lines the view is above the live screen.
    pub fn scrolled(&self) -> usize {
        self.screen().screen().scrollback()
    }

    /// Move the view. Positive is back into the past. The emulator limits the
    /// result to the lines it has.
    pub fn scroll(&self, delta: isize) {
        let mut p = self.screen();
        let to = p.screen().scrollback().saturating_add_signed(delta);
        p.screen_mut().set_scrollback(to);
    }

    pub fn scroll_to_live(&self) {
        self.screen().screen_mut().set_scrollback(0);
    }

    /// End the session. This blocks for as long as the BMC takes to answer, so
    /// the UI calls it from a thread.
    ///
    /// Killing ipmitool leaves the session open on the BMC, so it is
    /// deactivated afterwards. That is only done when ipmitool was still
    /// running and its session had been accepted: after a refusal, or after the
    /// session ended by itself, the console may belong to someone else.
    pub fn close(mut self) {
        let Some(mut child) = self.child.take() else { return };
        let running = matches!(child.try_wait(), Ok(None));
        if running {
            let _ = child.kill();
        }
        let _ = child.wait();
        let mut result = "session had ended".to_string();
        if running && self.flags.operational.load(Ordering::SeqCst) {
            let r = self.target.run(&["sol".into(), "deactivate".into()], Duration::from_secs(10));
            result = match r {
                Ok(_) => "deactivated".into(),
                Err(e) => format!("deactivate failed: {}", e.lines().next().unwrap_or("")),
            };
        }
        let _ = ipmi::audit(&self.target.describe(), &format!("sol closed: {result}"));
    }

    /// A console without a process. The receiver gets what would be typed.
    #[cfg(test)]
    pub fn fake(t: &Target, output: &[u8]) -> (Console, Receiver<Vec<u8>>) {
        let s = Settings::default();
        let parser = new_parser(&s);
        lock(&parser).process(output);
        let (input, rx) = sync_channel(INPUT_QUEUE);
        (Console { parser, flags: Arc::new(Flags::default()), input, child: None, target: t.clone() }, rx)
    }

    #[cfg(test)]
    pub fn end(&self) {
        self.flags.ended.store(true, Ordering::SeqCst);
        self.flags.dirty.store(true, Ordering::SeqCst);
    }
}

/// The byte a Ctrl key combination stands for, if it has one. Terminals send
/// 0x1c to 0x1f for Ctrl with `\ ] ^ _`, which crossterm reports as Ctrl with
/// `4 5 6 7`.
pub fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        ' ' | '@' => Some(0),
        '[' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' => Some(0x1f),
        _ => None,
    }
}

/// What to send to the host for a key press, in the encoding of a VT100 or
/// xterm. `app_cursor` is the cursor key mode the host has asked for.
pub fn encode_key(code: KeyCode, mods: KeyModifiers, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: u8| vec![0x1b, if app_cursor { b'O' } else { b'[' }, c];
    let seq = |s: &str| s.as_bytes().to_vec();
    let mut out = match code {
        KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) => vec![ctrl_byte(c)?],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => seq("\x1b[Z"),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => arrow(b'A'),
        KeyCode::Down => arrow(b'B'),
        KeyCode::Right => arrow(b'C'),
        KeyCode::Left => arrow(b'D'),
        KeyCode::Home => arrow(b'H'),
        KeyCode::End => arrow(b'F'),
        KeyCode::Insert => seq("\x1b[2~"),
        KeyCode::Delete => seq("\x1b[3~"),
        KeyCode::PageUp => seq("\x1b[5~"),
        KeyCode::PageDown => seq("\x1b[6~"),
        KeyCode::F(n @ 1..=4) => vec![0x1b, b'O', b'P' + n - 1],
        KeyCode::F(n @ 5..=12) => {
            let number = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            format!("\x1b[{number}~").into_bytes()
        }
        _ => return None,
    };
    // Alt is sent as an escape in front.
    if mods.contains(KeyModifiers::ALT) {
        out.insert(0, 0x1b);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(encode_key(KeyCode::Enter, none, false), Some(b"\r".to_vec()));
        assert_eq!(encode_key(KeyCode::Char('c'), KeyModifiers::CONTROL, false), Some(vec![3]));
        assert_eq!(encode_key(KeyCode::Char('5'), KeyModifiers::CONTROL, false), Some(vec![0x1d]));
        assert_eq!(encode_key(KeyCode::Up, none, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(KeyCode::Up, none, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(encode_key(KeyCode::F(2), none, false), Some(b"\x1bOQ".to_vec()));
        assert_eq!(encode_key(KeyCode::F(12), none, false), Some(b"\x1b[24~".to_vec()));
        assert_eq!(encode_key(KeyCode::Char('x'), KeyModifiers::ALT, false), Some(b"\x1bx".to_vec()));
        assert_eq!(encode_key(KeyCode::CapsLock, none, false), None);
    }

    /// Feeds pseudo-random bytes, rich in escape sequences, to the emulator
    /// while scrolling it. It must not panic, whatever the host prints.
    #[test]
    fn hostile_output_does_not_panic() {
        const PIECES: [&[u8]; 14] = [b"\x1b[", b"\x1b]", b"\x1b", b";", b"9999", b"0", b"r", b"H", b"J", b"?1049h", b"\n", b"\r", "宽".as_bytes(), b"L"];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for (cols, rows) in [(80, 24), (2, 2), (132, 43)] {
            let s = Settings { cols: NonZeroU16::new(cols).unwrap(), rows: NonZeroU16::new(rows).unwrap(), scrollback: 30 };
            let parser = new_parser(&s);
            let mut p = lock(&parser);
            for i in 0..60_000 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let r = seed as usize;
                if r.is_multiple_of(3) {
                    p.process(&[(r >> 8) as u8]);
                } else {
                    p.process(PIECES[(r >> 8) % PIECES.len()]);
                }
                if i % 97 == 0 {
                    p.screen_mut().set_scrollback(r % 50);
                    let _ = p.screen().contents();
                }
            }
        }
    }

    /// Runs the fake ipmitool in a real pseudo-terminal: output arrives, typed
    /// bytes reach the process, and its exit is noticed.
    #[test]
    fn session_with_the_fake_ipmitool() {
        let bin = format!("{}/tests/fake-ipmitool.sh", env!("CARGO_MANIFEST_DIR"));
        let t = Target { bin, name: None, host: None, user: None, iface: "lanplus".into(), pass_file: None };
        let c = Console::spawn(&t, &Settings::default()).unwrap();
        let wait = |what: &str, ok: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !ok() {
                assert!(start.elapsed() < Duration::from_secs(10), "no {what}: {}", c.screen().screen().contents());
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait("prompt", &|| c.screen().screen().contents().contains("testbox login:"));
        assert!(c.flags.operational.load(Ordering::SeqCst) && c.live());
        c.send(b"root\r".to_vec());
        wait("reply", &|| c.screen().screen().contents().contains("got: root"));
        wait("exit", &|| !c.live());
        assert!(!c.busy_elsewhere());
        // Not closed here: close writes to the real audit log of the user.
    }
}
