//! Every endpoint path, built in one place.
//!
//! Static paths are `const`s; parameterized ones are functions. Every
//! interpolated value goes through [`seg`], which percent-encodes it as a
//! single path segment.
//!
//! That encoding is not decoration. `OrionClient::execute` does
//! `format!("{base}{path}")` and hands the string to reqwest, which parses it
//! as a URL — so a `/`, `?` or `#` inside a value used to *restructure the
//! request* rather than travel inside it. Entity ids are constrained to
//! `[alnum][alnum.\-_]*` server-side and so encode to themselves, but a
//! channel **name** is not: `validate_name` checks only emptiness and length.
//! A channel named `orders/v2` was therefore reachable over HTTP and
//! unreachable from this client, which is the whole of `orion-cli` and
//! `orion-server package`.
//!
//! The server has always been ready for the encoded spelling: it decodes a
//! single-segment data path exactly once, "so the encoded spelling of a name
//! reaches the same channel" (`routes/data/mod.rs`). This is the client
//! producing it.

/// Everything but RFC 3986's unreserved set is percent-encoded.
///
/// One definition for both jobs below, because both want the same answer for
/// the same reason: a value a user chose must travel *inside* the URL
/// component it occupies and never restructure it. A path segment could in
/// principle keep the sub-delims a query component cannot, but nothing here
/// needs them, and two nearly-identical tables is how one of them ends up
/// missing a character.
const UNRESERVED_ONLY: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encode one path segment. A well-formed entity id is entirely
/// unreserved and encodes to itself, so this is a no-op on every path that
/// carries one — it earns its keep on the free-form values: a channel name, a
/// package name, an id a caller typed wrong.
fn seg(value: &str) -> percent_encoding::PercentEncode<'_> {
    percent_encoding::utf8_percent_encode(value, UNRESERVED_ONLY)
}

// -- Admin: workflows --
pub const WORKFLOWS: &str = "/api/v1/admin/workflows";
pub const WORKFLOWS_EXPORT: &str = "/api/v1/admin/workflows/export";
pub const WORKFLOWS_IMPORT: &str = "/api/v1/admin/workflows/import";
pub const WORKFLOWS_VALIDATE: &str = "/api/v1/admin/workflows/validate";

pub fn workflow(id: &str) -> String {
    format!("{WORKFLOWS}/{}", seg(id))
}
pub fn workflow_status(id: &str) -> String {
    format!("{WORKFLOWS}/{}/status", seg(id))
}
pub fn workflow_rollout(id: &str) -> String {
    format!("{WORKFLOWS}/{}/rollout", seg(id))
}
pub fn workflow_test(id: &str) -> String {
    format!("{WORKFLOWS}/{}/test", seg(id))
}
pub fn workflow_versions(id: &str) -> String {
    format!("{WORKFLOWS}/{}/versions", seg(id))
}
pub fn workflow_dependencies(id: &str) -> String {
    format!("{WORKFLOWS}/{}/dependencies", seg(id))
}

// -- Admin: channels --
pub const CHANNELS: &str = "/api/v1/admin/channels";
pub const CHANNELS_EXPORT: &str = "/api/v1/admin/channels/export";
pub const CHANNELS_IMPORT: &str = "/api/v1/admin/channels/import";
pub const CHANNELS_VALIDATE: &str = "/api/v1/admin/channels/validate";

pub fn channel(id: &str) -> String {
    format!("{CHANNELS}/{}", seg(id))
}
pub fn channel_status(id: &str) -> String {
    format!("{CHANNELS}/{}/status", seg(id))
}
pub fn channel_versions(id: &str) -> String {
    format!("{CHANNELS}/{}/versions", seg(id))
}

// -- Admin: plugins --
pub const PLUGINS: &str = "/api/v1/admin/plugins";
pub const PLUGINS_EXPORT: &str = "/api/v1/admin/plugins/export";
pub const PLUGINS_IMPORT: &str = "/api/v1/admin/plugins/import";
pub const PLUGINS_VALIDATE: &str = "/api/v1/admin/plugins/validate";

