//! The Huffman coder and decoder, the bit reader and writer, and the scan
//! coded again: on real files whose tables fit the picture (FFmpeg's encoder
//! and libjpeg's `optimize_coding`), and on random blocks coded with tables of
//! the test's own.

use proptest::prelude::*;

use super::*;
use crate::huffman::{BitReader, BitWriter, Decoder, Encoder, Table};
use crate::jpeg::tests::Build;
use crate::jpeg::{JpegError, parse};

/// What an encoder fits to the picture: FFmpeg (4:2:0, one DHT of the four),
/// then libjpeg's `optimize_coding` (4:2:2; 4:2:0 with a restart interval of
/// 3; 4:2:0 noise, whose coefficients run to the largest sizes).
const FFMPEG: &[u8] = include_bytes!("../tests/data/optimal.jpg");
const PIL_422: &[u8] = include_bytes!("../tests/data/optimal422.jpg");
const PIL_RESTART: &[u8] = include_bytes!("../tests/data/optimal420_restart.jpg");
const PIL_NOISE: &[u8] = include_bytes!("../tests/data/optimal_noise.jpg");

/// One block as the scan holds it: the symbol and the raw bits after it, in
/// order, the DC symbol first.
type Block = Vec<(u8, u32)>;

/// The DHT tables of a file by `Tc << 4 | Th`, and its scan (SOS to EOI).
fn parts(file: &[u8]) -> (Vec<(u8, Table)>, &[u8], u16) {
    let (mut tables, mut restart, mut at) = (Vec::new(), 0u16, 2);
    loop {
        let code = file[at + 1];
        let length = usize::from(u16::from_be_bytes([file[at + 2], file[at + 3]]));
        let body = &file[at + 4..at + 2 + length];
        match code {
            0xc4 => {
                let mut rest = body;
                while let Some((&info, after)) = rest.split_first() {
                    let counts: [u8; 16] = after[..16].try_into().unwrap();
                    let n: usize = counts.iter().map(|c| usize::from(*c)).sum();
                    tables.push((info, Table::new(&counts, &after[16..16 + n]).unwrap()));
                    rest = &after[16 + n..];
                }
            }
            0xdd => restart = u16::from_be_bytes([body[0], body[1]]),
            0xda => return (tables, &file[at + 2 + length..file.len() - 2], restart),
            _ => {}
        }
        at += 2 + length;
    }
}

/// Reads every block of `scan` with `coding`.
fn blocks_of(scan: &Scan<'_>) -> Result<Vec<Block>, String> {
    let decoders = scan.coding.each_ref().map(|c| (Decoder::new(&c.dc), Decoder::new(&c.ac)));
    let mut bits = BitReader::new(scan.data);
    let mut blocks = Vec::new();
    for mcu in 0..scan.mcus {
        for ((dc, ac), count) in decoders.iter().zip(scan.blocks) {
            for _ in 0..count {
                let mut block = Block::new();
                let size = dc.symbol(&mut bits).ok_or("dc")?;
                block.push((size, bits.bits(u32::from(size)).ok_or("dc bits")?));
                let mut index = 1;
                while index < 64 {
                    let symbol = ac.symbol(&mut bits).ok_or("ac")?;
                    let (run, size) = (usize::from(symbol >> 4), u32::from(symbol & 15));
                    let raw = if size > 0 { bits.bits(size).ok_or("ac bits")? } else { 0 };
                    block.push((symbol, raw));
                    if symbol == 0 {
                        break;
                    }
                    index += if size == 0 { 16 } else { run + 1 };
                }
                blocks.push(block);
            }
        }
        let done = mcu + 1;
        if done < scan.mcus && scan.restart.is_some_and(|n| done % n == 0) {
            bits.restart().ok_or("restart")?;
        }
    }
    Ok(blocks)
}

fn standard_coding() -> [Coding; 3] {
    let luma = || Coding { dc: Table::standard(false, false), ac: Table::standard(true, false) };
    let chroma = || Coding { dc: Table::standard(false, true), ac: Table::standard(true, true) };
    [luma(), chroma(), chroma()]
}

/// A real file's scan, and what it holds read with its own tables.
fn read_file(file: &[u8]) -> (Scan<'_>, Vec<Block>) {
    let jpeg = parse(file).unwrap();
    let (tables, data, restart) = parts(file);
    let table = |info: u8| tables.iter().find(|(i, _)| *i == info).map(|(_, t)| t.clone()).unwrap();
    let coding = [0x00u8, 0x01, 0x01].map(|id| Coding { dc: table(id), ac: table(0x10 | id) });
    let scan = Scan {
        data,
        blocks: [if jpeg.kind & 1 == 1 { 4 } else { 2 }, 1, 1],
        mcus: usize::from(jpeg.width).div_ceil(16)
            * usize::from(jpeg.height).div_ceil(if jpeg.kind & 1 == 1 { 16 } else { 8 }),
        restart: (restart > 0).then_some(usize::from(restart)),
        coding,
    };
    let blocks = blocks_of(&scan).unwrap();
    (scan, blocks)
}

