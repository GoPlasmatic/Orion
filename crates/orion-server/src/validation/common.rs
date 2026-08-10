use crate::errors::OrionError;

pub(crate) const MAX_ID_LEN: usize = 128;
pub(crate) const MAX_NAME_LEN: usize = 255;
pub(crate) const MAX_DESCRIPTION_LEN: usize = 2048;
/// D29: MySQL stores `route_pattern`, `topic` and `consumer_group` as
/// `varchar(255)` while SQLite and Postgres use unbounded `text`, and
/// `schema_parity`'s normaliser folds declared widths — so without a cap at
/// the validation boundary, a longer value stores on two backends and fails
/// on the third, silently. The narrowest backend sets the limit (characters,
/// not bytes: MySQL counts characters under utf8mb4).
pub(crate) const MAX_VARCHAR_FIELD_LEN: usize = 255;

/// Check if a string matches the identifier pattern:
/// starts with alphanumeric, then alphanumeric + dots/hyphens/underscores.
pub(crate) fn is_valid_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Entity ids that would collide with a static admin sub-resource.
///
/// R21: `/import`, `/export`, `/validate` and `/circuit-breakers` sit alongside
/// `/{id}` under the same prefixes, and axum prefers the static route only for
/// the verbs it declares. `DELETE /admin/workflows/import` therefore routed to
/// `delete_workflow` with `id = "import"` — benign today (the repo 404s), but an
/// entity actually *named* `import` would be unaddressable through `/{id}`, and
/// the audit log recorded `resource_id = "import"` for a delete that was never a
/// delete of anything.
///
/// 1.0 freezes these paths, so the cheap half of the fix is to make the ids
/// unreachable rather than move the endpoints.
pub(crate) const RESERVED_IDS: &[&str] = &[
    "import",
    "export",
    "validate",
    "versions",
    "status",
    "rollout",
    "test",
    "circuit-breakers",
    "purge",
    "requeue",
    "reload",
];

// G11: these returned `BadRequest`, and the channel/workflow validators each
// carried an identical `remap_to_field` helper to promote that into a
// `Validation` error with the caller's field path. The path is a parameter
// now, so every caller gets the structured error directly and the helpers
// are gone.

pub(crate) fn validate_id(id: &str, path: &'static str) -> Result<(), OrionError> {
    if id.len() > MAX_ID_LEN {
        return Err(OrionError::invalid_field(
            path,
            "INVALID",
            format!("ID exceeds maximum length of {MAX_ID_LEN} characters"),
        ));
    }
    if !is_valid_identifier(id) {
        return Err(OrionError::invalid_field(
            path,
            "INVALID",
            "ID must start with an alphanumeric character and contain only alphanumeric characters, dots, hyphens, or underscores",
        ));
    }
    if RESERVED_IDS.contains(&id.to_ascii_lowercase().as_str()) {
        return Err(OrionError::invalid_field(
            path,
            "INVALID",
            format!(
                "'{id}' is reserved: the admin API serves a static sub-resource at that \
                 path, so an entity with this id could not be addressed through /{{id}}"
            ),
        ));
    }
    Ok(())
}

pub(crate) fn validate_name(name: &str, path: &'static str) -> Result<(), OrionError> {
    if name.trim().is_empty() {
        return Err(OrionError::invalid_field(
            path,
            "INVALID",
            "Name must not be empty",
        ));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(OrionError::invalid_field(
            path,
            "INVALID",
            format!("Name exceeds maximum length of {MAX_NAME_LEN} characters"),
        ));
    }
    Ok(())
}

pub(crate) fn validate_description(desc: &str, path: &'static str) -> Result<(), OrionError> {
    if desc.len() > MAX_DESCRIPTION_LEN {
        return Err(OrionError::invalid_field(
            path,
            "INVALID",
            format!("Description exceeds maximum length of {MAX_DESCRIPTION_LEN} characters"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_id() {
        assert!(validate_id("my-workflow-1", "entity.id").is_ok());
        assert!(validate_id("workflow.v2", "entity.id").is_ok());
        assert!(validate_id("A123_test", "entity.id").is_ok());
    }

    #[test]
    fn test_invalid_id_chars() {
        assert!(validate_id("", "entity.id").is_err());
        assert!(validate_id("-starts-with-dash", "entity.id").is_err());
        assert!(validate_id(".starts-with-dot", "entity.id").is_err());
        assert!(validate_id("has spaces", "entity.id").is_err());
        assert!(validate_id("has/slash", "entity.id").is_err());
    }

    #[test]
    fn test_id_too_long() {
        let long_id = "a".repeat(MAX_ID_LEN + 1);
        assert!(validate_id(&long_id, "entity.id").is_err());
    }

    #[test]
    fn test_valid_name() {
        assert!(validate_name("My Workflow", "entity.name").is_ok());
    }

    #[test]
    fn test_empty_name() {
        assert!(validate_name("", "entity.name").is_err());
        assert!(validate_name("   ", "entity.name").is_err());
    }

    #[test]
    fn test_name_too_long() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long_name, "entity.name").is_err());
    }

    #[test]
    fn test_description_too_long() {
        let long_desc = "a".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(validate_description(&long_desc, "entity.description").is_err());
    }

    #[test]
    fn test_description_valid() {
        assert!(validate_description("A short description", "entity.description").is_ok());
        assert!(validate_description("", "entity.description").is_ok());
    }
}
