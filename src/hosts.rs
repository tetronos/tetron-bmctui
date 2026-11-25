//! Host list: an unencrypted TOML file that names the BMCs to pick from.
//!
//! It holds no passwords, only the path of each host's password file. It must
//! still be mode 0600, because addresses and user names are worth protecting,
//! and because whoever can edit it decides where passwords are sent.
//!
//! ```toml
//! [[host]]
//! name = "testbox"
//! address = "192.168.1.20"
//! user = "ADMIN"                 # optional
//! pass_file = "~/.bmc-pass"      # required, mode 0600
//! interface = "lanplus"          # optional: lanplus or lan
//! ```

use crate::ipmi::Target;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Keys a host entry may have. Anything else is an error, so that a typo is not
/// silently ignored and a `password = ...` line is noticed at once.
const KEYS: [&str; 5] = ["name", "address", "user", "pass_file", "interface"];

/// `~/.config/tetron-bmctui/hosts.toml`, or below `XDG_CONFIG_HOME` if set.
pub fn default_path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("tetron-bmctui").join("hosts.toml"))
}

/// Refuse a file that group or others can access.
pub fn check_private(path: &str) -> Result<(), String> {
    let m = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if m.mode() & 0o077 != 0 {
        return Err(format!("{path}: must not be accessible by group or others (chmod 600)"));
    }
    Ok(())
}

/// Replace a leading `~/` with the home directory. The shell does not do this
/// for text inside a file.
fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

/// Turn the file text into targets. `bin` is the ipmitool binary for all of
/// them. Password files are not touched here, see `load`.
pub fn parse(text: &str, bin: &str) -> Result<Vec<Target>, String> {
    let doc: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
    if let Some(k) = doc.keys().find(|k| *k != "host") {
        return Err(format!("unknown key {k} (hosts are [[host]] entries)"));
    }
    let list = doc.get("host").and_then(|v| v.as_array()).ok_or("no [[host]] entries")?;
    let mut out: Vec<Target> = vec![];
    for (i, entry) in list.iter().enumerate() {
        let n = i + 1;
        let t = entry.as_table().ok_or(format!("host {n}: not a table"))?;
        if let Some(k) = t.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(format!("host {n}: unknown key {k} (passwords do not belong in this file, use pass_file)"));
        }
        // Values end up in argv and in the one line per event audit log, so
        // spaces and control characters are not allowed. Paths may have spaces.
        let get = |key: &str| -> Result<Option<String>, String> {
            let Some(v) = t.get(key) else { return Ok(None) };
            let s = v.as_str().ok_or(format!("host {n}: {key} must be text"))?;
            let ok = |c: char| c.is_ascii_graphic() || (key == "pass_file" && c == ' ');
            if s.is_empty() || !s.chars().all(ok) {
                return Err(format!("host {n}: {key} must be printable ASCII without spaces"));
            }
            Ok(Some(s.to_string()))
        };
        let need = |key: &str| get(key)?.ok_or(format!("host {n}: {key} is missing"));
        let name = need("name")?;
        if out.iter().any(|o| o.name.as_ref() == Some(&name)) {
            return Err(format!("host {n}: name {name} is used twice"));
        }
        let iface = get("interface")?.unwrap_or_else(|| "lanplus".into());
        if iface != "lanplus" && iface != "lan" {
            return Err(format!("host {n}: interface must be lanplus or lan"));
        }
        out.push(Target {
            bin: bin.to_string(),
            name: Some(name),
            host: Some(need("address")?),
            user: get("user")?,
            iface,
            // Required: one IPMI_PASSWORD sent to many BMCs would leak it and
            // could lock accounts.
            pass_file: Some(expand_home(&need("pass_file")?)),
        });
    }
    if out.is_empty() {
        return Err("no [[host]] entries".into());
    }
    Ok(out)
}

/// Read the list and check the permissions of it and of every password file.
pub fn load(path: &Path, bin: &str) -> Result<Vec<Target>, String> {
    let shown = path.display().to_string();
    check_private(&shown)?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    let hosts = parse(&text, bin).map_err(|e| format!("{shown}: {e}"))?;
    for h in &hosts {
        if let Some(f) = &h.pass_file {
            check_private(f)?;
        }
    }
    Ok(hosts)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "[[host]]\nname='a'\naddress='10.0.0.1'\nuser='ADMIN'\npass_file='/x/a'\n\
                       [[host]]\nname='b'\naddress='10.0.0.2'\npass_file='/x/b'\ninterface='lan'\n";

    #[test]
    fn two_hosts() {
        let v = parse(TWO, "ipmitool").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!((v[0].label(), v[0].user.as_deref(), v[0].iface.as_str()), ("a".into(), Some("ADMIN"), "lanplus"));
        assert_eq!((v[1].user.as_deref(), v[1].iface.as_str()), (None, "lan"));
    }

    #[test]
    fn bad_files_are_refused() {
        let one = |extra: &str| format!("[[host]]\nname='a'\naddress='h'\npass_file='/p'\n{extra}");
        for (text, why) in [
            (one("password='x'\n"), "passwords do not belong"),
            (one("interface='serial'\n"), "lanplus or lan"),
            (one("user='a b'\n"), "without spaces"),
            (one("[[host]]\nname='a'\naddress='h'\npass_file='/p'\n"), "used twice"),
            ("[[host]]\nname='a'\naddress='h'\n".into(), "pass_file is missing"),
            ("".into(), "no [[host]]"),
            ("hosts = 1\n".into(), "unknown key"),
        ] {
            let e = parse(&text, "ipmitool").unwrap_err();
            assert!(e.contains(why), "{e}");
        }
    }
}
