use crate::impairment::ImpairmentConfig;
use rand::RngExt as _;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::time::Duration;

/// Configuration for a deterministic network impairment scenario.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub seed: u64,
    pub duration: Duration,
    pub step: Duration,
    pub links: Vec<LinkScenarioConfig>,
}

/// Per-link bounds and step sizes for scenario generation.
#[derive(Debug, Clone)]
pub struct LinkScenarioConfig {
    pub min_rate_kbit: u64,
    pub max_rate_kbit: u64,
    pub rate_step_kbit: u64,
    pub base_delay_ms: u32,
    pub delay_jitter_ms: u32,
    pub delay_step_ms: u32,
    pub max_loss_percent: f32,
    pub loss_step_percent: f32,
    /// Burst loss correlation (%).  Applied to every generated frame.
    /// Set to 0.0 for independent (Bernoulli) loss.
    pub loss_correlation: f32,
}

impl LinkScenarioConfig {
    /// Typical urban LTE uplink scenario bounds.
    ///
    /// Rate 3–10 Mbps, 18–40ms one-way delay, up to 3% loss.
    pub fn lte_urban() -> Self {
        Self {
            min_rate_kbit: 3_000,
            max_rate_kbit: 10_000,
            rate_step_kbit: 500,
            base_delay_ms: 22,
            delay_jitter_ms: 12,
            delay_step_ms: 4,
            max_loss_percent: 3.0,
            loss_step_percent: 0.5,
            loss_correlation: 25.0,
        }
    }

    /// Poor / congested LTE uplink scenario bounds.
    ///
    /// Rate 500–4000 kbit, 25–70ms delay, up to 8% loss.
    pub fn lte_poor() -> Self {
        Self {
            min_rate_kbit: 500,
            max_rate_kbit: 4_000,
            rate_step_kbit: 300,
            base_delay_ms: 30,
            delay_jitter_ms: 25,
            delay_step_ms: 6,
            max_loss_percent: 8.0,
            loss_step_percent: 1.0,
            loss_correlation: 30.0,
        }
    }

    /// Good-signal LTE uplink scenario bounds.
    ///
    /// Rate 4–8 Mbps, 15–30ms delay, up to 1.5% loss.
    pub fn lte_good() -> Self {
        Self {
            min_rate_kbit: 4_000,
            max_rate_kbit: 8_000,
            rate_step_kbit: 400,
            base_delay_ms: 18,
            delay_jitter_ms: 8,
            delay_step_ms: 3,
            max_loss_percent: 1.5,
            loss_step_percent: 0.3,
            loss_correlation: 15.0,
        }
    }

    /// 5G NSA uplink scenario bounds.
    ///
    /// Rate 15–50 Mbps, 8–20ms delay, up to 1% loss.
    pub fn fiveg_good() -> Self {
        Self {
            min_rate_kbit: 15_000,
            max_rate_kbit: 50_000,
            rate_step_kbit: 2_000,
            base_delay_ms: 12,
            delay_jitter_ms: 6,
            delay_step_ms: 2,
            max_loss_percent: 1.0,
            loss_step_percent: 0.2,
            loss_correlation: 10.0,
        }
    }
}

/// A single time-step of impairment values across all links.
#[derive(Debug, Clone)]
pub struct ScenarioFrame {
    pub t: Duration,
    pub configs: Vec<ImpairmentConfig>,
}

/// Deterministic random-walk scenario generator.
///
/// Given a seed, produces reproducible sequences of [`ScenarioFrame`]s
/// where each link's rate, delay, and loss evolve via random-walk steps
/// clamped to configured bounds.
#[derive(Debug)]
pub struct Scenario {
    cfg: ScenarioConfig,
    rng: StdRng,
    states: Vec<LinkState>,
}

#[derive(Debug, Clone)]
struct LinkState {
    rate_kbit: f64,
    delay_ms: f64,
    loss_percent: f64,
}

impl Scenario {
    pub fn new(cfg: ScenarioConfig) -> Self {
        let mut rng = StdRng::seed_from_u64(cfg.seed);
        let states = cfg
            .links
            .iter()
            .map(|link| {
                let rate_range = link.max_rate_kbit.saturating_sub(link.min_rate_kbit) as f64;
                let rate = link.min_rate_kbit as f64 + rng.random::<f64>() * rate_range;
                let delay = link.base_delay_ms as f64;
                let loss = rng.random::<f64>() * link.max_loss_percent as f64 * 0.2;
                LinkState {
                    rate_kbit: rate,
                    delay_ms: delay,
                    loss_percent: loss,
                }
            })
            .collect();

        Self { cfg, rng, states }
    }

