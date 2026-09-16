use std::{
    collections::HashMap,
    fmt, fs,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy)]
struct Fence {
    marker: u8,
    length: usize,
    quote_depth: usize,
    list_indent: Option<usize>,
}

#[derive(Default)]
struct BlockContext {
    fence: Option<Fence>,
    paragraph_open: bool,
    quote_depth: usize,
    list_indents: Vec<usize>,
}

struct VisibleMarkdownLine {
    content: String,
    reference_definition_allowed: bool,
}

#[derive(Clone, Debug)]
struct LinkDestination {
    value: String,
    line: usize,
}

#[derive(Debug)]
enum ObservedLink {
    Inline(LinkDestination),
    Reference { label: String },
}

/// A documentation-gate failure.
#[derive(Debug, PartialEq, Eq)]
pub enum DocsError {
    Read {
        path: PathBuf,
        message: String,
    },
    MissingLink {
        source: PathBuf,
        destination: PathBuf,
        line: usize,
    },
    EscapesRoot {
        source: PathBuf,
        destination: PathBuf,
        line: usize,
    },
}

impl DocsError {
    /// Returns the path most directly responsible for the failure.
    pub fn path(&self) -> &Path {
        match self {
            Self::Read { path, .. } => path,
            Self::MissingLink { destination, .. } | Self::EscapesRoot { destination, .. } => {
                destination
            }
        }
    }
}

impl fmt::Display for DocsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(formatter, "could not read {}: {message}", path.display())
            }
            Self::MissingLink {
                source,
                destination,
                line,
            } => write!(
                formatter,
                "{}:{line}: missing local link destination {}",
                source.display(),
                destination.display()
            ),
            Self::EscapesRoot {
                source,
                destination,
                line,
            } => write!(
                formatter,
                "{}:{line}: local link destination escapes repository root: {}",
                source.display(),
                destination.display()
            ),
        }
    }
}

impl std::error::Error for DocsError {}

/// Checks every repository Markdown file's local destinations.
pub fn check_local_links(root: &Path) -> Result<(), DocsError> {
    let canonical_root = canonical_root(root)?;
    for source in markdown_files(&canonical_root)? {
        let contents = read_text(&canonical_root, &source)?;
        let mut definitions = HashMap::new();
        let mut observed = Vec::new();
        for (line_index, line) in visible_markdown_lines(&contents).iter().enumerate() {
            let line_number = line_index + 1;
            if line.reference_definition_allowed
                && let Some((label, value)) = reference_definition(&line.content)
            {
                definitions.entry(label).or_insert(LinkDestination {
                    value,
                    line: line_number,
                });
                continue;
            }
            for destination in inline_destinations(&line.content) {
                observed.push(ObservedLink::Inline(LinkDestination {
                    value: destination,
                    line: line_number,
                }));
            }
            for label in reference_labels(&line.content) {
                observed.push(ObservedLink::Reference { label });
            }
        }
        for link in observed {
            let destination = match link {
                ObservedLink::Inline(destination) => destination,
                ObservedLink::Reference { label } => {
                    let Some(destination) = definitions.get(&label) else {
                        continue;
                    };
                    destination.clone()
                }
            };
            check_destination(&canonical_root, &source, &destination)?;
        }
    }
    Ok(())
}

