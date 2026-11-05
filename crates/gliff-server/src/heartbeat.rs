//! Client liveness. The server notices EOF and write errors on its own, but
//! not a link that goes silent, such as a client machine that sleeps or
//! loses its network with a key held. The session pings on a fixed interval,
//! and when the client stays silent past a deadline its held input is
//! released, input received from then on is dropped until a Ping sent after
//! that point is answered, and after a grace period the session ends.
//!
//! One `Heartbeat` is shared by the session's tasks, which all run on one
//! thread: the reader applies each message when it reads it and the writer
//! each Ping when its write completes, so events reach it in the order they
//! happened and the session's timer only has to check the deadline.
//!
//! Everything here is pure logic: methods take `now` so tests drive a fake
//! clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How often the session sends a Ping. Clients answer every Ping, so a
/// static screen with no input still produces a message this often.
pub const PING_EVERY: Duration = Duration::from_secs(2);
/// Silence allowed after a Ping finished writing. A completed write only
/// means the local transport (a socket, or the pipe to ssh) took it, not
/// that it reached the client, so this leaves room for a slow round trip.
const SILENCE_AFTER_WRITE: Duration = Duration::from_secs(10);
/// Silence allowed when no Ping has been written since the client was last
/// heard, because writes are blocked. A congested but alive link still
/// drains, so this only runs out on a link that has stopped moving.
const SILENCE_BLOCKED: Duration = Duration::from_secs(30);
/// How long a silent client keeps a headless session after its input is
/// released. A reconnect creates a new headless output rather than
/// reattaching the old one, so ending the session loses the windows placed
/// on it; give a sleeping laptop or a dropped network time to come back.
pub const GRACE_HEADLESS: Duration = Duration::from_secs(5 * 60);
/// The same for a mirrored output, which holds nothing the user loses when
/// the session ends, so only a short drop is worth waiting out.
pub const GRACE_MIRROR: Duration = Duration::from_secs(60);
/// Unanswered Ping tokens kept for matching; a lost client accumulates one
/// per interval for the whole grace period.
const MAX_TOKENS: usize = 256;

/// What the session must do after a deadline passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// The client just went silent past its deadline: release held input.
    Release,
    /// The grace period ran out: end the session.
    End,
}

pub struct Heartbeat {
    grace: Duration,
    /// When the latest message from the client was received.
    heard: Instant,
    /// When the first Ping after `heard` finished writing.
    written: Option<Instant>,
    /// Tokens of Pings sent and not answered, oldest first. Emptied when
    /// the client is declared lost, so only a Ping sent after that point
    /// can restore input.
    tokens: VecDeque<u64>,
    /// The newest token issued; tokens only grow, so none is reused.
    last_token: u64,
    /// When the client was declared lost; input is dropped until a Pong.
    lost_at: Option<Instant>,
    /// How many times the client was declared lost.
    losses: u64,
}

impl Heartbeat {
    pub fn new(now: Instant, grace: Duration) -> Self {
        Self {
            grace,
            heard: now,
            written: None,
            tokens: VecDeque::new(),
            last_token: 0,
            lost_at: None,
            losses: 0,
        }
    }

    /// A Ping with this token was sent.
    pub fn sent(&mut self, t: u64) {
        if self.tokens.len() == MAX_TOKENS {
            self.tokens.pop_front();
        }
        self.tokens.push_back(t);
        self.last_token = self.last_token.max(t);
    }

    /// Issue the token for a new Ping. It is the wall clock in ms, which
    /// the Pong's round-trip seed reads, bumped past the last token so a
    /// clock step back cannot make an old token match again.
    pub fn ping(&mut self, wall_ms: u64) -> u64 {
        let t = wall_ms.max(self.last_token + 1);
        self.sent(t);
        t
    }

