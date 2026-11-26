//! Change the password of a user on the BMC itself.
//!
//! The password is never put in an argument list, where other local users could
//! read it. `ipmitool user set password ID` asks for it on its terminal, twice,
//! so it is run in a pseudo-terminal and answered there.
//!
//! When the password of the login user is changed, the new one is written to a
//! file of its own *before* anything is sent. Whatever happens afterwards, it
//! is not lost. Nothing is retried: repeated wrong logins can lock a BMC
//! account.

use crate::console;
use crate::hosts;
use crate::ipmi::Target;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// IPMI 2.0 passwords are at most 20 bytes. There is no minimum here, as with
/// ipmitool itself (USER), apart from not being empty.
pub const MAX: usize = hosts::PASSWORD_MAX;

const CHANGE_TIMEOUT: Duration = Duration::from_secs(60);
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// One line of the BMC user list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct User {
    pub id: u8,
    pub name: String,
    pub privilege: String,
}

/// Parse `ipmitool user list`. Columns are ID, name (may be empty), three
/// true/false columns and the privilege (may hold spaces). Slots without a
/// name are left out.
pub fn parse_users(text: &str) -> Vec<User> {
    text.lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let id: u8 = words.first()?.parse().ok()?;
            let flag = |w: &&str| *w == "true" || *w == "false";
            // The last run of three flags: a user could be named "true".
            let at = (1..words.len().saturating_sub(2)).rev().find(|&i| words[i..i + 3].iter().all(flag))?;
            let name = words[1..at].join(" ");
            (!name.is_empty()).then(|| User { id, name, privilege: words[at + 3..].join(" ") })
        })
        .collect()
}

pub fn fetch_users(t: &Target) -> Result<Vec<User>, String> {
    let text = t.run(&["user".to_string(), "list".to_string()], READ_TIMEOUT)?;
    let users = parse_users(&text);
    if users.is_empty() { Err("the BMC reported no users that can be read".into()) } else { Ok(users) }
}

/// Is this the account the program logs in with?
pub fn is_login(t: &Target, u: &User) -> bool {
    t.host.is_some() && t.user.as_deref() == Some(u.name.as_str())
}

/// The password the program uses for this host: the first line of the password
/// file, as ipmitool reads it, else `IPMI_PASSWORD`.
pub fn current_password(t: &Target) -> Result<String, String> {
    match &t.pass_file {
        Some(f) => {
            let text = std::fs::read_to_string(f).map_err(|e| format!("password file: {e}"))?;
            Ok(text.split(['\r', '\n']).next().unwrap_or("").to_string())
        }
        None => std::env::var("IPMI_PASSWORD").map_err(|_| "no password file and no IPMI_PASSWORD".to_string()),
    }
}

/// What a change led to.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The BMC confirmed the change.
    pub changed: bool,
    /// For the login user: a login with the new password worked.
    pub verified: bool,
    /// File holding the new password of the login user. It is kept whenever it
    /// might be the password the BMC now has.
    pub saved: Option<PathBuf>,
    /// Why something did not work. Never contains a password.
    pub problem: Option<String>,
}

/// Run `ipmitool user set password ID` and answer its two prompts. The password
/// is typed only after a prompt has appeared: ipmitool discards what was typed
/// before.
fn set_with_prompt(t: &Target, id: u8, password: &str) -> Result<(), String> {
    let args = t.cli_args(&["user".into(), "set".into(), "password".into(), id.to_string()]);
    let (mut child, master) = console::spawn_in_pty(&t.bin, &args, &console::Settings::default()).map_err(|e| format!("{}: {e}", t.bin))?;
    let mut writer = master.try_clone().map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut reader = master;
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        // Ends with an error once ipmitool has closed the terminal.
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let prompt = format!("Password for user {id}: ");
    let (mut output, mut answered, start) = (String::new(), 0, Instant::now());
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
                while answered < 2 && output.matches(&prompt).count() > answered {
                    if let Err(e) = writer.write_all(format!("{password}\n").as_bytes()) {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("could not answer ipmitool: {e}"));
                    }
                    answered += 1;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if start.elapsed() > CHANGE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("timeout: it is not known whether the password changed".into());
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() && answered == 2 && output.contains("successful") {
        return Ok(());
    }
    // The last line that is not a prompt says what went wrong. The password is
    // removed in case anything echoed it.
    // The prompts are taken out first: a message can share a line with one.
    let said = output.replace(&prompt, "\n");
    let why = said.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no message");
    Err(why.replace(password, "(hidden)"))
}

