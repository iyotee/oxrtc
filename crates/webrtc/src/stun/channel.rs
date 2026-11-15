//! TURN's ChannelData messages (RFC 8656 section 12.5): a 4-byte header
//! (channel number, length) instead of a 36-byte Send or Data indication.

/// The first and last channel numbers a client may bind (RFC 8656 section 12).
pub(crate) const FIRST_CHANNEL: u16 = 0x4000;
pub(crate) const LAST_CHANNEL: u16 = 0x4FFF;

pub(crate) fn is_channel_number(number: u16) -> bool {
    (FIRST_CHANNEL..=LAST_CHANNEL).contains(&number)
}

/// A received ChannelData message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelData<'a> {
    pub(crate) channel: u16,
    pub(crate) data: &'a [u8],
}

impl<'a> ChannelData<'a> {
    /// Parses one ChannelData datagram. Over UDP the padding to four bytes
    /// is optional (RFC 8656 section 12.5); anything longer than the padded
    /// length is refused.
    pub(crate) fn parse(datagram: &'a [u8]) -> Option<Self> {
        let (header, rest) = datagram.split_first_chunk::<4>()?;
        let [c0, c1, l0, l1] = *header;
        let channel = u16::from_be_bytes([c0, c1]);
        let length = usize::from(u16::from_be_bytes([l0, l1]));
        if !is_channel_number(channel) || rest.len() < length || rest.len() > length.next_multiple_of(4) {
            return None;
        }
        Some(Self { channel, data: rest.get(..length)? })
    }
}

/// A ChannelData datagram carrying `data` (unpadded: UDP keeps its length).
pub(crate) fn channel_data(channel: u16, data: &[u8]) -> Option<Vec<u8>> {
    let length = u16::try_from(data.len()).ok()?;
    let mut out = Vec::with_capacity(4 + data.len());
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(data);
    Some(out)
}
