use crate::{CqrsError, RawEvent};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum AggregatePredicate {
    AllAggregates(EventTypesPredicate),
    SpecificAggregates(&'static [SpecificAggregatePredicate]),
}

impl Default for AggregatePredicate {
    fn default() -> Self {
        AggregatePredicate::AllAggregates(EventTypesPredicate::default())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum EventTypesPredicate {
    AllEventTypes,
    SpecificEventTypes(&'static [&'static str]),
}

impl Default for EventTypesPredicate {
    fn default() -> Self {
        EventTypesPredicate::AllEventTypes
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct ReactionPredicate {
    pub aggregate_predicate: AggregatePredicate,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct SpecificAggregatePredicate {
    pub aggregate_type: &'static str,
    pub event_types: EventTypesPredicate,
}

/// A Reactor “reacts” to events, as they are created.
pub trait Reactor {
    fn start_reaction<R: Reaction>(reaction: R);
    fn stop_reaction();
}

/// A Reaction is stateless, triggering side-effects in response to an event's creation.
pub trait Reaction {
    type Error: CqrsError;

    fn reaction_name() -> &'static str;
    fn react(&mut self, event: RawEvent) -> Result<(), Self::Error>;
    fn predicate(&self) -> ReactionPredicate;
    fn interval() -> std::time::Duration;
}
