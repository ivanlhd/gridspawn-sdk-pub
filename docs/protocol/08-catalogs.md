# 08 Catalogs on club pages (**Current**)

A game's server can publish a **catalog**: a shop, an auction list, a
bestiary, any list of records. The game's club shows it with a **catalog
block** on a club page: cards, a list, a table or sections per merchant,
with filters, search, sorting and paging that the club's managers set up in
the club layout editor.

A catalog is ordinary game data (06). There is no catalog endpoint, scope
or switch: a catalog is a set of documents written with
`PUT /v1/server/game-data`, under 06's rules (key format, 4 KiB per value,
quotas, rates, `seq`, public by nature). This part says which documents make
a catalog, what they hold, the order to write them in, and what a club page
shows.

**Not built yet:** buying from a row. A row has no button; that waits for
game actions (07, Proposed). An index may already carry `actions` (§2), but
no page uses it, so leave it out until 07 is Current. Images are not shown
either (§3.2, `image`).

## 1. The keys (Current)

A catalog is up to three kinds of documents of one app and one environment:

| Document | Key | Holds |
|---|---|---|
| index | any document key; by convention `shop.<name>`, for example `shop.demo` | what the catalog is (§2) |
| pages | `<index key>.p<n>`, for example `shop.demo.p0`, `shop.demo.p1` | the records (§3) |
| open signal (optional) | `<index key>.live`, for example `shop.demo.live` | "the shop is open" (§4) |

- `<n>` is a whole number from 0 to 9999 written without leading zeros
  (`p0`, `p7`, `p12`; never `p01`).
- The index lists its pages and names its open signal. A `pages` entry that
  is not `<index key>.p<n>`, or a `live` entry that is not
  `<index key>.live`, is ignored: an index cannot pull in another key's data
  or another game's.
- Pages and the signal are always read from the environment the index was
  read from (§7).
- A club picks a catalog by its index. The editor lists only documents whose
  value is an index (§2, `catalog`).
- All keys follow 06 §2 (`^[a-z][a-z0-9_.-]{0,63}$`), so keep the index key
  short enough that its page and signal keys stay within 64 characters.

## 2. The index (Current)

```json
{
  "catalog": 1,
  "title": "Demo shop",
  "currency": { "code": "gold", "label": "Gold" },
  "groups": [
    { "id": "smith", "name": "Blacksmith" },
    { "id": "alch", "name": "Alchemist" },
    { "id": "tailor", "name": "Tailor" }
  ],
  "fields": [
    { "id": "name", "type": "text", "label": "Name" },
    { "id": "price", "type": "currency", "label": "Price" },
    { "id": "stock", "type": "stock", "label": "Stock" },
    { "id": "merchant", "type": "group", "label": "Merchant" },
    { "id": "type", "type": "tag", "label": "Type",
      "values": [ { "id": "weapon", "label": "Weapons" }, { "id": "armor", "label": "Armor" } ] },
    { "id": "about", "type": "text", "label": "Description" },
    { "id": "listed", "type": "timestamp", "label": "Listed" }
  ],
  "pages": ["shop.demo.p0", "shop.demo.p1"],
  "live": "shop.demo.live"
}
```

| Field | Required | Meaning |
|---|---|---|
| `catalog` | yes | The contract version, an integer. This part is version **1**. A value that is not an integer of at least 1 means "not a catalog". A higher version shows "This shop needs a newer version of the app" until the app reads it |
| `title` | no | The catalog's name in the club editor's list of shops. Default: the index key |
| `currency` | no | `{ "code", "label"?, "symbol"?, "decimals"? }`, one currency per catalog. `code` is an id (below). `label` defaults to `code`. Prices show as the amount, then `symbol` (else `label`), for example `120 Gold`. `decimals` is 0 to 4 (default 0): the digits after the decimal point, in the viewer's number format |
| `groups` | no | The merchants: `[{ "id", "name"? }]`, at most 64. `name` defaults to `id`. Their order is the order of the "Grouped" sections and the merchant filter |
| `fields` | no | The record schema: `[{ "id", "type", "label"?, "values"? }]`, at most 32 (§3.2). Without it, the app guesses (§3.3) |
| `pages` | no | The page keys, in order (§1). This order, then each page's record order, is the catalog's "game order". A key listed twice counts once |
| `live` | no | The open signal's key (§4) |
| `actions` | no | Reserved for game actions (07, Proposed). Leave it out |

- **Ids** (record, field, group, tag value and currency `code`): text of 1
  to 64 characters, with no whitespace at the start or end and no line
  break inside.
- **Labels and titles** longer than 80 characters are cut.
- **Unknown fields** in the index are ignored, so a later minor addition
  does not break an older app. A breaking change raises `catalog`.
