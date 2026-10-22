//! Live voice checks against the SFU. Skipped unless configured:
//!
//!   GRIDSPAWN_TEST_APP_ID=… GRIDSPAWN_TEST_CREDENTIAL_FILE=/path/to/0600-file \
//!   cargo test --test live -- --nocapture --test-threads=1
//!
//! The application must allow guests and voice. `GRIDSPAWN_TEST_VOICE_LONG=1`
//! adds a 5.5-minute session that crosses SFU token refreshes and a voice
//! grant renewal.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gridspawn_client::{
    AttachedSession, Client, Environment, GrantEvent, MemoryTokenStore, Module,
};
use gridspawn_voice::{AudioOptions, TransmitMode, VoiceEvent, VoiceOptions, VoiceSession};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

struct Fixture {
    server: gridspawn_server::Session,
    client: Client,
}

async fn fixture() -> Option<Fixture> {
    let (Some(app_id), Some(path)) = (
        env("GRIDSPAWN_TEST_APP_ID"),
        env("GRIDSPAWN_TEST_CREDENTIAL_FILE"),
    ) else {
        eprintln!("skipped: GRIDSPAWN_TEST_APP_ID / GRIDSPAWN_TEST_CREDENTIAL_FILE not set");
        return None;
    };
    let server = gridspawn_server::Client::builder(
        app_id.clone(),
        gridspawn_server::Environment::Development,
        gridspawn_server::Credential::from_file(&path).expect("credential"),
    )
    .connect()
    .await
    .expect("server bootstrap");
    let key = gridspawn_server::IdempotencyKey::random();
    let session = server
        .create_session(gridspawn_server::CreateSession::new(
            format!("voice-live-{key}"),
            key,
        ))
        .await
        .expect("session");
    // Keep the lease alive for the whole test.
    std::mem::forget(session.spawn_renewer());
    let client = Client::builder(app_id, Environment::Development)
        .token_store(Arc::new(MemoryTokenStore::new()))
        .connect()
        .await
        .expect("client bootstrap");
    Some(Fixture {
        server: session,
        client,
    })
}

impl Fixture {
    async fn attach(&self, name: &str) -> AttachedSession {
        let key = gridspawn_server::IdempotencyKey::random();
        let guest = self
            .server
            .admit_guest(gridspawn_server::AdmitGuest::new(
                format!("voice-{key}"),
                name,
                key,
            ))
            .await
            .expect("guest");
        let ticket = guest
            .issue_ticket(&[gridspawn_server::Module::Voice])
            .await
            .expect("ticket");
        self.client.attach(ticket.as_str()).await.expect("attach")
    }

    async fn join(&self, name: &str) -> (AttachedSession, VoiceSession) {
        let started = Instant::now();
        let attached = self.attach(name).await;
        let attach_time = started.elapsed();
        let options = VoiceOptions::default()
            .transmit(TransmitMode::Continuous)
            // Tones are steady: noise suppression and gain control would reshape them.
            .audio(
                AudioOptions::default()
                    .echo_cancellation(false)
                    .noise_suppression(false)
                    .auto_gain(false),
            );
        let voice = VoiceSession::join(&self.client, &attached, options)
            .await
            .expect("voice join");
        let steps: Vec<String> = voice
            .join_timings()
            .iter()
            .map(|(step, took)| format!("{step} {}", took.as_millis()))
            .collect();
        eprintln!(
            "{name}: guest + ticket + attach {} ms; join {} ms: {}",
            attach_time.as_millis(),
            (started.elapsed() - attach_time).as_millis(),
            steps.join(", ")
        );
        (attached, voice)
    }
}

/// Sends a tone and records what each other participant sounds like, every 20 ms.
struct Pump {
    recorded: Arc<Mutex<HashMap<String, Vec<f32>>>>,
    task: tokio::task::JoinHandle<()>,
}

fn pump(voice: VoiceSession, hz: f32) -> Pump {
    let recorded: Arc<Mutex<HashMap<String, Vec<f32>>>> = Arc::default();
    let sink = recorded.clone();
    let task = tokio::spawn(async move {
        let mut phase = 0f32;
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        loop {
            ticker.tick().await;
            let tone: Vec<f32> = (0..960)
                .map(|_| {
                    phase += 2.0 * std::f32::consts::PI * hz / 48_000.0;
                    0.3 * phase.sin()
                })
                .collect();
            voice.push_capture(&tone);
            for participant in voice.participants() {
                let mut out = vec![0f32; 960];
                voice.pull_participant(&participant.id, &mut out);
                sink.lock()
                    .unwrap()
                    .entry(participant.id)
                    .or_default()
                    .extend(out);
            }
        }
    });
    Pump { recorded, task }
}

fn power(samples: &[f32], hz: f32) -> f32 {
    let k = 2.0 * (2.0 * std::f32::consts::PI * hz / 48_000.0).cos();
    let (mut s1, mut s2) = (0f32, 0f32);
    for &x in samples {
        let s0 = x + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - k * s1 * s2) / samples.len().max(1) as f32
}