fn check_destination(
    canonical_root: &Path,
    source: &Path,
    destination: &LinkDestination,
) -> Result<(), DocsError> {
    if has_uri_scheme(&destination.value) {
        return Ok(());
    }
    let path_without_anchor = destination.value.split('#').next().unwrap_or_default();
    if path_without_anchor.is_empty() || Path::new(path_without_anchor).is_absolute() {
        return Ok(());
    }
    let source_parent = source.parent().unwrap_or_else(|| Path::new(""));
    let destination_path = PathBuf::from(path_without_anchor);
    let Some(relative) = normalize_relative(&source_parent.join(&destination_path)) else {
        return Err(DocsError::EscapesRoot {
            source: source.to_owned(),
            destination: destination_path,
            line: destination.line,
        });
    };
    let resolved = canonical_root.join(relative);
    if !resolved.exists() {
        return Err(DocsError::MissingLink {
            source: source.to_owned(),
            destination: destination_path,
            line: destination.line,
        });
    }
    let canonical_destination = fs::canonicalize(&resolved).map_err(|error| DocsError::Read {
        path: resolved,
        message: error.to_string(),
    })?;
    if !canonical_destination.starts_with(canonical_root) {
        return Err(DocsError::EscapesRoot {
            source: source.to_owned(),
            destination: destination_path,
            line: destination.line,
        });
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf, DocsError> {
    fs::canonicalize(root).map_err(|error| DocsError::Read {
        path: root.to_owned(),
        message: error.to_string(),
    })
}

fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn has_uri_scheme(destination: &str) -> bool {
    let Some((scheme, _)) = destination.split_once(':') else {
        return false;
    };
    let mut characters = scheme.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
}

pub(crate) fn markdown_files(root: &Path) -> Result<Vec<PathBuf>, DocsError> {
    let mut files = Vec::new();
    collect_markdown_files(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_markdown_files(
    scan_root: &Path,
    directory: &Path,
    files: &mut Vec<PathBuf>,
) -> Result<(), DocsError> {
    let entries = fs::read_dir(directory).map_err(|error| DocsError::Read {
        path: directory.to_owned(),
        message: error.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| DocsError::Read {
            path: directory.to_owned(),
            message: error.to_string(),
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| DocsError::Read {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if file_type.is_dir() {
            if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".git" | ".worktrees" | "target")
            ) {
                continue;
            }
            collect_markdown_files(scan_root, &path, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|extension| extension.to_str()) == Some("md")
        {
            files.push(
                path.strip_prefix(scan_root)
                    .expect("walked path must remain below scan root")
                    .to_owned(),
            );
        }
    }
    Ok(())
}

fn read_text(root: &Path, relative: &Path) -> Result<String, DocsError> {
    fs::read_to_string(root.join(relative)).map_err(|error| DocsError::Read {
        path: relative.to_owned(),
        message: error.to_string(),
    })
}

fn commonmark_content(line: &str) -> Option<&str> {
    let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
    (indentation <= 3).then(|| &line[indentation..])
}

fn visible_markdown_lines(contents: &str) -> Vec<VisibleMarkdownLine> {
    let mut context = BlockContext::default();
    let block_visible = contents
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .map(|line| {
            let (is_code, reference_definition_allowed) = classify_block_line(line, &mut context);
            if is_code {
                (String::new(), false)
            } else {
                (
                    visible_container_content(line, &context).to_owned(),
                    reference_definition_allowed,
                )
            }
        })
        .collect::<Vec<_>>();
    let visible_contents = block_visible
        .iter()
        .map(|(content, _)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    mask_inline_code_spans(&visible_contents)
        .split('\n')
        .zip(block_visible)
        .map(
            |(content, (_, reference_definition_allowed))| VisibleMarkdownLine {
                content: content.to_owned(),
                reference_definition_allowed,
            },
        )
        .collect()
}

fn visible_container_content<'a>(line: &'a str, context: &BlockContext) -> &'a str {
    let (_, mut content) = strip_block_quotes(line);
    let mut removed_marker = false;
    while let Some((_, item_content)) = list_item_content(content) {
        content = item_content;
        removed_marker = true;
    }
    if !removed_marker
        && let Some(indentation) = context.list_indents.last().copied()
        && content.bytes().take_while(|byte| *byte == b' ').count() >= indentation
    {
        content = &content[indentation..];
    }
    content
}

fn classify_block_line(line: &str, context: &mut BlockContext) -> (bool, bool) {
    let (quote_depth, after_quotes) = strip_block_quotes(line);
    if let Some(open) = context.fence {
        if quote_depth < open.quote_depth {
            context.fence = None;
            return classify_block_line(line, context);
        }
        let Some(content) = fence_container_content(after_quotes, open.list_indent) else {
            context.fence = None;
            return classify_block_line(line, context);
        };
        if let Some((marker, length, remainder)) = fence_marker(content)
            && marker == open.marker
            && length >= open.length
            && remainder.trim().is_empty()
        {
            context.fence = None;
        }
        return (true, false);
    }

    if quote_depth != context.quote_depth {
        context.quote_depth = quote_depth;
        context.paragraph_open = false;
        context.list_indents.clear();
    }

    if after_quotes.trim().is_empty() {
        context.paragraph_open = false;
        return (false, false);
    }

    let (content, starts_list_item) = container_relative_content(after_quotes, context);
    if starts_list_item {
        context.paragraph_open = false;
    }
    let reference_definition_allowed = !context.paragraph_open;
    if let Some((marker, length, _)) = fence_marker(content) {
        context.fence = Some(Fence {
            marker,
            length,
            quote_depth,
            list_indent: context.list_indents.last().copied(),
        });
        context.paragraph_open = false;
        return (true, false);
    }

    if !context.paragraph_open && is_indented_code(content) {
        return (true, false);
    }

    if reference_definition(content).is_none() || reference_definition_allowed {
        context.paragraph_open = opens_paragraph(content);
    }
    (false, reference_definition_allowed)
}

fn container_relative_content<'a>(line: &'a str, context: &mut BlockContext) -> (&'a str, bool) {
    let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
    let matched_depth = context
        .list_indents
        .iter()
        .take_while(|required| indentation >= **required)
        .count();
    let base_indent = if matched_depth == 0 {
        0
    } else {
        context.list_indents[matched_depth - 1]
    };
    let candidate = &line[base_indent..];

    if let Some((relative_indent, content)) = list_item_content(candidate) {
        context.list_indents.truncate(matched_depth);
        context.list_indents.push(base_indent + relative_indent);
        return (content, true);
    }

    if matched_depth < context.list_indents.len() {
        if context.paragraph_open {
            return (line, false);
        }
        context.list_indents.truncate(matched_depth);
    }
    let active_indent = context.list_indents.last().copied().unwrap_or(0);
    (&line[active_indent..], false)
}

fn opens_paragraph(content: &str) -> bool {
    let trimmed = content.trim_start();
    if trimmed.is_empty() || reference_definition(content).is_some() {
        return false;
    }
    if trimmed.starts_with('#') || trimmed.starts_with("---") || trimmed.starts_with("***") {
        return false;
    }
    true
}

fn strip_block_quotes(mut line: &str) -> (usize, &str) {
    let mut depth = 0;
    loop {
        let spaces = line.bytes().take_while(|byte| *byte == b' ').count().min(4);
        if spaces > 3 || line.as_bytes().get(spaces) != Some(&b'>') {
            return (depth, line);
        }
        line = &line[spaces + 1..];
        if line.starts_with(' ') || line.starts_with('\t') {
            line = &line[1..];
        }
        depth += 1;
    }
}

fn list_item_content(line: &str) -> Option<(usize, &str)> {
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    if spaces > 3 {
        return None;
    }
    let rest = &line[spaces..];
    let marker_length = if rest
        .as_bytes()
        .first()
        .is_some_and(|byte| matches!(byte, b'-' | b'+' | b'*'))
    {
        1
    } else {
        let digits = rest
            .bytes()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=9).contains(&digits)
            || !rest
                .as_bytes()
                .get(digits)
                .is_some_and(|byte| matches!(byte, b'.' | b')'))
        {
            return None;
        }
        digits + 1
    };
    let after_marker = &rest[marker_length..];
    if after_marker.trim().is_empty() {
        return Some((
            spaces + marker_length + 1,
            &after_marker[after_marker.len()..],
        ));
    }
    let padding = match after_marker.as_bytes().first().copied()? {
        b' ' => {
            let spaces_after_marker = after_marker
                .bytes()
                .take_while(|byte| *byte == b' ')
                .count();
            if spaces_after_marker <= 4 {
                spaces_after_marker
            } else {
                1
            }
        }
        b'\t' => 1,
        _ => return None,
    };
    let content_start = spaces + marker_length + padding;
    Some((content_start, &line[content_start..]))
}

fn fence_container_content(line: &str, required_indent: Option<usize>) -> Option<&str> {
    let Some(required) = required_indent else {
        return Some(line);
    };
    if line.trim().is_empty() {
        return Some("");
    }
    let indentation = line.bytes().take_while(|byte| *byte == b' ').count();
    (indentation >= required).then(|| &line[required..])
}

fn fence_marker(line: &str) -> Option<(u8, usize, &str)> {
    let content = commonmark_content(line)?;
    let marker = content.as_bytes().first().copied()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = content
        .as_bytes()
        .iter()
        .take_while(|candidate| **candidate == marker)
        .count();
    (length >= 3).then(|| (marker, length, &content[length..]))
}

fn is_indented_code(line: &str) -> bool {
    line.starts_with('\t')
        || line
            .as_bytes()
            .iter()
            .take_while(|byte| **byte == b' ')
            .count()
            >= 4
}

fn mask_inline_code_spans(contents: &str) -> String {
    let mut lines = contents.split('\n').map(str::to_owned).collect::<Vec<_>>();
    let mut start = 0;
    while start < lines.len() {
        if is_inline_block_boundary(&lines[start]) {
            start += 1;
            continue;
        }
        let end = lines[start..]
            .iter()
            .position(|line| is_inline_block_boundary(line))
            .map_or(lines.len(), |offset| start + offset);
        let masked = mask_inline_code_segment(&lines[start..end].join("\n"));
        for (line, replacement) in lines[start..end].iter_mut().zip(masked.split('\n')) {
            *line = replacement.to_owned();
        }
        start = end;
    }
    lines.join("\n")
}

fn is_inline_block_boundary(line: &str) -> bool {
    let (_, content) = strip_block_quotes(line);
    content.trim().is_empty()
        || list_item_content(content).is_some_and(|(_, item)| item.trim().is_empty())
}

fn mask_inline_code_segment(contents: &str) -> String {
    let mut characters: Vec<char> = contents.chars().collect();
    let mut cursor = 0;
    while cursor < characters.len() {
        if characters[cursor] != '`' || is_escaped(&characters, cursor) {
            cursor += 1;
            continue;
        }
        let length = backtick_run_length(&characters, cursor);
        let Some(end) = inline_code_end(&characters, cursor + length, length) else {
            cursor += length;
            continue;
        };
        for character in &mut characters[cursor..end] {
            if *character != '\n' {
                *character = ' ';
            }
        }
        cursor = end;
    }
    characters.into_iter().collect()
}

fn reference_definition(line: &str) -> Option<(String, String)> {
    let (_, after_quotes) = strip_block_quotes(line);
    let line = list_item_content(after_quotes).map_or(after_quotes, |(_, content)| content);
    let characters: Vec<char> = line.chars().collect();
    let indentation = characters
        .iter()
        .take_while(|character| **character == ' ')
        .count();
    if indentation > 3 || characters.get(indentation) != Some(&'[') {
        return None;
    }
    let (label, mut cursor) = bracket_label(&characters, indentation)?;
    if characters.get(cursor) != Some(&':') {
        return None;
    }
    cursor += 1;
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_whitespace())
    {
        cursor += 1;
    }
    let (destination, suffix_start) = parse_reference_destination(&characters, cursor)?;
    let label = normalize_reference_label(&label)?;
    valid_reference_suffix(&characters, suffix_start).then_some((label, destination))
}

fn parse_reference_destination(characters: &[char], mut cursor: usize) -> Option<(String, usize)> {
    if characters.get(cursor) == Some(&'<') {
        return parse_angle_destination(characters, cursor + 1);
    }

    let mut destination = String::new();
    let mut parentheses = 0usize;
    while let Some(character) = characters.get(cursor).copied() {
        match character {
            '\\' => {
                cursor += 1;
                destination.push(*characters.get(cursor)?);
            }
            '(' => {
                parentheses += 1;
                destination.push(character);
            }
            ')' if parentheses == 0 => return None,
            ')' => {
                parentheses -= 1;
                destination.push(character);
            }
            character if character.is_whitespace() && parentheses == 0 => break,
            _ => destination.push(character),
        }
        cursor += 1;
    }
    (!destination.is_empty() && parentheses == 0).then_some((destination, cursor))
}

fn valid_reference_suffix(characters: &[char], mut cursor: usize) -> bool {
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_whitespace())
    {
        cursor += 1;
    }
    let Some(opening) = characters.get(cursor).copied() else {
        return true;
    };
    let closing = match opening {
        '"' => '"',
        '\'' => '\'',
        '(' => ')',
        _ => return false,
    };
    cursor += 1;
    loop {
        match characters.get(cursor).copied() {
            Some('\\') => cursor += 2,
            Some(character) if character == closing => {
                cursor += 1;
                break;
            }
            Some(_) => cursor += 1,
            None => return false,
        }
    }
    characters[cursor..]
        .iter()
        .all(|character| character.is_whitespace())
}

fn reference_labels(line: &str) -> Vec<String> {
    let characters: Vec<char> = line.chars().collect();
    let mut labels = Vec::new();
    let mut cursor = 0;
    while cursor < characters.len() {
        if characters[cursor] == '`' && !is_escaped(&characters, cursor) {
            let length = backtick_run_length(&characters, cursor);
            if let Some(end) = inline_code_end(&characters, cursor + length, length) {
                cursor = end;
                continue;
            }
            cursor += length;
            continue;
        }
        if characters[cursor] != '[' || is_escaped(&characters, cursor) {
            cursor += 1;
            continue;
        }
        let Some((text_label, after_text)) = bracket_label(&characters, cursor) else {
            cursor += 1;
            continue;
        };
        if characters.get(after_text) == Some(&'(') {
            cursor = parse_inline_destination(&characters, after_text + 1)
                .map_or(after_text + 1, |(_, end)| end);
            continue;
        }
        let (label, next) = if characters.get(after_text) == Some(&'[') {
            let Some((explicit, after_label)) = bracket_label(&characters, after_text) else {
                cursor = after_text + 1;
                continue;
            };
            (
                if explicit.is_empty() {
                    text_label
                } else {
                    explicit
                },
                after_label,
            )
        } else {
            (text_label, after_text)
        };
        if let Some(label) = normalize_reference_label(&label) {
            labels.push(label);
        }
        cursor = next;
    }
    labels
}

fn bracket_label(characters: &[char], opening: usize) -> Option<(String, usize)> {
    if characters.get(opening) != Some(&'[') {
        return None;
    }
    let mut label = String::new();
    let mut cursor = opening + 1;
    while let Some(character) = characters.get(cursor).copied() {
        match character {
            '\\' => {
                label.push(character);
                cursor += 1;
                label.push(*characters.get(cursor)?);
            }
            ']' => return Some((label, cursor + 1)),
            '[' => return None,
            _ => label.push(character),
        }
        cursor += 1;
    }
    None
}

fn normalize_reference_label(label: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut characters = label.chars().peekable();
    let mut pending_space = false;
    while let Some(character) = characters.next() {
        let character = if character == '\\'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_punctuation())
        {
            characters.next().expect("peeked punctuation must exist")
        } else {
            character
        };
        if character.is_whitespace() {
            pending_space = !normalized.is_empty();
            continue;
        }
        if pending_space {
            normalized.push(' ');
            pending_space = false;
        }
        normalized.extend(character.to_lowercase());
    }
    (!normalized.is_empty()).then_some(normalized)
}

