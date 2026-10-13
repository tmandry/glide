// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

/// A number from 0 to 1.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
#[serde(try_from = "f64", into = "f64")]
pub struct Proportion(f64);

impl Proportion {
    /// Returns `None` if `value` is not from 0 to 1.
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=1.0).contains(&value).then_some(Proportion(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Proportion {
    type Error = String;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Proportion::new(value).ok_or_else(|| format!("expected a number from 0 to 1, got {value}"))
    }
}

impl From<Proportion> for f64 {
    fn from(proportion: Proportion) -> f64 {
        proportion.get()
    }
}
