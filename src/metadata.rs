//! Song / episode metadata (title, artist, album, cover art…) and chapters, written into
//! exported files:
//!
//! | Format | Tags | Cover | Chapters |
//! |---|---|---|---|
//! | MP3  | ID3v2.4 | APIC | CHAP + CTOC (podcast apps) |
//! | FLAC | Vorbis comments | PICTURE block | `CHAPTERnnn` / `CHAPTERnnnNAME` |
//! | Ogg  | Vorbis comments | `METADATA_BLOCK_PICTURE` | `CHAPTERnnn` / `CHAPTERnnnNAME` |
//! | WAV  | RIFF `LIST/INFO` | – | – |

use std::{fs::OpenOptions, io::Write, path::Path};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub album_artist: String,
    #[serde(default)]
    pub year: String,
    #[serde(default)]
    pub genre: String,
    /// Track number, optionally "n/total".
    #[serde(default)]
    pub track: String,
    #[serde(default)]
    pub comment: String,
    #[serde(default)]
    pub cover: Option<Cover>,
}

/// Cover art (PNG or JPEG bytes).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cover {
    pub mime: String,
    #[serde(with = "b64")]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chapter {
    pub start_ms: u32,
    pub end_ms: u32,
    pub title: String,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        *self == Metadata::default()
    }

    fn fields(&self) -> [(&'static str, &str); 7] {
        [
            ("TITLE", &self.title),
            ("ARTIST", &self.artist),
            ("ALBUM", &self.album),
            ("ALBUMARTIST", &self.album_artist),
            ("DATE", &self.year),
            ("GENRE", &self.genre),
            ("COMMENT", &self.comment),
        ]
    }

    fn track_parts(&self) -> (Option<u32>, Option<u32>) {
        let mut it = self.track.split('/');
        let n = it.next().and_then(|s| s.trim().parse().ok());
        let total = it.next().and_then(|s| s.trim().parse().ok());
        (n, total)
    }

    /// Vorbis-comment pairs (used by FLAC and Ogg).
    pub fn vorbis_comments(&self, chapters: &[Chapter]) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .fields()
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .map(|(k, v)| (k.to_string(), v.trim().to_string()))
            .collect();
        let (n, total) = self.track_parts();
        if let Some(n) = n {
            out.push(("TRACKNUMBER".into(), n.to_string()));
        }
        if let Some(t) = total {
            out.push(("TRACKTOTAL".into(), t.to_string()));
        }
        for (i, c) in chapters.iter().enumerate() {
            let ms = c.start_ms;
            out.push((
                format!("CHAPTER{:03}", i + 1),
                format!(
                    "{:02}:{:02}:{:02}.{:03}",
                    ms / 3_600_000,
                    ms / 60_000 % 60,
                    ms / 1000 % 60,
                    ms % 1000
                ),
            ));
            out.push((format!("CHAPTER{:03}NAME", i + 1), c.title.clone()));
        }
        out
    }
}

/// Recognise PNG / JPEG and return (mime, width, height).
pub fn image_info(data: &[u8]) -> Option<(&'static str, u32, u32)> {
    if data.len() > 24 && data.starts_with(b"\x89PNG\r\n\x1a\n") {
        let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
        return Some(("image/png", w, h));
    }
    if data.len() > 4 && data[0] == 0xFF && data[1] == 0xD8 {
        // Walk JPEG segments to the first start-of-frame.
        let mut i = 2;
        while i + 9 < data.len() {
            if data[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = data[i + 1];
            let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
                return Some(("image/jpeg", w, h));
            }
            i += 2 + len;
        }
        return Some(("image/jpeg", 0, 0));
    }
    None
}

/// Load a cover image file (PNG or JPEG).
pub fn load_cover(path: &Path) -> Result<Cover> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let (mime, _, _) =
        image_info(&data).ok_or_else(|| anyhow!("cover art must be a PNG or JPEG image"))?;
    if data.len() > 8 * 1024 * 1024 {
        return Err(anyhow!(
            "the cover image is over 8 MB; use a smaller one (1400–3000 px is typical)"
        ));
    }
    Ok(Cover {
        mime: mime.into(),
        data,
    })
}

