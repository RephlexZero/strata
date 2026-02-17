//! # Session Management
//!
//! Handles the Strata session lifecycle: handshake, keepalive, teardown,
//! and link membership changes. The session state machine is:
//!
//! ```text
//!   Idle ──Hello──▶ Connecting ──Accept──▶ Established ──Teardown──▶ Closed
//!                      │                       │
//!                    Timeout                LinkJoin/LinkLeave
//! ```

use std::collections::HashMap;
use std::time::Duration;
use quanta::Instant;

use crate::wire::{PingPacket, PongPacket, SessionAction, SessionPacket};

// ─── Session State ──────────────────────────────────────────────────────────

/// Session lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Not connected.
    Idle,
    /// Hello sent, waiting for Accept.
    Connecting,
    /// Session fully established.
    Established,
    /// Graceful teardown in progress.
    Closing,
    /// Session terminated.
    Closed,
}

// ─── Link Info ──────────────────────────────────────────────────────────────

/// Information about a link within the session.
#[derive(Debug, Clone)]
pub struct LinkInfo {
    /// Link identifier (0-255).
    pub link_id: u8,
    /// When this link was added.
    pub joined_at: Instant,
    /// Whether the link is currently active.
    pub active: bool,
}

// ─── Session ────────────────────────────────────────────────────────────────

/// A Strata transport session.
pub struct Session {
    /// Unique session identifier.
    pub session_id: u64,
    /// Current state.
    pub state: SessionState,
    /// Active links in this session.
    pub links: HashMap<u8, LinkInfo>,
    /// When the session was created.
    pub created_at: Instant,
    /// When the last activity occurred.
    pub last_activity: Instant,
    /// Handshake timeout.
    pub handshake_timeout: Duration,
    /// Keepalive interval.
    pub keepalive_interval: Duration,
    /// Session inactivity timeout.
    pub inactivity_timeout: Duration,
}

impl Session {
    /// Create a new session in Idle state.
    pub fn new(session_id: u64) -> Self {
        let now = Instant::now();
        Session {
            session_id,
            state: SessionState::Idle,
            links: HashMap::new(),
            created_at: now,
            last_activity: now,
            handshake_timeout: Duration::from_secs(5),
            keepalive_interval: Duration::from_secs(1),
            inactivity_timeout: Duration::from_secs(10),
        }
    }

    /// Generate a Hello packet to initiate the session.
    pub fn make_hello(&mut self) -> SessionPacket {
        self.state = SessionState::Connecting;
        self.last_activity = Instant::now();
        SessionPacket {
            action: SessionAction::Hello,
            session_id: self.session_id,
            link_id: None,
        }
    }

    /// Generate an Accept packet (server side).
    pub fn make_accept(&mut self) -> SessionPacket {
        self.state = SessionState::Established;
        self.last_activity = Instant::now();
        SessionPacket {
            action: SessionAction::Accept,
            session_id: self.session_id,
            link_id: None,
        }
    }

    /// Generate a Teardown packet.
    pub fn make_teardown(&mut self) -> SessionPacket {
        self.state = SessionState::Closing;
        SessionPacket {
            action: SessionAction::Teardown,
            session_id: self.session_id,
            link_id: None,
        }
    }

    /// Generate a LinkJoin notification.
    pub fn make_link_join(&mut self, link_id: u8) -> SessionPacket {
        self.links.insert(
            link_id,
            LinkInfo {
                link_id,
                joined_at: Instant::now(),
                active: true,
            },
        );
        self.last_activity = Instant::now();
        SessionPacket {
            action: SessionAction::LinkJoin,
            session_id: self.session_id,
            link_id: Some(link_id),
        }
    }

    /// Generate a LinkLeave notification.
    pub fn make_link_leave(&mut self, link_id: u8) -> SessionPacket {
        if let Some(info) = self.links.get_mut(&link_id) {
            info.active = false;
        }
        self.last_activity = Instant::now();
        SessionPacket {
            action: SessionAction::LinkLeave,
            session_id: self.session_id,
            link_id: Some(link_id),
        }
    }

