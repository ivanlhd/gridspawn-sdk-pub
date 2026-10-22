//! Compiles examples/c/club_post.c with the system cc against the static
//! library built by this `cargo test`, and runs it against a mock server
//! (and, under gate 1, against the live test application).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
};

use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

const CRED: &str = "gsc1.gsc_test.AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const POSTS: &str = "/core/v1/game-servers/posts";

/// target/<profile>, from this test binary's own path (target/<profile>/deps/…).
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().to_path_buf()
}

/// Compiles once per test process: the tests run in parallel threads and
/// would otherwise write the same binary while another one runs it.
fn compile() -> PathBuf {
    static BINARY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BINARY.get_or_init(compile_example).clone()
}

fn compile_example() -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let profile = profile_dir();
    let library = profile.join("libgridspawn_server_capi.a");
    assert!(
        library.exists(),
        "{} is missing (it is built by `cargo test` because the crate lists staticlib)",
        library.display()
    );
    let out_dir = profile.join("c-example");
    std::fs::create_dir_all(&out_dir).unwrap();
    let binary = out_dir.join("club_post");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let output = Command::new(&cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(crate_dir.join("include"))
        .arg(crate_dir.join("examples/c/club_post.c"))
        .arg(&library)
        .args(["-lpthread", "-ldl", "-lm", "-lrt", "-o"])
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("the C compiler `{cc}` could not run: {error}"));
    assert!(
        output.status.success(),
        "cc failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}

fn credential_file(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let file = dir.join("credential");
    std::fs::write(&file, format!("{CRED}\n")).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    file
}

fn run(binary: &Path, env: &[(&str, String)], args: &[&str]) -> Output {
    let mut command = Command::new(binary);
    command.env_clear().args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_c_example_posts_against_a_mock_server() {
    let binary = compile();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/bootstrap"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "application": {"appId": "test-app"},
            "environment": "development", "protocol": {"selected": 1}, "modules": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/server/grants/club"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accessToken": "grant-token", "tokenType": "Bearer", "expiresIn": 300, "scope": "club.post"
        })))
        .mount(&server)
        .await;
    let seen = Arc::new(Mutex::new(HashSet::new()));
    Mock::given(method("POST"))
        .and(path(POSTS))
        .respond_with(move |request: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["feature"] != "patch-notes" {
                return ResponseTemplate::new(404)
                    .set_body_json(json!({"error": "feature_not_found"}));
            }
            let key = request.headers["idempotency-key"]
                .to_str()
                .unwrap()
                .to_owned();
            let duplicate = !seen.lock().unwrap().insert(key);
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": true, "id": "gs-1", "duplicate": duplicate}))
        })
        .mount(&server)
        .await;

    let dir = binary.parent().unwrap().to_path_buf();
    let env = vec![
        ("GRIDSPAWN_APP_ID", "test-app".to_owned()),
        ("GRIDSPAWN_ENVIRONMENT", "development".to_owned()),
        (
            "GRIDSPAWN_CREDENTIAL_FILE",
            credential_file(&dir).display().to_string(),
        ),
        ("GRIDSPAWN_SDK_BASE_URL", server.uri()),
        (
            "GRIDSPAWN_CLUB_POSTS_URL",
            format!("{}{POSTS}", server.uri()),
        ),
    ];
    let runs = {
        let (binary, env) = (binary.clone(), env.clone());
        tokio::task::spawn_blocking(move || {
            [
                run(&binary, &env, &["patch-notes", "patch-1.2-notes"]),
                run(&binary, &env, &["patch-notes", "patch-1.2-notes"]),
                run(&binary, &env, &["no-such-feature", "patch-1.2-notes"]),
            ]
        })
        .await
        .unwrap()
    };
    for (index, run) in runs.iter().enumerate() {
        println!(
            "run {index}: exit {:?}\n  stdout: {}\n  stderr: {}",
            run.status.code(),
            text(&run.stdout),
            text(&run.stderr)
        );
    }
    assert_eq!(runs[0].status.code(), Some(0));
    assert_eq!(text(&runs[0].stdout), "posted gs-1 (duplicate: false)");
    assert_eq!(runs[1].status.code(), Some(0));
    assert_eq!(text(&runs[1].stdout), "posted gs-1 (duplicate: true)");
    assert_eq!(runs[2].status.code(), Some(1));
    assert!(
        text(&runs[2].stderr).contains("code feature_not_found"),
        "{}",
        text(&runs[2].stderr)
    );

    let requests = server.received_requests().await.unwrap();
    let posts: Vec<_> = requests.iter().filter(|r| r.url.path() == POSTS).collect();
    assert_eq!(posts.len(), 3);
    let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(
        body,
        json!({"feature": "patch-notes", "title": "Patch 1.2 is out", "content": "Fixes and a new map."})
    );
    for request in &requests {
        assert!(
            !request.headers.contains_key("origin"),
            "Origin sent to {}",
            request.url
        );
    }
}

#[test]
fn the_c_example_against_the_live_test_application() {
    let credential = [
        "GRIDSPAWN_TEST_CREDENTIAL_FILE",
        "GRIDSPAWN_TEST_SERVER_CREDENTIAL_FILE",
    ]
    .iter()
    .find_map(|name| std::env::var(name).ok())
    .filter(|value| !value.is_empty());
    let app_id = std::env::var("GRIDSPAWN_TEST_APP_ID")
        .ok()
        .filter(|value| !value.is_empty());
    let (Some(credential), Some(app_id)) = (credential, app_id) else {
        eprintln!("skipped: GRIDSPAWN_TEST_CREDENTIAL_FILE / GRIDSPAWN_TEST_APP_ID not set");
        return;
    };
    let binary = compile();
    let output = run(
        &binary,
        &[
            ("GRIDSPAWN_APP_ID", app_id),
            ("GRIDSPAWN_ENVIRONMENT", "development".to_owned()),
            ("GRIDSPAWN_CREDENTIAL_FILE", credential),
        ],
        &["capi-live-check", "capi-live-check-key"],
    );
    let stderr = text(&output.stderr);
    println!("live: exit {:?}\n  stderr: {stderr}", output.status.code());
    // The test application has no game: the grant works and Core answers 404.
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr.contains("post failed: status 7"), "{stderr}");
    assert!(stderr.contains("code feature_not_found"), "{stderr}");
}