/// Writes one audit record for a host. A parameter, so that tests do not write
/// to the real log.
pub type Audit = dyn Fn(&str, &str) -> Result<(), String>;

/// Change the password of `user`. `pass_dir` receives the new password file of
/// the login user. This blocks, so the UI calls it from a thread.
pub fn change(t: &Target, user: &User, password: &str, pass_dir: &Path, audit: &Audit) -> Outcome {
    let mut out = Outcome::default();
    if password.is_empty() || password.len() > MAX || !password.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        out.problem = Some("the password must be 1 to 20 plain ASCII characters".into());
        return out;
    }
    let login = is_login(t, user);
    // No record, no change.
    if let Err(e) = audit(&t.describe(), &format!("set BMC password for user id {} ({})", user.id, user.name)) {
        out.problem = Some(format!("refused, cannot write the audit log: {e}"));
        return out;
    }
    if login {
        // Saved first: if the connection or this program dies from here on, the
        // new password is still there.
        match hosts::new_password_file(pass_dir, &format!("{}-new", t.label()), password) {
            Ok(p) => out.saved = Some(p),
            Err(e) => {
                out.problem = Some(format!("nothing was sent: {e}"));
                return out;
            }
        }
    }
    match set_with_prompt(t, user.id, password) {
        Ok(()) => out.changed = true,
        // The BMC may have changed it even so, for example when only the reply
        // was lost. The saved file stays, and no login is tried.
        Err(e) => out.problem = Some(e),
    }
    if out.changed
        && let Some(file) = &out.saved
    {
        // Exactly one login with the new password.
        let new = Target { pass_file: Some(file.display().to_string()), ..t.clone() };
        match new.run(&["mc".to_string(), "info".to_string()], READ_TIMEOUT) {
            Ok(_) => out.verified = true,
            Err(e) => out.problem = Some(format!("login with the new password failed: {}", e.lines().next().unwrap_or("").replace(password, "(hidden)"))),
        }
    }
    let result = match (out.changed, login, out.verified) {
        (true, true, true) => "changed, new login verified",
        (true, true, false) => "changed, new login NOT verified",
        (true, false, _) => "changed",
        (false, ..) => "not confirmed",
    };
    let _ = audit(&t.describe(), &format!("BMC password for user id {}: {result}", user.id));
    out
}

// ----- the form -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// The safety check: the current password (USER decision).
    Current,
    /// Waiting for the BMC user list.
    Loading,
    /// Choose the user and type the new password twice.
    Pick,
}

/// Fields of the `Pick` step, in order.
pub const PICK_USER: usize = 0;
pub const PICK_NEW: usize = 1;
pub const PICK_AGAIN: usize = 2;

/// Everything typed into the password change form.
#[derive(Clone)]
pub struct Form {
    pub step: Step,
    /// What was typed at the safety check.
    pub current: String,
    pub wrong_tries: u8,
    pub users: Vec<User>,
    pub user: usize,
    pub new: String,
    pub again: String,
    pub field: usize,
    pub error: String,
}

/// Written by hand: `Debug` output must never show a password.
impl std::fmt::Debug for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Form {{ step: {:?}, user: {}, field: {}, error: {:?}, passwords hidden }}", self.step, self.user, self.field, self.error)
    }
}

impl Default for Form {
    fn default() -> Form {
        Form { step: Step::Current, current: String::new(), wrong_tries: 0, users: vec![], user: 0, new: String::new(), again: String::new(), field: 0, error: String::new() }
    }
}

impl Form {
    pub fn selected(&self) -> Option<&User> {
        self.users.get(self.user)
    }

    /// The text field that typing goes to, if any.
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match (self.step, self.field) {
            (Step::Current, _) => Some(&mut self.current),
            (Step::Pick, PICK_NEW) => Some(&mut self.new),
            (Step::Pick, PICK_AGAIN) => Some(&mut self.again),
            _ => None,
        }
    }

    /// May the new password be sent? The two entries are compared here:
    /// ipmitool's own comparison can not be relied on.
    pub fn check_new(&self) -> Result<(), String> {
        if self.selected().is_none() {
            Err("choose a user".into())
        } else if self.new.is_empty() {
            Err("type the new password".into())
        } else if self.new != self.again {
            Err("the two entries do not match".into())
        } else if self.new.len() > MAX {
            Err(format!("at most {MAX} characters"))
        } else {
            Ok(())
        }
    }
}

