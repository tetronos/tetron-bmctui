//! Write the screen that is open to a text file.

use crate::canvas::safe_char;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `HH:MM` local time now.
pub fn clock() -> String {
    clock_seconds().chars().take(5).collect()
}

/// `HH:MM:SS` local time now. The standard library has no time zones.
pub fn clock_seconds() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as libc::time_t;
    // SAFETY: `tm` is plain data, and localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return "??:??:??".into();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Save `lines` in a new file below `dir` and return its path. The file is
/// private (0600), since it holds serial numbers and addresses, and an existing
/// file is never overwritten. Lines pass the same filter as the screen.
pub fn write(dir: &Path, host: &str, screen: &str, lines: &[String]) -> io::Result<PathBuf> {
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // A host name must not be able to leave the directory.
    let safe: String = host.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    for n in 0..100 {
        let extra = if n == 0 { String::new() } else { format!("-{n}") };
        let path = dir.join(format!("{safe}-{}-{ts}{extra}.txt", screen.to_ascii_lowercase()));
        let mut f = match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        };
        writeln!(f, "# tetron-bmctui {screen}, host {host}, unix time {ts}")?;
        for l in lines {
            writeln!(f, "{}", l.chars().map(safe_char).collect::<String>())?;
        }
        return Ok(path);
    }
    Err(io::Error::other("too many exports in one second"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn private_new_files_inside_the_directory() {
        let dir = std::env::temp_dir().join(format!("tetron-bmctui-export-{}", std::process::id()));
        let lines = vec!["a\x1b[2Jb".to_string()];
        let a = write(&dir, "../../etc", "Sensors", &lines).unwrap();
        let b = write(&dir, "../../etc", "Sensors", &lines).unwrap();
        assert!(a != b && a.parent() == Some(dir.as_path()));
        assert_eq!(std::fs::metadata(&a).unwrap().mode() & 0o777, 0o600);
        assert!(std::fs::read_to_string(&a).unwrap().ends_with("a?[2Jb\n"));
        assert_eq!((clock().len(), clock_seconds().len()), (5, 8));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
