use crate::{
    als::Scale,
    predictor::{data::Entry, AlsDirection},
};

use super::adaptive::LumaDirection;

// Lux is logarithmic, so this covers readings up to 999,999 lux. Linear ALS
// sources are defined on 0..=100 and therefore occupy coordinates 0..=5.
const LUX_COORDINATE_MAX: f64 = 6.0;
const LINEAR_COORDINATE_MAX: f64 = 5.0;
const LUMA_COORDINATE_MAX: f64 = 5.0;

/// A piecewise-bilinear monotonic surface whose knots include every learned
/// condition. At each knot, the lower envelope is the greatest observation
/// dominated by that condition and the upper envelope is the least observation
/// dominating it. Both envelopes are monotonic, so their midpoint is too.
///
/// A consistent observation makes both envelopes equal to its value and is
/// therefore reproduced exactly. Contradictory observations are retained and
/// compromised between rather than silently removed.
pub struct Model {
    values: Vec<f64>,
    als_knots: Vec<f64>,
    luma_knots: Vec<f64>,
    scale: Scale,
    als_coordinate_max: f64,
    als_direction: AlsDirection,
    luma_direction: LumaDirection,
    luma_aware: bool,
}

impl Model {
    pub fn fit(
        entries: &[Entry],
        scale: Scale,
        als_direction: AlsDirection,
        luma_direction: LumaDirection,
        luma_aware: bool,
    ) -> Option<Self> {
        if entries.is_empty() {
            return None;
        }

        let als_max = entries
            .iter()
            .map(|entry| scale.coordinate(entry.als))
            .fold(als_coordinate_max(scale), f64::max);
        let orient_als = |entry: &Entry| {
            orient(
                scale.coordinate(entry.als).clamp(0.0, als_max),
                als_max,
                als_direction == AlsDirection::Decreasing,
            )
        };
        let orient_luma = |entry: &Entry| {
            if luma_aware {
                orient(
                    (entry.luma as f64 / 20.0).clamp(0.0, LUMA_COORDINATE_MAX),
                    LUMA_COORDINATE_MAX,
                    luma_direction == LumaDirection::Decreasing,
                )
            } else {
                0.0
            }
        };

        let mut als_knots = vec![0.0, als_max];
        als_knots.extend(entries.iter().map(orient_als));
        sort_deduplicate(&mut als_knots);

        let mut luma_knots = if luma_aware {
            let mut knots = vec![0.0, LUMA_COORDINATE_MAX];
            knots.extend(entries.iter().map(orient_luma));
            sort_deduplicate(&mut knots);
            knots
        } else {
            vec![0.0]
        };

        // Keep the allocation and indexing below straightforward even if the
        // luma-aware construction changes in the future.
        sort_deduplicate(&mut luma_knots);
        let width = als_knots.len();
        let height = luma_knots.len();
        let minimum = entries.iter().map(|entry| entry.brightness).min().unwrap() as f64;
        let maximum = entries.iter().map(|entry| entry.brightness).max().unwrap() as f64;

        // Multiple observations can map to one clamped condition. Keep both
        // extremes: the envelope midpoint will make the conflict explicit.
        let mut anchor_min = vec![None::<f64>; width * height];
        let mut anchor_max = vec![None::<f64>; width * height];
        for entry in entries {
            let x = knot_index(&als_knots, orient_als(entry));
            let y = knot_index(&luma_knots, orient_luma(entry));
            let index = y * width + x;
            let value = entry.brightness as f64;
            anchor_min[index] = Some(anchor_min[index].map_or(value, |old| old.min(value)));
            anchor_max[index] = Some(anchor_max[index].map_or(value, |old| old.max(value)));
        }

        // Prefix maxima and suffix minima compute the two monotonic envelopes
        // in O(width * height), rather than scanning every observation at every
        // knot.
        let mut lower = vec![minimum; width * height];
        for y in 0..height {
            for x in 0..width {
                let index = y * width + x;
                let mut value = anchor_max[index].unwrap_or(minimum);
                if x > 0 {
                    value = value.max(lower[index - 1]);
                }
                if y > 0 {
                    value = value.max(lower[index - width]);
                }
                lower[index] = value;
            }
        }

        let mut upper = vec![maximum; width * height];
        for y in (0..height).rev() {
            for x in (0..width).rev() {
                let index = y * width + x;
                let mut value = anchor_min[index].unwrap_or(maximum);
                if x + 1 < width {
                    value = value.min(upper[index + 1]);
                }
                if y + 1 < height {
                    value = value.min(upper[index + width]);
                }
                upper[index] = value;
            }
        }

        let values = lower
            .into_iter()
            .zip(upper)
            .map(|(lower, upper)| (lower + upper) / 2.0)
            .collect();

        Some(Self {
            values,
            als_knots,
            luma_knots,
            scale,
            als_coordinate_max: als_max,
            als_direction,
            luma_direction,
            luma_aware,
        })
    }

