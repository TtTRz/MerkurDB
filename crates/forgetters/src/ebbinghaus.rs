use chrono::{DateTime, Utc};
use merkur_core::{Forgetter, LevelAction, Memory, MemoryLevel};

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
        Self { config }
    }
}

impl Forgetter for EbbinghausForgetter {
    fn compute_weight(&self, memory: &Memory, now: DateTime<Utc>) -> f64 {
        let delta_seconds = (now - memory.accessed_at).num_seconds().max(0) as f64;
        let half_lives = delta_seconds / self.config.half_life_seconds;
        let decay = self.config.decay_factor.powf(half_lives);

        let access_bonus = 1.0
            + self.config.access_boost * f64::ln(1.0 + memory.access_count as f64) / f64::ln(2.0);

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
    fn test_weight_decay_over_time() {
        let config = EbbinghausConfig {
            decay_factor: 0.9,
            half_life_seconds: 10.0,
            ..Default::default()
        };
        let f = EbbinghausForgetter::new(config);
        let now = Utc::now();
        let mem = make_memory(now - Duration::seconds(100), 1.0, MemoryLevel::Full, 0);
        let w = f.compute_weight(&mem, now);
        assert!(
            w < 0.5,
            "weight should decay significantly after 10 half-lives, got {w}"
        );
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
    fn test_decide_downgrade_full_to_summary() {
        let config = EbbinghausConfig {
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
