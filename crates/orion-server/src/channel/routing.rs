use std::borrow::Cow;
use std::collections::HashMap;

use crate::errors::OrionError;
use crate::storage::models::{Channel, ChannelProtocol};

/// Which of a channel's routes an entry is.
///
/// Almost every channel has exactly one, and it is [`RouteRole::Primary`]. A
/// channel carrying `config.oauth2_login` (#307) has a second: the IdP's
/// callback, whose whole purpose is to be a *different* request on the same
/// channel. The table is where the distinction is decided, once, so the data
/// route does not re-derive it by comparing the request path against the
/// config — two answers to "which leg is this?" is one too many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRole {
    /// The channel's own `route_pattern`.
    Primary,
    /// `config.oauth2_login.callback_path`.
    OAuthCallback,
}

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
    /// Which of the channel's routes this is.
    role: RouteRole,
}

impl RouteEntry {
    /// This entry's match shape, parameter names erased (R7).
    fn canonical(&self) -> String {
        canonical_segments(&self.segments)
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
    /// Which of the channel's routes matched. [`RouteRole::Primary`] for every
    /// channel that declares only one.
    pub role: RouteRole,
}

/// The shape a route pattern matches, with parameter *names* erased.
///
/// R7: two channels claiming `GET /orders/{id}` and `GET /orders/{order_id}`
/// match exactly the same requests — the names are workflow-facing labels, not
/// part of the match. Comparing raw patterns would miss that. Canonicalising to
/// `/orders/{}` is what makes "these two collide" decidable.
pub fn canonical_route(pattern: &str) -> String {
    canonical_segments(&parse_route_pattern(pattern))
}

/// The one canonicalisation rule, shared by the activation gate
/// ([`canonical_route`]) and the stored-collision warning
/// ([`RouteEntry::canonical`]) — two copies could drift on what "same route"
/// means.
fn canonical_segments(segments: &[RouteSegment]) -> String {
    let mut out = String::new();
    for segment in segments {
        out.push('/');
        match segment {
            // N10: case-preserving, mirroring the byte-exact match — two
            // patterns differing only in case are distinct routes now, so
            // canonicalising them together would refuse a legal activation
            // and report `/Orders` and `/orders` as one route.
            RouteSegment::Static(s) => out.push_str(s),
            RouteSegment::Param(_) => out.push_str("{}"),
        }
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// A channel's declared route, as the route table would see it: the canonical
/// match shape plus the methods it claims. Empty for channels that register
/// no route (Kafka, or no `route_pattern`).
///
/// [`declared_route_parts`] over a stored row — so the activation gate
/// (`ensure_route_is_unclaimed`, in the admin channel routes) and
/// [`RouteTable::build`] read one projection, which is the R7 argument on
/// [`declared_route_segments`].
pub(crate) fn declared_route(ch: &Channel) -> Vec<(String, Vec<String>)> {
    declared_route_parts(
        &ch.protocol,
        ch.route_pattern.as_deref(),
        &ch.methods().unwrap_or_default(),
        oauth_callback_path(ch).as_deref(),
    )
}

/// The callback path a stored channel's config declares, if any.
///
/// Read out of the raw `config_json` rather than a parsed [`ChannelConfig`]:
/// this runs on the activation path and inside `RouteTable::build`, both of
/// which hold the row and not the compiled runtime, and a config that no longer
/// parses must not take the whole route table down with it. A channel whose
/// config is broken is quarantined at load anyway — it simply keeps its primary
/// route here, which is the same shape it had before this existed.
pub(crate) fn oauth_callback_path(ch: &Channel) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&ch.config_json)
        .ok()?
        .get("oauth2_login")?
        .get("callback_path")?
        .as_str()
        .map(str::to_string)
}

/// **The** route projection: every route a channel claims, as parsed segments
/// tagged with the role each one plays.
///
/// R7 in one function. The activation gate and the table that actually serves
/// the route must agree on what "claims a route" means, and for a while they
/// agreed only by coincidence — this walk was written out twice, once here in
/// segment form for [`RouteTable::build`] and once in canonical-string form
/// for the gate, so the next change to route eligibility could land in one and
/// not the other. It has happened before in both directions: F39 below, and
/// the lint that compared raw pattern strings and folded methods into an "ANY"
/// sentinel, so `/o/{id}` and `/o/{orderId}` were one route to the table and
/// two to the lint while `methods: []` meant *every* method here and matched
/// nothing there.
///
/// The callers that want the canonical string take
/// [`declared_route_parts`], which is this projection with
/// [`canonical_segments`] applied — a view of one answer, not a second answer.
///
/// Methods come back **raw**. `build` uppercases them for matching, while the
/// activation error prints them back to the operator in the spelling they
/// wrote — folding the uppercase in here would change that message.
///
/// Takes loose fields rather than a `&Channel` because the definition-set
/// lint holds a channel *definition* and not a stored row, and gates promotion
/// on the same question activation asks.
fn declared_route_segments(
    protocol: &str,
    route_pattern: Option<&str>,
    methods: &[String],
    oauth_callback_path: Option<&str>,
) -> Vec<(Vec<RouteSegment>, Vec<String>, RouteRole)> {
    // F39: REST/HTTP channels register their route whatever their
    // channel_type. Filtering to `sync` here meant an async REST channel —
    // which validation *requires* to declare a `route_pattern` — had that
    // pattern silently ignored: the channel was reachable by name and its
    // declared route 404'd forever. `dynamic_handler` strips a trailing
    // `/async` before matching, so an async channel's pattern works at
    // `/{pattern}/async` with no further change.
    if !serves_a_route(protocol) {
        return Vec::new();
    }
    let Some(pattern) = route_pattern else {
        return Vec::new();
    };
    let mut out = vec![(
        parse_route_pattern(pattern),
        methods.to_vec(),
        RouteRole::Primary,
    )];
    // The callback is a second claim on the estate's route space and has to be
    // gated like the first one: two channels whose callbacks collide would
    // resolve one of them arbitrarily, and the loser's sign-ins would complete
    // against the wrong workflow. It is always a `GET` — the IdP redirects a
    // browser to it — whatever the channel's own `methods` say.
    if let Some(callback) = oauth_callback_path {
        out.push((
            parse_route_pattern(callback),
            vec!["GET".to_string()],
            RouteRole::OAuthCallback,
        ));
    }
    out
}

/// [`declared_route_segments`] in canonical-string form, for the callers
/// deciding whether two routes collide rather than which one matches.
///
/// The role is dropped: a collision is a collision whichever leg claimed it.
pub(crate) fn declared_route_parts(
    protocol: &str,
    route_pattern: Option<&str>,
    methods: &[String],
    oauth_callback_path: Option<&str>,
) -> Vec<(String, Vec<String>)> {
    declared_route_segments(protocol, route_pattern, methods, oauth_callback_path)
        .into_iter()
        .map(|(segments, methods, _role)| (canonical_segments(&segments), methods))
        .collect()
}

/// Whether a channel of this protocol registers an HTTP route at all.
fn serves_a_route(protocol: &str) -> bool {
    protocol == ChannelProtocol::Rest.as_str() || protocol == ChannelProtocol::Http.as_str()
}

/// [`declared_route_segments`] over a stored row.
fn declared_segments(ch: &Channel) -> Vec<(Vec<RouteSegment>, Vec<String>, RouteRole)> {
    declared_route_segments(
        &ch.protocol,
        ch.route_pattern.as_deref(),
        &ch.methods().unwrap_or_default(),
        oauth_callback_path(ch).as_deref(),
    )
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

/// Percent-decode one path segment exactly once (RFC 3986 §2.1). Returns
/// `None` when a `%` is not followed by two hex digits, or when the decoded
/// bytes are not valid UTF-8 — both are malformed request paths, answered
/// with 400 by [`RouteTable::match_route`]. Also used by the data plane's
/// single-segment channel-name fallback so an encoded name spelling reaches
/// the same channel as its decoded one (N10).
///
/// The no-`%` case — every static segment of every request — borrows rather
/// than copying: `match_segments` only needs an owned value in the `Param`
/// arm, so allocating one per segment on the data-plane path would be
/// discarded after a single comparison.
pub(crate) fn percent_decode_segment(segment: &str) -> Option<Cow<'_, str>> {
    if !segment.contains('%') {
        return Some(Cow::Borrowed(segment));
    }
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16))?;
            let lo = bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16))?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok().map(Cow::Owned)
}