- **Entries the app cannot use are skipped one by one; the rest of the index
  still works:** a group or field without a usable id, a field of an
  unknown `type` or with the id `id`, a second group or field with an id
  already used (the first is kept), entries past the limits, a `currency`
  without a usable `code` (prices then show without a unit). No message is
  sent back to your server, and the club editor does not list these today:
  check your index against this part.

## 3. Pages and records (Current)

A page is an object with a `records` list:

```json
{
  "records": [
    { "id": "iron-sword", "name": "Iron sword", "price": 120, "stock": 4,
      "merchant": "smith", "type": ["weapon"],
      "about": "A plain, dependable blade.", "listed": "2026-10-01T12:00:00Z" }
  ]
}
```

A page of any other shape is skipped as a whole.

### 3.1 Records

- **`id`** is required: an id (§2), unique across the whole catalog. A
  record without a usable `id` is skipped.
- Every other key is a field, named by its `id` in the index's `fields`.
  When the index declares `fields`, other keys are not shown.
- **Merging:** pages are read in the index's order, and records in each
  page's order. When two records share an `id`, the one read first wins
  (the one on the page listed first) and the other is skipped.
- **Keep ids stable.** A club page keeps its scroll position, the viewer's
  filters and the rows on screen across updates by `id`.
- A block works with at most **2,000 records**; any more are dropped, in
  game order. Within the quotas of 06 a catalog stays well under this (§5).

### 3.2 Field types

A value of the wrong shape shows nothing for that field; the record is
still listed.

| `type` | Value | Shown as |
|---|---|---|
| `text` | a string | the text |
| `number` | a finite number | the number, in the viewer's number format |
| `currency` | a finite number | the amount with the index's `currency` (§2) |
| `stock` | a number, or `null` | `4 left`, `Sold out` at 0, nothing for `null` (unlimited). A negative number counts as 0, and a fraction is rounded down. A record without the key shows nothing and counts as in stock |
| `tag` | a string, or a list of strings | the tags, with the labels of the field's `values` when given, otherwise as written: as badges, as filter chips, or as a list. `values` (only for `tag`): `[{ "id", "label"? }]`, at most 64 |
| `group` | a group `id` from the index | the merchant's `name`. An id the index does not list shows no merchant |
| `boolean` | `true` or `false` | Yes or No |
| `timestamp` | an RFC 3339 string, for example `2026-10-01T12:00:00Z` | how long ago ("3 days ago"); the "Newest" sort uses the catalog's first timestamp field |
| `image` | reserved | **not shown yet**: there is no way to put game media into storage. Leave it out |

### 3.3 Without `fields`

When the index has no `fields` (or none usable), the app guesses the fields
from the first 20 records: every key but `id`, up to 32, labelled with the
key itself. A number becomes `number`, a list of strings `tag`, `true` or
`false` `boolean`, anything else `text`. The editor marks such a schema as
guessed.

A guess never gives `currency`, `stock`, `group` or `timestamp`, so prices
show without a unit, and there is no stock, merchant or Newest sort.
**Declare `fields`.**

## 4. The open signal (Current)

An optional short-lived document that says your shop is open.

- The index names it in `live` (`<index key>.live`). Its value can be
  anything; publish `true`.
- Give it a TTL and refresh it while your server runs, for example a TTL of
  60 s refreshed every 30 s (06 §2, `ttlSeconds`). With the Rust crate,
  `keep_alive` does this (§9).
- **While the index names a signal and the signal is absent** (expired,
  removed or never published), the block keeps listing the items and adds
  the line **"Stock and price may be out of date"**. When your server comes
  back and publishes it again, the line goes.
