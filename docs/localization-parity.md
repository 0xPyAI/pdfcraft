# Localization parity

> **Last reviewed:** 2026-10-10 · **Last updated:** 2026-10-13 · **Change:** minor (Hindi and Korean ship as full 2,578-key catalogs, after Indonesian and Vietnamese; all twelve key languages now have one; the localization dimension is estimated at ≈ 93%) · **Target:** Adobe Acrobat Pro (Acrobat DC, continuous track 26.002.21931, macOS)

Per-language status of PdfCraft's interface against Acrobat Pro's. How translations work and how to
add a language: [localization.md](localization.md). Summary in [ROADMAP.md](../ROADMAP.md).

## How this was measured

- **Strings translated** is measured: the catalogs in `crates/ui-egui/src/i18n/<code>.tsv`, counted
  as distinct `(context, English)` keys with a non-empty translation. The denominator is the union
  of keys across all catalogs, **2,578** (it was 2,143 until the Lithuanian catalog arrived; the
  percentages in the table below for the languages measured on 2026-10-10 are that older
  snapshot's). Cross-check: of the `tl!("…")` literals in `crates/ui-egui/src`, Indonesian,
  Vietnamese, Hindi and Korean translate every one of them (required by
  `indonesian_and_vietnamese_cover_ui_literals` and `hindi_and_korean_cover_ui_literals`),
  and two literals are in no catalog yet.
  English is built in.
- **Not counted:** error details from the engine and the operating system, which are shown in
  English inside a translated frame; there is no translated user documentation or help yet (Acrobat
  ships localized help).
- **Script support** comes from [localization.md](localization.md), the vendored winit IME patches
  (`vendor/README.md`) and user reports.
- **Native review**: "community" means a native speaker contributed or corrected the catalog in a
  pull request or issue; "no" means no record of review. No catalog has had a formal review.
- **Acrobat** ships 24 interface localizations on macOS (`.lproj`: cs, da, de, en, es, fi, fr, hu,
  it, ja, ko, nl, no, pl, pt, ru, sv, tr, uk, zh_CN, zh_TW, plus en_AE, en_IL, fr_MA) and Middle
  East / North Africa editions (MEA Arabic, MEH Hebrew, NAF French; `Resources/Sequences/`).

## The twelve key languages

| Language | Code | UI strings translated | Dialogs / tooltips / help | Script support | Native review | Status | Acrobat ships it | To `full` (h) |
|---|---|---|---|---|---|---|---|---|
| English | `en` | built in (100%) | all; README and docs | Latin | yes | **full** | yes | 0 |
| Simplified Chinese | `zh-hans` | 2,088 (97.4%) | dialogs and tooltips through `tl!`; no help | CJK text and IME work, but **release builds have no Hans face**: many characters show as boxes (#826, #688, #728, #758; Flatpak #689) | no | partial | yes | 6–12 |
| Spanish | `es` | 2,046 (95.5%) | dialogs and tooltips; no help | Latin | no | partial | yes | 2–4 |
| Hindi | `hi` | 2,578 (100%) | dialogs and tooltips through `tl!`; no help | Devanagari drawn by the craft-fonts Noto Sans Devanagari face (`Deva`, kept in the web build); desktop builds without it fall back to one installed face (`SYSTEM_DEVANAGARI_FALLBACK`); tested in `crates/ui-egui/tests/fonts.rs` | no | partial | no | 2–4 |
| Arabic | `ar` | 2,143 (100%) | dialogs and tooltips; no help | Letters joined and lines reordered at load, but **layout not mirrored**, wrapped lines break at the wrong end, typed text runs left to right (`localization.md` §Arabic) | no | partial | yes (Middle East edition) | 25–45 |
| French | `fr` | 2,128 (99.3%) | dialogs and tooltips; no help | Latin | no | partial | yes | 1–2 |
| Portuguese (Brazil) | `pt-br` | 2,130 (99.4%) | dialogs and tooltips; no help | Latin | community (#288) | partial | yes | 1–2 |
| Indonesian | `id` | 2,578 (100%) | dialogs and tooltips through `tl!`; no help | Latin, drawn by the bundled Inter and JetBrains Mono (tested in `crates/ui-egui/tests/fonts.rs`) | no | partial | no | 1–2 |
| Japanese | `ja` | 2,123 (99.1%) | dialogs and tooltips; no help | BIZ UDPGothic from craft-fonts; IME patched on macOS, Windows, X11, Wayland; vertical text renders but can't be edited | no | partial | yes | 4–8 |
| German | `de` | 2,128 (99.3%) | dialogs and tooltips; no help | Latin | community, in progress (#828: terminology, clipping) | partial | yes | 2–4 |
| Korean | `ko` | 2,578 (100%) | dialogs and tooltips through `tl!`; no help | Hangul IME patched (Microsoft and Apple Korean, `vendor/README.md` winit 3, 6, 9, 10); **no Hangul face in craft-fonts**, so desktop builds draw Korean with one installed face (`SYSTEM_HANGUL_FALLBACK`: Malgun Gothic, Apple SD Gothic Neo, Nanum) and the web build and machines without one show boxes | no | partial | yes | 4–8 |
| Vietnamese | `vi` | 2,578 (100%) | dialogs and tooltips through `tl!`; no help | Latin with stacked diacritics; the bundled Inter covers them, tested in `crates/ui-egui/tests/fonts.rs` | no | partial | no | 1–2 |

**Localization dimension ≈ 93%** (estimated, not re-measured): the mean over the twelve, counting a
catalog's percentage but only 60% for Chinese and Arabic until their script problems are fixed. All
twelve now have a catalog. **≈ 45–75 h** to bring all twelve to `full`, of which ≈ 40–65 h is
engineering (Hans face, a Hangul craft-fonts face, RTL layout, bidi text); native-speaker review
needs humans.

No language is `full` except English: every catalog leaves engine error details in English, and
none has translated help.

## Other shipped languages

| Language | Code | UI strings translated | Status | Acrobat ships it |
|---|---|---|---|---|
| Traditional Chinese | `zh-hant` | 2,016 (94.1%) | partial (same Hans/Hant face caveat) | yes |
| Russian | `ru` | 2,043 (95.3%) | partial | yes |
| Bulgarian | `bg` | 2,094 (97.7%) | partial | no |
| Czech | `cs` | 135 (6.3%) | menus only | yes |
| Telugu | `te` | 2,058 (96.0%) | partial (the release-pinned craft-fonts has no Telugu face yet) | no |
| Hungarian | `hu` | 2,126 (99.2%) | partial | yes |
| Ukrainian | `uk` | 2,142 (100%) | partial | yes |
| Italian | `it` | 2,130 (99.4%) | partial | yes |
| Kazakh | `kk` | 2,327 entries | partial | no |
| Polish | `pl` | 2,287 entries | partial | yes |
| Slovenian | `sl` | 2,324 entries | partial | no |
| Greek | `el` | 2,341 entries | partial | no |
| Turkish | `tr` | 2,313 entries | partial | yes |
| Lithuanian | `lt` | 2,578 entries | partial; community native-speaker review ([#1401](https://github.com/storytold/pdfcraft/pull/1401)) | no |

**14 other languages** ship (23 catalogs plus English, 24 interface languages in Preferences).
The entry counts without percentages were measured on 2026-10-12; they are not percentages of the older 2,143-key snapshot above. Their presence does not imply a detailed linguistic or runtime review.
Acrobat languages PdfCraft lacks: Danish, Dutch, Finnish, Norwegian, Swedish, Hebrew.

## Revision history

| Date | Change | Summary |
|---|---|---|
| 2026-10-13 | minor | Hindi and Korean ship as full 2,578-key catalogs with month and weekday names; Korean draws with an installed Hangul face (no craft-fonts face yet); the dimension is now estimated at ≈ 93% and all twelve key languages have a catalog |
| 2026-10-13 | minor | Indonesian and Vietnamese ship as full 2,578-key catalogs with month and weekday names; the key union is 2,578 and the dimension re-measures at ≈ 76% (Hindi and Korean are the two languages still missing) |
| 2026-10-12 | minor | Added current counts for Kazakh, Polish, Slovenian, Greek, Turkish and Lithuanian; retained existing language assessments and the original percentage snapshot |
| 2026-10-10 | major | Created. Catalog counts measured from `crates/ui-egui/src/i18n/*.tsv` against the 2,143-key union; Acrobat's localizations from its bundle's `.lproj` and Sequences folder names |
