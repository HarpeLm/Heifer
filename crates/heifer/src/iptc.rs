//! IPTC-IIM metadata (IPTC Information Interchange Model, version 4).
//!
//! An IIM block is a sequence of datasets: a `0x1C` tag marker, a record number, a dataset
//! number, a length and the value. News and stock photos keep their caption, keywords, credit
//! and location there. [`read_iptc`](crate::read_iptc) returns the block of a HEIF file and
//! [`Iptc::parse`] reads it; every length comes from the file, so each one is checked against
//! the data before use.

use crate::Error;

/// One IIM dataset: its record and dataset numbers (e.g. 2:25 for a keyword) and its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dataset<'a> {
    /// Record number (1 envelope, 2 application).
    pub record: u8,
    /// Dataset number within the record.
    pub number: u8,
    /// Value bytes, in the character set the block declares (see [`Iptc::text`]).
    pub value: &'a [u8],
}

/// A parsed IPTC-IIM block: its datasets in file order, repeated ones included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Iptc<'a> {
    datasets: Vec<Dataset<'a>>,
    utf8: bool,
}

/// Dataset numbers of the application record (2) read by the accessors.
mod dataset {
    pub const KEYWORDS: u8 = 25;
    pub const DATE_CREATED: u8 = 55;
    pub const TIME_CREATED: u8 = 60;
    pub const BY_LINE: u8 = 80;
    pub const CITY: u8 = 90;
    pub const COUNTRY: u8 = 101;
    pub const COPYRIGHT: u8 = 116;
    pub const CAPTION: u8 = 120;
}

/// `ESC % G`: the 1:90 coded character set value that declares UTF-8.
const UTF8_MARKER: &[u8] = b"\x1b%G";

impl<'a> Iptc<'a> {
    /// Parses an IIM block. A Photoshop image resource block (`Photoshop 3.0` / `8BIM`) is
    /// also accepted: its IPTC resource (`0x0404`) is read.
    ///
    /// Reading stops at the first byte that is not a tag marker, as padding after the last
    /// dataset is common. A dataset whose length runs past the data is an error.
    pub fn parse(data: &'a [u8]) -> Result<Self, Error> {
        let data = photoshop_iptc(data).unwrap_or(data);
        let mut datasets = Vec::new();
        let mut at = 0usize;
        while data.get(at) == Some(&0x1C) {
            let header = data
                .get(at + 1..at + 5)
                .ok_or(Error::Invalid("truncated IPTC dataset header"))?;
            let (record, number) = (header[0], header[1]);
            let short = usize::from(u16::from_be_bytes([header[2], header[3]]));
            at += 5;
            let len = if short & 0x8000 == 0 {
                short
            } else {
                // Extended dataset: the low 15 bits give the size of the length field.
                let size = short & 0x7FFF;
                if size == 0 || size > 4 {
                    return Err(Error::Invalid("unsupported IPTC extended length"));
                }
                let field = data
                    .get(at..at + size)
                    .ok_or(Error::Invalid("truncated IPTC extended length"))?;
                at += size;
                field
                    .iter()
                    .fold(0usize, |acc, &b| (acc << 8) | usize::from(b))
            };
            let value = at
                .checked_add(len)
                .and_then(|end| data.get(at..end))
                .ok_or(Error::Invalid("IPTC dataset runs past the data"))?;
            at += len;
            datasets.push(Dataset {
                record,
                number,
                value,
            });
        }
        let utf8 = datasets
            .iter()
            .any(|d| d.record == 1 && d.number == 90 && d.value == UTF8_MARKER);
        Ok(Self { datasets, utf8 })
    }

    /// All datasets, in file order.
    pub fn datasets(&self) -> &[Dataset<'a>] {
        &self.datasets
    }

    /// Whether the block has no dataset.
    pub fn is_empty(&self) -> bool {
        self.datasets.is_empty()
    }

    /// The values of every `record:number` dataset, in file order.
    pub fn values(&self, record: u8, number: u8) -> impl Iterator<Item = &'a [u8]> + '_ {
        self.datasets
            .iter()
            .filter(move |d| d.record == record && d.number == number)
            .map(|d| d.value)
    }

    /// The first `record:number` value as text: UTF-8 when the block declares it (1:90) or the
    /// bytes are valid UTF-8, ISO 8859-1 otherwise. Trailing NUL bytes and spaces are trimmed.
    pub fn text(&self, record: u8, number: u8) -> Option<String> {
        self.values(record, number)
            .next()
            .map(|v| decode_text(v, self.utf8))
            .filter(|s| !s.is_empty())
    }

