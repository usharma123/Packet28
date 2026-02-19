use covy_core::model::{CoverageData, CoverageFormat, FileCoverage};
use covy_core::CovyError;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::Ingestor;

pub struct JaCoCoIngestor;

impl Ingestor for JaCoCoIngestor {
    fn format(&self) -> CoverageFormat {
        CoverageFormat::JaCoCo
    }

    fn parse(&self, data: &[u8]) -> Result<CoverageData, CovyError> {
        parse_jacoco(data)
    }
}

fn parse_jacoco(data: &[u8]) -> Result<CoverageData, CovyError> {
    if data.is_empty() {
        return Err(CovyError::EmptyInput {
            path: "(jacoco input)".into(),
        });
    }
    let prefix = std::str::from_utf8(&data[..data.len().min(512)]).unwrap_or("");
    let trimmed = prefix.trim();
    if !trimmed.is_empty() && !trimmed.starts_with('<') && !trimmed.starts_with("<?") {
        return Err(CovyError::Parse {
            format: "jacoco".into(),
            detail: "Input is not XML — did you mean --format lcov or --format gocov?".into(),
        });
    }

    let mut result = CoverageData::new();
    result.format = Some(CoverageFormat::JaCoCo);

    let mut reader = Reader::from_reader(data);
    reader.config_mut().trim_text(true);

    let mut current_package: Option<String> = None;
    let mut current_sourcefile: Option<String> = None;
    let mut current_coverage = FileCoverage::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.name();
                match name.as_ref() {
                    b"package" => {
                        current_package = get_attr(e, b"name");
                    }
                    b"sourcefile" => {
                        // Flush previous sourcefile
                        if let (Some(pkg), Some(sf)) = (&current_package, current_sourcefile.take())
                        {
                            let path = format!("{}/{}", pkg, sf);
                            result
                                .files
                                .entry(path)
                                .or_insert_with(FileCoverage::new)
                                .merge(&current_coverage);
                            current_coverage = FileCoverage::new();
                        }
                        current_sourcefile = get_attr(e, b"name");
                    }
                    b"line" => {
                        if current_sourcefile.is_some() {
                            if let (Some(nr), Some(ci)) =
                                (get_attr(e, b"nr"), get_attr(e, b"ci"))
                            {
                                if let (Ok(line_no), Ok(covered_instr)) =
                                    (nr.parse::<u32>(), ci.parse::<u64>())
                                {
                                    current_coverage.lines_instrumented.insert(line_no);
                                    if covered_instr > 0 {
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
                    b"sourcefile" => {
                        if let (Some(pkg), Some(sf)) = (&current_package, current_sourcefile.take())
                        {
                            let path = format!("{}/{}", pkg, sf);
                            result
                                .files
                                .entry(path)
                                .or_insert_with(FileCoverage::new)
                                .merge(&current_coverage);
                            current_coverage = FileCoverage::new();
                        }
                    }
                    b"package" => {
                        current_package = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(CovyError::Xml(format!(
                    "JaCoCo XML error at position {}: {e}",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_jacoco_basic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<!DOCTYPE report PUBLIC "-//JACOCO//DTD Report 1.1//EN" "report.dtd">
<report name="my-project">
    <package name="com/example">
        <sourcefile name="App.java">
            <line nr="3" mi="0" ci="3" mb="0" cb="0"/>
            <line nr="5" mi="2" ci="0" mb="0" cb="0"/>
            <line nr="7" mi="0" ci="1" mb="0" cb="0"/>
        </sourcefile>
    </package>
</report>"#;

        let result = parse_jacoco(xml.as_bytes()).unwrap();
        assert_eq!(result.files.len(), 1);
        let fc = &result.files["com/example/App.java"];
        assert_eq!(fc.lines_instrumented.len(), 3);
        assert_eq!(fc.lines_covered.len(), 2); // lines 3 and 7 have ci > 0
        assert!(fc.lines_covered.contains(3));
        assert!(!fc.lines_covered.contains(5));
        assert!(fc.lines_covered.contains(7));
    }

    #[test]
    fn test_parse_jacoco_multiple_packages() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<report name="test">
    <package name="com/foo">
        <sourcefile name="Foo.java">
            <line nr="1" mi="0" ci="1" mb="0" cb="0"/>
        </sourcefile>
    </package>
    <package name="com/bar">
        <sourcefile name="Bar.java">
            <line nr="1" mi="1" ci="0" mb="0" cb="0"/>
        </sourcefile>
    </package>
</report>"#;

        let result = parse_jacoco(xml.as_bytes()).unwrap();
        assert_eq!(result.files.len(), 2);
        assert!(result.files.contains_key("com/foo/Foo.java"));
        assert!(result.files.contains_key("com/bar/Bar.java"));
    }
}
