const MASK: &str = "******";

/// Keys to mask inside an `auth` sub-object.
const AUTH_SECRET_KEYS: &[&str] = &["token", "password", "key", "secret"];

/// Keys to mask at the top level of a connector config.
const TOP_LEVEL_SECRET_KEYS: &[&str] = &[
    "password",
    "secret",
    "api_key",
    "token",
    "connection_string",
];

/// Mask sensitive fields in a connector's config_json for API responses.
pub fn mask_connector_secrets(config_json: &str) -> String {
    let Ok(mut val) = serde_json::from_str::<serde_json::Value>(config_json) else {
        return config_json.to_string();
    };

    if let Some(obj) = val.as_object_mut() {
        // Mask auth fields
        if let Some(auth) = obj.get_mut("auth")
            && let Some(auth_obj) = auth.as_object_mut()
        {
            for key in AUTH_SECRET_KEYS {
                if auth_obj.contains_key(*key) {
                    auth_obj.insert(
                        (*key).to_string(),
                        serde_json::Value::String(MASK.to_string()),
                    );
                }
            }
        }
        // Mask top-level secret-looking fields
        for key in TOP_LEVEL_SECRET_KEYS {
            if obj.contains_key(*key) {
                obj.insert(
                    (*key).to_string(),
                    serde_json::Value::String(MASK.to_string()),
                );
            }
        }
    }

    serde_json::to_string(&val).unwrap_or_else(|_| config_json.to_string())
}

/// Return a connector model with secrets masked.
pub fn mask_connector(
    connector: &crate::storage::models::Connector,
) -> crate::storage::models::Connector {
    let mut masked = connector.clone();
    masked.config_json = mask_connector_secrets(&masked.config_json);
    masked
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_connector_secrets_bearer_token() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret123"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_basic_password() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"basic","username":"user","password":"secret"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["auth"]["password"], "******");
        // Username should NOT be masked
        assert_eq!(val["auth"]["username"], "user");
    }

    #[test]
    fn test_mask_connector_secrets_api_key() {
        let config = r#"{"type":"http","url":"https://api.example.com","auth":{"type":"apikey","key":"mysecretkey"}}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["auth"]["key"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_top_level_fields() {
        let config = r#"{"type":"http","url":"https://api.example.com","password":"top_secret","api_key":"ak123","token":"tk456","secret":"shhh"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["password"], "******");
        assert_eq!(val["api_key"], "******");
        assert_eq!(val["token"], "******");
        assert_eq!(val["secret"], "******");
        // URL should not be masked
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn test_mask_connector_secrets_no_auth() {
        let config = r#"{"type":"http","url":"https://api.example.com"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["url"], "https://api.example.com");
    }

    #[test]
    fn test_mask_connector_secrets_invalid_json() {
        let config = "not valid json";
        let masked = mask_connector_secrets(config);
        assert_eq!(masked, config);
    }

    #[test]
    fn test_mask_connector_model() {
        use chrono::NaiveDate;
        let connector = crate::storage::models::Connector {
            id: "c1".to_string(),
            name: "test".to_string(),
            connector_type: "http".to_string(),
            config_json: r#"{"type":"http","url":"https://api.example.com","auth":{"type":"bearer","token":"secret"}}"#.to_string(),
            enabled: true,
            created_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
            updated_at: NaiveDate::from_ymd_opt(2025, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        };
        let masked = mask_connector(&connector);
        assert_eq!(masked.id, "c1");
        let val: serde_json::Value = serde_json::from_str(&masked.config_json).unwrap();
        assert_eq!(val["auth"]["token"], "******");
    }

    #[test]
    fn test_mask_connector_secrets_connection_string() {
        let config = r#"{"type":"db","connection_string":"postgres://user:pass@host/db","driver":"postgres"}"#;
        let masked = mask_connector_secrets(config);
        let val: serde_json::Value = serde_json::from_str(&masked).unwrap();
        assert_eq!(val["connection_string"], "******");
        assert_eq!(val["driver"], "postgres");
    }
}
