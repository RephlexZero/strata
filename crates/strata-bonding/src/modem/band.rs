//! # Band Locking Automation
//!
//! Assigns cellular frequency bands to modems to maximise diversity and
//! prevent multiple modems from competing for the same tower sector.
//!
//! ## Strategy
//!
//! When N modems are available, the band manager pins each to a different
//! frequency tier:
//!
//! | Tier      | Freq range       | Characteristic                    |
//! |-----------|------------------|-----------------------------------|
//! | Coverage  | 600–900 MHz      | Long range, building penetration  |
//! | Balanced  | 1700–2100 MHz    | Mid-range, moderate capacity      |
//! | Capacity  | 2500–3700 MHz    | Short range, high throughput      |
//!
//! This ensures spatial/frequency diversity so that a single tower fade or
//! congestion event doesn't take out all links simultaneously.
//!
//! ## Usage
//!
//! ```no_run
//! use strata_bonding::modem::band::{BandManager, ModemInfo};
//!
//! let modems = vec![
//!     ModemInfo { index: 0, device_path: "/dev/cdc-wdm0".into(), current_band: None },
//!     ModemInfo { index: 1, device_path: "/dev/cdc-wdm1".into(), current_band: None },
//!     ModemInfo { index: 2, device_path: "/dev/cdc-wdm2".into(), current_band: None },
//! ];
//!
//! let mgr = BandManager::default();
//! let plan = mgr.assign(&modems);
//! for cmd in plan.mmcli_commands() {
//!     println!("{}", cmd);
//! }
//! ```

use std::fmt;

// ─── Band Catalog ───────────────────────────────────────────────────────────

/// LTE/NR frequency band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Band {
    /// 3GPP band number (e.g. 71, 41, 77).
    pub number: u16,
    /// Whether this is an NR (5G) band.
    pub nr: bool,
    /// Centre frequency in MHz (approximate).
    pub freq_mhz: u16,
    /// Typical channel bandwidth in MHz.
    pub bandwidth_mhz: u8,
    /// Band tier — describes the coverage/capacity trade-off.
    pub tier: BandTier,
}

/// Coverage vs capacity classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BandTier {
    /// Low-band (600–900 MHz): excellent range & penetration.
    Coverage,
    /// Mid-band (1700–2100 MHz): good balance of range and throughput.
    Balanced,
    /// High-band / mmWave (2500+ MHz): high throughput, short range.
    Capacity,
}

impl fmt::Display for BandTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BandTier::Coverage => write!(f, "coverage"),
            BandTier::Balanced => write!(f, "balanced"),
            BandTier::Capacity => write!(f, "capacity"),
        }
    }
}

impl fmt::Display for Band {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tech = if self.nr { "n" } else { "B" };
        write!(
            f,
            "{}{} ({}MHz, {})",
            tech, self.number, self.freq_mhz, self.tier
        )
    }
}