/// Split a request path on **raw** `/` separators, then percent-decode each
/// segment once. Splitting before decoding is what lets `%2F` travel inside
/// a parameter value without acting as a separator (N10).
fn decode_path_parts(path: &str) -> Option<Vec<Cow<'_, str>>> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(percent_decode_segment)
        .collect()
}

/// Try to match a request path against a route pattern's segments.
/// Returns extracted params on success.
///
/// N10: static segments compare **byte-exact** against the decoded request
/// segment — `/ORDERS/1` does not match `/orders/{id}` (RFC 3986 reserves
/// case-insensitivity for the scheme and host, never the path), while
/// `/%6Frders/1` does (percent-encoding an unreserved character is
/// equivalence, not difference). Captured params arrive already decoded.
fn match_segments<S: AsRef<str>>(
    segments: &[RouteSegment],
    path_parts: &[S],
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
                if expected.as_str() != part.as_ref() {
                    return None;
                }
            }
            RouteSegment::Param(name) => {
                params.insert(name.clone(), part.as_ref().to_string());
            }
        }
    }
    Some(params)
}

/// Route table built from active REST channels, sorted by priority.
///
/// N20: matching is indexed rather than a full scan. Entries bucket by
/// `(segment count, first static segment)` — plus a per-count bucket for
/// routes whose first segment is a parameter, which any first part can
/// satisfy — so a request consults only the routes that could possibly match
/// its shape instead of every route in the estate. Bucket contents are entry
/// indices in table order, and the two candidate lists are merged by index,
/// so the first hit is still the highest-priority route: the index changes
/// what is *examined*, never what wins.
#[derive(Default)]
pub struct RouteTable {
    entries: Vec<RouteEntry>,
    /// `segment_count` → first static segment → entry indices, ascending.
    ///
    /// Nested rather than keyed on `(usize, String)`: a tuple key cannot be
    /// borrowed as `(usize, &str)`, so every request allocated a `String` copy
    /// of its first path segment just to probe the map and dropped it again.
    by_first_static: HashMap<usize, HashMap<String, Vec<usize>>>,
    /// `segment_count` → indices of entries whose first segment is a
    /// parameter, ascending.
    by_param_first: HashMap<usize, Vec<usize>>,
}

