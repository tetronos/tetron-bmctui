//! Turns fetched ipmitool text into styled lines. No terminal code here.

use crate::ipmi::{self, Action, Screen};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Normal,
    Head,
    Good,
    Warn,
    Bad,
    Dim,
    Sel,
    /// A console cell with the colors and attributes the host asked for.
    Term(crate::canvas::TermStyle),
}

pub type Line = (String, Kind);

pub struct Ctx<'a> {
    pub data: Option<&'a Result<String, String>>,
    pub vendor: Option<u32>,
    pub cursor: usize,
    pub loading: bool,
    /// All configured hosts and the one in use, for the Hosts screen.
    pub hosts: &'a [ipmi::Target],
    pub current: usize,
    /// The BMC address of the host in use, once read.
    pub lan: Option<&'a ipmi::Lan>,
    /// Ask for UEFI boot with the one-time boot actions.
    pub efi_boot: bool,
}

/// Screens whose lines can be narrowed with the filter.
pub fn filterable(s: Screen) -> bool {
    matches!(s, Screen::Sensors | Screen::Sel | Screen::Fru)
}

/// Fetched text reduced to the lines that contain `needle`, ignoring case. The
/// comparison uses what is shown, not hidden columns such as sensor numbers.
pub fn filter(s: Screen, text: &str, needle: &str) -> String {
    let needle = needle.to_lowercase();
    let shown = |l: &str| match s {
        Screen::Sensors => ipmi::parse_sdr(l).first().map(|x| format!("{} {} {}", x.name, x.status, x.reading)).unwrap_or_default(),
        Screen::Sel => ipmi::decode_sel_line(l),
        _ => l.to_string(),
    };
    text.lines().filter(|l| shown(l).to_lowercase().contains(&needle)).map(|l| format!("{l}\n")).collect()
}

/// On screens with a menu (Power, Fans) the first action is on this line. Line
/// 0 is the current state and line 1 is blank, also while loading or after an
/// error, so the menu never moves and is never hidden while it can be used.
pub const MENU_FIRST_LINE: usize = 2;

/// Actions offered by a screen. The fan commands are Supermicro OEM commands,
/// so other vendors get none until their own commands are added.
pub fn actions(s: Screen, vendor: Option<u32>, efi_boot: bool) -> Vec<Action> {
    match s {
        Screen::Control => {
            let mut v = ipmi::control_actions();
            if efi_boot {
                for a in v.iter_mut().filter(|a| a.args.get(1).is_some_and(|x| x == "bootdev")) {
                    a.args.push("options=efiboot".into());
                    a.label.push_str(", UEFI");
                }
            }
            v
        }
        Screen::Fan if vendor == Some(ipmi::SUPERMICRO_ID) => ipmi::fan_actions(),
        _ => vec![],
    }
}

/// Break text into lines of at most `width` characters, preferring spaces.
/// Words longer than a line are split, so nothing is ever cut off. This matters
/// for the host name in dialogs.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = vec![];
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word: Vec<char> = word.chars().collect();
        if !line.is_empty() && line.chars().count() + 1 + word.len() > width {
            out.push(std::mem::take(&mut line));
        }
        while word.len() > width {
            out.push(word.drain(..width).collect());
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.extend(word);
    }
    if !line.is_empty() || out.is_empty() {
        out.push(line);
    }
    out
}

/// Meaning of the status column of `sdr elist`.
fn sensor_status(s: &str) -> &'static str {
    match s {
        "ok" => "normal",
        "ns" | "na" => "no reading",
        "nc" => "non-critical limit crossed",
        "cr" => "critical limit crossed",
        "nr" => "non-recoverable limit crossed",
        _ => "unrecognized status",
    }
}

