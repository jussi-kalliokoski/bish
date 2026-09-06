// What a file's bytes say, when they do not say UTF-8.
//
// The editor read every file with `read_to_string`, so a latin-1 file
// was not a file with an accent in it -- it was an error, and one you
// could not get past. That also made `.editorconfig`'s `charset` a
// property this shell parsed and then refused, because honouring it
// would have meant writing back an encoding it could not read.
//
// The set here is exactly the set `.editorconfig` names -- `latin1`,
// `utf-8`, `utf-8-bom`, `utf-16be`, `utf-16le` -- which is also what
// vim's default `fileencodings` finds in practice. Anything else needs
// a table this shell would have to carry, and a guess about which one:
// a byte in the 0x80..0xff range is a different character in every
// single-byte encoding there is, and nothing in the file says which.
// So the fallback is latin-1, which is the one whose table *is* the
// byte value, and the one vim falls back to for the same reason.
//
// Detection is vim's own `ucs-bom,utf-8,default,latin1` in the same
// order: a BOM decides it outright, then UTF-8 if the bytes are valid
// UTF-8, then latin-1, which cannot fail. That last step is why an
// encoding is *detected* rather than *guessed at*: every byte sequence
// decodes as latin-1, so opening a file never fails on its contents.
//
// What matters as much as reading is writing the same thing back. A
// buffer remembers what it was decoded from and `save` encodes with it,
// so a latin-1 file stays latin-1 and a file that had a BOM keeps it.
// Where that cannot be done -- a character that latin-1 has no byte for
// -- the save fails and says which character, rather than writing a `?`
// in its place. A file quietly mangled on save is the failure this
// module exists to make impossible.
#![allow(dead_code)]

/// The encodings this editor can read and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    /// ISO-8859-1, where the byte *is* the code point. Also the
    /// fallback for bytes that are not valid UTF-8, since it is the one
    /// encoding under which every byte sequence means something.
    Latin1,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    /// The `.editorconfig` spelling, which is also what this shell
    /// shows the user.
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Latin1 => "latin1",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
        }
    }

    /// A UTF-16 file without a BOM is a file nothing can identify --
    /// its bytes are valid latin-1 and will be read back as latin-1, by
    /// this editor and by vim both. So writing one is refused: the BOM
    /// rides along whatever the buffer's own `bom` flag says.
    pub fn requires_bom(self) -> bool {
        matches!(self, Encoding::Utf16Le | Encoding::Utf16Be)
    }
}

/// `.editorconfig`'s `charset` values, plus the aliases that mean the
/// same thing to vim. `None` for a name this shell cannot honour --
/// which is a refusal, and stays one, rather than a silent fallback to
/// UTF-8.
pub fn parse(name: &str) -> Option<Encoding> {
    match name.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "utf-8" | "utf8" => Some(Encoding::Utf8),
        "utf-8-bom" | "utf8-bom" => Some(Encoding::Utf8),
        "latin1" | "latin-1" | "iso-8859-1" | "iso8859-1" => Some(Encoding::Latin1),
        "utf-16le" | "utf16le" | "ucs-2le" => Some(Encoding::Utf16Le),
        "utf-16be" | "utf16be" | "ucs-2be" => Some(Encoding::Utf16Be),
        _ => None,
    }
}

/// Whether a `charset` name asks for a BOM in its own right.
/// `utf-8-bom` is the only one that does; the UTF-16 pair get theirs
/// from `requires_bom`.
pub fn names_a_bom(name: &str) -> bool {
    matches!(name.trim().to_ascii_lowercase().as_str(), "utf-8-bom" | "utf8-bom")
}

/// A file's text, and what it took to read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
    /// Whether the bytes began with a byte-order mark. Remembered
    /// separately from the encoding because a UTF-8 file with one and a
    /// UTF-8 file without one are the same encoding and different
    /// files.
    pub bom: bool,
}

const UTF8_BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

