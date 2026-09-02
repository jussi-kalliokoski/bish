// DEFLATE decompression (RFC 1951), hand-rolled -- no external crate,
// same spirit as glob.rs/regex.rs/json.rs/csscolor.rs. Decompression
// only: nothing in this codebase produces compressed output, and a
// compressor is a genuinely separate (and much larger) problem --
// Huffman *construction*, match finding, block splitting -- with no
// caller here to justify it.
//
// The decoder follows Mark Adler's own `puff` reference structure
// (canonical Huffman decoded one bit at a time, walking the code-length
// counts) rather than the table-driven approach zlib itself ships. That
// costs speed and buys a decoder small enough to read in one sitting and
// check against the RFC line by line -- the right trade for what this is
// for (opening an archive in the editor), where the files are a script
// or a config, not a filesystem image. If a real performance need ever
// shows up it looks like noticeable lag opening one specific large
// member, at which point a multi-bit lookup table is a self-contained
// change to `Huffman::decode` alone.
//
// Callers: archive.rs (gzip and zip both frame a raw DEFLATE stream).

// How many bytes one call may decompress to. DEFLATE's own maximum
// ratio is about 1032:1 and gzip members concatenate (archive::gunzip
// loops over them), so without a ceiling a megabyte of input can ask
// for hundreds of gigabytes of memory -- and the paths that get here
// are not all interactive: manpages.rs gunzips whatever `man -w` points
// at, on a background thread, while you type at the prompt. Same
// precedent as lsp.rs's MAX_CONTENT_LENGTH: refuse rather than trust
// the size a stranger's file asks for.
//
// 64 MiB, the same number lsp.rs already picked for the same kind of
// question, is far past anything this decoder exists for (a script, a
// config, a man page) and far below anything that hurts. A caller with
// a tighter idea of what it is reading passes its own budget instead --
// see manpages.rs, whose reads are not user-initiated at all.
pub const MAX_OUTPUT: usize = 64 * 1024 * 1024;

// A code is at most 15 bits, and the two alphabets are at most 288/30
// symbols -- all three fixed by the RFC, not tuning knobs.
const MAX_BITS: usize = 15;
const MAX_LIT_CODES: usize = 288;
// 30 distance codes carry meaning (DIST_BASE below), but the alphabet is
// 32 symbols wide: the fixed code assigns all 32 a 5-bit code, and HDIST
// may legally announce up to 32. Codes 30 and 31 simply never appear in
// a valid stream, which compressed_block checks for when one does.
const MAX_DIST_CODES: usize = 32;

// Length codes 257..=285: the base length each stands for, and how many
// extra bits follow it. Code 284's own 5 extra bits can encode 227..=257,
// but 285 is defined as exactly 258 with no extra bits, so the two
// overlap at the top -- that's the RFC's own table, not a mistake here.
const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

const DIST_BASE: [u16; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

// The order the dynamic-block header sends its code-length code lengths
// in -- front-loaded so the ones most likely to be zero end up last and
// can be omitted entirely (that's what HCLEN counts).
const CODE_LENGTH_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

// DEFLATE reads its bits least-significant-first *within* each byte,
// while a Huffman code's own bits arrive most-significant-first -- the
// one genuinely confusing thing about the format, and the reason
// `Huffman::decode` pulls one bit at a time and shifts it in from the
// bottom rather than grabbing a whole code at once.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    // Bits pulled out of `data` but not yet consumed, low bit first.
    buf: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0, buf: 0, count: 0 }
    }

    fn take(&mut self, need: u32) -> Result<u32, String> {
        while self.count < need {
            let byte = *self.data.get(self.pos).ok_or("unexpected end of compressed data")?;
            self.pos += 1;
            self.buf |= (byte as u32) << self.count;
            self.count += 8;
        }
        let value = self.buf & ((1u32 << need) - 1);
        self.buf >>= need;
        self.count -= need;
        Ok(value)
    }

    // Drops the partial byte, for a stored block (whose LEN/NLEN and
    // payload are byte-aligned).
    fn align(&mut self) {
        let whole = self.count / 8;
        self.buf = 0;
        self.count = 0;
        // Bytes already read into `buf` that were never consumed have to
        // go back, or the stored block would start mid-way through the
        // data.
        self.pos -= whole as usize;
    }
}

