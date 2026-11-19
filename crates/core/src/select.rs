//! Which sources a consumer reads. For each media it wants, the candidates
//! are the stream's sources in order, an alias ([`crate::local`]) standing
//! for the sources of the stream it names, the wish narrowed by the alias's
//! query; the first candidate that offers an accepted codec is taken.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::time::Instant;

use crate::error::SubscribeError;
use crate::local::Alias;
use crate::media::MediaKind;
use crate::slot::{DemandGuard, Offer, Slot};
use crate::stream::{Source, Stream};
use crate::want::{MediaWant, Want};

/// Aliases followed one after the other, at most: a deeper one is passed
/// over. Hub::upsert refuses a cycle; this bounds the walk anyway.
pub const MAX_ALIAS_DEPTH: usize = 8;
/// Candidates for one media, at most (aliases that each name a stream of
/// many aliases multiply); the rest is passed over.
pub const MAX_CANDIDATES: usize = 64;
/// Streams entered through aliases while resolving one media, at most, over
/// every path; the rest is passed over.
pub const MAX_ALIAS_VISITS: usize = 256;

/// What a pick takes from its source: each media, with the wish it meets.
pub(crate) type Kinds = Vec<(MediaKind, MediaWant)>;

/// A source that may serve one media, with the wish it must meet there.
pub(crate) struct Candidate {
    pub(crate) slot: Arc<Slot>,
    pub(crate) wish: MediaWant,
    /// The streams the aliases on the way named, nearest first.
    pub(crate) via: Vec<Arc<Stream>>,
}

/// The sources a subscription reads, with what it takes from each.
pub(crate) struct Pick {
    pub(crate) slot: Arc<Slot>,
    pub(crate) kinds: Kinds,
    pub(crate) guard: DemandGuard,
    /// The source's failed attempts when it was picked (or started to be
    /// probed): later ones are this subscription's.
    pub(crate) failures_before: u64,
    /// The streams it was reached through: one of them replaced, the pick
    /// is made again.
    pub(crate) via: Vec<Arc<Stream>>,
}

/// The candidates of `stream` for `kind`.
pub(crate) fn candidates(stream: &Stream, kind: MediaKind, wish: &MediaWant) -> Vec<Candidate> {
    let mut walk = Walk::new(kind);
    walk.expand(stream, &mut Vec::new(), wish);
    walk.out
}

/// The candidates of what `alias`, a source of `stream`, passes for `kind`.
fn alias_candidates(stream: &Stream, alias: &Alias, kind: MediaKind) -> Vec<Candidate> {
    let mut walk = Walk::new(kind);
    let wish = alias.want().get(kind);
    if let Some(target) = stream.hub().and_then(|hub| hub.get(alias.target()))
        && wish.is_wanted()
    {
        walk.expand(&target, &mut vec![target.clone()], wish);
    }
    walk.out
}

/// One media's resolution. A stream reached again with the same wish adds
/// nothing (the same sources, with the same wish, already listed), so it is
/// entered again only from nearer the top, where the depth bound passes over
/// fewer of its aliases: aliases fanning out onto the same streams cost each
/// stream once, not once per path (16 aliases in a row onto 8 streams in a
/// row were 16^8 entries, never stopped by [`MAX_CANDIDATES`] when none of
/// them has a source). [`MAX_ALIAS_VISITS`] bounds what remains.
struct Walk {
    kind: MediaKind,
    out: Vec<Candidate>,
    /// The streams entered, by name and wish, with the fewest aliases in
    /// front of them yet.
    entered: HashMap<(String, MediaWant), usize>,
    visits: usize,
}

impl Walk {
    fn new(kind: MediaKind) -> Self {
        Self { kind, out: Vec::new(), entered: HashMap::new(), visits: 0 }
    }