/// Reads `bytes` as text, working out how, and never failing.
pub fn decode(bytes: &[u8]) -> Decoded {
    if bytes.starts_with(&UTF8_BOM) {
        return Decoded { text: from_utf8_lossy_owned(&bytes[3..]), encoding: Encoding::Utf8, bom: true };
    }
    // 0xff 0xfe 0x00 0x00 is UTF-32LE, which this editor does not read
    // -- but it is also a valid UTF-16LE `U+FEFF` followed by `U+0000`,
    // and reading it as that puts a NUL in the buffer rather than
    // failing. Not distinguished, because a UTF-32 file is not a thing
    // this shell claims to open.
    if bytes.starts_with(&[0xff, 0xfe]) {
        return Decoded { text: from_utf16(&bytes[2..], true), encoding: Encoding::Utf16Le, bom: true };
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return Decoded { text: from_utf16(&bytes[2..], false), encoding: Encoding::Utf16Be, bom: true };
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Decoded { text: text.to_string(), encoding: Encoding::Utf8, bom: false },
        // Every byte is a character, so this is where detection stops.
        Err(_) => Decoded { text: bytes.iter().map(|&b| b as char).collect(), encoding: Encoding::Latin1, bom: false },
    }
}

/// The character `encode` could not write, and what it could not write
/// it as. Carries the character itself because "this file has something
/// latin-1 cannot say" is not actionable and "line 3 has an em dash" is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unrepresentable {
    pub ch: char,
    pub encoding: Encoding,
}

impl std::fmt::Display for Unrepresentable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "U+{:04X} ({}) cannot be written as {}", self.ch as u32, self.ch, self.encoding.name())
    }
}

/// Turns text back into a file's bytes.
///
/// Fails rather than substituting: latin-1 has 256 characters and text
/// that has left them is text this cannot write, so the save stops and
/// the file on disk stays as it was.
pub fn encode(text: &str, encoding: Encoding, bom: bool) -> Result<Vec<u8>, Unrepresentable> {
    let bom = bom || encoding.requires_bom();
    let mut out = Vec::with_capacity(text.len() + 3);
    match encoding {
        Encoding::Utf8 => {
            if bom {
                out.extend_from_slice(&UTF8_BOM);
            }
            out.extend_from_slice(text.as_bytes());
        }
        Encoding::Latin1 => {
            for ch in text.chars() {
                match u8::try_from(ch as u32) {
                    Ok(b) => out.push(b),
                    Err(_) => return Err(Unrepresentable { ch, encoding }),
                }
            }
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let big_endian = encoding == Encoding::Utf16Be;
            if bom {
                push16(&mut out, 0xfeff, big_endian);
            }
            for unit in text.encode_utf16() {
                push16(&mut out, unit, big_endian);
            }
        }
    }
    Ok(out)
}

fn push16(out: &mut Vec<u8>, unit: u16, big_endian: bool) {
    let bytes = if big_endian { unit.to_be_bytes() } else { unit.to_le_bytes() };
    out.extend_from_slice(&bytes);
}

/// An odd trailing byte is dropped rather than made into a character:
/// half a code unit is a truncated file, and inventing the other half
/// would put a character in the buffer the file does not have.
fn from_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|p| if little_endian { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) }).collect();
    String::from_utf16_lossy(&units)
}

