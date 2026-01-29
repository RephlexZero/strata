use crate::net::interface::LinkSender;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct LinkState<L: ?Sized> {
    pub link: Arc<L>,
    pub credits: f64,
    pub last_update: Instant,
}

pub struct Dwrr<L: LinkSender + ?Sized> {
    links: HashMap<usize, LinkState<L>>,
    sorted_ids: Vec<usize>,
    current_rr_idx: usize,
}

impl<L: LinkSender + ?Sized> Dwrr<L> {
    pub fn new() -> Self {
        Self {
            links: HashMap::new(),
            sorted_ids: Vec::new(),
            current_rr_idx: 0,
        }
    }

    pub fn add_link(&mut self, link: Arc<L>) {
        let id = link.id();
        self.links.insert(id, LinkState {
            link,
            credits: 0.0,
            last_update: Instant::now(),
        });
        self.sorted_ids.push(id);
        self.sorted_ids.sort();
    }

    pub fn remove_link(&mut self, id: usize) {
        self.links.remove(&id);
        if let Some(pos) = self.sorted_ids.iter().position(|&x| x == id) {
            self.sorted_ids.remove(pos);
        }
        // Reset RR index if out of bounds
        if self.current_rr_idx >= self.sorted_ids.len() {
            self.current_rr_idx = 0;
        }
    }

    pub fn get_active_links(&self) -> Vec<(usize, crate::net::interface::LinkMetrics)> {
        self.links
            .iter()
            .map(|(id, l)| (*id, l.link.get_metrics()))
            .collect()
    }

    pub fn select_link(&mut self, packet_len: usize) -> Option<Arc<L>> {
        if self.sorted_ids.is_empty() {
             return None;
        }

        let packet_cost = packet_len as f64;
        let now = Instant::now();
        
        // 1. Update Credits
        for state in self.links.values_mut() {
            let metrics = state.link.get_metrics();
            if metrics.alive {
                let elapsed = now.duration_since(state.last_update).as_secs_f64();
                
                // Calculate Effective Capacity (Quality Aware)
                // Penalty for loss: (1.0 - loss_rate)^4 to aggressively penalize bad links.
                let loss = metrics.loss_rate.clamp(0.0, 1.0);
                let quality_factor = (1.0 - loss).powi(4);
                
                let effective_bps = metrics.capacity_bps * quality_factor;
                // Capacity is in bps (bits per sec). Convert to bytes per sec.
                let bytes_per_sec = effective_bps / 8.0;
                
                // Add credits
                state.credits += bytes_per_sec * elapsed;
                
                // Cap credits (100ms max burst based on effective rate)
                let max_credits = bytes_per_sec * 0.1; 
                if state.credits > max_credits {
                    state.credits = max_credits;
                }
            }
            state.last_update = now;
        }

        // 2. Select Link (DWRR)
        let start_idx = self.current_rr_idx;
        let count = self.sorted_ids.len();
        
        for i in 0..count {
            let idx = (start_idx + i) % count;
            let id = self.sorted_ids[idx];
            
            if let Some(state) = self.links.get_mut(&id) {
                let metrics = state.link.get_metrics();
                if !metrics.alive {
                    continue;
                }
                
                if state.credits >= packet_cost {
                    state.credits -= packet_cost;
                    self.current_rr_idx = (idx + 1) % count;
                    return Some(state.link.clone());
                }
            }
        }
        
        // Fallback: Pick link with max credits (best effort)
        let mut best_id = None;
        let mut max_creds = f64::MIN;
        
        for &id in &self.sorted_ids {
            if let Some(state) = self.links.get(&id) {
                if state.link.get_metrics().alive {
                    if state.credits > max_creds {
                        max_creds = state.credits;
                        best_id = Some(id);
                    }
                }
            }
        }
        
        if let Some(id) = best_id {
             if let Some(state) = self.links.get_mut(&id) {
                state.credits -= packet_cost; // Goes negative
                return Some(state.link.clone());
             }
        }

        None
    }
}