#[test]
fn the_standard_codes_are_the_ones_of_the_standard() {
    // T.81 K.3: DC luminance size 0 is `00`, size 1 `010`; K.5: AC luminance
    // end of block `1010`, run 0 size 1 `00`, sixteen zeros `11111111001`.
    let code = |ac: bool, chroma: bool, symbol: u8| {
        let mut bits = BitWriter::default();
        assert!(Encoder::new(&Table::standard(ac, chroma)).put(&mut bits, symbol));
        bits.finish()
    };
    assert_eq!(code(false, false, 0), [0b0011_1111]);
    assert_eq!(code(false, false, 1), [0b0101_1111]);
    assert_eq!(code(true, false, 0x00), [0b1010_1111]);
    assert_eq!(code(true, false, 0x01), [0b0011_1111]);
}

#[test]
fn every_standard_symbol_reads_back_what_was_written() {
    for (ac, chroma, symbols) in [(false, false, 12), (false, true, 12), (true, false, 162), (true, true, 162)] {
        let table = Table::standard(ac, chroma);
        let (encoder, decoder) = (Encoder::new(&table), Decoder::new(&table));
        let mut all = Vec::new();
        let mut bits = BitWriter::default();
        for symbol in 0..=255u8 {
            if encoder.put(&mut bits, symbol) {
                all.push(symbol);
            }
        }
        assert_eq!(all.len(), symbols);
        let written = bits.finish();
        let mut reader = BitReader::new(&written);
        let read: Vec<u8> = (0..all.len()).map(|_| decoder.symbol(&mut reader).unwrap()).collect();
        assert_eq!(read, all);
    }
}

#[test]
fn tables_that_are_not_prefix_codes_are_refused() {
    let mut counts = [0u8; 16];
    counts[0] = 3; // three codes of one bit
    assert!(Table::new(&counts, &[0, 1, 2]).is_err());
    counts[0] = 2;
    assert!(Table::new(&counts, &[0, 1]).is_ok());
    counts[1] = 1; // a third code, with no code space left
    assert!(Table::new(&counts, &[0, 1, 2]).is_err());
    assert!(Table::new(&[0; 16], &[1]).is_err(), "values with no counts");
    assert!(Table::new(&counts, &[0, 1]).is_err(), "counts with no values");
    assert!(Table::new(&[255; 16], &[0; 256]).is_err(), "more than 256 symbols");
}

#[test]
fn the_bit_writer_stuffs_and_aligns_and_the_reader_undoes_it() {
    let mut writer = BitWriter::default();
    writer.put(0xff, 8);
    writer.put(0b101, 3);
    writer.align();
    writer.marker(0xd0);
    writer.put(0x12, 8);
    let out = writer.finish();
    assert_eq!(out, [0xff, 0x00, 0b1011_1111, 0xff, 0xd0, 0x12]);
    let mut reader = BitReader::new(&out);
    assert_eq!(reader.bits(8), Some(0xff));
    assert_eq!(reader.bits(3), Some(0b101));
    assert_eq!(reader.restart(), Some(0xd0));
    assert_eq!(reader.bits(8), Some(0x12));
    assert_eq!(reader.bit(), None, "the data ended");
}

#[test]
fn the_reader_stops_at_a_marker_and_at_the_end() {
    let mut reader = BitReader::new(&[0x80, 0xff, 0xd9]);
    assert_eq!(reader.bits(8), Some(0x80));
    assert_eq!(reader.bit(), None);
    assert_eq!(reader.restart(), None, "an end of image is no restart");
    let mut reader = BitReader::new(&[0xff, 0xff, 0xff, 0xd3, 0x55]);
    assert_eq!(reader.restart(), Some(0xd3), "fill bytes before a marker");
    assert_eq!(reader.bits(8), Some(0x55));
}

#[test]
fn a_file_coded_with_its_own_tables_is_coded_again_with_the_standard_ones() {
    for (name, file) in [("ffmpeg", FFMPEG), ("4:2:2", PIL_422), ("restart", PIL_RESTART), ("noise", PIL_NOISE)] {
        let (scan, blocks) = read_file(file);
        assert!(
            scan.coding.iter().zip(standard_coding()).any(|(own, std)| own.dc != std.dc || own.ac != std.ac),
            "{name}: the fixture has the standard tables"
        );
        let recoded = parse(file).unwrap();
        assert!(matches!(recoded.scan, std::borrow::Cow::Owned(_)), "{name}");
        assert_ne!(&*recoded.scan, scan.data, "{name}");
        assert!(
            recoded.scan.len() < 2 * scan.data.len() + 16,
            "{name}: {} from {}",
            recoded.scan.len(),
            scan.data.len()
        );
        // The same symbols and the same raw bits, block by block.
        let standard = Scan { data: &recoded.scan, coding: standard_coding(), ..scan };
        assert_eq!(blocks_of(&standard).unwrap(), blocks, "{name}");
    }
}