fn inline_destinations(line: &str) -> Vec<String> {
    let characters: Vec<char> = line.chars().collect();
    let mut destinations = Vec::new();
    let mut cursor = 0;
    while cursor + 1 < characters.len() {
        if characters[cursor] == '`' && !is_escaped(&characters, cursor) {
            let length = backtick_run_length(&characters, cursor);
            if let Some(end) = inline_code_end(&characters, cursor + length, length) {
                cursor = end;
                continue;
            }
            cursor += length;
            continue;
        }
        if characters[cursor] != ']'
            || characters[cursor + 1] != '('
            || is_escaped(&characters, cursor)
        {
            cursor += 1;
            continue;
        }
        cursor += 2;
        if let Some((destination, next)) = parse_inline_destination(&characters, cursor) {
            destinations.push(destination);
            cursor = next;
        }
    }
    destinations
}

fn is_escaped(characters: &[char], cursor: usize) -> bool {
    characters[..cursor]
        .iter()
        .rev()
        .take_while(|character| **character == '\\')
        .count()
        % 2
        == 1
}

fn backtick_run_length(characters: &[char], cursor: usize) -> usize {
    characters[cursor..]
        .iter()
        .take_while(|character| **character == '`')
        .count()
}

fn inline_code_end(characters: &[char], mut cursor: usize, opening_length: usize) -> Option<usize> {
    while cursor < characters.len() {
        if characters[cursor] != '`' {
            cursor += 1;
            continue;
        }
        let closing_length = backtick_run_length(characters, cursor);
        if closing_length == opening_length {
            return Some(cursor + closing_length);
        }
        cursor += closing_length;
    }
    None
}

