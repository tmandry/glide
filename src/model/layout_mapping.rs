// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

/// The set of layouts for a given space, keyed by screen size.
///
/// These layouts all track approximately the same windows, but we save one per
/// screen size so that users can change how their windows are laid out in
/// different configurations.
///
/// To keep the number of configurations manageable, we only save a unique
/// layout for a screen size if it was modified by user commands under that
/// screen size.
#[derive(Serialize, Deserialize, Debug)]
pub struct SpaceLayoutMapping {
    active_size: Size,
    active_layout: LayoutId,
    active_save_state: SaveState,
    memory: HashMap<Size, LayoutId>,
    /// All the layouts owned by this space, their order, plus a refcount.
    ///
    /// The refcount includes the active space and is used for garbage
    /// collection.
    layouts: IndexMap<LayoutId, i16>,
}

/// How the active state should be saved.
#[derive(Serialize, Deserialize, Debug)]
enum SaveState {
    Unretained,
    Retained,
}

impl SpaceLayoutMapping {
    pub fn new(
        size: CGSize,
        tree: &mut LayoutTree,
        kind: LayoutKind,
        orientation: Orientation,
    ) -> Self {
        let layout = match kind {
            LayoutKind::Tree => tree.create_layout_with_orientation(orientation),
            LayoutKind::Scroll => tree.create_scroll_layout(),
        };
        SpaceLayoutMapping {
            active_size: size.into(),
            active_save_state: SaveState::Unretained,
            active_layout: layout,
            memory: Default::default(),
            layouts: indexmap! {layout => 1},
        }
    }

    pub fn activate_size(&mut self, size: CGSize, tree: &mut LayoutTree) {
        let size = size.into();
        debug!(?self, ?size, "create_or_activate_layout");

        // Save the current active layout and update refcounts.
        if let SaveState::Retained = self.active_save_state {
            let saved = self.active_layout;
            let removed = self.memory.insert(self.active_size, saved);
            self.increment_ref(saved);
            if let Some(removed) = removed {
                self.decrement_ref(removed);
            }
        }

        // Load the remembered layout, if any.
        self.active_save_state = SaveState::Unretained;
        self.active_size = size;
        if let Some(&retained) = self.memory.get(&size) {
            debug!("Loading saved layout");
            self.increment_ref(retained);
            self.decrement_ref(self.active_layout);
            self.active_layout = retained;
        } else {
            // Keep whatever the active layout was before. No need to adjust refcounts.
            debug!("Reusing active layout");
        }
        debug!("Using layout {:?}", self.active_layout);

        // Garbage collect.
        self.layouts.retain(|&layout, &mut refcount| {
            if refcount > 0 {
                true
            } else {
                debug!("Garbage collecting {layout:?}");
                tree.remove_layout(layout);
                false
            }
        });
    }

    /// Selects the given layout for the current size.
    ///
    /// The selection is automatically retained.
    pub fn select_layout(&mut self, layout: LayoutId) {
        assert!(self.layouts.contains_key(&layout));
        self.increment_ref(layout);
        self.decrement_ref(self.active_layout);
        self.active_layout = layout;
        self.retain_layout();
    }

    pub fn layouts(&self) -> impl ExactSizeIterator<Item = LayoutId> {
        self.layouts.keys().copied()
    }

    pub fn active_layout(&self) -> LayoutId {
        self.active_layout
    }

    pub fn active_layout_index(&self) -> usize {
        self.layouts.get_index_of(&self.active_layout).unwrap()
    }

    pub fn change_layout_index(&mut self, offset: i16) -> LayoutId {
        let cur = self.active_layout_index() as i16;
        let next = (cur + offset).rem_euclid(self.layouts.len() as i16);
        let layout = *self.layouts.get_index(next as usize).unwrap().0;
        self.select_layout(layout);
        self.active_layout
    }

    /// Call this when the user does anything to indicate that they would like
    /// to continue using this layout on the current screen size.
    pub fn retain_layout(&mut self) {
        self.active_save_state = SaveState::Retained;
    }

    /// Call this before performing structural modifications on the current
    /// layout.
    ///
    /// Note that active_layout() may return a new value after calling this.
    /// Modifications should be performed on the new layout.
    pub fn prepare_modify(&mut self, tree: &mut LayoutTree) -> LayoutId {
        // Clone the current layout if anything else is using it.
        if let Some(count) = self.layouts.get_mut(&self.active_layout)
            && *count > 1
        {
            *count -= 1;
            let cloned = tree.clone_layout(self.active_layout);
            self.layouts.insert(cloned, 1);
            self.active_layout = cloned;
        }
        self.active_save_state = SaveState::Retained;
        self.active_layout
    }

