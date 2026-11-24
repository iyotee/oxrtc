//! An MPEG-TS reader written apart from the writer, for its tests and its
//! fuzz targets: packets checked (sync byte, continuity), the PAT and the
//! PMT read with their CRC, each PES put back together with its times.
//! F14 makes it the reader of MPEG-TS sources.

use std::collections::BTreeMap;

use crate::PACKET_SIZE;
use crate::crc::crc32;

/// Largest PES put back together.
pub const MAX_PES_BYTES: usize = 32 * 1024 * 1024;

/// A stream the PMT lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// Its type.
    pub stream_type: u8,
    /// Its PID.
    pub pid: u16,
    /// Its descriptors.
    pub descriptors: Vec<u8>,
}

/// A PES packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pes {
    /// The PID it came on.
    pub pid: u16,
    /// Its `stream_id`.
    pub stream_id: u8,
    /// Its presentation time.
    pub pts: Option<u64>,
    /// Its decode time, when written.
    pub dts: Option<u64>,
    /// The program clock reference its first packet carried.
    pub pcr: Option<u64>,
    /// Its first packet's `random_access_indicator`.
    pub random_access: bool,
    /// The length its header gave (0: as long as it lasts).
    pub declared: u16,
    /// Its payload.
    pub data: Vec<u8>,
}

/// What a stream holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Read {
    /// The PMT's PID, from the PAT.
    pub pmt_pid: Option<u16>,
    /// The PCR's PID, from the PMT.
    pub pcr_pid: Option<u16>,
    /// The PMT's streams.
    pub streams: Vec<Stream>,
    /// The tables read (PAT and PMT), counted.
    pub tables: usize,
    /// Every PES, in order.
    pub pes: Vec<Pes>,
}

fn time(b: &[u8]) -> Option<u64> {
    let [a, b1, c, d, e] = *b.first_chunk::<5>()?;
    if a & 1 == 0 || c & 1 == 0 || e & 1 == 0 {
        return None;
    }
    Some(
        (u64::from(a & 0x0E) << 29)
            | (u64::from(b1) << 22)
            | (u64::from(c >> 1) << 15)
            | (u64::from(d) << 7)
            | u64::from(e >> 1),
    )
}

/// A section's body, after its header, its CRC checked.
fn section(payload: &[u8]) -> Result<(u8, &[u8]), String> {
    let pointer = usize::from(*payload.first().ok_or("no pointer field")?);
    let rest = payload.get(1 + pointer..).ok_or("a pointer past the packet")?;
    let head = rest.get(..3).ok_or("a section header past the packet")?;
    let table_id = head.first().copied().unwrap_or(0xFF);
    let length =
        (usize::from(head.get(1).copied().unwrap_or(0) & 0x0F) << 8) | usize::from(head.get(2).copied().unwrap_or(0));
    let whole = rest.get(..3 + length).ok_or("a section longer than its packet")?;
    if length < 9 || crc32(whole) != 0 {
        return Err(format!("table {table_id}: a bad length or CRC"));
    }
    Ok((table_id, whole.get(8..whole.len() - 4).unwrap_or_default()))
}

fn pid_of(b: &[u8]) -> u16 {
    (u16::from(b.first().copied().unwrap_or(0) & 0x1F) << 8) | u16::from(b.get(1).copied().unwrap_or(0))
}

fn pmt(read: &mut Read, body: &[u8]) -> Result<(), String> {
    read.pcr_pid = Some(pid_of(body));
    let info =
        (usize::from(body.get(2).copied().unwrap_or(0) & 0x0F) << 8) | usize::from(body.get(3).copied().unwrap_or(0));
    let mut rest = body.get(4 + info..).ok_or("program info past the section")?;
    let mut streams = Vec::new();
    while !rest.is_empty() {
        let head = rest.get(..5).ok_or("a stream entry cut short")?;
        let length = (usize::from(head.get(3).copied().unwrap_or(0) & 0x03) << 8)
            | usize::from(head.get(4).copied().unwrap_or(0));
        let descriptors = rest.get(5..5 + length).ok_or("descriptors past the section")?.to_vec();
        streams.push(Stream {
            stream_type: head.first().copied().unwrap_or(0),
            pid: pid_of(head.get(1..).unwrap_or_default()),
            descriptors,
        });
        rest = rest.get(5 + length..).unwrap_or_default();
    }
    read.streams = streams;
    Ok(())
}