// A canonical Huffman code, stored the way decoding actually wants it:
// how many codes there are of each bit length, and the symbols in
// canonical order. That's all the shape a canonical code has -- the code
// *values* themselves are implied by the lengths, so they're never
// materialized.
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    // `lengths[sym]` is sym's code length, 0 meaning "not present".
    fn build(lengths: &[u8]) -> Result<Huffman, String> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &len in lengths {
            if len as usize > MAX_BITS {
                return Err("invalid Huffman code length".to_string());
            }
            counts[len as usize] += 1;
        }
        // A code with every length zero decodes nothing; that's legal
        // (an unused distance alphabet, for one) and only an error if
        // something later actually tries to read a symbol from it.
        if counts[0] as usize == lengths.len() {
            return Ok(Huffman { counts, symbols: Vec::new() });
        }
        // Kraft's inequality, checked the way puff does: `left` is how
        // many codes of the current length are still unassigned, and it
        // going negative means the code is over-subscribed (two symbols
        // sharing a prefix). Left over at the end means incomplete --
        // rejected too, since a decoder that ran off the end of an
        // incomplete code would read whatever came next as a symbol.
        let mut left = 1i32;
        #[allow(clippy::needless_range_loop, reason = "`len` is a code length; the range is the alphabet, not `counts`")]
        for len in 1..=MAX_BITS {
            left <<= 1;
            left -= counts[len] as i32;
            if left < 0 {
                return Err("over-subscribed Huffman code".to_string());
            }
        }
        if left > 0 {
            return Err("incomplete Huffman code".to_string());
        }
        // Where each length's symbols start inside `symbols`, then the
        // symbols filled in in symbol order -- which *is* canonical
        // order, since canonical codes are assigned in increasing
        // (length, symbol) order.
        let mut offsets = [0u16; MAX_BITS + 2];
        // `len` is a code length, not just an index -- it addresses two
        // different arrays at two different offsets -- so iterating the
        // slice instead would obscure exactly the thing that matters.
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbols[offsets[len as usize] as usize] = sym as u16;
                offsets[len as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    // Walks lengths shortest-first, keeping `first` (the first code
    // value of this length) and `index` (where this length's symbols
    // start) in step, until the code read so far falls inside the
    // current length's range.
    fn decode(&self, bits: &mut Bits) -> Result<u16, String> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=MAX_BITS {
            code |= bits.take(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("invalid Huffman code in compressed data".to_string())
    }
}

// The block type 01 alphabets, which every stream can use without
// sending a header for them -- their lengths are written into the RFC
// itself rather than into the file.
fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u8; MAX_LIT_CODES];
    for (sym, len) in lit.iter_mut().enumerate() {
        *len = match sym {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    // All 32, not just the 30 meaningful ones -- 30 five-bit codes would
    // leave two of the 32 five-bit slots unused, which is an incomplete
    // code and rightly rejected.
    let dist = [5u8; MAX_DIST_CODES];
    // Both are complete by construction, so the unwraps can't fire.
    (Huffman::build(&lit).unwrap(), Huffman::build(&dist).unwrap())
}

// Every byte `data` decompresses to. `data` is a *raw* DEFLATE stream:
// no zlib header, no gzip header -- peeling those is the container's job
// (archive.rs).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    inflate_prefix(data, MAX_OUTPUT).map(|(out, _)| out)
}