fn parse_inline_destination(characters: &[char], mut cursor: usize) -> Option<(String, usize)> {
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_whitespace())
    {
        cursor += 1;
    }
    match characters.get(cursor).copied()? {
        ')' => Some((String::new(), cursor + 1)),
        '<' => {
            let (destination, suffix_start) = parse_angle_destination(characters, cursor + 1)?;
            Some((destination, consume_link_suffix(characters, suffix_start)?))
        }
        _ => {
            let (destination, suffix_start) = parse_bare_destination(characters, cursor)?;
            match suffix_start {
                LinkSuffix::Consumed(end) => Some((destination, end)),
                LinkSuffix::Pending(start) => {
                    Some((destination, consume_link_suffix(characters, start)?))
                }
            }
        }
    }
}

fn parse_angle_destination(characters: &[char], mut cursor: usize) -> Option<(String, usize)> {
    let mut destination = String::new();
    while let Some(character) = characters.get(cursor).copied() {
        match character {
            '\\' => {
                cursor += 1;
                destination.push(*characters.get(cursor)?);
            }
            '>' => return Some((destination, cursor + 1)),
            _ => destination.push(character),
        }
        cursor += 1;
    }
    None
}

enum LinkSuffix {
    Consumed(usize),
    Pending(usize),
}

