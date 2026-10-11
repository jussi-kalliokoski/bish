// Read-only zip, gzip and tar, on top of inflate.rs, so the editor can
// browse an archive like a directory (`e some.zip`) and open a gzipped
// file as a read-only buffer.
//
// Writing would mean compressing, and rebuilding a zip's central
// directory around an edit, so all of it is read-only. Not supported,
// each reported as an error: zip64 (over 4 GiB or 65535 members),
// encrypted entries, and compression methods other than store and deflate.

use std::path::{Path, PathBuf};

use crate::inflate;

// Separates the archive from the member in a virtual path:
// `/a/b.zip!/dir/file.txt`, with `/a/b.zip!` naming the archive's root. A
// virtual path travels as a plain string (`EditTarget::path`,
// `browser::Entry::path`, the status line), so the distinction has to be
// in the spelling.
//
// `split` only treats a `!` as a separator when what precedes it is a real
// archive, so a `!` in an ordinary filename is safe.
pub const SEPARATOR: char = '!';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Zip,
    Gzip,
    Tar,
}

// What `path` is, by its magic bytes rather than its name. `None` means
// either "not an archive" or "unreadable".
pub fn kind_of(path: &Path) -> Option<Kind> {
    use std::io::Read;
    if !path.is_file() {
        return None;
    }
    let mut head = [0u8; 4];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut head).ok()?;
    match &head[..read] {
        [0x1f, 0x8b, ..] => Some(Kind::Gzip),
        // A local file header, or the end record alone for an empty
        // archive. A spanned archive (`PK\x07\x08`) is refused: its other
        // volumes aren't there.
        [b'P', b'K', 3, 4] | [b'P', b'K', 5, 6] => Some(Kind::Zip),
        // Tar's magic is 257 bytes in.
        _ => is_tar_file(path).then_some(Kind::Tar),
    }
}

// A pre-POSIX tar has no magic and isn't recognized. Guessing would show
// arbitrary binaries as directories.
fn is_tar_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else { return false };
    let mut head = [0u8; TAR_BLOCK];
    let Ok(read) = file.read(&mut head) else { return false };
    read >= 265 && is_tar(&head)
}

const TAR_BLOCK: usize = 512;

// The same test on bytes in hand, which is how a `.tar.gz` is recognized
// once inflated. So `x.tar.gz!/dir/f` needs only one `!`.
fn is_tar(data: &[u8]) -> bool {
    data.len() >= 265 && &data[257..262] == b"ustar"
}

// Splits a virtual path into the archive and the member path inside it
// ("" for the root); `None` for an ordinary path. The first `!` whose
// prefix is a real archive wins, so a directory named `weird!` on the way
// doesn't. Nested archives aren't supported.
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

// The inverse of `split`, and the only place a virtual path is built.
pub fn join(archive: &Path, inner: &str) -> String {
    let inner = inner.trim_matches('/');
    if inner.is_empty() { format!("{}{SEPARATOR}", archive.display()) } else { format!("{}{SEPARATOR}/{inner}", archive.display()) }
}

// Whether the file holds members rather than content: a zip, a tar, or a
// gzip whose contents are a tar. Only the last needs inflating, and it's
// how `notes.txt.gz` (open the text) is told from `src.tar.gz` (browse it)
// without trusting the name.
pub fn holds_members(path: &Path) -> bool {
    match kind_of(path) {
        Some(Kind::Zip) | Some(Kind::Tar) => true,
        Some(Kind::Gzip) => archive_bytes(path).map(|data| is_tar(&data)).unwrap_or(false),
        None => false,
    }
}

// A cheap guess for a listing's icon and colour: trusts a `.tar.gz` or
// `.tgz` name instead of inflating every gzip in the directory. Opening
// still goes through `is_browsable`, which reads the bytes.
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

// Whether the file browser can list this path: an archive on disk, or a
// directory inside one. `Path::is_dir` is always false for a virtual path.
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

// One member. `name` is its full path inside the archive, `/`-separated,
// without a leading slash.
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

// Every member, in the order the archive stores them.
pub fn list(path: &Path) -> Result<Vec<Member>, String> {
    let data = archive_bytes(path)?;
    members_of(&data)
}

// The file's bytes, or for a gzip what's inside it, so `list`,
// `read_member` and `is_browsable` agree on what a `.tar.gz` contains. A
// `.tar.gz` is inflated again for every member read.
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