// `inflate`, plus how many bytes of `data` the stream actually occupied
// -- what a container needs to find whatever follows it (gzip's own
// CRC/length trailer, and then possibly another member: see
// archive::gunzip). A DEFLATE stream doesn't announce its own length,
// so this is the only way to know.
//
// `max_out` is how many bytes this stream may produce -- MAX_OUTPUT for
// a caller decompressing one thing, and whatever is left of that budget
// for a caller walking a sequence of them.
pub fn inflate_prefix(data: &[u8], max_out: usize) -> Result<(Vec<u8>, usize), String> {
    let mut bits = Bits::new(data);
    let mut out = Vec::new();
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => stored_block(&mut bits, &mut out, max_out)?,
            1 => {
                let (lit, dist) = fixed_tables();
                compressed_block(&mut bits, &mut out, &lit, &dist, max_out)?;
            }
            2 => {
                let (lit, dist) = dynamic_tables(&mut bits)?;
                compressed_block(&mut bits, &mut out, &lit, &dist, max_out)?;
            }
            _ => return Err("invalid DEFLATE block type".to_string()),
        }
        if last {
            // Whole bytes pulled into the bit buffer but never consumed
            // belong to whatever comes next, not to this stream.
            return Ok((out, bits.pos - (bits.count / 8) as usize));
        }
    }
}

fn stored_block(bits: &mut Bits, out: &mut Vec<u8>, max_out: usize) -> Result<(), String> {
    bits.align();
    let header = bits.data.get(bits.pos..bits.pos + 4).ok_or("truncated stored block header")?;
    let len = u16::from_le_bytes([header[0], header[1]]) as usize;
    let nlen = u16::from_le_bytes([header[2], header[3]]);
    // NLEN is LEN's one's complement -- the format's own check that the
    // header wasn't garbled, cheap enough to honour.
    if nlen != !(len as u16) {
        return Err("stored block length check failed".to_string());
    }
    bits.pos += 4;
    let body = bits.data.get(bits.pos..bits.pos + len).ok_or("truncated stored block")?;
    if out.len() + len > max_out {
        return Err(too_big(max_out));
    }
    out.extend_from_slice(body);
    bits.pos += len;
    Ok(())
}

// The literal/length and distance alphabets a type-10 block sends ahead
// of itself: first a small alphabet describing code *lengths*, then both
// real alphabets' lengths encoded with it (16/17/18 being run-length
// escapes, which is what makes this compact enough to be worth sending).
fn dynamic_tables(bits: &mut Bits) -> Result<(Huffman, Huffman), String> {
    let nlen = bits.take(5)? as usize + 257;
    let ndist = bits.take(5)? as usize + 1;
    let ncode = bits.take(4)? as usize + 4;
    if nlen > MAX_LIT_CODES || ndist > MAX_DIST_CODES {
        return Err("too many DEFLATE codes".to_string());
    }
    let mut code_lengths = [0u8; 19];
    for &slot in CODE_LENGTH_ORDER.iter().take(ncode) {
        code_lengths[slot] = bits.take(3)? as u8;
    }
    let code_table = Huffman::build(&code_lengths)?;

    let mut lengths = vec![0u8; nlen + ndist];
    let mut i = 0;
    while i < lengths.len() {
        let sym = code_table.decode(bits)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            // 16 repeats the *previous* length; 17 and 18 are runs of
            // zeros, differing only in how long a run they can express.
            16 => {
                let prev = *lengths.get(i.wrapping_sub(1)).ok_or("DEFLATE length repeat with no previous length")?;
                let run = 3 + bits.take(2)? as usize;
                fill(&mut lengths, &mut i, prev, run)?;
            }
            17 => {
                let run = 3 + bits.take(3)? as usize;
                fill(&mut lengths, &mut i, 0, run)?;
            }
            18 => {
                let run = 11 + bits.take(7)? as usize;
                fill(&mut lengths, &mut i, 0, run)?;
            }
            _ => return Err("invalid code-length symbol".to_string()),
        }
    }
    let lit = Huffman::build(&lengths[..nlen])?;
    // A distance alphabet with exactly one code is what an encoder emits
    // for a block with no matches at all; Huffman::build calls that
    // incomplete, so allow it through as the empty code it effectively
    // is -- any actual attempt to decode a distance from it then fails
    // on its own, which is the correct outcome for a corrupt stream.
    let dist_lengths = &lengths[nlen..];
    let dist = match Huffman::build(dist_lengths) {
        Ok(h) => h,
        Err(e) if dist_lengths.iter().filter(|&&l| l != 0).count() <= 1 => {
            let _ = e;
            Huffman { counts: [0; MAX_BITS + 1], symbols: Vec::new() }
        }
        Err(e) => return Err(e),
    };
    Ok((lit, dist))
}

