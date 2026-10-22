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
//!   Then B leaves and joins again while A stays, and both check again.
//! - `headless_voice_rules`: the game server configures `match` with
//!   channels, both players in one channel. Each instance checks the room's
//!   rules and the other's display name, hears the other; the game server
//!   moves B to another channel (both get `audibility_changed(false)` and
//!   hear nothing), then back (heard again). Run twice: server-spatialized
//!   (binding a node is refused) and client-spatialized (not secrecy-safe,
//!   the other is bound to a 3D node).

use std::{
    collections::HashMap,
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

/// This machine's load (Linux `/proc/loadavg`), printed at the start and the
/// end of each test: a slow run on a busy host is visible at a glance. The
/// instances print their own join timings (`INFO join_ms`).
fn host_load() -> String {
    fs::read_to_string("/proc/loadavg")
        .map(|load| load.trim().to_owned())
        .unwrap_or_else(|_| "n/a (no /proc/loadavg)".to_owned())
}

impl Drop for Setup {
    fn drop(&mut self) {
        eprintln!("host load at the end: {}", host_load());
    }
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
    eprintln!("host load at the start: {}", host_load());
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

/// A guest of the session.
async fn guest(session: &gridspawn_server::Session, name: &str) -> gridspawn_server::Participant {
    let key = gridspawn_server::IdempotencyKey::random();
    session
        .admit_guest(gridspawn_server::AdmitGuest::new(
            format!("fixture-{key}"),
            name,
            key,
        ))
        .await
        .expect("admit guest")
}

async fn ticket(guest: &gridspawn_server::Participant) -> String {
    guest
        .issue_ticket(&[gridspawn_server::Module::Voice])
        .await
        .expect("ticket")
        .as_str()
        .to_owned()
}

/// A guest of the session and a voice ticket for them.
async fn voice_ticket(session: &gridspawn_server::Session, name: &str) -> String {
    ticket(&guest(session, name).await).await
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
    let run = |ticket: String, tone: &'static str, expect: &'static str, role: &'static str| {
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
                    ("GRIDSPAWN_TEST_ROLE", role),
                ],
            )
            .await
        }
    };
    // A stays; B leaves and joins again. A must keep its voice (regression:
    // a listener whose only speaker left lost its voice on the next join).
    let ((code_a, output_a), (code_b, output_b)) = tokio::join!(
        run(ticket_a, "440", "660", "stay"),
        run(ticket_b, "660", "440", "rejoin")
    );
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
    // Each sees the other's display name (from the voice roster).
    let heard = |other: &'static str| {
        [
            "joined match",
            other,
            "network sent=true received=true",
            "hears_other_tone=true",
        ]
    };
    let (sees_b, sees_a) = (
        heard("participant_joined display_name=Voice B"),
        heard("participant_joined display_name=Voice A"),
    );
    let mut stay: Vec<&str> = sees_b.to_vec();
    stay.push("participant_left");
    stay.extend_from_slice(&sees_b[1..]);
    stay.extend_from_slice(&["left left", "done"]);
    let mut rejoin: Vec<&str> = sees_a.to_vec();
    rejoin.push("left left");
    rejoin.extend_from_slice(&sees_a);
    rejoin.extend_from_slice(&["left left", "done"]);
    for ((name, code, output), expected) in runs.into_iter().zip([stay, rejoin]) {
        let results = lines(output, "RESULT ");
        assert_eq!(code, 0, "Godot {name} failed:\n{output}");
        assert_eq!(results, expected, "Godot {name}");
    }
}

/// Runs Godot and streams its output lines (tagged) into `lines`.
fn spawn_godot(
    bin: &str,
    example: &Path,
    envs: &[(&str, String)],
    tag: &'static str,
    lines: tokio::sync::mpsc::UnboundedSender<(&'static str, String)>,
) -> tokio::process::Child {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut command = tokio::process::Command::new(bin);
    command
        .arg("--headless")
        .arg("--path")
        .arg(example)
        .arg("res://tests/voice_rules_test.tscn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("Godot could not start");
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let lines = lines.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stream).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let _ = lines.send((tag, line));
            }
        });
    }
    child
}