/// Common LTE/NR bands in the United States.
///
/// Covers the major carriers' primary deployments. Extend as needed for
/// other regions.
pub const US_BANDS: &[Band] = &[
    // ─── Coverage tier ─────────────────────────────────────────────
    Band {
        number: 71,
        nr: false,
        freq_mhz: 617,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
    },
    Band {
        number: 12,
        nr: false,
        freq_mhz: 700,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
    },
    Band {
        number: 13,
        nr: false,
        freq_mhz: 746,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
    },
    Band {
        number: 14,
        nr: false,
        freq_mhz: 758,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
    },
    Band {
        number: 5,
        nr: false,
        freq_mhz: 850,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
    },
    // NR low-band
    Band {
        number: 71,
        nr: true,
        freq_mhz: 617,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
    },
    // ─── Balanced tier ─────────────────────────────────────────────
    Band {
        number: 4,
        nr: false,
        freq_mhz: 1755,
        bandwidth_mhz: 10,
        tier: BandTier::Balanced,
    },
    Band {
        number: 66,
        nr: false,
        freq_mhz: 1710,
        bandwidth_mhz: 20,
        tier: BandTier::Balanced,
    },
    Band {
        number: 2,
        nr: false,
        freq_mhz: 1900,
        bandwidth_mhz: 15,
        tier: BandTier::Balanced,
    },
    Band {
        number: 25,
        nr: false,
        freq_mhz: 1900,
        bandwidth_mhz: 15,
        tier: BandTier::Balanced,
    },
    // ─── Capacity tier ─────────────────────────────────────────────
    Band {
        number: 41,
        nr: false,
        freq_mhz: 2500,
        bandwidth_mhz: 20,
        tier: BandTier::Capacity,
    },
    Band {
        number: 41,
        nr: true,
        freq_mhz: 2500,
        bandwidth_mhz: 40,
        tier: BandTier::Capacity,
    },
    Band {
        number: 77,
        nr: true,
        freq_mhz: 3700,
        bandwidth_mhz: 100,
        tier: BandTier::Capacity,
    },
    Band {
        number: 78,
        nr: true,
        freq_mhz: 3500,
        bandwidth_mhz: 100,
        tier: BandTier::Capacity,
    },
];

/// Returns bands filtered by tier.
pub fn bands_in_tier(catalog: &[Band], tier: BandTier) -> Vec<&Band> {
    catalog.iter().filter(|b| b.tier == tier).collect()
}

// ─── Modem Info ─────────────────────────────────────────────────────────────

/// Describes a modem for band assignment purposes.
#[derive(Debug, Clone)]
pub struct ModemInfo {
    /// mmcli modem index (e.g. 0, 1, 2).
    pub index: u32,
    /// Device path (e.g. "/dev/cdc-wdm0").
    pub device_path: String,
    /// Currently locked band, if known.
    pub current_band: Option<u16>,
}

// ─── Band Assignment ────────────────────────────────────────────────────────

/// A single band assignment: modem N should lock to band B.
#[derive(Debug, Clone)]
pub struct BandAssignment {
    pub modem_index: u32,
    pub band: Band,
}

impl fmt::Display for BandAssignment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "modem {} → {}", self.modem_index, self.band)
    }
}

/// Complete band plan for a set of modems.
#[derive(Debug, Clone)]
pub struct BandPlan {
    pub assignments: Vec<BandAssignment>,
}

impl BandPlan {
    /// Generate mmcli commands to apply this band plan.
    ///
    /// Each command locks one modem to a specific band using mmcli's
    /// `--set-preferred-bands` option. The caller is responsible for
    /// executing these commands (requires root/CAP_NET_ADMIN).
    pub fn mmcli_commands(&self) -> Vec<String> {
        self.assignments
            .iter()
            .map(|a| {
                let band_name = if a.band.nr {
                    format!("utran-{}", a.band.number)
                } else {
                    format!("eutran-{}", a.band.number)
                };
                format!(
                    "mmcli -m {} --set-preferred-bands={}",
                    a.modem_index, band_name
                )
            })
            .collect()
    }

    /// Generate AT commands (Quectel-style) for band locking.
    ///
    /// These are for direct serial/QMI communication with Quectel modems
    /// (RM520N, EC25, EG25, etc.).
    pub fn at_commands(&self) -> Vec<String> {
        self.assignments
            .iter()
            .map(|a| {
                if a.band.nr {
                    format!(
                        "AT+QNWPREFCFG=\"nr5g_band\",{}",
                        a.band.number
                    )
                } else {
                    format!(
                        "AT+QNWPREFCFG=\"lte_band\",{}",
                        a.band.number
                    )
                }
            })
            .collect()
    }
}

impl fmt::Display for BandPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for a in &self.assignments {
            writeln!(f, "  {}", a)?;
        }
        Ok(())
    }
}

// ─── Band Manager ───────────────────────────────────────────────────────────

