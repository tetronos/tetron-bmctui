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
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
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

// ----- adding a host ----------------------------------------------------------

/// Longest password IPMI 2.0 allows.
pub const PASSWORD_MAX: usize = 20;

/// What was typed to add a host. Either `pass_file` or `password` is given.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct NewHost {
    pub name: String,
    pub address: String,
    pub user: String,
    pub pass_file: String,
    pub password: String,
}

/// Debug output must never show the password, so it is written by hand.
impl std::fmt::Debug for NewHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hidden = if self.password.is_empty() { "" } else { "(hidden)" };
        write!(f, "NewHost {{ name: {:?}, address: {:?}, user: {:?}, pass_file: {:?}, password: {hidden:?} }}", self.name, self.address, self.user, self.pass_file)
    }
}

/// A TOML basic string. Values are printable ASCII, checked by `parse`, so only
/// the backslash and the quote need escaping.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The lines added to the list for one host.
fn block(n: &NewHost, pass_file: &str) -> String {
    let mut s = format!("[[host]]\nname = {}\naddress = {}\n", quoted(&n.name), quoted(&n.address));
    if !n.user.is_empty() {
        s += &format!("user = {}\n", quoted(&n.user));
    }
    s + &format!("pass_file = {}\n", quoted(pass_file))
}

/// Check what can be checked without touching a file, with the same rules as
/// for the list.
pub fn validate(n: &NewHost) -> Result<(), String> {
    if n.name.is_empty() || n.address.is_empty() {
        return Err("name and address are needed".into());
    }
    if n.pass_file.is_empty() == n.password.is_empty() {
        return Err("give a password file or a password, one of the two".into());
    }
    if n.password.len() > PASSWORD_MAX || !n.password.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        return Err(format!("the password must be printable ASCII, at most {PASSWORD_MAX} characters"));
    }
    let file = if n.pass_file.is_empty() { "/placeholder" } else { &n.pass_file };
    parse(&block(n, file), "ipmitool").map(|_| ()).map_err(|e| e.replace("host 1: ", ""))
}

/// Create a new private file for a password below `dir` (mode 0700). A file
/// that exists is never written to.
fn new_password_file(dir: &Path, host: &str, password: &str) -> Result<PathBuf, String> {
    let fail = |e: std::io::Error| format!("password file in {}: {e}", dir.display());
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(fail)?;
    // The host name must not be able to leave the directory or hide the file.
    let mut safe: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    if safe.starts_with('.') {
        safe.replace_range(..1, "_");
    }
    for i in 0..100 {
        let path = dir.join(if i == 0 { safe.clone() } else { format!("{safe}-{i}") });
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{password}") {
                    let _ = std::fs::remove_file(&path);
                    return Err(fail(e));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(fail(e)),
        }
    }
    Err(format!("too many password files for {safe} in {}", dir.display()))
}

