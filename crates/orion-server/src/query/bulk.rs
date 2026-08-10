//! Per-item outcomes for bulk writes (F28).
//!
//! A bulk `insert` means three different things underneath, and until 1.0 all
//! three returned the same shape — a row count, or one error:
//!
//! | Backend | Failure model |
//! |---|---|
//! | SQL | **Atomic.** One `INSERT … VALUES (…), (…)` inside a transaction: every row or none. |
//! | MongoDB | **Prefix-applied.** `insert_many` is ordered, so it stops at the first bad document; everything before it is committed and everything after is never attempted. |
//! | Elasticsearch | **Arbitrary-applied.** `_bulk` attempts every item independently; any subset can succeed. |
//!
//! So a failed bulk left state the caller could not describe: Mongo reported no
//! index, ES reported one `first_error` and discarded the rest of `items`, and
//! in both cases the successful writes were invisible. This module is the one
//! shape all three report through — a per-item array plus a `status` that says
//! `partial` when the call really did apply some of its items.

use serde_json::{Value, json};

/// What happened to one item of a bulk write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Applied.
    Ok,
    /// Attempted and rejected.
    Error,
    /// Never attempted — an ordered backend stopped at an earlier failure.
    Skipped,
}

impl ItemStatus {
    fn as_str(self) -> &'static str {
        match self {
            ItemStatus::Ok => "ok",
            ItemStatus::Error => "error",
            ItemStatus::Skipped => "skipped",
        }
    }
}

/// One item's outcome, carrying the caller's original array index so a
/// compensating workflow can line results up against what it sent.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemOutcome {
    pub index: usize,
    pub status: ItemStatus,
    /// Document id, where the backend reports one for a successful item.
    pub id: Option<Value>,
    /// Backend-shaped error detail for a failed item.
    pub error: Option<Value>,
}

impl ItemOutcome {
    pub fn ok(index: usize, id: Option<Value>) -> Self {
        Self {
            index,
            status: ItemStatus::Ok,
            id,
            error: None,
        }
    }

    pub fn error(index: usize, error: Value) -> Self {
        Self {
            index,
            status: ItemStatus::Error,
            id: None,
            error: Some(error),
        }
    }

    pub fn skipped(index: usize) -> Self {
        Self {
            index,
            status: ItemStatus::Skipped,
            id: None,
            error: None,
        }
    }

    fn to_json(&self) -> Value {
        let mut o = json!({ "index": self.index, "status": self.status.as_str() });
        if let Some(id) = &self.id {
            o["id"] = id.clone();
        }
        if let Some(e) = &self.error {
            o["error"] = e.clone();
        }
        o
    }
}

/// The outcome of a whole bulk write.
#[derive(Debug, Clone, PartialEq)]
pub struct BulkOutcome {
    pub items: Vec<ItemOutcome>,
}

impl BulkOutcome {
    /// An all-succeeded outcome — the SQL path, where the transaction makes any
    /// other result impossible, and the happy path of the doc stores.
    pub fn all_ok(ids: Vec<Option<Value>>) -> Self {
        Self {
            items: ids
                .into_iter()
                .enumerate()
                .map(|(i, id)| ItemOutcome::ok(i, id))
                .collect(),
        }
    }

    pub fn count(&self, status: ItemStatus) -> usize {
        self.items.iter().filter(|i| i.status == status).count()
    }

    pub fn inserted(&self) -> usize {
        self.count(ItemStatus::Ok)
    }

    /// Whether the call applied some of its items but not all of them — the
    /// state that had no representation before F28.
    pub fn is_partial(&self) -> bool {
        self.inserted() > 0 && self.inserted() < self.items.len()
    }

    /// Whether nothing was applied. Distinct from [`is_partial`](Self::is_partial):
    /// no item landed, so there is no partial state to report and the call is
    /// simply a failure.
    pub fn nothing_applied(&self) -> bool {
        self.inserted() == 0 && !self.items.is_empty()
    }

    /// The ids of successfully written documents, in the caller's order.
    pub fn ids(&self) -> Vec<Value> {
        self.items
            .iter()
            .filter(|i| i.status == ItemStatus::Ok)
            .filter_map(|i| i.id.clone())
            .collect()
    }

