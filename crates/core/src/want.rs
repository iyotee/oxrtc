//! What a consumer wants from a stream: which media, which codecs.

use crate::error::WantError;
use crate::media::{Codec, MediaKind};

/// The wish for one kind of media.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum MediaWant {
    /// Not wanted.
    #[default]
    Skip,
    /// Any codec.
    Any,
    /// One of these codecs, in order of preference.
    OneOf(Vec<Codec>),
}

impl MediaWant {
    /// Whether this media is wanted at all.
    pub fn is_wanted(&self) -> bool {
        !matches!(self, Self::Skip)
    }

    /// Whether `codec` satisfies the wish.
    pub fn accepts(&self, codec: Codec) -> bool {
        match self {
            Self::Skip => false,
            Self::Any => true,
            Self::OneOf(codecs) => codecs.contains(&codec),
        }
    }

    /// What satisfies both this wish and `filter`, in this wish's order of
    /// preference (a consumer's wish read through a relay's query).
    pub fn narrow(&self, filter: &MediaWant) -> MediaWant {
        match (self, filter) {
            (Self::Skip, _) | (_, Self::Skip) => Self::Skip,
            (Self::Any, other) | (other, Self::Any) => other.clone(),
            (Self::OneOf(mine), Self::OneOf(theirs)) => {
                let both: Vec<Codec> = mine.iter().copied().filter(|c| theirs.contains(c)).collect();
                if both.is_empty() { Self::Skip } else { Self::OneOf(both) }
            }
        }
    }

    fn widen(&mut self, other: MediaWant) {
        *self = match (std::mem::take(self), other) {
            (Self::Any, _) | (_, Self::Any) => Self::Any,
            (Self::Skip, w) | (w, Self::Skip) => w,
            (Self::OneOf(mut a), Self::OneOf(b)) => {
                for c in b {
                    if !a.contains(&c) {
                        a.push(c);
                    }
                }
                Self::OneOf(a)
            }
        };
    }
}

/// The order in which a consumer that can take several codecs of a media is
/// given them, whatever egress it reads (a WebRTC offer, an MSE codec list):
/// the camera's own H.265 before an H.264 transcode; Opus, then AAC, then the
/// PCM family. Each egress keeps the codecs it carries ([`Want::preferred`]).
pub const PREFERENCE: [Codec; 7] =
    [Codec::H265, Codec::H264, Codec::Opus, Codec::Aac, Codec::Pcma, Codec::Pcmu, Codec::L16];

/// The codecs of `kind` among `accepted`, in the order of [`PREFERENCE`].
pub fn preferred(kind: MediaKind, accepted: &[Codec]) -> Vec<Codec> {
    PREFERENCE.iter().copied().filter(|c| c.kind() == kind && accepted.contains(c)).collect()
}

/// A consumer's wish for video and audio.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Want {
    /// Video.
    pub video: MediaWant,
    /// Audio.
    pub audio: MediaWant,
}

impl Want {
    /// Every media, any codec.
    pub fn all() -> Self {
        Self { video: MediaWant::Any, audio: MediaWant::Any }
    }

    /// The wish for `kind`.
    pub fn get(&self, kind: MediaKind) -> &MediaWant {
        match kind {
            MediaKind::Video => &self.video,
            MediaKind::Audio => &self.audio,
        }
    }

    /// What a consumer able to take each of `accepted` asks for: each media
    /// in the order of [`PREFERENCE`], skipped when it takes none of it.
    pub fn preferred(accepted: &[Codec]) -> Self {
        let wish = |kind| {
            let codecs = preferred(kind, accepted);
            if codecs.is_empty() { MediaWant::Skip } else { MediaWant::OneOf(codecs) }
        };
        Self { video: wish(MediaKind::Video), audio: wish(MediaKind::Audio) }
    }

    /// Whether nothing at all is wanted.
    pub fn is_empty(&self) -> bool {
        !self.video.is_wanted() && !self.audio.is_wanted()
    }

    /// Reads go2rtc's query notation: `video`, `video=h264`, `video=h264,h265`
    /// (or the key repeated), `audio=opus`; `copy`, `all` and `any` mean any
    /// codec. A query naming no media wants everything; one that names a media
    /// excludes the other. Other keys belong to the protocol and are ignored.
    pub fn from_query(query: &str) -> Result<Self, WantError> {
        let mut want = Self::default();
        let mut named = false;
        for pair in query.trim_start_matches('?').split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let kind = match key {
                "video" => MediaKind::Video,
                "audio" => MediaKind::Audio,
                _ => continue,
            };
            named = true;
            let wish = parse_codecs(kind, value)?;
            match kind {
                MediaKind::Video => want.video.widen(wish),
                MediaKind::Audio => want.audio.widen(wish),
            }
        }
        Ok(if named { want } else { Self::all() })
    }
}

