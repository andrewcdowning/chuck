//! Validation of uploaded media. Untrusted bytes go in; out comes the detected type, the dimensions, and a copy
//! with metadata removed (EXIF, which carries GPS location from phones, plus XMP, IPTC and text comments).
//!
//! The type is detected from the file's own bytes, never from the client's claim, and only the formats below are
//! accepted. The parsers only walk container structure (segment, chunk and box headers) with bounds checks; they
//! never decode pixels.

pub const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_THUMB_BYTES: usize = 2 * 1024 * 1024;
const MAX_SIDE: u32 = 20_000;
const MAX_PIXELS: u64 = 100_000_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    /// The file with metadata removed. Same format as the input.
    pub bytes: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum MediaError {
    Unsupported,
    Malformed(&'static str),
    TooLarge,
}

impl std::fmt::Display for MediaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaError::Unsupported => write!(f, "unsupported file type: Chuck accepts JPEG, PNG, GIF and WebP images"),
            MediaError::Malformed(why) => write!(f, "the file is damaged or not a valid image ({why})"),
            MediaError::TooLarge => write!(f, "the file is too large"),
        }
    }
}

type R<T> = Result<T, MediaError>;

/// Detects the type from the bytes. Returns None for anything not on the allowlist.
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Validates an uploaded image and returns it with metadata stripped.
pub fn check_image(b: &[u8], max_bytes: usize) -> R<Checked> {
    if b.len() > max_bytes {
        return Err(MediaError::TooLarge);
    }
    let mime = sniff(b).ok_or(MediaError::Unsupported)?;
    let (width, height, bytes) = match mime {
        "image/jpeg" => jpeg(b)?,
        "image/png" => png(b)?,
        "image/gif" => gif(b)?,
        "image/webp" => webp(b)?,
        _ => return Err(MediaError::Unsupported),
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || width as u64 * height as u64 > MAX_PIXELS {
        return Err(MediaError::Malformed("dimensions out of range"));
    }
    Ok(Checked { mime, width, height, bytes })
}

fn be16(b: &[u8], i: usize) -> R<u32> {
    b.get(i..i + 2).map(|x| u16::from_be_bytes([x[0], x[1]]) as u32).ok_or(MediaError::Malformed("truncated"))
}
fn le16(b: &[u8], i: usize) -> R<u32> {
    b.get(i..i + 2).map(|x| u16::from_le_bytes([x[0], x[1]]) as u32).ok_or(MediaError::Malformed("truncated"))
}
fn be32(b: &[u8], i: usize) -> R<u32> {
    b.get(i..i + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]])).ok_or(MediaError::Malformed("truncated"))
}
fn le32(b: &[u8], i: usize) -> R<u32> {
    b.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]])).ok_or(MediaError::Malformed("truncated"))
}
fn le24(b: &[u8], i: usize) -> R<u32> {
    b.get(i..i + 3).map(|x| x[0] as u32 | (x[1] as u32) << 8 | (x[2] as u32) << 16).ok_or(MediaError::Malformed("truncated"))
}