    pub fn predict(&self, als: u64, luma: u8) -> f64 {
        let als_coordinate = orient(
            self.scale
                .coordinate(als)
                .clamp(0.0, self.als_coordinate_max),
            self.als_coordinate_max,
            self.als_direction == AlsDirection::Decreasing,
        );
        let luma_coordinate = if self.luma_aware {
            orient(
                (luma as f64 / 20.0).clamp(0.0, LUMA_COORDINATE_MAX),
                LUMA_COORDINATE_MAX,
                self.luma_direction == LumaDirection::Decreasing,
            )
        } else {
            0.0
        };

        let (x0, x1, tx) = interval(&self.als_knots, als_coordinate);
        let (y0, y1, ty) = interval(&self.luma_knots, luma_coordinate);
        let width = self.als_knots.len();
        let bottom = lerp(
            self.values[y0 * width + x0],
            self.values[y0 * width + x1],
            tx,
        );
        let top = lerp(
            self.values[y1 * width + x0],
            self.values[y1 * width + x1],
            tx,
        );
        lerp(bottom, top, ty)
    }
}

fn als_coordinate_max(scale: Scale) -> f64 {
    match scale {
        Scale::Lux => LUX_COORDINATE_MAX,
        Scale::Linear => LINEAR_COORDINATE_MAX,
    }
}

fn orient(coordinate: f64, maximum: f64, decreasing: bool) -> f64 {
    if decreasing {
        maximum - coordinate
    } else {
        coordinate
    }
}

fn sort_deduplicate(values: &mut Vec<f64>) {
    values.sort_unstable_by(f64::total_cmp);
    values.dedup_by(|left, right| *left == *right);
}

fn knot_index(knots: &[f64], coordinate: f64) -> usize {
    knots
        .binary_search_by(|knot| knot.total_cmp(&coordinate))
        .expect("every observation coordinate must be a knot")
}

fn interval(knots: &[f64], coordinate: f64) -> (usize, usize, f64) {
    if knots.len() == 1 {
        return (0, 0, 0.0);
    }
    let upper = knots.partition_point(|knot| *knot < coordinate);
    if upper == 0 {
        return (0, 0, 0.0);
    }
    if upper == knots.len() {
        let last = knots.len() - 1;
        return (last, last, 0.0);
    }
    if knots[upper] == coordinate {
        return (upper, upper, 0.0);
    }
    let lower = upper - 1;
    let fraction = (coordinate - knots[lower]) / (knots[upper] - knots[lower]);
    (lower, upper, fraction)
}

