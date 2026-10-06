use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let unique = SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock must follow the Unix epoch");
        let path =
            std::env::temp_dir().join(format!("n2linkd-credential-cli-{}-{}", std::process::id(), unique.as_nanos()));
        std::fs::create_dir(&path).expect("temporary credential CLI home must be created");
        Self(path)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_credential_command_failure_is_visible_and_returns_failure() {
    let home = TempHome::new();
    std::fs::write(home.0.join("flows.json"), b"[]").unwrap();
    std::fs::write(home.0.join("flows.json.prev"), b"[]").unwrap();
    std::fs::write(home.0.join("flows_cred.json"), br#"{"fixture":{"password":"fixture-secret"}}"#).unwrap();
    std::fs::write(home.0.join("flows_cred.json.prev"), b"{}").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_n2linkd"))
        .args(["credentials", "migrate"])
        .env("N2LINK_HOME", &home.0)
        .env_remove("N2LINK_CREDENTIAL_KEY")
        .env_remove("EDGELINK_CREDENTIAL_KEY")
        .output()
        .expect("credential command must start");

    let stderr = String::from_utf8(output.stderr).expect("credential command stderr must be UTF-8");
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("Application error: migration requires --backup-dir"), "stderr was: {stderr}");
    assert!(!stderr.contains("fixture-secret"));
}
