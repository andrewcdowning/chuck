//! A minimal ZIP writer (stored, no compression) for the "export my data" download. Photos and audio are already
//! compressed, and the notes are small, so storing is fine and keeps this free of dependencies and easy to audit.
//! Names are UTF-8. Archives over 4 GB or 65,535 files are refused rather than written wrongly.

use crate::export::civil_from_days;

const fn make_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = make_table();

pub fn crc32(data: &[u8]) -> u32 {
    !data.iter().fold(!0u32, |c, b| CRC_TABLE[((c ^ *b as u32) & 0xFF) as usize] ^ (c >> 8))
}

struct Entry {
    name: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
    time: u16,
    date: u16,
}

#[derive(Default)]
pub struct ZipWriter {
    buf: Vec<u8>,
    entries: Vec<Entry>,
}

/// MS-DOS time and date, which ZIP uses. Years before 1980 are clamped.
fn dos(unix_secs: i64) -> (u16, u16) {
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    if y < 1980 {
        return (0, (1 << 5) | 1);
    }
    let t = unix_secs.rem_euclid(86_400) as u32;
    let time = ((t / 3600) << 11) | (((t % 3600) / 60) << 5) | ((t % 60) / 2);
    (time as u16, ((((y - 1980) as u32) << 9) | (m << 5) | d) as u16)
}

impl ZipWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Names are relative paths: no leading slash, no `..`, no backslashes, no control characters.
    pub fn add(&mut self, name: &str, data: &[u8], unix_secs: i64) -> Result<(), String> {
        if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.split('/').any(|p| p == ".." || p.is_empty()) || name.chars().any(|c| c.is_control()) {
            return Err(format!("invalid file name in archive: {name:?}"));
        }
        if self.entries.len() >= 65_535 || name.len() > 65_535 {
            return Err("too many files for one archive".into());
        }
        let size = u32::try_from(data.len()).map_err(|_| "a file is larger than 4 GB".to_string())?;
        let offset = u32::try_from(self.buf.len()).map_err(|_| "archive larger than 4 GB".to_string())?;
        let (time, date) = dos(unix_secs);
        let e = Entry { name: name.as_bytes().to_vec(), crc: crc32(data), size, offset, time, date };
        let b = &mut self.buf;
        b.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        b.extend_from_slice(&20u16.to_le_bytes()); // version needed
        b.extend_from_slice(&0x0800u16.to_le_bytes()); // flags: UTF-8 names
        b.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        b.extend_from_slice(&e.time.to_le_bytes());
        b.extend_from_slice(&e.date.to_le_bytes());
        b.extend_from_slice(&e.crc.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes()); // compressed
        b.extend_from_slice(&size.to_le_bytes()); // uncompressed
        b.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes()); // extra length
        b.extend_from_slice(&e.name);
        b.extend_from_slice(data);
        self.entries.push(e);
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        let cd_start = u32::try_from(self.buf.len()).map_err(|_| "archive larger than 4 GB".to_string())?;
        for e in &self.entries {
            let b = &mut self.buf;
            b.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            b.extend_from_slice(&20u16.to_le_bytes()); // made by
            b.extend_from_slice(&20u16.to_le_bytes()); // version needed
            b.extend_from_slice(&0x0800u16.to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&e.time.to_le_bytes());
            b.extend_from_slice(&e.date.to_le_bytes());
            b.extend_from_slice(&e.crc.to_le_bytes());
            b.extend_from_slice(&e.size.to_le_bytes());
            b.extend_from_slice(&e.size.to_le_bytes());
            b.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes()); // extra
            b.extend_from_slice(&0u16.to_le_bytes()); // comment
            b.extend_from_slice(&0u16.to_le_bytes()); // disk
            b.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            b.extend_from_slice(&0u32.to_le_bytes()); // external attributes
            b.extend_from_slice(&e.offset.to_le_bytes());
            b.extend_from_slice(&e.name);
        }
        let cd_size = u32::try_from(self.buf.len() - cd_start as usize).map_err(|_| "archive larger than 4 GB".to_string())?;
        let n = self.entries.len() as u16;
        let b = &mut self.buf;
        b.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&cd_size.to_le_bytes());
        b.extend_from_slice(&cd_start.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        Ok(self.buf)
    }
}

/// Reads back an archive this module wrote (stored entries only). Used by tests, and cheap enough to keep public so
/// other tools in this repo can check an export without a zip library.
pub fn read_stored(zip: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let u16at = |i: usize| zip.get(i..i + 2).map(|x| u16::from_le_bytes([x[0], x[1]]) as usize).ok_or("truncated");
    let u32at = |i: usize| zip.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize).ok_or("truncated");
    let eocd = (0..zip.len().saturating_sub(21)).rev().find(|&i| zip[i..].starts_with(&0x0605_4b50u32.to_le_bytes())).ok_or("no end record")?;
    let (count, mut p) = (u16at(eocd + 10)?, u32at(eocd + 16)?);
    let mut out = Vec::new();
    for _ in 0..count {
        if u32at(p)? != 0x0201_4b50 {
            return Err("bad central directory".into());
        }
        let (crc, size, nlen, elen, clen, off) = (u32at(p + 16)?, u32at(p + 24)?, u16at(p + 28)?, u16at(p + 30)?, u16at(p + 32)?, u32at(p + 42)?);
        let name = String::from_utf8(zip.get(p + 46..p + 46 + nlen).ok_or("truncated")?.to_vec()).map_err(|_| "bad name")?;
        let (lnlen, lelen) = (u16at(off + 26)?, u16at(off + 28)?);
        let start = off + 30 + lnlen + lelen;
        let data = zip.get(start..start + size).ok_or("truncated data")?.to_vec();
        if crc32(&data) as usize != crc {
            return Err(format!("checksum mismatch in {name}"));
        }
        out.push((name, data));
        p += 46 + nlen + elen + clen;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn round_trip_and_name_checks() {
        let mut z = ZipWriter::new();
        z.add("notes.json", b"[]", 1_795_732_903).unwrap();
        z.add("attachments/1-caf\u{e9}.jpg", &[0xFF, 0xD8, 0, 1, 2], 0).unwrap();
        z.add("empty.txt", b"", 1_795_732_903).unwrap();
        let bytes = z.finish().unwrap();
        let files = read_stored(&bytes).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0], ("notes.json".to_string(), b"[]".to_vec()));
        assert_eq!(files[1].0, "attachments/1-caf\u{e9}.jpg");
        assert_eq!(files[1].1, vec![0xFF, 0xD8, 0, 1, 2]);
        assert!(files[2].1.is_empty());
        // damaged data is detected
        let mut bad = bytes.clone();
        bad[40] ^= 1;
        assert!(read_stored(&bad).is_err());
        for name in ["", "/etc/passwd", "../x", "a/../b", "a\\b", "a//b", "a\nb", "dir/"] {
            assert!(ZipWriter::new().add(name, b"x", 0).is_err(), "{name:?} accepted");
        }
        assert_eq!(read_stored(&ZipWriter::new().finish().unwrap()).unwrap().len(), 0);
    }

    #[test]
    fn dos_dates() {
        // 2026-11-26 22:41:42 UTC
        let (t, d) = dos(1_795_732_902);
        assert_eq!((d >> 9) + 1980, 2026);
        assert_eq!((d >> 5) & 0xF, 11);
        assert_eq!(d & 0x1F, 26);
        assert_eq!((t >> 11, (t >> 5) & 0x3F, (t & 0x1F) * 2), (22, 41, 42));
        assert_eq!(dos(0).1, (1 << 5) | 1, "before 1980 is clamped");
    }
}