    /// Every `record:number` value as text, in file order (see [`text`](Self::text)).
    pub fn texts(&self, record: u8, number: u8) -> Vec<String> {
        self.values(record, number)
            .map(|v| decode_text(v, self.utf8))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Caption / abstract (2:120).
    pub fn caption(&self) -> Option<String> {
        self.text(2, dataset::CAPTION)
    }

    /// Keywords (2:25), in file order.
    pub fn keywords(&self) -> Vec<String> {
        self.texts(2, dataset::KEYWORDS)
    }

    /// By-line, the creator (2:80).
    pub fn by_line(&self) -> Option<String> {
        self.text(2, dataset::BY_LINE)
    }

    /// Copyright notice (2:116).
    pub fn copyright(&self) -> Option<String> {
        self.text(2, dataset::COPYRIGHT)
    }

    /// City (2:90).
    pub fn city(&self) -> Option<String> {
        self.text(2, dataset::CITY)
    }

    /// Country / primary location name (2:101).
    pub fn country(&self) -> Option<String> {
        self.text(2, dataset::COUNTRY)
    }

    /// Date created (2:55), as written: `CCYYMMDD`.
    pub fn date_created(&self) -> Option<String> {
        self.text(2, dataset::DATE_CREATED)
    }

    /// Time created (2:60), as written: `HHMMSS±HHMM`.
    pub fn time_created(&self) -> Option<String> {
        self.text(2, dataset::TIME_CREATED)
    }
}

/// Text of a value: UTF-8 when declared or valid, ISO 8859-1 otherwise.
fn decode_text(value: &[u8], utf8: bool) -> String {
    let text = match std::str::from_utf8(value) {
        Ok(s) => s.to_owned(),
        Err(_) if utf8 => String::from_utf8_lossy(value).into_owned(),
        Err(_) => value.iter().map(|&b| char::from(b)).collect(),
    };
    text.trim_end_matches(['\0', ' ']).to_owned()
}

/// The IPTC resource (`0x0404`) of a Photoshop image resource block, if `data` is one.
fn photoshop_iptc(data: &[u8]) -> Option<&[u8]> {
    let mut rest = data.strip_prefix(b"Photoshop 3.0\0").unwrap_or(data);
    while let Some(block) = rest.strip_prefix(b"8BIM") {
        let id = u16::from_be_bytes(block.get(..2)?.try_into().ok()?);
        // Pascal-string name, padded to an even size (length byte included).
        let name_len = usize::from(*block.get(2)?);
        let name_size = (name_len + 1).next_multiple_of(2);
        let size_at = 2 + name_size;
        let size = u32::from_be_bytes(block.get(size_at..size_at + 4)?.try_into().ok()?) as usize;
        let start = size_at + 4;
        let value = block.get(start..start.checked_add(size)?)?;
        if id == 0x0404 {
            return Some(value);
        }
        rest = block.get(start + size.next_multiple_of(2)..)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset(record: u8, number: u8, value: &[u8]) -> Vec<u8> {
        let mut d = vec![0x1C, record, number];
        d.extend_from_slice(&(value.len() as u16).to_be_bytes());
        d.extend_from_slice(value);
        d
    }

    fn sample() -> Vec<u8> {
        let mut d = dataset(1, 90, UTF8_MARKER);
        d.extend(dataset(2, 120, "Rue de la Paix, café".as_bytes()));
        d.extend(dataset(2, 25, b"Paris"));
        d.extend(dataset(2, 25, b"night"));
        d.extend(dataset(2, 80, b"A. Photographer"));
        d.extend(dataset(2, 116, b"(c) 2026"));
        d.extend(dataset(2, 90, b"Paris"));
        d.extend(dataset(2, 101, b"France"));
        d.extend(dataset(2, 55, b"20261105"));
        d.extend(dataset(2, 60, b"201500+0100"));
        d.extend([0, 0, 0]); // padding
        d
    }

    #[test]
    fn reads_common_datasets() {
        let data = sample();
        let iptc = Iptc::parse(&data).unwrap();
        assert_eq!(iptc.caption().as_deref(), Some("Rue de la Paix, café"));
        assert_eq!(iptc.keywords(), ["Paris", "night"]);
        assert_eq!(iptc.by_line().as_deref(), Some("A. Photographer"));
        assert_eq!(iptc.copyright().as_deref(), Some("(c) 2026"));
        assert_eq!(iptc.city().as_deref(), Some("Paris"));
        assert_eq!(iptc.country().as_deref(), Some("France"));
        assert_eq!(iptc.date_created().as_deref(), Some("20261105"));
        assert_eq!(iptc.time_created().as_deref(), Some("201500+0100"));
        assert_eq!(iptc.datasets().len(), 10);
    }

    #[test]
    fn latin1_without_utf8_marker() {
        let data = dataset(2, 120, b"caf\xe9");
        let iptc = Iptc::parse(&data).unwrap();
        assert_eq!(iptc.caption().as_deref(), Some("café"));
    }

    #[test]
    fn extended_length() {
        let mut data = vec![0x1C, 2, 120, 0x80, 0x02, 0x00, 0x03];
        data.extend_from_slice(b"abc");
        assert_eq!(
            Iptc::parse(&data).unwrap().caption().as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn photoshop_resource_block() {
        let iim = dataset(2, 25, b"tag");
        let mut data = b"Photoshop 3.0\08BIM".to_vec();
        data.extend_from_slice(&0x0404u16.to_be_bytes());
        data.extend_from_slice(&[0, 0]); // empty name, padded
        data.extend_from_slice(&(iim.len() as u32).to_be_bytes());
        data.extend_from_slice(&iim);
        assert_eq!(Iptc::parse(&data).unwrap().keywords(), ["tag"]);
    }

    #[test]
    fn rejects_truncated_data_without_panicking() {
        let data = sample();
        for end in 0..data.len() {
            let _ = Iptc::parse(&data[..end]);
        }
        assert!(Iptc::parse(&[0x1C, 2, 25, 0x00, 0x10, b'a']).is_err());
        assert!(Iptc::parse(&[0x1C, 2, 25, 0x80, 0x09]).is_err());
        assert!(Iptc::parse(&[0x1C, 2]).is_err());
        assert!(Iptc::parse(&[]).unwrap().is_empty());
    }
}
