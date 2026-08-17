pub const NEUTRAL_TEMPERATURE: u64 = 6500;
pub const MIN_TEMPERATURE: u64 = 1000;
pub const MAX_TEMPERATURE: u64 = 25000;

pub fn gains(dim: u64, temperature: u64) -> [f64; 3] {
    let temperature = temperature.clamp(MIN_TEMPERATURE, MAX_TEMPERATURE) as f64;
    let neutral = raw_temperature(NEUTRAL_TEMPERATURE as f64);
    let temperature = raw_temperature(temperature);
    let intensity = (100_u64.saturating_sub(dim.min(100))) as f64 / 100.0;
    std::array::from_fn(|channel| (temperature[channel] / neutral[channel]).min(1.0) * intensity)
}

pub fn linear(size: usize, dim: u64, temperature: u64) -> [Vec<u16>; 3] {
    let gains = gains(dim, temperature);
    std::array::from_fn(|channel| {
        (0..size)
            .map(|index| {
                let input = if size <= 1 {
                    0.0
                } else {
                    index as f64 / (size - 1) as f64
                };
                (input * gains[channel] * u16::MAX as f64).round() as u16
            })
            .collect()
    })
}

pub fn apply(base: &[Vec<u16>; 3], dim: u64, temperature: u64) -> [Vec<u16>; 3] {
    let gains = gains(dim, temperature);
    std::array::from_fn(|channel| {
        base[channel]
            .iter()
            .map(|value| (*value as f64 * gains[channel]).round() as u16)
            .collect()
    })
}

/// Removes the channel gains already present in a LUT while retaining each
/// channel's transfer curve. This gives back a neutral base to which absolute
/// dim and temperature values can be applied without compounding them.
pub fn neutralize(base: &[Vec<u16>; 3]) -> [Vec<u16>; 3] {
    let peaks = base
        .each_ref()
        .map(|channel| channel.iter().copied().max().unwrap_or_default() as f64);
    let reference = peaks
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.total_cmp(right.1))
        .map(|(channel, _)| channel)
        .unwrap_or_default();

    std::array::from_fn(|channel| {
        let source = if peaks[channel] > 0.0 {
            channel
        } else {
            reference
        };
        let peak = peaks[source];
        if peak == 0.0 {
            return linear(base[channel].len(), 0, NEUTRAL_TEMPERATURE)[channel].clone();
        }
        base[source]
            .iter()
            .map(|value| (*value as f64 / peak * u16::MAX as f64).round() as u16)
            .collect()
    })
}

/// Estimates the absolute controls represented by a LUT. The estimate is used
/// only as the starting point of a transition; subsequent LUTs are generated
/// from a neutralized base.
pub fn estimate(base: &[Vec<u16>; 3]) -> (u64, u64) {
    let peaks = base
        .each_ref()
        .map(|channel| channel.iter().copied().max().unwrap_or_default() as f64);
    let peak = peaks.iter().copied().fold(0.0_f64, f64::max);
    if peak == 0.0 {
        return (100, NEUTRAL_TEMPERATURE);
    }

    let observed = peaks.map(|value| value / peak);
    let temperature = (MIN_TEMPERATURE..=MAX_TEMPERATURE)
        .min_by(|left, right| {
            let error = |temperature| {
                gains(0, temperature)
                    .into_iter()
                    .zip(observed)
                    .map(|(expected, actual)| (expected - actual).powi(2))
                    .sum::<f64>()
            };
            error(*left).total_cmp(&error(*right))
        })
        .unwrap_or(NEUTRAL_TEMPERATURE);
    let dim = (100.0 * (1.0 - peak / u16::MAX as f64))
        .round()
        .clamp(0.0, 100.0) as u64;
    (dim, temperature)
}

fn raw_temperature(kelvin: f64) -> [f64; 3] {
    let value = kelvin / 100.0;
    let red = if value <= 66.0 {
        255.0
    } else {
        329.698727446 * (value - 60.0).powf(-0.1332047592)
    };
    let green = if value <= 66.0 {
        99.4708025861 * value.ln() - 161.1195681661
    } else {
        288.1221695283 * (value - 60.0).powf(-0.0755148492)
    };
    let blue = if value >= 66.0 {
        255.0
    } else if value <= 19.0 {
        0.0
    } else {
        138.5177312231 * (value - 10.0).ln() - 305.0447927307
    };
    [red, green, blue].map(|channel| channel.clamp(0.0, 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_temperature_has_equal_gains() {
        assert_eq!(gains(0, NEUTRAL_TEMPERATURE), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn dim_reduces_all_channels() {
        assert_eq!(gains(25, NEUTRAL_TEMPERATURE), [0.75, 0.75, 0.75]);
    }

    #[test]
    fn warm_temperature_reduces_blue_most() {
        let gains = gains(0, 4500);
        assert_eq!(gains[0], 1.0);
        assert!(gains[1] < gains[0]);
        assert!(gains[2] < gains[1]);
    }

    #[test]
    fn neutralizes_existing_channel_gains() {
        let base = [vec![0, 100, 200], vec![0, 50, 100], vec![0, 25, 50]];
        let neutral = neutralize(&base);

        assert_eq!(neutral[0], vec![0, 32768, 65535]);
        assert_eq!(neutral[1], neutral[0]);
        assert_eq!(neutral[2], neutral[0]);
    }

    #[test]
    fn estimates_generated_controls() {
        let existing = linear(256, 20, 4500);

        assert_eq!(estimate(&existing), (20, 4500));
    }

    #[test]
    fn applying_to_neutralized_lut_does_not_compound_temperature() {
        let existing = linear(256, 0, 4500);
        let reapplied = apply(&neutralize(&existing), 0, 4500);

        for channel in 0..3 {
            for (actual, expected) in reapplied[channel].iter().zip(&existing[channel]) {
                assert!(actual.abs_diff(*expected) <= 1);
            }
        }
    }
}
