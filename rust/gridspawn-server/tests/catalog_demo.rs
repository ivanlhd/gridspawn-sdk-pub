//! The catalog block demo shop fixture (tests/fixtures/catalog_demo.json,
//! docs/catalog-block.md section 7.2), offline: it is one batch the service
//! accepts, written in the catalog contract's order (pages, then the index,
//! then the open signal with its TTL). The live publisher is
//! `live_catalog_demo` in tests/live.rs; the app tests read the same file.

use gridspawn_server::DataBatch;
use serde_json::Value;
use std::time::Duration;

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/catalog_demo.json"
);

fn documents() -> Vec<Value> {
    let fixture: Value =
        serde_json::from_str(&std::fs::read_to_string(FIXTURE).expect("fixture")).expect("json");
    fixture["documents"].as_array().expect("documents").clone()
}

fn key(document: &Value) -> &str {
    document["key"].as_str().expect("key")
}

#[test]
fn catalog_demo_is_one_batch_in_write_order() {
    let documents = documents();
    let keys: Vec<&str> = documents.iter().map(key).collect();
    assert_eq!(
        keys,
        [
            "shop.demo.p0",
            "shop.demo.p1",
            "shop.demo",
            "shop.demo.live"
        ]
    );
    // One batch: 1-40 entries, each value at most 4 KiB as JSON.
    assert!(!documents.is_empty() && documents.len() <= 40);
    for document in &documents {
        let bytes = serde_json::to_vec(&document["value"]).expect("value").len();
        assert!(bytes <= 4 * 1024, "{}: {bytes} bytes", key(document));
        let name = key(document);
        assert!(
            name.len() <= 64
                && name.starts_with(|c: char| c.is_ascii_lowercase())
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c)),
            "{name}"
        );
    }
    // The pages come before the index that lists them; the signal is last,
    // with a TTL the game refreshes.
    let index = &documents[2]["value"];
    assert_eq!(index["catalog"], 1);
    let pages: Vec<&str> = index["pages"]
        .as_array()
        .expect("pages")
        .iter()
        .map(|page| page.as_str().expect("page"))
        .collect();
    assert_eq!(pages, ["shop.demo.p0", "shop.demo.p1"]);
    assert_eq!(index["live"], "shop.demo.live");
    assert_eq!(documents[3]["ttlSeconds"], 60);
    let records: usize = documents[..2]
        .iter()
        .map(|page| page["value"]["records"].as_array().expect("records").len())
        .sum();
    assert_eq!(records, 40);
    // The publisher builds the batch from it without loss.
    let mut batch = DataBatch::new();
    for document in &documents {
        let value = document["value"].clone();
        batch = match document["ttlSeconds"].as_u64() {
            Some(ttl) => batch.set_with_ttl(key(document), value, Duration::from_secs(ttl)),
            None => batch.set(key(document), value),
        };
    }
    assert_eq!(batch.len(), documents.len());
}
