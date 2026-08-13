//! The bulk-import contract shared by all three `/import` endpoints
//! (workflows, channels, connectors): the `on_conflict` request vocabulary,
//! the per-item action vocabulary, and the report the server answers with.

use serde::{Deserialize, Serialize};

/// How an `/import` treats an item whose conflict key is already stored (K2).
/// Rides on the request as `?on_conflict=`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum OnConflict {
    /// Refuse the item — a 409-shaped entry in `errors[]`. The pre-K2
    /// behaviour, and still the default.
    #[default]
    Fail,
    /// Leave the stored entity alone and report the item as `skipped`.
    Skip,
    /// Upsert: an existing draft is updated in place; an active entity whose
    /// content differs gets a new draft version carrying the item's content;
    /// content-identical items are reported `unchanged` and write nothing.
    /// This is what makes a re-run of the same artifact a no-op.
    NewVersion,
}

impl OnConflict {
    /// The query-string spelling — what a client puts after `?on_conflict=`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fail => "fail",
            Self::Skip => "skip",
            Self::NewVersion => "new_version",
        }
    }
}

/// What one import item did (or, on dry-run, would do) — the `action` values
/// in [`ImportItemResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ImportAction {
    Created,
    /// A versioned entity's existing draft was replaced with this content.
    UpdatedDraft,
    /// A connector (unversioned) was updated in place.
    Updated,
    /// A new draft version was created carrying this content.
    NewVersion,
    /// Stored content is identical (DB-owned fields excluded); nothing written.
    Unchanged,
    /// `on_conflict=skip` and the key exists; nothing written.
    Skipped,
}

impl ImportAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::UpdatedDraft => "updated_draft",
            Self::Updated => "updated",
            Self::NewVersion => "new_version",
            Self::Unchanged => "unchanged",
            Self::Skipped => "skipped",
        }
    }

    /// Whether this action wrote (or would write) anything.
    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Self::Created | Self::UpdatedDraft | Self::Updated | Self::NewVersion
        )
    }
}

// The `#[serde(default)]`s below are the client half of the contract: a
// report from an older (or newer) server must still parse. The paired
// `schema(required)` overrides are the server half: the published document
// keeps promising every field, because the server always sends them.

/// One entry of `errors[]`: a failed item, by its index in the request array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ImportItemError {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub index: u64,
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub error: String,
}

/// One entry of `results[]`: what a non-failed item did (K2).
///
/// `action` stays a `String` on the wire so a client one release behind the
/// server still parses a report whose vocabulary grew; compare it against
/// [`ImportAction::as_str`] values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ImportItemResult {
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub index: u64,
    /// The item's conflict key (`workflow_id`, `channel_id`, connector
    /// `name`), or `null` when the item named none and the store generated
    /// one.
    pub id: Option<String>,
    /// `created`, `updated_draft`, `updated` (connectors), `new_version`,
    /// `unchanged`, or `skipped`.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub action: String,
}

/// Result of a bulk import, in both dry-run and real modes (R18, K2) — the
/// body under `{"data": …}` that all three `/import` endpoints answer with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct ImportResult {
    /// `true` when the request carried `?dry_run=true`; nothing was written.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub dry_run: bool,
    /// Items written — created, updated, or given a new version. In a dry
    /// run, the count that would be written.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub imported: u64,
    /// Items rejected. Non-zero with a **200** status: check this field, not
    /// the status code.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub failed: u64,
    /// Content-identical items under `on_conflict=new_version` (K2): nothing
    /// written, not counted in `imported`. Re-importing the same artifact
    /// therefore reports 0 imports and N unchanged.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub unchanged: u64,
    /// Items skipped under `on_conflict=skip` (K2).
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub skipped: u64,
    /// One entry per failed item, carrying its index in the request array.
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub errors: Vec<ImportItemError>,
    /// One `{index, id, action}` entry per non-failed item (K2).
    #[serde(default)]
    #[cfg_attr(feature = "utoipa", schema(required))]
    pub results: Vec<ImportItemResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_conflict_query_spelling_matches_serde() {
        for oc in [OnConflict::Fail, OnConflict::Skip, OnConflict::NewVersion] {
            let wire = serde_json::to_value(oc).expect("test");
            assert_eq!(wire, oc.as_str(), "as_str and serde disagree for {oc:?}");
        }
    }

    #[test]
    fn import_action_wire_spelling_matches_serde() {
        for action in [
            ImportAction::Created,
            ImportAction::UpdatedDraft,
            ImportAction::Updated,
            ImportAction::NewVersion,
            ImportAction::Unchanged,
            ImportAction::Skipped,
        ] {
            let wire = serde_json::to_value(action).expect("test");
            assert_eq!(
                wire,
                action.as_str(),
                "as_str and serde disagree for {action:?}"
            );
        }
    }

    #[test]
    fn a_v1_report_parses() {
        let report: ImportResult = serde_json::from_str(
            r#"{
                "dry_run": false,
                "imported": 2, "failed": 1, "unchanged": 1, "skipped": 0,
                "errors": [{"index": 3, "error": "409: exists"}],
                "results": [
                    {"index": 0, "id": "wf-1", "action": "created"},
                    {"index": 1, "id": null, "action": "created"},
                    {"index": 2, "id": "wf-2", "action": "unchanged"}
                ]
            }"#,
        )
        .expect("test");
        assert_eq!(report.imported, 2);
        assert_eq!(report.errors[0].index, 3);
        assert_eq!(report.results[1].id, None);
        assert_eq!(report.results[0].action, ImportAction::Created.as_str());
    }

    #[test]
    fn a_minimal_pre10_report_parses_to_defaults() {
        // Pre-1.0 shapes had fewer fields; everything must default.
        let report: ImportResult =
            serde_json::from_str(r#"{"imported": 4, "failed": 0}"#).expect("test");
        assert_eq!(report.imported, 4);
        assert!(!report.dry_run);
        assert!(report.errors.is_empty());
        assert!(report.results.is_empty());
    }
}
