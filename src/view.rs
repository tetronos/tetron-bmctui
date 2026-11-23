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
}

pub type Line = (String, Kind);

pub struct Ctx<'a> {
    pub data: Option<&'a Result<String, String>>,
    pub vendor: Option<u32>,
    pub cursor: usize,
    pub loading: bool,
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

pub fn render(s: Screen, c: &Ctx) -> Vec<Line> {
    if s == Screen::Fan {
        match c.vendor {
            None => return vec![("detecting the BMC vendor...".into(), Kind::Dim)],
            Some(ipmi::SUPERMICRO_ID) => {}
            Some(id) => {
                return vec![
                    ("Fan control uses Supermicro OEM raw commands and is disabled here.".into(), Kind::Warn),
                    (format!("This BMC reports manufacturer id {id} (Supermicro is {})", ipmi::SUPERMICRO_ID), Kind::Dim),
                ];
            }
        }
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
                    (l.to_string(), k)
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
        Screen::Control => {
            let p = ipmi::parse_power(text).unwrap_or_else(|| "unknown".into());
            let mut v = vec![(format!("Chassis power: {p}"), if p == "on" { Kind::Good } else { Kind::Warn }), (String::new(), Kind::Normal)];
            v.extend(menu(&ipmi::control_actions(), c.cursor));
            v.push((String::new(), Kind::Normal));
            v.push(("! = asks for the host name. Enter runs the selected item (needs unlock: u).".into(), Kind::Dim));
            v
        }
        Screen::Fan => {
            let m = ipmi::parse_fan_mode(text);
            let cur = match m {
                Some(b) => format!("Fan mode: {} (0x{b:02x})", ipmi::fan_name(b)),
                None => format!("Fan mode: unreadable reply {:?}", text.trim()),
            };
            let mut v = vec![(cur, Kind::Good), (String::new(), Kind::Normal)];
            v.extend(menu(&ipmi::fan_actions(), c.cursor));
            v.push((String::new(), Kind::Normal));
            v.push(("Supermicro X9/X10 raw 0x30 0x45. Enter sets the mode (needs unlock: u).".into(), Kind::Dim));
            v
        }
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
        let c = Ctx { data: Some(&d), vendor: Some(674), cursor: 0, loading: false };
        let v = render(Screen::Fan, &c);
        assert!(v[0].0.contains("disabled"));
        let c2 = Ctx { data: Some(&d), vendor: Some(ipmi::SUPERMICRO_ID), cursor: 1, loading: false };
        let v2 = render(Screen::Fan, &c2);
        assert!(v2[0].0.contains("Full speed"));
    }

    #[test]
    fn error_lines() {
        let d: Result<String, String> = Err("boom".into());
        let c = Ctx { data: Some(&d), vendor: None, cursor: 0, loading: false };
        assert_eq!(render(Screen::Sensors, &c)[0], ("error: boom".to_string(), Kind::Bad));
    }
}
