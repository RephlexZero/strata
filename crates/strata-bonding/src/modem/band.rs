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
    /// Regions where this band is deployed.
    pub regions: &'static [Region],
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

/// Geographic region for band filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Region {
    /// United Kingdom (EE, Three, Vodafone, O2)
    UK,
    /// Europe (EU/EEA, common 3GPP allocations)
    Europe,
    /// United States (T-Mobile, AT&T, Verizon)
    US,
    /// Japan (NTT DoCoMo, KDDI, SoftBank, Rakuten)
    Japan,
    /// South Korea (SKT, KT, LGU+)
    Korea,
    /// Australia (Telstra, Optus, TPG)
    Australia,
    /// Middle East & Africa (common deployments)
    MEA,
    /// Southeast Asia (common deployments)
    SEA,
    /// Latin America (common deployments)
    LatAm,
    /// Global — present in most markets worldwide.
    Global,
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Region::UK => write!(f, "UK"),
            Region::Europe => write!(f, "EU"),
            Region::US => write!(f, "US"),
            Region::Japan => write!(f, "JP"),
            Region::Korea => write!(f, "KR"),
            Region::Australia => write!(f, "AU"),
            Region::MEA => write!(f, "MEA"),
            Region::SEA => write!(f, "SEA"),
            Region::LatAm => write!(f, "LATAM"),
            Region::Global => write!(f, "Global"),
        }
    }
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