    pub fn replace_active_layout(&mut self, new_layout: LayoutId) {
        let old = self.active_layout;
        self.layouts.insert(new_layout, 1);
        self.decrement_ref(old);
        self.active_layout = new_layout;
        self.active_save_state = SaveState::Retained;
    }

    fn increment_ref(&mut self, layout: LayoutId) {
        *self.layouts.entry(layout).or_insert(0) += 1;
    }

    fn decrement_ref(&mut self, layout: LayoutId) {
        let count = self.layouts.entry(layout).or_insert(1);
        *count -= 1;
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Eq, PartialEq, Hash, Ord, PartialOrd, Debug)]
struct Size {
    width: i32,
    height: i32,
}

impl From<CGSize> for Size {
    fn from(value: CGSize) -> Self {
        Self {
            width: value.width.round() as i32,
            height: value.height.round() as i32,
        }
    }
}

use indexmap::{IndexMap, indexmap};
use objc2_core_foundation::CGSize;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::collections::HashMap;
use crate::model::{LayoutId, LayoutKind, LayoutTree, Orientation};

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE_1: CGSize = CGSize::new(1920.0, 1080.0);
    const SIZE_2: CGSize = CGSize::new(2560.0, 1440.0);
    const SIZE_3: CGSize = CGSize::new(1000.0, 1000.0);

    #[test]
    fn unmodified_layout_is_reused() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);
        let layout1 = mapping.active_layout();
        assert_eq!(tree.layouts().len(), 1);

        // Switch without retention
        mapping.activate_size(SIZE_2, &mut tree);
        assert_eq!(mapping.active_layout(), layout1);
        assert_eq!(mapping.active_size, SIZE_2.into());
        assert_eq!(tree.layouts().len(), 1);

        // Switch with retention
        mapping.retain_layout();
        mapping.activate_size(SIZE_3, &mut tree);
        assert_eq!(mapping.active_layout(), layout1);
        assert_eq!(tree.layouts().len(), 1);
    }

    #[test]
    fn prepare_modify_reuses_unique_layouts() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);
        let original_layout = mapping.active_layout();

        let modified_layout = mapping.prepare_modify(&mut tree);
        assert_eq!(original_layout, modified_layout);
        assert_eq!(tree.layouts().len(), 1);
    }

    #[test]
    fn prepare_modify_clones_shared_layouts() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);
        let original_layout = mapping.active_layout();
        assert_eq!(tree.layouts().len(), 1);

        // Create shared layout by retaining and switching
        mapping.retain_layout();
        mapping.activate_size(SIZE_2, &mut tree);
        mapping.activate_size(SIZE_1, &mut tree);

        // prepare_modify should clone the layout since it's shared
        let modified_layout = mapping.prepare_modify(&mut tree);
        assert_ne!(modified_layout, original_layout);
        assert_eq!(mapping.active_layout(), modified_layout);
        assert_eq!(tree.layouts().len(), 2);
    }

    #[test]
    fn state_is_not_saved_without_retention() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);

        // Switch without retention - should not save to memory
        mapping.activate_size(SIZE_2, &mut tree);
        let original_layout = mapping.active_layout();
        assert_eq!(mapping.memory.len(), 0);

        // Retain on SIZE_2 and switch to SIZE_3.
        mapping.retain_layout();
        mapping.activate_size(SIZE_3, &mut tree);

        // Modify to create a new layout, then switch back to SIZE_1.
        // The original layout should be forgotten for SIZE_1, and the
        // modified layout should be reused.
        let modified_layout = mapping.prepare_modify(&mut tree);
        assert_ne!(modified_layout, original_layout);
        mapping.activate_size(SIZE_1, &mut tree);
        assert_eq!(mapping.active_layout(), modified_layout);
    }

    #[test]
    fn state_is_saved_with_retention() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);

        // Switch with retention - should save to memory
        mapping.retain_layout();
        mapping.activate_size(SIZE_2, &mut tree);
        let original_layout = mapping.active_layout();

        // Retain on SIZE_2 and switch to SIZE_3.
        mapping.retain_layout();
        mapping.activate_size(SIZE_3, &mut tree);

        // Modify to create a new layout, then switch back to SIZE_1.
        // The original layout should be remembered for SIZE_1.
        let modified_layout = mapping.prepare_modify(&mut tree);
        assert_ne!(modified_layout, original_layout);
        mapping.activate_size(SIZE_1, &mut tree);
        assert_eq!(mapping.active_layout(), original_layout);
    }

    #[test]
    fn correct_refcount_with_retain() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);
        let layout1 = mapping.active_layout();

        // Initially refcount should be 1
        assert_eq!(mapping.layouts[&layout1], 1);

        // Retain and switch - this should increment refcount
        mapping.retain_layout();
        mapping.activate_size(SIZE_2, &mut tree);
        assert_eq!(mapping.layouts[&layout1], 2); // Active + saved in memory

        // Retain and switch back - should increase to 3
        mapping.retain_layout();
        mapping.activate_size(SIZE_1, &mut tree);
        assert_eq!(mapping.layouts[&layout1], 3);

        // Switch to a new size without retaining. The refcount stays the same
        // since the layout was already retained.
        mapping.activate_size(SIZE_3, &mut tree);
        assert_eq!(mapping.layouts[&layout1], 3);

        // Switch back, and to a new size after retaining.
        // The refcount stays the same.
        mapping.activate_size(SIZE_1, &mut tree);
        mapping.retain_layout();
        mapping.activate_size(SIZE_3, &mut tree);
        assert_eq!(mapping.layouts[&layout1], 3);
    }

    #[test]
    fn garbage_collection_removes_layouts_when_unused() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);
        let layout1 = mapping.active_layout();
        assert_eq!(mapping.layouts().len(), 1);
        assert_eq!(tree.layouts().len(), 1);

        mapping.retain_layout();
        mapping.activate_size(SIZE_2, &mut tree);
        let layout2 = mapping.prepare_modify(&mut tree);
        assert_eq!(mapping.layouts().len(), 2);
        assert_eq!(tree.layouts().len(), 2);

        mapping.activate_size(SIZE_1, &mut tree);
        assert_eq!(mapping.active_layout(), layout1);
        mapping.select_layout(layout2);
        assert_eq!(mapping.active_layout(), layout2);

        // At this point no sizes refer to layout1, but garbage collection
        // should not happen until a new size is activated.
        assert_eq!(mapping.layouts().len(), 2);
        assert_eq!(tree.layouts().len(), 2);

        mapping.activate_size(SIZE_2, &mut tree);
        assert_eq!(mapping.active_layout(), layout2);
        assert_eq!(mapping.layouts().len(), 1);
        assert_eq!(tree.layouts().len(), 1);
    }

    #[test]
    fn change_index() {
        let mut tree = LayoutTree::new();
        let mut mapping =
            SpaceLayoutMapping::new(SIZE_1, &mut tree, LayoutKind::Tree, Orientation::Horizontal);

        // Make three layouts.
        let layout1 = mapping.active_layout();
        mapping.retain_layout();
        mapping.activate_size(SIZE_2, &mut tree);
        let layout2 = mapping.prepare_modify(&mut tree);
        mapping.activate_size(SIZE_3, &mut tree);
        let layout3 = mapping.prepare_modify(&mut tree);
        mapping.activate_size(SIZE_1, &mut tree);

        // Move forwards.
        assert_eq!(mapping.active_layout(), layout1);
        assert_eq!(mapping.active_layout_index(), 0);
        mapping.change_layout_index(1);
        assert_eq!(mapping.active_layout(), layout2);
        assert_eq!(mapping.active_layout_index(), 1);
        mapping.change_layout_index(1);
        assert_eq!(mapping.active_layout(), layout3);
        assert_eq!(mapping.active_layout_index(), 2);
        mapping.change_layout_index(1);
        assert_eq!(mapping.active_layout(), layout1);
        assert_eq!(mapping.active_layout_index(), 0);

        // Move backwards.
        assert_eq!(mapping.active_layout(), layout1);
        assert_eq!(mapping.active_layout_index(), 0);
        mapping.change_layout_index(-1);
        assert_eq!(mapping.active_layout(), layout3);
        assert_eq!(mapping.active_layout_index(), 2);
        mapping.change_layout_index(-1);
        assert_eq!(mapping.active_layout(), layout2);
        assert_eq!(mapping.active_layout_index(), 1);
        mapping.change_layout_index(-1);
        assert_eq!(mapping.active_layout(), layout1);
        assert_eq!(mapping.active_layout_index(), 0);
    }
}