pub fn plugin(id: &str) -> String {
    format!("{PLUGINS}/{}", seg(id))
}
pub fn plugin_status(id: &str) -> String {
    format!("{PLUGINS}/{}/status", seg(id))
}
pub fn plugin_versions(id: &str) -> String {
    format!("{PLUGINS}/{}/versions", seg(id))
}
pub fn plugin_dependencies(id: &str) -> String {
    format!("{PLUGINS}/{}/dependencies", seg(id))
}

// -- Admin: connectors --
pub const CONNECTORS: &str = "/api/v1/admin/connectors";
pub const CONNECTORS_EXPORT: &str = "/api/v1/admin/connectors/export";
pub const CONNECTORS_IMPORT: &str = "/api/v1/admin/connectors/import";
pub const CONNECTORS_VALIDATE: &str = "/api/v1/admin/connectors/validate";
pub const CIRCUIT_BREAKERS: &str = "/api/v1/admin/connectors/circuit-breakers";

pub fn connector(id: &str) -> String {
    format!("{CONNECTORS}/{}", seg(id))
}
pub fn connector_test(id: &str) -> String {
    format!("{CONNECTORS}/{}/test", seg(id))
}
pub fn circuit_breaker(id: &str) -> String {
    format!("{CIRCUIT_BREAKERS}/{}", seg(id))
}

// -- Admin: packages (promotion receipts) --
pub const PACKAGES: &str = "/api/v1/admin/packages";

pub fn package(name: &str) -> String {
    format!("{PACKAGES}/{}", seg(name))
}

// -- Admin: traces & DLQ --
pub const TRACES: &str = "/api/v1/admin/traces";
pub const TRACE_DLQ: &str = "/api/v1/admin/trace-dlq";
pub const TRACE_DLQ_PURGE: &str = "/api/v1/admin/trace-dlq/purge";

pub fn trace(id: &str) -> String {
    format!("{TRACES}/{}", seg(id))
}
pub fn trace_dlq_entry(id: &str) -> String {
    format!("{TRACE_DLQ}/{}", seg(id))
}
pub fn trace_dlq_requeue(id: &str) -> String {
    format!("{TRACE_DLQ}/{}/requeue", seg(id))
}

// -- Admin: the rest --
pub const AUDIT_LOGS: &str = "/api/v1/admin/audit-logs";
pub const BACKUPS: &str = "/api/v1/admin/backups";
pub const ENGINE_STATUS: &str = "/api/v1/admin/engine/status";
pub const ENGINE_RELOAD: &str = "/api/v1/admin/engine/reload";
pub const FUNCTIONS: &str = "/api/v1/admin/functions";

// -- Data plane --
pub fn data(channel: &str) -> String {
    format!("/api/v1/data/{}", seg(channel))
}
pub fn data_async(channel: &str) -> String {
    format!("/api/v1/data/{}/async", seg(channel))
}

// -- Operational --
pub const HEALTH: &str = "/health";
pub const METRICS: &str = "/metrics";