    /// A Ping finished writing at `at`. Only the first write after the
    /// client was last heard starts the silence clock.
    pub fn written(&mut self, at: Instant) {
        if self.written.is_none() && at >= self.heard {
            self.written = Some(at);
        }
    }

    /// A message from the client was received. It restarts the silence
    /// clock, but while lost it does not restore input: input sent before a
    /// sleep can arrive late, so only a Pong for a newer Ping shows the
    /// client is current again.
    pub fn arrived(&mut self, now: Instant) {
        self.heard = now;
        self.written = None;
    }

    /// A Pong arrived. True when it answers a Ping sent since the client
    /// was declared lost, so input flows again.
    pub fn pong(&mut self, t: u64) -> bool {
        let Some(i) = self.tokens.iter().position(|&s| s == t) else {
            return false;
        };
        // The stream is ordered, so earlier Pings will not be answered.
        self.tokens.drain(..=i);
        self.lost_at.take().is_some()
    }

    pub fn is_lost(&self) -> bool {
        self.lost_at.is_some()
    }

    /// Which stretch of liveness input received now belongs to: `None`
    /// while lost, otherwise the number of losses so far. Input is only
    /// injected while this is unchanged, so input received before a loss
    /// cannot press a key again after the release, however late the
    /// session gets to it.
    pub fn input_epoch(&self) -> Option<u64> {
        (!self.is_lost()).then_some(self.losses)
    }

    /// When `check` next has something to decide.
    pub fn deadline(&self) -> Instant {
        match (self.lost_at, self.written) {
            (Some(lost), _) => lost + self.grace,
            (None, Some(written)) => written + SILENCE_AFTER_WRITE,
            (None, None) => self.heard + SILENCE_BLOCKED,
        }
    }