/// Replace `path` with `text` in one step, mode 0600, so that a failure can not
/// leave half a list.
fn replace_private(path: &Path, text: &str) -> std::io::Result<()> {
    // Through a symbolic link, so that the link itself stays.
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let temp = path.with_extension(format!("new-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    let result = file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).and_then(|()| std::fs::rename(&temp, &path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Add a host to the list at `path`, creating the list if needed. The new entry
/// is appended, so comments and layout stay. A typed password goes to its own
/// file below `pass_dir`; the list only ever holds the path.
pub fn add(path: &Path, pass_dir: &Path, n: &NewHost, bin: &str) -> Result<Target, String> {
    validate(n)?;
    let shown = path.display().to_string();
    let old = match std::fs::read_to_string(path) {
        Ok(text) => {
            check_private(&shown)?;
            text
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(dir) = path.parent() {
                std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("{shown}: {e}"))?;
            }
            String::new()
        }
        Err(e) => return Err(format!("{shown}: {e}")),
    };
    let join = |entry: String| {
        let gap = if old.is_empty() { "" } else if old.ends_with('\n') { "\n" } else { "\n\n" };
        format!("{old}{gap}{entry}")
    };
    // The whole new list must be valid before anything is written. This also
    // finds a name that is in use and a list that is already broken.
    let probe = if n.pass_file.is_empty() { "/placeholder" } else { &n.pass_file };
    parse(&join(block(n, probe)), bin).map_err(|e| format!("{shown}: {e}"))?;
    let (pass_file, created) = if n.password.is_empty() {
        check_private(&expand_home(&n.pass_file))?;
        (n.pass_file.clone(), None)
    } else {
        let p = new_password_file(pass_dir, &n.name, &n.password)?;
        (p.display().to_string(), Some(p))
    };
    let text = join(block(n, &pass_file));
    let written = parse(&text, bin).and_then(|all| replace_private(path, &text).map(|()| all).map_err(|e| e.to_string()));
    match written {
        Ok(mut all) => all.pop().ok_or_else(|| "no host was added".into()),
        Err(e) => {
            // No password file is left behind without a host that uses it.
            if let Some(p) = created {
                let _ = std::fs::remove_file(p);
            }
            Err(format!("{shown}: {e}"))
        }
    }
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

    /// A directory of its own for one test, removed first if left over.
    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tetron-bmctui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().mode() & 0o777
    }

    fn typed(name: &str, password: &str) -> NewHost {
        NewHost { name: name.into(), address: "192.0.2.5".into(), password: password.into(), ..Default::default() }
    }

    #[test]
    fn add_with_a_typed_password() {
        let d = scratch("add");
        let (list, pass) = (d.join("cfg/hosts.toml"), d.join("cfg/pass"));
        let t = add(&list, &pass, &typed("web01", "pa ss\"w\\d"), "ipmitool").unwrap();
        let file = PathBuf::from(t.pass_file.unwrap());
        assert_eq!((mode(&list), mode(&file), mode(&pass)), (0o600, 0o600, 0o700));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "pa ss\"w\\d\n");
        let text = std::fs::read_to_string(&list).unwrap();
        assert!(!text.contains("pa ss"), "the list holds only the path");
        assert_eq!(load(&list, "ipmitool").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn add_keeps_comments_and_overwrites_nothing() {
        let d = scratch("keep");
        let (list, pass) = (d.join("hosts.toml"), d.join("pass"));
        let mine = "# my servers\n[[host]]\nname='a'\naddress='h'\npass_file='/x' # keep me";
        std::fs::write(&list, mine).unwrap();
        std::fs::set_permissions(&list, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        std::fs::create_dir_all(&pass).unwrap();
        std::fs::write(pass.join("b"), "someone else\n").unwrap();
        let t = add(&list, &pass, &typed("b", "secret"), "ipmitool").unwrap();
        assert!(t.pass_file.unwrap().ends_with("/b-1"));
        assert_eq!(std::fs::read_to_string(pass.join("b")).unwrap(), "someone else\n");
        assert!(std::fs::read_to_string(&list).unwrap().starts_with(mine));
        // A name in use is refused, and no password file is left over.
        let e = add(&list, &pass, &typed("a", "secret"), "ipmitool").unwrap_err();
        assert!(e.contains("used twice") && !e.contains("secret"), "{e}");
        assert_eq!(std::fs::read_dir(&pass).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn add_refuses_bad_input_and_open_files() {
        let d = scratch("bad");
        let (list, pass) = (d.join("hosts.toml"), d.join("pass"));
        let both = NewHost { pass_file: "/x".into(), ..typed("a", "p") };
        for (n, why) in [
            (typed("", "p"), "name and address"),
            (typed("a b", "p"), "without spaces"),
            (typed("a", ""), "one of the two"),
            (both, "one of the two"),
            (typed("a", &"x".repeat(21)), "at most 20"),
            (typed("a", "tab\there"), "printable ASCII"),
        ] {
            let e = add(&list, &pass, &n, "ipmitool").unwrap_err();
            assert!(e.contains(why), "{e}");
        }
        // A password file that others can read is refused.
        let open = d.join("open-pw");
        std::fs::write(&open, "p").unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
        let n = NewHost { pass_file: open.display().to_string(), ..typed("a", "") };
        assert!(add(&list, &pass, &n, "ipmitool").unwrap_err().contains("chmod 600"));
        assert!(!list.exists() && !pass.exists(), "nothing was written");
        // A host name can not leave the password directory.
        let t = add(&list, &pass, &typed("../../x", "p"), "ipmitool").unwrap();
        assert_eq!(Path::new(&t.pass_file.unwrap()).parent(), Some(pass.as_path()));
        assert!(!format!("{:?}", typed("a", "topsecret")).contains("topsecret"));
        let _ = std::fs::remove_dir_all(&d);
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
