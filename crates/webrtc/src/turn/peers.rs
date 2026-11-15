//! An allocation's permissions (one per peer IP, RFC 8656 section 9) and
//! channels (one per peer address, section 12), each bounded, refreshed while
//! used and forgotten when not.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use crate::stun::is_channel_number;

/// A permission lasts five minutes (RFC 8656 section 9).
const PERMISSION_LIFETIME: Duration = Duration::from_secs(300);
/// A channel binding lasts ten minutes (RFC 8656 section 12).
const CHANNEL_LIFETIME: Duration = Duration::from_secs(600);
/// Refreshed this long before it lapses, if used since the last refresh.
const REFRESH_AHEAD: Duration = Duration::from_secs(60);
/// A peer the server refused is asked for again after this.
const REFUSED_FOR: Duration = Duration::from_secs(60);
/// Peer IPs with a permission at once. Each session uses one or two; the
/// offers' candidates add a few each, while they are tried.
pub(crate) const MAX_PERMISSIONS: usize = 128;
/// Channels at once (out of the 4096 numbers RFC 8656 allows).
pub(crate) const MAX_CHANNELS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Pending,
    Installed { until: Instant },
    Refused { until: Instant },
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    state: State,
    used: Instant,
}

#[derive(Debug, Clone, Copy)]
struct Channel {
    number: u16,
    state: State,
    used: Instant,
}

#[derive(Default)]
pub(crate) struct Peers {
    permissions: HashMap<IpAddr, Entry>,
    channels: HashMap<SocketAddr, Channel>,
    numbers: HashMap<u16, SocketAddr>,
    next_number: u16,
}

impl Peers {
    /// Notes that `ip` is wanted. `true` when a CreatePermission should be
    /// sent now (none is installed or asked for, and there is room).
    pub(crate) fn want_permission(&mut self, ip: IpAddr, now: Instant) -> bool {
        if let Some(entry) = self.permissions.get_mut(&ip) {
            entry.used = now;
            return match entry.state {
                State::Refused { until } if until <= now => {
                    entry.state = State::Pending;
                    true
                }
                _ => false,
            };
        }
        if self.permissions.len() >= MAX_PERMISSIONS {
            // The least recently used one that is not being asked for goes.
            let oldest = self
                .permissions
                .iter()
                .filter(|(_, e)| e.state != State::Pending)
                .min_by_key(|(_, e)| e.used)
                .map(|(ip, _)| *ip);
            match oldest {
                Some(old) => {
                    self.permissions.remove(&old);
                }
                None => return false,
            }
        }
        self.permissions.insert(ip, Entry { state: State::Pending, used: now });
        true
    }

    /// The server's answer to a CreatePermission.
    pub(crate) fn permission_answered(&mut self, ip: IpAddr, installed: bool, now: Instant) {
        if let Some(entry) = self.permissions.get_mut(&ip) {
            entry.state = if installed {
                State::Installed { until: now + PERMISSION_LIFETIME }
            } else {
                State::Refused { until: now + REFUSED_FOR }
            };
        }
    }

    /// The permissions to refresh now (installed, about to lapse, used since
    /// they were installed: marked asked for). Unused ones that lapsed are
    /// forgotten.
    pub(crate) fn permissions_due(&mut self, now: Instant) -> Vec<IpAddr> {
        let mut due = Vec::new();
        self.permissions.retain(|ip, entry| match entry.state {
            State::Installed { until } if until <= now + REFRESH_AHEAD => {
                if entry.used + PERMISSION_LIFETIME > until {
                    entry.state = State::Pending;
                    due.push(*ip);
                    true
                } else {
                    until > now
                }
            }
            _ => true,
        });
        due
    }

    /// The channel bound to `peer`, if the server confirmed it.
    pub(crate) fn channel(&mut self, peer: SocketAddr, now: Instant) -> Option<u16> {
        let channel = self.channels.get_mut(&peer)?;
        channel.used = now;
        matches!(channel.state, State::Installed { .. }).then_some(channel.number)
    }