/// The last `seconds` of what `pump` heard from `participant`.
fn tail(pump: &Pump, participant: &str, seconds: usize) -> Vec<f32> {
    let recorded = pump.recorded.lock().unwrap();
    let all = recorded.get(participant).cloned().unwrap_or_default();
    all[all.len().saturating_sub(48_000 * seconds)..].to_vec()
}

/// Tone power summed over Hann-windowed 20 ms frames: robust to the small
/// frequency drift of a parametric codec (unlike one long coherent window),
/// with little leakage between the test tones.
fn frame_power(samples: &[f32], hz: f32) -> f32 {
    let window: Vec<f32> = (0..960)
        .map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / 959.0).cos())
        .collect();
    samples
        .chunks_exact(960)
        .map(|frame| {
            let windowed: Vec<f32> = frame.iter().zip(&window).map(|(s, w)| s * w).collect();
            power(&windowed, hz)
        })
        .sum()
}

/// `expected` dominates every other test tone by 20 dB.
fn hears(samples: &[f32], expected: f32, others: &[f32]) -> bool {
    let p = frame_power(samples, expected);
    others
        .iter()
        .all(|o| p > 100.0 * frame_power(samples, *o).max(1e-12))
        && p > 1e-6
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<VoiceEvent>,
    want: impl Fn(&VoiceEvent) -> bool,
) -> VoiceEvent {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match events.recv().await {
                Ok(event) if want(&event) => return event,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("event within 15 s")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_speakers_mute_and_rejoin() {
    let Some(f) = fixture().await else { return };
    let started = Instant::now();
    let (_a_session, a) = f.join("Voice A").await;
    let (_b_session, b) = f.join("Voice B").await;
    let (_c_session, c) = f.join("Voice C").await;
    eprintln!("three joined in {:?}", started.elapsed());
    let (pa, pb, pc) = (
        pump(a.clone(), 440.0),
        pump(b.clone(), 600.0),
        pump(c.clone(), 800.0),
    );
    tokio::time::sleep(Duration::from_secs(6)).await;

    // Each hears the two others, each with its own tone, and not itself.
    for (pump, me, tones) in [
        (&pa, &a, [(b.local_id(), 600.0), (c.local_id(), 800.0)]),
        (&pb, &b, [(a.local_id(), 440.0), (c.local_id(), 800.0)]),
        (&pc, &c, [(a.local_id(), 440.0), (b.local_id(), 600.0)]),
    ] {
        let ids: Vec<String> = me.participants().into_iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 2, "{} sees {ids:?}", me.local_id());
        for (other, hz) in tones {
            let heard = tail(pump, other, 3);
            let rest: Vec<f32> = [440.0, 600.0, 800.0]
                .into_iter()
                .filter(|t| *t != hz)
                .collect();
            assert!(
                hears(&heard, hz, &rest),
                "{} does not hear {hz} Hz from {other}",
                me.local_id()
            );
        }
    }
    let stats = b.stats();
    eprintln!("B stats after 6 s: {stats:?}");
    assert!(stats.packets_received[a.local_id()] > 200);
    assert!(stats.packets_sent > 200);

    // Mute A: no more packets from A reach B; unmute resumes.
    a.set_muted(true).await.expect("mute");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let before = b.stats().packets_received[a.local_id()];
    tokio::time::sleep(Duration::from_secs(2)).await;
    let after = b.stats().packets_received[a.local_id()];
    eprintln!("muted: B received {} packets from A in 2 s", after - before);
    assert_eq!(after, before, "a muted speaker must deliver zero packets");
    a.set_muted(false).await.expect("unmute");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        b.stats().packets_received[a.local_id()] > after + 50,
        "unmute resumes"
    );

    // A leaves: B is told; A joins again with a new attachment and is heard.
    let mut b_events = b.events();
    let a_id = a.local_id().to_owned();
    pa.task.abort();
    a.leave().await;
    wait_for(
        &mut b_events,
        |e| matches!(e, VoiceEvent::ParticipantLeft { participant } if *participant == a_id),
    )
    .await;
    eprintln!("B saw A leave");
    let (_a2_session, a2) = f.join("Voice A again").await;
    let a2_id = a2.local_id().to_owned();
    wait_for(
        &mut b_events,
        |e| matches!(e, VoiceEvent::ParticipantJoined { participant } if *participant == a2_id),
    )
    .await;
    let pa2 = pump(a2.clone(), 440.0);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        hears(&tail(&pb, &a2_id, 2), 440.0, &[600.0, 800.0]),
        "B hears A after the rejoin"
    );
    eprintln!("rejoin ok; total {:?}", started.elapsed());

    for p in [pa2, pb, pc] {
        p.task.abort();
    }
    for v in [a2, b, c] {
        v.leave().await;
    }
}