#[test]
fn restart_markers_stay_where_they_were_and_in_order() {
    let markers = |data: &[u8]| -> Vec<u8> {
        data.windows(2).filter(|w| w[0] == 0xff && (0xd0..=0xd7).contains(&w[1])).map(|w| w[1]).collect()
    };
    let (scan, _) = read_file(PIL_RESTART);
    let recoded = parse(PIL_RESTART).unwrap();
    assert_eq!(recoded.restart, Some(3));
    assert!(markers(scan.data).len() > 4);
    assert_eq!(markers(&recoded.scan), markers(scan.data));
}

#[test]
fn a_scan_that_does_not_decode_is_refused_never_half_converted() {
    let (scan, _) = read_file(PIL_NOISE);
    let limit = 1 << 24;
    let with = |data: &[u8]| to_standard(&Scan { data, ..scan.clone() }, limit);
    assert!(with(scan.data).is_ok());
    // Cut short.
    for end in [0, 1, scan.data.len() / 3, scan.data.len() - 40] {
        assert!(matches!(with(&scan.data[..end]), Err(Refusal::Malformed(_))), "{end}");
    }
    // A marker in the middle of the data.
    let mut marked = scan.data.to_vec();
    marked.splice(scan.data.len() / 2..scan.data.len() / 2, [0xff, 0xd9]);
    assert!(matches!(with(&marked), Err(Refusal::Malformed(_))));
    // More coded bytes than allowed.
    assert_eq!(to_standard(&scan, 10), Err(Refusal::TooLarge));
}

#[test]
fn restarts_that_are_missing_or_misplaced_are_refused() {
    let (scan, _) = read_file(PIL_RESTART);
    // The file's own interval is 3; read as 2, the marker is not where it should be.
    let early = Scan { restart: Some(2), ..scan.clone() };
    assert!(matches!(to_standard(&early, 1 << 24), Err(Refusal::Malformed(_))));
    // Read as none, the first marker is in the way.
    let none = Scan { restart: None, ..scan };
    assert!(matches!(to_standard(&none, 1 << 24), Err(Refusal::Malformed(_))));
}

/// The test's own tables: every symbol with a code of one length (a valid
/// prefix code no standard table is), or the standard symbols in the opposite
/// order (the standard lengths, the symbols exchanged).
fn custom(ac: bool, flat: bool) -> Table {
    let standard = Table::standard(ac, false).dht_body();
    let (counts, values) = standard.split_at(16);
    if flat {
        let mut flat = [0u8; 16];
        flat[if ac { 7 } else { 3 }] = u8::try_from(values.len()).unwrap();
        Table::new(&flat, values).unwrap()
    } else {
        let reversed: Vec<u8> = values.iter().rev().copied().collect();
        Table::new(counts.try_into().unwrap(), &reversed).unwrap()
    }
}

/// A block of random coefficients: a DC size and its bits, then AC symbols
/// (runs, sizes, a few sixteen-zero runs) up to the end of the block.
fn block_strategy() -> impl Strategy<Value = Block> {
    (0u8..=11, any::<u32>(), proptest::collection::vec((0u8..16, 1u8..=10, any::<u32>(), 0u8..2), 0..24)).prop_map(
        |(dc_size, dc_bits, acs)| {
            let mut block = vec![(dc_size, dc_bits & ((1 << dc_size) - 1))];
            let mut index = 1usize;
            for (run, size, bits, zrl) in acs {
                let needed = usize::from(zrl) * 16 + usize::from(run) + 1;
                if index + needed > 64 {
                    break;
                }
                block.extend(std::iter::repeat_n((0xf0, 0), usize::from(zrl)));
                block.push((run << 4 | size, bits & ((1 << size) - 1)));
                index += needed;
            }
            if index < 64 {
                block.push((0, 0));
            }
            block
        },
    )
}

/// `blocks` coded with `tables` (Y, Cb and Cr alike), restart markers every
/// `restart` MCUs of `per_mcu` blocks.
fn write_blocks(blocks: &[Block], tables: &(Table, Table), per_mcu: usize, restart: Option<usize>) -> Vec<u8> {
    let (dc, ac) = (Encoder::new(&tables.0), Encoder::new(&tables.1));
    let mut out = BitWriter::default();
    let mcus = blocks.len() / per_mcu;
    for (index, block) in blocks.iter().enumerate() {
        for (n, (symbol, raw)) in block.iter().enumerate() {
            let size = if n == 0 { *symbol } else { symbol & 15 };
            assert!(if n == 0 { dc.put(&mut out, *symbol) } else { ac.put(&mut out, *symbol) });
            out.put(*raw, u32::from(size));
        }
        let mcu = index / per_mcu + 1;
        if (index + 1) % per_mcu == 0 && mcu < mcus && restart.is_some_and(|n| mcu.is_multiple_of(n)) {
            out.align();
            out.marker(0xd0 + u8::try_from((mcu / restart.unwrap_or(1) - 1) % 8).unwrap());
        }
    }
    out.finish()
}