    pub fn frames(&mut self) -> Vec<ScenarioFrame> {
        let mut frames = Vec::new();
        let total_steps =
            (self.cfg.duration.as_secs_f64() / self.cfg.step.as_secs_f64()).ceil() as u64;

        for step_idx in 0..=total_steps {
            let t = self.cfg.step.mul_f64(step_idx as f64);
            let mut configs = Vec::with_capacity(self.cfg.links.len());

            for idx in 0..self.cfg.links.len() {
                let link_cfg = self.cfg.links[idx].clone();
                let rate_delta = rand_signed(&mut self.rng, link_cfg.rate_step_kbit as f64);
                let delay_delta = rand_signed(&mut self.rng, link_cfg.delay_step_ms as f64);
                let loss_delta = rand_signed(&mut self.rng, link_cfg.loss_step_percent as f64);

                let state = &mut self.states[idx];

                state.rate_kbit = (state.rate_kbit + rate_delta)
                    .clamp(link_cfg.min_rate_kbit as f64, link_cfg.max_rate_kbit as f64);
                state.delay_ms = (state.delay_ms + delay_delta).clamp(
                    1.0,
                    (link_cfg.base_delay_ms + link_cfg.delay_jitter_ms) as f64,
                );
                state.loss_percent =
                    (state.loss_percent + loss_delta).clamp(0.0, link_cfg.max_loss_percent as f64);

                let jitter_ms = if link_cfg.delay_jitter_ms == 0 {
                    None
                } else {
                    Some(link_cfg.delay_jitter_ms)
                };

                configs.push(ImpairmentConfig {
                    rate_kbit: Some(state.rate_kbit.max(1.0) as u64),
                    delay_ms: Some(state.delay_ms.max(1.0) as u32),
                    jitter_ms,
                    delay_distribution_normal: jitter_ms.is_some(),
                    loss_percent: Some(state.loss_percent as f32),
                    loss_correlation: if link_cfg.loss_correlation > 0.0 {
                        Some(link_cfg.loss_correlation)
                    } else {
                        None
                    },
                    ..Default::default()
                });
            }

            frames.push(ScenarioFrame { t, configs });
        }

        frames
    }
}

fn rand_signed(rng: &mut StdRng, max_step: f64) -> f64 {
    if max_step <= 0.0 {
        return 0.0;
    }
    let mag = rng.random::<f64>() * max_step;
    if rng.random::<bool>() { mag } else { -mag }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_is_deterministic_for_seed() {
        let cfg = ScenarioConfig {
            seed: 42,
            duration: Duration::from_secs(5),
            step: Duration::from_secs(1),
            links: vec![
                LinkScenarioConfig {
                    min_rate_kbit: 500,
                    max_rate_kbit: 1500,
                    rate_step_kbit: 150,
                    base_delay_ms: 30,
                    delay_jitter_ms: 20,
                    delay_step_ms: 5,
                    max_loss_percent: 10.0,
                    loss_step_percent: 2.0,
                    loss_correlation: 25.0,
                },
                LinkScenarioConfig {
                    min_rate_kbit: 800,
                    max_rate_kbit: 2000,
                    rate_step_kbit: 200,
                    base_delay_ms: 20,
                    delay_jitter_ms: 10,
                    delay_step_ms: 4,
                    max_loss_percent: 5.0,
                    loss_step_percent: 1.0,
                    loss_correlation: 15.0,
                },
            ],
        };

        let mut s1 = Scenario::new(cfg.clone());
        let mut s2 = Scenario::new(cfg);

        let f1 = s1.frames();
        let f2 = s2.frames();

        assert_eq!(f1.len(), f2.len());
        for (a, b) in f1.iter().zip(f2.iter()) {
            assert_eq!(a.t, b.t);
            assert_eq!(a.configs.len(), b.configs.len());
            for (ca, cb) in a.configs.iter().zip(b.configs.iter()) {
                assert_eq!(ca.rate_kbit, cb.rate_kbit);
                assert_eq!(ca.delay_ms, cb.delay_ms);
                assert_eq!(ca.loss_percent, cb.loss_percent);
            }
        }
    }
}