// The immediate children of `inner`, each once. Directories are derived
// from member names, since many zip writers store no entries for them.
pub fn list_dir(members: &[Member], inner: &str) -> Vec<Member> {
    let prefix = if inner.is_empty() { String::new() } else { format!("{}/", inner.trim_matches('/')) };
    let mut out: Vec<Member> = Vec::new();
    for member in members {
        let Some(rest) = member.name.strip_prefix(&prefix) else { continue };
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            continue;
        }
        // A directory if it has its own entry or something below it.
        let (child, is_dir) = match rest.split_once('/') {
            Some((head, _)) => (head, true),
            None => (rest, member.is_dir),
        };
        match out.iter_mut().find(|m| m.name == child) {
            // Named by its own entry and by the files under it, in either
            // order.
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

// A gzip file's decompressed bytes, and the original filename if it
// stores one, so `e archive.gz` can tell the content was a `.json`.
pub fn gunzip(path: &Path) -> Result<(Option<String>, Vec<u8>), String> {
    gunzip_within(path, inflate::MAX_OUTPUT)
}

// `gunzip` with the caller's ceiling on the decompressed size. It covers
// the whole file: members concatenate, so a per-member ceiling could be
// bypassed with `cat bomb.gz bomb.gz`.
pub fn gunzip_within(path: &Path, max_out: usize) -> Result<(Option<String>, Vec<u8>), String> {
    let data = read_file(path)?;
    let mut pos = 0;
    let mut out = Vec::new();
    let mut first_name = None;
    // Concatenated members are a legal gzip file and decompress to the
    // concatenation of their contents.
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
    // FHCRC: the header's CRC16, not checked.
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
    // ISIZE is the length mod 2^32, so only the low bits are compared.
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

// Marks a tar member in `method`, which otherwise holds a zip compression
// method.
const TAR_STORED: u16 = u16::MAX;

// Tar has no index: each member is a 512-byte header followed by its
// data padded to the next block, ending at two zero blocks or simply at
// the end of the file.
fn tar_members(data: &[u8]) -> Vec<Member> {
    let mut out = Vec::new();
    let mut at = 0usize;
    // GNU long names: an `L` entry's data is the next entry's name.
    let mut long_name: Option<String> = None;
    while at + TAR_BLOCK <= data.len() {
        let header = &data[at..at + TAR_BLOCK];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let size = octal_at(header, 124, 12);
        let data_at = at + TAR_BLOCK;
        at = data_at + size.div_ceil(TAR_BLOCK as u64) as usize * TAR_BLOCK;
        let typeflag = header[156];
        if typeflag == b'L' {
            long_name = data.get(data_at..data_at + size as usize).map(tar_string);
            continue;
        }
        let name = match long_name.take() {
            Some(name) => name,
            None => {
                // POSIX ustar splits a long path into `prefix` and `name`.
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
        // Only regular files and directories are listed. Links, devices
        // and pax/GNU metadata entries have no content worth showing.
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
    // Read whole: a zip's central directory is at the end and its
    // members at the front, so both ends get read anyway.
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

// The end-of-central-directory record says where the central directory
// is. It follows a variable-length comment at the end of the file, so it
// is found by scanning backwards.
const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
// The comment's length is a u16, which bounds how far back the record
// can start.
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
        // Bit 0 marks an encrypted entry. Refused here, or inflating it
        // would produce noise and call the archive corrupt.
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
    // Backwards, so a zip stored uncompressed inside this one can't
    // match first.
    (earliest..data.len().checked_sub(22)? + 1).rev().find(|&i| u32_at(data, i) == Ok(EOCD_SIG))
}

// Found through the local header, not the central directory's copy of
// the name and extra field: their lengths may differ, and the local one
// says where the data starts.
fn extract(data: &[u8], member: &Member) -> Result<Vec<u8>, String> {
    if member.is_dir {
        return Err(format!("{}: is a directory", member.name));
    }
    // A tar member is stored uncompressed, and its `local_offset` is where
    // its data starts.
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

    // Written by python3's zipfile, so these tests check agreement with a
    // real tool rather than with this file's reading of the format:
    //   notes.txt          -> "hello from a zip\n"      (deflated)
    //   dir/inner.json     -> "{\"a\": 1}\n"            (deflated)
    //   dir/deep/leaf.txt  -> "leaf\n"                  (stored)
    // No entries for `dir/` or `dir/deep/`, so `list_dir` has to derive them.
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
        // Flip a bit inside the deflate stream.
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
        let plain = write_temp("notazip.bin", b"PK\x03\x04 but only barely");
        assert!(list(&plain).is_err());
        std::fs::remove_file(plain).unwrap();
    }
}

#[cfg(test)]
mod real_world_tests {
    use super::*;

    // Every gzipped man page found, against the real `gzip`. Skipped where
    // there are none or no `gzip`.
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

    // A minimal ustar header.
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

    #[test]
    fn a_ustar_prefix_is_joined_back_onto_the_name() {
        let mut block = tar_header("deep.txt", 0, b'0');
        let prefix = "a/very/long/path";
        block[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        let mut data = block;
        data.extend(vec![0u8; 1024]);
        assert_eq!(members_of(&data).unwrap()[0].name, "a/very/long/path/deep.txt");
    }

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