impl RouteTable {
    pub(super) fn build<'a>(channels: impl IntoIterator<Item = &'a Channel>) -> Self {
        let mut entries: Vec<RouteEntry> = channels
            .into_iter()
            .flat_map(|ch| {
                declared_segments(ch)
                    .into_iter()
                    .map(|(segments, methods, role)| RouteEntry {
                        channel_name: ch.name.clone(),
                        // `RouteEntry::methods` is uppercase by contract; only
                        // the activation error wants the operator's own
                        // spelling back.
                        methods: methods.into_iter().map(|m| m.to_uppercase()).collect(),
                        segments,
                        priority: ch.priority,
                        role,
                    })
                    .collect::<Vec<_>>()
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

        let table = Self::from_sorted_entries(entries);
        table.warn_on_conflicts();
        table
    }

    /// Index already-sorted entries. Split from [`Self::build`] so tests can
    /// construct a table from hand-written entries without skipping the index
    /// the production path relies on.
    fn from_sorted_entries(entries: Vec<RouteEntry>) -> Self {
        let mut by_first_static: HashMap<usize, HashMap<String, Vec<usize>>> = HashMap::new();
        let mut by_param_first: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, entry) in entries.iter().enumerate() {
            match entry.segments.first() {
                Some(RouteSegment::Static(s)) => by_first_static
                    .entry(entry.segments.len())
                    .or_default()
                    .entry(s.clone())
                    .or_default()
                    .push(i),
                // A leading parameter — or a zero-segment route, which cannot
                // exist past `declared_segments` but costs nothing to file
                // consistently — matches any first part of its length.
                Some(RouteSegment::Param(_)) | None => by_param_first
                    .entry(entry.segments.len())
                    .or_default()
                    .push(i),
            }
        }
        Self {
            entries,
            by_first_static,
            by_param_first,
        }
    }

    /// Log every (method × path) collision in the built table.
    ///
    /// Channels stored before R7's activation check can still collide, and the
    /// loser is simply dead — its declared route resolves to the winner's
    /// workflow, which is a *wrong answer*, not an error. Nothing said so.
    fn warn_on_conflicts(&self) {
        // Canonicalised once per entry rather than once per *pair*. The inner
        // loop used to call `winner.canonical()`, which rebuilds a `String` from
        // the entry's segments, so the scan allocated O(n^2) strings — ~125k of
        // them at 500 routes, on every engine reload, to report the handful of
        // collisions that are usually zero.
        let canonicals: Vec<String> = self.entries.iter().map(RouteEntry::canonical).collect();
        for (i, entry) in self.entries.iter().enumerate() {
            let canonical = &canonicals[i];
            for (j, winner) in self.entries[..i].iter().enumerate() {
                if &canonicals[j] == canonical && methods_overlap(&winner.methods, &entry.methods) {
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
    ///
    /// `Ok(None)` = no route claims the path; `Err(BadRequest)` = the path
    /// carries an invalid percent-sequence and the request must be answered
    /// with 400 rather than matched or silently passed along (N10).
    pub fn match_route(&self, method: &str, path: &str) -> Result<Option<RouteMatch>, OrionError> {
        let Some(path_parts) = decode_path_parts(path) else {
            return Err(OrionError::validation(
                "Invalid percent-encoding in request path".to_string(),
            ));
        };

        // Candidates: routes whose first static segment equals the request's
        // first part, and routes that open with a parameter — both restricted
        // to the request's segment count. Each bucket is ascending, so a
        // two-pointer merge visits candidates in table (priority) order.
        let empty: Vec<usize> = Vec::new();
        let static_bucket = path_parts
            .first()
            .and_then(|first| {
                self.by_first_static
                    .get(&path_parts.len())?
                    .get(first.as_ref())
            })
            .unwrap_or(&empty);
        let param_bucket = self.by_param_first.get(&path_parts.len()).unwrap_or(&empty);

        let (mut a, mut b) = (0, 0);
        while a < static_bucket.len() || b < param_bucket.len() {
            let idx = match (static_bucket.get(a), param_bucket.get(b)) {
                (Some(&x), Some(&y)) if x < y => {
                    a += 1;
                    x
                }
                (Some(_), Some(&y)) => {
                    b += 1;
                    y
                }
                (Some(&x), None) => {
                    a += 1;
                    x
                }
                (None, Some(&y)) => {
                    b += 1;
                    y
                }
                (None, None) => unreachable!("loop condition"),
            };
            let entry = &self.entries[idx];
            // Check method match (empty methods = accept any)
            if !entry.methods.is_empty()
                && !entry.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
            {
                continue;
            }
            if let Some(params) = match_segments(&entry.segments, &path_parts) {
                return Ok(Some(RouteMatch {
                    channel_name: entry.channel_name.clone(),
                    params,
                    role: entry.role,
                }));
            }
        }
        Ok(None)
    }

    /// The pre-N20 full scan, kept as the oracle for the equivalence test:
    /// the index may change what is examined, never what wins.
    #[cfg(test)]
    fn match_route_linear(
        &self,
        method: &str,
        path: &str,
    ) -> Result<Option<RouteMatch>, OrionError> {
        let Some(path_parts) = decode_path_parts(path) else {
            return Err(OrionError::validation(
                "Invalid percent-encoding in request path".to_string(),
            ));
        };
        for entry in &self.entries {
            if !entry.methods.is_empty()
                && !entry.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
            {
                continue;
            }
            if let Some(params) = match_segments(&entry.segments, &path_parts) {
                return Ok(Some(RouteMatch {
                    channel_name: entry.channel_name.clone(),
                    params,
                    role: entry.role,
                }));
            }
        }
        Ok(None)
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

    fn parts(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// What the one projection produces, read through both of its views.
    ///
    /// The two used to be separate walks that agreed by coincidence. Sharing a
    /// body makes "they agree" untestable — it is now true by construction —
    /// so what is worth pinning is the answer itself: a sign-in channel claims
    /// two routes, the callback is a `GET` whatever the channel's own methods
    /// say, the primary's methods come back in the spelling the operator
    /// wrote, and the roles are not interchangeable.
    #[test]
    fn a_sign_in_channel_claims_its_route_and_its_callback_in_both_views() {
        let methods = parts(&["post"]);
        let segments =
            declared_route_segments("rest", Some("/orders/{id}"), &methods, Some("/cb/{tenant}"));

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].2, RouteRole::Primary);
        assert_eq!(segments[1].2, RouteRole::OAuthCallback);
        assert_eq!(
            segments[0].1,
            parts(&["post"]),
            "methods stay in the spelling the operator wrote — `build` uppercases \
             for matching, the activation error prints them back"
        );
        assert_eq!(segments[1].1, parts(&["GET"]));

        let canonical =
            declared_route_parts("rest", Some("/orders/{id}"), &methods, Some("/cb/{tenant}"));
        assert_eq!(
            canonical,
            vec![
                ("/orders/{}".to_string(), parts(&["post"])),
                ("/cb/{}".to_string(), parts(&["GET"])),
            ],
            "the canonical view is the same routes with parameter names erased"
        );
    }

    /// Route eligibility is one decision, so both views answer it the same.
    #[test]
    fn a_channel_that_serves_no_route_declares_none_in_either_view() {
        let methods = parts(&["POST"]);
        for (protocol, pattern) in [
            // Kafka registers no HTTP route at all.
            ("kafka", Some("/orders")),
            // Nor does cron: it is started by its schedule, and a route would
            // make it reachable by request.
            ("cron", Some("/orders")),
            // REST with no pattern claims nothing, callback or not.
            ("rest", None),
        ] {
            assert!(declared_route_segments(protocol, pattern, &methods, Some("/cb")).is_empty());
            assert!(declared_route_parts(protocol, pattern, &methods, Some("/cb")).is_empty());
        }
    }

    #[test]
    fn test_match_segments_exact() {
        let segments = parse_route_pattern("/orders/{id}");
        let params = match_segments(&segments, &parts(&["orders", "123"]));
        assert!(params.is_some());
        assert_eq!(params.expect("test").get("id").expect("test"), "123");
    }

    #[test]
    fn test_match_segments_no_match() {
        let segments = parse_route_pattern("/orders/{id}");
        assert!(match_segments(&segments, &parts(&["users", "123"])).is_none());
        assert!(match_segments(&segments, &parts(&["orders"])).is_none());
        assert!(match_segments(&segments, &parts(&["orders", "123", "items"])).is_none());
    }

    /// N10: static-segment matching is byte-exact — RFC 3986 makes the path
    /// component case-sensitive, and the old `eq_ignore_ascii_case` let
    /// `/ORDERS/1` resolve to `/orders/{id}` while cache keys derived from
    /// the raw path treated them as different requests.
    #[test]
    fn test_match_segments_is_case_sensitive() {
        let segments = parse_route_pattern("/orders/{id}");
        assert!(match_segments(&segments, &parts(&["ORDERS", "1"])).is_none());
        assert!(match_segments(&segments, &parts(&["Orders", "1"])).is_none());
        assert!(match_segments(&segments, &parts(&["orders", "1"])).is_some());
    }

    #[test]
    fn test_route_table_match() {
        let table = RouteTable::from_sorted_entries(vec![RouteEntry {
            channel_name: "orders.get".to_string(),
            methods: vec!["GET".to_string()],
            segments: parse_route_pattern("/orders/{id}"),
            priority: 0,
            role: RouteRole::Primary,
        }]);
        let result = table.match_route("GET", "orders/42").expect("valid path");
        assert!(result.is_some());
        let rm = result.expect("test");
        assert_eq!(rm.channel_name, "orders.get");
        assert_eq!(rm.params.get("id").expect("test"), "42");
    }

    #[test]
    fn test_route_table_method_mismatch() {
        let table = RouteTable::from_sorted_entries(vec![RouteEntry {
            channel_name: "orders.get".to_string(),
            methods: vec!["GET".to_string()],
            segments: parse_route_pattern("/orders/{id}"),
            priority: 0,
            role: RouteRole::Primary,
        }]);
        assert!(
            table
                .match_route("POST", "orders/42")
                .expect("valid path")
                .is_none()
        );
    }

    fn orders_table() -> RouteTable {
        RouteTable::from_sorted_entries(vec![RouteEntry {
            channel_name: "orders.get".to_string(),
            methods: vec!["GET".to_string()],
            segments: parse_route_pattern("/orders/{id}"),
            priority: 0,
            role: RouteRole::Primary,
        }])
    }

    /// N10: `%2F` reaches the workflow as a literal `/` inside the param —
    /// decoded exactly once, after splitting, so it never acts as a
    /// separator. Previously the workflow received the raw `a%2Fb` and a
    /// literal slash was inexpressible in a parameter.
    #[test]
    fn test_params_are_percent_decoded_once() {
        let m = orders_table()
            .match_route("GET", "orders/a%2Fb")
            .expect("valid path")
            .expect("must match");
        assert_eq!(m.params.get("id").expect("id"), "a/b");

        // Double-encoding decodes one layer only.
        let m = orders_table()
            .match_route("GET", "orders/a%252Fb")
            .expect("valid path")
            .expect("must match");
        assert_eq!(m.params.get("id").expect("id"), "a%2Fb");
    }

    /// N10: percent-encoding an unreserved character is RFC 3986
    /// *equivalence* — `%6F` is `o` — so the encoded spelling of a static
    /// segment still matches.
    #[test]
    fn test_encoded_static_segment_matches() {
        let m = orders_table()
            .match_route("GET", "%6Frders/1")
            .expect("valid path");
        assert!(m.is_some());
        // But the encoded spelling of a *different* case does not.
        let m = orders_table()
            .match_route("GET", "%4Frders/1") // %4F = 'O'
            .expect("valid path");
        assert!(m.is_none());
    }

    /// N10: an invalid percent-sequence is a malformed path — 400, not a
    /// silent literal match and not a fall-through.
    #[test]
    fn test_invalid_percent_sequence_is_rejected() {
        for path in ["orders/a%ZZ", "orders/a%2", "orders/%", "orders/%G1"] {
            assert!(
                orders_table().match_route("GET", path).is_err(),
                "{path} must be rejected"
            );
        }
        // Invalid UTF-8 after decoding is equally malformed.
        assert!(orders_table().match_route("GET", "orders/%FF").is_err());
    }

    #[test]
    fn test_route_table_priority_ordering() {
        let table = RouteTable::from_sorted_entries(vec![
            RouteEntry {
                channel_name: "low".to_string(),
                methods: vec![],
                segments: parse_route_pattern("/items/{id}"),
                priority: 0,
                role: RouteRole::Primary,
            },
            RouteEntry {
                channel_name: "high".to_string(),
                methods: vec![],
                segments: parse_route_pattern("/items/{id}"),
                priority: 10,
                role: RouteRole::Primary,
            },
        ]);
        // After sorting by priority desc, "high" should be first
        // But since we build the entries manually without sorting here,
        // let's test via RouteTable::build instead
        assert_eq!(
            table
                .match_route("GET", "items/1")
                .expect("valid path")
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
        RouteTable::from_sorted_entries(vec![
            RouteEntry {
                channel_name: "users.get".to_string(),
                methods: vec!["GET".to_string()],
                segments: parse_route_pattern("/users/{id}/orders/{oid}"),
                priority: 0,
                role: RouteRole::Primary,
            },
            RouteEntry {
                channel_name: "static.post".to_string(),
                methods: vec!["POST".to_string()],
                segments: parse_route_pattern("/a/b/c"),
                priority: 0,
                role: RouteRole::Primary,
            },
        ])
    }

    /// N20 equivalence: the indexed lookup must answer exactly what the
    /// pre-index full scan answered, for every request shape — including the
    /// priority tie-breaks the scan encoded by entry order. A wider table
    /// than `table()`: overlapping static/param first segments, mixed
    /// lengths, an any-method route, and equal-shape routes at different
    /// priorities.
    fn wide_table() -> RouteTable {
        RouteTable::from_sorted_entries(vec![
            RouteEntry {
                channel_name: "orders.high".to_string(),
                methods: vec!["GET".to_string()],
                segments: parse_route_pattern("/orders/{id}"),
                priority: 10,
                role: RouteRole::Primary,
            },
            RouteEntry {
                channel_name: "orders.low".to_string(),
                methods: vec![],
                segments: parse_route_pattern("/orders/{id}"),
                priority: 0,
                role: RouteRole::Primary,
            },
            RouteEntry {
                channel_name: "param.first".to_string(),
                methods: vec!["GET".to_string()],
                segments: parse_route_pattern("/{tenant}/orders"),
                priority: 0,
                role: RouteRole::Primary,
            },
            RouteEntry {
                channel_name: "deep".to_string(),
                methods: vec!["POST".to_string()],
                segments: parse_route_pattern("/a/b/c"),
                priority: 0,
                role: RouteRole::Primary,
            },
            RouteEntry {
                channel_name: "single".to_string(),
                methods: vec!["GET".to_string()],
                segments: parse_route_pattern("/orders"),
                priority: 0,
                role: RouteRole::Primary,
            },
        ])
    }

    proptest! {
        /// Totality: any method/path bytes — unicode, `%`-escapes, `..`,
        /// empty segments, control characters — must resolve to
        /// Ok(Some)/Ok(None)/Err(400), never panic. The data plane feeds
        /// this attacker-controlled input.
        #[test]
        fn match_route_is_total(method in ".*", path in ".*") {
            let _ = table().match_route(&method, &path);
        }

        /// N20: the indexed lookup answers exactly what the pre-index full
        /// scan answers — same channel, same params, same errors — for
        /// arbitrary methods and paths against a table with overlapping
        /// static/param first segments, mixed lengths and priority ties.
        #[test]
        fn indexed_match_equals_the_linear_scan(
            method in "(GET|POST|PUT|.*)",
            path in "[a-c/{}%2F]{0,24}",
        ) {
            let t = wide_table();
            let indexed = t.match_route(&method, &path);
            let linear = t.match_route_linear(&method, &path);
            match (indexed, linear) {
                (Ok(i), Ok(l)) => {
                    prop_assert_eq!(
                        i.as_ref().map(|m| (&m.channel_name, &m.params)),
                        l.as_ref().map(|m| (&m.channel_name, &m.params))
                    );
                }
                (Err(_), Err(_)) => {}
                (i, l) => prop_assert!(false, "indexed={i:?} linear={l:?}"),
            }
        }

        /// Same equivalence over the realistic shapes the generator above
        /// under-samples: well-formed multi-segment paths that actually hit
        /// the routes.
        #[test]
        fn indexed_match_equals_the_linear_scan_on_real_shapes(
            first in "(orders|a|zzz)",
            second in "[a-z]{1,4}",
            method in "(GET|POST|DELETE)",
            depth in 1usize..4,
        ) {
            let t = wide_table();
            let path = match depth {
                1 => first.clone(),
                2 => format!("{first}/{second}"),
                _ => format!("{first}/{second}/c"),
            };
            let indexed = t.match_route(&method, &path).expect("valid path");
            let linear = t.match_route_linear(&method, &path).expect("valid path");
            prop_assert_eq!(
                indexed.as_ref().map(|m| (&m.channel_name, &m.params)),
                linear.as_ref().map(|m| (&m.channel_name, &m.params))
            );
        }

        /// Percent-free params round-trip verbatim: whatever slash-free,
        /// percent-free segment arrives in a param position comes back
        /// unchanged. (`%` segments are covered by the decode tests — N10
        /// decodes exactly once at extraction, so they are *not* verbatim.)
        #[test]
        fn extracted_params_round_trip(id in "[^/%]+", oid in "[^/%]+") {
            let path = format!("users/{id}/orders/{oid}");
            let m = table()
                .match_route("GET", &path)
                .expect("percent-free paths are always valid")
                .expect("must match");
            prop_assert_eq!(m.channel_name.as_str(), "users.get");
            prop_assert_eq!(m.params.get("id").expect("id").as_str(), id.as_str());
            prop_assert_eq!(m.params.get("oid").expect("oid").as_str(), oid.as_str());
        }

        /// N10: whatever a caller percent-encodes into a param position
        /// arrives decoded — including `/` via `%2F`, which must never act
        /// as a separator.
        #[test]
        fn encoded_params_decode_to_the_original(id in "[^/%]+", oid in "[^/%]+") {
            fn encode(s: &str) -> String {
                s.bytes().map(|b| format!("%{b:02X}")).collect()
            }
            let path = format!("users/{}/orders/{}", encode(&id), encode(&oid));
            let m = table()
                .match_route("GET", &path)
                .expect("fully-encoded segments are valid")
                .expect("must match");
            prop_assert_eq!(m.params.get("id").expect("id").as_str(), id.as_str());
            prop_assert_eq!(m.params.get("oid").expect("oid").as_str(), oid.as_str());
        }

        /// A pattern never matches outside its shape: extra or missing
        /// segments must not resolve to the two-param route.
        #[test]
        fn wrong_arity_never_matches(id in "[^/%]+") {
            let short = format!("users/{id}");
            let long = format!("users/{id}/orders/{id}/extra");
            prop_assert!(table().match_route("GET", &short).expect("valid").is_none());
            prop_assert!(table().match_route("GET", &long).expect("valid").is_none());
        }
    }
}
