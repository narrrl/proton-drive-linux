//! A least-recently-used cache with a cost budget.
//!
//! Each entry carries a cost (bytes for a texture cache, `1` for a cache capped
//! by count). Inserting past the budget drops the entries used longest ago.
//! Lookups and touches are logarithmic, so a cache of a few thousand textures
//! can be touched on every tile bind without a linear scan.

use std::borrow::Borrow;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

struct Entry<V> {
    value: V,
    cost: usize,
    stamp: u64,
}

pub(crate) struct Lru<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// Entries by the stamp of their last use, oldest first.
    order: BTreeMap<u64, K>,
    next: u64,
    used: usize,
    budget: usize,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            next: 0,
            used: 0,
            budget,
        }
    }

    /// Look an entry up without marking it used.
    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.get(key).map(|entry| &entry.value)
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.contains_key(key)
    }

    /// Mark an entry as just used, so it is the last to be evicted. Returns
    /// whether it was there.
    pub(crate) fn touch<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let stamp = self.next;
        let Some(entry) = self.entries.get_mut(key) else {
            return false;
        };
        if let Some(held) = self.order.remove(&entry.stamp) {
            self.order.insert(stamp, held);
        }
        entry.stamp = stamp;
        self.next += 1;
        true
    }

    /// Look an entry up and mark it used.
    pub(crate) fn fetch<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if !self.touch(key) {
            return None;
        }
        self.get(key)
    }

    /// Store `value` as the most recently used entry, then evict the least
    /// recently used ones until the total cost fits the budget again. The new
    /// entry itself is never evicted, even when it alone is over budget.
    pub(crate) fn insert(&mut self, key: K, value: V, cost: usize) {
        self.remove(&key);
        let stamp = self.next;
        self.next += 1;
        self.order.insert(stamp, key.clone());
        self.entries.insert(key, Entry { value, cost, stamp });
        self.used += cost;
        while self.used > self.budget && self.order.len() > 1 {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.used -= entry.cost;
            }
        }
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let entry = self.entries.remove(key)?;
        self.order.remove(&entry.stamp);
        self.used -= entry.cost;
        Some(entry.value)
    }

    /// Keep only the entries `keep` accepts.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let order = &mut self.order;
        let used = &mut self.used;
        self.entries.retain(|key, entry| {
            let kept = keep(key, &entry.value);
            if !kept {
                order.remove(&entry.stamp);
                *used -= entry.cost;
            }
            kept
        });
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.used = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used
    }
}

#[cfg(test)]
mod tests {
    use super::Lru;

    #[test]
    fn evicts_least_recently_used_past_budget() {
        let mut cache = Lru::new(3);
        cache.insert("a", 1, 1);
        cache.insert("b", 2, 1);
        cache.insert("c", 3, 1);
        assert!(cache.touch("a"));
        cache.insert("d", 4, 1);
        assert!(cache.contains_key("a"));
        assert!(!cache.contains_key("b"));
        assert!(cache.contains_key("c"));
        assert!(cache.contains_key("d"));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn budget_counts_cost_not_entries() {
        let mut cache = Lru::new(100);
        cache.insert("small", (), 10);
        cache.insert("medium", (), 40);
        cache.insert("large", (), 60);
        // 110 > 100: the oldest goes, and that is enough.
        assert!(!cache.contains_key("small"));
        assert_eq!(cache.used(), 100);
        cache.insert("huge", (), 90);
        assert_eq!(cache.used(), 90);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn keeps_a_single_entry_over_budget() {
        let mut cache = Lru::new(10);
        cache.insert("a", (), 5);
        cache.insert("big", (), 50);
        assert!(cache.contains_key("big"));
        assert!(!cache.contains_key("a"));
        assert_eq!(cache.used(), 50);
    }

    #[test]
    fn remove_and_clear_keep_cost_in_step() {
        let mut cache = Lru::new(100);
        cache.insert("a", (), 10);
        cache.insert("b", (), 20);
        assert_eq!(cache.remove("a"), Some(()));
        assert_eq!(cache.used(), 20);
        cache.clear();
        assert_eq!(cache.used(), 0);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn reinsert_replaces_cost_and_refreshes() {
        let mut cache = Lru::new(30);
        cache.insert("a", 1, 10);
        cache.insert("b", 2, 10);
        cache.insert("a", 3, 15);
        assert_eq!(cache.used(), 25);
        assert_eq!(cache.get("a"), Some(&3));
        cache.insert("c", 4, 10);
        // "b" was used longest ago now.
        assert!(!cache.contains_key("b"));
        assert!(cache.contains_key("a"));
    }

    #[test]
    fn fetch_touches_and_get_does_not() {
        let mut cache = Lru::new(2);
        cache.insert("a", 1, 1);
        cache.insert("b", 2, 1);
        assert_eq!(cache.get("a"), Some(&1));
        cache.insert("c", 3, 1);
        assert!(!cache.contains_key("a"));
        assert_eq!(cache.fetch("b"), Some(&2));
        cache.insert("d", 4, 1);
        assert!(cache.contains_key("b"));
        assert!(!cache.contains_key("c"));
        assert_eq!(cache.fetch("missing"), None);
    }

    #[test]
    fn retain_drops_rejected_entries_and_their_cost() {
        let mut cache = Lru::new(100);
        cache.insert((true, "a".to_string()), (), 10);
        cache.insert((false, "b".to_string()), (), 20);
        cache.insert((true, "c".to_string()), (), 30);
        cache.retain(|(private, _), _| !private);
        assert_eq!(cache.used(), 20);
        assert_eq!(cache.len(), 1);
        assert!(cache.remove(&(false, "b".to_string())).is_some());
        assert_eq!(cache.used(), 0);
        cache.insert((true, "d".to_string()), (), 10);
        cache.clear();
        assert_eq!(cache.used(), 0);
        assert_eq!(cache.len(), 0);
    }
}
