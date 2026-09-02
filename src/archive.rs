// zip and gzip, hand-rolled on top of inflate.rs -- reading only. What
// this is for: `e some.zip` browses the archive like a directory, and
// `e some.txt.gz` opens the decompressed text as a read-only buffer, so
// looking inside a compressed thing doesn't mean leaving the editor to
// unpack it first.
//
// Read-only is not a limitation waiting to be lifted, it's the design.
// Writing into either format means *compressing*, which is a much larger
// problem than inflate.rs solves (see its own module comment), and
// rewriting a zip in place means rebuilding its central directory around
// an edit. Both buffers this feeds are opened readonly for that reason,
// not as a placeholder.
//
// Deliberately out of scope, each reported as a plain error rather than
// half-handled: zip64 (archives past 4GiB or 65535 members), encrypted
// entries, and compression methods other than store/deflate. tar --
// which is what a `.tar.gz` actually contains once gunzipped -- is a
// separate container this doesn't parse yet; see plan.md.

use std::path::{Path, PathBuf};

use crate::inflate;

// What `!` separates in a virtual path: the archive on disk from the
// member inside it, `/a/b.zip!/dir/file.txt`, with `/a/b.zip!` naming
// the archive's own root. Same spelling Java uses for a jar URL, and
// picked over inventing a scheme prefix because a virtual path has to
// survive being passed around as an ordinary string -- through
// EditTarget::path, browser::Entry::path and the editor's own status
// line -- with no type to carry the distinction.
//
// A literal `!` in a real filename can't be mistaken for this: `split`
// only treats one as a separator when everything before it is a file
// that really is an archive, checked by reading its magic bytes.
pub const SEPARATOR: char = '!';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Gzip,
    Tar,
}

// What `path` actually is, by its first bytes rather than its name -- a
// `.zip` that isn't one shouldn't open as an archive, and the `.tgz`/
// `.z`/no-extension-at-all cases should still work. Cheap enough to call
// on any path the user names (one open, one 4-byte read) and `None` for
// anything unreadable, since "can't tell" and "not an archive" lead to
// exactly the same place.
pub fn kind_of(path: &Path) -> Option<Kind> {
    use std::io::Read;
    if !path.is_file() {
        return None;
    }
    let mut head = [0u8; 4];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut head).ok()?;
    match &head[..read] {
        // 1f 8b is gzip's own magic; the third byte is the compression
        // method, and 8 (deflate) is the only one ever defined.
        [0x1f, 0x8b, ..] => Some(Kind::Gzip),
        // "PK" then either a local file header (03 04) or, for an
        // archive with no members at all, the end-of-central-directory
        // record on its own (05 06). "PK\x07\x08" (a spanned archive)
        // deliberately isn't accepted -- there'd be no other volume to
        // read.
        [b'P', b'K', 3, 4] | [b'P', b'K', 5, 6] => Some(Kind::Zip),
        // Tar has no header at all -- its "magic" is `ustar` 257 bytes
        // in, which is why this needs a second, longer look rather than
        // another arm above.
        _ => is_tar_file(path).then_some(Kind::Tar),
    }
}

// Whether a file's bytes at offset 257 say `ustar`. A pre-POSIX tar has
// nothing there and is not recognized: without the magic there is no way
// to tell one from an arbitrary file whose 257th byte happens to be
// interesting, and guessing wrong means showing someone a binary as a
// directory.
fn is_tar_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; TAR_BLOCK];
    let Ok(read) = file.read(&mut head) else { return false };
    read >= 265 && is_tar(&head)
}

const TAR_BLOCK: usize = 512;

// The same test against bytes already in hand -- which is how a
// `.tar.gz` is recognized: its *file* is a gzip, and only once inflated
// can anything tell whether what is inside is a tar or an ordinary
// file. That is the whole reason a doubly-wrapped archive needs no
// second `!` in its path: `x.tar.gz!/dir/f` is one archive, unwrapped
// twice on the way in.
fn is_tar(data: &[u8]) -> bool {
    data.len() >= 265 && &data[257..262] == b"ustar"
}

