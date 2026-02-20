use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingObservation {
    pub test_id: String,
    pub duration_ms: u64,
}

pub fn parse_junit_xml_file(path: &Path) -> Result<Vec<TimingObservation>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read JUnit XML file {}", path.display()))?;
    parse_junit_xml_content(&content)
}

pub fn parse_timing_jsonl_file(path: &Path) -> Result<Vec<TimingObservation>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read timing JSONL file {}", path.display()))?;
    parse_timing_jsonl_content(&content)
}

fn parse_junit_xml_content(content: &str) -> Result<Vec<TimingObservation>> {
    let mut reader = quick_xml::Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.name().as_ref() != b"testcase" {
                    continue;
                }

                let mut classname: Option<String> = None;
                let mut name: Option<String> = None;
                let mut time: Option<String> = None;
                for attr in e.attributes() {
                    let attr = attr.context("Invalid XML attribute in testcase element")?;
                    let value = String::from_utf8_lossy(attr.value.as_ref()).trim().to_string();
                    match attr.key.as_ref() {
                        b"classname" => classname = Some(value),
                        b"name" => name = Some(value),
                        b"time" => time = Some(value),
                        _ => {}
                    }
                }

                let test_id = match (classname.as_deref(), name.as_deref()) {
                    (Some(c), Some(n)) if !c.is_empty() && !n.is_empty() => format!("{c}.{n}"),
                    (_, Some(n)) if !n.is_empty() => n.to_string(),
                    (Some(c), _) if !c.is_empty() => c.to_string(),
                    _ => String::new(),
                };
                if test_id.is_empty() {
                    continue;
                }
                let duration_ms = time
                    .as_deref()
                    .and_then(parse_seconds_to_ms)
                    .unwrap_or_default();
                out.push(TimingObservation {
                    test_id,
                    duration_ms,
                });
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => anyhow::bail!("Failed to parse JUnit XML: {e}"),
        }
    }

    Ok(out)
}

fn parse_timing_jsonl_content(content: &str) -> Result<Vec<TimingObservation>> {
    #[derive(serde::Deserialize)]
    struct JsonlTimingRecord {
        test_id: String,
        duration_ms: u64,
    }

    let mut out = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: JsonlTimingRecord = serde_json::from_str(line)
            .with_context(|| format!("Invalid JSONL timing record at line {}", line_no + 1))?;
        if rec.test_id.trim().is_empty() {
            continue;
        }
        out.push(TimingObservation {
            test_id: rec.test_id.trim().to_string(),
            duration_ms: rec.duration_ms,
        });
    }
    Ok(out)
}

fn parse_seconds_to_ms(raw: &str) -> Option<u64> {
    let secs = raw.trim().parse::<f64>().ok()?;
    if secs.is_sign_negative() {
        return Some(0);
    }
    Some((secs * 1000.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_junit_xml_content() {
        let xml = r#"
            <testsuite name="suite">
                <testcase classname="com.foo.BarTest" name="testOne" time="0.123" />
                <testcase name="tests/test_mod.py::test_two" time="0.500" />
            </testsuite>
        "#;
        let obs = parse_junit_xml_content(xml).unwrap();
        assert_eq!(obs.len(), 2);
        assert_eq!(
            obs[0],
            TimingObservation {
                test_id: "com.foo.BarTest.testOne".to_string(),
                duration_ms: 123
            }
        );
        assert_eq!(
            obs[1],
            TimingObservation {
                test_id: "tests/test_mod.py::test_two".to_string(),
                duration_ms: 500
            }
        );
    }

    #[test]
    fn test_parse_timing_jsonl_content() {
        let content = r#"
{"test_id":"com.foo.BarTest","duration_ms":1200}
{"test_id":"tests/test_mod.py::test_one","duration_ms":900}
"#;
        let obs = parse_timing_jsonl_content(content).unwrap();
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].test_id, "com.foo.BarTest");
        assert_eq!(obs[0].duration_ms, 1200);
        assert_eq!(obs[1].test_id, "tests/test_mod.py::test_one");
        assert_eq!(obs[1].duration_ms, 900);
    }
}
