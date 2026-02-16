//! # Thompson Sampling — Link Selection Bandit
//!
//! Contextual bandit approach for choosing which link to prefer. Each link
//! maintains a Beta(α, β) distribution modeling its "reward" (successful
//! delivery within latency budget).
//!
//! Thompson Sampling handles delayed feedback better than LinUCB and naturally
//! explores under-sampled links.

use rand::Rng;
use rand::RngExt;
use std::collections::HashMap;

/// Parameters for a Beta distribution.
#[derive(Debug, Clone)]
pub struct BetaParams {
    /// Success count (α).
    pub alpha: f64,
    /// Failure count (β).
    pub beta: f64,
}

impl BetaParams {
    /// Uninformative prior: Beta(1, 1) = uniform.
    pub fn uninformative() -> Self {
        BetaParams {
            alpha: 1.0,
            beta: 1.0,
        }
    }

    /// Expected value E[X] = α / (α + β).
    pub fn mean(&self) -> f64 {
        self.alpha / (self.alpha + self.beta)
    }

    /// Sample from the Beta distribution using the Jöhnk algorithm.
    /// This is adequate for our use case (small α, β values).
    pub fn sample(&self, rng: &mut impl Rng) -> f64 {
        // Use gamma-based sampling: Beta(a,b) = Ga/(Ga+Gb) where Ga~Gamma(a,1), Gb~Gamma(b,1)
        let ga = gamma_sample(self.alpha, rng);
        let gb = gamma_sample(self.beta, rng);
        if ga + gb == 0.0 {
            0.5
        } else {
            ga / (ga + gb)
        }
    }

    /// Total observations.
    pub fn total(&self) -> f64 {
        self.alpha + self.beta - 2.0 // subtract prior
    }
}

/// Simple Gamma(shape, 1) sampler using Marsaglia and Tsang's method.
fn gamma_sample(shape: f64, rng: &mut impl Rng) -> f64 {
    if shape < 1.0 {
        // Boost: Gamma(a) = Gamma(a+1) * U^(1/a)
        let u: f64 = rng.random();
        return gamma_sample(shape + 1.0, rng) * u.powf(1.0 / shape);
    }

    let d = shape - 1.0 / 3.0;
    let c = 1.0 / (9.0 * d).sqrt();

    loop {
        let x: f64 = standard_normal(rng);
        let v = (1.0 + c * x).powi(3);
        if v <= 0.0 {
            continue;
        }
        let u: f64 = rng.random();
        if u < 1.0 - 0.0331 * x.powi(4) {
            return d * v;
        }
        if u.ln() < 0.5 * x * x + d * (1.0 - v + v.ln()) {
            return d * v;
        }
    }
}