/// FLAC PICTURE block body (also used base64-encoded in Ogg Vorbis comments).
fn flac_picture_block(c: &Cover) -> Vec<u8> {
    let (_, w, h) = image_info(&c.data).unwrap_or(("", 0, 0));
    let mut b = Vec::with_capacity(c.data.len() + 64);
    b.extend_from_slice(&3u32.to_be_bytes()); // front cover
    b.extend_from_slice(&(c.mime.len() as u32).to_be_bytes());
    b.extend_from_slice(c.mime.as_bytes());
    b.extend_from_slice(&0u32.to_be_bytes()); // description
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&24u32.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&(c.data.len() as u32).to_be_bytes());
    b.extend_from_slice(&c.data);
    b
}

/// Comments for an Ogg Vorbis stream, including the cover picture.
pub fn ogg_comments(meta: &Metadata, chapters: &[Chapter]) -> Vec<(String, String)> {
    let mut v = meta.vorbis_comments(chapters);
    if let Some(c) = &meta.cover {
        v.push((
            "METADATA_BLOCK_PICTURE".into(),
            b64::encode(&flac_picture_block(c)),
        ));
    }
    v
}

/// An ID3v2.4 tag to put in front of MP3 data.
pub fn id3_tag(meta: &Metadata, chapters: &[Chapter]) -> Result<Vec<u8>> {
    use id3::{
        Frame, TagLike, Version,
        frame::{Chapter as Chap, Comment, Picture, PictureType, TableOfContents},
    };
    let mut tag = id3::Tag::new();
    let set = |tag: &mut id3::Tag, id: &str, v: &str| {
        if !v.trim().is_empty() {
            tag.set_text(id, v.trim());
        }
    };
    set(&mut tag, "TIT2", &meta.title);
    set(&mut tag, "TPE1", &meta.artist);
    set(&mut tag, "TALB", &meta.album);
    set(&mut tag, "TPE2", &meta.album_artist);
    set(&mut tag, "TDRC", &meta.year);
    set(&mut tag, "TCON", &meta.genre);
    set(&mut tag, "TRCK", &meta.track);
    set(&mut tag, "TSSE", "Rust Audacity");
    if !meta.comment.trim().is_empty() {
        tag.add_frame(Comment {
            lang: "eng".into(),
            description: String::new(),
            text: meta.comment.trim().into(),
        });
    }
    if let Some(c) = &meta.cover {
        tag.add_frame(Picture {
            mime_type: c.mime.clone(),
            picture_type: PictureType::CoverFront,
            description: "Cover".into(),
            data: c.data.clone(),
        });
    }
    if !chapters.is_empty() {
        let ids: Vec<String> = (0..chapters.len()).map(|i| format!("chp{i}")).collect();
        tag.add_frame(TableOfContents {
            element_id: "toc".into(),
            top_level: true,
            ordered: true,
            elements: ids.clone(),
            frames: Vec::new(),
        });
        for (c, id) in chapters.iter().zip(ids) {
            tag.add_frame(Chap {
                element_id: id,
                start_time: c.start_ms,
                end_time: c.end_ms,
                start_offset: 0xFFFF_FFFF,
                end_offset: 0xFFFF_FFFF,
                frames: vec![Frame::text("TIT2", c.title.clone())],
            });
        }
    }
    let mut out = Vec::new();
    tag.write_to(&mut out, Version::Id3v24)
        .map_err(|e| anyhow!("could not write the MP3 tags: {e}"))?;
    Ok(out)
}

/// Insert tags, cover and chapters into a FLAC stream produced by the encoder.
///
/// The VORBIS_COMMENT block is written by hand so comments keep their order: readers such
/// as ffmpeg need `CHAPTERnnn` before `CHAPTERnnnNAME`.
pub fn tag_flac_bytes(flac: &mut Vec<u8>, meta: &Metadata, chapters: &[Chapter]) -> Result<()> {
    if flac.len() < 42 || &flac[..4] != b"fLaC" || flac[4] & 0x7f != 0 {
        return Err(anyhow!("unexpected FLAC layout"));
    }
    // Is STREAMINFO the last metadata block? Find where the metadata ends.
    let mut pos = 4;
    loop {
        let last = flac[pos] & 0x80 != 0;
        let len = u32::from_be_bytes([0, flac[pos + 1], flac[pos + 2], flac[pos + 3]]) as usize;
        if last {
            flac[pos] &= 0x7f;
            pos += 4 + len;
            break;
        }
        pos += 4 + len;
        if pos + 4 > flac.len() {
            return Err(anyhow!("unexpected FLAC layout"));
        }
    }
    let mut vc = Vec::new();
    let vendor = b"Rust Audacity";
    vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    vc.extend_from_slice(vendor);
    let comments = meta.vorbis_comments(chapters);
    vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (k, v) in comments {
        let entry = format!("{k}={v}");
        vc.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        vc.extend_from_slice(entry.as_bytes());
    }
    let mut blocks: Vec<(u8, Vec<u8>)> = vec![(4, vc)];
    if let Some(c) = &meta.cover {
        blocks.push((6, flac_picture_block(c)));
    }
    blocks.push((1, vec![0; 1024])); // padding for later tag edits
    let mut ins = Vec::new();
    let n = blocks.len();
    for (i, (ty, body)) in blocks.into_iter().enumerate() {
        if body.len() >= 1 << 24 {
            return Err(anyhow!("cover art is too large for FLAC (max 16 MB)"));
        }
        ins.push(if i + 1 == n { ty | 0x80 } else { ty });
        ins.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        ins.extend_from_slice(&body);
    }
    flac.splice(pos..pos, ins);
    Ok(())
}