    /// Appends the candidates of `stream`, reached through the aliases that
    /// named the streams of `via`.
    fn expand(&mut self, stream: &Stream, via: &mut Vec<Arc<Stream>>, wish: &MediaWant) {
        for source in stream.sources() {
            if self.out.len() >= MAX_CANDIDATES {
                tracing::warn!(
                    stream = stream.name(),
                    "more than {MAX_CANDIDATES} candidate sources; the rest is passed over"
                );
                return;
            }
            match source {
                Source::Slot(slot) => {
                    if !self.out.iter().any(|c| Arc::ptr_eq(&c.slot, slot) && c.wish == *wish) {
                        self.out.push(Candidate { slot: slot.clone(), wish: wish.clone(), via: via.clone() });
                    }
                }
                Source::Alias { alias, .. } => {
                    let narrowed = wish.narrow(alias.want().get(self.kind));
                    if !narrowed.is_wanted() {
                        continue;
                    }
                    let Some(target) = stream.hub().and_then(|hub| hub.get(alias.target())) else { continue };
                    if via.len() >= MAX_ALIAS_DEPTH {
                        tracing::warn!(
                            stream = stream.name(),
                            target = alias.target(),
                            "more than {MAX_ALIAS_DEPTH} aliases in a row; this one is passed over"
                        );
                        continue;
                    }
                    let depth = via.len() + 1;
                    let nearest =
                        self.entered.entry((alias.target().to_owned(), narrowed.clone())).or_insert(usize::MAX);
                    if *nearest <= depth {
                        continue;
                    }
                    *nearest = depth;
                    if self.visits >= MAX_ALIAS_VISITS {
                        tracing::warn!(
                            stream = stream.name(),
                            "more than {MAX_ALIAS_VISITS} streams entered through aliases; the rest is passed over"
                        );
                        return;
                    }
                    self.visits += 1;
                    via.push(target.clone());
                    self.expand(&target, via, &narrowed);
                    via.pop();
                }
            }
        }
    }
}

/// What a stream's listing shows for `alias`: the source it reads (the first
/// that has tracks it passes, else the first running, else the first), with
/// the wish of each media it passes there. `Err` says why there is none.
pub(crate) fn alias_view(stream: &Stream, alias: &Alias) -> Result<(Arc<Slot>, Kinds), String> {
    if stream.hub().and_then(|hub| hub.get(alias.target())).is_none() {
        return Err(format!("no stream named {}", alias.target()));
    }
    let lists: Vec<(MediaKind, Vec<Candidate>)> =
        MediaKind::ALL.into_iter().map(|kind| (kind, alias_candidates(stream, alias, kind))).collect();
    let all = || lists.iter().flat_map(|(kind, list)| list.iter().map(move |c| (*kind, c)));
    let shown = all()
        .find(|(kind, c)| c.slot.offers(*kind, &c.wish) == Offer::Yes && c.slot.channel.tracks().is_some())
        .or_else(|| all().find(|(_, c)| c.slot.demand() > 0))
        .or_else(|| all().next())
        .map(|(_, c)| c.slot.clone())
        .ok_or_else(|| format!("{} has no source this alias passes", alias.target()))?;
    let kinds = all().filter(|(_, c)| Arc::ptr_eq(&c.slot, &shown)).map(|(kind, c)| (kind, c.wish.clone())).collect();
    Ok((shown, kinds))
}

fn key(slot: &Arc<Slot>) -> usize {
    Arc::as_ptr(slot) as usize
}