    /// Process an incoming session packet.
    pub fn handle_session_packet(&mut self, pkt: &SessionPacket) -> SessionEvent {
        self.last_activity = Instant::now();

        match (&self.state, pkt.action) {
            // Server receives Hello → send Accept
            (SessionState::Idle, SessionAction::Hello) => {
                self.session_id = pkt.session_id;
                self.state = SessionState::Established;
                SessionEvent::SendAccept
            }
            // Client receives Accept → established
            (SessionState::Connecting, SessionAction::Accept) => {
                self.state = SessionState::Established;
                SessionEvent::Established
            }
            // Either side receives Teardown
            (_, SessionAction::Teardown) => {
                self.state = SessionState::Closed;
                SessionEvent::Closed
            }
            // Link join
            (SessionState::Established, SessionAction::LinkJoin) => {
                if let Some(link_id) = pkt.link_id {
                    self.links.insert(
                        link_id,
                        LinkInfo {
                            link_id,
                            joined_at: Instant::now(),
                            active: true,
                        },
                    );
                }
                SessionEvent::LinkJoined(pkt.link_id.unwrap_or(0))
            }
            // Link leave
            (SessionState::Established, SessionAction::LinkLeave) => {
                if let Some(link_id) = pkt.link_id {
                    if let Some(info) = self.links.get_mut(&link_id) {
                        info.active = false;
                    }
                }
                SessionEvent::LinkLeft(pkt.link_id.unwrap_or(0))
            }
            // Unexpected
            _ => SessionEvent::Unexpected,
        }
    }

    /// Check for timeouts. Call periodically.
    pub fn check_timeouts(&self) -> Option<SessionEvent> {
        let elapsed = self.last_activity.elapsed();
        match self.state {
            SessionState::Connecting if elapsed > self.handshake_timeout => {
                Some(SessionEvent::HandshakeTimeout)
            }
            SessionState::Established if elapsed > self.inactivity_timeout => {
                Some(SessionEvent::InactivityTimeout)
            }
            _ => None,
        }
    }

    /// Whether it's time to send a keepalive (PING).
    pub fn needs_keepalive(&self) -> bool {
        self.state == SessionState::Established
            && self.last_activity.elapsed() > self.keepalive_interval
    }

    /// Number of active links.
    pub fn active_link_count(&self) -> usize {
        self.links.values().filter(|l| l.active).count()
    }

    /// Touch activity timestamp (call after any packet exchange).
    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }
}

/// Events produced by session state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Server should send Accept.
    SendAccept,
    /// Session is fully established.
    Established,
    /// Session is closed.
    Closed,
    /// A new link joined.
    LinkJoined(u8),
    /// A link left.
    LinkLeft(u8),
    /// Handshake timed out.
    HandshakeTimeout,
    /// Inactivity timeout.
    InactivityTimeout,
    /// Unexpected packet for current state.
    Unexpected,
}

// ─── RTT Tracker ──────────────────────────────────────────────────────────

/// Per-link RTT measurement via PING/PONG.
pub struct RttTracker {
    /// Pending pings awaiting pong: ping_id → (send_time, origin_timestamp_us).
    pending: HashMap<u16, Instant>,
    /// Next ping ID to use.
    next_ping_id: u16,
    /// Smoothed RTT (SRTT) in µs.
    srtt_us: f64,
    /// RTT variation (RTTVAR) in µs.
    rttvar_us: f64,
    /// Minimum RTT observed.
    min_rtt_us: f64,
    /// Maximum RTT observed.
    max_rtt_us: f64,
    /// Number of RTT samples.
    sample_count: u64,
    /// Ping interval.
    pub ping_interval: Duration,
    /// Last time a ping was sent.
    pub last_ping_sent: Instant,
}

impl RttTracker {
    pub fn new() -> Self {
        RttTracker {
            pending: HashMap::new(),
            next_ping_id: 0,
            srtt_us: 0.0,
            rttvar_us: 0.0,
            min_rtt_us: f64::MAX,
            max_rtt_us: 0.0,
            sample_count: 0,
            ping_interval: Duration::from_millis(100),
            last_ping_sent: Instant::now(),
        }
    }

    /// Generate a PING packet and record the send time.
    pub fn make_ping(&mut self, timestamp_us: u32) -> PingPacket {
        let ping_id = self.next_ping_id;
        self.next_ping_id = self.next_ping_id.wrapping_add(1);
        self.pending.insert(ping_id, Instant::now());
        self.last_ping_sent = Instant::now();
        PingPacket {
            origin_timestamp_us: timestamp_us,
            ping_id,
        }
    }

    /// Generate a PONG response to a received PING.
    pub fn make_pong(ping: &PingPacket, receive_timestamp_us: u32) -> PongPacket {
        PongPacket {
            origin_timestamp_us: ping.origin_timestamp_us,
            ping_id: ping.ping_id,
            receive_timestamp_us,
        }
    }

