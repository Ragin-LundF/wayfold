//! Bounded cache of directory listings shared by all tabs and Miller columns.
//! Bounded by total entry count; least recently used listings not on screen are evicted.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use wayfold_core::fs::Entry;

#[derive(Default, Debug)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub loading: bool,
    pub error: Option<String>,
    /// Bumped per load; batches from older loads are discarded.
    pub generation: u64,
    /// Refresh of an already shown listing: collect here, swap on finish (no flicker).
    pending: Option<Vec<Entry>>,
    last_used: u64,
    /// Changes whenever `entries` changes, unique across listings (taken from the cache clock);
    /// keys the sorted-row cache, which must not mix up a reloaded listing with an evicted one.
    pub version: u64,
}

pub struct ListingCache {
    map: HashMap<PathBuf, Listing>,
    clock: u64,
    max_entries: usize,
}

impl ListingCache {
    pub fn new(max_entries: usize) -> Self {
        Self { map: HashMap::new(), clock: 0, max_entries }
    }

    pub fn get(&self, path: &Path) -> Option<&Listing> {
        self.map.get(path)
    }

    pub fn touch(&mut self, path: &Path) {
        let clock = self.tick();
        if let Some(l) = self.map.get_mut(path) {
            l.last_used = clock;
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Start (re)loading `path`. Returns the generation to tag batches with.
    pub fn begin(&mut self, path: &Path) -> u64 {
        let clock = self.tick();
        let l = self.map.entry(path.to_path_buf()).or_default();
        l.generation += 1;
        l.version = clock;
        l.loading = true;
        l.last_used = clock;
        let shown = !l.entries.is_empty() || l.error.is_some();
        l.pending = shown.then(Vec::new);
        l.generation
    }

    /// Returns true when the batch was accepted (current generation).
    pub fn push_batch(&mut self, path: &Path, generation: u64, batch: Vec<Entry>) -> bool {
        let clock = self.tick();
        match self.map.get_mut(path) {
            Some(l) if l.generation == generation => {
                match &mut l.pending {
                    Some(p) => p.extend(batch),
                    None => {
                        l.entries.extend(batch);
                        l.version = clock;
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub fn finish(&mut self, path: &Path, generation: u64, error: Option<String>) -> bool {
        let clock = self.tick();
        match self.map.get_mut(path) {
            Some(l) if l.generation == generation => {
                l.loading = false;
                if let Some(p) = l.pending.take()
                    && error.is_none()
                {
                    l.entries = p;
                }
                if error.is_some() {
                    l.entries.clear();
                }
                l.error = error;
                l.version = clock;
                true
            }
            _ => false,
        }
    }

    /// Insert or replace one entry (e.g. right after creating it) without a full reload.
    pub fn upsert(&mut self, dir: &Path, entry: Entry) {
        let clock = self.tick();
        if let Some(l) = self.map.get_mut(dir) {
            l.entries.retain(|e| e.path != entry.path);
            l.entries.push(entry);
            l.version = clock;
        }
    }

    /// Evict least recently used listings outside `keep` until under the entry bound.
    pub fn evict(&mut self, keep: &HashSet<PathBuf>) {
        let mut total: usize = self.map.values().map(|l| l.entries.len()).sum();
        if total <= self.max_entries {
            return;
        }
        let mut victims: Vec<(u64, PathBuf)> =
            self.map.iter().filter(|(p, l)| !keep.contains(*p) && !l.loading).map(|(p, l)| (l.last_used, p.clone())).collect();
        victims.sort();
        for (_, p) in victims {
            if total <= self.max_entries {
                break;
            }
            if let Some(l) = self.map.remove(&p) {
                total -= l.entries.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wayfold_core::fs::stat_entry;

    fn batch(n: usize) -> Vec<Entry> {
        (0..n).map(|i| stat_entry(Path::new(&format!("/nonexistent/{i}")))).collect()
    }

    #[test]
    fn stale_batches_are_discarded() {
        let mut c = ListingCache::new(1000);
        let p = Path::new("/a");
        let g1 = c.begin(p);
        let g2 = c.begin(p);
        assert!(!c.push_batch(p, g1, batch(3)));
        assert!(c.push_batch(p, g2, batch(2)));
        assert!(c.finish(p, g2, None));
        assert_eq!(c.get(p).unwrap().entries.len(), 2);
    }

    #[test]
    fn refresh_swaps_on_finish() {
        let mut c = ListingCache::new(1000);
        let p = Path::new("/a");
        let g = c.begin(p);
        c.push_batch(p, g, batch(5));
        c.finish(p, g, None);
        let g = c.begin(p);
        c.push_batch(p, g, batch(1));
        assert_eq!(c.get(p).unwrap().entries.len(), 5, "old entries stay until refresh completes");
        c.finish(p, g, None);
        assert_eq!(c.get(p).unwrap().entries.len(), 1);
    }

    #[test]
    fn eviction_is_bounded_and_keeps_visible() {
        let mut c = ListingCache::new(10);
        for name in ["/a", "/b", "/c"] {
            let p = Path::new(name);
            let g = c.begin(p);
            c.push_batch(p, g, batch(6));
            c.finish(p, g, None);
        }
        let keep: HashSet<PathBuf> = [PathBuf::from("/a")].into();
        c.evict(&keep);
        assert!(c.get(Path::new("/a")).is_some());
        assert!(c.get(Path::new("/b")).is_none() && c.get(Path::new("/c")).is_none());
    }

    #[test]
    fn versions_are_unique_across_listings_and_reloads() {
        let mut c = ListingCache::new(1);
        let load = |c: &mut ListingCache, name: &str| {
            let p = Path::new(name);
            let g = c.begin(p);
            c.push_batch(p, g, batch(2));
            c.finish(p, g, None);
            c.get(p).unwrap().version
        };
        let a = load(&mut c, "/a");
        let b = load(&mut c, "/b");
        c.evict(&HashSet::new());
        assert!(c.get(Path::new("/a")).is_none());
        let a_again = load(&mut c, "/a");
        assert!(a != b && a_again != a && a_again != b, "{a} {b} {a_again}");
    }
}
