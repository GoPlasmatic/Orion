use crate::errors::OrionError;

pub(crate) const MAX_ID_LEN: usize = 128;
pub(crate) const MAX_NAME_LEN: usize = 255;
pub(crate) const MAX_DESCRIPTION_LEN: usize = 2048;

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

pub(crate) fn validate_id(id: &str) -> Result<(), OrionError> {
    if id.len() > MAX_ID_LEN {
        return Err(OrionError::BadRequest(format!(
            "ID exceeds maximum length of {MAX_ID_LEN} characters"
        )));
    }
    if !is_valid_identifier(id) {
        return Err(OrionError::BadRequest(
            "ID must start with an alphanumeric character and contain only alphanumeric characters, dots, hyphens, or underscores".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_name(name: &str, field: &str) -> Result<(), OrionError> {
    if name.trim().is_empty() {
        return Err(OrionError::BadRequest(format!("{field} must not be empty")));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(OrionError::BadRequest(format!(
            "{field} exceeds maximum length of {MAX_NAME_LEN} characters"
        )));
    }
    Ok(())
}

pub(crate) fn validate_description(desc: &str) -> Result<(), OrionError> {
    if desc.len() > MAX_DESCRIPTION_LEN {
        return Err(OrionError::BadRequest(format!(
            "Description exceeds maximum length of {MAX_DESCRIPTION_LEN} characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_id() {
        assert!(validate_id("my-workflow-1").is_ok());
        assert!(validate_id("workflow.v2").is_ok());
        assert!(validate_id("A123_test").is_ok());
    }

    #[test]
    fn test_invalid_id_chars() {
        assert!(validate_id("").is_err());
        assert!(validate_id("-starts-with-dash").is_err());
        assert!(validate_id(".starts-with-dot").is_err());
        assert!(validate_id("has spaces").is_err());
        assert!(validate_id("has/slash").is_err());
    }

    #[test]
    fn test_id_too_long() {
        let long_id = "a".repeat(MAX_ID_LEN + 1);
        assert!(validate_id(&long_id).is_err());
    }

    #[test]
    fn test_valid_name() {
        assert!(validate_name("My Workflow", "Name").is_ok());
    }

    #[test]
    fn test_empty_name() {
        assert!(validate_name("", "Name").is_err());
        assert!(validate_name("   ", "Name").is_err());
    }

    #[test]
    fn test_name_too_long() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long_name, "Name").is_err());
    }

    #[test]
    fn test_description_too_long() {
        let long_desc = "a".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(validate_description(&long_desc).is_err());
    }

    #[test]
    fn test_description_valid() {
        assert!(validate_description("A short description").is_ok());
        assert!(validate_description("").is_ok());
    }
}