/// Global LTE/NR band catalog.
///
/// Covers major deployments across all regions. Each band is tagged with
/// the regions where it is commonly deployed, allowing region-specific
/// filtering at runtime.
///
/// Sources: 3GPP TS 36.101 / 38.101, GSMA band plan summaries, carrier
/// deployment databases.
pub const GLOBAL_BANDS: &[Band] = &[
    // ═══════════════════════════════════════════════════════════════════
    //  COVERAGE TIER (600–900 MHz)
    //  Long range, building penetration, rural reach
    // ═══════════════════════════════════════════════════════════════════

    // B20: 800 MHz — Europe's primary low-band (EU Digital Dividend)
    Band {
        number: 20,
        nr: false,
        freq_mhz: 800,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::UK, Region::Europe, Region::MEA],
    },
    // n28: 700 MHz — EU 5G coverage layer, UK Ofcom cleared 2020
    Band {
        number: 28,
        nr: true,
        freq_mhz: 703,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[
            Region::UK,
            Region::Europe,
            Region::Australia,
            Region::LatAm,
            Region::SEA,
        ],
    },
    // B28: 700 MHz LTE — APT band, widely deployed
    Band {
        number: 28,
        nr: false,
        freq_mhz: 703,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[
            Region::UK,
            Region::Europe,
            Region::Australia,
            Region::LatAm,
            Region::SEA,
        ],
    },
    // B8: 900 MHz — global GSM refarmed to LTE
    Band {
        number: 8,
        nr: false,
        freq_mhz: 900,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[
            Region::UK,
            Region::Europe,
            Region::MEA,
            Region::SEA,
            Region::Global,
        ],
    },
    // n8: 900 MHz NR
    Band {
        number: 8,
        nr: true,
        freq_mhz: 900,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::Europe],
    },
    // B71: 600 MHz — T-Mobile US primary low-band
    Band {
        number: 71,
        nr: false,
        freq_mhz: 617,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::US],
    },
    // n71: 600 MHz NR
    Band {
        number: 71,
        nr: true,
        freq_mhz: 617,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::US],
    },
    // B12: 700 MHz — AT&T US low-band
    Band {
        number: 12,
        nr: false,
        freq_mhz: 700,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::US],
    },
    // B13: 700 MHz — Verizon US low-band
    Band {
        number: 13,
        nr: false,
        freq_mhz: 746,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::US],
    },
    // B5: 850 MHz — Americas + Korea
    Band {
        number: 5,
        nr: false,
        freq_mhz: 850,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::US, Region::Korea, Region::LatAm],
    },
    // B18: 850 MHz — Japan (KDDI)
    Band {
        number: 18,
        nr: false,
        freq_mhz: 860,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::Japan],
    },
    // B19: 850 MHz — Japan (NTT DoCoMo)
    Band {
        number: 19,
        nr: false,
        freq_mhz: 875,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::Japan],
    },
    // B26: 850 MHz extended (Sprint legacy, global)
    Band {
        number: 26,
        nr: false,
        freq_mhz: 859,
        bandwidth_mhz: 10,
        tier: BandTier::Coverage,
        regions: &[Region::Japan, Region::LatAm],
    },
    // ═══════════════════════════════════════════════════════════════════
    //  BALANCED TIER (1700–2100 MHz)
    //  Good range with moderate throughput
    // ═══════════════════════════════════════════════════════════════════

    // B3: 1800 MHz — the most widely deployed LTE band globally
    Band {
        number: 3,
        nr: false,
        freq_mhz: 1800,
        bandwidth_mhz: 20,
        tier: BandTier::Balanced,
        regions: &[
            Region::UK,
            Region::Europe,
            Region::Japan,
            Region::Korea,
            Region::Australia,
            Region::MEA,
            Region::SEA,
            Region::LatAm,
            Region::Global,
        ],
    },
    // n3: 1800 MHz NR (DSS or refarmed)
    Band {
        number: 3,
        nr: true,
        freq_mhz: 1800,
        bandwidth_mhz: 20,
        tier: BandTier::Balanced,
        regions: &[Region::UK, Region::Europe, Region::Global],
    },
    // B1: 2100 MHz — global 3G/LTE workhorse
    Band {
        number: 1,
        nr: false,
        freq_mhz: 2100,
        bandwidth_mhz: 15,
        tier: BandTier::Balanced,
        regions: &[
            Region::UK,
            Region::Europe,
            Region::Japan,
            Region::Korea,
            Region::MEA,
            Region::SEA,
            Region::Global,
        ],
    },
    // n1: 2100 MHz NR
    Band {
        number: 1,
        nr: true,
        freq_mhz: 2100,
        bandwidth_mhz: 15,
        tier: BandTier::Balanced,
        regions: &[Region::Europe, Region::Global],
    },
    // B7: 2600 MHz FDD — Europe/Asia capacity+coverage mix
    Band {
        number: 7,
        nr: false,
        freq_mhz: 2600,
        bandwidth_mhz: 20,
        tier: BandTier::Balanced,
        regions: &[Region::UK, Region::Europe, Region::LatAm, Region::SEA],
    },
    // B4/B66: AWS 1700/2100 MHz — Americas
    Band {
        number: 66,
        nr: false,
        freq_mhz: 1710,
        bandwidth_mhz: 20,
        tier: BandTier::Balanced,
        regions: &[Region::US, Region::LatAm],
    },
    // B2/B25: PCS 1900 MHz — Americas
    Band {
        number: 2,
        nr: false,
        freq_mhz: 1900,
        bandwidth_mhz: 15,
        tier: BandTier::Balanced,
        regions: &[Region::US, Region::LatAm],
    },
    // B21: 1500 MHz — Japan (NTT DoCoMo)
    Band {
        number: 21,
        nr: false,
        freq_mhz: 1500,
        bandwidth_mhz: 15,
        tier: BandTier::Balanced,
        regions: &[Region::Japan],
    },
    // ═══════════════════════════════════════════════════════════════════
    //  CAPACITY TIER (2500+ MHz)
    //  High throughput, short range, dense urban
    // ═══════════════════════════════════════════════════════════════════

    // n77: C-band (3.3–4.2 GHz) — primary global 5G mid-band
    Band {
        number: 77,
        nr: true,
        freq_mhz: 3700,
        bandwidth_mhz: 100,
        tier: BandTier::Capacity,
        regions: &[
            Region::US,
            Region::Japan,
            Region::Korea,
            Region::Australia,
            Region::SEA,
            Region::Global,
        ],
    },
    // n78: C-band (3.3–3.8 GHz) — primary European/UK 5G
    Band {
        number: 78,
        nr: true,
        freq_mhz: 3500,
        bandwidth_mhz: 100,
        tier: BandTier::Capacity,
        regions: &[
            Region::UK,
            Region::Europe,
            Region::Japan,
            Region::Korea,
            Region::Australia,
            Region::MEA,
            Region::SEA,
            Region::Global,
        ],
    },
    // B38: 2600 MHz TDD — EU/Asia
    Band {
        number: 38,
        nr: false,
        freq_mhz: 2600,
        bandwidth_mhz: 20,
        tier: BandTier::Capacity,
        regions: &[Region::Europe, Region::MEA, Region::SEA],
    },
    // B40: 2300 MHz TDD — UK (Three/O2), India, SEA
    Band {
        number: 40,
        nr: false,
        freq_mhz: 2300,
        bandwidth_mhz: 20,
        tier: BandTier::Capacity,
        regions: &[Region::UK, Region::Europe, Region::MEA, Region::SEA],
    },
    // B41/n41: 2500 MHz TDD — global
    Band {
        number: 41,
        nr: false,
        freq_mhz: 2500,
        bandwidth_mhz: 20,
        tier: BandTier::Capacity,
        regions: &[Region::US, Region::Japan, Region::Korea, Region::Global],
    },
    Band {
        number: 41,
        nr: true,
        freq_mhz: 2500,
        bandwidth_mhz: 40,
        tier: BandTier::Capacity,
        regions: &[Region::US, Region::Japan, Region::Korea, Region::Global],
    },
    // n79: 4.5 GHz — Japan (NTT DoCoMo, KDDI)
    Band {
        number: 79,
        nr: true,
        freq_mhz: 4700,
        bandwidth_mhz: 100,
        tier: BandTier::Capacity,
        regions: &[Region::Japan, Region::Korea],
    },
];

/// Convenience alias — kept for backward compatibility.
pub const US_BANDS: &[Band] = GLOBAL_BANDS;

