pub struct Ewma {
    value: f64,
    alpha: f64,
    initialized: bool,
}

impl Ewma {
    pub fn new(alpha: f64) -> Self {
        Self {
            value: 0.0,
            alpha,
            initialized: false,
        }
    }

    pub fn update(&mut self, measurement: f64) {
        if !self.initialized {
            self.value = measurement;
            self.initialized = true;
        } else {
            self.value = self.value * (1.0 - self.alpha) + measurement * self.alpha;
        }
    }

    pub fn value(&self) -> f64 {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ewma_logic() {
        let mut ewma = Ewma::new(0.5);

        // First update should set the value
        ewma.update(10.0);
        assert!((ewma.value() - 10.0).abs() < f64::EPSILON);

        // Second update: (10 * 0.5) + (20 * 0.5) = 15
        ewma.update(20.0);
        assert!((ewma.value() - 15.0).abs() < f64::EPSILON);

        // Third update: (15 * 0.5) + (30 * 0.5) = 7.5 + 15 = 22.5
        ewma.update(30.0);
        assert!((ewma.value() - 22.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_ewma_smoothing() {
        let mut ewma = Ewma::new(0.1); // Low alpha, high smoothing (retains history)
        ewma.update(100.0);
        assert!((ewma.value() - 100.0).abs() < f64::EPSILON);

        // Sudden drop
        ewma.update(0.0);
        // value = 100 * 0.9 + 0 * 0.1 = 90
        assert!((ewma.value() - 90.0).abs() < f64::EPSILON);
    }
}
