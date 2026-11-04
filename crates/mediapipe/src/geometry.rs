//! 轻量几何类型，替代 nvgx 的 Rect/Point/Size。
//! 仅包含纯数据与简单算术，无外部依赖，与原 nvgx 类型字段布局一致。

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl From<(f32, f32)> for Point {
    #[inline]
    fn from(v: (f32, f32)) -> Self {
        Self { x: v.0, y: v.1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl From<(f32, f32)> for Size {
    #[inline]
    fn from(v: (f32, f32)) -> Self {
        Self {
            width: v.0,
            height: v.1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub xy: Point,
    pub size: Size,
}

impl Rect {
    #[inline]
    pub fn area(&self) -> f32 {
        self.size.width * self.size.height
    }

    #[inline]
    pub fn intersect(&self, rhs: &Rect) -> Rect {
        let left = self.xy.x.max(rhs.xy.x);
        let top = self.xy.y.max(rhs.xy.y);
        let right = (self.xy.x + self.size.width).min(rhs.xy.x + rhs.size.width);
        let bottom = (self.xy.y + self.size.height).min(rhs.xy.y + rhs.size.height);
        Rect {
            xy: Point { x: left, y: top },
            size: Size {
                width: (right - left).max(0.0),
                height: (bottom - top).max(0.0),
            },
        }
    }
}
