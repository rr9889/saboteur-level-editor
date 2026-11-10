use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sab_tools::stage::{sha256, write_scripts, Dest};

struct Fixture {
    root: PathBuf,
    game: PathBuf,
    stage: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!("sab_stage_scripts_{}_{}_{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)));
        let game = root.join("synthetic game");
        let stage = root.join("staged package");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(game.join("source.pack"), b"synthetic source").unwrap();
        Self { root, game, stage }
    }

    fn destination(&self, name: &str, original: Option<&[u8]>) -> Dest {
        std::fs::write(self.stage.join(name), b"synthetic edited pack").unwrap();
        if let Some(bytes) = original {
            std::fs::write(self.game.join(name), bytes).unwrap();
        }
        Dest { rel: name.into(), existed: original.is_some(), original_sha: original.map(sha256),
            staged_name: name.into(), installed_sha: sha256(b"synthetic edited pack") }
    }

    fn generate(&self, destinations: &[Dest]) {
        write_scripts(&self.stage, &self.game, "fixture", destinations,
            &[("source.pack".into(), sha256(b"synthetic source"))], serde_json::json!({"synthetic": true})).unwrap();
    }

    #[cfg(windows)]
    fn run(&self, script: &str) -> std::process::Output {
        std::process::Command::new("pwsh").args(["-NoProfile", "-NonInteractive", "-File"])
            .arg(self.stage.join(script)).output().expect("PowerShell 7 (pwsh) must be on PATH for script execution tests")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let resolved = self.root.canonicalize().unwrap();
        let temporary = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(resolved.parent(), Some(temporary.as_path()));
        assert!(resolved.file_name().unwrap().to_string_lossy().starts_with("sab_stage_scripts_"));
        std::fs::remove_dir_all(resolved).unwrap();
    }
}

#[test]
fn installer_checks_every_staged_file_before_the_first_destination_write() {
    let fixture = Fixture::new();
    fixture.generate(&[fixture.destination("first.pack", None), fixture.destination("second.pack", None)]);
    let script = std::fs::read_to_string(fixture.stage.join("install.ps1")).unwrap();
    let first_write = script.find("Copy-Item").unwrap();
    for name in ["first.pack", "second.pack"] {
        let check = format!("if ((Get-Sha256 (Join-Path $PSScriptRoot '{name}')) -ne '");
        assert!(script.find(&check).is_some_and(|offset| offset < first_write), "missing pre-write hash check for {name}");
    }
}

#[cfg(windows)]
#[test]
fn missing_or_corrupt_staged_files_leave_destinations_and_backups_untouched() {
    for original in [None, Some(b"synthetic original".as_slice())] {
        for missing in [false, true] {
            let fixture = Fixture::new();
            fixture.generate(&[fixture.destination("patch.pack", original)]);
            if missing {
                std::fs::remove_file(fixture.stage.join("patch.pack")).unwrap();
            } else {
                std::fs::write(fixture.stage.join("patch.pack"), b"corrupt staging").unwrap();
            }
            let output = fixture.run("install.ps1");
            assert!(!output.status.success(), "invalid staging must fail");
            assert!(String::from_utf8_lossy(&output.stderr).contains("staged file"), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(std::fs::read(fixture.game.join("patch.pack")).ok().as_deref(), original);
            assert!(!fixture.game.join("patch.pack.fixture.bak").exists());
            assert_eq!(std::fs::read(fixture.game.join("source.pack")).unwrap(), b"synthetic source");
        }
    }
}

#[cfg(windows)]
#[test]
fn corrupt_later_artifact_does_not_install_an_earlier_destination() {
    let fixture = Fixture::new();
    fixture.generate(&[fixture.destination("first.pack", None), fixture.destination("second.pack", None)]);
    std::fs::write(fixture.stage.join("second.pack"), b"corrupt later artifact").unwrap();
    let output = fixture.run("install.ps1");
    assert!(!output.status.success());
    assert!(!fixture.game.join("first.pack").exists());
    assert!(!fixture.game.join("second.pack").exists());
}

#[cfg(windows)]
#[test]
fn valid_single_file_install_and_restore_are_hash_checked_and_repeatable() {
    for original in [None, Some(b"synthetic original".as_slice())] {
        let fixture = Fixture::new();
        fixture.generate(&[fixture.destination("patch.pack", original)]);
        for _attempt in 0..2 {
            let output = fixture.run("install.ps1");
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(std::fs::read(fixture.game.join("patch.pack")).unwrap(), b"synthetic edited pack");
        }
        for _attempt in 0..2 {
            let output = fixture.run("restore.ps1");
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(std::fs::read(fixture.game.join("patch.pack")).ok().as_deref(), original);
            assert!(!fixture.game.join("patch.pack.fixture.bak").exists());
        }
    }
}

#[cfg(windows)]
#[test]
fn restore_preserves_an_unrecognized_destination_instead_of_forcing_cleanup() {
    let fixture = Fixture::new();
    fixture.generate(&[fixture.destination("patch.pack", None)]);
    assert!(fixture.run("install.ps1").status.success());
    std::fs::write(fixture.game.join("patch.pack"), b"unknown user contents").unwrap();
    let output = fixture.run("restore.ps1");
    assert!(!output.status.success());
    assert_eq!(std::fs::read(fixture.game.join("patch.pack")).unwrap(), b"unknown user contents");
}
