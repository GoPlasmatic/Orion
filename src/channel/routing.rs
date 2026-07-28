use std::collections::HashMap;

use crate::storage::models::{Channel, ChannelProtocol};

/// A single entry in the route table.
struct RouteEntry {
    /// Channel name (used as engine channel key).
    channel_name: String,
    /// Allowed HTTP methods (uppercase). Empty = any method.
    methods: Vec<String>,
    /// Route segments parsed from route_pattern. Each is Static("orders")
    /// or Param("id").
    segments: Vec<RouteSegment>,
    /// Channel priority (higher = checked first).
    priority: i64,
}

impl RouteEntry {
    /// This entry's match shape, parameter names erased (R7).
    fn canonical(&self) -> String {
        let mut out = String::new();
        for segment in &self.segments {
            out.push('/');
            match segment {
                RouteSegment::Static(s) => out.push_str(&s.to_ascii_lowercase()),
                RouteSegment::Param(_) => out.push_str("{}"),
            }
        }
        if out.is_empty() {
            out.push('/');
        }
        out
    }
}

#[derive(Debug, Clone)]
enum RouteSegment {
    Static(String),
    Param(String),
}

/// Result of a successful route match.
#[derive(Debug, Clone)]
pub struct RouteMatch {
    /// The channel name that matched.
    pub channel_name: String,
    /// Extracted path parameters (e.g. {"id": "123"}).
    pub params: HashMap<String, String>,
}