    pub fn check(&mut self, now: Instant) -> Option<Effect> {
        let deadline = self.deadline();
        if now < deadline {
            return None;
        }
        if self.lost_at.is_some() {
            return Some(Effect::End);
        }
        // Count the grace from when the deadline passed, which may be
        // earlier than `now` if the session loop was busy.
        self.lost_at = Some(deadline);
        self.losses += 1;
        self.tokens.clear();
        Some(Effect::Release)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn unanswered_written_ping_loses_then_ends() {
        let t0 = Instant::now();
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        hb.ping(1);
        hb.written(t0 + secs(1));
        assert_eq!(hb.check(t0 + secs(10)), None);
        assert_eq!(hb.check(t0 + secs(11)), Some(Effect::Release));
        assert!(hb.is_lost());
        assert_eq!(hb.check(t0 + secs(12)), None);
        assert_eq!(hb.deadline(), t0 + secs(11) + GRACE_MIRROR);
        assert_eq!(hb.check(t0 + secs(11) + GRACE_MIRROR), Some(Effect::End));
    }

    #[test]
    fn any_client_message_counts_as_liveness() {
        let t0 = Instant::now();
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        for n in 0..20 {
            let at = t0 + secs(n * 5);
            hb.ping(n);
            hb.written(at);
            // A frame ack or a key, never a Pong.
            hb.arrived(at + secs(4));
            assert_eq!(hb.check(at + secs(5)), None);
        }
        assert!(!hb.is_lost());
    }

    #[test]
    fn only_a_pong_for_a_ping_sent_after_the_loss_restores_input() {
        let t0 = Instant::now();
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        let old = hb.ping(100);
        let answered = hb.ping(200);
        hb.written(t0);
        assert_eq!(hb.check(t0 + secs(10)), Some(Effect::Release));
        // A message that arrives while lost does not restore input.
        hb.arrived(t0 + secs(12));
        assert!(hb.is_lost());
        // Nor does a buffered Pong for a Ping sent before the loss.
        assert!(!hb.pong(old));
        assert!(!hb.pong(answered));
        assert!(!hb.pong(99));
        assert!(hb.is_lost());
        let fresh = hb.ping(150);
        assert!(fresh > answered);
        hb.arrived(t0 + secs(15));
        assert!(hb.pong(fresh));
        assert!(!hb.is_lost());
        assert_eq!(hb.deadline(), t0 + secs(15) + SILENCE_BLOCKED);
    }

    #[test]
    fn silence_counts_from_the_write_not_the_queue() {
        let t0 = Instant::now();
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        let t = hb.ping(1);
        // The write is stuck behind a congested socket for 20 s.
        assert_eq!(hb.check(t0 + secs(20)), None);
        hb.written(t0 + secs(20));
        assert_eq!(hb.check(t0 + secs(29)), None);
        hb.arrived(t0 + secs(29));
        assert!(!hb.pong(t));
        assert_eq!(hb.check(t0 + secs(40)), None);
    }

    #[test]
    fn a_write_that_never_completes_still_loses_the_client() {
        let t0 = Instant::now();
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        hb.ping(1);
        assert_eq!(hb.check(t0 + SILENCE_BLOCKED - secs(1)), None);
        assert_eq!(hb.check(t0 + SILENCE_BLOCKED), Some(Effect::Release));
    }

    #[test]
    fn receipts_and_a_late_write_in_any_order() {
        let t0 = Instant::now();
        let w = t0 + secs(29);
        // Messages at 1 s and 2 s, then a Ping that was blocked finishes
        // writing at 29 s: the reply is due by 39 s.
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        hb.arrived(t0 + secs(1));
        hb.arrived(t0 + secs(2));
        hb.written(w);
        assert_eq!(hb.check(t0 + secs(32)), None);
        assert_eq!(hb.check(t0 + secs(38)), None);
        assert_eq!(hb.check(t0 + secs(39)), Some(Effect::Release));
        // A message and the write in the same instant, either task first:
        // nothing is due before SILENCE_AFTER_WRITE after the write.
        for write_first in [true, false] {
            let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
            hb.arrived(t0 + secs(2));
            if write_first {
                hb.written(w);
                hb.arrived(w);
            } else {
                hb.arrived(w);
                hb.written(w);
            }
            assert_eq!(hb.check(t0 + secs(38)), None, "write_first = {write_first}");
            assert!(hb.deadline() >= w + SILENCE_AFTER_WRITE);
        }
    }

    #[test]
    fn input_epochs_change_with_each_loss() {
        let t0 = Instant::now();
        let mut hb = Heartbeat::new(t0, GRACE_MIRROR);
        assert_eq!(hb.input_epoch(), Some(0));
        hb.check(t0 + SILENCE_BLOCKED);
        assert_eq!(hb.input_epoch(), None);
        let t = hb.ping(1);
        hb.arrived(t0 + secs(31));
        assert!(hb.pong(t));
        assert_eq!(hb.input_epoch(), Some(1));
    }

    #[test]
    fn grace_depends_on_the_output_kind() {
        let t0 = Instant::now();
        for grace in [GRACE_HEADLESS, GRACE_MIRROR] {
            let mut hb = Heartbeat::new(t0, grace);
            assert_eq!(hb.check(t0 + SILENCE_BLOCKED), Some(Effect::Release));
            let lost = t0 + SILENCE_BLOCKED;
            assert_eq!(hb.check(lost + grace - secs(1)), None);
            assert_eq!(hb.check(lost + grace), Some(Effect::End));
        }
        assert!(GRACE_HEADLESS >= secs(5 * 60));
    }

    #[test]
    fn tokens_stay_bounded_and_unique() {
        let mut hb = Heartbeat::new(Instant::now(), GRACE_MIRROR);
        let mut last = 0;
        for _ in 0..1000 {
            // A wall clock that stands still or steps back.
            let t = hb.ping(5);
            assert!(t > last);
            last = t;
        }
        assert_eq!(hb.tokens.len(), MAX_TOKENS);
        assert!(!hb.pong(5));
        hb.lost_at = Some(Instant::now());
        assert!(hb.pong(last));
    }
}
