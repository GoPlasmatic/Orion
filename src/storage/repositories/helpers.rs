/// Converts an `Option<&str>` to a `sea_query::Value::String`, mapping `None`
/// to a SQL NULL string.  Replaces the repetitive
/// `.as_ref().map(|s| s.as_str().into()).unwrap_or(sea_query::Value::String(None))`
/// pattern used throughout repository create/update methods.
pub fn optional_string_value(opt: Option<&str>) -> sea_query::Value {
    opt.map(|s| s.to_string().into())
        .unwrap_or(sea_query::Value::String(None))
}

/// Normalises the `limit` / `offset` pagination parameters coming from filter
/// DTOs into clamped values safe for SQL queries.
///
/// - `limit`:  defaults to 50, clamped to [1, 1000]
/// - `offset`: defaults to 0, clamped to >= 0
pub fn clamp_pagination(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    let limit = limit.unwrap_or(50).clamp(1, 1000);
    let offset = offset.unwrap_or(0).max(0);
    (limit, offset)
}

/// Parses an optional sort-order string (`"asc"` or `"desc"`) into a
/// `sea_query::Order`.  Defaults to `Desc` when the value is `None` or any
/// unrecognised string.
pub fn parse_sort_order(sort_order: Option<&str>) -> sea_query::Order {
    match sort_order {
        Some("asc") => sea_query::Order::Asc,
        _ => sea_query::Order::Desc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_string_some() {
        let v = optional_string_value(Some("hello"));
        assert_eq!(v, sea_query::Value::String(Some(Box::new("hello".into()))));
    }

    #[test]
    fn optional_string_none() {
        let v = optional_string_value(None);
        assert_eq!(v, sea_query::Value::String(None));
    }

    #[test]
    fn pagination_defaults() {
        assert_eq!(clamp_pagination(None, None), (50, 0));
    }

    #[test]
    fn pagination_clamps() {
        assert_eq!(clamp_pagination(Some(0), Some(-5)), (1, 0));
        assert_eq!(clamp_pagination(Some(9999), Some(10)), (1000, 10));
    }

    #[test]
    fn sort_order_asc() {
        assert!(matches!(
            parse_sort_order(Some("asc")),
            sea_query::Order::Asc
        ));
    }

    #[test]
    fn sort_order_desc() {
        assert!(matches!(
            parse_sort_order(Some("desc")),
            sea_query::Order::Desc
        ));
    }

    #[test]
    fn sort_order_none_defaults_desc() {
        assert!(matches!(parse_sort_order(None), sea_query::Order::Desc));
    }
}
