//! MPEG-2 / DVB **descriptor** loops (ISO/IEC 13818-1 §2.6, ETSI EN 300 468 §6).
//!
//! Descriptors are the extensible `tag` + `length` + `payload` triples that hang
//! off the program loops of a [`super::pmt`], the transport-stream loop of a
//! [`super::nit`], the service loop of an [`super::sdt`], and elsewhere. This
//! module parses a descriptor **loop** into a bounded list of borrowed
//! [`Descriptor`]s without interpreting their payloads (each table extracts the
//! few descriptors it cares about).

use super::MpegTsError;

/// One raw descriptor: an 8-bit tag, plus its payload bytes (the 8-bit length is
/// consumed during parsing and reflected by `data.len()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor<'a> {
    /// The descriptor tag (e.g. `0x48` service descriptor, `0x09` CA descriptor).
    pub tag: u8,
    /// The descriptor payload (exactly `descriptor_length` bytes).
    pub data: &'a [u8],
}

/// A parsed descriptor loop: an in-order, bounded collection of [`Descriptor`]s.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Descriptors<'a> {
    items: Vec<Descriptor<'a>>,
}

/// Upper bound on the number of descriptors parsed from a single loop.
///
/// A descriptor loop is itself length-bounded (≤ 4093 bytes) and each descriptor
/// is ≥ 2 bytes, so a loop can hold at most ~2046 descriptors; this cap is a
/// belt-and-braces guard so a crafted loop cannot force an unbounded `Vec`.
pub const MAX_DESCRIPTORS: usize = 4096;

impl<'a> Descriptors<'a> {
    /// Parse a descriptor loop occupying the whole of `bytes`.
    ///
    /// Walks `tag` + `length` + payload triples until `bytes` is exhausted.
    ///
    /// # Errors
    ///
    /// * [`MpegTsError::Overrun`] when a descriptor's declared length runs past
    ///   the end of the loop.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, MpegTsError> {
        let mut items = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            if items.len() >= MAX_DESCRIPTORS {
                return Err(MpegTsError::Overrun {
                    declared: items.len(),
                    available: MAX_DESCRIPTORS,
                });
            }
            let tag = *bytes.get(offset).ok_or(MpegTsError::Overrun {
                declared: offset.saturating_add(1),
                available: bytes.len(),
            })?;
            let len_index = offset.checked_add(1).ok_or(MpegTsError::Overrun {
                declared: offset,
                available: bytes.len(),
            })?;
            let length = usize::from(*bytes.get(len_index).ok_or(MpegTsError::Overrun {
                declared: len_index.saturating_add(1),
                available: bytes.len(),
            })?);
            let data_start = len_index.checked_add(1).ok_or(MpegTsError::Overrun {
                declared: len_index,
                available: bytes.len(),
            })?;
            let data_end = data_start.checked_add(length).ok_or(MpegTsError::Overrun {
                declared: length,
                available: bytes.len(),
            })?;
            let data = bytes
                .get(data_start..data_end)
                .ok_or(MpegTsError::Overrun {
                    declared: data_end,
                    available: bytes.len(),
                })?;
            items.push(Descriptor { tag, data });
            offset = data_end;
        }
        Ok(Self { items })
    }

    /// The descriptors in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[Descriptor<'a>] {
        &self.items
    }

    /// The number of descriptors parsed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the loop carried no descriptors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The first descriptor with the given `tag`, if any.
    #[must_use]
    pub fn find(&self, tag: u8) -> Option<&Descriptor<'a>> {
        self.items.iter().find(|d| d.tag == tag)
    }
}
