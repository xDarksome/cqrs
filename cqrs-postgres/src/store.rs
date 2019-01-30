use std::{fmt, marker::PhantomData};
use cqrs_core::{Aggregate, AggregateId, Event, EventNumber, EventSource, EventSink, Precondition, VersionedEvent, Since, Version, SerializableEvent, DeserializableEvent, SnapshotSink, SnapshotStrategy, SnapshotRecommendation, SnapshotSource, VersionedAggregate, NeverSnapshot};
use fallible_iterator::FallibleIterator;
use postgres::Connection;
use error::{LoadError, PersistError};
use util::{BorrowedJson, Json, RawJsonPersist, RawJsonRead};
use serde::{de::DeserializeOwned, Serialize};

/// A PostgreSQL storage backend.
pub struct PostgresStore<'conn, A, M, S = NeverSnapshot>
where
    A: Aggregate,
    S: SnapshotStrategy,
{
    conn: &'conn Connection,
    snapshot_strategy: S,
    _phantom: PhantomData<(A, M)>,
}

impl<'conn, A, M, S> fmt::Debug for PostgresStore<'conn, A, M, S>
where
    A: Aggregate,
    S: SnapshotStrategy,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("PostgresStore")
            .field("conn", &*self.conn)
            .finish()
    }
}

impl<'conn, A, M, S> PostgresStore<'conn, A, M, S>
where
    A: Aggregate,
    S: SnapshotStrategy + Default,
{
    /// Constructs a transient store based on a provided PostgreSQL connection using the default snapshot strategy.
    pub fn new(conn: &'conn Connection) -> Self {
        PostgresStore {
            conn,
            snapshot_strategy: S::default(),
            _phantom: PhantomData,
        }
    }

    /// Constructs a transient store based on a provided PostgreSQL connection and snapshot strategy.
    pub fn with_snapshot_strategy(conn: &'conn Connection, snapshot_strategy: S) -> Self {
        PostgresStore {
            conn,
            snapshot_strategy,
            _phantom: PhantomData,
        }
    }

    /// Creates the base set of tables required to support the CQRS system.
    pub fn create_tables(&self) -> Result<(), postgres::Error> {
        self.conn.batch_execute(include_str!("migrations/00_create_migrations.sql"))?;

        let current_version: i32 =
            self.conn.query("SELECT MAX(version) from migrations", &[])?
                .iter()
                .next()
                .and_then(|r| r.get(0))
                .unwrap_or_default();

        if current_version < 1 {
            self.conn.batch_execute(include_str!("migrations/01_create_tables.sql"))?;
        }

        Ok(())
    }
}

impl<'conn, A, M, S> EventSink<A, M> for PostgresStore<'conn, A, M, S>
where
    A: Aggregate,
    A::Event: SerializableEvent + fmt::Debug,
    M: Serialize + fmt::Debug,
    S: SnapshotStrategy,
{
    type Error = PersistError<<A::Event as SerializableEvent>::Error>;

    fn append_events<I>(&self, id: &I, events: &[A::Event], precondition: Option<Precondition>, metadata: M) -> Result<EventNumber, Self::Error>
    where
        I: AggregateId<Aggregate=A>,
    {
        let trans = self.conn.transaction()?;

        let check_stmt = trans.prepare_cached("SELECT MAX(sequence) FROM events WHERE entity_type = $1 AND entity_id = $2")?;

        let result = check_stmt.query(&[&A::entity_type(), &id.as_ref()])?;
        let current_version = result.iter().next().and_then(|r| {
            let max_sequence: Option<i64> = r.get(0);
            max_sequence.map(|x| {
                Version::new(x as u64)
            })
        });

        log::trace!("entity {}: current version: {:?}", id.as_ref(), current_version);

        if events.is_empty() {
            return Ok(current_version.unwrap_or_default().next_event())
        }

        if let Some(precondition) = precondition {
            precondition.verify(current_version)?;
        }

        log::trace!("entity {}: precondition satisfied", id.as_ref());

        let first_sequence = current_version.unwrap_or_default().next_event();
        let mut next_sequence = Version::Number(first_sequence);
        let mut buffer = Vec::with_capacity(128);

        let stmt = trans.prepare_cached("INSERT INTO events (entity_type, entity_id, sequence, event_type, payload, metadata, timestamp) VALUES ($1, $2, $3, $4, $5, $6, CURRENT_TIMESTAMP)")?;
        for event in events {
            buffer.clear();
            event.serialize_event_to_buffer(&mut buffer).map_err(PersistError::SerializationError)?;
            let modified_count = stmt.execute(&[&A::entity_type(), &id.as_ref(), &(next_sequence.get() as i64), &event.event_type(), &RawJsonPersist(&buffer), &BorrowedJson(&metadata)])?;
            debug_assert!(modified_count > 0);
            log::trace!("entity {}: inserted event; sequence: {}", id.as_ref(), next_sequence);
            next_sequence.incr();
        }

        trans.commit()?;

        Ok(first_sequence)
    }
}