/// One run of the rules test; `spatial` is "server" or "client".
async fn rules_run(spatial: &'static str) {
    use gridspawn_server::{VoiceConfig, VoicePlayerState, VoiceRoom};
    let Some(setup) = setup().await else { return };
    let voice = setup.session.voice();
    let room = if spatial == "client" {
        VoiceRoom::channels().spatial_client()
    } else {
        VoiceRoom::channels()
    };
    voice
        .configure(&VoiceConfig::new().room("match", room))
        .await
        .expect("configure");
    let (a, b) = (
        guest(&setup.session, "Voice A").await,
        guest(&setup.session, "Voice B").await,
    );
    let red = VoicePlayerState::new().channels(["team:red"]);
    let blue = VoicePlayerState::new().channels(["team:blue"]);
    for player in [&a, &b] {
        voice
            .set_player_state(&player.id, &red)
            .await
            .expect("state");
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let env = |ticket: String, tone: &str, expect: &str| {
        vec![
            ("GRIDSPAWN_TEST_APP_ID", setup.app_id.clone()),
            ("GRIDSPAWN_TEST_TICKET", ticket),
            ("GRIDSPAWN_TEST_TONE_HZ", tone.to_owned()),
            ("GRIDSPAWN_TEST_EXPECT_HZ", expect.to_owned()),
            ("GRIDSPAWN_TEST_SPATIAL", spatial.to_owned()),
        ]
    };
    let mut godot_a = spawn_godot(
        &setup.bin,
        &setup.example,
        &env(ticket(&a).await, "440", "660"),
        "A",
        tx.clone(),
    );
    let mut godot_b = spawn_godot(
        &setup.bin,
        &setup.example,
        &env(ticket(&b).await, "660", "440"),
        "B",
        tx.clone(),
    );
    drop(tx);
    let mut output: HashMap<&str, Vec<String>> = HashMap::new();
    let phase_done = |output: &HashMap<&str, Vec<String>>, marker: &str| {
        ["A", "B"].iter().all(|who| {
            output
                .get(who)
                .is_some_and(|lines| lines.iter().any(|l| l.trim() == marker))
        })
    };
    let (mut moved, mut back) = (false, false);
    let collect = async {
        while let Some((who, line)) = rx.recv().await {
            if line.contains("RESULT") || line.contains("INFO") || line.contains("ERROR") {
                eprintln!("{spatial} {who}: {}", line.trim());
            }
            output.entry(who).or_default().push(line);
            if !moved && phase_done(&output, "RESULT phase1_done") {
                moved = true;
                voice.set_player_state(&b.id, &blue).await.expect("move B");
                eprintln!("harness: B moved to team:blue");
            }
            if !back && phase_done(&output, "RESULT phase2_done") {
                back = true;
                voice
                    .set_player_state(&b.id, &red)
                    .await
                    .expect("move B back");
                eprintln!("harness: B back to team:red");
            }
        }
    };
    let finished = tokio::time::timeout(Duration::from_secs(200), collect)
        .await
        .is_ok();
    if !finished {
        for (who, text) in &output {
            let _ = fs::write(
                profile_dir().join(format!("godot-rules-{spatial}-{who}.log")),
                text.join("\n"),
            );
        }
        panic!("the rules test ({spatial}) did not finish within 200 s");
    }
    let codes = (
        godot_a.wait().await.expect("A").code(),
        godot_b.wait().await.expect("B").code(),
    );
    let room_line = if spatial == "client" {
        "room managed=true mode=channels spatial=client secrecy_safe=false is_secrecy_safe=false"
    } else {
        "room managed=true mode=channels spatial=server secrecy_safe=true is_secrecy_safe=true"
    };
    let bind_line = if spatial == "client" {
        "bound"
    } else {
        "bind_refused voice/not_client_spatial"
    };
    for ((who, code), other) in [("A", codes.0), ("B", codes.1)]
        .into_iter()
        .zip(["Voice B", "Voice A"])
    {
        let text = output.get(who).cloned().unwrap_or_default().join("\n");
        let _ = fs::write(
            profile_dir().join(format!("godot-rules-{spatial}-{who}.log")),
            &text,
        );
        for line in lines(&text, "INFO ") {
            eprintln!("{spatial} {who} INFO {line}");
        }
        let results = lines(&text, "RESULT ");
        eprintln!("{spatial} {who} RESULT {}", results.join(" | "));
        assert_eq!(code, Some(0), "Godot {who} failed:\n{text}");
        // Order-independent: the room, the name and the binding arrive as they come.
        let name_line = format!("participant_joined display_name={other}");
        for wanted in [room_line, name_line.as_str(), bind_line] {
            assert!(
                results.contains(&wanted),
                "{who}: missing {wanted:?} in {results:?}"
            );
        }
        // The phases, in order.
        let phases: Vec<&str> = results
            .iter()
            .copied()
            .filter(|l| {
                l.starts_with("hears_other_tone")
                    || l.starts_with("phase")
                    || l.starts_with("audibility")
                    || l.starts_with("left")
                    || *l == "done"
                    || l.starts_with("joined")
            })
            .collect();
        assert_eq!(
            phases,
            [
                "joined match",
                "hears_other_tone=true",
                "phase1_done",
                "audibility audible=false",
                "hears_other_tone=false",
                "phase2_done",
                "audibility audible=true",
                "hears_other_tone=true",
                "left left",
                "done",
            ],
            "{who}"
        );
    }
}

#[tokio::test]
async fn headless_voice_rules() {
    rules_run("server").await;
    rules_run("client").await;
}