fn fill(lengths: &mut [u8], i: &mut usize, value: u8, run: usize) -> Result<(), String> {
    if *i + run > lengths.len() {
        return Err("DEFLATE code-length run overflows the alphabet".to_string());
    }
    for _ in 0..run {
        lengths[*i] = value;
        *i += 1;
    }
    Ok(())
}

// The actual LZ77 loop, identical for fixed and dynamic blocks once the
// two alphabets are in hand: symbols under 256 are literal bytes, 256
// ends the block, and anything above is a (length, distance) back
// reference into what's already been decompressed.
fn compressed_block(bits: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, max_out: usize) -> Result<(), String> {
    loop {
        let sym = lit.decode(bits)?;
        match sym {
            0..=255 => {
                if out.len() == max_out {
                    return Err(too_big(max_out));
                }
                out.push(sym as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let idx = sym as usize - 257;
                let len = LENGTH_BASE[idx] as usize + bits.take(LENGTH_EXTRA[idx] as u32)? as usize;
                let dsym = dist.decode(bits)? as usize;
                if dsym >= DIST_BASE.len() {
                    return Err("invalid distance code".to_string());
                }
                let distance = DIST_BASE[dsym] as usize + bits.take(DIST_EXTRA[dsym] as u32)? as usize;
                if distance > out.len() {
                    return Err("back reference before the start of the output".to_string());
                }
                // Checked before the copy rather than inside it: this is
                // the one place a few bytes of input turn into up to 258
                // of output, so it is where a bomb actually grows.
                if out.len() + len > max_out {
                    return Err(too_big(max_out));
                }
                // Copied one byte at a time on purpose: an overlapping
                // copy (distance < len, which is how a run of the same
                // byte is encoded) has to see the bytes this very loop
                // is writing. extend_from_within/copy_within would both
                // read the pre-copy state and get it wrong.
                let start = out.len() - distance;
                for i in 0..len {
                    let byte = out[start + i];
                    out.push(byte);
                }
            }
            _ => return Err("invalid literal/length code".to_string()),
        }
    }
}

fn too_big(max_out: usize) -> String {
    format!("decompressed size exceeds the {max_out}-byte limit")
}

// The CRC-32 gzip and zip both carry, computed bit by bit rather than
// from a 256-entry table -- the table is a speed optimization for
// streaming a whole disk, and this only ever checks one member's worth
// of bytes right after decompressing it.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            // The reversed (LSB-first) form of the standard polynomial,
            // which is the form both formats specify.
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    // Every input below is compressed by a real compressor (python3's
    // zlib) rather than hand-assembled: a bit stream written by hand
    // would only ever prove this decoder agrees with my own reading of
    // the RFC, which is precisely the thing under test. Skipped, not
    // failed, where there's no python3 -- the same "quietly unavailable,
    // not a hard dependency" contract the git tests already use, and
    // archive.rs's own tests cover real compressed bytes with no
    // external program at all.
    fn compressor_available() -> bool {
        std::process::Command::new("python3")
            .arg("-c")
            .arg("import zlib")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn deflate_raw(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        use std::process::Command;
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import sys,zlib;c=zlib.compressobj(9,zlib.DEFLATED,-15);sys.stdout.buffer.write(c.compress(sys.stdin.buffer.read())+c.flush())",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 is needed for the inflate round-trip tests");
        child.stdin.take().unwrap().write_all(data).unwrap();
        child.wait_with_output().unwrap().stdout
    }

    fn round_trip(data: &[u8]) {
        if !compressor_available() {
            return;
        }
        let compressed = deflate_raw(data);
        assert_eq!(inflate(&compressed).unwrap(), data, "round trip of {} bytes", data.len());
    }

    #[test]
    fn inflates_an_empty_stream() {
        round_trip(b"");
    }

    #[test]
    fn inflates_short_text() {
        round_trip(b"hello, deflate\n");
    }

    // A long run compresses into overlapping back references (distance
    // smaller than length), the one case a bulk copy gets wrong.
    #[test]
    fn inflates_overlapping_back_references() {
        round_trip("ab".repeat(5000).as_bytes());
        round_trip(&vec![b'x'; 70000]);
    }

    // Enough distinct bytes with a skewed distribution to make a real
    // compressor emit a dynamic-Huffman block rather than a stored or
    // fixed one.
    #[test]
    fn inflates_a_dynamic_huffman_block() {
        let mut data = Vec::new();
        for i in 0..20000u32 {
            data.push((i % 7) as u8 + b'a');
            if i % 13 == 0 {
                data.extend_from_slice(b" the quick brown fox ");
            }
        }
        round_trip(&data);
    }

    // The whole point of the ceiling: a compressor turns 4 MiB of one
    // repeated byte into a few kilobytes, and a decompressor with no
    // limit will happily be told to produce hundreds of gigabytes by a
    // file that costs nothing to send.
    #[test]
    fn refuses_to_decompress_past_the_ceiling() {
        if !compressor_available() {
            return;
        }
        let compressed = deflate_raw(&vec![0u8; 4 << 20]);
        assert!(compressed.len() < 64 << 10, "{} bytes in, 4 MiB out -- that ratio is the threat", compressed.len());

        let (out, _) = inflate_prefix(&compressed, 8 << 20).unwrap();
        assert_eq!(out.len(), 4 << 20, "a budget it fits inside decompresses in full");

        let err = inflate_prefix(&compressed, 1 << 20).unwrap_err();
        assert!(err.contains("exceeds"), "and one it does not is refused, not attempted: {err}");
        // The refusal has to come from the back-reference path -- that
        // is where a bomb grows -- so a budget under even the first
        // literal byte is not what is being tested here.
        assert!(inflate_prefix(&compressed, 0).is_err());
    }

    // Stored blocks carry their bytes literally, so they can never be a
    // bomb -- but they are a second way into `out`, and the ceiling has
    // to hold on that path too.
    #[test]
    fn the_ceiling_holds_on_the_stored_block_path() {
        if !compressor_available() {
            return;
        }
        let mut data = Vec::new();
        for i in 0..40000u32 {
            data.push((i.wrapping_mul(2654435761) >> 24) as u8);
        }
        let compressed = deflate_raw(&data);
        assert_eq!(inflate_prefix(&compressed, data.len()).unwrap().0, data);
        assert!(inflate_prefix(&compressed, data.len() - 1).is_err());
    }

    // Incompressible input is what makes a compressor fall back to
    // stored blocks, which take the byte-aligned path.
    #[test]
    fn inflates_stored_blocks() {
        let mut data = Vec::new();
        let mut x = 12345u32;
        for _ in 0..60000 {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            data.push((x >> 16) as u8);
        }
        round_trip(&data);
    }

    #[test]
    fn inflates_every_byte_value() {
        let data: Vec<u8> = (0..=255u8).cycle().take(9000).collect();
        round_trip(&data);
    }

    #[test]
    fn rejects_truncated_input_instead_of_looping_or_panicking() {
        if !compressor_available() {
            return;
        }
        let compressed = deflate_raw(&b"some text worth compressing, repeated. ".repeat(50));
        for cut in [0, 1, 2, compressed.len() / 2, compressed.len() - 1] {
            assert!(inflate(&compressed[..cut]).is_err(), "truncation to {cut} bytes must be an error");
        }
    }

    #[test]
    fn rejects_an_invalid_block_type() {
        // BFINAL=1, BTYPE=11 (reserved) in the first byte's low bits.
        assert!(inflate(&[0b111]).is_err());
    }

    // Against the RFC's own published check value for "123456789".
    #[test]
    fn crc32_matches_the_known_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