impl<'conn, A, M, S> EventSource<A> for PostgresStore<'conn, A, M, S>
where
    A: Aggregate,
    A::Event: DeserializableEvent,
    S: SnapshotStrategy,
{
    type Events = Vec<Result<VersionedEvent<A::Event>, Self::Error>>;
    type Error = LoadError<<A::Event as DeserializableEvent>::Error>;

    fn read_events<I>(&self, id: &I, since: Since, max_count: Option<u64>) -> Result<Option<Self::Events>, Self::Error>
    where
        I: AggregateId<Aggregate=A>,
    {
        let last_sequence = match since {
            cqrs_core::Since::BeginningOfStream => 0,
            cqrs_core::Since::Event(x) => x.get(),
        } as i64;

        let events;
        let trans = self.conn.transaction_with(postgres::transaction::Config::default().read_only(true))?;

        let handle_row = |row: postgres::rows::Row| {
            let event_type: String = row.get("event_type");
            let sequence: i64 = row.get("sequence");
            let raw: RawJsonRead = row.get("payload");
            let event = A::Event::deserialize_event_from_buffer(&raw.0, &event_type).map_err(LoadError::DeserializationError)?.ok_or_else(|| LoadError::UnknownEventType(event_type.clone()))?;
            log::trace!("entity {}: loaded event; sequence: {}, type: {}", id.as_ref(), sequence, event_type);
            Ok(VersionedEvent {
                sequence: EventNumber::new(sequence as u64).expect("Sequence number should be non-zero"),
                event,
            })
        };

        let stmt;
        {
            let mut rows;
            if let Some(max_count) = max_count {
                stmt = trans.prepare_cached("SELECT sequence, event_type, payload FROM events WHERE entity_type = $1 AND entity_id = $2 AND sequence > $3 ORDER BY sequence ASC LIMIT $4")?;
                rows = stmt.lazy_query(&trans, &[&A::entity_type(), &id.as_ref(), &last_sequence, &(max_count.min(i64::max_value() as u64) as i64)], 100)?;
            } else {
                stmt = trans.prepare_cached("SELECT sequence, event_type, payload FROM events WHERE entity_type = $1 AND entity_id = $2 AND sequence > $3 ORDER BY sequence ASC")?;
                rows = stmt.lazy_query(&trans, &[&A::entity_type(), &id.as_ref(), &last_sequence], 100)?;
            }

            let (lower, upper) = rows.size_hint();
            let cap = upper.unwrap_or(lower);
            let mut inner_events = Vec::with_capacity(cap);

            while let Some(row) = rows.next()? {
                inner_events.push(handle_row(row));
            }
            events = inner_events;
        }

        trans.commit()?;

        log::trace!("entity {}: read {} events", id.as_ref(), events.len());

        Ok(Some(events))
    }
}

impl<'conn, A, M, S> SnapshotSink<A> for PostgresStore<'conn, A, M, S>
where
    A: Aggregate + Serialize + fmt::Debug,
    S: SnapshotStrategy,
{
    type Error = PersistError<serde_json::Error>;

    fn persist_snapshot<I>(&self, id: &I, aggregate: &A, version: Version, last_snapshot_version: Version) -> Result<Version, Self::Error>
    where
        I: AggregateId<Aggregate=A>,
    {
        if version <= last_snapshot_version || self.snapshot_strategy.snapshot_recommendation(version, last_snapshot_version) == SnapshotRecommendation::DoNotSnapshot {
            return Ok(last_snapshot_version);
        }

        let stmt = self.conn.prepare_cached("INSERT INTO snapshots (entity_type, entity_id, sequence, payload) VALUES ($1, $2, $3, $4)")?;
        let _modified_count = stmt.execute(&[&A::entity_type(), &id.as_ref(), &(version.get() as i64), &Json(aggregate)])?;

        // Clean up strategy for snapshots?
//        let stmt = self.conn.prepare_cached("DELETE FROM snapshots WHERE entity_type = $1 AND entity_id = $2 AND sequence < $3")?;
//        let _modified_count = stmt.execute(&[&A::entity_type(), &id.as_ref(), &(version.get() as i64)])?;

        log::trace!("entity {}: persisted snapshot", id.as_ref());
        Ok(version)
    }
}

impl<'conn, A, M, S> SnapshotSource<A> for PostgresStore<'conn, A, M, S>
where
    A: Aggregate + DeserializeOwned,
    S: SnapshotStrategy,
{
    type Error = postgres::Error;

    fn get_snapshot<I>(&self, id: &I) -> Result<Option<VersionedAggregate<A>>, Self::Error>
    where
        I: AggregateId<Aggregate=A>,
    {
        let stmt = self.conn.prepare_cached("SELECT sequence, payload FROM snapshots WHERE entity_type = $1 AND entity_id = $2 ORDER BY sequence DESC LIMIT 1")?;
        let rows = stmt.query(&[&A::entity_type(), &id.as_ref()])?;
        if let Some(row) = rows.iter().next() {
            let sequence: i64 = row.get("sequence");
            let raw: Json<A> = row.get("payload");
            log::trace!("entity {}: loaded snapshot", id.as_ref());
            Ok(Some(VersionedAggregate {
                version: Version::new(sequence as u64),
                payload: raw.0,
            }))
        } else {
            log::trace!("entity {}: no snapshot found", id.as_ref());
            Ok(None)
        }
    }
}