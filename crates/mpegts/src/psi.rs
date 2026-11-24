//! The program's tables (ISO/IEC 13818-1, 2.4.4): the program association
//! table naming one program, and its program map table.

use crate::crc::crc32;

/// The one program's number.
pub(crate) const PROGRAM: u16 = 1;

/// A stream of the program map table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub stream_type: u8,
    pub pid: u16,
    pub descriptors: Vec<u8>,
}

/// A long-form section: its header, `body`, its CRC.
fn section(table_id: u8, extension: u16, body: &[u8]) -> Vec<u8> {
    // From after the length field to the end of the CRC.
    let length = 5 + body.len() + 4;
    let mut out = Vec::with_capacity(3 + length);
    out.push(table_id);
    // section_syntax_indicator, '0', two reserved bits, the 12-bit length.
    out.push(0xB0 | ((length >> 8) & 0x0F) as u8);
    out.push((length & 0xFF) as u8);
    out.extend_from_slice(&extension.to_be_bytes());
    // Two reserved bits, version 0, current_next_indicator.
    out.push(0xC1);
    out.push(0); // section_number
    out.push(0); // last_section_number
    out.extend_from_slice(body);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn pid_field(reserved: u8, pid: u16) -> [u8; 2] {
    [reserved | ((pid >> 8) & 0x1F) as u8, (pid & 0xFF) as u8]
}

/// The program association table: program 1 at `pmt_pid`.
pub(crate) fn pat(pmt_pid: u16) -> Vec<u8> {
    let mut body = PROGRAM.to_be_bytes().to_vec();
    body.extend_from_slice(&pid_field(0xE0, pmt_pid));
    // transport_stream_id 1.
    section(0x00, 1, &body)
}

/// The program map table of program 1. A stream's descriptors must stay
/// under 1 KiB (the writer's are a few bytes), and the whole section under
/// one packet's payload.
pub(crate) fn pmt(pcr_pid: u16, entries: &[Entry]) -> Vec<u8> {
    let mut body = pid_field(0xE0, pcr_pid).to_vec();
    body.extend_from_slice(&[0xF0, 0x00]); // no program descriptor
    for entry in entries {
        body.push(entry.stream_type);
        body.extend_from_slice(&pid_field(0xE0, entry.pid));
        let length = entry.descriptors.len().min(0x3FF);
        body.extend_from_slice(&[0xF0 | ((length >> 8) & 0x03) as u8, (length & 0xFF) as u8]);
        body.extend_from_slice(entry.descriptors.get(..length).unwrap_or_default());
    }
    section(0x02, PROGRAM, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pat_is_ffmpegs() {
        // FFmpeg 8.0.1's PAT for one program at 0x1000.
        let expected = [0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0x2A, 0xB1, 0x04, 0xB2];
        assert_eq!(pat(0x1000), expected);
    }

    #[test]
    fn pmt_lists_each_stream_and_its_crc_checks() {
        let entries = [
            Entry { stream_type: 0x1B, pid: 0x100, descriptors: Vec::new() },
            Entry { stream_type: 0x06, pid: 0x101, descriptors: vec![0x05, 4, b'O', b'p', b'u', b's'] },
        ];
        let table = pmt(0x100, &entries);
        assert_eq!(&table[..3], &[0x02, 0xB0, (table.len() - 3) as u8]);
        assert_eq!(&table[8..10], &[0xE1, 0x00], "the PCR PID");
        assert_eq!(&table[12..17], &[0x1B, 0xE1, 0x00, 0xF0, 0x00]);
        assert_eq!(&table[17..22], &[0x06, 0xE1, 0x01, 0xF0, 0x06]);
        assert_eq!(crc32(&table), 0);
    }
}
