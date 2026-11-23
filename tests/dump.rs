use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_bmc-tui")
}

fn fake() -> String {
    format!("{}/tests/fake-ipmitool.sh", env!("CARGO_MANIFEST_DIR"))
}

fn dump(screen: &str, extra: &[&str], password: Option<&str>) -> Output {
    let mut c = Command::new(bin());
    c.args(["--ipmitool", &fake(), "--dump", screen]).args(extra).env_remove("IPMI_PASSWORD").env_remove("BMC_HOST").env_remove("BMC_USER");
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
    let p = std::env::temp_dir().join(format!("bmc-tui-pw-{}", std::process::id()));
    std::fs::write(&p, "s3cret\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    let o = dump("sensors", &["-H", "h", "--pass-file", p.to_str().unwrap()], None);
    assert_eq!(o.status.code(), Some(2));
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    let o = dump("sensors", &["-H", "h", "--pass-file", p.to_str().unwrap()], None);
    assert!(o.status.success());
    let _ = std::fs::remove_file(&p);
}
