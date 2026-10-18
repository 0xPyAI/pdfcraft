# AcroForm `/MaxLen 0` compatibility

Status: implemented; proposed as a separate compatibility PR. Last updated: 2026-10-19.

- `crates/forms/src/lib.rs::walk` reads inherited `/MaxLen`; the UI and value validation both
  consume `Field::max_len`.

The parser exposed `/MaxLen 0` as `Some(0)`, so both editor and API blocked input. It now
exposes only positive limits; zero clears an inherited limit. The original object entry is
preserved through save/reopen. This is an interoperability interpretation for zero, **not
a claim that ISO explicitly defines zero as unlimited**. ISO 32000-1 Table 229 defines an
optional inheritable maximum length without defining a special zero meaning:
[public specification](https://opensource.adobe.com/dc-acrobat-sdk-docs/pdfstandards/PDF32000_2008.pdf).
Positive limits remain enforced; no field names or character-sheet rules enter product code.

Regression: `crates/forms/src/tests.rs::zero_max_len_is_unlimited_and_overrides_inherited_limit`.
The forms README and `form.fill-text` parity entry document this behavior.

Historical validation recorded before this review:

- Forms unit suite: all 68 tests passed. The new zero-limit regression was first run against
  the old parser and failed (`Some(0)` instead of `None`), then passed with the compatibility
  interpretation. Its positive-limit control and preserved raw entry also passed.

The user reports successful Class text input on the integrated release using their Pathfinder
character sheet. This is a manual functional result, without timings; the private document
was not used for automated regression tests in this review.

## Independent branch validation

Validated on upstream `a12fe8f4116984ebbc870eb514346ea922233316` with an isolated
Cargo target directory and no performance changes:

- The focused regression failed with the original parser (`Some(0)` rather than `None`),
  then passed after restoring the compatibility fix.
- `cargo test -p pdfcraft-forms -j 2`: 81 unit tests passed; doc tests passed.
- `cargo clippy -p pdfcraft-forms -j 2 --all-targets -- -D warnings`: passed.
- `cargo check --target wasm32-unknown-unknown -p pdfcraft-forms -j 2`: passed.
- `cargo fmt --check`, `git diff --check`, and `cargo xtask layers`, `assets`, `parity`: passed.

These are local checks, not hosted CI. No private PDF was used.

## Rebase validation

Rebased onto freshly fetched upstream main
`932bf9c6bfdd63e2a21bb831696a3447b9def9cf` on 2026-10-18. The only conflict was
in the M6 roadmap row; upstream CSV/XML updates were retained alongside the compatibility
note. The implementation and original regression are unchanged from the independent branch.

Validation used a fresh Cargo target directory, two build jobs, and locked, offline builds:

- The original regression failed with the upstream parser (`Some(0)` instead of `None`),
  then passed with the compatibility fix restored.
- Forms: 83 unit tests passed. Engine: 282 unit and 9 integration tests passed.
  JavaScript: 47 unit tests passed. Doc tests passed. The timing probe and two private-file
  tests remained ignored.
- The automation `javascript_through_tools` regression passed.
- Scoped Clippy for forms, engine and JavaScript, with `--all-targets -- -D warnings`, passed.
- WASM checks for those three crates passed.
- Rust formatting, whitespace, layering, assets and parity checks passed.
- A freshly built CLI filled synthetic fields with direct and inherited `/MaxLen 0`, rendered
  their values, saved/reopened them and accepted longer text after reopening. Its positive-limit
  control rejected six characters for `/MaxLen 5`; the synthetic input file stayed unchanged.

No performance optimizations, JavaScript snapshot/diff changes or profiling instrumentation
are included. Existing executables, backups and private PDFs were not modified.

## PR #3323 refresh

Revalidated against freshly fetched upstream main
`4db6ac4ae2448df6389d8bb0bc54db6ba175721c` on 2026-10-18. Conflicts in both
roadmaps were resolved by keeping upstream's button-label and content-editing notes alongside
the `/MaxLen 0` documentation. The original compatibility implementation and regression are
preserved; no performance changes were added to the PR.

Forms: 85 unit tests passed. Engine: 283 unit and 15 integration tests passed. JavaScript:
48 unit tests passed. The automation JavaScript regression, scoped Clippy with warnings denied,
WASM, formatting, whitespace, layering, assets and parity checks all passed. The synthetic CLI
fill/render/save/reopen and positive-limit control passed. The original regression again failed
with the old parser and passed with the fix restored. Private-file tests remained ignored.

## 2026-10-19 revalidation

Rebased onto freshly fetched upstream main
`459379ef9b127648cc30c0b2fe2b20c2b2375f04`. Only `ROADMAP.md` required manual
conflict resolution; all upstream entries and the compatibility note were retained.
The implementation, original regression and forms README remain byte-for-byte identical
to the previously reviewed compatibility head. No performance changes were introduced.

Validation used a fresh isolated build directory, two jobs, and locked offline builds:

- The original regression failed with the old parser and passed with the fix restored.
- Forms: 93 unit tests passed. Engine: 283 unit and 15 integration tests passed.
  JavaScript: 48 tests passed. Doc tests passed; the timing probe and two private-file
  tests remained ignored.
- Automation JavaScript, scoped all-targets Clippy with warnings denied, WASM, formatting,
  whitespace, layering, assets and parity passed.
- Synthetic CLI fill/render/save/reopen checks passed for direct zero and zero overriding
  an inherited positive limit. Longer text after reopening was accepted. The `/MaxLen 5`
  control rejected six characters, and the synthetic input stayed unchanged.

These are local checks, not hosted CI. Existing executables, backups and private PDFs
were not modified; only the compatibility worktree was changed.

## Revision history

- 2026-10-19: refresh PR #3323 onto upstream `459379ef`, retain all upstream roadmap entries
  and independently repeat regression, native/WASM, repository and CLI validation.

- 2026-10-18: refresh PR #3323 onto upstream `4db6ac4a`, retaining both roadmap updates and
  independently revalidating compatibility and repository checks.

- 2026-10-18: rebase onto fetched upstream `932bf9c6`; preserve upstream roadmap updates and
  independently revalidate the regression, forms/engine/JavaScript suites and quality gates.

- 2026-10-18: isolate the compatibility change on current upstream main; verify the
  regression before and after the fix, the forms suite and scoped quality gates.
