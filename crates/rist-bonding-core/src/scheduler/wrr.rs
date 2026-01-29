use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub struct LinkCandidate {
    pub id: usize,
    pub capacity_bps: f64,
    pub rtt_ms: f64,
    pub queue_depth: usize,
    pub max_queue: usize,
    pub alive: bool,
}

impl LinkCandidate {
    pub fn score(&self) -> f64 {
        // score(l) = (l.capacity * 0.7) - (l.rtt * 0.3)
        // Normalize?
        // If capacity is 10Mbps (10,000,000) and RTT is 50ms (50).
        // 10,000,000 * 0.7 - 50 * 0.3 is huge.
        // The spec formula: "score(l) = (l.capacity * 0.7) - (l.rtt * 0.3)" might be symbolic or requires normalization.
        // Assuming strict adherence to formula for now, but usually one would normalize.
        // Let's assume the formula is illustrative of "Higher capacity is better, lower RTT is better".
        // But since I must implement it, I should probably handle units safely.

        // Let's use the raw formula for now but assume RTT impact needs to be significant relative to capacity?
        // Actually, if I just implement the math as written:
        (self.capacity_bps * 0.7) - (self.rtt_ms * 0.3)
    }
}

pub fn select_best_link(candidates: &[LinkCandidate]) -> Option<usize> {
    candidates
        .iter()
        .filter(|l| l.alive)
        .filter(|l| l.queue_depth < l.max_queue)
        // Find max score.
        // f64 is not Ord, so we need partial_cmp or helper.
        .max_by(|a, b| a.score().partial_cmp(&b.score()).unwrap_or(Ordering::Equal))
        .map(|l| l.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_best_link_basic() {
        let links = vec![
            LinkCandidate {
                id: 1,
                capacity_bps: 10_000_000.0,
                rtt_ms: 50.0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
            },
            LinkCandidate {
                id: 2,
                capacity_bps: 5_000_000.0,
                rtt_ms: 20.0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
            },
        ];

        // Link 1 score: 7,000,000 - 15 = 6,999,985
        // Link 2 score: 3,500,000 - 6 = 3,499,994
        // Link 1 should win.
        assert_eq!(select_best_link(&links), Some(1));
    }

    #[test]
    fn test_select_best_link_rtt_impact() {
        // If two links have very similar capacity, RTT should decide.
        // Or if capacity is low, RTT matters more?
        // With current formula, capacity dominates heavily if it is in Mbps.

        // Let's create a scenario where capacity is identical.
        let links = vec![
            LinkCandidate {
                id: 1,
                capacity_bps: 1_000_000.0,
                rtt_ms: 100.0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
            },
            LinkCandidate {
                id: 2,
                capacity_bps: 1_000_000.0,
                rtt_ms: 10.0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
            },
        ];

        // Link 1: 700000 - 30 = 699970
        // Link 2: 700000 - 3 = 699997
        // Link 2 is better.
        assert_eq!(select_best_link(&links), Some(2));
    }

    #[test]
    fn test_filter_dead_links() {
        let links = vec![
            LinkCandidate {
                id: 1,
                capacity_bps: 10_000_000.0,
                rtt_ms: 10.0,
                queue_depth: 0,
                max_queue: 100,
                alive: false, // Dead
            },
            LinkCandidate {
                id: 2,
                capacity_bps: 5_000_000.0,
                rtt_ms: 20.0,
                queue_depth: 0,
                max_queue: 100,
                alive: true,
            },
        ];

        assert_eq!(select_best_link(&links), Some(2));
    }

    #[test]
    fn test_filter_full_queue() {
        let links = vec![
            LinkCandidate {
                id: 1,
                capacity_bps: 100_000_000.0, // Huge capacity
                rtt_ms: 10.0,
                queue_depth: 100, // Full
                max_queue: 100,
                alive: true,
            },
            LinkCandidate {
                id: 2,
                capacity_bps: 5_000_000.0,
                rtt_ms: 20.0,
                queue_depth: 50,
                max_queue: 100,
                alive: true,
            },
        ];

        assert_eq!(select_best_link(&links), Some(2));
    }

    #[test]
    fn test_all_unsuitable() {
        let links = vec![LinkCandidate {
            id: 1,
            capacity_bps: 10.0,
            rtt_ms: 10.0,
            queue_depth: 100,
            max_queue: 100,
            alive: true,
        }];
        assert_eq!(select_best_link(&links), None);
    }
}