// Splits a virtual path into the archive and the member path inside it
// ("" for the archive's own root). `None` for an ordinary path.
//
// Scans left to right and takes the first `!` whose prefix is a real
// archive, so a directory called `weird!` on the way to a real archive
// doesn't win, and a plain file called `notes!.txt` is never split at
// all. Nested archives (a zip inside a zip) aren't supported, which is
// also why the *first* match is the right one rather than the last.
pub fn split(path: &str) -> Option<(PathBuf, String)> {
    for (i, _) in path.char_indices().filter(|(_, c)| *c == SEPARATOR) {
        let archive = Path::new(&path[..i]);
        if kind_of(archive).is_some() {
            let inner = path[i + 1..].trim_start_matches('/').trim_end_matches('/');
            return Some((archive.to_path_buf(), inner.to_string()));
        }
    }
    None
}

// The virtual path for `inner` inside `archive` -- the inverse of
// `split`, and the only thing that should ever build one, so the
// spelling stays in one place.
pub fn join(archive: &Path, inner: &str) -> String {
    let inner = inner.trim_matches('/');
    if inner.is_empty() { format!("{}{SEPARATOR}", archive.display()) } else { format!("{}{SEPARATOR}/{inner}", archive.display()) }
}

// Whether the file browser can list this path: a zip archive on disk, or
// a directory inside one. The one question `e SOMETHING` has to answer
// before deciding between "browse it" and "open it as a buffer", and the
// counterpart to `Path::is_dir` for virtual paths (which name nothing on
// disk, so `is_dir` is always false for them).
//
// A gzip file is deliberately not browsable: it holds one compressed
// stream, not a directory of members, so there'd be exactly one thing to
// pick out of it. It opens as a read-only buffer instead.
// Whether this file is a directory of members rather than content: a
// zip, a tar, or a gzip whose contents turn out to be a tar.
//
// That last case is the only one that costs anything -- deciding it
// means inflating, since a `.tar.gz`'s own bytes say only "gzip". It is
// also the only honest way to tell `notes.txt.gz` (open the text) from
// `src.tar.gz` (browse it) without trusting the name, which is the rule
// `kind_of` already sets for every other archive here.
pub fn holds_members(path: &Path) -> bool {
    match kind_of(path) {
        Some(Kind::Zip) | Some(Kind::Tar) => true,
        Some(Kind::Gzip) => archive_bytes(path).map(|data| is_tar(&data)).unwrap_or(false),
        None => false,
    }
}

// The cheap version, for a *listing* -- which icon and colour an entry
// gets, not how it opens.
//
// `holds_members` is the honest answer and this one isn't: it trusts a
// `.tar.gz`/`.tgz` name rather than inflating, because inflating every
// gzip in a directory to decide what to draw is not a trade worth
// making. The distinction is safe exactly because it is cosmetic --
// pressing Enter still goes through `is_browsable`, which reads the
// bytes, so a misnamed file is drawn wrong for a moment and still opens
// correctly.
pub fn looks_like_archive(path: &Path) -> bool {
    match kind_of(path) {
        Some(Kind::Zip) | Some(Kind::Tar) => true,
        Some(Kind::Gzip) => {
            let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
            name.ends_with(".tar.gz") || name.ends_with(".tgz")
        }
        None => false,
    }
}

pub fn is_browsable(path: &str) -> bool {
    let Some((archive, inner)) = split(path) else {
        return holds_members(Path::new(path));
    };
    if inner.is_empty() {
        return true;
    }
    let Ok(members) = list(&archive) else { return false };
    members.iter().any(|m| (m.name == inner && m.is_dir) || m.name.starts_with(&format!("{inner}/")))
}

// One entry in a zip's central directory. `name` is the full path inside
// the archive, `/`-separated with no leading slash -- the format's own
// convention, kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    method: u16,
    compressed_size: u64,
    local_offset: u64,
    crc: u32,
}

// Every member of a zip archive, in the order its central directory
// lists them (which is the order they were added, and what `unzip -l`
// shows).
pub fn list(path: &Path) -> Result<Vec<Member>, String> {
    let data = archive_bytes(path)?;
    members_of(&data)
}

// The bytes an archive is actually made of: the file itself, or -- for a
// gzip -- what is inside it. One place, so `list`, `read_member` and
// `is_browsable` can never disagree about what a `.tar.gz` contains.
//
// Known cost, accepted for now: a `.tar.gz` is inflated again for every
// member read, since nothing here holds the decompressed bytes between
// calls. That is one inflate per file you open out of an archive, which
// is what the read-whole-file approach above already assumes about
// scale.
fn archive_bytes(path: &Path) -> Result<Vec<u8>, String> {
    match kind_of(path) {
        Some(Kind::Gzip) => gunzip(path).map(|(_, data)| data),
        _ => read_file(path),
    }
}

