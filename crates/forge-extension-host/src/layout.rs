//! Which extension panels live in which extension slot, and in what order.
//!
//! Each slot is a dock panel of its own; a slot holding several extension panels shows
//! them as tabs. The layout survives restarts (it is stored as JSON in Forge's key-value
//! store) and keeps entries for extensions that aren't loaded right now, so uninstalling
//! and reinstalling an extension puts it back where it was.

use serde::{Deserialize, Serialize};

/// How many extension slots (dock panels) exist.
pub const SLOTS: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtensionLayout {
    slots: Vec<Vec<String>>,
}

impl Default for ExtensionLayout {
    fn default() -> Self {
        Self { slots: vec![Vec::new(); SLOTS] }
    }
}

impl ExtensionLayout {
    pub fn from_json(json: &str) -> Self {
        let mut layout: Self = serde_json::from_str(json).unwrap_or_default();
        layout.slots.resize(SLOTS, Vec::new());
        layout
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn slot_of(&self, tab: &str) -> Option<usize> {
        self.slots.iter().position(|s| s.iter().any(|t| t == tab))
    }

    /// Tabs of `slot` that are currently registered, in order.
    pub fn tabs<'a>(&'a self, slot: usize, registered: &'a [String]) -> impl Iterator<Item = &'a String> + 'a {
        self.slots.get(slot).into_iter().flatten().filter(move |t| registered.contains(t))
    }

    fn is_free(&self, slot: usize, registered: &[String]) -> bool {
        self.tabs(slot, registered).next().is_none()
    }

    /// Gives a newly registered panel its own slot (the first free one) unless the layout
    /// already remembers where it goes. Returns whether the layout changed.
    pub fn place(&mut self, tab: &str, registered: &[String]) -> bool {
        if self.slot_of(tab).is_some() {
            return false;
        }
        let slot = (0..SLOTS).find(|s| self.is_free(*s, registered)).unwrap_or(SLOTS - 1);
        self.slots[slot].push(tab.to_string());
        true
    }

    /// Moves `tab` into `slot` before position `index` (among that slot's registered tabs;
    /// `None` appends).
    pub fn move_tab(&mut self, tab: &str, slot: usize, index: Option<usize>, registered: &[String]) {
        if slot >= SLOTS {
            return;
        }
        for s in &mut self.slots {
            s.retain(|t| t != tab);
        }
        // Translate the index among visible tabs into a position in the stored list.
        let visible: Vec<String> = self.tabs(slot, registered).cloned().collect();
        let at = match index.and_then(|i| visible.get(i)) {
            Some(before) => self.slots[slot].iter().position(|t| t == before).unwrap_or(self.slots[slot].len()),
            None => self.slots[slot].len(),
        };
        self.slots[slot].insert(at, tab.to_string());
    }

    /// Moves `tab` into a slot of its own; returns that slot (`None` when all are taken).
    pub fn move_to_free_slot(&mut self, tab: &str, registered: &[String]) -> Option<usize> {
        let current = self.slot_of(tab);
        // Already alone in its slot: keep it.
        if let Some(s) = current {
            if self.tabs(s, registered).all(|t| t == tab) {
                return Some(s);
            }
        }
        let slot = (0..SLOTS).find(|s| Some(*s) != current && self.is_free(*s, registered))?;
        self.move_tab(tab, slot, None, registered);
        Some(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn new_panels_get_their_own_slot() {
        let mut l = ExtensionLayout::default();
        let r = reg(&["a", "b"]);
        assert!(l.place("a", &r));
        assert!(l.place("b", &r));
        assert!(!l.place("a", &r), "already placed");
        assert_eq!((l.slot_of("a"), l.slot_of("b")), (Some(0), Some(1)));
    }

    #[test]
    fn grouping_reordering_and_splitting() {
        let mut l = ExtensionLayout::default();
        let r = reg(&["a", "b", "c"]);
        for t in ["a", "b", "c"] {
            l.place(t, &r);
        }
        // Drop b onto slot 0 → tabs [a, b]; slot 1 becomes free.
        l.move_tab("b", 0, None, &r);
        assert_eq!(l.tabs(0, &r).collect::<Vec<_>>(), ["a", "b"]);
        assert!(l.is_free(1, &r));
        // Reorder: b before a.
        l.move_tab("b", 0, Some(0), &r);
        assert_eq!(l.tabs(0, &r).collect::<Vec<_>>(), ["b", "a"]);
        // Drag a out to a window edge → first free slot (1).
        assert_eq!(l.move_to_free_slot("a", &r), Some(1));
        assert_eq!(l.tabs(0, &r).collect::<Vec<_>>(), ["b"]);
        // A tab already alone stays put.
        assert_eq!(l.move_to_free_slot("a", &r), Some(1));
    }

    #[test]
    fn remembers_unloaded_extensions_and_round_trips() {
        let mut l = ExtensionLayout::default();
        l.place("gone", &reg(&["gone"]));
        let r = reg(&["new"]);
        // "gone" isn't loaded now, so slot 0 counts as free for "new"…
        l.place("new", &r);
        assert_eq!(l.slot_of("new"), Some(0));
        // …but "gone" keeps its place for when it comes back.
        assert_eq!(l.slot_of("gone"), Some(0));
        let back = ExtensionLayout::from_json(&l.to_json());
        assert_eq!(back, l);
        assert_eq!(ExtensionLayout::from_json("garbage"), ExtensionLayout::default());
    }
}