/// Bytes that carried a UTF-8 BOM and then were not UTF-8 after all.
/// The BOM is the file saying what it is, so it is taken at its word
/// and the bad bytes become replacement characters, rather than the
/// whole file being re-read as latin-1 against its own declaration.
fn from_utf8_lossy_owned(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_is_utf8_without_a_bom() {
        let d = decode(b"hello\n");
        assert_eq!(d, Decoded { text: "hello\n".to_string(), encoding: Encoding::Utf8, bom: false });
    }

    #[test]
    fn a_utf8_bom_is_recognised_and_not_left_in_the_text() {
        let d = decode(b"\xef\xbb\xbfhello");
        assert_eq!(d.text, "hello", "the mark is not a character in the file");
        assert_eq!((d.encoding, d.bom), (Encoding::Utf8, true));
    }

    // The byte that decided it: 0xe9 is `é` in latin-1 and is not a
    // valid UTF-8 sequence on its own, so a file containing one used to
    // be a file the editor could not open.
    #[test]
    fn bytes_that_are_not_utf8_are_read_as_latin1() {
        let d = decode(b"caf\xe9\n");
        assert_eq!(d.text, "café\n");
        assert_eq!((d.encoding, d.bom), (Encoding::Latin1, false));
    }

    #[test]
    fn utf16_is_read_from_either_end() {
        let le = decode(b"\xff\xfeh\x00i\x00");
        assert_eq!(le.text, "hi");
        assert_eq!((le.encoding, le.bom), (Encoding::Utf16Le, true));
        let be = decode(b"\xfe\xff\x00h\x00i");
        assert_eq!(be.text, "hi");
        assert_eq!((be.encoding, be.bom), (Encoding::Utf16Be, true));
    }

    #[test]
    fn utf16_reaches_past_the_basic_plane() {
        // U+1F600, which is a surrogate pair.
        let bytes = encode("\u{1f600}", Encoding::Utf16Le, true).unwrap();
        assert_eq!(bytes, b"\xff\xfe\x3d\xd8\x00\xde");
        assert_eq!(decode(&bytes).text, "\u{1f600}");
    }

    #[test]
    fn an_odd_trailing_byte_is_dropped_rather_than_invented() {
        assert_eq!(decode(b"\xff\xfeh\x00i").text, "h");
    }

    #[test]
    fn every_encoding_round_trips_its_own_text() {
        for (encoding, text) in [
            (Encoding::Utf8, "aé\u{1f600}\n"),
            (Encoding::Latin1, "café\n"),
            (Encoding::Utf16Le, "aé\u{1f600}\n"),
            (Encoding::Utf16Be, "aé\u{1f600}\n"),
        ] {
            let bytes = encode(text, encoding, false).unwrap();
            let back = decode(&bytes);
            assert_eq!(back.text, text, "{}", encoding.name());
            assert_eq!(back.encoding, encoding, "{}", encoding.name());
        }
    }

    // Latin-1 without a BOM is not identifiable, so this one is the
    // exception: it round-trips as text and is *detected* as latin-1
    // only because it is not valid UTF-8. Text that stays inside ASCII
    // reads back as UTF-8, which is the same string.
    #[test]
    fn ascii_written_as_latin1_reads_back_as_utf8() {
        let bytes = encode("plain\n", Encoding::Latin1, false).unwrap();
        let back = decode(&bytes);
        assert_eq!(back.text, "plain\n");
        assert_eq!(back.encoding, Encoding::Utf8, "the same bytes, and nothing in them says otherwise");
    }

    #[test]
    fn a_character_latin1_has_no_byte_for_is_refused_by_name() {
        let err = encode("an em dash \u{2014} here", Encoding::Latin1, false).unwrap_err();
        assert_eq!(err.ch, '\u{2014}');
        assert_eq!(err.to_string(), "U+2014 (—) cannot be written as latin1");
    }

    #[test]
    fn utf16_carries_a_mark_whether_or_not_it_was_asked_to() {
        assert!(encode("hi", Encoding::Utf16Le, false).unwrap().starts_with(&[0xff, 0xfe]));
        assert!(encode("hi", Encoding::Utf16Be, false).unwrap().starts_with(&[0xfe, 0xff]));
        assert!(!encode("hi", Encoding::Utf8, false).unwrap().starts_with(&UTF8_BOM));
    }

    #[test]
    fn charset_names_are_the_editorconfig_ones() {
        assert_eq!(parse("latin1"), Some(Encoding::Latin1));
        assert_eq!(parse("UTF-8"), Some(Encoding::Utf8));
        assert_eq!(parse("utf-8-bom"), Some(Encoding::Utf8));
        assert!(names_a_bom("utf-8-bom") && !names_a_bom("utf-8"));
        assert_eq!(parse("utf-16le"), Some(Encoding::Utf16Le));
        assert_eq!(parse("shift_jis"), None, "a name this cannot honour is refused, not defaulted");
    }
}
