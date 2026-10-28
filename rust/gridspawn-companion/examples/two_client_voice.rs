//! Live test of the companion end to end (Windows): plays the map-server with
//! `gridspawn-server`, and runs two copies of the 32-bit example client
//! (`rathena_client.exe`, which loads gridspawn_rathena.dll, which starts a
//! companion each).
//!
//! 1. A party session: guests "Aria" and "Bram" in `party:1`, a voice ticket
//!    each; both clients attach, join voice and send a test tone, so each must
//!    see the other start speaking.
//! 2. Group switch: after a while Aria gets a ticket for a guild session and
//!    her client attaches it (`on_detached("replaced")`, then attached and in
//!    voice again).
//!
//!   set GRIDSPAWN_TEST_APP_ID=...            (an app with guests and voice on)
//!   set GRIDSPAWN_TEST_CREDENTIAL_FILE=...   (its server key)
//!   cargo run --release --example two_client_voice -- <path\to\rathena_client.exe>

use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use gridspawn_server::{
    AdmitGuest, Client, CreateSession, Credential, Environment, IdempotencyKey, Module, Session,
    VoiceConfig, VoicePlayerState,
};

const CONFIG: &str = r#"{"rooms": {"match": {"mode": "channels", "spatial": "client"}}}"#;

fn key(text: String) -> IdempotencyKey {
    IdempotencyKey::new(text).expect("idempotency key")
}

async fn group(client: &Client, name: &str, stamp: u64) -> Session {
    let session = client
        .create_session(
            CreateSession::new(
                format!("companion-test-{name}"),
                key(format!("companion-test-{name}-{stamp}")),
            )
            .max_participants(64),
        )
        .await
        .expect("create session");
    session
        .voice()
        .configure(&VoiceConfig::from_json(CONFIG).expect("config"))
        .await
        .expect("configure voice");
    session
}

async fn ticket(
    session: &Session,
    account: &str,
    character: &str,
    channel: &str,
    stamp: u64,
) -> String {
    let player = session
        .admit_guest(AdmitGuest::new(
            account,
            character,
            key(format!("companion-test-{account}-{channel}-{stamp}")),
        ))
        .await
        .expect("admit guest");
    session
        .voice()
        .set_player_state(&player.id, &VoicePlayerState::new().channels([channel]))
        .await
        .expect("voice state");
    player
        .issue_ticket(&[Module::Voice])
        .await
        .expect("ticket")
        .as_str()
        .to_owned()
}

#[tokio::main]
async fn main() {
    let exe = PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: two_client_voice <rathena_client.exe>"),
    );
    let app_id = std::env::var("GRIDSPAWN_TEST_APP_ID").expect("GRIDSPAWN_TEST_APP_ID");
    let key_file =
        std::env::var("GRIDSPAWN_TEST_CREDENTIAL_FILE").expect("GRIDSPAWN_TEST_CREDENTIAL_FILE");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let client = Client::builder(
        app_id.clone(),
        Environment::Development,
        Credential::from_file_unchecked(&key_file).expect("credential"),
    )
    .connect()
    .await
    .expect("connect");

    let party = group(&client, "party-1", stamp).await;
    let _party_renewer = party.spawn_renewer();
    let guild = group(&client, "guild-7", stamp).await;
    let _guild_renewer = guild.spawn_renewer();
    let aria = ticket(&party, "aid-test-1", "Aria", "party:1", stamp).await;
    let bram = ticket(&party, "aid-test-2", "Bram", "party:1", stamp).await;
    println!("sessions and tickets ready (tickets not printed)");

    let dir = exe.parent().expect("exe directory").to_path_buf();
    let next = dir.join("aria-next-ticket.txt");
    let _ = std::fs::remove_file(&next);
    let spawn = |ticket: &str, seconds: &str, log: &str, extra: &[&str]| {
        let mut command = Command::new(&exe);
        command
            .current_dir(&dir)
            .env("GRIDSPAWN_APP_ID", &app_id)
            .env("GRIDSPAWN_ENVIRONMENT", "development")
            .args([ticket, "--tone", "--seconds", seconds, "--log"])
            .arg(dir.join(log))
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command.spawn().expect("start rathena_client.exe")
    };
    let next_arg = next.display().to_string();
    let a = spawn(
        &aria,
        "40",
        "companion-aria.log",
        &["--next-ticket-file", &next_arg],
    );
    let b = spawn(&bram, "25", "companion-bram.log", &[]);

    tokio::time::sleep(Duration::from_secs(18)).await;
    let switched = ticket(&guild, "aid-test-1", "Aria", "guild:7", stamp).await;
    std::fs::write(&next, switched).expect("write the next ticket");
    println!("Aria's guild ticket handed over");

    let (a, b) = tokio::task::spawn_blocking(move || (a.wait_with_output(), b.wait_with_output()))
        .await
        .expect("join");
    let (a, b) = (a.expect("aria"), b.expect("bram"));
    let text_a = String::from_utf8_lossy(&a.stdout).to_string();
    let text_b = String::from_utf8_lossy(&b.stdout).to_string();
    println!(
        "===== Aria's client (exit {:?})\n{text_a}{}",
        a.status.code(),
        String::from_utf8_lossy(&a.stderr)
    );
    println!(
        "===== Bram's client (exit {:?})\n{text_b}{}",
        b.status.code(),
        String::from_utf8_lossy(&b.stderr)
    );

    let checks = [
        ("Aria heard Bram", text_a.contains("Bram started speaking")),
        ("Bram heard Aria", text_b.contains("Aria started speaking")),
        ("Aria saw Bram join", text_a.contains("player joined: Bram")),
        (
            "Aria's switch detached the party",
            text_a.contains("detached: replaced"),
        ),
        (
            "Aria attached twice",
            text_a.matches("attached as Aria").count() == 2,
        ),
        (
            "Aria joined voice twice",
            text_a.matches("voice: joined").count() == 2,
        ),
        (
            "Aria left voice once per session",
            text_a.matches("voice: left").count() == 2,
        ),
        (
            "Bram saw Aria leave, by name",
            text_b.contains("player left: Aria"),
        ),
        (
            "both detached on request",
            text_a.contains("detached: requested") && text_b.contains("detached: requested"),
        ),
        (
            "both shut down cleanly",
            text_a.contains("shutdown: 0") && text_b.contains("shutdown: 0"),
        ),
    ];
    let mut ok = true;
    for (what, passed) in checks {
        println!("{} {what}", if passed { "PASS" } else { "FAIL" });
        ok &= passed;
    }
    std::process::exit(if ok { 0 } else { 1 });
}