// One member's decompressed bytes.
pub fn read_member(path: &Path, name: &str) -> Result<Vec<u8>, String> {
    let data = archive_bytes(path)?;
    let members = members_of(&data)?;
    let member = members.iter().find(|m| m.name.trim_end_matches('/') == name.trim_matches('/')).ok_or_else(|| format!("no such member: {name}"))?;
    extract(&data, member)
}

// What a directory listing of `inner` inside the archive looks like:
// each immediate child once, directories included even when the archive
// has no explicit entry for them.
//
// That last part is why this synthesizes rather than filters. Zip stores
// a flat list of full paths, and whether a directory gets its own entry
// is up to whatever wrote the archive -- `zip -r` writes them, plenty of
// other producers don't. A browser that only listed explicit entries
// would show an archive as empty at its root and hide everything in it,
// so the directory structure is derived from the member names instead,
// which is what every zip tool does.
pub fn list_dir(members: &[Member], inner: &str) -> Vec<Member> {
    let prefix = if inner.is_empty() { String::new() } else { format!("{}/", inner.trim_matches('/')) };
    let mut out: Vec<Member> = Vec::new();
    for member in members {
        let Some(rest) = member.name.strip_prefix(&prefix) else { continue };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            continue;
        }
        // A child is a directory either because it has its own entry
        // ending in `/`, or because something deeper than it exists.
        let (child, is_dir) = match rest.split_once('/') {
            Some((head, _)) => (head, true),
            None => (rest, member.is_dir),
        };
        match out.iter_mut().find(|m| m.name == child) {
            // Seen already -- an explicit directory entry and the files
            // under it both name it, and either order is possible.
            Some(existing) => existing.is_dir |= is_dir,
            None => out.push(Member {
                name: child.to_string(),
                size: if is_dir { 0 } else { member.size },
                is_dir,
                method: member.method,
                compressed_size: member.compressed_size,
                local_offset: member.local_offset,
                crc: member.crc,
            }),
        }
    }
    out
}

// A gzip file's decompressed bytes, plus the original filename it
// records if it has one (gzip stores the name it compressed, which is
// how `e archive.gz` can still tell that the thing inside was a `.json`).
pub fn gunzip(path: &Path) -> Result<(Option<String>, Vec<u8>), String> {
    gunzip_within(path, inflate::MAX_OUTPUT)
}

// `gunzip` with a caller-chosen ceiling on the decompressed size, for a
// caller that knows what it is reading and does not want the general
// one. The budget is the whole file's, not each member's: members
// concatenate, so a per-member ceiling would be no ceiling at all --
// `cat bomb.gz bomb.gz ...` would walk straight past it.
pub fn gunzip_within(path: &Path, max_out: usize) -> Result<(Option<String>, Vec<u8>), String> {
    let data = read_file(path)?;
    let mut pos = 0;
    let mut out = Vec::new();
    let mut first_name = None;
    // Concatenated members are a legal gzip file (`cat a.gz b.gz` makes
    // one) and decompress to the concatenation of their contents, which
    // is what gzip itself does with them.
    while pos < data.len() {
        let (name, body, next) = gzip_member(&data, pos, max_out.saturating_sub(out.len()))?;
        if first_name.is_none() {
            first_name = name;
        }
        out.extend_from_slice(&body);
        pos = next;
    }
    if out.is_empty() && first_name.is_none() && data.len() < 18 {
        return Err("not a gzip file".to_string());
    }
    Ok((first_name, out))
}