fn lerp(left: f64, right: f64, fraction: f64) -> f64 {
    left + (right - left) * fraction
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(entries: &[Entry]) -> Model {
        Model::fit(
            entries,
            Scale::Linear,
            AlsDirection::Increasing,
            LumaDirection::Increasing,
            true,
        )
        .unwrap()
    }

    #[test]
    fn consistent_observations_are_exact_anchors() {
        let entries = vec![
            Entry::new(10, 10, 20),
            Entry::new(50, 30, 50),
            Entry::new(90, 80, 90),
        ];
        let model = model(&entries);

        for entry in entries {
            assert_eq!(
                entry.brightness as f64,
                model.predict(entry.als, entry.luma)
            );
        }
    }

    #[test]
    fn incomparable_observations_remain_exact() {
        let entries = vec![Entry::new(20, 80, 70), Entry::new(80, 20, 30)];
        let model = model(&entries);

        for entry in entries {
            assert_eq!(
                entry.brightness as f64,
                model.predict(entry.als, entry.luma)
            );
        }
    }

    #[test]
    fn sparse_observations_are_interpolated() {
        let model = model(&[Entry::new(0, 0, 20), Entry::new(100, 100, 80)]);

        assert_eq!(20.0, model.predict(0, 0));
        assert_eq!(50.0, model.predict(50, 50));
        assert_eq!(80.0, model.predict(100, 100));
    }

    #[test]
    fn one_observation_is_constant_across_the_domain() {
        let model = model(&[Entry::new(50, 50, 42)]);

        assert_eq!(42.0, model.predict(0, 0));
        assert_eq!(42.0, model.predict(100, 100));
    }

    #[test]
    fn contradictory_observations_are_retained_as_a_monotonic_compromise() {
        let model = model(&[Entry::new(20, 20, 80), Entry::new(80, 80, 20)]);

        assert_eq!(50.0, model.predict(20, 20));
        assert_eq!(50.0, model.predict(80, 80));
    }

    #[test]
    fn prediction_is_globally_monotonic_in_every_direction() {
        let entries = vec![
            Entry::new(0, 14, 10),
            Entry::new(1, 61, 10),
            Entry::new(50, 40, 60),
            Entry::new(90, 18, 100),
        ];

        for als_direction in [AlsDirection::Increasing, AlsDirection::Decreasing] {
            for luma_direction in [LumaDirection::Increasing, LumaDirection::Decreasing] {
                let model =
                    Model::fit(&entries, Scale::Linear, als_direction, luma_direction, true)
                        .unwrap();
                for luma in 0..=100 {
                    let predictions = (0..=100)
                        .map(|als| model.predict(als, luma))
                        .collect::<Vec<_>>();
                    assert!(predictions.windows(2).all(|pair| match als_direction {
                        AlsDirection::Increasing => pair[0] <= pair[1] + f64::EPSILON,
                        AlsDirection::Decreasing => pair[0] + f64::EPSILON >= pair[1],
                    }));
                }
                for als in 0..=100 {
                    let predictions = (0..=100)
                        .map(|luma| model.predict(als, luma))
                        .collect::<Vec<_>>();
                    assert!(predictions.windows(2).all(|pair| match luma_direction {
                        LumaDirection::Increasing => pair[0] <= pair[1] + f64::EPSILON,
                        LumaDirection::Decreasing => pair[0] + f64::EPSILON >= pair[1],
                    }));
                }
            }
        }
    }

    #[test]
    fn lux_observations_beyond_the_normal_domain_remain_exact() {
        let entries = vec![Entry::new(100, 20, 20), Entry::new(2_000_000, 20, 80)];
        let model = Model::fit(
            &entries,
            Scale::Lux,
            AlsDirection::Increasing,
            LumaDirection::Increasing,
            true,
        )
        .unwrap();

        assert_eq!(80.0, model.predict(2_000_000, 20));
    }

    #[test]
    fn ignoring_luma_reproduces_als_observations() {
        let entries = vec![Entry::new(20, 10, 20), Entry::new(80, 90, 80)];
        let model = Model::fit(
            &entries,
            Scale::Linear,
            AlsDirection::Increasing,
            LumaDirection::Decreasing,
            false,
        )
        .unwrap();

        assert_eq!(20.0, model.predict(20, 0));
        assert_eq!(20.0, model.predict(20, 100));
        assert_eq!(80.0, model.predict(80, 50));
    }
}