/// The sources `want` reads in `stream`, or why there are none.
pub(crate) async fn select(stream: &Arc<Stream>, want: &Want, deadline: Instant) -> Result<Vec<Pick>, SubscribeError> {
    let mut probes: HashMap<usize, (DemandGuard, u64)> = HashMap::new();
    let mut picks: Vec<Pick> = Vec::new();
    for kind in MediaKind::ALL {
        let wish = want.get(kind);
        if !wish.is_wanted() {
            continue;
        }
        let list = candidates(stream, kind, wish);
        let Some(chosen) = first_offer(kind, &list, &mut probes, deadline).await else { continue };
        let Some(candidate) = list.get(chosen) else { continue };
        if let Some(pick) = picks.iter_mut().find(|p| Arc::ptr_eq(&p.slot, &candidate.slot)) {
            pick.kinds.push((kind, candidate.wish.clone()));
            for stream in &candidate.via {
                if !pick.via.iter().any(|s| Arc::ptr_eq(s, stream)) {
                    pick.via.push(stream.clone());
                }
            }
            continue;
        }
        let slot = candidate.slot.clone();
        let (guard, failures_before) = probes.remove(&key(&slot)).unwrap_or_else(|| (slot.acquire(), slot.failures()));
        let kinds = vec![(kind, candidate.wish.clone())];
        picks.push(Pick { slot, kinds, guard, failures_before, via: candidate.via.clone() });
    }
    if picks.is_empty() {
        return Err(SubscribeError::NoMatchingSource);
    }
    Ok(picks)
}

async fn first_offer(
    kind: MediaKind,
    list: &[Candidate],
    probes: &mut HashMap<usize, (DemandGuard, u64)>,
    deadline: Instant,
) -> Option<usize> {
    for (index, candidate) in list.iter().enumerate() {
        let (slot, wish) = (&candidate.slot, &candidate.wish);
        let offer = slot.offers(kind, wish);
        if offer == Offer::No {
            continue;
        }
        // Declined by its input and waiting out its backoff: it would not be
        // asked again before the backoff ends, so a new consumer that took it
        // on its word waited for that (or for the probe deadline) with nothing
        // to come of it. openIVR's live view asks a webcam's sound, which its
        // input declines (no microphone): RC2's viewers waited up to 8 s for
        // their answer (W4, D37). A source that is idle again is tried once.
        if slot.declined_and_waiting() {
            continue;
        }
        // Somewhere to go if this one fails. Without it, a camera that
        // fails its first attempt keeps its chance until the deadline (it
        // may refuse one connection and take the next); a source that
        // reads this server does not, its failure being the server's own.
        let fallback = list.iter().skip(index + 1).any(|c| c.slot.offers(kind, &c.wish) != Offer::No);
        let decisive = fallback || slot.is_local();
        let proven = slot.channel.tracks().is_some();
        if offer == Offer::Yes && (proven || !fallback) {
            return Some(index);
        }
        if decisive && slot.has_failed() {
            continue;
        }
        probes.entry(key(slot)).or_insert_with(|| (slot.acquire(), slot.failures()));
        match prove(slot, decisive, deadline).await {
            Proof::Tracks if slot.offers(kind, wish) == Offer::Yes => return Some(index),
            // Declared, still starting: taken on its word, as before.
            Proof::Late if offer == Offer::Yes => return Some(index),
            Proof::Tracks | Proof::Failed | Proof::Late => {}
        }
    }
    None
}

/// What a started source showed before the deadline.
enum Proof {
    /// Its tracks.
    Tracks,
    /// A failed attempt (only reported when it is decisive).
    Failed,
    /// Neither, by the deadline.
    Late,
}

/// Waits for a started source's tracks, or with `decisive` for its first
/// failed attempt too.
async fn prove(slot: &Slot, decisive: bool, deadline: Instant) -> Proof {
    let mut tracks = slot.channel.watch_tracks();
    let mut failures = slot.watch_failures();
    let before = *failures.borrow_and_update();
    let wait = async {
        loop {
            if tracks.borrow_and_update().is_some() {
                return Proof::Tracks;
            }
            if decisive && *failures.borrow_and_update() > before {
                return Proof::Failed;
            }
            tokio::select! {
                changed = tracks.changed() => if changed.is_err() { return Proof::Late },
                changed = failures.changed() => if changed.is_err() { return Proof::Late },
            }
        }
    };
    tokio::time::timeout_at(deadline, wait).await.unwrap_or(Proof::Late)
}