/// Text of the details pane for the selected row, wrapped to `width`.
pub fn details(s: Screen, c: &Ctx, width: usize) -> Vec<Line> {
    // Unwrapped (text, style) pairs are collected first and wrapped at the end.
    let mut v: Vec<Line> = vec![];
    let items = actions(s, c.vendor, c.efi_boot);
    if s == Screen::Hosts {
        // Row 0 is the table header, so host N is on row N + 1.
        if let Some(h) = c.cursor.checked_sub(1).and_then(|i| c.hosts.get(i)) {
            v.push((h.label(), Kind::Head));
            v.push((String::new(), Kind::Normal));
            let pass = h.pass_file.clone().unwrap_or_else(|| "IPMI_PASSWORD".into());
            for (name, value) in [
                ("address", h.host.clone().unwrap_or_else(|| "in-band (/dev/ipmi0)".into())),
                ("user", h.user.clone().unwrap_or_else(|| "(none)".into())),
                ("interface", h.iface.clone()),
                ("password from", pass),
            ] {
                v.push((name.into(), Kind::Dim));
                v.push((value, Kind::Normal));
            }
            // Also here, for terminals too narrow to show it in the top bar.
            if c.cursor == c.current + 1
                && let Some(lan) = c.lan
            {
                for (name, value) in [("BMC IP", &lan.ip), ("BMC MAC", &lan.mac)] {
                    v.push((name.into(), Kind::Dim));
                    v.push((value.clone(), Kind::Normal));
                }
            }
        }
    } else if let Some(a) = items.get(c.cursor) {
        v.push((a.label.clone(), Kind::Head));
        v.push((String::new(), Kind::Normal));
        v.push((format!("ipmitool {}", a.args.join(" ")), Kind::Dim));
        v.push((String::new(), Kind::Normal));
        v.push(match a.risk {
            ipmi::Risk::Low => ("Runs at once.".into(), Kind::Normal),
            ipmi::Risk::High => ("Asks for confirmation.".into(), Kind::Warn),
        });
    } else if let Some(Ok(text)) = c.data {
        match s {
            Screen::Sensors => {
                // Row 0 is the table header, so sensor N is on row N + 1.
                if let Some(x) = c.cursor.checked_sub(1).and_then(|i| ipmi::parse_sdr(text).into_iter().nth(i)) {
                    v.push((x.name, Kind::Head));
                    v.push((String::new(), Kind::Normal));
                    v.push((x.reading, Kind::Normal));
                    v.push((format!("status {}: {}", x.status, sensor_status(&x.status)), Kind::Dim));
                }
            }
            Screen::Sel => {
                // Events are listed newest first, as in `render`.
                if let Some(l) = text.lines().rev().nth(c.cursor) {
                    let (fields, note) = ipmi::sel_fields(l);
                    if fields.is_empty() {
                        v.push((l.to_string(), Kind::Normal));
                    }
                    for (name, value) in fields {
                        v.push((name.to_string(), Kind::Dim));
                        v.push((value, Kind::Normal));
                    }
                    if let Some(n) = note {
                        v.push((String::new(), Kind::Normal));
                        v.push((n.to_string(), Kind::Dim));
                    }
                }
            }
            _ => {
                if let Some((l, _)) = render(s, c).into_iter().nth(c.cursor) {
                    v.push((l, Kind::Normal));
                }
            }
        }
    }
    v.into_iter().flat_map(|(t, k)| wrap(&t, width).into_iter().map(move |l| (l, k))).collect()
}

/// "Chassis power: on", or why it is not known.
pub fn power_state(data: Option<&Result<String, String>>, loading: bool) -> Line {
    match data {
        None => (if loading { "reading power state..." } else { "power state not read (r reads it)" }.into(), Kind::Dim),
        Some(Err(_)) => ("power state could not be read".into(), Kind::Bad),
        Some(Ok(text)) => {
            let p = ipmi::parse_power(text).unwrap_or_else(|| "unknown".into());
            let kind = if p == "on" { Kind::Good } else { Kind::Warn };
            (format!("Chassis power: {p}"), kind)
        }
    }
}

/// Power and Fans: state line, blank line, menu, then notes. The menu is drawn
/// even when the state could not be read, because actions such as a BMC reset
/// are most useful exactly then.
fn menu_screen(s: Screen, c: &Ctx) -> Vec<Line> {
    let mut notes: Vec<Line> = vec![];
    let state = match c.data {
        None => (if c.loading { "loading..." } else { "state not read yet (press r)" }.to_string(), Kind::Dim),
        Some(Err(e)) => {
            notes.extend(e.lines().map(|l| (format!("error: {l}"), Kind::Bad)));
            ("state could not be read".to_string(), Kind::Bad)
        }
        Some(Ok(text)) if s == Screen::Control => {
            let p = ipmi::parse_power(text).unwrap_or_else(|| "unknown".into());
            let kind = if p == "on" { Kind::Good } else { Kind::Warn };
            (format!("Chassis power: {p}"), kind)
        }
        Some(Ok(text)) => match ipmi::parse_fan_mode(text) {
            Some(b) => (format!("Fan mode: {} (0x{b:02x})", ipmi::fan_name(b)), Kind::Good),
            None => (format!("Fan mode: unreadable reply {:?}", text.trim()), Kind::Warn),
        },
    };
    let mut v = vec![state, (String::new(), Kind::Normal)];
    v.extend(menu(&actions(s, c.vendor, c.efi_boot), c.cursor));
    v.push((String::new(), Kind::Normal));
    if s == Screen::Control {
        v.push(("! asks for confirmation, the others run at once.".into(), Kind::Dim));
        v.push(("Actions need unlock (u).".into(), Kind::Dim));
    } else {
        v.push(("Supermicro OEM raw command 0x30 0x45.".into(), Kind::Dim));
        v.push(("Boards differ: check the mode and fan speeds after a change.".into(), Kind::Warn));
        v.push(("A mode asks for confirmation. Actions need unlock (u).".into(), Kind::Dim));
    }
    if !notes.is_empty() {
        v.push((String::new(), Kind::Normal));
        v.extend(notes);
    }
    v
}

