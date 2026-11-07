//! Change gate: a decision is only requested again when its redacted input
//! (or question set) changed since the last answer. Both caches are bounded,
//! with no new crate: a `HashMap` plus a `VecDeque` of keys in recency order.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use sha2::{Digest, Sha256};

use super::Question;
use crate::team::config::DecisionPath;

/// Capacity of the triage verdict cache (body hash to verdict).
pub const TRIAGE_CACHE_CAPACITY: usize = 256;
/// Capacity of the change gate itself. Normally one entry per (path, member);
/// task-keyed paths are bounded by this cap as well.
pub const GATE_CAPACITY: usize = 256;

/// Least-recently-used map with a fixed capacity.
#[derive(Debug, Clone)]
pub struct BoundedCache<K, V> {
    capacity: usize,
    map: HashMap<K, V>,
    /// Oldest first.
    order: VecDeque<K>,
}

impl<K: Eq + Hash + Clone, V> BoundedCache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn touch(&mut self, key: &K) {
        if let Some(position) = self.order.iter().position(|k| k == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.clone());
    }

    /// Insert or replace, marking the key most recent. Evicts the oldest entry
    /// when the cache is full.
    pub fn insert(&mut self, key: K, value: V) {
        if !self.map.contains_key(&key) && self.map.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.touch(&key);
        self.map.insert(key, value);
    }

    /// Read and mark the key most recent.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        if self.map.contains_key(key) {
            self.touch(key);
        }
        self.map.get(key)
    }

    /// Read without changing recency, for `&self` callers.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        if let Some(position) = self.order.iter().position(|k| k == key) {
            self.order.remove(position);
        }
        self.map.remove(key)
    }
}

/// Hash of the redacted input plus the question set, as a `u64` key.
pub fn input_hash(redacted_input: &str, questions: &[Question]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(redacted_input.as_bytes());
    hasher.update([0u8]);
    for question in questions {
        hasher.update(format!("{question:?}").as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// Hash of text alone, for caches keyed by content (the triage cache).
pub fn text_hash(redacted_text: &str) -> u64 {
    input_hash(redacted_text, &[])
}

/// Last input hash and answer per `(path, key)`.
#[derive(Debug, Clone)]
pub struct ChangeGate<T> {
    entries: BoundedCache<(DecisionPath, String), (u64, T)>,
}

impl<T> Default for ChangeGate<T> {
    fn default() -> Self {
        Self::new(GATE_CAPACITY)
    }
}

impl<T> ChangeGate<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: BoundedCache::new(capacity),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// True when `hash` equals the hash of the last recorded answer.
    pub fn unchanged(&self, path: DecisionPath, key: &str, hash: u64) -> bool {
        self.entries
            .peek(&(path, key.to_string()))
            .is_some_and(|(last, _)| *last == hash)
    }

    /// The last recorded answer for `(path, key)`, whatever its input was.
    pub fn answer(&self, path: DecisionPath, key: &str) -> Option<&T> {
        self.entries
            .peek(&(path, key.to_string()))
            .map(|(_, answer)| answer)
    }

    pub fn record(&mut self, path: DecisionPath, key: &str, hash: u64, answer: T) {
        self.entries.insert((path, key.to_string()), (hash, answer));
    }

    pub fn forget(&mut self, path: DecisionPath, key: &str) {
        self.entries.remove(&(path, key.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(name: &str) -> Question {
        Question::Predicate {
            name: name.into(),
            instructions: "x".into(),
        }
    }

    #[test]
    fn unchanged_input_hits_the_gate() {
        let mut gate: ChangeGate<&str> = ChangeGate::default();
        let questions = [question("q")];
        let hash = input_hash("same input", &questions);
        assert!(!gate.unchanged(DecisionPath::Triage, "msg-1", hash));
        gate.record(DecisionPath::Triage, "msg-1", hash, "noise");
        assert!(gate.unchanged(DecisionPath::Triage, "msg-1", hash));
        assert_eq!(gate.answer(DecisionPath::Triage, "msg-1"), Some(&"noise"));
    }

    #[test]
    fn changed_input_or_questions_miss_the_gate() {
        let mut gate: ChangeGate<u8> = ChangeGate::default();
        let questions = [question("q")];
        gate.record(
            DecisionPath::Triage,
            "msg-1",
            input_hash("old", &questions),
            1,
        );
        assert!(!gate.unchanged(DecisionPath::Triage, "msg-1", input_hash("new", &questions)));
        assert!(!gate.unchanged(
            DecisionPath::Triage,
            "msg-1",
            input_hash("old", &[question("other")])
        ));
    }

    #[test]
    fn gate_entries_are_per_path_and_key() {
        let mut gate: ChangeGate<u8> = ChangeGate::default();
        let hash = input_hash("x", &[]);
        gate.record(DecisionPath::Triage, "a", hash, 1);
        assert!(!gate.unchanged(DecisionPath::Triage, "b", hash));
        assert!(!gate.unchanged(DecisionPath::Stall, "a", hash));
        gate.forget(DecisionPath::Triage, "a");
        assert!(gate.is_empty());
    }

    #[test]
    fn gate_is_bounded() {
        let mut gate: ChangeGate<usize> = ChangeGate::new(4);
        for i in 0..10 {
            gate.record(DecisionPath::Completion, &format!("task-{i}"), i as u64, i);
        }
        assert_eq!(gate.len(), 4);
        assert!(gate.answer(DecisionPath::Completion, "task-0").is_none());
        assert!(gate.answer(DecisionPath::Completion, "task-9").is_some());
    }

    #[test]
    fn hash_is_stable_and_input_sensitive() {
        assert_eq!(input_hash("a", &[]), input_hash("a", &[]));
        assert_ne!(input_hash("a", &[]), input_hash("b", &[]));
        assert_eq!(text_hash("a"), input_hash("a", &[]));
    }

    #[test]
    fn lru_holds_256_and_evicts_the_oldest() {
        let mut cache: BoundedCache<u64, u64> = BoundedCache::new(TRIAGE_CACHE_CAPACITY);
        for i in 0..257u64 {
            cache.insert(i, i);
        }
        assert_eq!(cache.len(), 256);
        assert!(cache.peek(&0).is_none());
        assert_eq!(cache.peek(&1), Some(&1));
        assert_eq!(cache.peek(&256), Some(&256));
    }

    #[test]
    fn lru_get_protects_recently_used_entries() {
        let mut cache: BoundedCache<u8, u8> = BoundedCache::new(3);
        cache.insert(1, 1);
        cache.insert(2, 2);
        cache.insert(3, 3);
        assert_eq!(cache.get(&1), Some(&1));
        cache.insert(4, 4);
        assert!(cache.peek(&2).is_none());
        assert!(cache.peek(&1).is_some());
    }

    #[test]
    fn lru_reinsert_replaces_without_growth() {
        let mut cache: BoundedCache<u8, u8> = BoundedCache::new(2);
        cache.insert(1, 1);
        cache.insert(1, 9);
        cache.insert(2, 2);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.peek(&1), Some(&9));
        assert_eq!(cache.remove(&1), Some(9));
        assert_eq!(cache.len(), 1);
    }
}
