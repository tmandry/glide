// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! This module defines the [`LayoutTree`][layout_tree::LayoutTree] data
//! structure, on which all layout logic is defined.

mod layout_mapping;
mod layout_tree;
mod proportion;
mod scroll_constraints;
pub mod scroll_viewport;
mod selection;
mod size;
pub mod spring;
mod tree;
mod window;

pub use layout_mapping::SpaceLayoutMapping;
pub use layout_tree::{LayoutId, LayoutKind, LayoutTree};
pub use proportion::Proportion;
pub use size::{ContainerKind, Direction, GroupBarInfo, Orientation};
pub use tree::NodeId;