/// Work for the main loop, which does the file and network access.
#[derive(Clone)]
pub enum Job {
    /// Compare the typed password with the one in use.
    Check(String),
    Change(User, String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_list_of_a_real_bmc() {
        let text = "ID  Name\t     Callin  Link Auth\tIPMI Msg   Channel Priv Limit\n\
                    1                    true    false      false      Unknown (0x00)\n\
                    2   admin            false   false      true       ADMINISTRATOR\n\
                    3   monitor          true    true       true       USER\n\
                    4   old user         true    false      false      NO ACCESS\n\
                    5   true             true    false      false      OPERATOR\n\
                    6                    true    false      false      Unknown (0x00)\n";
        let u = parse_users(text);
        assert_eq!(u.iter().map(|u| (u.id, u.name.as_str())).collect::<Vec<_>>(), [(2, "admin"), (3, "monitor"), (4, "old user"), (5, "true")]);
        assert_eq!((u[0].privilege.as_str(), u[2].privilege.as_str()), ("ADMINISTRATOR", "NO ACCESS"));
        assert!(parse_users("Get User Access command failed").is_empty());
    }

    /// A scratch directory with a fake ipmitool that stands for a BMC: it
    /// keeps the login password in a file, asks for new ones on its terminal
    /// like the real one, and refuses logins that use a different password. It
    /// also records every argument list it is given.
    struct Bmc {
        dir: PathBuf,
        target: Target,
    }

    impl Bmc {
        fn new(name: &str) -> Bmc {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("tetron-bmctui-bmc-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let write = |file: &str, text: &str, mode: u32| {
                let p = dir.join(file);
                std::fs::write(&p, text).unwrap();
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
                p.display().to_string()
            };
            write("bmc-password", "old-secret\n", 0o600);
            let own = write("my-pass-file", "old-secret\n", 0o600);
            let fake = format!("{}/tests/fake-ipmitool.sh", env!("CARGO_MANIFEST_DIR"));
            let script = format!(
                r#"#!/bin/sh
D='{}'
echo "$*" >> "$D/argv"
file=; prev=; for a; do [ "$prev" = -f ] && file=$a; prev=$a; done
cmp -s "$file" "$D/bmc-password" || {{ echo "Error: Unable to establish IPMI v2 / RMCP+ session" >&2; exit 1; }}
case "$*" in *"user set password"*) [ -e "$D/drop-reply" ] && export FAKE_DROP=1 ;; esac
FAKE_BMC_PASSWORD_FILE="$D/target-password" '{fake}' "$@"
rc=$?
# Only a change of the login user (id 2) changes what logs in.
case "$*" in *"user set password 2") [ -e "$D/target-password" ] && mv "$D/target-password" "$D/bmc-password" ;; esac
[ -e "$D/drop-reply" ] && case "$*" in *"user set password"*) echo "no response" ; exit 1 ;; esac
exit $rc
"#,
                dir.display()
            );
            let bin = write("ipmitool", &script, 0o700);
            let target = Target { bin, name: Some("box".into()), host: Some("192.0.2.9".into()), user: Some("admin".into()), iface: "lanplus".into(), pass_file: Some(own) };
            Bmc { dir, target }
        }

        fn read(&self, file: &str) -> String {
            std::fs::read_to_string(self.dir.join(file)).unwrap_or_default()
        }

        fn run(&self, user: &User, password: &str) -> (Outcome, String) {
            let log = std::cell::RefCell::new(String::new());
            let log: &'static std::cell::RefCell<String> = Box::leak(Box::new(log));
            let out = change(&self.target, user, password, &self.dir.join("pass"), &|host, line| {
                log.borrow_mut().push_str(&format!("{host} {line}\n"));
                Ok(())
            });
            (out, log.borrow().clone())
        }
    }

    impl Drop for Bmc {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn user(id: u8, name: &str) -> User {
        User { id, name: name.into(), privilege: "ADMINISTRATOR".into() }
    }

    /// A password full of characters that shells and parsers trip over.
    const HARD: &str = "aY! #$%&'\"(){}~;|^\\";

    #[test]
    fn login_password_is_changed_verified_and_never_in_argv() {
        let bmc = Bmc::new("ok");
        let (out, log) = bmc.run(&user(2, "admin"), HARD);
        assert!(out.changed && out.verified && out.problem.is_none(), "{out:?}");
        assert_eq!(bmc.read("bmc-password"), format!("{HARD}\n"));
        let saved = out.saved.unwrap();
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), format!("{HARD}\n"));
        assert_eq!(std::os::unix::fs::MetadataExt::mode(&std::fs::metadata(&saved).unwrap()) & 0o777, 0o600);
        assert_eq!(bmc.read("my-pass-file"), "old-secret\n", "the given file is never written");
        let argv = bmc.read("argv");
        assert!(!argv.contains(HARD) && !argv.contains("aY!") && !log.contains("aY!"), "{argv}{log}");
        // One change and exactly one login to verify it.
        assert_eq!((argv.matches("user set password 2").count(), argv.matches("mc info").count()), (1, 1), "{argv}");
        assert!(log.contains("changed, new login verified"), "{log}");
    }

    #[test]
    fn short_passwords_are_allowed_and_bad_ones_send_nothing() {
        let bmc = Bmc::new("len");
        let (out, _) = bmc.run(&user(3, "monitor"), "x");
        assert!(out.changed && out.saved.is_none(), "no file for another user: {out:?}");
        assert!(!bmc.read("argv").contains("mc info"), "no login test for another user");
        let before = bmc.read("argv");
        for bad in ["", &"y".repeat(21), "tab\there", "p\u{e4}ss"] {
            let (out, _) = bmc.run(&user(2, "admin"), bad);
            assert!(!out.changed && out.saved.is_none() && out.problem.is_some(), "{bad:?}");
        }
        assert_eq!(bmc.read("argv"), before, "nothing was run");
        let (out, _) = bmc.run(&user(2, "admin"), &"z".repeat(20));
        assert!(out.changed && out.verified, "{out:?}");
    }

    #[test]
    fn refusal_and_lost_reply_keep_the_new_password_and_try_no_login() {
        let bmc = Bmc::new("refused");
        let (out, _) = bmc.run(&user(2, "admin"), "reject");
        assert!(!out.changed && out.saved.as_ref().unwrap().exists(), "{out:?}");
        assert!(out.problem.unwrap().contains("failed"));
        // The BMC changes the password but the reply is lost.
        std::fs::write(bmc.dir.join("drop-reply"), "").unwrap();
        let (out, log) = bmc.run(&user(2, "admin"), "new-one");
        assert!(!out.changed && !out.verified, "{out:?}");
        assert_eq!(std::fs::read_to_string(out.saved.unwrap()).unwrap(), "new-one\n", "not lost");
        assert_eq!(bmc.read("bmc-password"), "new-one\n");
        assert!(!bmc.read("argv").contains("mc info"), "no login attempt on an unclear result");
        assert!(log.contains("not confirmed"));
    }

    #[test]
    fn no_audit_record_or_no_place_to_save_means_no_change() {
        let bmc = Bmc::new("noaudit");
        let out = change(&bmc.target, &user(2, "admin"), "new", &bmc.dir.join("pass"), &|_, _| Err("disk full".into()));
        assert!(!out.changed && out.problem.unwrap().contains("audit"));
        // The directory for the recovery file can not be made.
        let out = change(&bmc.target, &user(2, "admin"), "new", &bmc.dir.join("bmc-password/pass"), &|_, _| Ok(()));
        assert!(!out.changed && out.problem.unwrap().contains("nothing was sent"));
        assert_eq!((bmc.read("argv"), bmc.read("bmc-password")), (String::new(), "old-secret\n".to_string()));
    }

    #[test]
    fn form_debug_output_hides_passwords() {
        let f = Form { current: "cur-secret".into(), new: "new-secret".into(), again: "new-secret".into(), ..Default::default() };
        assert!(!format!("{f:?}").contains("secret"));
    }

    #[test]
    fn current_password_is_read_as_ipmitool_reads_it() {
        let p = std::env::temp_dir().join(format!("tetron-bmctui-cur-{}", std::process::id()));
        std::fs::write(&p, " pa ss \r\nsecond line\n").unwrap();
        let t = Target { pass_file: Some(p.display().to_string()), ..Target::local("x") };
        assert_eq!(current_password(&t).unwrap(), " pa ss ");
        let _ = std::fs::remove_file(&p);
        assert!(current_password(&t).is_err());
    }
}
