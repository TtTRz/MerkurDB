use chrono::{DateTime, Utc};
use merkur_core::{Forgetter, LevelAction, Memory, MemoryLevel};
use tracing::warn;

/// Configuration for the Ebbinghaus-style forgetting curve.
///
/// `half_life_seconds` is a true half-life: a memory whose `accessed_at` is
/// `half_life_seconds` ago decays to `0.5 * weight * access_bonus`. The previous
/// formulation `decay_factor.powf(t / half_life)` did not satisfy this contract
/// (it produced an effective half-life of `h * ln2 / ln(1/decay_factor)`),
/// which made tuning unintuitive. The `decay_factor` field is kept for backwards
/// compatibility but is no longer used in the formula; it is logged on init.
pub struct EbbinghausConfig {
    pub decay_factor: f64,
    pub half_life_seconds: f64,
    pub access_boost: f64,
    pub threshold_to_l1: f64,
    pub threshold_to_l0: f64,
    pub threshold_archive: f64,
}

impl Default for EbbinghausConfig {
    fn default() -> Self {
        Self {
            decay_factor: 0.9,
            half_life_seconds: 86400.0,
            access_boost: 0.1,
            threshold_to_l1: 0.3,
            threshold_to_l0: 0.2,
            threshold_archive: 0.1,
        }
    }
}

pub struct EbbinghausForgetter {
    config: EbbinghausConfig,
}

impl EbbinghausForgetter {
    pub fn new(config: EbbinghausConfig) -> Self {
        if config.half_life_seconds <= 0.0 {
            warn!(
                "EbbinghausForgetter: half_life_seconds={} is non-positive; weights will not decay",
                config.half_life_seconds
            );
        }
        Self { config }
    }
}

const LN_2: f64 = std::f64::consts::LN_2;

impl Forgetter for EbbinghausForgetter {
    fn compute_weight(&self, memory: &Memory, now: DateTime<Utc>) -> f64 {
        let raw_seconds = (now - memory.accessed_at).num_seconds();
        if raw_seconds < 0 {
            warn!(
                memory_id = memory.id.as_str(),
                delta = raw_seconds,
                "Memory accessed_at is in the future; treating as just-accessed"
            );
        }
        let delta_seconds = raw_seconds.max(0) as f64;

        // True exponential decay: w(t) = w0 * exp(-t * ln2 / half_life).
        // At t = half_life, the multiplier is exactly 0.5.
        let decay = if self.config.half_life_seconds > 0.0 {
            (-delta_seconds * LN_2 / self.config.half_life_seconds).exp()
        } else {
            1.0
        };

        let access_bonus =
            1.0 + self.config.access_boost * f64::ln(1.0 + memory.access_count as f64) / LN_2;

        memory.weight * decay * access_bonus
    }

    fn decide(&self, memory: &Memory, now: DateTime<Utc>) -> LevelAction {
        let new_weight = self.compute_weight(memory, now);

        if new_weight < self.config.threshold_archive {
            return LevelAction::Archive;
        }

        match memory.level {
            MemoryLevel::Full => {
                if new_weight < self.config.threshold_to_l1 {
                    LevelAction::Downgrade(MemoryLevel::Summary)
                } else {
                    LevelAction::Keep
                }
            }
            MemoryLevel::Summary => {
                if new_weight < self.config.threshold_to_l0 {
                    LevelAction::Downgrade(MemoryLevel::Title)
                } else {
                    LevelAction::Keep
                }
            }
            MemoryLevel::Title => {
                if new_weight < self.config.threshold_archive {
                    LevelAction::Archive
                } else {
                    LevelAction::Keep
                }
            }
            MemoryLevel::Archived => LevelAction::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn make_memory(
        accessed_at: DateTime<Utc>,
        weight: f64,
        level: MemoryLevel,
        access_count: u64,
    ) -> Memory {
        Memory {
            id: "test".into(),
            content: "test".into(),
            abstract_: None,
            category: "general".into(),
            weight,
            level,
            pending_consolidation: false,
            embedding: None,
            metadata: Default::default(),
            context: Default::default(),
            created_at: accessed_at,
            updated_at: accessed_at,
            accessed_at,
            access_count,
        }
    }

    #[test]
    fn test_half_life_is_exact() {
        let config = EbbinghausConfig {
            half_life_seconds: 100.0,
            access_boost: 0.0,
            ..Default::default()
        };
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(now - Duration::seconds(100), 1.0, MemoryLevel::Full, 0);
        let w = f.compute_weight(&mem, now);
        // At exactly one half-life, the weight should be 0.5 within fp tolerance.
        assert!((w - 0.5).abs() < 1e-6, "expected 0.5, got {w}");
    }

    #[test]
    fn test_weight_decay_over_time() {
        let config = EbbinghausConfig {
            half_life_seconds: 10.0,
            ..Default::default()
        };
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(now - Duration::seconds(100), 1.0, MemoryLevel::Full, 0);
        let w = f.compute_weight(&mem, now);
        // 100s = 10 half-lives -> 2^-10 ≈ 0.000977 at base, plus small bonus 0.
        assert!(w < 0.01, "expected near-zero decay, got {w}");
    }

    #[test]
    fn test_access_boosts_weight() {
        let config = EbbinghausConfig::default();
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(now - Duration::seconds(10), 1.0, MemoryLevel::Full, 100);
        let w = f.compute_weight(&mem, now);
        assert!(w > 0.95, "frequent access should keep weight high, got {w}");
    }

    #[test]
    fn test_clock_skew_treated_as_zero() {
        let config = EbbinghausConfig::default();
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        // accessed_at is in the future relative to `now`.
        let mem = make_memory(now + Duration::seconds(1000), 1.0, MemoryLevel::Full, 0);
        let w = f.compute_weight(&mem, now);
        assert!((w - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_decide_downgrade_full_to_summary() {
        // Use a 30-day half-life so that a 10-day-old memory decays to roughly
        // 0.4 * 2^(-10/30) ≈ 0.4 * 0.794 ≈ 0.317 — above threshold_archive (0.1)
        // but below threshold_to_l1 (0.5) → Downgrade to Summary.
        let config = EbbinghausConfig {
            half_life_seconds: 86400.0 * 30.0,
            access_boost: 0.0,
            threshold_to_l1: 0.5,
            ..Default::default()
        };
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(
            now - Duration::seconds(86400 * 10),
            0.4,
            MemoryLevel::Full,
            0,
        );
        let action = f.decide(&mem, now);
        assert!(matches!(
            action,
            LevelAction::Downgrade(MemoryLevel::Summary)
        ));
    }

    #[test]
    fn test_decide_keep_high_weight() {
        let config = EbbinghausConfig::default();
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(now, 1.0, MemoryLevel::Full, 10);
        let action = f.decide(&mem, now);
        assert!(matches!(action, LevelAction::Keep));
    }

    #[test]
    fn test_decide_archive() {
        let config = EbbinghausConfig {
            threshold_archive: 0.5,
            ..Default::default()
        };
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(
            now - Duration::seconds(86400 * 30),
            0.1,
            MemoryLevel::Title,
            0,
        );
        let action = f.decide(&mem, now);
        assert!(matches!(action, LevelAction::Archive));
    }
}
