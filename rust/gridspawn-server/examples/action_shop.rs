//! A small shop that answers game actions (protocol
//! docs/protocol/07-game-actions.md).
//!
//! It receives `shop.buy`, keeps each player's gold in memory (keyed by the
//! pairwise subject), and answers `ok` with the new balance, or `declined`
//! with `game/not_enough_gold`. It takes two payloads:
//! - `{"itemId": "...", "quantity": n}`;
//! - a catalog block's press (a game action, protocol 07-game-actions.md;
//!   this payload is not described in the protocol yet): `{"shop",
//!   "recordId", "quantity", "expectedPrice", "currency": "gold", "fields"}`.
//!   A press made at a price that has since changed is declined with
//!   `game/price_changed`.
//!
//! GRIDSPAWN_APP_ID=game-… GRIDSPAWN_ENVIRONMENT=development \
//! GRIDSPAWN_CREDENTIAL_FILE=/path/to/0600-key-file \
//! [GRIDSPAWN_SDK_URL=http://<edge>:<port>] [SHOP_START_GOLD=100] \
//! [SHOP_HANDLER_DELAY_MS=0] \
//! cargo run --example action_shop
//!
//! Ctrl-C stops it cleanly. SHOP_HANDLER_DELAY_MS makes every handler wait
//! before it answers (to stop the process mid-handler in a rehearsal).
//!
//! **Dedupe:** a real game must insert the action id into a uniquely keyed
//! table in the same database transaction as the charge, before anything
//! else, in storage every instance shares. This example keeps it in memory,
//! which is enough for one process only.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use gridspawn_server::{
    Action, ActionReceiverOptions, ActionReply, Client, Credential, Environment,
};
use serde_json::{json, Value};

/// What the shop sells, in gold per item.
const PRICES: [(&str, i64); 3] = [("red_potion", 30), ("blue_potion", 45), ("iron_sword", 120)];

/// One finished action, to answer the same again for a repeated id.
#[derive(Clone)]
enum Outcome {
    Bought {
        item: String,
        quantity: i64,
        balance: i64,
    },
    NotEnoughGold {
        cost: i64,
        balance: i64,
    },
    PriceChanged {
        item: String,
        price: i64,
    },
    BadOrder,
}

impl Outcome {
    fn reply(&self) -> ActionReply {
        match self {
            Outcome::Bought {
                item,
                quantity,
                balance,
            } => ActionReply::ok(json!({"balance": balance, "itemId": item, "quantity": quantity}))
                .with_user_message(format!("Bought {quantity} x {item}.")),
            Outcome::NotEnoughGold { cost, balance } => {
                ActionReply::declined("game/not_enough_gold", "Not enough gold.")
                    .with_detail(format!("needs {cost}, has {balance}"))
            }
            Outcome::PriceChanged { item, price } => ActionReply::declined(
                "game/price_changed",
                format!("The price changed: {item} now costs {price} gold."),
            ),
            Outcome::BadOrder => {
                ActionReply::declined("game/invalid_order", "This item is not sold here.")
            }
        }
    }
}

struct Shop {
    start_gold: i64,
    gold: HashMap<String, i64>,
    /// Action id -> its outcome (the dedupe; see the module docs).
    done: HashMap<String, Outcome>,
}

impl Shop {
    fn buy(&mut self, action: &Action) -> Outcome {
        if let Some(outcome) = self.done.get(&action.id) {
            return outcome.clone();
        }
        let outcome = match order(&action.payload) {
            Order::Bad => Outcome::BadOrder,
            Order::PriceChanged { item, price } => Outcome::PriceChanged { item, price },
            Order::Buy {
                item,
                quantity,
                price,
            } => {
                let cost = price * quantity;
                let balance = self
                    .gold
                    .entry(action.actor.subject.clone())
                    .or_insert(self.start_gold);
                if *balance < cost {
                    Outcome::NotEnoughGold {
                        cost,
                        balance: *balance,
                    }
                } else {
                    *balance -= cost;
                    Outcome::Bought {
                        item,
                        quantity,
                        balance: *balance,
                    }
                }
            }
        };
        self.done.insert(action.id.clone(), outcome.clone());
        outcome
    }
}

/// What a payload asks for. The payload is never trusted.
#[derive(Debug, PartialEq)]
enum Order {
    Buy {
        item: String,
        quantity: i64,
        price: i64,
    },
    /// The press was made at another price than the shop's own.
    PriceChanged {
        item: String,
        price: i64,
    },
    Bad,
}

