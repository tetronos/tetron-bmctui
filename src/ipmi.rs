//! ipmitool wrapper: argument building (no secrets in argv), timed runs,
//! output parsers, the action lists and the audit log.

use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SUPERMICRO_ID: u32 = 10876;

#[derive(Clone, Debug)]
pub struct Target {
    pub bin: String,
    /// Name from the host list. `None` for a host given on the command line.
    pub name: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
    pub iface: String,
    pub pass_file: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Screen {
    Dash,
    Sensors,
    Sel,
    Fru,
    Control,
    Fan,
    /// Serial over LAN in a pane. It has its own session instead of fetches.
    Console,
    /// The host list. It is local: nothing is fetched for it.
    Hosts,
}

impl Screen {
    pub fn title(self) -> &'static str {
        match self {
            Screen::Dash => "Overview",
            Screen::Sensors => "Sensors",
            Screen::Sel => "Events",
            Screen::Fru => "Inventory",
            Screen::Control => "Power",
            Screen::Fan => "Fans",
            Screen::Hosts => "Hosts",
            Screen::Console => "Console",
        }
    }

    pub fn from_name(s: &str) -> Option<Screen> {
        match s {
            "dash" => Some(Screen::Dash),
            "sensors" => Some(Screen::Sensors),
            "sel" | "events" => Some(Screen::Sel),
            "fru" => Some(Screen::Fru),
            "control" | "power" => Some(Screen::Control),
            "hosts" => Some(Screen::Hosts),
            "fan" => Some(Screen::Fan),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Risk {
    /// Runs at once after unlock.
    Low,
    /// After unlock, also needs a confirmation dialog naming host and action.
    High,
}

#[derive(Clone, Debug)]
pub struct Action {
    pub label: String,
    pub args: Vec<String>,
    pub risk: Risk,
}

fn act(label: &str, args: &[&str], risk: Risk) -> Action {
    Action { label: label.into(), args: args.iter().map(|s| s.to_string()).collect(), risk }
}

pub fn control_actions() -> Vec<Action> {
    vec![
        act("Power on", &["chassis", "power", "on"], Risk::Low),
        act("Power soft shutdown (ACPI)", &["chassis", "power", "soft"], Risk::High),
        act("Power off (hard)", &["chassis", "power", "off"], Risk::High),
        act("Power cycle", &["chassis", "power", "cycle"], Risk::High),
        act("Power reset (hard)", &["chassis", "power", "reset"], Risk::High),
        act("Identify LED 15 s", &["chassis", "identify", "15"], Risk::Low),
        act("Clear event log", &["sel", "clear"], Risk::High),
        act("BMC cold reset", &["mc", "reset", "cold"], Risk::High),
    ]
}

/// Supermicro OEM fan mode: raw 0x30 0x45 0x01 <mode>.
pub fn fan_actions() -> Vec<Action> {
    let m = |label: &str, b: &str| act(label, &["raw", "0x30", "0x45", "0x01", b], Risk::Low);
    vec![m("Standard", "0x00"), m("Full speed", "0x01"), m("Optimal", "0x02"), m("Heavy IO", "0x04")]
}

pub fn fan_name(b: u8) -> &'static str {
    match b {
        0 => "Standard",
        1 => "Full speed",
        2 => "Optimal",
        4 => "Heavy IO",
        _ => "Unknown",
    }
}

impl Target {
    /// Short name for the host bar, status line and audit log.
    pub fn label(&self) -> String {
        self.name.clone().or_else(|| self.host.clone()).unwrap_or_else(|| "local".into())
    }

    /// Name and address together, for dialogs and the audit log: a list name
    /// alone does not prove which machine is meant.
    pub fn describe(&self) -> String {
        match (&self.name, &self.host) {
            (Some(n), Some(h)) => format!("{n} ({h})"),
            _ => self.label(),
        }
    }

    /// Full argument list. The password never appears here: `-E` makes
    /// ipmitool read IPMI_PASSWORD from the environment, `-f` reads a file.
    pub fn cli_args(&self, cmd: &[String]) -> Vec<String> {
        let mut a: Vec<String> = vec![];
        if let Some(h) = &self.host {
            a.extend(["-I".into(), self.iface.clone(), "-H".into(), h.clone()]);
            if let Some(u) = &self.user {
                a.extend(["-U".into(), u.clone()]);
            }
            match &self.pass_file {
                Some(f) => a.extend(["-f".into(), f.clone()]),
                None => a.push("-E".into()),
            }
        }
        a.extend(cmd.iter().cloned());
        a
    }

    pub fn run(&self, cmd: &[String], timeout: Duration) -> Result<String, String> {
        let mut child = Command::new(&self.bin)
            .args(self.cli_args(cmd))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", self.bin))?;
        let mut so = child.stdout.take().ok_or("no stdout")?;
        let mut se = child.stderr.take().ok_or("no stderr")?;
        let t1 = thread::spawn(move || {
            let mut b = vec![];
            let _ = so.read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        });
        let t2 = thread::spawn(move || {
            let mut b = vec![];
            let _ = se.read_to_end(&mut b);
            String::from_utf8_lossy(&b).into_owned()
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("timeout after {} s", timeout.as_secs()));
                    }
                    thread::sleep(Duration::from_millis(40));
                }
                Err(e) => return Err(e.to_string()),
            }
        };
        let out = t1.join().unwrap_or_default();
        let err = t2.join().unwrap_or_default();
        if status.success() {
            Ok(out)
        } else {
            let e = err.trim();
            Err(if e.is_empty() { out.trim().to_string() } else { e.to_string() })
        }
    }
}

