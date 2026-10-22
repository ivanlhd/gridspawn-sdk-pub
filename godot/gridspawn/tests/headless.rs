//! Headless Godot test of the addon. Skipped unless configured:
//!
//!   cargo build    # first: `cargo test` does not build the cdylib itself
//!   GRIDSPAWN_GODOT_BIN=/path/to/Godot_v4.3-stable_linux.x86_64 (or the Windows console exe)
//!   GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/0600-file \
//!   cargo test --test headless -- --nocapture
//!
//! The game server side (`gridspawn-server`) makes a session, a guest and a
//! voice ticket. The example project (`../example`) then runs headless with the
//! freshly built library and checks restore, an assertion while signed out,
//! attach, renew, and a forged ticket, through the GDScript API and signals.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// `target/<profile>/`, where Cargo put the cdylib.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("test path");
    exe.parent()
        .and_then(Path::parent)
        .expect("target dir")
        .to_path_buf()
}

fn library() -> (PathBuf, &'static str) {
    if cfg!(windows) {
        (
            profile_dir().join("gridspawn_godot.dll"),
            "gridspawn_godot.windows.x86_64.dll",
        )
    } else {
        (
            profile_dir().join("libgridspawn_godot.so"),
            "libgridspawn_godot.linux.x86_64.so",
        )
    }
}

/// Copies the addon (and the built library) into the example project.
fn install_addon(example: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("addons/gridspawn");
    let target = example.join("addons/gridspawn");
    let _ = fs::remove_dir_all(&target);
    fs::create_dir_all(target.join("bin")).unwrap();
    for entry in fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            fs::copy(entry.path(), target.join(entry.file_name())).unwrap();
        }
    }
    let (built, packaged) = library();
    assert!(
        built.exists(),
        "{} is missing: build the crate first",
        built.display()
    );
    fs::copy(built, target.join("bin").join(packaged)).unwrap();
}

async fn godot(bin: &str, example: &Path, args: &[&str], envs: &[(&str, &str)]) -> (i32, String) {
    let mut command = tokio::process::Command::new(bin);
    command
        .arg("--headless")
        .arg("--path")
        .arg(example)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = tokio::time::timeout(Duration::from_secs(180), command.output())
        .await
        .expect("Godot did not finish within 180 s")
        .expect("Godot could not start");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code().unwrap_or(-1), text)
}

#[tokio::test]
async fn headless_attach() {
    let (Some(bin), Some(app_id), Some(credential)) = (
        env("GRIDSPAWN_GODOT_BIN"),
        env("GRIDSPAWN_TEST_APP_ID"),
        env("GRIDSPAWN_TEST_CREDENTIAL_FILE"),
    ) else {
        eprintln!(
            "skipped: GRIDSPAWN_GODOT_BIN / GRIDSPAWN_TEST_APP_ID / GRIDSPAWN_TEST_CREDENTIAL_FILE not set"
        );
        return;
    };

    // The game server's side: a session, a guest, a voice ticket.
    let server = gridspawn_server::Client::builder(
        app_id.clone(),
        gridspawn_server::Environment::Development,
        gridspawn_server::Credential::from_file(&credential).expect("credential"),
    )
    .connect()
    .await
    .expect("server bootstrap");
    let key = gridspawn_server::IdempotencyKey::random();
    let session = server
        .create_session(gridspawn_server::CreateSession::new(
            format!("godot-headless-{key}"),
            key,
        ))
        .await
        .expect("create session");
    let _lease = session.spawn_renewer();
    let guest_key = gridspawn_server::IdempotencyKey::random();
    let guest = session
        .admit_guest(gridspawn_server::AdmitGuest::new(
            format!("fixture-{guest_key}"),
            "Fixture Guest",
            guest_key,
        ))
        .await
        .expect("admit guest");
    let ticket = guest
        .issue_ticket(&[gridspawn_server::Module::Voice])
        .await
        .expect("ticket");

    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../example");
    install_addon(&example);
    let (code, output) = godot(&bin, &example, &["--import"], &[]).await;
    assert_eq!(code, 0, "import failed:\n{output}");

    let (code, output) = godot(
        &bin,
        &example,
        &[],
        &[
            ("GRIDSPAWN_TEST_APP_ID", app_id.as_str()),
            ("GRIDSPAWN_TEST_TICKET", ticket.as_str()),
        ],
    )
    .await;
    let results: Vec<&str> = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("RESULT "))
        .collect();
    eprintln!("{}", results.join("\n"));
    assert_eq!(code, 0, "Godot run failed:\n{output}");
    assert_eq!(
        results,
        [
            "signed_out signed_in=false persist=false",
            "assertion_failed sign_in/required",
            "attached session=true name=Fixture Guest capabilities=voice",
            "grant_renewed voice",
            "attach_failed session/ticket_invalid",
            "done",
        ]
    );
}
