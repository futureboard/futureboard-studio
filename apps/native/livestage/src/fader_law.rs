//! Where a fader's travel puts its level.
//!
//! A console taper rather than a straight dB line: unity sits three quarters
//! up so there is room to push, the working range around it is spread out,
//! and the bottom quarter falls away quickly to silence.

use livestage_engine::{MAX_FADER_DB, MIN_FADER_DB};

/// `(position, dB)` knots, bottom to top. Linear in between.
const KNOTS: [(f32, f32); 7] = [
    (0.0, MIN_FADER_DB),
    (0.1, -60.0),
    (0.25, -40.0),
    (0.5, -20.0),
    (0.65, -10.0),
    (0.75, 0.0),
    (1.0, MAX_FADER_DB),
];

pub fn position_to_db(position: f32) -> f32 {
    let p = position.clamp(0.0, 1.0);
    for pair in KNOTS.windows(2) {
        let ((p0, d0), (p1, d1)) = (pair[0], pair[1]);
        if p <= p1 {
            return d0 + (d1 - d0) * (p - p0) / (p1 - p0);
        }
    }
    MAX_FADER_DB
}

pub fn db_to_position(db: f32) -> f32 {
    let d = db.clamp(MIN_FADER_DB, MAX_FADER_DB);
    for pair in KNOTS.windows(2) {
        let ((p0, d0), (p1, d1)) = (pair[0], pair[1]);
        if d <= d1 {
            return p0 + (p1 - p0) * (d - d0) / (d1 - d0);
        }
    }
    1.0
}

/// A fader readout: "-∞", "-12.0", "+3.5".
pub fn format_db(db: f32) -> String {
    if db <= MIN_FADER_DB + 0.05 {
        "-∞".to_string()
    } else if db > 0.05 {
        format!("+{db:.1}")
    } else {
        format!("{db:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_taper_round_trips_and_puts_unity_three_quarters_up() {
        assert_eq!(position_to_db(0.75), 0.0);
        assert_eq!(db_to_position(0.0), 0.75);
        for step in 0..=100 {
            let p = step as f32 / 100.0;
            assert!((db_to_position(position_to_db(p)) - p).abs() < 1e-4);
        }
        assert_eq!(format_db(MIN_FADER_DB), "-∞");
        assert_eq!(format_db(3.5), "+3.5");
    }
}
