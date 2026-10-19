//! Picture clauses that list alternatives (`date{YYYYMMDD}|date{YYMMDD}`): a date may be typed
//! by any of them, and is shown, and kept in the field, by the first.

use crate::data::{date_letters, iso_to_pattern, pattern_to_iso, template_field};
use crate::layout::date_pattern;
use crate::model::{Template, Ui};

/// The alternatives of a picture clause: its parts between `|`s that aren't inside braces or
/// quotes.
pub(crate) fn alternatives(picture: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quoted, mut start) = (0usize, false, 0);
    for (i, c) in picture.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '{' if !quoted => depth = depth.saturating_add(1),
            '}' if !quoted => depth = depth.saturating_sub(1),
            '|' if !quoted && depth == 0 => {
                out.extend(picture.get(start..i));
                start = i + 1;
            }
            _ => {}
        }
    }
    out.extend(picture.get(start..));
    out
}

/// `text` typed into the date field at `som` by one of its edit picture's alternatives after the
/// first, written in the first, which the field shows and checks typed dates against (`261018`
/// becomes `20261018` for `date{YYYYMMDD}|date{YYMMDD}`). A pattern reads the text only when the
/// date it gives, written in that pattern, is the text again. `None` when the field isn't a date
/// field with alternatives, the first pattern reads the text, or no other does.
pub fn typed_date(tpl: &Template, som: &str, text: &str) -> Option<String> {
    let f = template_field(tpl, som)?;
    if f.ui != Ui::DateTimeEdit {
        return None;
    }
    let picture = f.picture.as_deref()?;
    let reads = |pattern: &str| pattern_to_iso(text, pattern).filter(|iso| iso_to_pattern(iso, pattern).eq_ignore_ascii_case(text.trim()));
    let first = date_pattern(Some(picture));
    if reads(&first).is_some() {
        return None;
    }
    let iso = alternatives(picture).into_iter().skip(1).filter_map(date_letters).find_map(|p| reads(&p))?;
    Some(iso_to_pattern(&iso, &first))
}
