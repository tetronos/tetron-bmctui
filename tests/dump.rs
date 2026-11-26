use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_tetron-bmctui")
}

fn fake() -> String {
    format!("{}/tests/fake-ipmitool.sh", env!("CARGO_MANIFEST_DIR"))
}

fn dump(screen: &str, extra: &[&str], password: Option<&str>) -> Output {
    let mut c = Command::new(bin());
    c.args(["--ipmitool", &fake(), "--dump", screen]).args(extra).env_remove("IPMI_PASSWORD").env_remove("BMC_HOST").env_remove("BMC_USER");
    // A real host list of the person running the tests must not be picked up.
    c.env("XDG_CONFIG_HOME", "/nonexistent");
    if let Some(p) = password {
        c.env("IPMI_PASSWORD", p);
    }
    c.output().unwrap()
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn sensors_local() {
    let o = dump("sensors", &[], None);
    assert!(o.status.success());
    let t = text(&o);
    assert!(t.contains("CPU1 Temp"));
    assert!(t.contains("38 degrees C"));
    assert!(t.contains("System Temp"));
}

#[test]
fn dash_and_sel_and_fru() {
    assert!(text(&dump("dash", &[], None)).contains("System Power"));
    assert!(text(&dump("sel", &[], None)).contains("Upper Critical"));
    assert!(text(&dump("fru", &[], None)).contains("Supermicro"));
}

#[test]
fn fan_reads_mode_on_supermicro() {
    let t = text(&dump("fan", &[], None));
    assert!(t.contains("Full speed"), "{t}");
}

#[test]
fn control_shows_power() {
    assert!(text(&dump("control", &[], None)).contains("Chassis power: on"));
}

#[test]
fn lan_without_password_is_refused() {
    let o = dump("sensors", &["-H", "10.0.0.9", "-U", "ADMIN"], None);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("IPMI_PASSWORD"));
}

#[test]
fn password_flag_is_rejected() {
    let o = dump("sensors", &["-P", "s3cret"], None);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn password_stays_out_of_argv() {
    // the fake exits 9 if the secret shows up in its arguments
    let o = dump("sensors", &["-H", "10.0.0.9", "-U", "ADMIN"], Some("s3cret"));
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn pass_file_must_be_private() {
    use std::os::unix::fs::PermissionsExt;
    let p = std::env::temp_dir().join(format!("tetron-bmctui-pw-{}", std::process::id()));
    std::fs::write(&p, "s3cret\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let o = dump("sensors", &["-H", "h", "--pass-file", p.to_str().unwrap()], None);
    assert_eq!(o.status.code(), Some(2));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    let o = dump("sensors", &["-H", "h", "--pass-file", p.to_str().unwrap()], None);
    assert!(o.status.success());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn host_list() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tetron-bmctui-hosts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |name: &str, text: &str, mode: u32| {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p.to_str().unwrap().to_string()
    };
    let pw = write("pw", "s3cret\n", 0o600);
    let entry = |n: &str, a: &str| format!("[[host]]\nname='{n}'\naddress='{a}'\npass_file='{pw}'\n");
    let list = write("hosts.toml", &(entry("one", "10.0.0.1") + &entry("two", "10.0.0.2")), 0o600);

    let o = dump("hosts", &["--hosts", &list, "--select", "two"], None);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(text(&o).contains("* two"), "{}", text(&o));
    // This machine is listed after the saved hosts, and can be selected. A
    // BMC_HOST in the environment must not turn it into a remote host.
    assert!(text(&o).contains("localhost") && text(&o).contains("this machine"), "{}", text(&o));
    let o = Command::new(bin())
        .args(["--ipmitool", &fake(), "--dump", "hosts", "--hosts", &list, "--select", "localhost"])
        .env("BMC_HOST", "10.9.9.9")
        .env("BMC_USER", "someone")
        .output()
        .unwrap();
    assert!(text(&o).contains("* localhost") && !text(&o).contains("10.9.9.9") && !text(&o).contains("someone"), "{}", text(&o));
    assert!(dump("sensors", &["--hosts", &list], None).status.success());

    for bad in [vec!["--hosts", &list, "--select", "nope"], vec!["--hosts", &list, "-H", "x"], vec!["--hosts", &list, "-U", "x"]] {
        assert_eq!(dump("hosts", &bad, None).status.code(), Some(2), "{bad:?}");
    }
    let open = write("open.toml", &entry("one", "10.0.0.1"), 0o644);
    assert_eq!(dump("hosts", &["--hosts", &open], None).status.code(), Some(2));
    write("pw", "s3cret\n", 0o644);
    assert_eq!(dump("hosts", &["--hosts", &list], None).status.code(), Some(2));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn localhost_is_listed_without_a_saved_list() {
    let t = text(&dump("hosts", &[], None));
    assert!(t.contains("* localhost"), "{t}");
    assert_eq!(t.matches("localhost").count(), 1, "{t}");
}

#[test]
fn version_flag() {
    let o = Command::new(bin()).arg("--version").output().unwrap();
    assert!(o.status.success());
    assert_eq!(text(&o), format!("tetron-bmctui {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn saved_hosts_are_kept_next_to_a_command_line_host() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("tetron-bmctui-merge-{}", std::process::id()));
    let cfg = dir.join("tetron-bmctui");
    std::fs::create_dir_all(&cfg).unwrap();
    let write = |p: &std::path::Path, text: &str, mode: u32| {
        std::fs::write(p, text).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    let pw = dir.join("pw");
    write(&pw, "s3cret\n", 0o600);
    let list = cfg.join("hosts.toml");
    write(&list, &format!("[[host]]\nname='saved'\naddress='10.0.0.2'\npass_file='{}'\n", pw.display()), 0o600);
    let run = |extra: &[&str]| {
        let mut c = Command::new(bin());
        c.args(["--ipmitool", &fake(), "--dump", "hosts", "-H", "10.0.0.9", "--pass-file", pw.to_str().unwrap()]).args(extra);
        c.env_remove("BMC_HOST").env_remove("BMC_USER").env("XDG_CONFIG_HOME", &dir);
        c.output().unwrap()
    };
    let t = text(&run(&[]));
    assert!(t.contains("* 10.0.0.9") && t.contains("  saved") && t.contains("  localhost"), "{t}");
    assert!(text(&run(&["--select", "saved"])).contains("* saved"));
    assert_eq!(run(&["--select", "nobody"]).status.code(), Some(2));
    // A broken list does not stop the named host.
    write(&list, "nonsense", 0o600);
    let o = run(&[]);
    assert!(o.status.success() && text(&o).contains("* 10.0.0.9") && text(&o).contains("  localhost"));
    let _ = std::fs::remove_dir_all(&dir);
}
