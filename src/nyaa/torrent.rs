//! Minimal, defensive `.torrent` reader: only the file list, which decides what to download.
//! Input comes from the internet, so size, depth and path components are all bounded.

use std::collections::BTreeMap;

const MAX_TORRENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 16;
const MAX_FILES: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    /// Path inside the torrent, `/`-separated, relative to the torrent's root folder.
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentInfo {
    pub name: String,
    pub files: Vec<TorrentFile>,
}

impl TorrentInfo {
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

#[derive(Debug)]
enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

fn parse(b: &[u8], pos: &mut usize, depth: usize) -> Result<Value, String> {
    if depth > MAX_DEPTH {
        return Err("torrent nesting too deep".into());
    }
    let c = *b.get(*pos).ok_or("unexpected end of torrent")?;
    match c {
        b'i' => {
            let end = b[*pos..].iter().position(|&x| x == b'e').ok_or("unterminated integer")? + *pos;
            let n = std::str::from_utf8(&b[*pos + 1..end]).map_err(|e| e.to_string())?.parse::<i64>().map_err(|e| e.to_string())?;
            *pos = end + 1;
            Ok(Value::Int(n))
        }
        b'l' => {
            *pos += 1;
            let mut out = Vec::new();
            while *b.get(*pos).ok_or("unterminated list")? != b'e' {
                if out.len() > MAX_FILES {
                    return Err("list too long".into());
                }
                out.push(parse(b, pos, depth + 1)?);
            }
            *pos += 1;
            Ok(Value::List(out))
        }
        b'd' => {
            *pos += 1;
            let mut out = BTreeMap::new();
            while *b.get(*pos).ok_or("unterminated dictionary")? != b'e' {
                let Value::Bytes(k) = parse(b, pos, depth + 1)? else { return Err("dictionary key is not a string".into()) };
                out.insert(k, parse(b, pos, depth + 1)?);
            }
            *pos += 1;
            Ok(Value::Dict(out))
        }
        b'0'..=b'9' => {
            let colon = b[*pos..].iter().position(|&x| x == b':').ok_or("bad string length")? + *pos;
            let len: usize = std::str::from_utf8(&b[*pos..colon]).map_err(|e| e.to_string())?.parse().map_err(|_| "bad string length")?;
            let start = colon + 1;
            let end = start.checked_add(len).filter(|e| *e <= b.len()).ok_or("string runs past the end")?;
            *pos = end;
            Ok(Value::Bytes(b[start..end].to_vec()))
        }
        _ => Err(format!("unexpected byte {c:#x} in torrent")),
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        _ => None,
    }
}

/// A path component that could escape the download folder is rejected, not repaired.
fn safe_component(c: &str) -> bool {
    !(c.is_empty() || c == "." || c == ".." || c.contains('/') || c.contains('\\') || c.contains('\0'))
}

/// SHA-1 of the bencoded `info` dictionary: the torrent's info hash (v1), as 40 lower-case hex digits.
pub fn info_hash(bytes: &[u8]) -> Result<String, String> {
    use sha1::{Digest, Sha1};
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err("torrent file is too large".into());
    }
    if bytes.first() != Some(&b'd') {
        return Err("not a torrent (no top-level dictionary)".into());
    }
    let mut pos = 1;
    while bytes.get(pos).ok_or("unterminated torrent")? != &b'e' {
        let Value::Bytes(key) = parse(bytes, &mut pos, 1)? else { return Err("dictionary key is not a string".into()) };
        let start = pos;
        parse(bytes, &mut pos, 1)?;
        if key == b"info" {
            let digest = Sha1::digest(&bytes[start..pos]);
            return Ok(digest.iter().map(|b| format!("{b:02x}")).collect());
        }
    }
    Err("torrent has no info dictionary".into())
}