/// Configuration for the band manager.
#[derive(Debug, Clone)]
pub struct BandManagerConfig {
    /// Available band catalog (defaults to US_BANDS).
    pub catalog: Vec<Band>,
    /// Preferred tier order for assignment. First modem gets the first tier,
    /// second modem the second tier, etc. Wraps around if more modems than tiers.
    pub tier_priority: Vec<BandTier>,
}

impl Default for BandManagerConfig {
    fn default() -> Self {
        BandManagerConfig {
            catalog: US_BANDS.to_vec(),
            tier_priority: vec![BandTier::Coverage, BandTier::Capacity, BandTier::Balanced],
        }
    }
}

/// Manages band assignments for a fleet of modems.
///
/// The assignment strategy maximises frequency diversity:
/// 1. Sort modems by index (deterministic ordering)
/// 2. Assign each modem to a different tier (coverage → capacity → balanced)
/// 3. Within a tier, pick the first available band
/// 4. If more modems than tiers, wrap around (two modems may share a tier but
///    get different bands within it)
pub struct BandManager {
    config: BandManagerConfig,
}

impl BandManager {
    pub fn new(config: BandManagerConfig) -> Self {
        BandManager { config }
    }

    /// Compute optimal band assignments for the given modems.
    pub fn assign(&self, modems: &[ModemInfo]) -> BandPlan {
        let mut assignments = Vec::with_capacity(modems.len());

        // Track which bands we've already assigned to avoid duplicates
        let mut used_bands: Vec<(u16, bool)> = Vec::new(); // (number, nr)

        for (i, modem) in modems.iter().enumerate() {
            // Pick a tier for this modem (round-robin through priority list)
            let tier_idx = i % self.config.tier_priority.len();
            let target_tier = self.config.tier_priority[tier_idx];

            // Find the best band in this tier that hasn't been used
            let band = self.pick_band(target_tier, &used_bands);

            if let Some(band) = band {
                used_bands.push((band.number, band.nr));
                assignments.push(BandAssignment {
                    modem_index: modem.index,
                    band,
                });
            }
            // If no band available in preferred tier, try other tiers
            else {
                let fallback = self.pick_any_unused_band(&used_bands);
                if let Some(band) = fallback {
                    used_bands.push((band.number, band.nr));
                    assignments.push(BandAssignment {
                        modem_index: modem.index,
                        band,
                    });
                }
            }
        }

        BandPlan { assignments }
    }

    /// Pick the best band in a tier that hasn't been assigned yet.
    fn pick_band(&self, tier: BandTier, used: &[(u16, bool)]) -> Option<Band> {
        self.config
            .catalog
            .iter()
            .filter(|b| b.tier == tier)
            .find(|b| !used.contains(&(b.number, b.nr)))
            .copied()
    }

    /// Pick any unused band from the catalog (fallback when preferred tier
    /// is exhausted).
    fn pick_any_unused_band(&self, used: &[(u16, bool)]) -> Option<Band> {
        // Try tiers in priority order
        for &tier in &self.config.tier_priority {
            if let Some(band) = self.pick_band(tier, used) {
                return Some(band);
            }
        }
        None
    }

    /// Check if the current band assignments provide good diversity.
    ///
    /// Returns true if all assigned modems are on different tiers
    /// (or if there are fewer modems than tiers).
    pub fn is_diverse(plan: &BandPlan) -> bool {
        if plan.assignments.len() <= 1 {
            return true;
        }
        let mut tiers: Vec<BandTier> = plan.assignments.iter().map(|a| a.band.tier).collect();
        tiers.sort();
        tiers.dedup();
        // Diverse if we have as many unique tiers as assignments (up to 3)
        tiers.len() >= plan.assignments.len().min(3)
    }
}

