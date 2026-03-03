use std::path::Path;

use anyhow::{Context, Result};

pub fn write_strip_prefixes(config_path: &str, strip_prefixes: &[String]) -> Result<()> {
    let path = Path::new(config_path);
    let mut doc = if path.exists() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        raw.parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("Failed to parse {} as TOML", path.display()))?
    } else {
        toml_edit::DocumentMut::new()
    };

    if !doc.as_table().contains_key("paths") {
        doc["paths"] = toml_edit::table();
    }
    if !doc["paths"].is_table() {
        anyhow::bail!("[paths] must be a TOML table");
    }

    let mut array = toml_edit::Array::default();
    for prefix in strip_prefixes {
        array.push(prefix.as_str());
    }
    doc["paths"]["strip_prefix"] = toml_edit::value(array);

    std::fs::write(path, doc.to_string())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}
