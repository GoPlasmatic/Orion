//! Structural checks on a channel's HTTP fields, shared by the HTTP channel
//! driver and the authoring-time validation.

/// The HTTP methods a channel may declare.
///
/// R6: `methods` was checked for non-emptiness and nothing else, so
/// `["POTS"]` was created, activated and reloaded — and simply never matched,
/// because `RouteTable::match_route` compares against the request's real verb.
/// Nothing reported it. This is the set the data plane can actually route.
pub const VALID_HTTP_METHODS: &[&str] =
    &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Structural check on a channel's declared HTTP methods (R6).
pub fn check_http_methods(methods: &[String]) -> Vec<crate::errors::FieldError> {
    use crate::errors::FieldError;
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::with_capacity(methods.len());
    for method in methods {
        let upper = method.trim().to_ascii_uppercase();
        if !VALID_HTTP_METHODS.contains(&upper.as_str()) {
            out.push(
                FieldError::new(
                    "channel.methods",
                    "INVALID",
                    format!(
                        "'{method}' is not an HTTP method this channel can be reached by — \
                         a route declaring it is never matched"
                    ),
                )
                .with_expected(serde_json::Value::String(VALID_HTTP_METHODS.join(", ")))
                .with_got(serde_json::Value::String(method.clone())),
            );
        } else if seen.contains(&upper) {
            out.push(FieldError::new(
                "channel.methods",
                "INVALID",
                format!("HTTP method '{upper}' is listed more than once"),
            ));
        } else {
            seen.push(upper);
        }
    }
    out
}

/// Structural check on a channel's route pattern (R6).
///
/// `parse_route_pattern` is deliberately total — it never fails, because it
/// runs on the request path at reload time where there is nobody to report to.
/// That meant `"orders/{id"` parsed as a *static* segment named `{id`, and
/// `"{}"` as a parameter named `""`: the channel was created, activated and
/// reloaded, and was then unreachable forever with nothing saying so. The
/// grammar is checked here instead, at the one boundary that has a caller.
pub fn check_route_pattern(pattern: &str) -> Vec<crate::errors::FieldError> {
    use crate::errors::FieldError;
    let err = |message: String| {
        FieldError::new("channel.route_pattern", "INVALID", message)
            .with_expected(serde_json::Value::String(
                "a path like \"/orders/{id}/items\"".to_string(),
            ))
            .with_got(serde_json::Value::String(pattern.to_string()))
    };

    if !pattern.starts_with('/') {
        return vec![err(format!(
            "route_pattern must start with '/' (got \"{pattern}\")"
        ))];
    }
    if let Some(bad) = pattern.chars().find(|c| c.is_whitespace()) {
        return vec![err(format!(
            "route_pattern must not contain whitespace (found {bad:?})"
        ))];
    }
    if let Some(bad) = pattern.chars().find(|c| matches!(c, '?' | '#')) {
        return vec![err(format!(
            "route_pattern must not contain '{bad}' — it ends the path, so \
             everything after it is a query string or fragment and is never matched"
        ))];
    }
    // N10 (symmetry): requests are matched by their percent-decoded value,
    // but pattern segments are compared as written. An escape here (`%20`)
    // could therefore only ever match a double-encoded request — write the
    // character itself instead.
    if pattern.contains('%') {
        return vec![err(
            "route_pattern must not contain '%' — patterns are written literally \
             and requests match by their decoded value, so write the character \
             itself instead of a percent-escape"
                .to_string(),
        )];
    }

    let mut out = Vec::new();
    let mut params: Vec<&str> = Vec::new();
    // `[1..]` drops the leading '/' checked above; every remaining segment must
    // be non-empty, so `//` and a trailing '/' are rejected rather than silently
    // collapsed by `parse_route_pattern`'s empty-segment filter.
    for segment in pattern[1..].split('/') {
        if segment.is_empty() {
            out.push(err(
                "route_pattern has an empty path segment — remove the doubled or \
                 trailing '/'"
                    .to_string(),
            ));
            continue;
        }
        let has_brace = segment.contains('{') || segment.contains('}');
        if !has_brace {
            continue;
        }
        let Some(name) = segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) else {
            out.push(err(format!(
                "segment \"{segment}\" has unbalanced braces — a parameter is a whole \
                 segment written as \"{{name}}\""
            )));
            continue;
        };
        if name.is_empty() {
            out.push(err(
                "route_pattern has an unnamed parameter \"{}\" — a parameter must be \
                 named so the workflow can read it from `metadata.params`"
                    .to_string(),
            ));
            continue;
        }
        if name.contains('{') || name.contains('}') {
            out.push(err(format!(
                "parameter name \"{name}\" contains a brace — nested parameters are \
                 not supported"
            )));
            continue;
        }
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.push(err(format!(
                "parameter name \"{name}\" is not a valid identifier — it becomes a key \
                 in `metadata.params`, so it must match [A-Za-z_][A-Za-z0-9_]*"
            )));
            continue;
        }
        if params.contains(&name) {
            out.push(err(format!(
                "parameter \"{name}\" appears more than once — the later capture \
                 silently overwrites the earlier one"
            )));
            continue;
        }
        params.push(name);
    }
    out
}
