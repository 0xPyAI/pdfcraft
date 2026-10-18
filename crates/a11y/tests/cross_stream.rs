use pdfcraft_a11y::{Options, Rule, Status, autotag_text, check, figures, mark_decorative, mark_tag_artifact, tag_content, tags};
use pdfcraft_content::{Op, Pieces};
use pdfcraft_cos::{Document, ObjRef, Object};

const FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";
const HIDDEN_AND_AFTER: &str = "BT /F1 12 Tf 72 520 Td (Hidden) Tj ET\nBT /F1 12 Tf 72 500 Td (After) Tj ET";

fn stream(data: &str) -> String {
    format!("<< /Length {} >>\nstream\n{data}\nendstream", data.len())
}

fn build_pdf(objects: &[(u32, String)]) -> Document {
    let mut raw = Vec::from(&b"%PDF-1.7\n"[..]);
    let max_num = objects.iter().map(|(n, _)| *n).max().unwrap_or(0);
    let mut offsets = std::collections::HashMap::new();
    for (num, body) in objects {
        offsets.insert(*num, raw.len());
        raw.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
    }
    let xref = raw.len();
    let size = max_num + 1;
    raw.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for num in 1..size {
        if let Some(off) = offsets.get(&num) {
            raw.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        } else {
            raw.extend_from_slice(b"0000000000 65535 f \n");
        }
    }
    raw.extend_from_slice(format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
    Document::open(std::sync::Arc::new(raw)).expect("valid synthetic PDF")
}

fn untagged_two_streams(first: &str, second: &str) -> Document {
    build_pdf(&[
        (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
        (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
        (3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents [5 0 R 6 0 R] /Resources << /Font << /F1 4 0 R >> >> >>".into()),
        (4, FONT.into()),
        (5, stream(first)),
        (6, stream(second)),
    ])
}

fn tagged_figure_two_streams(after_figure: &str, second: &str) -> Document {
    let figure = "/Figure << /MCID 0 >> BDC q 50 0 0 50 72 600 cm /Im1 Do Q EMC\n";
    let first = format!("{figure}{after_figure}");
    build_pdf(&[
        (
            1,
            "<< /Type /Catalog /Pages 2 0 R /StructTreeRoot 6 0 R /MarkInfo << /Marked true >> >>"
                .into(),
        ),
        (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into()),
        (
            3,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /StructParents 0 /Contents [10 0 R 11 0 R] /Resources << /Font << /F1 4 0 R >> /XObject << /Im1 5 0 R >> >> >>".into(),
        ),
        (4, FONT.into()),
        (
            5,
            "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Length 4 >>\nstream\n\x00\x7f\x40\x20\nendstream".into(),
        ),
        (6, "<< /Type /StructTreeRoot /K 7 0 R /ParentTree 9 0 R >>".into()),
        (7, "<< /Type /StructElem /S /Document /P 6 0 R /K [8 0 R] >>".into()),
        (
            8,
            "<< /Type /StructElem /S /Figure /P 7 0 R /Pg 3 0 R /K 0 /Alt (A grey square) >>"
                .into(),
        ),
        (9, "<< /Nums [0 [8 0 R]] >>".into()),
        (10, stream(&first)),
        (11, stream(second)),
    ])
}

fn joined_ops(doc: &Document) -> (Vec<Op>, usize) {
    let page = &pdfcraft_model::pages(doc)[0];
    let contents = page.dict.get(b"Contents").expect("page contents");
    let pieces: Vec<Vec<u8>> = match &*doc.resolve(contents) {
        Object::Stream(s) => vec![s.decoded().expect("decoded stream")],
        Object::Array(items) => items
            .iter()
            .filter_map(|item| match &*doc.resolve(item) {
                Object::Stream(s) => Some(s.decoded().expect("decoded stream")),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let joined = Pieces::join(&pieces);
    let parsed = joined.parse();
    (parsed.ops, parsed.skipped)
}

fn shown_strings(ops: &[Op]) -> Vec<String> {
    ops.iter().filter(|op| op.is("Tj")).filter_map(|op| op.operands.first()?.as_string().map(|s| s.to_text())).collect()
}

#[test]
fn autotag_keeps_text_commented_out_across_stream_boundary_hidden() {
    let mut doc = untagged_two_streams("BT /F1 12 Tf 72 540 Td (Visible) Tj ET\n% a note at the end of the first stream", HIDDEN_AND_AFTER);
    let result = autotag_text(&mut doc).expect("autotag succeeds");
    assert_eq!(result.pages, 1);
    assert_eq!(result.paragraphs, 2, "commented-out text at start of second stream must not be tagged");

    let (ops, skipped) = joined_ops(&doc);
    assert_eq!(skipped, 0);
    assert_eq!(shown_strings(&ops), ["Visible", "After"]);

    let p_tags: Vec<_> = tags(&doc).into_iter().filter(|t| t.kind == "P").collect();
    assert_eq!(p_tags.len(), 2);
    let texts: Vec<_> = p_tags.iter().flat_map(|t| tag_content(&doc, t.object).expect("tag content")).map(|item| item.text).collect();
    assert_eq!(texts, ["Visible", "After"]);

    // Control: when the comment ends with a newline inside the first stream, Hidden is visible.
    let mut control = untagged_two_streams("BT /F1 12 Tf 72 540 Td (Visible) Tj ET\n% a note at the end of the first stream\n", HIDDEN_AND_AFTER);
    let control_res = autotag_text(&mut control).expect("control autotag succeeds");
    assert_eq!(control_res.paragraphs, 3);
    let (control_ops, _) = joined_ops(&control);
    assert_eq!(shown_strings(&control_ops), ["Visible", "Hidden", "After"]);
}

#[test]
fn mark_decorative_preserves_shape_split_across_content_streams() {
    let mut doc = tagged_figure_two_streams("0 0 1 rg 300 600 100 50", "re f\nBT /F1 12 Tf 72 500 Td (After) Tj ET");
    mark_decorative(&mut doc, ObjRef::new(8, 0)).expect("mark decorative succeeds");
    assert!(figures(&doc).is_empty());

    let (ops, skipped) = joined_ops(&doc);
    assert_eq!(skipped, 0, "split rectangle operands must not be dropped as skipped tokens");
    let re_op = ops.iter().find(|op| op.is("re")).expect("rectangle operator must survive");
    assert_eq!(re_op.nums::<4>(), Some([300.0, 600.0, 100.0, 50.0]));
    assert_eq!(shown_strings(&ops), ["After"]);

    // Control: rectangle whole in the first stream.
    let mut control = tagged_figure_two_streams("0 0 1 rg 300 600 100 50 re f", "BT /F1 12 Tf 72 500 Td (After) Tj ET");
    mark_decorative(&mut control, ObjRef::new(8, 0)).expect("control mark decorative succeeds");
    let (control_ops, control_skipped) = joined_ops(&control);
    assert_eq!(control_skipped, 0);
    let control_re = control_ops.iter().find(|op| op.is("re")).expect("control rectangle");
    assert_eq!(control_re.nums::<4>(), Some([300.0, 600.0, 100.0, 50.0]));
    assert_eq!(shown_strings(&control_ops), ["After"]);
}

#[test]
fn mark_decorative_and_mark_artifact_keep_cross_stream_comment_hidden() {
    let mut doc = tagged_figure_two_streams("% a note at the end of the first stream", HIDDEN_AND_AFTER);
    mark_decorative(&mut doc, ObjRef::new(8, 0)).expect("mark decorative succeeds");
    let (ops, skipped) = joined_ops(&doc);
    assert_eq!(skipped, 0);
    assert_eq!(shown_strings(&ops), ["After"], "commented-out Hidden text must not become visible after mark_decorative");

    let mut doc_artifact = tagged_figure_two_streams("% a note at the end of the first stream", HIDDEN_AND_AFTER);
    mark_tag_artifact(&mut doc_artifact, ObjRef::new(8, 0)).expect("mark_tag_artifact succeeds");
    let (artifact_ops, artifact_skipped) = joined_ops(&doc_artifact);
    assert_eq!(artifact_skipped, 0);
    assert_eq!(shown_strings(&artifact_ops), ["After"], "commented-out Hidden text must not become visible after mark_tag_artifact");

    // Checker also honours the cross-stream comment when scanning for untagged content.
    let mut comment_only = tagged_figure_two_streams("% a note at the end of the first stream", "BT /F1 12 Tf 72 520 Td (Hidden) Tj ET\n");
    let report = check(&comment_only, &Options { rules: [Rule::TaggedContent].into(), pages: None });
    assert_eq!(report.result(Rule::TaggedContent).expect("rule result").status, Status::Passed);
    mark_decorative(&mut comment_only, ObjRef::new(8, 0)).expect("mark decorative");
    let report_after = check(&comment_only, &Options { rules: [Rule::TaggedContent].into(), pages: None });
    assert_eq!(report_after.result(Rule::TaggedContent).expect("rule result").status, Status::Passed);
}