/// Returns bands available in a specific region.
pub fn bands_for_region(region: Region) -> Vec<&'static Band> {
    GLOBAL_BANDS
        .iter()
        .filter(|b| b.regions.contains(&region) || b.regions.contains(&Region::Global))
        .collect()
}

/// Returns bands filtered by tier from a given catalog slice.
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
                    format!("AT+QNWPREFCFG=\"nr5g_band\",{}", a.band.number)
                } else {
                    format!("AT+QNWPREFCFG=\"lte_band\",{}", a.band.number)
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
    /// Available band catalog (defaults to GLOBAL_BANDS).
    pub catalog: Vec<Band>,
    /// Preferred tier order for assignment. First modem gets the first tier,
    /// second modem the second tier, etc. Wraps around if more modems than tiers.
    pub tier_priority: Vec<BandTier>,
}

impl BandManagerConfig {
    /// Create a config filtered to bands available in a specific region.
    pub fn for_region(region: Region) -> Self {
        BandManagerConfig {
            catalog: bands_for_region(region).into_iter().copied().collect(),
            tier_priority: vec![BandTier::Coverage, BandTier::Capacity, BandTier::Balanced],
        }
    }
}

impl Default for BandManagerConfig {
    fn default() -> Self {
        BandManagerConfig {
            catalog: GLOBAL_BANDS.to_vec(),
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
            plan.assignments[0].band.tier, plan.assignments[1].band.tier,
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
        let coverage = bands_in_tier(GLOBAL_BANDS, BandTier::Coverage);
        assert!(!coverage.is_empty());
        assert!(coverage.iter().all(|b| b.tier == BandTier::Coverage));

        let capacity = bands_in_tier(GLOBAL_BANDS, BandTier::Capacity);
        assert!(!capacity.is_empty());
        assert!(capacity.iter().all(|b| b.tier == BandTier::Capacity));
    }

    // ─── Region filtering tests ─────────────────────────────────────

    #[test]
    fn uk_bands_have_all_tiers() {
        let uk = bands_for_region(Region::UK);
        assert!(
            uk.len() >= 5,
            "UK should have at least 5 bands, got {}",
            uk.len()
        );

        let tiers: std::collections::HashSet<_> = uk.iter().map(|b| b.tier).collect();
        assert!(
            tiers.contains(&BandTier::Coverage),
            "UK must have coverage bands"
        );
        assert!(
            tiers.contains(&BandTier::Balanced),
            "UK must have balanced bands"
        );
        assert!(
            tiers.contains(&BandTier::Capacity),
            "UK must have capacity bands"
        );
    }

    #[test]
    fn us_bands_include_b71_b13() {
        let us = bands_for_region(Region::US);
        assert!(
            us.iter().any(|b| b.number == 71 && !b.nr),
            "US should have LTE B71"
        );
        assert!(us.iter().any(|b| b.number == 13), "US should have B13");
    }

    #[test]
    fn japan_bands_include_b18_b19() {
        let jp = bands_for_region(Region::Japan);
        assert!(jp.iter().any(|b| b.number == 18), "Japan should have B18");
        assert!(jp.iter().any(|b| b.number == 19), "Japan should have B19");
    }

    #[test]
    fn global_bands_included_in_all_regions() {
        // B8 (900 MHz) is tagged with Region::Global — it should appear
        // in every region's filtered list.
        let b8_global = GLOBAL_BANDS
            .iter()
            .find(|b| b.number == 8 && !b.nr && b.regions.contains(&Region::Global));
        assert!(b8_global.is_some(), "B8 should be tagged Global");

        for region in &[
            Region::UK,
            Region::US,
            Region::Japan,
            Region::Korea,
            Region::Australia,
        ] {
            let filtered = bands_for_region(*region);
            assert!(
                filtered.iter().any(|b| b.number == 8 && !b.nr),
                "B8 Global should appear in {:?} filtered results",
                region
            );
        }
    }

    #[test]
    fn region_config_three_modems_diverse() {
        let cfg = BandManagerConfig::for_region(Region::UK);
        let mgr = BandManager::new(cfg);
        let plan = mgr.assign(&sample_modems(3));

        assert_eq!(plan.assignments.len(), 3);
        assert!(BandManager::is_diverse(&plan));
    }

    #[test]
    fn global_catalog_larger_than_single_region() {
        assert!(
            GLOBAL_BANDS.len() >= 25,
            "Global catalog should have 25+ bands, got {}",
            GLOBAL_BANDS.len()
        );
        // Global catalog should be larger than any single region
        let uk_count = bands_for_region(Region::UK).len();
        let us_count = bands_for_region(Region::US).len();
        assert!(GLOBAL_BANDS.len() > uk_count);
        assert!(GLOBAL_BANDS.len() > us_count);
    }

    #[test]
    fn region_display_formatting() {
        assert_eq!(format!("{}", Region::UK), "UK");
        assert_eq!(format!("{}", Region::Europe), "EU");
        assert_eq!(format!("{}", Region::Japan), "JP");
        assert_eq!(format!("{}", Region::Global), "Global");
    }
}
