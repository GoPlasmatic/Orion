use super::*;

/// A one-column read-back. `sqlx::FromRow` needs a named struct even for a
/// single column, and this one is read inside the acquisition transaction.
#[derive(sqlx::FromRow)]
pub(super) struct FencingTokenRow {
    pub(super) fencing_token: i64,
}

/// One slot of a singleton key.
#[derive(Clone, Copy)]
pub(super) enum SlotRow<'a> {
    /// Slot 0: the `cron_singletons` row, addressed exactly as before slots
    /// existed, so an older binary contends for it with the same statements.
    Legacy { key: &'a str },
    /// Slot 1 and up: a `cron_singleton_slots` row.
    Extra { key: &'a str, slot: u32 },
}

impl<'a> SlotRow<'a> {
    pub(super) fn of(key: &'a str, slot: u32) -> Self {
        if slot == 0 {
            Self::Legacy { key }
        } else {
            Self::Extra { key, slot }
        }
    }
}

/// The slot `occurrence_id` already holds on `key`, if any.
pub(super) async fn own_slot(
    tx: &mut crate::storage::DbTransaction,
    backend: crate::storage::DbBackend,
    key: &str,
    occurrence_id: &str,
) -> Result<Option<u32>, OrionError> {
    let (sql, values) = build_sqlx(
        backend,
        Query::select()
            .column(CronSingletons::SingletonKey)
            .from(CronSingletons::Table)
            .and_where(Expr::col(CronSingletons::SingletonKey).eq(key))
            .and_where(Expr::col(CronSingletons::OccurrenceId).eq(occurrence_id)),
    );
    if tx
        .fetch_optional_as::<(String,)>(&sql, values)
        .await?
        .is_some()
    {
        return Ok(Some(0));
    }
    let (sql, values) = build_sqlx(
        backend,
        Query::select()
            .column(CronSingletonSlots::Slot)
            .from(CronSingletonSlots::Table)
            .and_where(Expr::col(CronSingletonSlots::SingletonKey).eq(key))
            .and_where(Expr::col(CronSingletonSlots::OccurrenceId).eq(occurrence_id)),
    );
    Ok(tx
        .fetch_optional_as::<(i64,)>(&sql, values)
        .await?
        .map(|(slot,)| slot as u32))
}

/// Take one slot for `occurrence_id`: renew its own hold, take over one
/// whose lease has expired, or insert a free one — `Some(token)` when this
/// attempt now holds it. `fencing_token + 1` on every acquisition, so a
/// superseded holder's conditional writes match nothing.
pub(super) async fn take_slot(
    tx: &mut crate::storage::DbTransaction,
    backend: crate::storage::DbBackend,
    row: SlotRow<'_>,
    occurrence_id: &str,
    holder: &str,
    lease_until: &str,
    now: &'static str,
) -> Result<Option<i64>, OrionError> {
    // Ours to renew, or nobody's because the lease ran out. Anything else is
    // a live holder and matches nothing.
    let (mut update, (insert, conflict), mut token_query) = match row {
        SlotRow::Legacy { key } => (
            Query::update()
                .table(CronSingletons::Table)
                .value(CronSingletons::OccurrenceId, occurrence_id)
                .value(CronSingletons::Holder, holder)
                .value(
                    CronSingletons::FencingToken,
                    Expr::col(CronSingletons::FencingToken).add(1),
                )
                .value(
                    CronSingletons::LeaseUntil,
                    Expr::cust(lease_until.to_string()),
                )
                .cond_where(
                    Condition::all()
                        .add(Expr::col(CronSingletons::SingletonKey).eq(key))
                        .add(
                            Condition::any()
                                .add(Expr::col(CronSingletons::OccurrenceId).eq(occurrence_id))
                                .add(Expr::col(CronSingletons::LeaseUntil).lt(Expr::cust(now))),
                        ),
                )
                .to_owned(),
            (
                Query::insert()
                    .into_table(CronSingletons::Table)
                    .columns([
                        CronSingletons::SingletonKey,
                        CronSingletons::OccurrenceId,
                        CronSingletons::Holder,
                        CronSingletons::FencingToken,
                        CronSingletons::LeaseUntil,
                    ])
                    .values_panic([
                        key.into(),
                        occurrence_id.into(),
                        holder.into(),
                        1i64.into(),
                        Expr::cust(lease_until.to_string()),
                    ])
                    .to_owned(),
                vec![CronSingletons::SingletonKey.into_iden()],
            ),
            Query::select()
                .column(CronSingletons::FencingToken)
                .from(CronSingletons::Table)
                .and_where(Expr::col(CronSingletons::SingletonKey).eq(key))
                .to_owned(),
        ),
        SlotRow::Extra { key, slot } => (
            Query::update()
                .table(CronSingletonSlots::Table)
                .value(CronSingletonSlots::OccurrenceId, occurrence_id)
                .value(CronSingletonSlots::Holder, holder)
                .value(
                    CronSingletonSlots::FencingToken,
                    Expr::col(CronSingletonSlots::FencingToken).add(1),
                )
                .value(
                    CronSingletonSlots::LeaseUntil,
                    Expr::cust(lease_until.to_string()),
                )
                .cond_where(
                    Condition::all()
                        .add(Expr::col(CronSingletonSlots::SingletonKey).eq(key))
                        .add(Expr::col(CronSingletonSlots::Slot).eq(i64::from(slot)))
                        .add(
                            Condition::any()
                                .add(Expr::col(CronSingletonSlots::OccurrenceId).eq(occurrence_id))
                                .add(Expr::col(CronSingletonSlots::LeaseUntil).lt(Expr::cust(now))),
                        ),
                )
                .to_owned(),
            (
                Query::insert()
                    .into_table(CronSingletonSlots::Table)
                    .columns([
                        CronSingletonSlots::SingletonKey,
                        CronSingletonSlots::Slot,
                        CronSingletonSlots::OccurrenceId,
                        CronSingletonSlots::Holder,
                        CronSingletonSlots::FencingToken,
                        CronSingletonSlots::LeaseUntil,
                    ])
                    .values_panic([
                        key.into(),
                        i64::from(slot).into(),
                        occurrence_id.into(),
                        holder.into(),
                        1i64.into(),
                        Expr::cust(lease_until.to_string()),
                    ])
                    .to_owned(),
                vec![
                    CronSingletonSlots::SingletonKey.into_iden(),
                    CronSingletonSlots::Slot.into_iden(),
                ],
            ),
            Query::select()
                .column(CronSingletonSlots::FencingToken)
                .from(CronSingletonSlots::Table)
                .and_where(Expr::col(CronSingletonSlots::SingletonKey).eq(key))
                .and_where(Expr::col(CronSingletonSlots::Slot).eq(i64::from(slot)))
                .to_owned(),
        ),
    };
    let (sql, values) = build_sqlx(backend, &mut update);
    let took = tx.execute_query(&sql, values).await? > 0;
    if !took {
        // Either no row exists (the slot is free) or a live lease holds it.
        // Insert-if-absent settles which, without a read.
        if helpers::insert_if_absent_tx(tx, insert, conflict).await? == 0 {
            return Ok(None);
        }
    }
    // The token now held: what the heartbeat and the release are checked
    // against for the rest of the attempt.
    let (sql, values) = build_sqlx(backend, &mut token_query);
    Ok(tx
        .fetch_optional_as::<FencingTokenRow>(&sql, values)
        .await?
        .map(|row| row.fencing_token))
}
