//! Resolution-independent desktop edge coordinates.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Desktop coordinates; right and bottom are exclusive.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

pub fn bounds(displays: &[Rect]) -> Option<Rect> {
    displays.iter().copied().reduce(|a, b| Rect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    })
}

/// Fraction from the top (vertical edges) or left (horizontal edges).
pub fn fraction(bounds: Rect, edge: Edge, x: f64, y: f64) -> u16 {
    let (value, start, end) = match edge {
        Edge::Left | Edge::Right => (y, bounds.top, bounds.bottom),
        Edge::Top | Edge::Bottom => (x, bounds.left, bounds.right),
    };
    (((value - start) / (end - start - 1.0).max(1.0)).clamp(0.0, 1.0) * f64::from(u16::MAX)).round()
        as u16
}

/// Place just inside the outer desktop edge. Gaps select the nearest real
/// segment of that edge, rather than placing the cursor outside a monitor.
pub fn entry(displays: &[Rect], edge: Edge, fraction: u16) -> Option<(f64, f64)> {
    let b = bounds(displays)?;
    let vertical = matches!(edge, Edge::Left | Edge::Right);
    let (start, end) = if vertical {
        (b.top, b.bottom)
    } else {
        (b.left, b.right)
    };
    let target = start + f64::from(fraction) / f64::from(u16::MAX) * (end - start - 1.0).max(0.0);
    displays
        .iter()
        .filter(|d| match edge {
            Edge::Left => d.left == b.left,
            Edge::Right => d.right == b.right,
            Edge::Top => d.top == b.top,
            Edge::Bottom => d.bottom == b.bottom,
        })
        .map(|d| {
            let inset = |min: f64, max: f64| (min + 1.0).min(max - 1.0).max(min);
            let along = if vertical {
                target.clamp(
                    inset(d.top, d.bottom),
                    (d.bottom - 2.0).max(inset(d.top, d.bottom)),
                )
            } else {
                target.clamp(
                    inset(d.left, d.right),
                    (d.right - 2.0).max(inset(d.left, d.right)),
                )
            };
            let point = match edge {
                Edge::Left => (inset(d.left, d.right), along),
                Edge::Right => ((d.right - 2.0).max(d.left), along),
                Edge::Top => (along, inset(d.top, d.bottom)),
                Edge::Bottom => (along, (d.bottom - 2.0).max(d.top)),
            };
            ((along - target).abs(), point)
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, point)| point)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_between_different_sizes_and_origins() {
        let source = Rect {
            left: -1920.,
            top: -100.,
            right: 0.,
            bottom: 901.,
        };
        let destination = Rect {
            left: 100.,
            top: 50.,
            right: 2660.,
            bottom: 2051.,
        };
        let f = fraction(source, Edge::Right, -1., 800.);
        let (x, y) = entry(&[destination], Edge::Left, f).unwrap();
        assert_eq!(x, 101.);
        assert!((y - 1850.).abs() < 0.1);
        let f = fraction(source, Edge::Bottom, -960., 900.);
        let (x, y) = entry(&[destination], Edge::Top, f).unwrap();
        assert!((x - 1380.).abs() < 1.);
        assert_eq!(y, 51.);
    }

    #[test]
    fn corners_and_gaps_stay_inside_real_displays() {
        let displays = [
            Rect {
                left: -100.,
                top: 0.,
                right: 100.,
                bottom: 100.,
            },
            Rect {
                left: -100.,
                top: 200.,
                right: 100.,
                bottom: 300.,
            },
        ];
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            for f in [0, u16::MAX / 2, u16::MAX] {
                let (x, y) = entry(&displays, edge, f).unwrap();
                assert!(
                    displays
                        .iter()
                        .any(|d| x > d.left && x < d.right - 1. && y > d.top && y < d.bottom - 1.)
                );
            }
        }
        assert!(entry(&[], Edge::Left, 0).is_none());
    }
}
