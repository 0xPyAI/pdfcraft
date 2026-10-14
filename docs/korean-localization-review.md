# Korean UI terminology review — draft follow-up to #1976

This is a code-grounded review draft, NOT evidence of completed native-speaker or runtime QA. Only
Korean UI labels change; English lookup keys, stable command IDs, PDF contents and automation
protocols remain unchanged.

## Redaction has three separate operations

The initial Korean catalog used the word "검열" (censorship) for all redaction actions.
The project source has three distinct stages:

| Stage | Relevant code | Korean meaning |
| --- | --- | --- |
| Mark | crates/ui-egui/src/redact_ui.rs (RedactPrefs::mark), crates/engine/src/commands.rs (redact.mark/pages/search) | Mark a future removal area; content is not yet permanently removed |
| Apply | crates/ui-egui/src/commands.rs (redact.apply), crates/ui-egui/src/redact_ui.rs (confirmation), crates/redact/src/lib.rs (apply) | Confirm and remove covered content permanently |
| Clear | crates/ui-egui/src/commands.rs (redact.clear -> Edit::ClearRedactions) | Clear pending marks WITHOUT deleting the underlying content |

Terminology chosen from these behaviors:

- Tool (Redact): 민감 정보 제거
- Mark for redaction: 제거할 영역 표시
- Apply redactions: 표시된 정보 영구 제거
- Clear redaction marks: 제거 대상 표시 지우기
- Redaction code: 제거 사유 코드 (reason/exemption code sets: crates/redact/src/codes.rs)

"Clear all" is a generic English lookup key used in other contexts and intentionally
is NOT changed to "clear redaction marks" globally.

## Additional terminology improvements

| English | Previous | Proposed | Source |
| --- | --- | --- | --- |
| Sanitize document… | 문서 정리… | 문서에서 숨겨진 정보 제거… | crates/redact/src/sanitize.rs |
| Flatten form fields | 양식 필드 병합 | 양식 필드 평탄화 | crates/ui-egui/src/optimize_ui.rs |
| Flatten Fill & Sign when saving | 저장할 때 채우기 및 서명 병합 | 저장 시 입력 내용과 서명을 페이지에 고정 | crates/ui-egui/src/editing.rs |
| Add printer marks | 프린터 표시 추가 | 인쇄용 표시 추가 | printer marks are printed on pages, not on devices |

This review does not change acceptable variations such as 확인란/체크박스 or
Bates/베이츠 simply for stylistic reasons. Certificate and digital-signature
language requires separate code-context review.

## Verification and remaining gates

- Tested: catalog key coverage, TSV structural rules, placeholder preservation,
  ellipsis preservation, old-vs-new textual regression assertions.
- Added a Rust regression test for the distinct redaction stage labels. This test
  had failing baseline strings before the terminology changes.
- NOT YET EXECUTED: cargo fmt, Rust unit tests, real Windows UI, font glyph coverage,
  PDF save/reopen, OCR, permanent-redaction behavior on a synthetic PDF.
  These gates are necessary before calling the translation production-ready.

## UI acceptance test matrix (synthetic data only)

1. With an English UI, open a sample PDF called 한글_검수.pdf containing Hangul bookmark
   and comment text. Verify that tab, bookmark, search and dialogs render Hangul rather
   than replacement boxes, regardless of interface language.
2. Select 한국어 under Preferences and check persistence; separately test Auto on ko-KR.
3. Draw a redaction mark on TEST-REDACT-UNIQUE. Before Apply, the UI must say
   "marked" rather than implying permanent removal. Clear must remove only marks.
4. On a separate synthetic document, Apply redactions, save and reopen; inspect
   extracted text and rendered output for the removed token (source-byte/fidelity
   checks belong in the engine suite as well).
5. Flatten an AcroForm field and reopen: the visible value should remain on the
   page without the interactive field being editable as before.
6. Test 100/125/150% Windows display scaling for Korean text overflow and truncation.
7. Test web build font delivery separately once Hangul font PR #1484 lands.

The translations were written from intended source-code behavior. Do not copy other
products' localization files, proprietary Adobe assets, fonts or real users' PDFs.