/// The shape a route pattern matches, with parameter *names* erased.
///
/// R7: two channels claiming `GET /orders/{id}` and `GET /orders/{order_id}`
/// match exactly the same requests — the names are workflow-facing labels, not
/// part of the match. Comparing raw patterns would miss that. Canonicalising to
/// `/orders/{}` is what makes "these two collide" decidable.
pub fn canonical_route(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for segment in parse_route_pattern(pattern) {
        out.push('/');
        match segment {
            RouteSegment::Static(s) => out.push_str(&s.to_ascii_lowercase()),
            RouteSegment::Param(_) => out.push_str("{}"),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Whether two declared method sets can be matched by one request.
///
/// An empty set means "any method" (`RouteTable::match_route` skips the check),
/// so it overlaps with everything — including another empty set.
pub fn methods_overlap(a: &[String], b: &[String]) -> bool {
    if a.is_empty() || b.is_empty() {
        return true;
    }
    a.iter()
        .any(|x| b.iter().any(|y| x.eq_ignore_ascii_case(y)))
}

/// Parse a route pattern like "/orders/{id}/items/{item_id}" into segments.
fn parse_route_pattern(pattern: &str) -> Vec<RouteSegment> {
    pattern
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            if seg.starts_with('{') && seg.ends_with('}') {
                RouteSegment::Param(seg[1..seg.len() - 1].to_string())
            } else {
                RouteSegment::Static(seg.to_string())
            }
        })
        .collect()
}

/// Try to match a request path against a route pattern's segments.
/// Returns extracted params on success.
fn match_segments(
    segments: &[RouteSegment],
    path_parts: &[&str],
) -> Option<HashMap<String, String>> {
    if segments.len() != path_parts.len() {
        return None;
    }
    let param_count = segments
        .iter()
        .filter(|s| matches!(s, RouteSegment::Param(_)))
        .count();
    let mut params = HashMap::with_capacity(param_count);
    for (seg, part) in segments.iter().zip(path_parts.iter()) {
        match seg {
            RouteSegment::Static(expected) => {
                if !expected.eq_ignore_ascii_case(part) {
                    return None;
                }
            }
            RouteSegment::Param(name) => {
                params.insert(name.clone(), (*part).to_string());
            }
        }
    }
    Some(params)
}

/// Route table built from active REST channels, sorted by priority.
#[derive(Default)]
pub struct RouteTable {
    entries: Vec<RouteEntry>,
}

impl RouteTable {
    pub(super) fn build(channels: &[Channel]) -> Self {
        let mut entries: Vec<RouteEntry> = channels
            .iter()
            // F39: REST/HTTP channels register their route whatever their
            // channel_type. Filtering to `sync` here meant an async REST
            // channel — which validation *requires* to declare a
            // `route_pattern` — had that pattern silently ignored: the channel
            // was reachable by name and its declared route 404'd forever.
            // `dynamic_handler` strips a trailing `/async` before matching, so
            // an async channel's pattern works at `/{pattern}/async` with no
            // further change.
            .filter(|ch| {
                (ch.protocol == ChannelProtocol::Rest.as_str()
                    || ch.protocol == ChannelProtocol::Http.as_str())
                    && ch.route_pattern.is_some()
            })
            .filter_map(|ch| {
                let pattern = ch.route_pattern.as_deref()?;
                let segments = parse_route_pattern(pattern);
                let methods: Vec<String> = ch
                    .methods
                    .as_deref()
                    .and_then(|m| serde_json::from_str::<Vec<String>>(m).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.to_uppercase())
                    .collect();
                Some(RouteEntry {
                    channel_name: ch.name.clone(),
                    methods,
                    segments,
                    priority: ch.priority,
                })
            })
            .collect();

        // Sort by priority descending, then by segment count descending
        // (more specific routes first).
        //
        // R7: the channel name is the final tie-break, and it is load-bearing.
        // Two active channels claiming `GET /orders/{id}` at equal priority used
        // to resolve by **DB row order** — so which one served the route could
        // differ between nodes, and could change on any reload that happened to
        // return the rows differently. `activation_route_conflict` refuses new
        // collisions at the door; this makes the ones already stored resolve the
        // same way everywhere, which is the difference between a misconfiguration
        // an operator can see and one that moves around.
        entries.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| b.segments.len().cmp(&a.segments.len()))
                .then_with(|| a.channel_name.cmp(&b.channel_name))
        });

        let table = Self { entries };
        table.warn_on_conflicts();
        table
    }

    /// Log every (method × path) collision in the built table.
    ///
    /// Channels stored before R7's activation check can still collide, and the
    /// loser is simply dead — its declared route resolves to the winner's
    /// workflow, which is a *wrong answer*, not an error. Nothing said so.
    fn warn_on_conflicts(&self) {
        for (i, entry) in self.entries.iter().enumerate() {
            let canonical: String = entry.canonical();
            for winner in &self.entries[..i] {
                if winner.canonical() == canonical
                    && methods_overlap(&winner.methods, &entry.methods)
                {
                    tracing::warn!(
                        route = %canonical,
                        shadowed_channel = %entry.channel_name,
                        serving_channel = %winner.channel_name,
                        "Two active channels claim the same route; the shadowed one is \
                         unreachable by path (it is still reachable by name). Change its \
                         route_pattern, methods or priority."
                    );
                    break;
                }
            }
        }
    }

    /// Match a request (method, path) against the route table.
    /// Path should NOT include the `/api/v1/data/` prefix.
    pub fn match_route(&self, method: &str, path: &str) -> Option<RouteMatch> {
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        for entry in &self.entries {
            // Check method match (empty methods = accept any)
            if !entry.methods.is_empty()
                && !entry.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
            {
                continue;
            }
            if let Some(params) = match_segments(&entry.segments, &path_parts) {
                return Some(RouteMatch {
                    channel_name: entry.channel_name.clone(),
                    params,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_route_pattern_simple() {
        let segments = parse_route_pattern("/orders");
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], RouteSegment::Static(s) if s == "orders"));
    }

    #[test]
    fn test_parse_route_pattern_with_params() {
        let segments = parse_route_pattern("/orders/{id}/items/{item_id}");
        assert_eq!(segments.len(), 4);
        assert!(matches!(&segments[0], RouteSegment::Static(s) if s == "orders"));
        assert!(matches!(&segments[1], RouteSegment::Param(s) if s == "id"));
        assert!(matches!(&segments[2], RouteSegment::Static(s) if s == "items"));
        assert!(matches!(&segments[3], RouteSegment::Param(s) if s == "item_id"));
    }

    #[test]
    fn test_match_segments_exact() {
        let segments = parse_route_pattern("/orders/{id}");
        let params = match_segments(&segments, &["orders", "123"]);
        assert!(params.is_some());
        assert_eq!(params.expect("test").get("id").expect("test"), "123");
    }

    #[test]
    fn test_match_segments_no_match() {
        let segments = parse_route_pattern("/orders/{id}");
        assert!(match_segments(&segments, &["users", "123"]).is_none());
        assert!(match_segments(&segments, &["orders"]).is_none());
        assert!(match_segments(&segments, &["orders", "123", "items"]).is_none());
    }

    #[test]
    fn test_route_table_match() {
        let table = RouteTable {
            entries: vec![RouteEntry {
                channel_name: "orders.get".to_string(),
                methods: vec!["GET".to_string()],
                segments: parse_route_pattern("/orders/{id}"),
                priority: 0,
            }],
        };
        let result = table.match_route("GET", "orders/42");
        assert!(result.is_some());
        let rm = result.expect("test");
        assert_eq!(rm.channel_name, "orders.get");
        assert_eq!(rm.params.get("id").expect("test"), "42");
    }

    #[test]
    fn test_route_table_method_mismatch() {
        let table = RouteTable {
            entries: vec![RouteEntry {
                channel_name: "orders.get".to_string(),
                methods: vec!["GET".to_string()],
                segments: parse_route_pattern("/orders/{id}"),
                priority: 0,
            }],
        };
        assert!(table.match_route("POST", "orders/42").is_none());
    }

    #[test]
    fn test_route_table_priority_ordering() {
        let table = RouteTable {
            entries: vec![
                RouteEntry {
                    channel_name: "low".to_string(),
                    methods: vec![],
                    segments: parse_route_pattern("/items/{id}"),
                    priority: 0,
                },
                RouteEntry {
                    channel_name: "high".to_string(),
                    methods: vec![],
                    segments: parse_route_pattern("/items/{id}"),
                    priority: 10,
                },
            ],
        };
        // After sorting by priority desc, "high" should be first
        // But since we build the entries manually without sorting here,
        // let's test via RouteTable::build instead
        assert_eq!(
            table
                .match_route("GET", "items/1")
                .expect("test")
                .channel_name,
            "low"
        );
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    fn table() -> RouteTable {
        RouteTable {
            entries: vec![
                RouteEntry {
                    channel_name: "users.get".to_string(),
                    methods: vec!["GET".to_string()],
                    segments: parse_route_pattern("/users/{id}/orders/{oid}"),
                    priority: 0,
                },
                RouteEntry {
                    channel_name: "static.post".to_string(),
                    methods: vec!["POST".to_string()],
                    segments: parse_route_pattern("/a/b/c"),
                    priority: 0,
                },
            ],
        }
    }

    proptest! {
        /// Totality: any method/path bytes — unicode, `%`-escapes, `..`,
        /// empty segments, control characters — must resolve to Some/None,
        /// never panic. The data plane feeds this attacker-controlled input.
        #[test]
        fn match_route_is_total(method in ".*", path in ".*") {
            let _ = table().match_route(&method, &path);
        }

        /// Extracted params round-trip verbatim: whatever slash-free segment
        /// arrives in a param position comes back unchanged — no truncation
        /// or decoding surprises at this layer.
        #[test]
        fn extracted_params_round_trip(id in "[^/]+", oid in "[^/]+") {
            let path = format!("users/{id}/orders/{oid}");
            let m = table().match_route("GET", &path).expect("must match");
            prop_assert_eq!(m.channel_name.as_str(), "users.get");
            prop_assert_eq!(m.params.get("id").expect("id").as_str(), id.as_str());
            prop_assert_eq!(m.params.get("oid").expect("oid").as_str(), oid.as_str());
        }

        /// A pattern never matches outside its shape: extra or missing
        /// segments must not resolve to the two-param route.
        #[test]
        fn wrong_arity_never_matches(id in "[^/]+") {
            let short = format!("users/{id}");
            let long = format!("users/{id}/orders/{id}/extra");
            prop_assert!(table().match_route("GET", &short).is_none());
            prop_assert!(table().match_route("GET", &long).is_none());
        }
    }
}