/// Box-Muller standard normal.
fn standard_normal(rng: &mut impl Rng) -> f64 {
    let u1: f64 = rng.random();
    let u2: f64 = rng.random();
    (-2.0_f64 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

/// Thompson Sampling link selector.
pub struct ThompsonSelector {
    /// Per-link Beta distribution parameters.
    links: HashMap<usize, BetaParams>,
}

impl ThompsonSelector {
    pub fn new() -> Self {
        ThompsonSelector {
            links: HashMap::new(),
        }
    }

    /// Register a new link with uninformative prior Beta(1,1).
    pub fn add_link(&mut self, link_id: usize) {
        self.links.insert(link_id, BetaParams::uninformative());
    }

    /// Remove a link.
    pub fn remove_link(&mut self, link_id: usize) {
        self.links.remove(&link_id);
    }

    /// Reset a link to uninformative prior.
    pub fn reset_link(&mut self, link_id: usize) {
        if let Some(params) = self.links.get_mut(&link_id) {
            *params = BetaParams::uninformative();
        }
    }

    /// Select the best link by Thompson Sampling.
    ///
    /// Samples from each link's Beta distribution and picks the one with
    /// the highest sample value.
    pub fn select(&self, rng: &mut impl Rng) -> Option<usize> {
        self.links
            .iter()
            .map(|(&id, params)| (id, params.sample(rng)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(id, _)| id)
    }

    /// Select from a subset of available link IDs.
    pub fn select_from(&self, available: &[usize], rng: &mut impl Rng) -> Option<usize> {
        available
            .iter()
            .filter_map(|&id| self.links.get(&id).map(|p| (id, p.sample(rng))))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(id, _)| id)
    }

    /// Record a success for a link (packet delivered within latency budget).
    pub fn record_success(&mut self, link_id: usize) {
        if let Some(params) = self.links.get_mut(&link_id) {
            params.alpha += 1.0;
        }
    }

    /// Record a failure for a link (packet lost or late).
    pub fn record_failure(&mut self, link_id: usize) {
        if let Some(params) = self.links.get_mut(&link_id) {
            params.beta += 1.0;
        }
    }

    /// Get the estimated success probability for a link.
    pub fn estimated_success_rate(&self, link_id: usize) -> Option<f64> {
        self.links.get(&link_id).map(|p| p.mean())
    }

    /// Number of links registered.
    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    /// Get the Beta params for a link (for inspection/debugging).
    pub fn params(&self, link_id: usize) -> Option<&BetaParams> {
        self.links.get(&link_id)
    }
}

impl Default for ThompsonSelector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::SeedableRng;

    fn seeded_rng() -> StdRng {
        StdRng::seed_from_u64(42)
    }

    // ─── Basic Selection ────────────────────────────────────────────────

    #[test]
    fn single_link_always_selected() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);
        let mut rng = seeded_rng();

        for _ in 0..10 {
            assert_eq!(selector.select(&mut rng), Some(0));
        }
    }

    #[test]
    fn no_links_returns_none() {
        let selector = ThompsonSelector::new();
        let mut rng = seeded_rng();
        assert_eq!(selector.select(&mut rng), None);
    }

    // ─── Reward Learning ────────────────────────────────────────────────

    #[test]
    fn successful_link_preferred_over_time() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);
        selector.add_link(1);

        // Link 0: 90% success rate
        for _ in 0..90 {
            selector.record_success(0);
        }
        for _ in 0..10 {
            selector.record_failure(0);
        }

        // Link 1: 30% success rate
        for _ in 0..30 {
            selector.record_success(1);
        }
        for _ in 0..70 {
            selector.record_failure(1);
        }

        // Sample 1000 times and count selections
        let mut rng = seeded_rng();
        let mut counts = [0u32; 2];
        for _ in 0..1000 {
            match selector.select(&mut rng) {
                Some(0) => counts[0] += 1,
                Some(1) => counts[1] += 1,
                _ => {}
            }
        }

        assert!(
            counts[0] > counts[1] * 2,
            "link 0 (90% success) should be selected much more often: {:?}",
            counts
        );
    }

    #[test]
    fn uninformative_prior_explores_both() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);
        selector.add_link(1);

        let mut rng = seeded_rng();
        let mut counts = [0u32; 2];
        for _ in 0..1000 {
            match selector.select(&mut rng) {
                Some(0) => counts[0] += 1,
                Some(1) => counts[1] += 1,
                _ => {}
            }
        }

        // With uninformative priors, both should be explored roughly equally
        let ratio = counts[0] as f64 / counts[1] as f64;
        assert!(
            (0.5..2.0).contains(&ratio),
            "uninformative prior should explore roughly equally: {:?}",
            counts
        );
    }

    // ─── Estimated Success Rate ─────────────────────────────────────────

    #[test]
    fn estimated_success_rate_correctness() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);

        // Beta(1,1) → mean 0.5
        assert!((selector.estimated_success_rate(0).unwrap() - 0.5).abs() < 0.01);

        // 8 successes, 2 failures → Beta(9, 3) → mean ~0.75
        for _ in 0..8 {
            selector.record_success(0);
        }
        for _ in 0..2 {
            selector.record_failure(0);
        }
        let rate = selector.estimated_success_rate(0).unwrap();
        assert!((rate - 0.75).abs() < 0.05, "expected ~0.75, got {rate}");
    }

    // ─── Reset ──────────────────────────────────────────────────────────

    #[test]
    fn reset_link_clears_history() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);
        for _ in 0..100 {
            selector.record_success(0);
        }

        selector.reset_link(0);
        let rate = selector.estimated_success_rate(0).unwrap();
        assert!(
            (rate - 0.5).abs() < 0.01,
            "reset should return to uninformative prior"
        );
    }

    // ─── Select From Subset ─────────────────────────────────────────────

    #[test]
    fn select_from_subset() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);
        selector.add_link(1);
        selector.add_link(2);

        let mut rng = seeded_rng();
        let available = vec![1, 2]; // Exclude link 0

        for _ in 0..100 {
            let selected = selector.select_from(&available, &mut rng);
            assert!(selected == Some(1) || selected == Some(2));
        }
    }

    // ─── Beta Distribution Properties ───────────────────────────────────

    #[test]
    fn beta_sample_in_range() {
        let params = BetaParams {
            alpha: 2.0,
            beta: 5.0,
        };
        let mut rng = seeded_rng();

        for _ in 0..1000 {
            let sample = params.sample(&mut rng);
            assert!(
                (0.0..=1.0).contains(&sample),
                "Beta sample out of range: {sample}"
            );
        }
    }

    #[test]
    fn beta_mean_correctness() {
        let params = BetaParams {
            alpha: 3.0,
            beta: 7.0,
        };
        assert!((params.mean() - 0.3).abs() < 0.01);
    }

    // ─── Remove Link ────────────────────────────────────────────────────

    #[test]
    fn remove_link_excludes_from_selection() {
        let mut selector = ThompsonSelector::new();
        selector.add_link(0);
        selector.add_link(1);

        selector.remove_link(0);
        let mut rng = seeded_rng();
        for _ in 0..100 {
            assert_eq!(selector.select(&mut rng), Some(1));
        }
    }
}