/// A listener whose only remote speaker leaves keeps its voice: the speaker
/// rejoins and is heard, and a third player who joins later is heard too
/// (regression: removing the last receive section broke renegotiation).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn listener_survives_its_only_speaker_leaving() {
    let Some(f) = fixture().await else { return };
    let (_a_session, a) = f.join("Voice A").await;
    let mut a_events = a.events();
    let watch = a.clone();
    let log = tokio::spawn(async move {
        let mut events = watch.events();
        while let Ok(event) = events.recv().await {
            eprintln!("A event: {event:?}");
        }
    });
    let pa = pump(a.clone(), 440.0);

    // B joins, is heard, and leaves: A's receive leg loses its only section.
    let (_b_session, b) = f.join("Voice B").await;
    let b_id = b.local_id().to_owned();
    wait_for(
        &mut a_events,
        |e| matches!(e, VoiceEvent::ParticipantJoined { participant } if *participant == b_id),
    )
    .await;
    let pb = pump(b.clone(), 600.0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        hears(&tail(&pa, &b_id, 2), 600.0, &[440.0, 800.0]),
        "A hears B"
    );
    pb.task.abort();
    b.leave().await;
    wait_for(
        &mut a_events,
        |e| matches!(e, VoiceEvent::ParticipantLeft { participant } if *participant == b_id),
    )
    .await;
    eprintln!("A saw B leave");

    // B joins again: A adds a section after the removal and hears B.
    let (_b2_session, b2) = f.join("Voice B again").await;
    let b2_id = b2.local_id().to_owned();
    wait_for(
        &mut a_events,
        |e| matches!(e, VoiceEvent::ParticipantJoined { participant } if *participant == b2_id),
    )
    .await;
    let pb2 = pump(b2.clone(), 600.0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        hears(&tail(&pa, &b2_id, 2), 600.0, &[440.0, 800.0]),
        "A hears B after B rejoined"
    );
    eprintln!("A hears B after the rejoin");

    // B leaves again, then C joins: A hears C.
    pb2.task.abort();
    b2.leave().await;
    wait_for(
        &mut a_events,
        |e| matches!(e, VoiceEvent::ParticipantLeft { participant } if *participant == b2_id),
    )
    .await;
    let (_c_session, c) = f.join("Voice C").await;
    let c_id = c.local_id().to_owned();
    wait_for(
        &mut a_events,
        |e| matches!(e, VoiceEvent::ParticipantJoined { participant } if *participant == c_id),
    )
    .await;
    let pc = pump(c.clone(), 800.0);
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        hears(&tail(&pa, &c_id, 2), 800.0, &[440.0, 600.0]),
        "A hears C, who joined after B left"
    );
    eprintln!("A hears C after B left");

    // A never failed.
    while let Ok(event) = a_events.try_recv() {
        assert!(
            !matches!(event, VoiceEvent::Error { .. } | VoiceEvent::Left { .. }),
            "A's session failed: {event:?}"
        );
    }
    log.abort();
    for p in [pa, pc] {
        p.task.abort();
    }
    for v in [a, c] {
        v.leave().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_session_renews() {
    if env("GRIDSPAWN_TEST_VOICE_LONG").is_none() {
        eprintln!("skipped: GRIDSPAWN_TEST_VOICE_LONG not set");
        return;
    }
    let Some(f) = fixture().await else { return };
    let (a_session, a) = f.join("Long A").await;
    let (_b_session, b) = f.join("Long B").await;
    let mut grant_events = a_session.subscribe();
    let mut a_events = a.events();
    let (pa, pb) = (pump(a.clone(), 440.0), pump(b.clone(), 600.0));
    let started = Instant::now();
    let mut grant_renewals = 0;
    while started.elapsed() < Duration::from_secs(330) {
        tokio::time::sleep(Duration::from_secs(30)).await;
        while let Ok(event) = grant_events.try_recv() {
            if matches!(event, GrantEvent::Renewed(Module::Voice)) {
                grant_renewals += 1;
            }
        }
        while let Ok(event) = a_events.try_recv() {
            eprintln!("A event: {event:?}");
            if let VoiceEvent::Error { code, message } = event {
                panic!("voice error during the long run: {code} {message}");
            }
        }
        eprintln!("A auth renewals so far: {}", a.stats().auth_renewals);
        let heard = tail(&pb, a.local_id(), 5);
        assert!(
            hears(&heard, 440.0, &[600.0]),
            "B still hears A at {:?}",
            started.elapsed()
        );
        eprintln!(
            "{:>4}s: A sent {}, B received {} from A, voice grant renewals {grant_renewals}",
            started.elapsed().as_secs(),
            a.stats().packets_sent,
            b.stats().packets_received[a.local_id()]
        );
    }
    let renewals = a.stats().auth_renewals;
    eprintln!(
        "SFU authorization renewals in {:?}: {renewals}",
        started.elapsed()
    );
    assert!(renewals >= 5, "refreshAuth cycles: {renewals}");
    assert!(
        grant_renewals >= 1,
        "the voice grant was renewed during the run"
    );
    for p in [pa, pb] {
        p.task.abort();
    }
    a.leave().await;
    b.leave().await;
}
