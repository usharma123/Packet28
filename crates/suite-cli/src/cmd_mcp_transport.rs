use super::*;

pub(crate) fn render_command_preview(command: &str, args: &[String]) -> String {
    std::iter::once(command.to_string())
        .chain(args.iter().map(|arg| {
            if arg.contains(' ') {
                format!("{arg:?}")
            } else {
                arg.clone()
            }
        }))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpMessageFraming {
    ContentLength,
    NewlineJson,
}

pub(crate) fn read_message(reader: &mut impl BufRead) -> Result<Option<(Value, McpMessageFraming)>> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            let value = serde_json::from_str(trimmed)?;
            return Ok(Some((value, McpMessageFraming::NewlineJson)));
        }
        return read_header_framed_message(reader, trimmed);
    }
}

fn read_header_framed_message(
    reader: &mut impl BufRead,
    first_line: &str,
) -> Result<Option<(Value, McpMessageFraming)>> {
    let mut content_length = None::<usize>;
    parse_header_line(first_line, &mut content_length)?;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            parse_header(name, value, &mut content_length)?;
        }
    }

    let content_length =
        content_length.ok_or_else(|| anyhow!("missing Content-Length header in MCP request"))?;
    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(Some((
        serde_json::from_slice(&body)?,
        McpMessageFraming::ContentLength,
    )))
}

fn parse_header_line(line: &str, content_length: &mut Option<usize>) -> Result<()> {
    let Some((name, value)) = line.split_once(':') else {
        return Err(anyhow!("missing Content-Length header in MCP request"));
    };
    parse_header(name, value, content_length)
}

fn parse_header(name: &str, value: &str, content_length: &mut Option<usize>) -> Result<()> {
    if name.eq_ignore_ascii_case("content-length") {
        *content_length = Some(value.trim().parse::<usize>()?);
    }
    Ok(())
}

pub(crate) fn write_message(
    writer: &mut impl Write,
    value: &Value,
    framing: McpMessageFraming,
) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    match framing {
        McpMessageFraming::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
            writer.write_all(&body)?;
        }
        McpMessageFraming::NewlineJson => {
            writer.write_all(&body)?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()?;
    Ok(())
}
