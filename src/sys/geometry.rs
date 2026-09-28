// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

use core_graphics_types::geometry as cg;
use objc2_core_foundation as ic;
use serde::{Deserialize, Deserializer, Serialize};
use serde_with::{DeserializeAs, SerializeAs};

pub trait ToICrate<T> {
    fn to_icrate(&self) -> T;
}

impl ToICrate<ic::CGPoint> for cg::CGPoint {
    fn to_icrate(&self) -> ic::CGPoint {
        ic::CGPoint { x: self.x, y: self.y }
    }
}

impl ToICrate<ic::CGSize> for cg::CGSize {
    fn to_icrate(&self) -> ic::CGSize {
        ic::CGSize {
            width: self.width,
            height: self.height,
        }
    }
}

impl ToICrate<ic::CGRect> for cg::CGRect {
    fn to_icrate(&self) -> ic::CGRect {
        ic::CGRect {
            origin: self.origin.to_icrate(),
            size: self.size.to_icrate(),
        }
    }
}

pub trait Round {
    fn round(&self) -> Self;
}

impl Round for ic::CGRect {
    fn round(&self) -> Self {
        // Round each corner to pixel boundaries, then use that to calculate the size.
        let min_rounded = self.min().round();
        let max_rounded = self.max().round();
        ic::CGRect {
            origin: min_rounded,
            size: ic::CGSize {
                width: max_rounded.x - min_rounded.x,
                height: max_rounded.y - min_rounded.y,
            },
        }
    }
}

pub fn round_to_physical(rect: ic::CGRect, scale: f64) -> ic::CGRect {
    let min_x = (rect.min().x * scale).round() / scale;
    let min_y = (rect.min().y * scale).round() / scale;
    let max_x = (rect.max().x * scale).round() / scale;
    let max_y = (rect.max().y * scale).round() / scale;
    ic::CGRect {
        origin: ic::CGPoint { x: min_x, y: min_y },
        size: ic::CGSize {
            width: max_x - min_x,
            height: max_y - min_y,
        },
    }
}

impl Round for ic::CGPoint {
    fn round(&self) -> Self {
        ic::CGPoint {
            x: self.x.round(),
            y: self.y.round(),
        }
    }
}

impl Round for ic::CGSize {
    fn round(&self) -> Self {
        ic::CGSize {
            width: self.width.round(),
            height: self.height.round(),
        }
    }
}

pub trait IsWithin {
    fn is_within(&self, how_much: f64, other: Self) -> bool;
}

impl IsWithin for ic::CGRect {
    fn is_within(&self, how_much: f64, other: Self) -> bool {
        self.origin.is_within(how_much, other.origin) && self.size.is_within(how_much, other.size)
    }
}

impl IsWithin for ic::CGPoint {
    fn is_within(&self, how_much: f64, other: Self) -> bool {
        self.x.is_within(how_much, other.x) && self.y.is_within(how_much, other.y)
    }
}

impl IsWithin for ic::CGSize {
    fn is_within(&self, how_much: f64, other: Self) -> bool {
        self.width.is_within(how_much, other.width) && self.height.is_within(how_much, other.height)
    }
}

impl IsWithin for f64 {
    fn is_within(&self, how_much: f64, other: Self) -> bool {
        (self - other).abs() < how_much
    }
}

pub trait SameAs: IsWithin + Sized {
    fn same_as(&self, other: Self) -> bool {
        self.is_within(0.1, other)
    }
}

impl SameAs for ic::CGRect {}
impl SameAs for ic::CGPoint {}
impl SameAs for ic::CGSize {}

pub trait CGSizeExt {
    fn contains(&self, other: Self) -> bool;
}

impl CGSizeExt for ic::CGSize {
    fn contains(&self, other: Self) -> bool {
        self.width >= other.width && self.height >= other.height
    }
}

pub trait CGRectExt {
    fn intersection(&self, other: &Self) -> Self;
    fn contains(&self, point: ic::CGPoint) -> bool;
    fn area(&self) -> f64;
    fn inset(&self, amount: f64) -> Self;
    fn scaled_about_center(&self, scale: f64) -> Self;
}

