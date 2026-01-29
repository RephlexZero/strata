use std::sync::atomic::AtomicU64;
use std::sync::Mutex;
use crate::scheduler::ewma::Ewma;

pub struct EwmaStats {
    pub rtt: Ewma,
    pub bandwidth: Ewma,
    pub loss: Ewma,
    pub last_sent: u64,
    pub last_lost: u64,
    pub last_rex: u64,
}

impl Default for EwmaStats {
    fn default() -> Self {
        Self {
            rtt: Ewma::new(0.125),
            bandwidth: Ewma::new(0.125),
            loss: Ewma::new(0.125),
            last_sent: 0,
            last_lost: 0,
            last_rex: 0,
        }
    }
}

pub struct LinkStats {
    pub rtt: AtomicU64,
    pub bandwidth: AtomicU64,
    pub retransmitted: AtomicU64,
    pub sent: AtomicU64,
    pub lost: AtomicU64,
    pub smoothed_rtt_us: AtomicU64,
    pub smoothed_bw_bps: AtomicU64,
    pub smoothed_loss_permille: AtomicU64, // Stored as * 1000. 1000 = 100% loss.
    pub ewma_state: Mutex<EwmaStats>,
}

impl Default for LinkStats {
    fn default() -> Self {
        Self {
            rtt: AtomicU64::new(0),
            bandwidth: AtomicU64::new(0),
            retransmitted: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            smoothed_rtt_us: AtomicU64::new(0),
            smoothed_bw_bps: AtomicU64::new(0),
            smoothed_loss_permille: AtomicU64::new(0),
            ewma_state: Mutex::new(EwmaStats::default()),
        }
    }
}