fn menu(items: &[Action], cursor: usize) -> Vec<Line> {
    items
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let hi = i == cursor;
            let mark = if a.risk == ipmi::Risk::High { "!" } else { " " };
            (format!("{} {mark} {}", if hi { ">" } else { " " }, a.label), if hi { Kind::Sel } else { Kind::Normal })
        })
        .collect()
}

fn dash_kind(l: &str) -> Kind {
    let v = l.rsplit(':').next().unwrap_or("").trim();
    let key = l.split(':').next().unwrap_or("");
    let alarm = ["Fault", "Overload", "Intrusion", "Interlock"].iter().any(|k| key.contains(k));
    if alarm && (v == "true" || v == "active") {
        Kind::Bad
    } else if key.contains("System Power") {
        if v == "on" { Kind::Good } else { Kind::Warn }
    } else {
        Kind::Normal
    }
}

/// The host table. The host in use is marked with `*`.
fn hosts_screen(c: &Ctx) -> Vec<Line> {
    let mut v = vec![(format!("  {:<16} {:<24} {}", "NAME", "ADDRESS", "USER"), Kind::Head)];
    for (i, h) in c.hosts.iter().enumerate() {
        let mark = if i == c.current { "*" } else { " " };
        let line = format!("{mark} {:<16} {:<24} {}", h.label(), h.host.as_deref().unwrap_or("in-band"), h.user.as_deref().unwrap_or(""));
        v.push((line, Kind::Normal));
    }
    v.push((String::new(), Kind::Normal));
    v.push(("Enter or click selects a host. Switching locks writes.".into(), Kind::Dim));
    v
}