/// Build a URL query string from key-value pairs, skipping `None` values.
/// Returns `""` when nothing is set, `"?k=v&k2=v2"` otherwise.
///
/// Keys and values are percent-encoded through [`UNRESERVED_ONLY`], so a
/// user-supplied filter value — tags have no server-side charset restriction —
/// can never split into extra parameters or decode to something else.
pub fn query_string(params: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = params
        .iter()
        .filter_map(|(k, v)| {
            v.as_ref().map(|val| {
                format!(
                    "{}={}",
                    percent_encoding::utf8_percent_encode(k, UNRESERVED_ONLY),
                    percent_encoding::utf8_percent_encode(val, UNRESERVED_ONLY)
                )
            })
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameterized_paths_interpolate() {
        assert_eq!(workflow("wf-1"), "/api/v1/admin/workflows/wf-1");
        assert_eq!(
            workflow_status("wf-1"),
            "/api/v1/admin/workflows/wf-1/status"
        );
        assert_eq!(
            circuit_breaker("db"),
            "/api/v1/admin/connectors/circuit-breakers/db"
        );
        assert_eq!(data_async("orders"), "/api/v1/data/orders/async");
        assert_eq!(package("billing"), "/api/v1/admin/packages/billing");
    }

    /// The defect this encoding exists for. `execute` concatenates the path
    /// onto the base URL and reqwest parses the result, so an unencoded `/`,
    /// `?` or `#` restructured the request instead of travelling inside it —
    /// and a channel is addressed on the data plane by its **name**, which
    /// `validate_name` leaves free-form. Such a channel was reachable over
    /// plain HTTP and unreachable from this client.
    #[test]
    fn a_channel_name_is_one_segment_whatever_it_contains() {
        // Was: `/api/v1/data/orders/v2` — two segments, matching no route, 404.
        assert_eq!(data("orders/v2"), "/api/v1/data/orders%2Fv2");
        assert_eq!(
            data_async("orders/v2"),
            "/api/v1/data/orders%2Fv2/async",
            "the trailing /async must stay a real segment"
        );
        // Was: everything from `?` onwards became a query string.
        assert_eq!(data("q?x=1"), "/api/v1/data/q%3Fx%3D1");
        // A fragment would have truncated the path entirely.
        assert_eq!(data("a#b"), "/api/v1/data/a%23b");
        // And a traversal attempt cannot climb out of the data plane.
        assert_eq!(
            data("../admin/channels"),
            "/api/v1/data/..%2Fadmin%2Fchannels"
        );
    }

    /// Ids are `[alnum][alnum.\-_]*` server-side — entirely unreserved — so
    /// encoding is a no-op on every well-formed path. This is what makes the
    /// change safe to apply everywhere rather than only on the data plane.
    #[test]
    fn a_well_formed_id_encodes_to_itself() {
        for id in ["wf-1", "orders_v2", "a.b.c", "X9", "a-b_c.d~e"] {
            assert_eq!(workflow(id), format!("/api/v1/admin/workflows/{id}"));
            assert_eq!(channel(id), format!("/api/v1/admin/channels/{id}"));
            assert_eq!(connector(id), format!("/api/v1/admin/connectors/{id}"));
            assert_eq!(trace(id), format!("/api/v1/admin/traces/{id}"));
            assert_eq!(package(id), format!("/api/v1/admin/packages/{id}"));
        }
    }

    /// The fixed segments around an interpolated one stay literal, so encoding
    /// cannot swallow the sub-resource a path is addressing.
    #[test]
    fn surrounding_segments_are_not_encoded() {
        assert_eq!(
            workflow_status("a/b"),
            "/api/v1/admin/workflows/a%2Fb/status"
        );
        assert_eq!(
            trace_dlq_requeue("a b"),
            "/api/v1/admin/trace-dlq/a%20b/requeue"
        );
    }

    #[test]
    fn query_string_skips_none_and_prefixes_once() {
        assert_eq!(query_string(&[("a", None)]), "");
        assert_eq!(
            query_string(&[
                ("status", Some("active".to_string())),
                ("tag", None),
                ("limit", Some("10".to_string())),
            ]),
            "?status=active&limit=10"
        );
    }

    #[test]
    fn query_string_encodes_reserved_characters() {
        // A tag like "env=prod&team=pay" must stay one value — unencoded it
        // splits into extra parameters the server silently drops.
        assert_eq!(
            query_string(&[("tag", Some("env=prod&team=pay".to_string()))]),
            "?tag=env%3Dprod%26team%3Dpay"
        );
        // '+' would decode server-side as a space; '%' must not start an
        // accidental escape; spaces and non-ASCII round-trip.
        assert_eq!(
            query_string(&[("tag", Some("v1+hotfix 100%é".to_string()))]),
            "?tag=v1%2Bhotfix%20100%25%C3%A9"
        );
        // Unreserved characters pass through untouched.
        assert_eq!(
            query_string(&[("tag", Some("a-b_c.d~e".to_string()))]),
            "?tag=a-b_c.d~e"
        );
    }
}
