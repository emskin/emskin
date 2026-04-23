//! Per-connection registry of fcitx5 input contexts.
//!
//! Each DBus client that calls `InputMethod1.CreateInputContext` gets
//! back an object path that references an IC we host locally. This
//! module allocates those paths, tracks per-IC state (capability,
//! focus, last-reported cursor rect), and hands out the uuid that the
//! portal frontend echoes back.
//!
//! The registry is **per connection**: fcitx5 IC paths aren't shared
//! across DBus clients in real fcitx5 either, so there's no need to
//! put it on a process-global map. The broker stores one
//! [`IcRegistry`] per [`crate::broker::state::ConnectionState`] owner.

use std::collections::HashMap;

/// Portal-style IC object path. Matches fcitx5's format so clients
/// that hardcode the prefix recognize us.
pub const PORTAL_IC_PATH_PREFIX: &str = "/org/freedesktop/portal/inputcontext/";

/// Everything we track for one IC. Fields are mutable via
/// [`IcRegistry::get_mut`]; the `id` / uuid are immutable after
/// allocation so clients can round-trip them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcState {
    pub id: u64,
    pub uuid: [u8; 16],
    /// Bits set via `InputContext1.SetCapability`. fcitx5's capability
    /// flags — preedit-inline, surrounding-text, password, etc.
    pub capability: u64,
    /// Whether the client has called `FocusIn` more recently than
    /// `FocusOut`.
    pub focused: bool,
    /// Last client-reported cursor rect in the client's surface-local
    /// frame. `None` until `SetCursorRect[V2]` / `SetCursorLocation`
    /// runs at least once.
    pub cursor_rect: Option<[i32; 4]>,
}

#[derive(Debug, Default)]
pub struct IcRegistry {
    /// IC paths → state.
    ics: HashMap<String, IcState>,
    /// Monotonic counter for allocated IC ids. Starts at 1; `0` is
    /// reserved as "no IC".
    next_id: u64,
}

impl IcRegistry {
    pub fn new() -> Self {
        Self {
            ics: HashMap::new(),
            next_id: 1,
        }
    }

    /// Allocate a fresh IC and return its object path + uuid. The
    /// uuid is deterministically derived from the id (64-bit in the
    /// high half, zeroed low half) so tests can assert on it; real
    /// fcitx5 uses a random uuid but clients treat it as opaque.
    pub fn allocate(&mut self) -> (String, IcState) {
        let id = self.next_id;
        self.next_id += 1;
        let path = format!("{PORTAL_IC_PATH_PREFIX}{id}");
        let mut uuid = [0u8; 16];
        uuid[..8].copy_from_slice(&id.to_le_bytes());
        let state = IcState {
            id,
            uuid,
            capability: 0,
            focused: false,
            cursor_rect: None,
        };
        self.ics.insert(path.clone(), state.clone());
        (path, state)
    }

    pub fn get(&self, path: &str) -> Option<&IcState> {
        self.ics.get(path)
    }

    pub fn get_mut(&mut self, path: &str) -> Option<&mut IcState> {
        self.ics.get_mut(path)
    }

    /// Drop IC state. Returns the removed state, or `None` if the
    /// path wasn't known.
    pub fn destroy(&mut self, path: &str) -> Option<IcState> {
        self.ics.remove(path)
    }

    pub fn len(&self) -> usize {
        self.ics.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ics.is_empty()
    }

    /// Iterate over `(path, state)` pairs. Stable order is **not**
    /// guaranteed — only the current focused IC really matters for
    /// the broker.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &IcState)> {
        self.ics.iter().map(|(k, v)| (k.as_str(), v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_registry_is_empty() {
        let r = IcRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn allocate_returns_portal_style_path() {
        let mut r = IcRegistry::new();
        let (path, _) = r.allocate();
        assert_eq!(path, "/org/freedesktop/portal/inputcontext/1");
    }

    #[test]
    fn allocate_increments_id() {
        let mut r = IcRegistry::new();
        let (p1, s1) = r.allocate();
        let (p2, s2) = r.allocate();
        assert_ne!(p1, p2);
        assert_eq!(s1.id, 1);
        assert_eq!(s2.id, 2);
    }

    #[test]
    fn uuid_encodes_id_in_low_bytes() {
        let mut r = IcRegistry::new();
        let (_, s) = r.allocate();
        assert_eq!(s.uuid[..8], 1u64.to_le_bytes());
    }

    #[test]
    fn get_returns_registered_ic() {
        let mut r = IcRegistry::new();
        let (path, s) = r.allocate();
        assert_eq!(r.get(&path), Some(&s));
    }

    #[test]
    fn get_mut_allows_field_updates() {
        let mut r = IcRegistry::new();
        let (path, _) = r.allocate();
        let st = r.get_mut(&path).unwrap();
        st.focused = true;
        st.capability = 0xF;
        st.cursor_rect = Some([1, 2, 3, 4]);

        let st_again = r.get(&path).unwrap();
        assert!(st_again.focused);
        assert_eq!(st_again.capability, 0xF);
        assert_eq!(st_again.cursor_rect, Some([1, 2, 3, 4]));
    }

    #[test]
    fn destroy_removes_and_returns_state() {
        let mut r = IcRegistry::new();
        let (path, s) = r.allocate();
        assert_eq!(r.destroy(&path), Some(s));
        assert!(r.get(&path).is_none());
        assert!(r.is_empty());
    }

    #[test]
    fn destroy_of_unknown_path_is_none() {
        let mut r = IcRegistry::new();
        assert_eq!(r.destroy("/does-not-exist"), None);
    }

    #[test]
    fn ids_after_destroy_keep_counting_forward() {
        // Once allocated, ids never recycle — the monotonic counter
        // lets clients hold stale ic_paths without collisions.
        let mut r = IcRegistry::new();
        let (p1, _) = r.allocate();
        r.destroy(&p1);
        let (p2, _) = r.allocate();
        assert_eq!(p2, "/org/freedesktop/portal/inputcontext/2");
    }

    #[test]
    fn iter_visits_every_ic() {
        let mut r = IcRegistry::new();
        let _ = r.allocate();
        let _ = r.allocate();
        let _ = r.allocate();
        assert_eq!(r.iter().count(), 3);
    }
}