    /// The first failure's error detail, for the message of a hard error.
    pub fn first_error(&self) -> Option<&Value> {
        self.items
            .iter()
            .find(|i| i.status == ItemStatus::Error)
            .and_then(|i| i.error.as_ref())
    }

    /// The result envelope written to the task's `output` path.
    pub fn to_json(&self) -> Value {
        let failed = self.count(ItemStatus::Error);
        let skipped = self.count(ItemStatus::Skipped);
        let mut out = json!({
            "status": if self.is_partial() { "partial" } else { "ok" },
            "inserted": self.inserted(),
            "ids": self.ids(),
        });
        // Only carried when non-zero: an unremarkable bulk insert keeps the
        // shape it had before F28 plus `status`, rather than growing three
        // always-zero counters.
        if failed > 0 {
            out["failed"] = json!(failed);
        }
        if skipped > 0 {
            out["skipped"] = json!(skipped);
        }
        if self.is_partial() {
            out["items"] = Value::Array(self.items.iter().map(ItemOutcome::to_json).collect());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_all_ok_bulk_reports_ok_and_no_item_array() {
        let out = BulkOutcome::all_ok(vec![Some(json!("a")), Some(json!("b"))]);
        assert!(!out.is_partial());
        let j = out.to_json();
        assert_eq!(j["status"], "ok");
        assert_eq!(j["inserted"], 2);
        assert_eq!(j["ids"], json!(["a", "b"]));
        assert!(
            j.get("items").is_none(),
            "a clean bulk should not carry a per-item array: {j}"
        );
        assert!(j.get("failed").is_none(), "{j}");
    }

    /// The F28 case: some items applied, some did not. The caller must be able
    /// to see *which*, and `status` must not read as success.
    #[test]
    fn a_mixed_bulk_is_partial_and_names_every_item() {
        let out = BulkOutcome {
            items: vec![
                ItemOutcome::ok(0, Some(json!("a"))),
                ItemOutcome::error(1, json!({ "type": "version_conflict" })),
                ItemOutcome::ok(2, Some(json!("c"))),
            ],
        };
        assert!(out.is_partial());
        assert!(!out.nothing_applied());

        let j = out.to_json();
        assert_eq!(j["status"], "partial", "{j}");
        assert_eq!(j["inserted"], 2, "{j}");
        assert_eq!(j["failed"], 1, "{j}");
        assert_eq!(j["ids"], json!(["a", "c"]));

        let items = j["items"].as_array().expect("items array");
        assert_eq!(items.len(), 3, "every item must be reported: {j}");
        assert_eq!(items[1]["index"], 1);
        assert_eq!(items[1]["status"], "error");
        assert_eq!(items[1]["error"]["type"], "version_conflict");
    }

    /// Nothing landed, so there is no partial state — the handler turns this
    /// into a hard error rather than a 207.
    #[test]
    fn an_all_failed_bulk_is_not_partial() {
        let out = BulkOutcome {
            items: vec![
                ItemOutcome::error(0, json!({ "m": "x" })),
                ItemOutcome::error(1, json!({ "m": "y" })),
            ],
        };
        assert!(!out.is_partial());
        assert!(out.nothing_applied());
        assert_eq!(out.first_error(), Some(&json!({ "m": "x" })));
    }

    /// An ordered backend's shape: prefix applied, one failure, the rest never
    /// attempted. `skipped` must not be counted as either success or failure.
    #[test]
    fn skipped_items_are_counted_separately() {
        let out = BulkOutcome {
            items: vec![
                ItemOutcome::ok(0, None),
                ItemOutcome::error(1, json!({ "code": 11000 })),
                ItemOutcome::skipped(2),
            ],
        };
        assert!(out.is_partial());
        let j = out.to_json();
        assert_eq!(j["inserted"], 1, "{j}");
        assert_eq!(j["failed"], 1, "{j}");
        assert_eq!(j["skipped"], 1, "{j}");
        assert_eq!(j["items"][2]["status"], "skipped", "{j}");
    }
}