/// Append a RIFF `LIST/INFO` chunk to a finished WAV file.
pub fn tag_wav(path: &Path, meta: &Metadata) -> Result<()> {
    let items = [
        (b"INAM", &meta.title),
        (b"IART", &meta.artist),
        (b"IPRD", &meta.album),
        (b"ICRD", &meta.year),
        (b"IGNR", &meta.genre),
        (b"ITRK", &meta.track),
        (b"ICMT", &meta.comment),
    ];
    let mut info = Vec::new();
    for (id, v) in items {
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        let mut bytes = v.as_bytes().to_vec();
        bytes.push(0);
        info.extend_from_slice(id);
        info.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        info.extend_from_slice(&bytes);
        if bytes.len() % 2 == 1 {
            info.push(0);
        }
    }
    if info.is_empty() {
        return Ok(());
    }
    let mut chunk = b"LIST".to_vec();
    chunk.extend_from_slice(&((info.len() + 4) as u32).to_le_bytes());
    chunk.extend_from_slice(b"INFO");
    chunk.extend_from_slice(&info);
    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let len = f.metadata()?.len();
    if len % 2 == 1 {
        f.write_all(&[0])?;
    }
    use std::io::{Seek, SeekFrom};
    f.seek(SeekFrom::End(0))?;
    f.write_all(&chunk)?;
    let riff_size = (f.metadata()?.len() - 8) as u32;
    f.seek(SeekFrom::Start(4))?;
    f.write_all(&riff_size.to_le_bytes())?;
    Ok(())
}

/// Minimal base64 (standard alphabet, padded) for cover art in project files and Ogg tags.
pub mod b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn decode(s: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(s.len() / 4 * 3);
        let mut buf = 0u32;
        let mut bits = 0;
        for c in s.bytes().filter(|c| !c.is_ascii_whitespace()) {
            if c == b'=' {
                break;
            }
            let v = T.iter().position(|&t| t == c).ok_or("invalid base64")? as u32;
            buf = buf << 6 | v;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((buf >> bits) as u8);
            }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(data: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&encode(data))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map_err(serde::de::Error::custom)
    }
}

/// A tiny valid PNG for tests.
#[cfg(test)]
pub(crate) fn tests_png() -> Vec<u8> {
    tests::tiny_png()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny valid 2×3 PNG.
    pub(crate) fn tiny_png() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 3);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[200; 18]).unwrap();
        }
        out
    }

    #[test]
    fn base64_and_images() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(
                b64::decode(&b64::encode(s.as_bytes())).unwrap(),
                s.as_bytes()
            );
        }
        assert_eq!(b64::encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(image_info(&tiny_png()), Some(("image/png", 2, 3)));
        assert_eq!(
            image_info(b"GIF89a.........................").map(|x| x.0),
            None
        );
    }

    #[test]
    fn comments_and_project_json() {
        let m = Metadata {
            title: "Ep 1".into(),
            track: "3/12".into(),
            cover: Some(Cover {
                mime: "image/png".into(),
                data: tiny_png(),
            }),
            ..Default::default()
        };
        let c = m.vorbis_comments(&[Chapter {
            start_ms: 61_500,
            end_ms: 90_000,
            title: "News".into(),
        }]);
        assert!(c.contains(&("TITLE".into(), "Ep 1".into())));
        assert!(c.contains(&("TRACKNUMBER".into(), "3".into())));
        assert!(c.contains(&("TRACKTOTAL".into(), "12".into())));
        assert!(c.contains(&("CHAPTER001".into(), "00:01:01.500".into())));
        assert!(c.contains(&("CHAPTER001NAME".into(), "News".into())));
        let json = serde_json::to_string(&m).unwrap();
        let back: Metadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
