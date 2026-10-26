//! The paths the platform itself serves, which a data mount or a channel
//! route must never claim.

/// The platform routes a data mount must never shadow — the single source the
/// mount validation and the channel-activation gate both consult, so the two
/// cannot drift.
///
/// The invariant these hold: **platform routes are single-segment at root, or
/// under `/api`.** That is what lets a future platform route be added without
/// silently stealing a path from a channel already serving it.
pub const PLATFORM_ROUTES: &[&str] = &[
    "/health", "/healthz", "/readyz", "/metrics", "/docs", "/api",
];

/// Whether `prefix` claims `path`: the same string, or `path` sitting under it
/// at a `/` boundary. Allocation-free, so it is safe on the request path.
///
/// One rule, because four sites had grown their own `format!("{p}/")` copy of
/// it — the mount reserved-prefix check, the mount nesting check, the
/// rate-limit route classifier, and this function.
pub fn path_claims(prefix: &str, path: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// The platform route `served_path` would be shadowed by, if any.
pub fn shadowed_platform_route(served_path: &str) -> Option<&'static str> {
    PLATFORM_ROUTES
        .iter()
        .copied()
        .find(|p| path_claims(p, served_path))
}