    /// Process a received PONG and update RTT estimates.
    /// Returns the measured RTT in µs, or None if the ping_id is unknown.
    pub fn handle_pong(&mut self, pong: &PongPacket) -> Option<f64> {
        let send_time = self.pending.remove(&pong.ping_id)?;
        let rtt = send_time.elapsed();
        let rtt_us = rtt.as_micros() as f64;

        self.sample_count += 1;

        if rtt_us < self.min_rtt_us {
            self.min_rtt_us = rtt_us;
        }
        if rtt_us > self.max_rtt_us {
            self.max_rtt_us = rtt_us;
        }

        // RFC 6298 SRTT/RTTVAR update
        if self.sample_count == 1 {
            self.srtt_us = rtt_us;
            self.rttvar_us = rtt_us / 2.0;
        } else {
            // α = 1/8, β = 1/4
            self.rttvar_us = 0.75 * self.rttvar_us + 0.25 * (self.srtt_us - rtt_us).abs();
            self.srtt_us = 0.875 * self.srtt_us + 0.125 * rtt_us;
        }

        // Cleanup stale pending pings (older than 5 seconds)
        let cutoff = Instant::now() - Duration::from_secs(5);
        self.pending.retain(|_, t| *t > cutoff);

        Some(rtt_us)
    }

    /// Whether it's time to send a new PING.
    pub fn needs_ping(&self) -> bool {
        self.last_ping_sent.elapsed() >= self.ping_interval
    }

    /// Get smoothed RTT in µs.
    pub fn srtt_us(&self) -> f64 {
        self.srtt_us
    }

    /// Get RTT variation in µs.
    pub fn rttvar_us(&self) -> f64 {
        self.rttvar_us
    }

    /// Get minimum RTT in µs.
    pub fn min_rtt_us(&self) -> f64 {
        self.min_rtt_us
    }

    /// Get RTO (retransmission timeout) in µs. RFC 6298: RTO = SRTT + 4*RTTVAR.
    pub fn rto_us(&self) -> f64 {
        let rto = self.srtt_us + 4.0 * self.rttvar_us;
        // Minimum RTO of 1ms, maximum 60s
        rto.clamp(1_000.0, 60_000_000.0)
    }

    /// Number of RTT samples collected.
    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }
}

impl Default for RttTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::SessionAction;

    #[test]
    fn session_handshake_flow() {
        let mut client = Session::new(0xCAFE);
        let mut server = Session::new(0);

        // Client sends Hello
        let hello = client.make_hello();
        assert_eq!(client.state, SessionState::Connecting);
        assert_eq!(hello.action, SessionAction::Hello);

        // Server receives Hello
        let event = server.handle_session_packet(&hello);
        assert_eq!(event, SessionEvent::SendAccept);
        assert_eq!(server.state, SessionState::Established);
        assert_eq!(server.session_id, 0xCAFE);

        // Client receives Accept
        let accept = server.make_accept();
        let event = client.handle_session_packet(&accept);
        assert_eq!(event, SessionEvent::Established);
        assert_eq!(client.state, SessionState::Established);
    }

    #[test]
    fn session_link_management() {
        let mut session = Session::new(42);
        session.state = SessionState::Established;

        let join = session.make_link_join(1);
        assert_eq!(join.link_id, Some(1));
        assert_eq!(session.active_link_count(), 1);

        let _join2 = session.make_link_join(2);
        assert_eq!(session.active_link_count(), 2);

        let _leave = session.make_link_leave(1);
        assert_eq!(session.active_link_count(), 1);
    }

    #[test]
    fn session_teardown() {
        let mut session = Session::new(42);
        session.state = SessionState::Established;
        let td = session.make_teardown();
        assert_eq!(td.action, SessionAction::Teardown);
        assert_eq!(session.state, SessionState::Closing);
    }

    #[test]
    fn rtt_tracker_basic() {
        let mut tracker = RttTracker::new();
        let ping = tracker.make_ping(1000);
        assert_eq!(ping.ping_id, 0);

        // Simulate some delay
        std::thread::sleep(std::time::Duration::from_millis(1));

        let pong = RttTracker::make_pong(&ping, 1050);
        let rtt = tracker.handle_pong(&pong).unwrap();
        assert!(rtt > 0.0);
        assert!(tracker.srtt_us() > 0.0);
        assert!(tracker.sample_count() == 1);
    }

    #[test]
    fn rtt_tracker_multiple_samples() {
        let mut tracker = RttTracker::new();

        for i in 0..5 {
            let ping = tracker.make_ping(i * 100);
            std::thread::sleep(std::time::Duration::from_millis(1));
            let pong = RttTracker::make_pong(&ping, i * 100 + 50);
            tracker.handle_pong(&pong);
        }

        assert_eq!(tracker.sample_count(), 5);
        assert!(tracker.srtt_us() > 0.0);
        assert!(tracker.rttvar_us() >= 0.0);
        assert!(tracker.rto_us() >= 1_000.0); // at least 1ms
    }

    #[test]
    fn rtt_tracker_unknown_pong_ignored() {
        let mut tracker = RttTracker::new();
        let pong = PongPacket {
            origin_timestamp_us: 0,
            ping_id: 999,
            receive_timestamp_us: 0,
        };
        assert!(tracker.handle_pong(&pong).is_none());
    }
}
