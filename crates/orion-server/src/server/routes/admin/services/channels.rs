//! Channel activation gates.

use crate::errors::OrionError;

/// K8: a channel activates only when the workflow it names has an active
/// version — the condition `engine/loader.rs` otherwise enforces later, as a
/// quarantine the activating caller never sees.
///
/// The channel-with-no-`workflow_id` case is refused too: the loader
/// quarantines it identically, so activating it can never serve a request.
pub(crate) async fn ensure_workflow_is_active(
    workflows: &dyn crate::storage::repositories::workflows::WorkflowRepository,
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    let Some(workflow_id) = draft
        .workflow_id
        .as_deref()
        .filter(|w| !w.trim().is_empty())
    else {
        return Err(OrionError::validation(format!(
            "Cannot activate channel '{}': it names no workflow_id, so it would be \
             quarantined at load and never serve. Set workflow_id first.",
            draft.name
        )));
    };
    let has_active = workflows
        .list_active()
        .await?
        .iter()
        .any(|w| w.workflow_id == workflow_id);
    if !has_active {
        let detail = match workflows.get_by_id(workflow_id).await {
            Ok(_) => "has no active version",
            Err(OrionError::NotFound(_)) => "does not exist",
            Err(e) => return Err(e),
        };
        return Err(OrionError::validation(format!(
            "Cannot activate channel '{}': workflow '{workflow_id}' {detail} — \
             activate the workflow first",
            draft.name
        )));
    }
    Ok(())
}
/// K7: the activation half of the unique-name rule — a name held by another
/// **active** channel loses the registry slot to the incumbent, so the
/// activation is refused. The write-time gate (`ensure_name_unclaimed` in the
/// repository) keeps new collisions out; this one catches rows that predate
/// it.
pub(crate) async fn ensure_name_is_unclaimed(
    channels: &dyn crate::storage::repositories::channels::ChannelRepository,
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    if let Some(holder) = channels
        .active_name_holder(&draft.name, &draft.channel_id)
        .await?
    {
        return Err(OrionError::Conflict(format!(
            "Cannot activate channel '{}': active channel id '{holder}' already uses \
             that name, and the data plane addresses channels by name. Rename one \
             of the two first.",
            draft.name
        )));
    }
    Ok(())
}
/// R7: refuse to activate a channel whose (method × path) another **active**
/// channel already claims.
///
/// `RouteTable::match_route` returns the first hit, so a second claimant is
/// simply dead: requests to its declared path run the incumbent's workflow.
/// Before this the tie broke on DB row order, so which one served the route
/// could differ per node and change on any reload. The incumbent wins by
/// construction here, which is why this is an activation gate rather than a
/// reload-time quarantine — adding a channel must never take a running one down.
///
/// What counts as a declared route is `routing::declared_route`, which is
/// `declared_route_segments` — the walk `RouteTable::build` runs — rendered to
/// canonical strings. One body, two views, so this gate cannot come to
/// disagree with the table that serves the route. It said as much while the
/// walk was written out twice and the two agreed only by coincidence.
pub(crate) async fn ensure_route_is_unclaimed(
    channels: &dyn crate::storage::repositories::channels::ChannelRepository,
    data_mounts: &[String],
    draft: &crate::storage::models::Channel,
) -> Result<(), OrionError> {
    use crate::channel::routing::declared_route;
    // A channel usually claims one route. One carrying `config.oauth2_login`
    // claims two — its own pattern and the IdP callback — and both have to be
    // gated, or a second channel's callback silently shadows the first and its
    // sign-ins complete against the wrong workflow.
    let declared = declared_route(draft);
    if declared.is_empty() {
        return Ok(());
    }
    let active = channels.list_active().await?;

    for (route, methods) in &declared {
        // #279: under `server.data_mounts` a channel's route is also served at
        // `{mount}{route}`, so it can collide with a PLATFORM route rather than
        // only with another channel. R7's rule is that the gate and the table must
        // not drift, so the check belongs here rather than only at reload.
        //
        // With the reserved-prefix validation on mounts, only the `"/"` escape
        // hatch can actually produce one of these — which is precisely the hazard
        // that makes `"/"` worth warning about, so it is worth reporting by name.
        for mount in data_mounts {
            let served = if mount == "/" {
                route.clone()
            } else {
                format!("{mount}{route}")
            };
            if let Some(platform) = crate::server::routes::shadowed_platform_route(&served) {
                return Err(OrionError::validation(format!(
                    "Cannot activate channel '{}': with server.data_mounts entry '{mount}' \
                     its route would be served at {served}, which collides with the \
                     platform route '{platform}'. The platform route wins, so the channel \
                     would never run. Change the route_pattern or the mount.",
                    draft.name,
                )));
            }
        }
        for other in &active {
            if other.channel_id == draft.channel_id {
                continue; // a new version of this same channel replaces itself
            }
            for (other_route, other_methods) in declared_route(other) {
                if &other_route == route
                    && other.priority == draft.priority
                    && crate::channel::routing::methods_overlap(methods, &other_methods)
                {
                    return Err(OrionError::validation(format!(
                        "Cannot activate channel '{}': active channel '{}' (id {}) already \
                         claims {} {route} at priority {}. Requests to that path would run \
                         one of the two arbitrarily. Change the route_pattern, narrow the \
                         methods, or give one a higher priority.",
                        draft.name,
                        other.name,
                        other.channel_id,
                        if methods.is_empty() {
                            "every method on".to_string()
                        } else {
                            methods.join("/")
                        },
                        draft.priority,
                    )));
                }
            }
        }
    }
    Ok(())
}