proptest! {
    #[test]
    fn random_blocks_coded_with_any_tables_come_out_the_same_with_the_standard_ones(
        blocks in proptest::collection::vec(block_strategy(), 24),
        flat in any::<bool>(),
        restart in proptest::option::of(1usize..4),
        four_two_two in any::<bool>(),
    ) {
        let per_mcu = if four_two_two { 4 } else { 6 };
        let mcus = blocks.len() / per_mcu;
        let own = (custom(false, flat), custom(true, flat));
        let data = write_blocks(&blocks[..mcus * per_mcu], &own, per_mcu, restart);
        let coding = || Coding { dc: own.0.clone(), ac: own.1.clone() };
        let scan = Scan {
            data: &data,
            blocks: [if four_two_two { 2 } else { 4 }, 1, 1],
            mcus,
            restart,
            coding: [coding(), coding(), coding()],
        };
        let recoded = to_standard(&scan, 1 << 24).unwrap();
        // Y keeps the luminance tables and the chroma gets the chrominance ones.
        let standard = Scan { data: &recoded, coding: standard_coding(), ..scan };
        let read = blocks_of(&standard).unwrap();
        prop_assert_eq!(&read[..], &blocks[..mcus * per_mcu]);
    }

    #[test]
    fn whatever_the_bytes_the_coder_stops_and_stays_within_its_limit(
        data in proptest::collection::vec(any::<u8>(), 0..300),
        restart in proptest::option::of(1usize..5),
    ) {
        let coding = || Coding { dc: custom(false, true), ac: custom(true, true) };
        let scan = Scan { data: &data, blocks: [4, 1, 1], mcus: 12, restart, coding: [coding(), coding(), coding()] };
        if let Ok(out) = to_standard(&scan, 4096) {
            prop_assert!(out.len() <= 4096 + 1);
        }
    }
}

#[test]
fn a_file_that_numbers_its_standard_tables_differently_is_sent_as_it_is() {
    // The chrominance tables as 0 and the luminance ones as 1, the scan
    // saying so: the same codes, so the same scan, and it used to be refused.
    let mut swapped = Build::new(64, 48, 100);
    swapped.huffman = vec![
        (0x00, Table::standard(false, true).dht_body()),
        (0x01, Table::standard(false, false).dht_body()),
        (0x10, Table::standard(true, true).dht_body()),
        (0x11, Table::standard(true, false).dht_body()),
    ];
    swapped.selectors = [0x11, 0x00, 0x00];
    let file = swapped.bytes();
    let jpeg = parse(&file).unwrap();
    assert!(matches!(jpeg.scan, std::borrow::Cow::Borrowed(_)));
    assert_eq!(&*jpeg.scan, &swapped.scan[..]);
}

#[test]
fn a_file_with_no_tables_is_the_standard_ones() {
    let mut avi = Build::new(64, 48, 100);
    avi.huffman.clear();
    let file = avi.bytes();
    let jpeg = parse(&file).unwrap();
    assert!(matches!(jpeg.scan, std::borrow::Cow::Borrowed(_)));
    // A scan that names a table nothing defines is not one.
    avi.selectors = [0x00, 0x22, 0x22];
    assert!(matches!(parse(&avi.bytes()), Err(JpegError::Malformed(_))));
}

#[test]
fn a_scan_that_names_the_components_in_another_order_is_refused() {
    let mut reversed = Build::new(64, 48, 100);
    reversed.scan_ids = [3, 2, 1];
    assert!(matches!(parse(&reversed.bytes()), Err(JpegError::Unsupported(_))));
}

#[test]
fn an_oversubscribed_table_is_malformed_not_a_loop() {
    let mut counts = [0u8; 16];
    counts[0] = 5;
    let mut body = counts.to_vec();
    body.extend([0, 1, 2, 3, 4]);
    let mut file = Build::new(64, 48, 100);
    file.huffman = vec![(0x00, body)];
    assert!(matches!(parse(&file.bytes()), Err(JpegError::Malformed(_))));
    // A table id out of range.
    let mut file = Build::new(64, 48, 100);
    file.huffman.push((0x04, Table::standard(false, false).dht_body()));
    assert!(matches!(parse(&file.bytes()), Err(JpegError::Malformed(_))));
}