pub fn render(s: Screen, c: &Ctx) -> Vec<Line> {
    if s == Screen::Hosts {
        return hosts_screen(c);
    }
    if s == Screen::Fan {
        match c.vendor {
            None => return vec![("BMC vendor not known yet (r asks again).".into(), Kind::Dim)],
            Some(ipmi::SUPERMICRO_ID) => {}
            Some(id) => {
                return vec![
                    ("Fan control is not available for this vendor yet.".into(), Kind::Warn),
                    ("The commands built so far are Supermicro OEM commands.".into(), Kind::Dim),
                    (format!("This BMC reports manufacturer id {id} (Supermicro is {}).", ipmi::SUPERMICRO_ID), Kind::Dim),
                ];
            }
        }
    }
    if matches!(s, Screen::Control | Screen::Fan) {
        return menu_screen(s, c);
    }
    let text = match c.data {
        None => return vec![(if c.loading { "loading..." } else { "no data (press r)" }.into(), Kind::Dim)],
        Some(Err(e)) => {
            let v: Vec<Line> = e.lines().map(|l| (format!("error: {l}"), Kind::Bad)).collect();
            return if v.is_empty() { vec![("error: (no message)".into(), Kind::Bad)] } else { v };
        }
        Some(Ok(t)) => t,
    };
    match s {
        Screen::Dash => text.lines().map(|l| (l.to_string(), dash_kind(l))).collect(),
        Screen::Sensors => {
            let mut v = vec![(format!("{:<28} {:<7} {}", "SENSOR", "STATUS", "READING"), Kind::Head)];
            for s in ipmi::parse_sdr(text) {
                let k = match s.status.as_str() {
                    "ok" => Kind::Normal,
                    "ns" | "na" => Kind::Dim,
                    "cr" | "nr" => Kind::Bad,
                    _ => Kind::Warn,
                };
                v.push((format!("{:<28} {:<7} {}", s.name, s.status, s.reading), k));
            }
            v
        }
        Screen::Sel => {
            let mut v: Vec<Line> = text
                .lines()
                .rev()
                .map(|l| {
                    let k = if l.contains("Non-critical") || l.contains("Non-Critical") {
                        Kind::Warn
                    } else if l.contains("Critical") || l.contains("Failure") || l.contains("Fault") {
                        Kind::Bad
                    } else {
                        Kind::Normal
                    };
                    (ipmi::decode_sel_line(l), k)
                })
                .collect();
            if v.is_empty() {
                v.push(("event log is empty".into(), Kind::Dim));
            }
            v
        }
        Screen::Fru => text
            .lines()
            .map(|l| (l.to_string(), if l.starts_with("FRU Device Description") { Kind::Head } else { Kind::Normal }))
            .collect(),
        // Drawn from the emulator, not from fetched text.
        Screen::Console => vec![],
        // Handled above.
        Screen::Control | Screen::Fan => menu_screen(s, c),
        Screen::Hosts => hosts_screen(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_alarm_colors() {
        assert_eq!(dash_kind("Main Power Fault     : true"), Kind::Bad);
        assert_eq!(dash_kind("Main Power Fault     : false"), Kind::Normal);
        assert_eq!(dash_kind("System Power         : on"), Kind::Good);
        assert_eq!(dash_kind("System Power         : off"), Kind::Warn);
    }

    #[test]
    fn fan_gate_for_other_vendor() {
        let d: Result<String, String> = Ok(" 01".into());
        let c = Ctx { data: Some(&d), vendor: Some(674), cursor: 0, loading: false, hosts: &[], current: 0, lan: None, efi_boot: false };
        let v = render(Screen::Fan, &c);
        assert!(v[0].0.contains("not available"));
        assert!(actions(Screen::Fan, c.vendor, false).is_empty());
        let c2 = Ctx { data: Some(&d), vendor: Some(ipmi::SUPERMICRO_ID), cursor: 1, loading: false, hosts: &[], current: 0, lan: None, efi_boot: false };
        let v2 = render(Screen::Fan, &c2);
        assert!(v2[0].0.contains("Full speed"));
    }

    #[test]
    fn menu_stays_in_place_after_an_error() {
        let d: Result<String, String> = Err("boom".into());
        let c = Ctx { data: Some(&d), vendor: None, cursor: 0, loading: false, hosts: &[], current: 0, lan: None, efi_boot: false };
        let v = render(Screen::Control, &c);
        assert!(v[MENU_FIRST_LINE].0.contains("Power on"));
        assert!(v.iter().any(|l| l.0 == "error: boom"));
    }

    #[test]
    fn wrap_never_drops_text() {
        assert_eq!(wrap("Run on host:", 40), ["Run on host:"]);
        let long = "a-very-long-host-name.example.internal";
        assert_eq!(wrap(long, 10).concat(), long);
        assert!(wrap(&format!("on {long} now"), 12).iter().all(|l| l.len() <= 12));
    }

    #[test]
    fn filter_uses_what_is_shown() {
        let sdr = "CPU1 Temp | 01h | ok | 3.1 | 38 degrees C\nFAN1 | 41h | ns | 7.1 | No Reading\n";
        assert_eq!(filter(Screen::Sensors, sdr, "fan").lines().count(), 1);
        assert_eq!(filter(Screen::Sensors, sdr, "41h"), "");
        let sel = "1 | d | t | OEM record c0 | 000137 | 630069007000\n";
        assert_eq!(filter(Screen::Sel, sel, "CIP"), sel);
    }

    #[test]
    fn uefi_boot_actions() {
        let args = |efi| actions(Screen::Control, None, efi).into_iter().map(|a| a.args.join(" ")).collect::<Vec<_>>();
        assert!(args(false).contains(&"chassis bootdev cdrom".to_string()));
        assert!(args(true).contains(&"chassis bootdev cdrom options=efiboot".to_string()));
        assert!(args(true).contains(&"chassis power on".to_string()));
    }

    #[test]
    fn error_lines() {
        let d: Result<String, String> = Err("boom".into());
        let c = Ctx { data: Some(&d), vendor: None, cursor: 0, loading: false, hosts: &[], current: 0, lan: None, efi_boot: false };
        assert_eq!(render(Screen::Sensors, &c)[0], ("error: boom".to_string(), Kind::Bad));
    }
}
