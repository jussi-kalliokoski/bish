// DEFLATE decompression (RFC 1951). Decompression only: nothing here
// produces compressed output.
//
// Structured like Mark Adler's `puff` (canonical Huffman decoded a bit at
// a time from the code-length counts) rather than zlib's tables: slower,
// but small enough to check against the RFC, and the files are scripts
// and configs. A multi-bit lookup table would be a change to
// `Huffman::decode` alone. Used by archive.rs (gzip and zip both wrap raw
// DEFLATE).

// How many bytes one call may decompress to. DEFLATE's ratio reaches
// about 1032:1 and gzip members concatenate, so a megabyte of input
// could ask for hundreds of gigabytes, and not only interactively:
// manpages.rs gunzips what `man -w` names on a background thread. 64
// MiB, as lsp.rs's limit, is far above any script, config or man page.
// Callers expecting less pass their own budget (manpages.rs).
pub const MAX_OUTPUT: usize = 64 * 1024 * 1024;

// Fixed by the RFC: codes of at most 15 bits, alphabets of at most 288
// and 30 symbols.
const MAX_BITS: usize = 15;
const MAX_LIT_CODES: usize = 288;
// 30 distance codes mean something (`DIST_BASE`), but the alphabet is
// 32 wide: the fixed code gives all 32 a 5-bit code, and HDIST may name
// 32. Codes 30 and 31 never appear in a valid stream;
// `compressed_block` rejects them.
const MAX_DIST_CODES: usize = 32;

// Length codes 257..=285: each one's base length and extra bits. 284
// with 5 extra bits reaches 227..=257, and 285 is exactly 258 with none,
// so they overlap at the top, as the RFC's table has it.
const LENGTH_BASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LENGTH_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];

const DIST_BASE: [u16; 30] =
    [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

// The order a dynamic header sends its code-length code lengths, the
// likely zeros last so HCLEN can omit them.
const CODE_LENGTH_ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

// DEFLATE packs bits least significant first within each byte, while a
// Huffman code's bits arrive most significant first, so
// `Huffman::decode` takes one bit at a time and shifts it in from the
// bottom.
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

    // Drops the partial byte, for a stored block, whose header and payload
    // are byte-aligned.
    fn align(&mut self) {
        let whole = self.count / 8;
        self.buf = 0;
        self.count = 0;
        // Whole bytes read into `buf` but unconsumed go back, or the stored
        // block would start mid-data.
        self.pos -= whole as usize;
    }
}

// A canonical Huffman code as decoding wants it: how many codes of each
// length, and the symbols in canonical order. The code values follow
// from the lengths, so they're never stored.
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
        // Every length zero decodes nothing, which is legal (an unused
        // distance alphabet) until something reads a symbol from it.
        if counts[0] as usize == lengths.len() {
            return Ok(Huffman { counts, symbols: Vec::new() });
        }
        // Kraft's inequality, as puff checks it: `left` counts unassigned
        // codes of the current length; negative means over-subscribed (two
        // symbols sharing a prefix), leftover at the end means incomplete,
        // also rejected, since decoding would run off the end of the code.
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
        // Where each length's symbols start in `symbols`, then the symbols in
        // symbol order, which is canonical order, since canonical codes go by
        // (length, symbol).
        let mut offsets = [0u16; MAX_BITS + 2];
        // `len` addresses two arrays at two offsets, so it's a range, not an
        // iterator over one.
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

    // Walks lengths shortest first, keeping `first` (this length's first
    // code) and `index` (where its symbols start) in step, until the code
    // read so far falls in the current length's range.
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

// Block type 01's alphabets, whose lengths are in the RFC, not the
// stream.
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
    // All 32: 30 five-bit codes would leave the code incomplete.
    let dist = [5u8; MAX_DIST_CODES];
    // Both complete by construction.
    (Huffman::build(&lit).unwrap(), Huffman::build(&dist).unwrap())
}

// Everything a raw DEFLATE stream decompresses to; zlib or gzip headers
// are the container's (archive.rs).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, String> {
    inflate_prefix(data, MAX_OUTPUT).map(|(out, _)| out)
}

// `inflate`, plus how many bytes of `data` the stream took, so a
// container can find what follows (gzip's trailer and maybe another
// member, see `archive::gunzip`); DEFLATE doesn't record its length.
// `max_out` is this stream's output budget: `MAX_OUTPUT`, or what's
// left of it across a sequence.
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
            // Whole bytes in the bit buffer but unconsumed belong to what
            // follows.
            return Ok((out, bits.pos - (bits.count / 8) as usize));
        }
    }
}

fn stored_block(bits: &mut Bits, out: &mut Vec<u8>, max_out: usize) -> Result<(), String> {
    bits.align();
    let header = bits.data.get(bits.pos..bits.pos + 4).ok_or("truncated stored block header")?;
    let len = u16::from_le_bytes([header[0], header[1]]) as usize;
    let nlen = u16::from_le_bytes([header[2], header[3]]);
    // NLEN is LEN's one's complement, the format's check against a garbled
    // header.
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

// A type-10 block's alphabets: a small alphabet of code lengths, then
// both alphabets' lengths in it, with 16/17/18 as run-length escapes.
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
            // 16 repeats the previous length; 17 and 18 are runs of zeros of
            // different reach.
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
    // A distance alphabet of one code is what encoders emit for a block
    // with no matches. `Huffman::build` calls it incomplete, so it passes as
    // the empty code; decoding a distance from it then fails, as it should
    // for a corrupt stream.
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

// The LZ77 loop for fixed and dynamic blocks: symbols below 256 are
// literal bytes, 256 ends the block, above is a (length, distance) back
// reference.
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
                // Checked before the copy: this is where a few input bytes
                // become up to 258 output bytes, where a bomb grows.
                if out.len() + len > max_out {
                    return Err(too_big(max_out));
                }
                // One byte at a time: an overlapping copy (distance < len,
                // a run of one byte) must see what this loop writes, which
                // `extend_from_within` and `copy_within` wouldn't.
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

// The CRC-32 gzip and zip carry, bit by bit rather than from a table:
// it checks one member after decompressing.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            // The reversed, LSB-first standard polynomial, as both formats
            // specify.
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    // Inputs come from a real compressor (python3's zlib), not hand-built
    // streams, which would only test the decoder against my reading of the
    // RFC. Skipped without python3; archive.rs's tests use real compressed
    // bytes without one.
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

    // A long run becomes overlapping back references (distance shorter
    // than length), which a bulk copy gets wrong.
    #[test]
    fn inflates_overlapping_back_references() {
        round_trip("ab".repeat(5000).as_bytes());
        round_trip(&vec![b'x'; 70000]);
    }

    // Distinct bytes with a skewed distribution, so the compressor emits a
    // dynamic-Huffman block.
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

    // The ceiling: 4 MiB of one byte compresses to a few kilobytes, which
    // would ask an unlimited decompressor for gigabytes if repeated.
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
        // The refusal must come from the back-reference path, so the budget
        // isn't below the first literal.
        assert!(inflate_prefix(&compressed, 0).is_err());
    }

    // Stored blocks can't be bombs, but they're a second way into `out`,
    // and the ceiling holds there too.
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

    // Incompressible input makes the compressor use stored blocks.
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

    // The RFC's published check value for "123456789".
    #[test]
    fn crc32_matches_the_known_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }
}
