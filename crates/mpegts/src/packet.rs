//! Transport packets (ISO/IEC 13818-1, 2.4.3): a payload cut into 188-byte
//! packets of one PID, the last one filled by an adaptation field's stuffing
//! (a PES) or by `0xFF` after the section (a table).

use crate::PACKET_SIZE;

/// A packet's payload when it has no adaptation field.
const ROOM: usize = PACKET_SIZE - 4;

/// What the first packet of a payload says in its adaptation field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Marks {
    /// The program clock reference, on the 90 kHz clock (its base).
    pub pcr: Option<u64>,
    /// `random_access_indicator`: a decoder can start at this PES.
    pub random_access: bool,
}

/// One PID's packets and continuity counter.
#[derive(Debug, Clone)]
pub(crate) struct Pid {
    pub pid: u16,
    counter: u8,
}

fn pcr_bytes(base: u64) -> [u8; 6] {
    let base = base & ((1 << 33) - 1);
    [
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        // The base's last bit, six reserved bits, the extension's ninth (0).
        (((base & 1) as u8) << 7) | 0x7E,
        0, // the extension: 0, the reference is on the 90 kHz clock
    ]
}

impl Pid {
    pub(crate) fn new(pid: u16) -> Self {
        Self { pid, counter: 0 }
    }

    fn header(&mut self, start: bool, adaptation: bool) -> [u8; 4] {
        let control = if adaptation { 0x30 } else { 0x10 };
        let header = [
            0x47,
            (u8::from(start) << 6) | ((self.pid >> 8) & 0x1F) as u8,
            (self.pid & 0xFF) as u8,
            control | self.counter,
        ];
        self.counter = (self.counter + 1) & 0x0F;
        header
    }

    /// A section in one packet: the pointer field, the section, `0xFF`
    /// after it. A section that does not fit is cut at the packet's end
    /// (none of the writer's comes near: a PMT of eight tracks is 120 bytes).
    pub(crate) fn section(&mut self, out: &mut Vec<u8>, section: &[u8]) {
        let start = out.len();
        out.extend_from_slice(&self.header(true, false));
        out.push(0); // pointer_field
        out.extend_from_slice(section.get(..section.len().min(ROOM - 1)).unwrap_or_default());
        out.resize(start + PACKET_SIZE, 0xFF);
    }

    /// A PES cut into packets: the first with `payload_unit_start_indicator`
    /// and `marks`, the last filled by stuffing.
    pub(crate) fn pes(&mut self, out: &mut Vec<u8>, payload: &[u8], marks: Marks) {
        let mut rest = payload;
        let mut first = true;
        while first || !rest.is_empty() {
            // The adaptation field after its length byte.
            let mut field: Vec<u8> = Vec::new();
            if first && (marks.pcr.is_some() || marks.random_access) {
                let flags = (u8::from(marks.random_access) << 6) | (u8::from(marks.pcr.is_some()) << 4);
                field.push(flags);
                if let Some(pcr) = marks.pcr {
                    field.extend_from_slice(&pcr_bytes(pcr));
                }
            }
            let mut adaptation = !field.is_empty();
            let room = if adaptation { ROOM - 1 - field.len() } else { ROOM };
            if rest.len() < room {
                let missing = room - rest.len();
                if adaptation {
                    field.resize(field.len() + missing, 0xFF);
                } else {
                    // The length byte alone fills one; flags and 0xFF the rest.
                    adaptation = true;
                    if missing >= 2 {
                        field.push(0x00);
                        field.resize(missing - 1, 0xFF);
                    }
                }
            }
            let start = out.len();
            out.extend_from_slice(&self.header(first, adaptation));
            if adaptation {
                out.push(u8::try_from(field.len()).unwrap_or(u8::MAX));
                out.extend_from_slice(&field);
            }
            let take = (PACKET_SIZE - (out.len() - start)).min(rest.len());
            let (now, later) = rest.split_at(take);
            out.extend_from_slice(now);
            rest = later;
            first = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(payload_len: usize, marks: Marks) -> Vec<u8> {
        let mut pid = Pid::new(0x100);
        let mut out = Vec::new();
        pid.pes(&mut out, &vec![0xAB; payload_len], marks);
        out
    }

    #[test]
    fn every_size_fills_whole_packets_with_its_payload() {
        for marks in [Marks::default(), Marks { pcr: Some(12_345), random_access: true }] {
            for len in [1, 2, 3, 170, 175, 176, 177, 182, 183, 184, 185, 366, 367, 368, 1000] {
                let out = packets(len, marks);
                assert_eq!(out.len() % PACKET_SIZE, 0, "{len}");
                let mut carried = 0;
                for (i, packet) in out.chunks(PACKET_SIZE).enumerate() {
                    assert_eq!(packet[0], 0x47);
                    assert_eq!(packet[1] & 0x40 != 0, i == 0, "start only on the first");
                    assert_eq!(packet[3] & 0x0F, i as u8 & 0x0F, "continuity");
                    let at = if packet[3] & 0x20 != 0 { 5 + usize::from(packet[4]) } else { 4 };
                    assert!(at <= PACKET_SIZE);
                    assert!(packet[at..].iter().all(|b| *b == 0xAB), "{len}: payload only after the field");
                    carried += PACKET_SIZE - at;
                }
                assert_eq!(carried, len, "{len}");
            }
        }
    }

    #[test]
    fn the_pcr_and_random_access_are_in_the_first_packet() {
        let out = packets(400, Marks { pcr: Some(0x1_2345_6789), random_access: true });
        assert_eq!(out[3] & 0x30, 0x30);
        assert_eq!(out[4], 7);
        assert_eq!(out[5], 0x50);
        let pcr = &out[6..12];
        let base = (u64::from(pcr[0]) << 25)
            | (u64::from(pcr[1]) << 17)
            | (u64::from(pcr[2]) << 9)
            | (u64::from(pcr[3]) << 1)
            | u64::from(pcr[4] >> 7);
        assert_eq!(base, 0x1_2345_6789);
        assert_eq!(out[PACKET_SIZE + 3] & 0x30, 0x10, "the second packet has no field");
    }

    #[test]
    fn a_section_is_one_packet_padded_with_ff() {
        let mut pid = Pid::new(0);
        let mut out = Vec::new();
        pid.section(&mut out, &[1, 2, 3]);
        pid.section(&mut out, &[1, 2, 3]);
        assert_eq!(out.len(), 2 * PACKET_SIZE);
        assert_eq!(&out[..8], &[0x47, 0x40, 0x00, 0x10, 0, 1, 2, 3]);
        assert!(out[8..PACKET_SIZE].iter().all(|b| *b == 0xFF));
        assert_eq!(out[PACKET_SIZE + 3], 0x11, "the counter runs on");
    }
}