impl Default for BandManager {
    fn default() -> Self {
        Self::new(BandManagerConfig::default())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_modems(n: usize) -> Vec<ModemInfo> {
        (0..n)
            .map(|i| ModemInfo {
                index: i as u32,
                device_path: format!("/dev/cdc-wdm{i}"),
                current_band: None,
            })
            .collect()
    }

    #[test]
    fn single_modem_gets_coverage_band() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(1));

        assert_eq!(plan.assignments.len(), 1);
        assert_eq!(plan.assignments[0].band.tier, BandTier::Coverage);
    }

    #[test]
    fn two_modems_get_different_tiers() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(2));

        assert_eq!(plan.assignments.len(), 2);
        assert_ne!(
            plan.assignments[0].band.tier,
            plan.assignments[1].band.tier,
            "two modems should be on different tiers"
        );
        assert!(BandManager::is_diverse(&plan));
    }

    #[test]
    fn three_modems_use_all_tiers() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(3));

        assert_eq!(plan.assignments.len(), 3);

        let mut tiers: Vec<_> = plan.assignments.iter().map(|a| a.band.tier).collect();
        tiers.sort();
        assert_eq!(
            tiers,
            vec![BandTier::Coverage, BandTier::Balanced, BandTier::Capacity]
        );
        assert!(BandManager::is_diverse(&plan));
    }

    #[test]
    fn four_modems_wraps_tiers() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(4));

        assert_eq!(plan.assignments.len(), 4);
        // Fourth modem wraps back to coverage (but different band)
        assert_eq!(plan.assignments[3].band.tier, BandTier::Coverage);
        // Ensure no duplicate bands
        let bands: Vec<_> = plan
            .assignments
            .iter()
            .map(|a| (a.band.number, a.band.nr))
            .collect();
        let unique: std::collections::HashSet<_> = bands.iter().collect();
        assert_eq!(unique.len(), bands.len(), "all bands should be unique");
    }

    #[test]
    fn mmcli_commands_generated() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(2));
        let cmds = plan.mmcli_commands();

        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].starts_with("mmcli -m "));
        assert!(cmds[0].contains("--set-preferred-bands="));
    }

    #[test]
    fn at_commands_generated() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(2));
        let cmds = plan.at_commands();

        assert_eq!(cmds.len(), 2);
        for cmd in &cmds {
            assert!(
                cmd.starts_with("AT+QNWPREFCFG="),
                "AT command should start with QNWPREFCFG: {}",
                cmd
            );
        }
    }

    #[test]
    fn display_formatting() {
        let mgr = BandManager::default();
        let plan = mgr.assign(&sample_modems(3));

        let display = format!("{plan}");
        assert!(display.contains("modem 0"));
        assert!(display.contains("modem 1"));
        assert!(display.contains("modem 2"));
    }

    #[test]
    fn diversity_check_single_modem() {
        let plan = BandPlan {
            assignments: vec![BandAssignment {
                modem_index: 0,
                band: US_BANDS[0],
            }],
        };
        assert!(BandManager::is_diverse(&plan));
    }

    #[test]
    fn diversity_check_same_tier() {
        let plan = BandPlan {
            assignments: vec![
                BandAssignment {
                    modem_index: 0,
                    band: US_BANDS[0], // Coverage
                },
                BandAssignment {
                    modem_index: 1,
                    band: US_BANDS[1], // Also Coverage
                },
            ],
        };
        assert!(
            !BandManager::is_diverse(&plan),
            "same-tier assignments should not be considered diverse"
        );
    }

    #[test]
    fn bands_in_tier_filter() {
        let coverage = bands_in_tier(US_BANDS, BandTier::Coverage);
        assert!(!coverage.is_empty());
        assert!(coverage.iter().all(|b| b.tier == BandTier::Coverage));

        let capacity = bands_in_tier(US_BANDS, BandTier::Capacity);
        assert!(!capacity.is_empty());
        assert!(capacity.iter().all(|b| b.tier == BandTier::Capacity));
    }
}