fn parse_codecs(kind: MediaKind, value: &str) -> Result<MediaWant, WantError> {
    let mut codecs = Vec::new();
    for name in value.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if matches!(name, "copy" | "all" | "any") {
            return Ok(MediaWant::Any);
        }
        let codec = Codec::parse(name).ok_or_else(|| WantError::UnknownCodec(name.chars().take(32).collect()))?;
        if codec.kind() != kind {
            return Err(WantError::WrongKind { codec: codec.as_str(), kind: kind.as_str() });
        }
        if !codecs.contains(&codec) {
            codecs.push(codec);
        }
    }
    Ok(if codecs.is_empty() { MediaWant::Any } else { MediaWant::OneOf(codecs) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go2rtc_queries() {
        assert_eq!(Want::from_query("").unwrap(), Want::all());
        assert_eq!(Want::from_query("timeout=5").unwrap(), Want::all());
        let w = Want::from_query("?video").unwrap();
        assert_eq!((w.video, w.audio), (MediaWant::Any, MediaWant::Skip));
        let w = Want::from_query("video=copy&audio=opus").unwrap();
        assert_eq!((w.video, w.audio), (MediaWant::Any, MediaWant::OneOf(vec![Codec::Opus])));
        let w = Want::from_query("video=h264&video=hevc").unwrap();
        assert_eq!(w.video, MediaWant::OneOf(vec![Codec::H264, Codec::H265]));
        let w = Want::from_query("video=h264,h265&audio").unwrap();
        assert_eq!((w.video, w.audio), (MediaWant::OneOf(vec![Codec::H264, Codec::H265]), MediaWant::Any));
    }

    #[test]
    fn narrowing_keeps_what_both_accept_in_the_consumers_order() {
        let one_of = |c: &[Codec]| MediaWant::OneOf(c.to_vec());
        assert_eq!(MediaWant::Any.narrow(&MediaWant::Skip), MediaWant::Skip);
        assert_eq!(MediaWant::Skip.narrow(&MediaWant::Any), MediaWant::Skip);
        assert_eq!(MediaWant::Any.narrow(&MediaWant::Any), MediaWant::Any);
        assert_eq!(MediaWant::Any.narrow(&one_of(&[Codec::Opus])), one_of(&[Codec::Opus]));
        assert_eq!(one_of(&[Codec::Pcma]).narrow(&MediaWant::Any), one_of(&[Codec::Pcma]));
        let mine = one_of(&[Codec::H265, Codec::H264]);
        assert_eq!(mine.narrow(&one_of(&[Codec::H264, Codec::H265])), mine, "the consumer's preference");
        assert_eq!(mine.narrow(&one_of(&[Codec::H264])), one_of(&[Codec::H264]));
        assert_eq!(one_of(&[Codec::Opus]).narrow(&one_of(&[Codec::Pcma])), MediaWant::Skip, "nothing in common");
    }

    #[test]
    fn one_order_of_preference_for_every_egress() {
        // Every codec an egress chooses among (all but JPEG, which is sent
        // as it is), once each.
        let chosen: Vec<Codec> = Codec::ALL.into_iter().filter(|c| *c != Codec::Jpeg).collect();
        assert_eq!(PREFERENCE.len(), chosen.len());
        assert!(chosen.iter().all(|c| PREFERENCE.contains(c)));
        // The order does not depend on the consumer's.
        let listed = [Codec::L16, Codec::Pcmu, Codec::Pcma, Codec::Aac, Codec::Opus, Codec::H264, Codec::H265];
        let want = Want::preferred(&listed);
        assert_eq!(want.video, MediaWant::OneOf(vec![Codec::H265, Codec::H264]), "the camera's H.265 first");
        assert_eq!(want.audio, MediaWant::OneOf(vec![Codec::Opus, Codec::Aac, Codec::Pcma, Codec::Pcmu, Codec::L16]));
        assert_eq!(Want::preferred(&[Codec::Opus]).video, MediaWant::Skip, "no video taken");
        assert!(Want::preferred(&[Codec::Jpeg]).is_empty());
    }

    #[test]
    fn refuses_unknown_and_misplaced_codecs() {
        assert!(matches!(Want::from_query("video=vp9"), Err(WantError::UnknownCodec(_))));
        assert!(matches!(Want::from_query("audio=h264"), Err(WantError::WrongKind { .. })));
    }
}
