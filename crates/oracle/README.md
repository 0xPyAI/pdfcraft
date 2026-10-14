# pdfcraft-oracle

The external programs PdfCraft checks its own output against, and one rule: **a missing oracle is
either a declared skip or a failure, never a silent pass.**

- **Layer:** test support (`Class::Testkit` in `xtask/src/layers.rs`). A dev-dependency only, never
  compiled into the product, and so usable even from the standalone crates (`cos`, `geom`,
  `filters`, `crypt`, `arlington`) whose own tests need it.
- No panicking path of its own. The `or_skip!` macro expands its `assert!` into the caller, which
  is test code.

## Why it exists

Every test that wanted an oracle used to probe for it by hand and decide on its own what to do when
it was missing. The common choice — print a line and return — reports as a **pass**, so a suite
with no oracles installed looked exactly like one in which every oracle agreed.

That was not hypothetical. No CI runner installed `qpdf`, so `cos/tests/encryption.rs::qpdf_agrees`
had never executed on a runner, while `README.md` said saved output was "independently checked with
qpdf". For R2–R4 that test is the *only* check of owner-password authentication (Algorithm 7) that
does not go through our own code.

## API

```rust
// In a test: bind the oracle, or return after saying why it was skipped.
let qpdf = pdfcraft_oracle::or_skip!(pdfcraft_oracle::qpdf());
let out = qpdf.command().arg("--check").arg(&path).output()?;

// Elsewhere (xtask), handle the absence yourself.
match pdfcraft_oracle::pdftotext() {
    Ok(tool) => { /* tool.command() */ }
    Err(absent) => bail!("{absent}"),
}
```

| Probe | Tool | Used for |
|---|---|---|
| `qpdf()` | qpdf | `--check` structural validation; the independent owner-password oracle |
| `pdftoppm()` | Poppler | page pixels, for comparing against our own raster output |
| `pdftotext()` | Poppler | text extraction (`cargo xtask text-oracle` scores word F1 against it) |

Each returns `Result<Oracle, Absent>`. `Oracle::command()` hands back a fresh `Command`;
`Absent` renders as an actionable sentence naming the install command.

## The gate

`PDFCRAFT_ORACLES_REQUIRED` (any value, including `0` — the same convention as
`CRAFT_FONTS_REQUIRED`) turns absence from a skip into a failure. CI sets it on the runners that
install the tools, so a skip notice there becomes a red build.

```
$ cargo test -p pdfcraft-cos --test encryption qpdf_agrees
test result: ok. 1 passed            # qpdf absent: skipped, with a notice on stderr

$ PDFCRAFT_ORACLES_REQUIRED=1 cargo test -p pdfcraft-cos --test encryption qpdf_agrees
oracle `qpdf` is required but is not on PATH (program not found): PDFCRAFT_ORACLES_REQUIRED is
set, so this is a failure rather than a skip (apt install qpdf / brew install qpdf)
test result: FAILED. 1 failed
```

`or_skip!` expands to a `return`, so it belongs in a test returning `()`; a test returning `Result`
should match on the probe directly.

## Still to do

- CI installs only `qpdf`, the one tool the test job uses. The Poppler pixel comparison in
  `crates/organize` stays `#[ignore]`d until a change turns it on and verifies it there.
- Oracles that are not external processes (hayro, `lopdf`) are ordinary dev-dependencies and are
  not routed through this crate.