- An index without `live` never shows that line.
- **Do not give the index or the pages a TTL.** An expired page's items
  leave the page (§8: "Some items are not available right now"), and an
  expired index shows "Shop not available right now" (or another
  environment's index of the same key, §7). Use the signal for
  "the game is down", and keep the catalog itself without expiry.

## 5. Sizes and budget (Current)

The limits are 06's, per app and environment, and a catalog shares them with
everything else the game publishes:

| What | Limit (06) | For a catalog |
|---|---|---|
| One value | 4 KiB (4,096 bytes as compact JSON) | one page holds as many records as fit |
| Live keys | 256 | 1 index + the pages (+ 1 signal) |
| Total value bytes | 256 KiB | the index and every page, full or not |
| Entries per batch | 40 | a large catalog takes several batches |
| Entry rate | 20 a second (bursts of 40); per key 5 a second (bursts of 10) | a page can change at most 5 times a second |

**Measured on the demo catalog (§9):** a record with eight short fields is
about 170 bytes, so a page of 20 records is about 3.3 KiB, and the index is
737 bytes.

**The budget.** At 150 to 200 bytes per record, a 4 KiB page holds about
20 to 25 records. One index and 60 full pages use about 244 KiB of the
256 KiB and 61 of the 256 keys. That is about **1,200 to 1,500 records per
environment**, leaving about 12 KiB and 195 keys for the game's other data.
Shorter records fit more: keep ids short, and put long descriptions only
where they matter.

Publishing 61 keys at once takes at least two batches, and the entry rate
spreads them over more than a second. The crate's publisher paces this for
you (06 §8).

## 6. Write order (Current)

**What the service does.** Every `set` replaces the whole value: there is
no update inside a value. A batch is stored all at once, so a club page
that loads afterwards sees all of it. But **a club page that is already open
receives one update per key**, so for a moment it can hold some keys of a
batch and not the others. Within one batch, the service sends the updates of
`set` in key order, then the updates of `remove` in the batch's order. An
index key sorts before its pages and its signal (`shop.demo` comes before
`shop.demo.live` and `shop.demo.p0`), so **within one batch, open club pages
receive the index first**.

The block tolerates every gap below and never shows an error for one. The
rules keep a club page from showing a gap at all:

1. **Pages before the index that lists them.** Publish new pages first, and
   the index that lists them in a later write. In one batch the index
   arrives first, and an open club page shows "Some items are not available
   right now" until the pages follow, a moment later. The crate's publisher
   sends each tick's changes as one batch: call `flush` after setting the
   pages, then set the index.
2. **Drop a page from the index before you remove it.** Publish the index
   without the page, then remove the page. One batch is enough here: the
   index's `set` goes out before the page's `remove`.
3. **Move a record in one batch:** the page it leaves and the page it joins
   together. In between, an open club page may show it from either page
   (the page listed first wins) or not at all, for as short a time as
   possible.
4. **Change one record** (stock, price): publish only its page. Change the
   title, currency, merchants or fields: publish only the index.
5. **Replace a catalog:** write the new pages under the same keys (or new
   ones), then the index; then remove the pages it no longer lists (rule 2).
   Removing fields is safe: settings that named them show as missing in the
   club editor (§8.2).
6. **Remove a catalog:** remove the index first, then the pages and the
   signal, for example in one batch in that order. Club pages then show
   "Shop not available right now".

Per key, 06 §3 still applies: one writer, and each write numbered above the
last. The crate does that for you.

## 7. Which environment a club shows (Current)

- **The game's club.** A catalog shows on the pages of the game's club,
  through a catalog block that one of the club's managers adds and sets up.
  Publishing a catalog changes no page by itself.
- **Which environments a club page can read** is set by the game's owner,
  not by the club, with **one switch**: in the Dev Hub, the game's
  **"Game data"** section, the **"Game data"** switch.
  - **On:** your server may publish game data in every environment, and the
    club's pages read **production, development and staging**.
  - **Off:** nothing is published or shown.
  - If only part of it is on, the Dev Hub says "Game data is only partly on,
    so some of it may not show. Finish turning it on." with a **"Finish
    turning on"** button. A page reads only the environments that are on.
  - Nothing shows while the game has no club ("This game has no club yet.
    Live game data shows on the game's club."), while the Dev Hub's
    **"Game server can post to my club"** switch is off (it turns off the
    game's server link as a whole, game data included), or once the game is
    deleted.
  - A page asks for all three environments; the service refuses the ones the
    switch does not show, and the block reads nothing from them.
- **One environment per catalog on a page.** Among the environments a page
  can read, the block reads the index from **production when production has
  that key**, otherwise from development or staging, whichever holds the
  more recently updated index. It then reads **the pages and the signal from
  that same environment only**, so a production shop never shows a
  development page.
- So a production index of a key hides a development index of the same key.
  To try a catalog in development while production has one, give it another
  index key.
- **Public by nature** (06 §6): every viewer of the club's page can see
  every record. Publish display names only, and nothing private.

## 8. What a club page shows (Current)

### 8.1 The block

The club's managers choose, in the block's editor:
- the catalog (by its title, with its number of items and merchants);
- which field fills each **role**. The editor offers only fields of a
  fitting type, and pre-fills the first `text` field as Title, the first
  `currency` as Price, the `group` as Merchant, the `stock` as Stock and the
  first `tag` as Badge:

  | Role | Field types |
  |---|---|
  | Title, Subtitle, Description | `text`, `number` |
  | Price | `currency`, `number` |
  | Badge | `tag`, `text`, `number` |
  | Merchant | `group` |
  | Stock | `stock` |
  | More fields (label and value) | any type but `image` |

- the layout: cards, list, table, or grouped by merchant (needs a Merchant
  field); comfortable or compact;
- which filters viewers get (tag chips, merchant, price range, in stock
  only), and fixed filters viewers do not see;
- search on or off, and in which fields (the Title field by default);
- the sort: game order, price low to high or high to low, title A to Z, or
  Newest (by the first `timestamp` field); and whether viewers may change it;
- 10, 20 or 50 items per page, with "Show more".

Under the items, "Updated X ago" gives the **oldest** update time among the
index and its pages, so one stale page is not hidden behind fresh ones.

Updates arrive live: a published page changes only the block's list. The
viewer's scroll position, filters and search stay.

**States a viewer can see:**

| When | The block shows |
|---|---|
| No index under the key in any environment the page reads, or the document is not an index | "Shop not available right now" |
| `catalog` is a higher version | "This shop needs a newer version of the app" |
| The catalog has no records (no pages, empty pages, or every listed page missing) | "This shop is empty" |
| No record matches the filters or the search | "No items match" |
| A listed page is missing (not published yet, removed, expired) | its records are left out, with the line "Some items are not available right now." |
| The index names a signal that is absent | items listed, with the line "Stock and price may be out of date" |

The club can replace "Shop not available right now", "This shop is empty"
and "No items match" with its own texts.

### 8.2 What the club editor reports

- The shop list shows only documents that are an index, with their title,
  number of items and merchants, and when they were updated. A catalog of a
  higher version is listed but cannot be chosen.
- Under the chosen catalog: whether its open signal is found, not found, or
  not named by the index.
- A guessed schema (§3.3) is marked as guessed.
- A setting that names something your catalog no longer has shows as
  **missing**, and a setting the field's new type cannot show as **can't be
  shown here**. The club keeps both until a manager changes them.
- When a block is saved through automated club authoring, the same findings
  come back as warnings with these codes:

  | Code | Meaning |
  |---|---|
  | `not_published` | no document under the chosen key |
  | `needs_newer_app` | the catalog's `catalog` version is higher than the app reads |
  | `unknown_field` | a role, extra field or search field names a field the catalog does not have |
  | `unusable_field` | a saved setting names a field whose type the setting cannot show (the game changed its type), or Newest is saved while the catalog has no `timestamp` field |
  | `unknown_tag_value` | a tag filter names a tag the catalog does not have |
  | `unknown_merchant` | a fixed merchant filter names a group the index does not list |

**Renaming is not free.** Changing a field, tag or group `id` breaks the
settings clubs saved with the old one. Change labels and names instead.

## 9. A worked example (Current)

The complete demo catalog, 40 items on two pages with three merchants,
stock 0 and `null` included, is the `gridspawn-server` crate's test fixture
`tests/fixtures/catalog_demo.json`, with the offline test
`tests/catalog_demo.rs`. Its index is the one in §2 (with all four tag
values: `weapon`, `armor`, `potion`, `cloth`), and its first record is the
one in §3. Its open signal is `shop.demo.live`, `true`, with a TTL of 60 s.

Publishing it with the Rust crate (`serde_json::json!` values; `page0`,
`page1` and `index` hold the values above):

```rust
use gridspawn_server::DataBatch;
use serde_json::json;
use std::time::Duration;

let game = client.game();

// 1. The pages, then the index that lists them (§6 rule 1).
game.data().set_many(&DataBatch::new()
    .set("shop.demo.p0", page0)
    .set("shop.demo.p1", page1)).await?;
game.data().set("shop.demo", index).await?;

// 2. The open signal, kept alive while the server runs (§4).
let live = game.spawn_data_publisher(2);
live.set_with_ttl("shop.demo.live", json!(true), Duration::from_secs(60))?;
live.keep_alive("shop.demo.live")?;            // sent again every 30 s

// 3. A sale: republish only the page that changed (§6 rule 4).
live.set("shop.demo.p0", page0_after_sale)?;

// On shutdown: remove the signal, so club pages show the "out of date"
// line at once instead of after the TTL.
live.remove("shop.demo.live");
live.shutdown(Duration::from_secs(5)).await.ok();
```

The fixture itself is one batch: pages, index, signal. That is fine for a
first publication, when no club page shows the catalog yet. For later
changes, follow §6.

## 10. SDKs (Current)

There is no catalog-specific API: the game data calls of 06 §8 are all a
catalog needs (`set`, `set_with_ttl`, `remove`, `set_many`, the publisher
and `keep_alive`), in the Rust crate `gridspawn-server` and its C ABI
(`gs_game_data_*`). They check keys, value sizes and batch sizes before
sending; the catalog rules of §1 to §4 are not checked there. Game clients
and the Godot addon have nothing to do here.