impl CGRectExt for ic::CGRect {
    fn intersection(&self, other: &Self) -> Self {
        let min_x = f64::max(self.min().x, other.min().x);
        let max_x = f64::min(self.max().x, other.max().x);
        let min_y = f64::max(self.min().y, other.min().y);
        let max_y = f64::min(self.max().y, other.max().y);
        ic::CGRect {
            origin: ic::CGPoint::new(min_x, min_y),
            size: ic::CGSize::new(f64::max(max_x - min_x, 0.), f64::max(max_y - min_y, 0.)),
        }
    }

    fn contains(&self, point: ic::CGPoint) -> bool {
        (self.min().x..=self.max().x).contains(&point.x)
            && (self.min().y..=self.max().y).contains(&point.y)
    }

    fn area(&self) -> f64 {
        self.size.width * self.size.height
    }

    fn inset(&self, amount: f64) -> Self {
        ic::CGRect {
            origin: ic::CGPoint {
                x: self.origin.x + amount,
                y: self.origin.y + amount,
            },
            size: ic::CGSize {
                width: self.size.width - amount * 2.0,
                height: self.size.height - amount * 2.0,
            },
        }
    }

    fn scaled_about_center(&self, scale: f64) -> Self {
        let size = ic::CGSize::new(self.size.width * scale, self.size.height * scale);
        let mid = self.mid();
        ic::CGRect::new(
            ic::CGPoint::new(mid.x - size.width / 2.0, mid.y - size.height / 2.0),
            size,
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ic::CGRect")]
pub struct CGRectDef {
    #[serde(with = "CGPointDef")]
    pub origin: ic::CGPoint,
    #[serde(with = "CGSizeDef")]
    pub size: ic::CGSize,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ic::CGPoint")]
pub struct CGPointDef {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "ic::CGSize")]
pub struct CGSizeDef {
    pub width: f64,
    pub height: f64,
}

impl SerializeAs<ic::CGRect> for CGRectDef {
    fn serialize_as<S>(value: &ic::CGRect, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        CGRectDef::serialize(value, serializer)
    }
}

impl<'de> DeserializeAs<'de, ic::CGRect> for CGRectDef {
    fn deserialize_as<D>(deserializer: D) -> Result<ic::CGRect, D::Error>
    where
        D: Deserializer<'de>,
    {
        CGRectDef::deserialize(deserializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_to_physical_retina() {
        let rect = ic::CGRect {
            origin: ic::CGPoint { x: 10.3, y: 20.7 },
            size: ic::CGSize { width: 100.3, height: 200.7 },
        };
        let result = round_to_physical(rect, 2.0);
        assert_eq!(result.origin.x, 10.5);
        assert_eq!(result.origin.y, 20.5);
        assert_eq!(result.size.width, 100.0);
        assert_eq!(result.size.height, 201.0);
    }

    #[test]
    fn round_to_physical_1x() {
        let rect = ic::CGRect {
            origin: ic::CGPoint { x: 10.3, y: 20.7 },
            size: ic::CGSize { width: 100.3, height: 200.7 },
        };
        let result = round_to_physical(rect, 1.0);
        assert_eq!(result.origin.x, 10.0);
        assert_eq!(result.origin.y, 21.0);
        assert_eq!(result.size.width, 101.0);
        assert_eq!(result.size.height, 200.0);
    }

    #[test]
    fn round_to_physical_preserves_pixel_aligned() {
        let rect = ic::CGRect {
            origin: ic::CGPoint { x: 10.0, y: 20.0 },
            size: ic::CGSize { width: 100.0, height: 200.0 },
        };
        let result = round_to_physical(rect, 2.0);
        assert_eq!(result.origin.x, 10.0);
        assert_eq!(result.origin.y, 20.0);
        assert_eq!(result.size.width, 100.0);
        assert_eq!(result.size.height, 200.0);
    }
}