    /// A number to bind to `peer`, when none is bound or asked for and there
    /// is room. Numbers are handed out in turn, not reused at once: RFC 8656
    /// forbids rebinding a number to another peer for five minutes.
    pub(crate) fn want_channel(&mut self, peer: SocketAddr, now: Instant) -> Option<u16> {
        if let Some(channel) = self.channels.get_mut(&peer) {
            channel.used = now;
            return match channel.state {
                State::Refused { until } if until <= now => {
                    channel.state = State::Pending;
                    Some(channel.number)
                }
                _ => None,
            };
        }
        if self.channels.len() >= MAX_CHANNELS {
            return None;
        }
        let number = self.free_number()?;
        self.channels.insert(peer, Channel { number, state: State::Pending, used: now });
        self.numbers.insert(number, peer);
        Some(number)
    }

    fn free_number(&mut self) -> Option<u16> {
        for _ in 0..4096 {
            let number = 0x4000 + (self.next_number % 4096);
            self.next_number = self.next_number.wrapping_add(1) % 4096;
            if is_channel_number(number) && !self.numbers.contains_key(&number) {
                return Some(number);
            }
        }
        None
    }

    /// The server's answer to a ChannelBind.
    pub(crate) fn channel_answered(&mut self, number: u16, bound: bool, now: Instant) {
        let Some(peer) = self.numbers.get(&number) else { return };
        if let Some(channel) = self.channels.get_mut(peer) {
            channel.state = if bound {
                State::Installed { until: now + CHANNEL_LIFETIME }
            } else {
                State::Refused { until: now + REFUSED_FOR }
            };
        }
        // A binding also installs (or refreshes) the peer's permission.
        if bound && let Some(entry) = self.permissions.get_mut(&peer.ip()) {
            entry.state = State::Installed { until: now + PERMISSION_LIFETIME };
        }
    }

    /// Channels to refresh now; unused ones that lapsed are unbound.
    pub(crate) fn channels_due(&mut self, now: Instant) -> Vec<(u16, SocketAddr)> {
        let mut due = Vec::new();
        let mut gone = Vec::new();
        for (peer, channel) in &mut self.channels {
            match channel.state {
                State::Installed { until } if until <= now + REFRESH_AHEAD => {
                    if channel.used + CHANNEL_LIFETIME > until {
                        channel.state = State::Pending;
                        due.push((channel.number, *peer));
                    } else if until <= now {
                        gone.push(*peer);
                    }
                }
                // Asked for again at its next use, under a new number.
                State::Refused { until } if until <= now => gone.push(*peer),
                _ => {}
            }
        }
        for peer in gone {
            if let Some(channel) = self.channels.remove(&peer) {
                self.numbers.remove(&channel.number);
            }
        }
        due
    }

    /// The peer a channel number names (bound or being bound).
    pub(crate) fn peer_of(&self, number: u16) -> Option<SocketAddr> {
        self.numbers.get(&number).copied()
    }

    /// When the next refresh or expiry is due: a refresh for an entry used
    /// since it was installed, its lapse otherwise (a use after this was
    /// computed is seen at the driver's next tick, well before the lapse).
    pub(crate) fn next_due(&self) -> Option<Instant> {
        let due = |state: State, used: Instant, lifetime: Duration| match state {
            State::Installed { until } if used + lifetime > until => {
                Some(until.checked_sub(REFRESH_AHEAD).unwrap_or(until))
            }
            State::Installed { until } | State::Refused { until } => Some(until),
            State::Pending => None,
        };
        // A refused permission is asked for again at its next use: no timer.
        let permissions = self
            .permissions
            .values()
            .filter(|e| !matches!(e.state, State::Refused { .. }))
            .filter_map(|e| due(e.state, e.used, PERMISSION_LIFETIME));
        let channels = self.channels.values().filter_map(|c| due(c.state, c.used, CHANNEL_LIFETIME));
        permissions.chain(channels).min()
    }

    #[cfg(any(test, feature = "fuzzing"))]
    pub(crate) fn counts(&self) -> (usize, usize) {
        (self.permissions.len(), self.channels.len())
    }
}

#[cfg(test)]
#[path = "peers_tests.rs"]
mod tests;
