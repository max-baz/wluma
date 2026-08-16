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
}
