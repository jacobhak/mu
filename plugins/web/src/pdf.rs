//! PDF to Markdown with pdf-inspector (omp's `pi-natives/src/pdf.rs`).

use pdf_inspector::{MarkdownOptions, PdfOptions, process_pdf_mem_with_options};

const NO_TEXT_LAYER: &str = "This PDF has no text layer (scanned pages); OCR is not supported.";

pub fn to_markdown(bytes: &[u8]) -> Result<String, String> {
    let options = PdfOptions::new().markdown(MarkdownOptions { include_page_numbers: true, ..Default::default() });
    let converted = process_pdf_mem_with_options(bytes, options).map_err(|e| format!("PDF conversion failed: {e}"))?;
    match converted.markdown {
        Some(md) => Ok(md),
        None if !converted.pages_needing_ocr.is_empty() => Ok(NO_TEXT_LAYER.to_string()),
        None => Err("PDF conversion failed: converter returned no Markdown".into()),
    }
}

/// A minimal PDF with one text page per entry, for tests.
#[cfg(test)]
pub(crate) fn fixture(page_contents: &[&str], title: Option<&str>) -> Vec<u8> {
    let font_id = 3 + page_contents.len() * 2;
    let info_id = title.map(|_| font_id + 1);
    let mut objects: Vec<String> = vec!["<< /Type /Catalog /Pages 2 0 R >>".into()];
    let kids = (0..page_contents.len()).map(|i| format!("{} 0 R", 3 + i * 2)).collect::<Vec<_>>().join(" ");
    objects.push(format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", page_contents.len()));
    for (i, content) in page_contents.iter().enumerate() {
        let content_id = 4 + i * 2;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 {font_id} 0 R >> >> \
             /Contents {content_id} 0 R >>"
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()));
    }
    objects.push("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".into());
    if let Some(title) = title {
        objects.push(format!("<< /Title ({title}) >>"));
    }

    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    let info = info_id.map_or_else(String::new, |id| format!(" /Info {id} 0 R"));
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R{info} >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
    );
    pdf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_text_with_page_markers() {
        let pdf = fixture(
            &[
                "BT /F1 12 Tf 72 720 Td (First page text) Tj 0 -18 Td (More first page text) Tj ET",
                "BT /F1 12 Tf 72 720 Td (Second page text) Tj 0 -18 Td (More second page text) Tj ET",
            ],
            Some("Fixture Title"),
        );
        let md = to_markdown(&pdf).expect("fixture PDF should convert");
        assert!(md.contains("First page text"), "{md}");
        assert!(md.contains("Second page text"), "{md}");
        assert!(md.contains("<!-- Page 1 -->"), "{md}");
        assert!(md.contains("<!-- Page 2 -->"), "{md}");
    }

    #[test]
    fn malformed_input_is_prefixed() {
        let err = to_markdown(b"not a PDF").expect_err("malformed input should fail");
        assert!(err.starts_with("PDF conversion failed:"), "{err}");
    }
}
