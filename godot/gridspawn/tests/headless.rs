//! Headless Godot test of the addon. Skipped unless configured:
//!
//!   cargo build    # first: `cargo test` does not build the cdylib itself
//!   GRIDSPAWN_GODOT_BIN=/path/to/Godot_v4.3-stable_linux.x86_64 (or the Windows console exe)
//!   GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/0600-file \
//!   cargo test --test headless -- --nocapture --test-threads=1
//!
//! (One thread: both tests install the addon into the same example project.)
//!
//! The game server side (`gridspawn-server`) makes a session, guests and
//! voice tickets. The example project (`../example`) then runs headless with
//! the freshly built library:
//! - `headless_attach`: restore, an assertion while signed out, attach, renew,
//!   a forged ticket, and presence refused while signed out, through the
//!   GDScript API and signals;
//! - `headless_voice`: two instances join one voice room, each sending its own
//!   tone through a custom input (Godot's headless audio driver has no
//!   microphone), and each checks it receives and plays the other's tone.

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

struct Setup {
    bin: String,
    app_id: String,
    session: gridspawn_server::Session,
    _lease: gridspawn_server::RenewerHandle,
    example: PathBuf,
}

/// The Godot binary, a game session, and the example project with the addon
/// installed and imported. `None` when the test is not configured.
async fn setup() -> Option<Setup> {
    let (Some(bin), Some(app_id), Some(credential)) = (
        env("GRIDSPAWN_GODOT_BIN"),
        env("GRIDSPAWN_TEST_APP_ID"),
        env("GRIDSPAWN_TEST_CREDENTIAL_FILE"),
    ) else {
        eprintln!(
            "skipped: GRIDSPAWN_GODOT_BIN / GRIDSPAWN_TEST_APP_ID / GRIDSPAWN_TEST_CREDENTIAL_FILE not set"
        );
        return None;
    };
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
    let lease = session.spawn_renewer();

    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../example");
    install_addon(&example);
    let (code, output) = godot(&bin, &example, &["--import"], &[]).await;
    assert_eq!(code, 0, "import failed:\n{output}");
    Some(Setup {
        bin,
        app_id,
        session,
        _lease: lease,
        example,
    })
}

/// A guest of the session and a voice ticket for them.
async fn voice_ticket(session: &gridspawn_server::Session, name: &str) -> String {
    let key = gridspawn_server::IdempotencyKey::random();
    let guest = session
        .admit_guest(gridspawn_server::AdmitGuest::new(
            format!("fixture-{key}"),
            name,
            key,
        ))
        .await
        .expect("admit guest");
    guest
        .issue_ticket(&[gridspawn_server::Module::Voice])
        .await
        .expect("ticket")
        .as_str()
        .to_owned()
}

fn lines<'a>(output: &'a str, prefix: &str) -> Vec<&'a str> {
    output
        .lines()
        .filter_map(|line| line.trim().strip_prefix(prefix))
        .collect()
}

#[tokio::test]
async fn headless_attach() {
    let Some(setup) = setup().await else { return };
    let ticket = voice_ticket(&setup.session, "Fixture Guest").await;
    let (bin, app_id, example) = (&setup.bin, &setup.app_id, &setup.example);

    let (code, output) = godot(
        bin,
        example,
        &[],
        &[
            ("GRIDSPAWN_TEST_APP_ID", app_id.as_str()),
            ("GRIDSPAWN_TEST_TICKET", ticket.as_str()),
        ],
    )
    .await;
    let results = lines(&output, "RESULT ");
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
            "presence_failed sign_in/required",
            "done",
        ]
    );
}

#[tokio::test]
async fn headless_voice() {
    let Some(setup) = setup().await else { return };
    let ticket_a = voice_ticket(&setup.session, "Voice A").await;
    let ticket_b = voice_ticket(&setup.session, "Voice B").await;
    let run = |ticket: String, tone: &'static str, expect: &'static str| {
        let setup = &setup;
        async move {
            godot(
                &setup.bin,
                &setup.example,
                &["res://tests/voice_test.tscn"],
                &[
                    ("GRIDSPAWN_TEST_APP_ID", setup.app_id.as_str()),
                    ("GRIDSPAWN_TEST_TICKET", ticket.as_str()),
                    ("GRIDSPAWN_TEST_TONE_HZ", tone),
                    ("GRIDSPAWN_TEST_EXPECT_HZ", expect),
                ],
            )
            .await
        }
    };
    let ((code_a, output_a), (code_b, output_b)) =
        tokio::join!(run(ticket_a, "440", "660"), run(ticket_b, "660", "440"));
    let runs = [("A", code_a, &output_a), ("B", code_b, &output_b)];
    // Both outputs first, so a failure in A still shows B.
    for (name, _, output) in runs {
        // The whole Godot output, for diagnosis: target/<profile>/godot-voice-<name>.log
        let _ = fs::write(
            profile_dir().join(format!("godot-voice-{name}.log")),
            output,
        );
        for line in lines(output, "INFO ") {
            eprintln!("{name} INFO {line}");
        }
        eprintln!("{name} RESULT {}", lines(output, "RESULT ").join(" | "));
    }
    for (name, code, output) in runs {
        let results = lines(output, "RESULT ");
        assert_eq!(code, 0, "Godot {name} failed:\n{output}");
        assert_eq!(
            results,
            [
                "joined match",
                "participant_joined display_name_empty=true",
                "network sent=true received=true",
                "hears_other_tone=true",
                "left left",
                "done",
            ],
            "Godot {name}"
        );
    }
}