/// JPEG: keeps JFIF (APP0), the color profile (APP2) and Adobe color info (APP14); drops every other APPn segment
/// (EXIF and XMP are APP1, IPTC is APP13) and comments. Image data after start-of-scan is copied unchanged.
///
/// Dropping EXIF also drops the orientation flag, so the uploader must send pixels already upright. The browser
/// client does: it redraws photos on a canvas, which applies the orientation.
fn jpeg(b: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    let mut out = Vec::with_capacity(b.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut i = 2;
    let mut dims = None;
    loop {
        // Markers may be padded with extra 0xFF bytes.
        if *b.get(i).ok_or(MediaError::Malformed("no image data"))? != 0xFF {
            return Err(MediaError::Malformed("bad marker"));
        }
        while b.get(i) == Some(&0xFF) {
            i += 1;
        }
        let m = *b.get(i).ok_or(MediaError::Malformed("truncated"))?;
        i += 1;
        if m == 0xD9 {
            return Err(MediaError::Malformed("no image data"));
        }
        if (0xD0..=0xD7).contains(&m) || m == 0x01 {
            out.extend_from_slice(&[0xFF, m]);
            continue;
        }
        let len = be16(b, i)? as usize;
        if len < 2 || i + len > b.len() {
            return Err(MediaError::Malformed("bad segment length"));
        }
        let seg = &b[i..i + len];
        let is_sof = matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
        if is_sof {
            if len < 7 {
                return Err(MediaError::Malformed("bad frame header"));
            }
            dims = Some((be16(seg, 5)?, be16(seg, 3)?));
        }
        let drop = matches!(m, 0xE1 | 0xE3..=0xED | 0xEF | 0xFE);
        if !drop {
            out.extend_from_slice(&[0xFF, m]);
            out.extend_from_slice(seg);
        }
        i += len;
        if m == 0xDA {
            // Start of scan: the rest is entropy-coded data (and any later markers), kept as is.
            out.extend_from_slice(&b[i..]);
            let (w, h) = dims.ok_or(MediaError::Malformed("no frame header"))?;
            return Ok((w, h, out));
        }
    }
}

/// PNG: drops text, EXIF and timestamp chunks; everything else, including the color profile, is kept.
fn png(b: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    let mut out = b[..8].to_vec();
    let mut i = 8;
    let mut dims = None;
    loop {
        let len = be32(b, i)? as usize;
        let ty = b.get(i + 4..i + 8).ok_or(MediaError::Malformed("truncated"))?;
        let end = i.checked_add(12 + len).ok_or(MediaError::Malformed("bad chunk length"))?;
        if end > b.len() {
            return Err(MediaError::Malformed("bad chunk length"));
        }
        if dims.is_none() {
            if ty != b"IHDR" || len < 8 {
                return Err(MediaError::Malformed("missing header"));
            }
            dims = Some((be32(b, i + 8)?, be32(b, i + 12)?));
        }
        let drop = matches!(ty, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME");
        if !drop {
            out.extend_from_slice(&b[i..end]);
        }
        if ty == b"IEND" {
            let (w, h) = dims.unwrap();
            return Ok((w, h, out));
        }
        i = end;
    }
}

/// GIF: dimensions from the logical screen descriptor. GIFs carry no location data, so the file is kept as is.
fn gif(b: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    Ok((le16(b, 6)?, le16(b, 8)?, b.to_vec()))
}

/// WebP: drops the EXIF and XMP chunks, clears their flags in the extended header, and fixes the RIFF size.
fn webp(b: &[u8]) -> R<(u32, u32, Vec<u8>)> {
    let riff = le32(b, 4)? as usize;
    if riff + 8 > b.len() || riff < 4 {
        return Err(MediaError::Malformed("bad RIFF size"));
    }
    let body = &b[12..riff + 8];
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut dims = None;
    let mut i = 0;
    while i < body.len() {
        let fourcc: [u8; 4] = body.get(i..i + 4).ok_or(MediaError::Malformed("truncated"))?.try_into().unwrap();
        let size = le32(body, i + 4)? as usize;
        let padded = size + (size & 1);
        let data = body.get(i + 8..i + 8 + size).ok_or(MediaError::Malformed("bad chunk size"))?;
        match &fourcc {
            b"VP8X" if size >= 10 => dims = dims.or(Some((le24(data, 4)? + 1, le24(data, 7)? + 1))),
            b"VP8L" if size >= 5 => {
                if data[0] != 0x2F {
                    return Err(MediaError::Malformed("bad lossless header"));
                }
                let v = le32(data, 1)?;
                dims = dims.or(Some(((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1)));
            }
            b"VP8 " if size >= 10 => {
                if data[3..6] != [0x9D, 0x01, 0x2A] {
                    return Err(MediaError::Malformed("bad lossy header"));
                }
                dims = dims.or(Some((le16(data, 6)? & 0x3FFF, le16(data, 8)? & 0x3FFF)));
            }
            _ => {}
        }
        if &fourcc != b"EXIF" && &fourcc != b"XMP " {
            let mut c = body[i..i + 8 + size].to_vec();
            if &fourcc == b"VP8X" && !c.is_empty() {
                c[8] &= !(0x08 | 0x04); // EXIF and XMP present flags
            }
            if size & 1 == 1 {
                c.push(0);
            }
            chunks.push(c);
        }
        i += 8 + padded;
    }
    let (w, h) = dims.ok_or(MediaError::Malformed("no image chunk"))?;
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(12 + total);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + total) as u32).to_le_bytes());
    out.extend_from_slice(b"WEBP");
    for c in chunks {
        out.extend_from_slice(&c);
    }
    Ok((w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    /// A structurally valid JPEG (no real pixels) with EXIF GPS, XMP, IPTC, a comment and a color profile.
    fn jpeg_with_metadata() -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        j.extend(seg(0xE1, b"Exif\0\0GPSLatitude 40.7128 N"));
        j.extend(seg(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta/>"));
        j.extend(seg(0xE2, b"ICC_PROFILE\0\x01\x01profile"));
        j.extend(seg(0xED, b"Photoshop 3.0\0IPTC"));
        j.extend(seg(0xFE, b"a comment"));
        j.extend(seg(0xDB, &[0u8; 65]));
        j.extend(seg(0xC0, &[8, 0x01, 0xE0, 0x02, 0x80, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1])); // 640x480
        j.extend(seg(0xC4, &[0u8; 20]));
        j.extend(seg(0xDA, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 0x3F, 0]));
        j.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD9]);
        j
    }

    #[test]
    fn jpeg_strips_metadata_and_reads_size() {
        let j = jpeg_with_metadata();
        let c = check_image(&j, MAX_IMAGE_BYTES).unwrap();
        assert_eq!((c.mime, c.width, c.height), ("image/jpeg", 640, 480));
        let s = String::from_utf8_lossy(&c.bytes);
        for gone in ["GPSLatitude", "xmpmeta", "IPTC", "a comment"] {
            assert!(!s.contains(gone), "{gone} survived");
        }
        assert!(s.contains("JFIF") && s.contains("ICC_PROFILE"));
        assert!(c.bytes.ends_with(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0xFF, 0xD9]), "scan data must be untouched");
        // stripping twice changes nothing
        assert_eq!(check_image(&c.bytes, MAX_IMAGE_BYTES).unwrap().bytes, c.bytes);
    }

    fn chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0, 0, 0, 0]); // CRC is not checked here; browsers do that
        v
    }

    #[test]
    fn png_strips_text_and_exif() {
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = 1920u32.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&1080u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        p.extend(chunk(b"IHDR", &ihdr));
        p.extend(chunk(b"eXIf", b"GPS secret"));
        p.extend(chunk(b"tEXt", b"Author\0me"));
        p.extend(chunk(b"iCCP", b"profile"));
        p.extend(chunk(b"IDAT", b"pixels"));
        p.extend(chunk(b"IEND", b""));
        let c = check_image(&p, MAX_IMAGE_BYTES).unwrap();
        assert_eq!((c.mime, c.width, c.height), ("image/png", 1920, 1080));
        let s = String::from_utf8_lossy(&c.bytes);
        assert!(!s.contains("GPS secret") && !s.contains("Author"));
        assert!(s.contains("iCCP") && s.contains("IDAT") && s.contains("IEND"));
    }

    fn riff(chunks: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        for (cc, d) in chunks {
            body.extend_from_slice(*cc);
            body.extend_from_slice(&(d.len() as u32).to_le_bytes());
            body.extend_from_slice(d);
            if d.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend(body);
        v
    }

    #[test]
    fn webp_strips_exif_and_xmp() {
        let mut vp8x = vec![0x08 | 0x04 | 0x10, 0, 0, 0];
        vp8x.extend_from_slice(&[0x7F, 0x07, 0x00, 0x37, 0x04, 0x00]); // 1920 x 1080 (stored minus one)
        let w = riff(&[(b"VP8X", vp8x), (b"VP8L", vec![0x2F, 0, 0, 0, 0]), (b"EXIF", b"GPS here".to_vec()), (b"XMP ", b"<xmp/>".to_vec())]);
        let c = check_image(&w, MAX_IMAGE_BYTES).unwrap();
        assert_eq!((c.mime, c.width, c.height), ("image/webp", 1920, 1080));
        let s = String::from_utf8_lossy(&c.bytes);
        assert!(!s.contains("GPS here") && !s.contains("<xmp/>"));
        assert_eq!(c.bytes[20] & (0x08 | 0x04), 0, "EXIF and XMP flags cleared");
        assert_eq!(c.bytes[20] & 0x10, 0x10, "alpha flag kept");
        assert_eq!(u32::from_le_bytes(c.bytes[4..8].try_into().unwrap()) as usize, c.bytes.len() - 8);
        // simple lossy WebP
        let lossy = riff(&[(b"VP8 ", vec![0, 0, 0, 0x9D, 0x01, 0x2A, 0x80, 0x02, 0xE0, 0x01])]);
        let c = check_image(&lossy, MAX_IMAGE_BYTES).unwrap();
        assert_eq!((c.width, c.height), (640, 480));
    }

    #[test]
    fn gif_size() {
        let mut g = b"GIF89a".to_vec();
        g.extend_from_slice(&[0x40, 0x01, 0xF0, 0x00, 0, 0, 0]);
        let c = check_image(&g, MAX_IMAGE_BYTES).unwrap();
        assert_eq!((c.mime, c.width, c.height), ("image/gif", 320, 240));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(check_image(b"<svg onload=alert(1)>", MAX_IMAGE_BYTES), Err(MediaError::Unsupported));
        assert_eq!(check_image(b"%PDF-1.7", MAX_IMAGE_BYTES), Err(MediaError::Unsupported));
        assert_eq!(check_image(b"", MAX_IMAGE_BYTES), Err(MediaError::Unsupported));
        assert_eq!(check_image(&jpeg_with_metadata(), 10), Err(MediaError::TooLarge));
        // truncated or corrupt structures are rejected, never panic
        let j = jpeg_with_metadata();
        for cut in [3, 6, 20, 60, 120] {
            assert!(check_image(&j[..cut], MAX_IMAGE_BYTES).is_err(), "cut at {cut}");
        }
        assert!(check_image(b"\x89PNG\r\n\x1a\n\xff\xff\xff\xffIHDR", MAX_IMAGE_BYTES).is_err());
        assert!(check_image(b"RIFF\xff\xff\xff\x7fWEBPVP8X", MAX_IMAGE_BYTES).is_err());
        assert!(check_image(b"GIF89a\x00\x00\x00\x00", MAX_IMAGE_BYTES).is_err(), "zero size");
        assert!(check_image(b"GIF89a\x20\x4e\x20\x4e", MAX_IMAGE_BYTES).is_err(), "too many pixels");
    }
}
