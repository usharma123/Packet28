use covy_core::model::{CoverageData, CoverageFormat, FileCoverage};
use covy_core::CovyError;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::Ingestor;

pub struct CoberturaIngestor;

impl Ingestor for CoberturaIngestor {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::Cobertura
    }

    fn parse(&self, data: &[u8]) -> Result<CoverageData, CovyError> {
        parse_cobertura(data)
    }
}

fn parse_cobertura(data: &[u8]) -> Result<CoverageData, CovyError> {
    if data.is_empty() {
        return Err(CovyError::EmptyInput {
            path: "(cobertura input)".into(),
        });
    }
    // Check it's actually XML
    let prefix = std::str::from_utf8(&data[..data.len().min(512)]).unwrap_or("");
    let trimmed = prefix.trim();
    if !trimmed.is_empty() && !trimmed.starts_with('<') && !trimmed.starts_with("<?") {
        return Err(CovyError::Parse {
            format: "cobertura".into(),
            detail: "Input is not XML — did you mean --format lcov or --format gocov?".into(),
        });
    }

    let mut result = CoverageData::new();
    result.format = Some(CoverageFormat::Cobertura);

    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(true);

    let mut sources: Vec<String> = Vec::new();
    let mut in_sources = false;
    let mut current_filename: Option<String> = None;
    let mut current_coverage = FileCoverage::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"sources" => {
                        in_sources = true;
                    }
                    b"source" => {
                        // Will read text in next event
                    }
                    b"class" => {
                        // Get filename attribute
                        if let Some(filename) = get_attr(e, b"filename") {
                            // Flush previous class if same file needs merge
                            if let Some(prev) = current_filename.take() {
                                let resolved = resolve_path(&prev, &sources);
                                result
                                    .files
                                    .entry(resolved)
                                    .or_default()
                                    .merge(&current_coverage);
                                current_coverage = FileCoverage::new();
                            }
                            current_filename = Some(filename);
                        }
                    }
                    b"line" => {
                        if current_filename.is_some() {
                            if let (Some(number), Some(hits)) =
                                (get_attr(e, b"number"), get_attr(e, b"hits"))
                            {
                                if let (Ok(line_no), Ok(hit_count)) =
                                    (number.parse::<u32>(), hits.parse::<u64>())
                                {
                                    current_coverage.lines_instrumented.insert(line_no);
                                    if hit_count > 0 {
                                        current_coverage.lines_covered.insert(line_no);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.name().as_ref() {
                    b"sources" => {
                        in_sources = false;
                    }
                    b"class" | b"package" => {
                        // Flush on class end
                        if let Some(filename) = current_filename.take() {
                            let resolved = resolve_path(&filename, &sources);
                            result
                                .files
                                .entry(resolved)
                                .or_default()
                                .merge(&current_coverage);
                            current_coverage = FileCoverage::new();
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if in_sources {
                    if let Ok(text) = e.unescape() {
                        let s = text.trim().to_string();
                        if !s.is_empty() {
                            sources.push(s);
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(CovyError::Xml(format!(
                    "Cobertura XML error at position {}: {e}",
                    reader.error_position()
                )))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(result)
}

fn get_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.as_ref() == name {
            return String::from_utf8(attr.value.to_vec()).ok();
        }
    }
    None
}

fn resolve_path(filename: &str, sources: &[String]) -> String {
    // If filename is already absolute or sources is empty, use as-is
    if filename.starts_with('/') || sources.is_empty() {
        return normalize_slashes(filename);
    }

    // Try prepending each source and use the filename directly
    // In most Cobertura reports, the filename is already relative
    normalize_slashes(filename)
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cobertura_basic() {
        let xml = r#"<?xml version="1.0" ?>
<coverage version="5.5" timestamp="1234" lines-valid="10" lines-covered="7" line-rate="0.7" branches-covered="0" branches-valid="0" branch-rate="0" complexity="0">
    <sources>
        <source>/app/src</source>
    </sources>
    <packages>
        <package name="." line-rate="0.7" branch-rate="0" complexity="0">
            <classes>
                <class name="main.py" filename="main.py" line-rate="0.7" branch-rate="0" complexity="0">
                    <lines>
                        <line number="1" hits="1"/>
                        <line number="2" hits="1"/>
                        <line number="3" hits="0"/>
                    </lines>
                </class>
            </classes>
        </package>
    </packages>
</coverage>"#;

        let result = parse_cobertura(xml.as_bytes()).unwrap();
        assert_eq!(result.files.len(), 1);
        let fc = &result.files["main.py"];
        assert_eq!(fc.lines_instrumented.len(), 3);
        assert_eq!(fc.lines_covered.len(), 2);
    }

    #[test]
    fn test_parse_cobertura_multiple_classes() {
        let xml = r#"<?xml version="1.0" ?>
<coverage version="5.5" timestamp="1234">
    <packages>
        <package name="pkg">
            <classes>
                <class name="a" filename="a.py" line-rate="1.0">
                    <lines>
                        <line number="1" hits="1"/>
                        <line number="2" hits="1"/>
                    </lines>
                </class>
                <class name="b" filename="b.py" line-rate="0.5">
                    <lines>
                        <line number="1" hits="1"/>
                        <line number="2" hits="0"/>
                    </lines>
                </class>
            </classes>
        </package>
    </packages>
</coverage>"#;

        let result = parse_cobertura(xml.as_bytes()).unwrap();
        assert_eq!(result.files.len(), 2);
    }
}
