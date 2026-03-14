use super::*;

pub(crate) fn push_token_term(tokens: &mut Vec<String>, seen: &mut HashSet<String>, raw: &str) {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.len() >= 2 && seen.insert(normalized.clone()) {
        tokens.push(normalized);
    }
}

pub(crate) fn split_identifier_fragments(raw: &str) -> Vec<String> {
    let mut fragments = Vec::new();
    let chars = raw.chars().collect::<Vec<_>>();
    if chars.is_empty() {
        return fragments;
    }

    let mut current = String::new();
    for (idx, ch) in chars.iter().copied().enumerate() {
        let prev = idx.checked_sub(1).and_then(|prev| chars.get(prev)).copied();
        let next = chars.get(idx + 1).copied();
        let boundary = !current.is_empty()
            && prev.is_some_and(|prev| {
                (ch.is_ascii_uppercase() && prev.is_ascii_lowercase())
                    || (ch.is_ascii_digit() && !prev.is_ascii_digit())
                    || (!ch.is_ascii_digit() && prev.is_ascii_digit())
                    || (prev.is_ascii_uppercase()
                        && ch.is_ascii_uppercase()
                        && next.is_some_and(|next| next.is_ascii_lowercase()))
            });
        if boundary {
            fragments.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        fragments.push(current);
    }
    fragments
}

pub(crate) fn tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut seen = HashSet::new();
    for part in input.split(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '/') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        push_token_term(&mut tokens, &mut seen, part);
        for segment in part.split(['/', ':', '.', '_', '-']) {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            push_token_term(&mut tokens, &mut seen, segment);
            for fragment in split_identifier_fragments(segment) {
                push_token_term(&mut tokens, &mut seen, &fragment);
            }
        }
    }
    tokens
}

pub fn normalize_context_path(
    raw: &str,
    workspace_root: Option<&Path>,
) -> Option<NormalizedPathRef> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut normalized = trimmed.replace('\\', "/");
    if let Some(root) = workspace_root {
        let root = normalize_path_string(&root.to_string_lossy().replace('\\', "/"));
        let root = root.trim_matches('/').to_string();
        let absolute = normalize_path_string(&normalized);
        let absolute_trimmed = absolute.trim_matches('/').to_string();
        if !root.is_empty() && absolute_trimmed == root {
            normalized = ".".to_string();
        } else if !root.is_empty() && absolute_trimmed.starts_with(&(root.clone() + "/")) {
            normalized = absolute_trimmed[root.len() + 1..].to_string();
        } else {
            normalized = absolute;
        }
    } else {
        normalized = normalize_path_string(&normalized);
    }

    let canonical = normalized.trim_matches('/').to_ascii_lowercase();
    if canonical.is_empty() || canonical == "." {
        return None;
    }
    let basename = canonical
        .rsplit('/')
        .next()
        .map(str::trim)
        .filter(|basename| !basename.is_empty())
        .map(ToOwned::to_owned);
    Some(NormalizedPathRef {
        canonical,
        basename,
    })
}

pub fn basename_alias(raw: &str) -> Option<String> {
    normalize_context_path(raw, None).and_then(|path| path.basename)
}

pub(crate) fn normalize_path_string(raw: &str) -> String {
    let mut parts = Vec::<String>::new();
    let is_absolute = raw.starts_with('/');
    let raw_path = Path::new(raw);
    for component in raw_path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() && parts.last().is_some_and(|part| part != "..") {
                    parts.pop();
                } else if !is_absolute {
                    parts.push("..".to_string());
                }
            }
            Component::Normal(part) => parts.push(part.to_string_lossy().replace('\\', "/")),
            Component::RootDir => {}
            Component::Prefix(prefix) => {
                parts.push(prefix.as_os_str().to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let joined = parts.join("/");
    if is_absolute && !joined.is_empty() {
        format!("/{joined}")
    } else {
        joined
    }
}

pub(crate) fn extract_query_path_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|part| {
            part.trim_matches(|c: char| matches!(c, ',' | ';' | '"' | '\'' | '(' | ')' | '[' | ']'))
        })
        .filter(|part| looks_like_path(part))
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn push_normalized_path(
    paths: &mut Vec<String>,
    basenames: &mut Vec<String>,
    raw: &str,
    workspace_root: Option<&Path>,
) {
    let Some(path_ref) = normalize_context_path(raw, workspace_root) else {
        return;
    };
    push_unique_text(paths, &path_ref.canonical, usize::MAX);
    if let Some(basename) = path_ref.basename {
        push_unique_text(basenames, &basename, usize::MAX);
    }
}
