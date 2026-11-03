//! Pure decisions of the power module: overheat and low-battery levels with hysteresis, and
//! remaining battery time from the measured discharge rate.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// 0 fine, 1 warn (提醒), 2 cool down (降温), 3 critical (休眠).
pub type Level = u8;

pub const LEVEL_NAMES: [&str; 4] = ["ok", "warn", "cool", "critical"];

/// Three rising thresholds: warn, cool, critical.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Thresholds {
    pub warn: f64,
    pub cool: f64,
    pub critical: f64,
}

impl Thresholds {
    pub fn as_array(&self) -> [f64; 3] {
        [self.warn, self.cool, self.critical]
    }

    pub fn shifted(&self, by: f64) -> Thresholds {
        Thresholds { warn: self.warn + by, cool: self.cool + by, critical: self.critical + by }
    }
}

/// Level for a value where higher is worse (temperature). Going up is immediate; going down
/// needs the value to fall `hysteresis` below the threshold of the current level.
pub fn rising_level(current: Level, value: f64, t: [f64; 3], hysteresis: f64) -> Level {
    let raw = t.iter().filter(|&&th| value >= th).count() as Level;
    if raw >= current {
        return raw;
    }
    let mut level = current;
    while level > raw && value < t[level as usize - 1] - hysteresis {
        level -= 1;
    }
    level
}

/// Level for a value where lower is worse (battery percent): thresholds warn > cool >
/// critical.
pub fn falling_level(current: Level, value: f64, t: [f64; 3], hysteresis: f64) -> Level {
    rising_level(current, -value, t.map(|x| -x), hysteresis)
}

/// Battery samples while discharging, for the remaining time at the real discharge rate.
#[derive(Debug, Default)]
pub struct Discharge {
    samples: VecDeque<(i64, u8)>,
}

/// Window the rate is measured over.
const WINDOW_MS: i64 = 30 * 60_000;
/// No estimate before this much time and drop.
const MIN_SPAN_MS: i64 = 3 * 60_000;

impl Discharge {
    pub fn add(&mut self, ts: i64, on_ac: Option<bool>, percent: Option<u8>) {
        let (Some(false), Some(p)) = (on_ac, percent) else {
            self.samples.clear();
            return;
        };
        if self.samples.back().is_some_and(|&(_, last)| p > last) {
            // Charged in between (or a jumpy gauge): start over.
            self.samples.clear();
        }
        self.samples.push_back((ts, p));
        while self.samples.front().is_some_and(|&(t, _)| ts - t > WINDOW_MS) {
            self.samples.pop_front();
        }
    }

    pub fn remaining_minutes(&self) -> Option<u64> {
        let (&(t0, p0), &(t1, p1)) = (self.samples.front()?, self.samples.back()?);
        let span = t1 - t0;
        if span < MIN_SPAN_MS || p0 <= p1 {
            return None;
        }
        let per_ms = f64::from(p0 - p1) / span as f64;
        Some((f64::from(p1) / per_ms / 60_000.0).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: [f64; 3] = [80.0, 85.0, 92.0];

    #[test]
    fn temperature_levels_rise_at_once_and_fall_with_hysteresis() {
        assert_eq!(rising_level(0, 50.0, T, 3.0), 0);
        assert_eq!(rising_level(0, 86.0, T, 3.0), 2);
        assert_eq!(rising_level(2, 83.0, T, 3.0), 2, "within hysteresis of 85");
        assert_eq!(rising_level(2, 81.9, T, 3.0), 1);
        assert_eq!(rising_level(2, 70.0, T, 3.0), 0);
        assert_eq!(rising_level(1, 95.0, T, 3.0), 3);
        assert_eq!(rising_level(3, 90.0, T, 3.0), 3);
        assert_eq!(rising_level(3, 88.9, T, 3.0), 2);
    }

    #[test]
    fn battery_levels_fall_with_the_percentage() {
        let b = [20.0, 12.0, 6.0];
        assert_eq!(falling_level(0, 50.0, b, 2.0), 0);
        assert_eq!(falling_level(0, 19.0, b, 2.0), 1);
        assert_eq!(falling_level(0, 5.0, b, 2.0), 3);
        assert_eq!(falling_level(1, 21.0, b, 2.0), 1, "needs 22 to clear");
        assert_eq!(falling_level(1, 23.0, b, 2.0), 0);
    }

    #[test]
    fn remaining_time_follows_the_measured_rate() {
        let mut d = Discharge::default();
        d.add(0, Some(false), Some(60));
        d.add(60_000, Some(false), Some(59));
        assert_eq!(d.remaining_minutes(), None, "too early");
        d.add(10 * 60_000, Some(false), Some(55));
        // 5 % in 10 min → 55 % lasts 110 min.
        assert_eq!(d.remaining_minutes(), Some(110));
        d.add(11 * 60_000, Some(true), Some(55));
        assert_eq!(d.remaining_minutes(), None, "plugged in");
    }
}
