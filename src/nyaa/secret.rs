//! qBittorrent Web UI password in the Secret Service (KWallet's `ksecretd` here), through the
//! `secret-tool` command. The password goes to the tool over stdin: never on a command line,
//! never in a file of ours, never in the repository, never logged.

use std::io::Write;
use std::process::{Command, Stdio};

const SERVICE: &str = "manga-reader";
const KIND: &str = "qbittorrent-webui";

fn attrs(user: &str) -> [String; 6] {
    ["service".into(), SERVICE.into(), "kind".into(), KIND.into(), "user".into(), user.into()]
}

pub fn store(user: &str, password: &str) -> Result<(), String> {
    let mut child = Command::new("secret-tool")
        .args(["store", "--label=Manga Reader: qBittorrent Web UI"])
        .args(attrs(user))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run secret-tool (install libsecret-tools): {e}"))?;
    child.stdin.take().ok_or("no stdin")?.write_all(password.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(format!("the keyring refused the password: {}", String::from_utf8_lossy(&out.stderr).trim())) }
}

pub fn lookup(user: &str) -> Option<String> {
    let out = Command::new("secret-tool").arg("lookup").args(attrs(user)).stderr(Stdio::null()).output().ok()?;
    let pw = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !pw.is_empty()).then_some(pw)
}

pub fn clear(user: &str) {
    let _ = Command::new("secret-tool").arg("clear").args(attrs(user)).stderr(Stdio::null()).status();
}
