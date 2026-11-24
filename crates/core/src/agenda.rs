use crate::time::VirtualTime;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AgendaKey(pub u64);

/// Future platform events ordered by virtual time, then by registration order.
pub struct Agenda<E> {
    events: BTreeMap<(VirtualTime, AgendaKey), E>,
    keys: BTreeMap<AgendaKey, VirtualTime>,
    next_key: u64,
}

impl<E> Default for Agenda<E> {
    fn default() -> Self {
        Agenda { events: BTreeMap::new(), keys: BTreeMap::new(), next_key: 0 }
    }
}

impl<E> Agenda<E> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schedule(&mut self, t: VirtualTime, event: E) -> AgendaKey {
        let key = AgendaKey(self.next_key);
        self.next_key += 1;
        self.events.insert((t, key), event);
        self.keys.insert(key, t);
        key
    }

    /// Removes every event `f` selects.
    pub fn cancel_where(&mut self, f: impl Fn(&E) -> bool) {
        let gone: Vec<(VirtualTime, AgendaKey)> = self.events.iter().filter(|(_, e)| f(e)).map(|(k, _)| *k).collect();
        for (t, key) in gone {
            self.events.remove(&(t, key));
            self.keys.remove(&key);
        }
    }

    pub fn cancel(&mut self, key: AgendaKey) -> Option<E> {
        let t = self.keys.remove(&key)?;
        self.events.remove(&(t, key))
    }

    /// Inspect scheduled events in delivery order without consuming them.
    pub fn iter(&self) -> impl Iterator<Item = (VirtualTime, &E)> {
        self.events.iter().map(|((at, _), event)| (*at, event))
    }

    pub fn next_time(&self) -> Option<VirtualTime> {
        self.events.keys().next().map(|(t, _)| *t)
    }

    pub fn pop_due(&mut self, now: VirtualTime) -> Vec<(VirtualTime, E)> {
        let mut out = Vec::new();
        while let Some(((t, key), _)) = self.events.first_key_value() {
            if *t > now {
                break;
            }
            let (t, key) = (*t, *key);
            let e = self.events.remove(&(t, key)).expect("present");
            self.keys.remove(&key);
            out.push((t, e));
        }
        out
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Persisted<E> {
    next_key: u64,
    events: Vec<(VirtualTime, u64, E)>,
}

impl<E: serde::Serialize + Clone> serde::Serialize for Agenda<E> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        Persisted {
            next_key: self.next_key,
            events: self.events.iter().map(|((t, k), e)| (*t, k.0, e.clone())).collect(),
        }
        .serialize(s)
    }
}

impl<'de, E: serde::Deserialize<'de>> serde::Deserialize<'de> for Agenda<E> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let p = Persisted::<E>::deserialize(d)?;
        let mut agenda = Agenda { events: BTreeMap::new(), keys: BTreeMap::new(), next_key: p.next_key };
        for (t, k, e) in p.events {
            if k >= p.next_key {
                return Err(serde::de::Error::custom(format!("agenda key {k} is not below next_key {}", p.next_key)));
            }
            agenda.events.insert((t, AgendaKey(k)), e);
            agenda.keys.insert(AgendaKey(k), t);
        }
        Ok(agenda)
    }
}

#[cfg(test)]
#[path = "agenda_tests.rs"]
mod tests;
