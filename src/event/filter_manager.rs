#![allow(unused)]
use super::ty::EventType;
use std::collections::{HashMap, HashSet};

/// The filter manager finds out what event we should subscribe if a new set is required.
///
/// For each change requested, it returns a set of commands that need to be sent to FreeSWITCH to
/// establish the required state.
#[derive(Clone, Default, Debug)]
struct FilterManager {
    /// The minimum set of we are interested in without an attached filter
    events: HashSet<EventType>,
    /// Specific name / value pair filters and on which events they should be apply to.
    filters: HashMap<Filter, HashSet<EventType>>,
}

impl FilterManager {
    /// Add new events to the global list of subscribed events.
    pub fn add_events(&mut self, events: impl IntoIterator<Item = EventType>) {
        self.events.extend(events)
    }

    pub fn remove_events(&mut self, events: impl IntoIterator<Item = EventType>) {
        for event in events {
            self.events.remove(&event);
        }
    }

    /// Add a filter for the given events.
    pub fn add_filter(&mut self, filter: Filter, events: impl IntoIterator<Item = EventType>) {
        todo!()
    }

    pub fn remove_filter(&mut self, filter: (String, String)) {
        todo!()
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Filter {
    name: String,
    value: String,
}

#[derive(Clone, Debug)]
enum SubscriptionChange {
    AddFilter(Filter),
    RemoveFilter(Filter),
    AddEvents(HashSet<EventType>),
    ClearEvents,
}

#[derive(Debug)]
struct EventFilterState {
    events: HashSet<EventType>,
    filters: HashSet<Filter>,
}

impl From<FilterManager> for EventFilterState {
    fn from(ef: FilterManager) -> Self {
        let mut events = ef.events;
        for filter_events in ef.filters.values() {
            events.extend(filter_events.iter())
        }
        let filters = HashSet::from_iter(ef.filters.keys().cloned());
        Self { events, filters }
    }
}

impl EventFilterState {
    pub fn changes(&self, next: &Self) -> Vec<SubscriptionChange> {
        // always add new filters first, so that - if no filter was installed together with the
        // requested event, we don't receive unfiltered events.
        todo!()
    }
}