// One gzip member starting at `pos`: its stored filename, its
// decompressed bytes, and where the next member would begin.
fn gzip_member(data: &[u8], pos: usize, max_out: usize) -> Result<(Option<String>, Vec<u8>, usize), String> {
    let header = data.get(pos..pos + 10).ok_or("truncated gzip header")?;
    if header[0] != 0x1f || header[1] != 0x8b {
        return Err("not a gzip file".to_string());
    }
    if header[2] != 8 {
        return Err(format!("unsupported gzip compression method {}", header[2]));
    }
    let flags = header[3];
    let mut at = pos + 10;
    // FEXTRA: a length-prefixed blob of subfields nothing here needs.
    if flags & 0b0000_0100 != 0 {
        let len = u16::from_le_bytes([*byte(data, at)?, *byte(data, at + 1)?]) as usize;
        at += 2 + len;
    }
    // FNAME / FCOMMENT: NUL-terminated, in that order.
    let name = if flags & 0b0000_1000 != 0 { Some(read_cstring(data, &mut at)?) } else { None };
    if flags & 0b0001_0000 != 0 {
        read_cstring(data, &mut at)?;
    }
    // FHCRC: a CRC16 of the header, which this doesn't check -- the
    // CRC32 of the actual data below catches everything it would.
    if flags & 0b0000_0010 != 0 {
        at += 2;
    }
    let stream = data.get(at..).ok_or("truncated gzip data")?;
    let (body, used) = inflate::inflate_prefix(stream, max_out)?;
    let trailer = data.get(at + used..at + used + 8).ok_or("truncated gzip trailer")?;
    let want_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let want_len = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
    if inflate::crc32(&body) != want_crc {
        return Err("gzip checksum mismatch (the file is corrupt)".to_string());
    }
    // ISIZE is the length mod 2^32, so on a >4GiB member this compares
    // the low bits -- which is all the format itself claims.
    if (body.len() as u32) != want_len {
        return Err("gzip length mismatch (the file is corrupt)".to_string());
    }
    Ok((name, body, at + used + 8))
}

fn read_cstring(data: &[u8], at: &mut usize) -> Result<String, String> {
    let start = *at;
    while *byte(data, *at)? != 0 {
        *at += 1;
    }
    let s = String::from_utf8_lossy(&data[start..*at]).into_owned();
    *at += 1;
    Ok(s)
}

fn byte(data: &[u8], at: usize) -> Result<&u8, String> {
    data.get(at).ok_or_else(|| "truncated gzip header".to_string())
}

// A tar member is stored whole and uncompressed; this marks one so
// `extract` can tell it from a zip member, whose `method` really is a
// zip compression method.
const TAR_STORED: u16 = u16::MAX;

// Every member of a tar archive. Tar has no index: the members *are* the
// file, each a 512-byte header followed by its own data padded up to the
// next block, ending at two zero blocks (or simply at the end, since
// plenty of writers omit them).
fn tar_members(data: &[u8]) -> Vec<Member> {
    let mut out = Vec::new();
    let mut at = 0usize;
    // GNU's answer to tar's 100-byte name field: an `L` entry whose
    // *data* is the next entry's real name. Carried across one iteration.
    let mut long_name: Option<String> = None;
    while at + TAR_BLOCK <= data.len() {
        let header = &data[at..at + TAR_BLOCK];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let size = octal_at(header, 124, 12);
        let data_at = at + TAR_BLOCK;
        // Padded up to the next block boundary, which is how the next
        // header is found.
        at = data_at + size.div_ceil(TAR_BLOCK as u64) as usize * TAR_BLOCK;
        let typeflag = header[156];
        if typeflag == b'L' {
            long_name = data.get(data_at..data_at + size as usize).map(tar_string);
            continue;
        }
        let name = match long_name.take() {
            Some(name) => name,
            None => {
                // POSIX ustar splits a long path across `prefix` and
                // `name`; a short one leaves the prefix empty.
                let name = tar_string(&header[..100]);
                match tar_string(&header[345..500]) {
                    prefix if prefix.is_empty() => name,
                    prefix => format!("{prefix}/{name}"),
                }
            }
        };
        if name.is_empty() {
            continue;
        }
        // Everything that isn't a regular file or a directory -- links,
        // devices, the pax/GNU metadata entries -- is skipped rather
        // than shown: none of them has content a reader wants, and a
        // `PaxHeader` entry in a listing is noise from the tool that
        // wrote the archive, not part of what is in it.
        let is_dir = typeflag == b'5' || name.ends_with('/');
        if !is_dir && !matches!(typeflag, b'0' | 0) {
            continue;
        }
        out.push(Member {
            name: name.trim_end_matches('/').to_string(),
            size: if is_dir { 0 } else { size },
            is_dir,
            method: TAR_STORED,
            compressed_size: size,
            local_offset: data_at as u64,
            crc: 0,
        });
    }
    out
}