/// `itemId` (or a catalog press's `recordId`) naming a sold item, `quantity`
/// 1-99 (default 1); a catalog press also sends `expectedPrice` (per item)
/// and `currency`, which must be `gold`. A catalog block always sends both,
/// null when it does not know them (no Price role, no index currency): null
/// counts as absent. Other fields (`shop`, `fields`) are ignored.
fn order(payload: &Value) -> Order {
    let Some(item) = payload
        .get("itemId")
        .or_else(|| payload.get("recordId"))
        .and_then(Value::as_str)
    else {
        return Order::Bad;
    };
    let Some(price) = PRICES
        .iter()
        .find(|(name, _)| *name == item)
        .map(|(_, p)| *p)
    else {
        return Order::Bad;
    };
    let quantity = match payload.get("quantity") {
        None => 1,
        Some(value) => match value.as_i64() {
            Some(quantity) if (1..=99).contains(&quantity) => quantity,
            _ => return Order::Bad,
        },
    };
    let present = |key: &str| payload.get(key).filter(|value| !value.is_null());
    if let Some(currency) = present("currency") {
        if currency.as_str() != Some("gold") {
            return Order::Bad;
        }
    }
    if let Some(expected) = present("expectedPrice") {
        // Prices here are whole gold, so a float such as 45.0 is Bad by design.
        match expected.as_i64() {
            Some(expected) if expected == price => {}
            Some(_) => {
                return Order::PriceChanged {
                    item: item.to_string(),
                    price,
                }
            }
            None => return Order::Bad,
        }
    }
    Order::Buy {
        item: item.to_string(),
        quantity,
        price,
    }
}

fn env_i64(name: &str, default: i64) -> Result<i64, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(_) => Ok(default),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let app_id = std::env::var("GRIDSPAWN_APP_ID")?;
    let environment: Environment = std::env::var("GRIDSPAWN_ENVIRONMENT")?.parse()?;
    let credential = Credential::from_file(std::env::var("GRIDSPAWN_CREDENTIAL_FILE")?)?;
    let delay = Duration::from_millis(env_i64("SHOP_HANDLER_DELAY_MS", 0)?.max(0) as u64);
    let shop = Arc::new(Mutex::new(Shop {
        start_gold: env_i64("SHOP_START_GOLD", 100)?,
        gold: HashMap::new(),
        done: HashMap::new(),
    }));

    let mut builder = Client::builder(app_id, environment, credential);
    if let Ok(url) = std::env::var("GRIDSPAWN_SDK_URL") {
        builder = builder.sdk_base_url(url.parse()?);
    }
    let client = builder.connect().await?;

    let receiver = client.spawn_action_receiver(
        ActionReceiverOptions::new(["shop.buy"]),
        move |action: Action| {
            let shop = shop.clone();
            async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let outcome = shop.lock().unwrap().buy(&action);
                let reply = outcome.reply();
                println!(
                    "{} attempt {} {}: {}",
                    action.id,
                    action.attempt,
                    action.payload,
                    match &outcome {
                        Outcome::Bought { balance, .. } => format!("ok, balance {balance}"),
                        Outcome::NotEnoughGold { .. } => "declined game/not_enough_gold".into(),
                        Outcome::PriceChanged { price, .. } =>
                            format!("declined game/price_changed (now {price})"),
                        Outcome::BadOrder => "declined game/invalid_order".into(),
                    }
                );
                reply
            }
        },
    )?;
    println!(
        "receiving shop.buy as {}; Ctrl-C to stop",
        receiver.instance_id()
    );
    tokio::signal::ctrl_c().await?;
    let still_running = receiver.stop(Duration::from_secs(10)).await;
    println!("stopped ({still_running} handler(s) still running)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{order, Order};

    fn buy(item: &str, quantity: i64, price: i64) -> Order {
        Order::Buy {
            item: item.into(),
            quantity,
            price,
        }
    }

    #[test]
    fn item_payloads_are_read() {
        assert_eq!(
            order(&json!({"itemId": "red_potion"})),
            buy("red_potion", 1, 30)
        );
        assert_eq!(
            order(&json!({"itemId": "iron_sword", "quantity": 2})),
            buy("iron_sword", 2, 120)
        );
        assert_eq!(order(&json!({"itemId": "dragon"})), Order::Bad);
        assert_eq!(
            order(&json!({"itemId": "red_potion", "quantity": 0})),
            Order::Bad
        );
        assert_eq!(
            order(&json!({"itemId": "red_potion", "quantity": 100})),
            Order::Bad
        );
        assert_eq!(order(&json!({"quantity": 1})), Order::Bad);
    }

    #[test]
    fn catalog_presses_are_read_and_their_price_is_checked() {
        let press = |expected: serde_json::Value, currency: serde_json::Value| {
            json!({
                "shop": "general_store", "recordId": "blue_potion", "quantity": 3,
                "expectedPrice": expected, "currency": currency,
                "fields": {"title": "Blue potion"},
            })
        };
        let gold = || json!("gold");
        assert_eq!(order(&press(json!(45), gold())), buy("blue_potion", 3, 45));
        assert_eq!(
            order(&press(json!(40), gold())),
            Order::PriceChanged {
                item: "blue_potion".into(),
                price: 45
            }
        );
        assert_eq!(order(&press(json!(45), json!("gems"))), Order::Bad);
        assert_eq!(order(&press(json!("45"), gold())), Order::Bad);
        assert_eq!(
            order(&press(json!(45.0), gold())),
            Order::Bad,
            "whole gold only"
        );
        assert_eq!(
            order(&press(json!(null), gold())),
            buy("blue_potion", 3, 45),
            "a null price is not checked"
        );
        assert_eq!(
            order(&press(json!(45), json!(null))),
            buy("blue_potion", 3, 45),
            "a null currency is not checked"
        );
        assert_eq!(
            order(&press(json!(null), json!(null))),
            buy("blue_potion", 3, 45)
        );
        assert_eq!(
            order(&json!({"recordId": "red_potion"})),
            buy("red_potion", 1, 30),
            "price and currency are optional"
        );
    }
}