pub fn read(bytes: &[u8]) -> Result<TorrentInfo, String> {
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err("torrent file is too large".into());
    }
    let mut pos = 0;
    let Value::Dict(root) = parse(bytes, &mut pos, 0)? else { return Err("not a torrent (no top-level dictionary)".into()) };
    let Some(Value::Dict(info)) = root.get(b"info".as_slice()) else { return Err("torrent has no info dictionary".into()) };
    let name = info.get(b"name".as_slice()).and_then(text).ok_or("torrent has no name")?;
    if !safe_component(&name) {
        return Err(format!("unsafe torrent name {name:?}"));
    }
    let files = match info.get(b"files".as_slice()) {
        None => {
            let Some(Value::Int(len)) = info.get(b"length".as_slice()) else { return Err("single-file torrent without length".into()) };
            vec![TorrentFile { path: name.clone(), size: (*len).max(0) as u64 }]
        }
        Some(Value::List(list)) => {
            if list.len() > MAX_FILES {
                return Err("too many files in torrent".into());
            }
            let mut files = Vec::with_capacity(list.len());
            for entry in list {
                let Value::Dict(d) = entry else { return Err("bad file entry".into()) };
                let Some(Value::Int(len)) = d.get(b"length".as_slice()) else { return Err("file without length".into()) };
                let Some(Value::List(parts)) = d.get(b"path".as_slice()) else { return Err("file without path".into()) };
                let parts: Vec<String> = parts.iter().filter_map(text).collect();
                if parts.is_empty() || !parts.iter().all(|p| safe_component(p)) {
                    return Err(format!("unsafe file path {parts:?}"));
                }
                files.push(TorrentFile { path: parts.join("/"), size: (*len).max(0) as u64 });
            }
            files
        }
        Some(_) => return Err("bad files entry".into()),
    };
    Ok(TorrentInfo { name, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bstr(s: &str) -> Vec<u8> {
        format!("{}:{s}", s.len()).into_bytes()
    }

    fn multi(paths: &[(&[&str], i64)]) -> Vec<u8> {
        let mut files = Vec::new();
        for (parts, len) in paths {
            files.extend(b"d6:lengthi".iter());
            files.extend(len.to_string().bytes());
            files.extend(b"e4:pathl".iter());
            for p in *parts {
                files.extend(bstr(p));
            }
            files.extend(b"ee".iter());
        }
        let mut t = b"d8:announce3:foo4:infod5:filesl".to_vec();
        t.extend(files);
        t.extend(b"e4:name".iter());
        t.extend(bstr("Teppeki Honeymoon"));
        t.extend(b"ee".iter());
        t
    }

    #[test]
    fn reads_a_folder_torrent() {
        let t = multi(&[(&["Teppeki Honeymoon v01 (2025) (Digital) (UploaderE).cbz"], 100), (&["Teppeki Honeymoon v02 (2026).cbz"], 250)]);
        let info = read(&t).unwrap();
        assert_eq!(info.name, "Teppeki Honeymoon");
        assert_eq!(info.files.len(), 2);
        assert_eq!(info.total_size(), 350);
        assert!(info.files[1].path.ends_with("v02 (2026).cbz"));
    }

    #[test]
    fn reads_a_single_file_torrent() {
        let t = format!("d4:infod6:lengthi42e4:name{}ee", String::from_utf8(bstr("Visions.cbz")).unwrap()).into_bytes();
        let info = read(&t).unwrap();
        assert_eq!(info.files, vec![TorrentFile { path: "Visions.cbz".into(), size: 42 }]);
    }

    #[test]
    fn info_hash_is_the_sha1_of_the_info_dictionary() {
        let t = format!("d8:announce3:foo4:infod6:lengthi42e4:name{}ee", String::from_utf8(bstr("Visions.cbz")).unwrap()).into_bytes();
        // sha1(b"d6:lengthi42e4:name11:Visions.cbze"), computed independently with Python's hashlib
        assert_eq!(info_hash(&t).unwrap(), "a51f3c07e7474f147484b1a0f11315d864331750");
        // key order and extra keys around "info" do not matter
        let t2 = format!("d4:infod6:lengthi42e4:name{}e7:comment1:xe", String::from_utf8(bstr("Visions.cbz")).unwrap()).into_bytes();
        assert_eq!(info_hash(&t2).unwrap(), info_hash(&t).unwrap());
        assert!(info_hash(b"d3:foo3:bare").is_err());
        assert!(info_hash(b"garbage").is_err());
    }

    #[test]
    fn hostile_paths_are_rejected() {
        assert!(read(&multi(&[(&["..", "evil.cbz"], 1)])).is_err());
        assert!(read(&multi(&[(&["a/b"], 1)])).is_err());
        assert!(read(&multi(&[(&[""], 1)])).is_err());
        let t = format!("d4:infod6:lengthi1e4:name{}ee", String::from_utf8(bstr("..")).unwrap()).into_bytes();
        assert!(read(&t).is_err());
    }

    #[test]
    fn garbage_and_limits_do_not_panic() {
        assert!(read(b"").is_err());
        assert!(read(b"not a torrent").is_err());
        assert!(read(b"d4:infod4:name3:abc5:filesli1ee").is_err());
        assert!(read(&vec![b'l'; 100]).is_err(), "deep nesting");
        assert!(read(b"d4:infod4:name99999:x").is_err(), "string past the end");
        assert!(read(&vec![0u8; MAX_TORRENT_BYTES + 1]).is_err());
    }
}