fn parse_bare_destination(characters: &[char], mut cursor: usize) -> Option<(String, LinkSuffix)> {
    let mut destination = String::new();
    let mut parentheses = 0usize;
    while let Some(character) = characters.get(cursor).copied() {
        match character {
            '\\' => {
                cursor += 1;
                destination.push(*characters.get(cursor)?);
            }
            '(' => {
                parentheses += 1;
                destination.push(character);
            }
            ')' if parentheses == 0 => {
                return Some((destination, LinkSuffix::Consumed(cursor + 1)));
            }
            ')' => {
                parentheses -= 1;
                destination.push(character);
            }
            character if character.is_whitespace() && parentheses == 0 => {
                return Some((destination, LinkSuffix::Pending(cursor)));
            }
            _ => destination.push(character),
        }
        cursor += 1;
    }
    None
}

fn consume_link_suffix(characters: &[char], mut cursor: usize) -> Option<usize> {
    let before_whitespace = cursor;
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_whitespace())
    {
        cursor += 1;
    }
    if characters.get(cursor) == Some(&')') {
        return Some(cursor + 1);
    }
    if cursor == before_whitespace {
        return None;
    }

    let closing = match characters.get(cursor)? {
        '"' => '"',
        '\'' => '\'',
        '(' => ')',
        _ => return None,
    };
    cursor += 1;
    loop {
        match characters.get(cursor).copied()? {
            '\\' => cursor += 2,
            character if character == closing => {
                cursor += 1;
                break;
            }
            _ => cursor += 1,
        }
    }
    while characters
        .get(cursor)
        .is_some_and(|character| character.is_whitespace())
    {
        cursor += 1;
    }
    (characters.get(cursor) == Some(&')')).then_some(cursor + 1)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TREE_ID: AtomicU64 = AtomicU64::new(0);

    struct TestTree {
        container: PathBuf,
        root: PathBuf,
    }

    impl TestTree {
        fn new() -> Self {
            let id = NEXT_TREE_ID.fetch_add(1, Ordering::Relaxed);
            let container =
                std::env::temp_dir().join(format!("minicontainer-docs-{}-{id}", process::id()));
            let root = container.join("repository");
            fs::create_dir_all(&root).expect("must create documentation fixture directory");
            Self { container, root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("must create fixture parent");
            }
            fs::write(path, contents).expect("must write fixture");
        }

        fn write_outside(&self, relative: &str, contents: &str) {
            let path = self.container.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("must create outside fixture parent");
            }
            fs::write(path, contents).expect("must write outside fixture");
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.container);
        }
    }

    #[test]
    fn missing_link_diagnostic_names_source_destination_and_line() {
        let temp = TestTree::new();
        temp.write("docs/index.md", "# Index\n[missing](missing.md)\n");

        let error = check_local_links(temp.path()).expect_err("link must be missing");

        assert_eq!(error.path(), Path::new("missing.md"));
        assert_eq!(
            error.to_string(),
            "docs/index.md:2: missing local link destination missing.md"
        );
    }

    #[test]
    fn reference_links_report_the_definition_destination_and_line() {
        for usage in ["[guide][g]", "[guide][]", "[guide]"] {
            let temp = TestTree::new();
            let definition = if usage == "[guide][g]" { "g" } else { "guide" };
            temp.write(
                "docs/index.md",
                &format!("# Index\n{usage}\n\n[{definition}]: missing.md \"Guide\"\n"),
            );

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "docs/index.md:4: missing local link destination missing.md"
            );
        }
    }

    #[test]
    fn reference_labels_are_case_folded_whitespace_collapsed_and_unescaped() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[guide][G\\!   label]\n\n[g! label]: docs/guide.md\n",
        );
        temp.write("docs/guide.md", "# Guide\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn reference_labels_only_remove_backslashes_before_escapable_punctuation() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[literal reference][a\\q]\n\n[aq]: docs/missing.md\n",
        );

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn first_reference_definition_wins_and_undefined_references_are_literal_text() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[defined][guide] [undefined][nowhere] [collapsed][] [shortcut]\n\
             [guide]: docs/guide.md\n\
             [guide]: missing.md\n",
        );
        temp.write("docs/guide.md", "# Guide\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn inline_link_titles_do_not_create_reference_uses_and_scanning_resumes_after_the_link() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[inline](docs/inline.md \"title contains [fake]\") [real link][real]\n\n\
             [fake]: docs/fake-missing.md\n\
             [real]: docs/real-missing.md\n",
        );
        temp.write("docs/inline.md", "# Inline\n");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:4: missing local link destination docs/real-missing.md"
        );
    }

    #[test]
    fn image_references_check_local_targets_and_ignore_external_schemes() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "![remote][logo]\n![local][missing]\n\n[logo]: https://example.com/logo.png\n[missing]: assets/missing.png\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:5: missing local link destination assets/missing.png"
        );
    }

    #[test]
    fn reference_destinations_cannot_lexically_escape_the_repository() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[outside][escape]\n\n[escape]: ../outside.md\n",
        );
        temp.write_outside("outside.md", "# Outside\n");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:3: local link destination escapes repository root: ../outside.md"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reference_destinations_cannot_follow_a_symlink_outside_the_repository() {
        use std::os::unix::fs::symlink;

        let temp = TestTree::new();
        temp.write("README.md", "[outside][escape]\n\n[escape]: linked.md\n");
        temp.write_outside("outside.md", "# Outside\n");
        symlink(
            temp.container.join("outside.md"),
            temp.root.join("linked.md"),
        )
        .expect("must create reference symlink fixture");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:3: local link destination escapes repository root: linked.md"
        );
    }

    #[test]
    fn container_reference_definitions_report_missing_destinations() {
        for definition in ["> [guide]: docs/missing.md", "- [guide]: docs/missing.md"] {
            let temp = TestTree::new();
            temp.write("README.md", &format!("[read it][guide]\n\n{definition}\n"));

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "README.md:3: missing local link destination docs/missing.md"
            );
        }
    }

    #[test]
    fn indented_list_continuation_reference_definitions_use_container_relative_columns() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            concat!(
                "   - item\n",
                "\n",
                "     [guide]: docs/missing.md\n",
                "\n",
                "[read it][guide]\n",
            ),
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:3: missing local link destination docs/missing.md"
        );
    }

    #[test]
    fn list_marker_padding_sets_the_container_column_for_continuation_paragraphs() {
        for marker in ["-", "1.", "2)"] {
            for (padding, relative_indentation) in [(1, 0), (2, 3), (3, 2), (4, 1)] {
                let temp = TestTree::new();
                let content_indentation = marker.len() + padding;
                temp.write(
                    "README.md",
                    &format!(
                        "{marker}{padding_spaces}item\n\
                         \n\
                         {exact_spaces}[existing](docs/existing.md)\n\
                         \n\
                         {relative_spaces}[missing](docs/missing.md)\n",
                        padding_spaces = " ".repeat(padding),
                        exact_spaces = " ".repeat(content_indentation),
                        relative_spaces = " ".repeat(content_indentation + relative_indentation),
                    ),
                );
                temp.write("docs/existing.md", "# Existing\n");

                assert_eq!(
                    check_local_links(temp.path()).unwrap_err().to_string(),
                    "README.md:5: missing local link destination docs/missing.md",
                    "marker {marker:?} with {padding}-space padding",
                );
            }
        }
    }

    #[test]
    fn list_marker_padding_keeps_actual_indented_code_masked() {
        for marker in ["-", "1.", "2)"] {
            for padding in 1..=4 {
                let temp = TestTree::new();
                let content_indentation = marker.len() + padding;
                temp.write(
                    "README.md",
                    &format!(
                        "{marker}{padding_spaces}item\n\n{code_spaces}[ignored](docs/missing.md)\n",
                        padding_spaces = " ".repeat(padding),
                        code_spaces = " ".repeat(content_indentation + 4),
                    ),
                );

                assert_eq!(
                    check_local_links(temp.path()),
                    Ok(()),
                    "marker {marker:?} with {padding}-space padding",
                );
            }
        }
    }

    #[test]
    fn marker_only_blank_list_items_mask_list_contained_indented_code() {
        for (marker, code_indentation) in [("-", 6), ("1.", 7), ("2)", 7)] {
            let temp = TestTree::new();
            temp.write(
                "README.md",
                &format!(
                    "{marker}\n{code_spaces}[ignored](docs/missing.md)\n日本語の続き\n",
                    code_spaces = " ".repeat(code_indentation),
                ),
            );

            assert_eq!(
                check_local_links(temp.path()),
                Ok(()),
                "marker-only blank item {marker:?}",
            );
        }
    }

    #[test]
    fn spaces_only_blank_list_items_mask_list_contained_indented_code() {
        for (marker, code_indentation) in [("-    ", 6), ("1.    ", 7), ("2)    ", 7)] {
            let temp = TestTree::new();
            temp.write(
                "README.md",
                &format!(
                    "{marker}\n{code_spaces}[ignored](docs/missing.md)\n日本語の続き\n",
                    code_spaces = " ".repeat(code_indentation),
                ),
            );

            assert_eq!(
                check_local_links(temp.path()),
                Ok(()),
                "spaces-only blank item {marker:?}",
            );
        }
    }

    #[test]
    fn blank_list_item_continuation_links_use_one_column_padding() {
        for (marker, content_indentation) in [
            ("-", 2),
            ("1.", 3),
            ("2)", 3),
            ("-    ", 2),
            ("1.    ", 3),
            ("2)    ", 3),
        ] {
            let temp = TestTree::new();
            temp.write(
                "README.md",
                &format!(
                    "{marker}\n{content_spaces}[missing](docs/continuation.md)\n",
                    content_spaces = " ".repeat(content_indentation),
                ),
            );

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "README.md:2: missing local link destination docs/continuation.md",
                "blank item {marker:?}",
            );
        }
    }

    #[test]
    fn blank_list_item_state_ends_before_unindented_content() {
        for (marker, code_indentation) in [
            ("-", 6),
            ("1.", 7),
            ("2)", 7),
            ("-    ", 6),
            ("1.    ", 7),
            ("2)    ", 7),
        ] {
            let temp = TestTree::new();
            temp.write(
                "README.md",
                &format!(
                    "{marker}\n{code_spaces}[ignored](docs/code.md)\n[missing](docs/outside.md)\n",
                    code_spaces = " ".repeat(code_indentation),
                ),
            );

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "README.md:3: missing local link destination docs/outside.md",
                "blank item {marker:?}",
            );
        }
    }

    #[test]
    fn reference_definition_shaped_lines_do_not_interrupt_open_paragraphs() {
        for contents in [
            "Paragraph text\n[guide]: docs/missing.md\n[read it][guide]\n",
            "- Paragraph text\n  [guide]: docs/missing.md\n  [read it][guide]\n",
        ] {
            let temp = TestTree::new();
            temp.write("README.md", contents);

            assert_eq!(check_local_links(temp.path()), Ok(()));
        }
    }

    #[test]
    fn reference_definitions_after_blank_lines_are_recognized() {
        for (contents, line) in [
            (
                "Paragraph text\n\n[guide]: docs/missing.md\n[read it][guide]\n",
                3,
            ),
            (
                "- Paragraph text\n\n  [guide]: docs/missing.md\n\n  [read it][guide]\n",
                3,
            ),
        ] {
            let temp = TestTree::new();
            temp.write("README.md", contents);

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                format!("README.md:{line}: missing local link destination docs/missing.md"),
            );
        }
    }

    #[test]
    fn container_reference_definitions_do_not_open_a_paragraph_before_indented_code() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            concat!(
                "   - item\n",
                "\n",
                "     [guide]: docs/guide.md\n",
                "         [ignored](inside-code.md)\n",
                "\n",
                "[read it][guide]\n",
            ),
        );
        temp.write("docs/guide.md", "# Guide\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn container_reference_definitions_cannot_lexically_escape_the_repository() {
        for definition in ["> [outside]: ../outside.md", "- [outside]: ../outside.md"] {
            let temp = TestTree::new();
            temp.write("README.md", &format!("[leave][outside]\n\n{definition}\n"));
            temp.write_outside("outside.md", "# Outside\n");

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "README.md:3: local link destination escapes repository root: ../outside.md"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn container_reference_definitions_cannot_follow_a_symlink_outside_the_repository() {
        use std::os::unix::fs::symlink;

        for definition in ["> [outside]: linked.md", "- [outside]: linked.md"] {
            let temp = TestTree::new();
            temp.write("README.md", &format!("[leave][outside]\n\n{definition}\n"));
            temp.write_outside("outside.md", "# Outside\n");
            symlink(
                temp.container.join("outside.md"),
                temp.root.join("linked.md"),
            )
            .expect("must create container reference symlink fixture");

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "README.md:3: local link destination escapes repository root: linked.md"
            );
        }
    }

    #[test]
    fn container_reference_definitions_resolve_existing_destinations() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[quote guide][quote] [list guide][list]\n\n\
             > [quote]: docs/quote.md\n\n\
             - [list]: docs/list.md\n",
        );
        temp.write("docs/quote.md", "# Quote guide\n");
        temp.write("docs/list.md", "# List guide\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn accepts_relative_links_parent_segments_and_anchors_inside_repository() {
        let temp = TestTree::new();
        temp.write(
            "docs/guide/chapter.md",
            "[guide](../guide.md#start) [self](#section)\n",
        );
        temp.write("docs/guide.md", "# Start\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn rejects_parent_segments_that_escape_repository() {
        let temp = TestTree::new();
        temp.write("README.md", "[outside](../outside.md)\n");
        temp.write_outside("outside.md", "# Outside\n");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:1: local link destination escapes repository root: ../outside.md"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_file_and_directory_symlink_destinations_outside_repository() {
        use std::os::unix::fs::symlink;

        for (link, destination) in [
            ("linked.md", "outside.md"),
            ("linked/destination.md", "outside/destination.md"),
        ] {
            let temp = TestTree::new();
            temp.write("README.md", &format!("[outside]({link})\n"));
            temp.write_outside(destination, "# Outside\n");
            if link == "linked.md" {
                symlink(
                    temp.container.join(destination),
                    temp.root.join("linked.md"),
                )
                .expect("must create file symlink fixture");
            } else {
                symlink(temp.container.join("outside"), temp.root.join("linked"))
                    .expect("must create directory symlink fixture");
            }

            let diagnostic = check_local_links(temp.path()).unwrap_err().to_string();
            assert!(diagnostic.contains("README.md:1"));
            assert!(diagnostic.contains(link));
            assert!(diagnostic.contains("escapes repository root"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn does_not_traverse_directory_symlinks_while_finding_markdown() {
        use std::os::unix::fs::symlink;

        let temp = TestTree::new();
        temp.write("README.md", "# Repository\n");
        temp.write_outside("outside/broken.md", "[missing](missing.md)\n");
        symlink(temp.container.join("outside"), temp.root.join("linked"))
            .expect("must create directory symlink fixture");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn accepts_repository_root_given_through_a_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TestTree::new();
        temp.write("README.md", "[guide](guide.md)\n");
        temp.write("guide.md", "# Guide\n");
        let linked_root = temp.container.join("linked-repository");
        symlink(temp.path(), &linked_root).expect("must create root symlink fixture");

        assert_eq!(check_local_links(&linked_root), Ok(()));
    }

    #[test]
    fn ignores_external_schemes_absolute_paths_and_fragment_only_links() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[http](http://example.com) [https](HTTPS://example.com) [mail](mailto:a@example.com) [ftp](ftp://example.com) [data](data:text/plain,x) [root](/missing.md) [self](#here)\n",
        );

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn ignores_links_inside_backtick_and_tilde_fences() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "```markdown\n[missing](one.md)\n```\n~~~~\n[missing](two.md)\n~~~~\n",
        );

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn ignores_links_inside_space_and_tab_indented_code_blocks() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "    [ignored](one.md)\n\t[ignored][inside]\n\n[inside]: two.md\n[missing](real.md)\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:5: missing local link destination real.md"
        );
    }

    #[test]
    fn four_space_indentation_cannot_interrupt_an_open_paragraph() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "paragraph begins\n    [missing](paragraph-continuation.md)\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:2: missing local link destination paragraph-continuation.md"
        );
    }

    #[test]
    fn list_continuation_indentation_is_measured_relative_to_each_list_container() {
        for (markdown, diagnostic) in [
            (
                "- item\n\n    [missing](list-continuation.md)\n",
                "README.md:3: missing local link destination list-continuation.md",
            ),
            (
                "- outer\n  - inner\n\n      [missing](nested-continuation.md)\n",
                "README.md:4: missing local link destination nested-continuation.md",
            ),
        ] {
            let temp = TestTree::new();
            temp.write("README.md", markdown);

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                diagnostic
            );
        }
    }

    #[test]
    fn utf8_lazy_list_continuations_are_checked_without_slicing_inside_a_character() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "- list paragraph\n既存の続き [missing](lazy-continuation.md)\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:2: missing local link destination lazy-continuation.md"
        );
    }

    #[test]
    fn actual_top_level_and_list_contained_indented_code_stays_masked() {
        for markdown in [
            "    [ignored](top-level-code.md)\n",
            "- item\n\n      [ignored](list-code.md)\n",
            "- outer\n  - inner\n\n        [ignored](nested-list-code.md)\n",
        ] {
            let temp = TestTree::new();
            temp.write("README.md", markdown);

            assert_eq!(check_local_links(temp.path()), Ok(()));
        }
    }

    #[test]
    fn paragraph_and_list_state_reset_at_blank_and_container_boundaries() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            concat!(
                "- item\n",
                "\n",
                "    [existing](docs/existing.md)\n",
                "\n",
                "outside paragraph\n",
                "    [existing](docs/existing.md)\n",
                "\n",
                "    [ignored](after-blank-code.md)\n",
                "[missing](after-reset.md)\n",
            ),
        );
        temp.write("docs/existing.md", "# Existing\n");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:9: missing local link destination after-reset.md"
        );
    }

    #[test]
    fn ignores_fences_nested_in_blockquotes_and_lists_without_skipping_container_links() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "> ```markdown\n> [ignored](one.md)\n> ```\n\
             - ~~~markdown\n  [ignored][inside]\n  ~~~\n\
             [inside]: two.md\n\
             > [missing](real.md)\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:8: missing local link destination real.md"
        );
    }

    #[test]
    fn checks_real_links_in_blockquotes_and_list_items() {
        for (markdown, destination) in [
            ("> [missing](quote.md)\n", "quote.md"),
            ("- [missing](list.md)\n", "list.md"),
        ] {
            let temp = TestTree::new();
            temp.write("README.md", markdown);

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().path(),
                Path::new(destination)
            );
        }
    }

    #[test]
    fn an_unclosed_container_fence_ends_when_its_container_ends() {
        for markdown in [
            "> ```markdown\n> [ignored](inside.md)\n[missing](real.md)\n",
            "- ```markdown\n  [ignored](inside.md)\n[missing](real.md)\n",
        ] {
            let temp = TestTree::new();
            temp.write("README.md", markdown);

            assert_eq!(
                check_local_links(temp.path()).unwrap_err().to_string(),
                "README.md:3: missing local link destination real.md"
            );
        }
    }

    #[test]
    fn multiline_code_spans_ignore_links_until_the_matching_backtick_run() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "``code `\n[ignored](one.md) [ignored][inside]\nstill code`` [missing](real.md)\n\
             [inside]: two.md\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:3: missing local link destination real.md"
        );
    }

    #[test]
    fn unmatched_code_span_openers_are_literal_and_code_state_resets_between_files() {
        let temp = TestTree::new();
        temp.write("a.md", "`unmatched\n[missing](first.md)\n");
        temp.write("b.md", "````markdown\n[ignored](inside.md)\n");
        temp.write("c.md", "[missing](later.md)\n");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "a.md:2: missing local link destination first.md"
        );
    }

    #[test]
    fn multiline_code_spans_do_not_cross_a_blank_line_block_boundary() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "`unmatched opener\n\n[missing](real.md) trailing `\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().to_string(),
            "README.md:3: missing local link destination real.md"
        );
    }

    #[test]
    fn shorter_or_indented_markers_do_not_close_or_open_fences() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "~~~~markdown\n~~~\n[ignored](one.md)\n    ~~~~\n[ignored](two.md)\n~~~~\n    ```\n[missing](real.md)\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().path(),
            Path::new("real.md")
        );
    }

    #[test]
    fn ignores_inline_code_and_escaped_pseudo_links_but_finds_the_next_link() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "日本語``code `[ignored](one.md)` code`` \\](two.md) [missing](real.md)\n",
        );

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().path(),
            Path::new("real.md")
        );
    }

    #[test]
    fn accepts_angle_escaped_and_nested_parenthesis_destinations() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[space](<docs/file name.md>) [escaped](docs/guide\\(ja\\).md) [nested](docs/other(en).md)\n",
        );
        temp.write("docs/file name.md", "# Space\n");
        temp.write("docs/guide(ja).md", "# Escaped\n");
        temp.write("docs/other(en).md", "# Nested\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn accepts_all_title_forms_and_does_not_scan_inside_titles() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[one](guide.md \"see ](missing.md)\") [two](guide.md 'title') [three](guide.md (see \\(details\\)))\n",
        );
        temp.write("guide.md", "# Guide\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn finds_a_second_link_after_inline_code_and_a_titled_link() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "`[ignored](one.md)` [guide](guide.md \"title\") [missing](real.md)\n",
        );
        temp.write("guide.md", "# Guide\n");

        assert_eq!(
            check_local_links(temp.path()).unwrap_err().path(),
            Path::new("real.md")
        );
    }

    #[test]
    fn malformed_or_unterminated_title_suffix_is_not_treated_as_a_link() {
        let temp = TestTree::new();
        temp.write(
            "README.md",
            "[one](missing.md \"title\" trailing)\n[two](missing.md \"unterminated\\\n",
        );

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }

    #[test]
    fn ignores_markdown_below_git_and_target_directories() {
        let temp = TestTree::new();
        temp.write(".git/notes.md", "[missing](missing.md)\n");
        temp.write("target/generated/docs.md", "[missing](missing.md)\n");

        assert_eq!(check_local_links(temp.path()), Ok(()));
    }
}
