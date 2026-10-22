//! Manual test: a person signs in with the system browser.
//!
//!   GRIDSPAWN_APP_ID=<your app id> [GRIDSPAWN_ENVIRONMENT=development] \
//!   cargo run --example sign_in
//!
//! The application needs a registered loopback redirect `http://127.0.0.1/callback`
//! and the `identity.basic` scope. Prints the display name only, makes a player
//! assertion, then signs out (which clears the stored session and revokes it).

use gridspawn_client::{Client, Environment, Error, Restore, SignIn, SignInError};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_id = std::env::var("GRIDSPAWN_APP_ID").map_err(|_| "set GRIDSPAWN_APP_ID")?;
    let environment: Environment = std::env::var("GRIDSPAWN_ENVIRONMENT")
        .unwrap_or_else(|_| "development".into())
        .parse()?;
    let client = Client::builder(app_id, environment).connect().await?;
    println!(
        "connected to {} ({environment}); sessions persist: {}",
        client.application_name(),
        client.persists_sessions()
    );

    let user = match client.restore().await? {
        Restore::SignedIn(user) => {
            println!("restored the stored session");
            user
        }
        Restore::SignedOut | Restore::InteractionRequired { .. } => {
            let request = SignIn::new(["identity.basic"]);
            let pending = match client.begin_sign_in(request.clone()).await {
                Err(Error::SignIn(SignInError::BrowserUnavailable)) => {
                    client.begin_sign_in(request.open_browser(false)).await?
                }
                other => other?,
            };
            println!(
                "Sign in in the browser. If it did not open, visit:\n{}",
                pending.authorization_url()
            );
            pending.finish().await?
        }
    };
    println!("signed in as {}", user.display_name);

    // A game server would send this challenge; any 32-256 URL-safe characters.
    let challenge = format!("example-{:x}", std::process::id()).repeat(4);
    let assertion = client.player_assertion(&challenge).await?;
    println!("assertion ok: {assertion:?}");

    client.sign_out().await?;
    println!("signed out");
    Ok(())
}
