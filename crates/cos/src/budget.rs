//! A shared allowance for operations that decode many streams.

use std::collections::HashSet;

use pdfcraft_filters::FilterError;

use crate::{CosError, Document, ObjRef, Object, Stream};

/// Decoded bytes one page's content walk may produce in all: its `/Contents` streams and every
/// form XObject it enters, each occurrence counted again.
pub const MAX_PAGE_DECODED: usize = 256 << 20;
/// Form XObjects one page's content walk may enter, each entry counted again.
pub const MAX_PAGE_FORMS: usize = 50_000;

/// What one operation may still decode and enter across all the streams it reads.
///
/// [`MAX_DECODED`](crate::MAX_DECODED) bounds one decode. It does not bound a page whose
/// `/Contents` names the same compressed stream thousands of times, or a form drawn thousands of
/// times from other forms: each occurrence would get a fresh allowance. A `DecodeBudget` is
/// shared by every decode of one operation, so the total decoded bytes and form entries stay
/// under fixed limits however the file repeats its references. A decode that runs past the
/// remaining allowance spends all of it, so a bomb referenced many times is inflated once.
#[derive(Debug)]
pub struct DecodeBudget {
    bytes: usize,
    forms: usize,
    seen: HashSet<ObjRef>,
}

impl DecodeBudget {
    /// An allowance of `bytes` decoded bytes and `forms` form entries.
    pub fn new(bytes: usize, forms: usize) -> Self {
        Self { bytes, forms, seen: HashSet::new() }
    }

    /// The allowance for one page: [`MAX_PAGE_DECODED`] bytes and [`MAX_PAGE_FORMS`] forms.
    pub fn page() -> Self {
        Self::new(MAX_PAGE_DECODED, MAX_PAGE_FORMS)
    }

    /// Decoded bytes still allowed.
    pub fn remaining(&self) -> usize {
        self.bytes
    }

    /// Whether a decode was refused for lack of allowance or a form entry for lack of visits.
    pub fn exhausted(&self) -> bool {
        self.bytes == 0 || self.forms == 0
    }

    /// `stream` decoded like [`Stream::decoded`], charged to the allowance. Fails, without
    /// decoding, once the allowance is spent.
    pub fn decode(&mut self, stream: &Stream) -> Result<Vec<u8>, CosError> {
        if self.bytes == 0 && !stream.raw.is_empty() {
            return Err(CosError::Filter(FilterError::LimitExceeded(0).to_string()));
        }
        match stream.decoded_within_typed(self.bytes) {
            Ok(data) => {
                self.bytes = self.bytes.saturating_sub(data.len());
                Ok(data)
            }
            Err(e) => {
                if matches!(e, FilterError::LimitExceeded(_)) {
                    self.bytes = 0;
                }
                Err(CosError::Filter(e.to_string()))
            }
        }
    }

    /// Counts one form entry. `false` once [`DecodeBudget::new`]'s `forms` entries are spent:
    /// the caller leaves that form out.
    pub fn enter_form(&mut self) -> bool {
        match self.forms.checked_sub(1) {
            Some(left) => {
                self.forms = left;
                true
            }
            None => false,
        }
    }

    /// `true` the first time `r` is passed to this budget, for callers that need each
    /// referenced stream only once.
    pub fn first_visit(&mut self, r: ObjRef) -> bool {
        self.seen.insert(r)
    }

    /// Each stream `contents` names, decoded against the allowance, in order. `contents` is a
    /// page's `/Contents`: one stream, or an array whose non-stream entries are left out.
    /// A stream that fails to decode, or arrives after the allowance is spent, is an `Err` in its
    /// place, so the caller chooses between skipping it and giving up.
    pub fn decode_contents(&mut self, doc: &Document, contents: &Object) -> Vec<Result<Vec<u8>, CosError>> {
        match &*doc.resolve(contents) {
            Object::Stream(s) => vec![self.decode(s)],
            Object::Array(list) => list
                .iter()
                .filter_map(|entry| match &*doc.resolve(entry) {
                    Object::Stream(s) => Some(self.decode(s)),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dict;

    #[test]
    fn repeated_references_share_one_allowance() {
        let mut doc = Document::new_empty();
        let bomb = doc.add(Object::Stream(Stream::flate(Dict::new(), &vec![b' '; 1 << 20])));
        let contents = Object::Array(vec![Object::Ref(bomb); 64]);
        let mut budget = DecodeBudget::new(10 << 20, 1);
        let decoded = budget.decode_contents(&doc, &contents);
        assert_eq!(decoded.len(), 64);
        assert_eq!(decoded.iter().filter(|d| d.is_ok()).count(), 10);
        let total: usize = decoded.iter().flatten().map(Vec::len).sum();
        assert_eq!(total, 10 << 20);
        assert!(budget.exhausted());
        // A decode past the allowance spends it, so a bomb is inflated at most once more.
        let mut budget = DecodeBudget::new((1 << 20) - 1, 1);
        assert!(budget.decode_contents(&doc, &contents).iter().all(Result::is_err));
        assert_eq!(budget.remaining(), 0);

        assert!(budget.enter_form());
        assert!(!budget.enter_form());
        assert!(budget.first_visit(bomb));
        assert!(!budget.first_visit(bomb));
    }
}
