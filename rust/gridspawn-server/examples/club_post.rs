//! Post to your game's club: the two-call quickstart.
//!
//! GRIDSPAWN_APP_ID=game-… GRIDSPAWN_ENVIRONMENT=development \
//! GRIDSPAWN_CREDENTIAL_FILE=/path/to/0600-key-file \
//! cargo run --example club_post -- patch-notes "Patch 1.2 is out" "Fixes and a new map."

use gridspawn_server::{Client, Credential, Environment, IdempotencyKey, Post};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_id = std::env::var("GRIDSPAWN_APP_ID")?;
    let environment: Environment = std::env::var("GRIDSPAWN_ENVIRONMENT")?.parse()?;
    let credential = Credential::from_file(std::env::var("GRIDSPAWN_CREDENTIAL_FILE")?)?;
    let mut args = std::env::args().skip(1);
    let feature = args.next().unwrap_or_else(|| "patch-notes".into());
    let title = args.next().unwrap_or_else(|| "Patch 1.2 is out".into());
    let content = args.next().unwrap_or_else(|| "Fixes and a new map.".into());

    let client = Client::builder(app_id, environment, credential)
        .connect()
        .await?;
    let receipt = client
        .post(
            &feature,
            Post::titled(title, content),
            &IdempotencyKey::random(),
        )
        .await?;
    println!("posted {} (duplicate: {})", receipt.id, receipt.duplicate);
    Ok(())
}