pub fn fetch(t: &Target, s: Screen) -> Result<String, String> {
    let to = Duration::from_secs(20);
    let r = |a: &[&str]| t.run(&a.iter().map(|x| x.to_string()).collect::<Vec<_>>(), to);
    match s {
        Screen::Dash => {
            let a = r(&["chassis", "status"])?;
            let b = r(&["mc", "info"]).unwrap_or_default();
            Ok(format!("{a}\n{b}"))
        }
        Screen::Sensors => r(&["sdr", "elist"]),
        Screen::Sel => r(&["sel", "elist"]),
        Screen::Fru => r(&["fru", "print"]),
        Screen::Control => r(&["chassis", "power", "status"]),
        Screen::Fan => r(&["raw", "0x30", "0x45", "0x00"]),
        Screen::Console | Screen::Hosts => Ok(String::new()),
    }
}

pub fn fetch_vendor(t: &Target) -> Option<u32> {
    t.run(&["mc".to_string(), "info".to_string()], Duration::from_secs(20)).ok().and_then(|s| parse_manufacturer(&s))
}

/// Errors that suggest bad credentials or a locked account. The UI stops
/// auto refresh on these so a wrong password is not retried in a loop.
pub fn looks_like_auth_error(e: &str) -> bool {
    let l = e.to_lowercase();
    ["authentication", "rmcp+ session", "password", "privilege", "invalid user", "unauthorized"].iter().any(|k| l.contains(k))
}

pub struct Sensor {
    pub name: String,
    pub status: String,
    pub reading: String,
}

/// `ipmitool sdr elist` lines: name | id | status | entity | reading
pub fn parse_sdr(s: &str) -> Vec<Sensor> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').map(str::trim).collect();
            if f.len() < 5 {
                return None;
            }
            Some(Sensor { name: f[0].into(), status: f[2].into(), reading: f[4].into() })
        })
        .collect()
}

/// "Chassis Power is on" -> "on"
pub fn parse_power(s: &str) -> Option<String> {
    s.lines().find_map(|l| l.trim().strip_prefix("Chassis Power is ").map(|v| v.trim().to_string()))
}

/// raw reply like " 01" -> 1
pub fn parse_fan_mode(s: &str) -> Option<u8> {
    u8::from_str_radix(s.split_whitespace().next()?, 16).ok()
}

/// `mc info` line "Manufacturer ID : 10876"
pub fn parse_manufacturer(s: &str) -> Option<u32> {
    s.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim() == "Manufacturer ID" { v.trim().parse().ok() } else { None }
    })
}

fn audit_path() -> Option<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(p) if !p.is_empty() => std::path::PathBuf::from(p),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(base.join("tetron-bmctui").join("audit.log"))
}

/// Append one line to the audit log (mode 0600). Writes are refused by the
/// caller when this fails, so no write goes unrecorded.
pub fn audit(host: &str, line: &str) -> Result<(), String> {
    let p = audit_path().ok_or("no HOME for the audit log")?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("audit dir: {e}"))?;
    }
    let mut f = OpenOptions::new().create(true).append(true).mode(0o600).open(&p).map_err(|e| format!("audit log: {e}"))?;
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    writeln!(f, "ts={ts} host={host} {line}").map_err(|e| format!("audit write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdr_lines() {
        let s = "CPU1 Temp        | 01h | ok  |  3.1 | 38 degrees C\nFAN1             | 41h | ns  |  7.1 | No Reading\nbad line\n";
        let v = parse_sdr(s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "CPU1 Temp");
        assert_eq!(v[0].status, "ok");
        assert_eq!(v[0].reading, "38 degrees C");
        assert_eq!(v[1].status, "ns");
    }

    #[test]
    fn power_and_fan_and_vendor() {
        assert_eq!(parse_power("Chassis Power is on\n").as_deref(), Some("on"));
        assert_eq!(parse_power("x"), None);
        assert_eq!(parse_fan_mode(" 01\n"), Some(1));
        assert_eq!(parse_fan_mode(""), None);
        assert_eq!(parse_manufacturer("Device ID : 32\nManufacturer ID         : 10876\n"), Some(SUPERMICRO_ID));
        assert_eq!(parse_manufacturer("nothing"), None);
    }

    #[test]
    fn password_never_in_argv() {
        let t = Target { bin: "ipmitool".into(), name: None, host: Some("h".into()), user: Some("ADMIN".into()), iface: "lanplus".into(), pass_file: None };
        let a = t.cli_args(&["chassis".into(), "status".into()]);
        assert!(a.contains(&"-E".to_string()));
        assert!(!a.iter().any(|x| x == "-P"));
        let t2 = Target { pass_file: Some("/x/pw".into()), ..t };
        let a2 = t2.cli_args(&["sel".into(), "elist".into()]);
        assert!(a2.windows(2).any(|w| w == ["-f", "/x/pw"]));
        assert!(!a2.contains(&"-E".to_string()));
    }

    #[test]
    fn local_has_no_lan_args() {
        let t = Target { bin: "ipmitool".into(), name: None, host: None, user: None, iface: "lanplus".into(), pass_file: None };
        assert_eq!(t.cli_args(&["mc".into(), "info".into()]), vec!["mc", "info"]);
    }

    #[test]
    fn auth_errors() {
        assert!(looks_like_auth_error("Error: Unable to establish IPMI v2 / RMCP+ session"));
        assert!(!looks_like_auth_error("Get Chassis Power: Command not supported"));
    }
}
