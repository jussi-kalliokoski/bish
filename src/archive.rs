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
        _ => None,
    }
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
pub fn is_browsable(path: &str) -> bool {
    let Some((archive, inner)) = split(path) else {
        return kind_of(Path::new(path)) == Some(Kind::Zip);
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
    let data = read_file(path)?;
    members_of(&data)
}

// One member's decompressed bytes.
pub fn read_member(path: &Path, name: &str) -> Result<Vec<u8>, String> {
    let data = read_file(path)?;
    let members = members_of(&data)?;
    let member = members
        .iter()
        .find(|m| m.name.trim_end_matches('/') == name.trim_matches('/'))
        .ok_or_else(|| format!("no such member: {name}"))?;
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
    let data = read_file(path)?;
    let mut pos = 0;
    let mut out = Vec::new();
    let mut first_name = None;
    // Concatenated members are a legal gzip file (`cat a.gz b.gz` makes
    // one) and decompress to the concatenation of their contents, which
    // is what gzip itself does with them.
    while pos < data.len() {
        let (name, body, next) = gzip_member(&data, pos)?;
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
fn gzip_member(data: &[u8], pos: usize) -> Result<(Option<String>, Vec<u8>, usize), String> {
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
    let (body, used) = inflate::inflate_prefix(stream)?;
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
}