// A NUL-terminated, NUL-padded field.
fn tar_string(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).trim().to_string()
}

// Tar's numbers are ASCII octal, space- or NUL-terminated.
fn octal_at(header: &[u8], at: usize, len: usize) -> u64 {
    let text = tar_string(&header[at..(at + len).min(header.len())]);
    u64::from_str_radix(text.trim(), 8).unwrap_or(0)
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    // Read whole rather than seeking around it. A zip's central
    // directory is at the *end* and its members are at the front, so any
    // real use touches both ends anyway, and what this opens is an
    // archive someone is browsing in an editor -- not a disk image.
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

// Zip's end-of-central-directory record, which is what makes an archive
// readable at all: it says where the central directory is. It sits at
// the very end of the file, after a variable-length comment, so the only
// way to find it is to scan backwards for its signature.
const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
// The comment is a u16 length, so the record can't start further back
// than this from the end.
const MAX_COMMENT: usize = u16::MAX as usize;

fn members_of(data: &[u8]) -> Result<Vec<Member>, String> {
    if is_tar(data) {
        return Ok(tar_members(data));
    }
    let eocd = find_eocd(data).ok_or("not a zip file (no end-of-central-directory record)")?;
    let count = u16::from_le_bytes([data[eocd + 10], data[eocd + 11]]) as usize;
    let offset = u32_at(data, eocd + 16)? as usize;
    if count == u16::MAX as usize || offset == u32::MAX as usize {
        return Err("zip64 archives are not supported".to_string());
    }
    let mut members = Vec::with_capacity(count);
    let mut at = offset;
    for _ in 0..count {
        if u32_at(data, at)? != CENTRAL_SIG {
            return Err("corrupt zip central directory".to_string());
        }
        let flags = u16_at(data, at + 8)?;
        let method = u16_at(data, at + 10)?;
        let crc = u32_at(data, at + 16)?;
        let compressed_size = u32_at(data, at + 20)? as u64;
        let size = u32_at(data, at + 24)? as u64;
        let name_len = u16_at(data, at + 28)? as usize;
        let extra_len = u16_at(data, at + 30)? as usize;
        let comment_len = u16_at(data, at + 32)? as usize;
        let local_offset = u32_at(data, at + 42)? as u64;
        let raw = data.get(at + 46..at + 46 + name_len).ok_or("corrupt zip member name")?;
        let name = String::from_utf8_lossy(raw).into_owned();
        // Bit 0 is the format's own "this entry is encrypted" flag,
        // whichever scheme was used -- refused here rather than handed to
        // the decompressor, which would otherwise produce noise and blame
        // the archive for being corrupt.
        if flags & 1 != 0 {
            return Err(format!("{name}: encrypted zip entries are not supported"));
        }
        if compressed_size == u32::MAX as u64 || size == u32::MAX as u64 || local_offset == u32::MAX as u64 {
            return Err("zip64 archives are not supported".to_string());
        }
        members.push(Member {
            is_dir: name.ends_with('/'),
            name: name.trim_end_matches('/').to_string(),
            size,
            method,
            compressed_size,
            local_offset,
            crc,
        });
        at += 46 + name_len + extra_len + comment_len;
    }
    Ok(members)
}

fn find_eocd(data: &[u8]) -> Option<usize> {
    let earliest = data.len().saturating_sub(MAX_COMMENT + 22);
    // Backwards, so a zip whose *contents* happen to contain the
    // signature (a zip inside a zip, stored uncompressed) doesn't win
    // over the real record at the end.
    (earliest..data.len().checked_sub(22)? + 1).rev().find(|&i| u32_at(data, i) == Ok(EOCD_SIG))
}

// One member's bytes, found through its local header rather than the
// central directory's own copy of the name and extra field: the two are
// allowed to differ in length, and it's the local one that says where
// the data actually starts.
fn extract(data: &[u8], member: &Member) -> Result<Vec<u8>, String> {
    if member.is_dir {
        return Err(format!("{}: is a directory", member.name));
    }
    // Tar stores its members whole and uncompressed, so a member *is* a
    // slice -- `local_offset` is where its data starts rather than where
    // a local header does.
    if member.method == TAR_STORED {
        let at = member.local_offset as usize;
        return data.get(at..at + member.size as usize).map(|d| d.to_vec()).ok_or_else(|| "truncated tar member data".to_string());
    }
    let at = member.local_offset as usize;
    if u32_at(data, at)? != LOCAL_SIG {
        return Err(format!("{}: corrupt zip local header", member.name));
    }
    let name_len = u16_at(data, at + 26)? as usize;
    let extra_len = u16_at(data, at + 28)? as usize;
    let start = at + 30 + name_len + extra_len;
    let raw = data.get(start..start + member.compressed_size as usize).ok_or("truncated zip member data")?;
    let out = match member.method {
        0 => raw.to_vec(),
        8 => inflate::inflate(raw).map_err(|e| format!("{}: {e}", member.name))?,
        other => return Err(format!("{}: unsupported zip compression method {other}", member.name)),
    };
    if inflate::crc32(&out) != member.crc {
        return Err(format!("{}: checksum mismatch (the archive is corrupt)", member.name));
    }
    Ok(out)
}

fn u16_at(data: &[u8], at: usize) -> Result<u16, String> {
    let b = data.get(at..at + 2).ok_or("truncated zip structure")?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(data: &[u8], at: usize) -> Result<u32, String> {
    let b = data.get(at..at + 4).ok_or("truncated zip structure")?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real 3-member zip, produced once by python3's zipfile and
    // embedded byte for byte rather than assembled here -- the point of
    // these tests is agreement with what real tools write, which a
    // fixture built by this file's own understanding of the format
    // couldn't show. Contents:
    //   notes.txt          -> "hello from a zip\n"      (deflated)
    //   dir/inner.json     -> "{\"a\": 1}\n"            (deflated)
    //   dir/deep/leaf.txt  -> "leaf\n"                  (stored)
    // and no explicit entry for `dir/` or `dir/deep/`, which is what
    // makes it exercise list_dir's synthesis.
    const ZIP: &[u8] = include_bytes!("testdata/sample.zip");
    // gzip of "compressed text\nsecond line\n", with FNAME set.
    const GZ: &[u8] = include_bytes!("testdata/sample.txt.gz");

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("bish-archive-test-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn kind_is_read_from_the_magic_bytes_not_the_name() {
        let zip = write_temp("misnamed.txt", ZIP);
        let gz = write_temp("misnamed.doc", GZ);
        let plain = write_temp("plain.zip", b"this is not a zip\n");
        assert_eq!(kind_of(&zip), Some(Kind::Zip));
        assert_eq!(kind_of(&gz), Some(Kind::Gzip));
        assert_eq!(kind_of(&plain), None);
        assert_eq!(kind_of(Path::new("/nonexistent/nothing")), None);
        for p in [zip, gz, plain] {
            std::fs::remove_file(p).unwrap();
        }
    }

    #[test]
    fn lists_every_member_of_a_real_zip() {
        let zip = write_temp("list.zip", ZIP);
        let names: Vec<String> = list(&zip).unwrap().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["notes.txt", "dir/inner.json", "dir/deep/leaf.txt"]);
        std::fs::remove_file(zip).unwrap();
    }

    #[test]
    fn reads_a_deflated_and_a_stored_member() {
        let zip = write_temp("read.zip", ZIP);
        assert_eq!(read_member(&zip, "notes.txt").unwrap(), b"hello from a zip\n");
        assert_eq!(read_member(&zip, "dir/deep/leaf.txt").unwrap(), b"leaf\n");
        assert!(read_member(&zip, "nope.txt").unwrap_err().contains("no such member"));
        std::fs::remove_file(zip).unwrap();
    }

    // The archive has no `dir/` or `dir/deep/` entry of its own, so
    // every directory here is synthesized from the member names.
    #[test]
    fn lists_a_directory_inside_the_archive_including_implied_ones() {
        let zip = write_temp("dirs.zip", ZIP);
        let members = list(&zip).unwrap();

        let root: Vec<(String, bool)> = list_dir(&members, "").into_iter().map(|m| (m.name, m.is_dir)).collect();
        assert_eq!(root, vec![("notes.txt".to_string(), false), ("dir".to_string(), true)]);

        let dir: Vec<(String, bool)> = list_dir(&members, "dir").into_iter().map(|m| (m.name, m.is_dir)).collect();
        assert_eq!(dir, vec![("inner.json".to_string(), false), ("deep".to_string(), true)]);

        let deep: Vec<(String, bool)> = list_dir(&members, "dir/deep").into_iter().map(|m| (m.name, m.is_dir)).collect();
        assert_eq!(deep, vec![("leaf.txt".to_string(), false)]);

        assert!(list_dir(&members, "dir/nope").is_empty());
        std::fs::remove_file(zip).unwrap();
    }

    #[test]
    fn a_listed_files_size_is_its_uncompressed_size() {
        let zip = write_temp("size.zip", ZIP);
        let members = list(&zip).unwrap();
        let notes = list_dir(&members, "").into_iter().find(|m| m.name == "notes.txt").unwrap();
        assert_eq!(notes.size, "hello from a zip\n".len() as u64);
        std::fs::remove_file(zip).unwrap();
    }

    #[test]
    fn gunzips_a_real_gzip_file_and_reports_its_stored_name() {
        let gz = write_temp("sample.txt.gz", GZ);
        let (name, body) = gunzip(&gz).unwrap();
        assert_eq!(name.as_deref(), Some("sample.txt"));
        assert_eq!(body, b"compressed text\nsecond line\n");
        std::fs::remove_file(gz).unwrap();
    }

    // `cat a.gz b.gz` is a legal gzip file that decompresses to both.
    #[test]
    fn gunzips_concatenated_members_as_one_stream() {
        let mut both = GZ.to_vec();
        both.extend_from_slice(GZ);
        let gz = write_temp("double.gz", &both);
        let (_, body) = gunzip(&gz).unwrap();
        assert_eq!(body, b"compressed text\nsecond line\ncompressed text\nsecond line\n");
        std::fs::remove_file(gz).unwrap();
    }

    #[test]
    fn a_corrupt_gzip_body_fails_its_checksum_rather_than_returning_garbage() {
        let mut broken = GZ.to_vec();
        // Flip a bit in the middle of the deflate stream, past the
        // header and before the trailer.
        let middle = broken.len() / 2;
        broken[middle] ^= 0x40;
        let gz = write_temp("broken.gz", &broken);
        assert!(gunzip(&gz).is_err());
        std::fs::remove_file(gz).unwrap();
    }

    #[test]
    fn split_finds_the_archive_and_the_member_inside_it() {
        let zip = write_temp("split.zip", ZIP);
        let root = format!("{}!", zip.display());
        let member = format!("{}!/dir/inner.json", zip.display());
        assert_eq!(split(&root), Some((zip.clone(), String::new())));
        assert_eq!(split(&member), Some((zip.clone(), "dir/inner.json".to_string())));
        std::fs::remove_file(zip).unwrap();
    }

    // The separator is only a separator when what precedes it really is
    // an archive -- otherwise `!` is just a character in a filename.
    #[test]
    fn split_leaves_an_ordinary_path_containing_a_bang_alone() {
        let plain = write_temp("bang!.txt", b"not an archive\n");
        assert_eq!(split(&plain.display().to_string()), None);
        assert_eq!(split("/no/such/file.zip!/x"), None);
        assert_eq!(split("/etc/hostname"), None);
        std::fs::remove_file(plain).unwrap();
    }

    #[test]
    fn join_is_splits_inverse() {
        let zip = write_temp("join.zip", ZIP);
        for inner in ["", "notes.txt", "dir/deep/leaf.txt"] {
            let joined = join(&zip, inner);
            assert_eq!(split(&joined), Some((zip.clone(), inner.to_string())), "{joined}");
        }
        std::fs::remove_file(zip).unwrap();
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_reported_rather_than_parsed() {
        let plain = write_temp("notazip.bin", b"PK\\x03\\x04 but only barely");
        assert!(list(&plain).is_err());
        std::fs::remove_file(plain).unwrap();
    }
}

#[cfg(test)]
mod real_world_tests {
    use super::*;

    // Every gzip file on this machine's man pages, decompressed and
    // compared byte for byte against what the real `gzip` produces. A
    // few hundred real files written by a real compressor, which is a
    // far better test of the decoder than any fixture chosen by hand --
    // skipped where the directory or the gzip binary isn't there, same
    // "quietly unavailable" contract the git tests use.
    #[test]
    fn matches_real_gzip_on_every_man_page_it_can_find() {
        let Ok(dir) = std::fs::read_dir("/usr/share/man/man1") else { return };
        let mut checked = 0;
        for entry in dir.flatten().take(300) {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "gz") {
                continue;
            }
            let Ok(expected) = std::process::Command::new("gzip").arg("-dc").arg(&path).output() else { return };
            if !expected.status.success() {
                continue;
            }
            let (_, ours) = gunzip(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(ours, expected.stdout, "{}", path.display());
            checked += 1;
        }
        assert!(checked > 0, "found no man pages to check against");
    }

    // A real tar, built by hand: one 512-byte header per member, data
    // padded up to the next block.
    fn tar_header(name: &str, size: u64, typeflag: u8) -> Vec<u8> {
        let mut block = vec![0u8; 512];
        block[..name.len()].copy_from_slice(name.as_bytes());
        let octal = format!("{size:011o}\0");
        block[124..124 + octal.len()].copy_from_slice(octal.as_bytes());
        block[156] = typeflag;
        block[257..262].copy_from_slice(b"ustar");
        block
    }

    fn tar_of(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, body) in entries {
            let is_dir = name.ends_with('/');
            out.extend(tar_header(name, if is_dir { 0 } else { body.len() as u64 }, if is_dir { b'5' } else { b'0' }));
            if !is_dir {
                out.extend(body.as_bytes());
                out.resize(out.len().div_ceil(512) * 512, 0);
            }
        }
        out.extend(vec![0u8; 1024]);
        out
    }

    #[test]
    fn a_tar_lists_its_members() {
        let data = tar_of(&[("a.txt", "alpha"), ("dir/", ""), ("dir/b.txt", "bravo")]);
        assert!(is_tar(&data));
        let members = members_of(&data).unwrap();
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "dir", "dir/b.txt"]);
        assert!(members[1].is_dir);
        assert_eq!(members[0].size, 5);
    }

    #[test]
    fn a_tar_member_extracts_whole() {
        let data = tar_of(&[("a.txt", "alpha"), ("b.txt", "bravo")]);
        let members = members_of(&data).unwrap();
        assert_eq!(extract(&data, &members[1]).unwrap(), b"bravo");
    }

    // POSIX ustar splits a long path across `prefix` and `name`.
    #[test]
    fn a_ustar_prefix_is_joined_back_onto_the_name() {
        let mut block = tar_header("deep.txt", 0, b'0');
        let prefix = "a/very/long/path";
        block[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        let mut data = block;
        data.extend(vec![0u8; 1024]);
        assert_eq!(members_of(&data).unwrap()[0].name, "a/very/long/path/deep.txt");
    }

    // GNU's answer to the 100-byte name field: an `L` entry whose data
    // is the *next* entry's real name.
    #[test]
    fn a_gnu_long_name_entry_names_the_member_after_it() {
        let long = "x".repeat(140);
        let mut data = tar_header("././@LongLink", long.len() as u64, b'L');
        data.extend(long.as_bytes());
        data.resize(data.len().div_ceil(512) * 512, 0);
        data.extend(tar_header("truncated", 3, b'0'));
        data.extend(b"abc");
        data.resize(data.len().div_ceil(512) * 512, 0);
        data.extend(vec![0u8; 1024]);
        let members = members_of(&data).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].name, long);
    }

    // Links, devices and the metadata entries tools write are not
    // content anybody wants in a listing.
    #[test]
    fn entries_that_are_not_files_or_directories_are_skipped() {
        let mut data = tar_header("link", 0, b'2');
        data.extend(tar_header("real.txt", 1, b'0'));
        data.extend(b"x");
        data.resize(data.len().div_ceil(512) * 512, 0);
        data.extend(vec![0u8; 1024]);
        let members = members_of(&data).unwrap();
        assert_eq!(members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["real.txt"]);
    }

    #[test]
    fn a_tar_directory_listing_is_synthesized_the_same_way_a_zips_is() {
        let data = tar_of(&[("dir/b.txt", "bravo"), ("a.txt", "alpha")]);
        let members = members_of(&data).unwrap();
        let listed = list_dir(&members, "");
        let root: Vec<&str> = listed.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(root, vec!["dir", "a.txt"], "the directory is derived even with no entry of its own");
    }

    #[test]
    fn a_non_tar_is_not_mistaken_for_one() {
        assert!(!is_tar(b"just some text that happens to be long enough to reach offset 257 if it kept going"));
        assert!(!is_tar(&[0u8; 512]));
    }
}