/// A PES from the bytes put back together.
fn finish(pid: u16, bytes: &[u8], pcr: Option<u64>, random_access: bool) -> Result<Pes, String> {
    let head = bytes.get(..9).ok_or("a PES header cut short")?;
    if head.get(..3) != Some(&[0, 0, 1][..]) {
        return Err(format!("PID {pid}: no PES start code"));
    }
    let stream_id = head.get(3).copied().unwrap_or(0);
    let declared = u16::from_be_bytes([head.get(4).copied().unwrap_or(0), head.get(5).copied().unwrap_or(0)]);
    let flags = head.get(7).copied().unwrap_or(0);
    let header_length = usize::from(head.get(8).copied().unwrap_or(0));
    let fields = bytes.get(9..9 + header_length).ok_or("a PES header longer than the PES")?;
    let pts = if flags & 0x80 != 0 { Some(time(fields).ok_or("a bad PTS")?) } else { None };
    let dts =
        if flags & 0xC0 == 0xC0 { Some(time(fields.get(5..).unwrap_or_default()).ok_or("a bad DTS")?) } else { None };
    let data = bytes.get(9 + header_length..).unwrap_or_default().to_vec();
    if declared != 0 && usize::from(declared) != bytes.len() - 6 {
        return Err(format!("PID {pid}: a PES of {} bytes said {declared}", bytes.len() - 6));
    }
    Ok(Pes { pid, stream_id, pts, dts, pcr, random_access, declared, data })
}

struct Open {
    bytes: Vec<u8>,
    pcr: Option<u64>,
    random_access: bool,
}

/// Reads `data`, which must be whole packets.
pub fn read(data: &[u8]) -> Result<Read, String> {
    if !data.len().is_multiple_of(PACKET_SIZE) {
        return Err(format!("{} bytes is not whole packets", data.len()));
    }
    let mut read = Read::default();
    let mut counters: BTreeMap<u16, u8> = BTreeMap::new();
    let mut open: BTreeMap<u16, Open> = BTreeMap::new();
    for (index, packet) in data.as_chunks::<PACKET_SIZE>().0.iter().enumerate() {
        let head = packet.get(..4).ok_or("a short packet")?;
        if head.first() != Some(&0x47) {
            return Err(format!("packet {index}: no sync byte"));
        }
        let b1 = head.get(1).copied().unwrap_or(0);
        if b1 & 0x80 != 0 {
            return Err(format!("packet {index}: a transport error"));
        }
        let start = b1 & 0x40 != 0;
        let pid = pid_of(head.get(1..).unwrap_or_default());
        let control = head.get(3).copied().unwrap_or(0);
        let counter = control & 0x0F;
        let has_payload = control & 0x10 != 0;
        let mut at = 4;
        let mut pcr = None;
        let mut random_access = false;
        if control & 0x20 != 0 {
            let length = usize::from(packet.get(4).copied().unwrap_or(0));
            let field = packet
                .get(5..5 + length)
                .ok_or_else(|| format!("packet {index}: an adaptation field past the packet"))?;
            if let Some(flags) = field.first() {
                random_access = flags & 0x40 != 0;
                if flags & 0x10 != 0 {
                    let b = field.get(1..7).ok_or("a PCR cut short")?;
                    let base = b.iter().take(4).fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte));
                    pcr = Some((base << 1) | u64::from(b.get(4).copied().unwrap_or(0) >> 7));
                }
            }
            at = 5 + length;
        }
        if has_payload {
            if let Some(last) = counters.insert(pid, counter)
                && (last + 1) & 0x0F != counter
            {
                return Err(format!("packet {index}: PID {pid} counter {counter} after {last}"));
            }
        } else {
            continue;
        }
        let payload = packet.get(at..).unwrap_or_default();
        if pid == 0 || Some(pid) == read.pmt_pid {
            if !start {
                return Err(format!("packet {index}: a table across packets"));
            }
            let (table_id, body) = section(payload)?;
            read.tables += 1;
            match (pid, table_id) {
                (0, 0x00) => read.pmt_pid = Some(pid_of(body.get(2..).unwrap_or_default())),
                (_, 0x02) => pmt(&mut read, body)?,
                _ => return Err(format!("packet {index}: table {table_id} on PID {pid}")),
            }
            continue;
        }
        if start {
            if let Some(done) = open.remove(&pid) {
                read.pes.push(finish(pid, &done.bytes, done.pcr, done.random_access)?);
            }
            open.insert(pid, Open { bytes: Vec::new(), pcr, random_access });
        }
        let Some(current) = open.get_mut(&pid) else { continue };
        if current.bytes.len() + payload.len() > MAX_PES_BYTES {
            return Err(format!("PID {pid}: a PES over {MAX_PES_BYTES} bytes"));
        }
        current.bytes.extend_from_slice(payload);
    }
    for (pid, done) in open {
        read.pes.push(finish(pid, &done.bytes, done.pcr, done.random_access)?);
    }
    Ok(read)
}
