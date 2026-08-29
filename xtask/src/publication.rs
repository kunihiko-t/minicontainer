use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

const REQUIRED_PUBLICATION_FILES: [&str; 8] = [
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "SECURITY.md",
    "CONTRIBUTING.md",
    "README.md",
    "docs/design/architecture.md",
    "docs/guide/README.md",
    "docs/reference/threat-model.md",
];

const REQUIRED_README_TEXT: [&str; 2] = [
    "MIT OR Apache-2.0",
    "本番用のセキュリティー境界ではありません",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForbiddenKind {
    LocalPath,
    LocalUrl,
    PersonalEmail,
    PrivateKey,
    GitHubToken,
    AwsAccessKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityRole {
    Author,
    Committer,
}

#[derive(Debug, PartialEq, Eq)]
struct WorkflowPolicyError {
    line: usize,
    message: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
struct ActionReference {
    line: usize,
    value: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublicationError {
    Git {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    Read {
        path: PathBuf,
        message: String,
    },
    MissingFile {
        path: PathBuf,
    },
    FileEscapesRoot {
        path: PathBuf,
    },
    MissingReadmeText {
        text: &'static str,
    },
    NonUtf8TrackedPath,
    ForbiddenPath {
        path: PathBuf,
    },
    ForbiddenContent {
        path: PathBuf,
        line: usize,
        kind: ForbiddenKind,
    },
    Workflow {
        path: PathBuf,
        line: usize,
        message: &'static str,
    },
    MutableAction {
        path: PathBuf,
        line: usize,
        reference: String,
    },
    NonPublicIdentity {
        role: IdentityRole,
        email: String,
    },
}

impl PublicationError {
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Read { path, .. }
            | Self::MissingFile { path }
            | Self::FileEscapesRoot { path }
            | Self::ForbiddenPath { path }
            | Self::ForbiddenContent { path, .. }
            | Self::Workflow { path, .. }
            | Self::MutableAction { path, .. } => Some(path),
            Self::MissingReadmeText { .. } => Some(Path::new("README.md")),
            Self::Git { .. } | Self::NonUtf8TrackedPath | Self::NonPublicIdentity { .. } => None,
        }
    }
}

impl fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git {
                args,
                status,
                stderr,
            } => write!(
                formatter,
                "git {} failed (status: {}): {}",
                args.join(" "),
                status.map_or_else(
                    || "terminated by signal".to_owned(),
                    |status| status.to_string()
                ),
                stderr.trim_end()
            ),
            Self::Read { path, message } => {
                write!(formatter, "could not read {}: {message}", path.display())
            }
            Self::MissingFile { path } => {
                write!(
                    formatter,
                    "{}: missing required publication file",
                    path.display()
                )
            }
            Self::FileEscapesRoot { path } => write!(
                formatter,
                "{}: required publication file escapes repository root",
                path.display()
            ),
            Self::MissingReadmeText { text } => {
                write!(formatter, "README.md: missing required text: {text}")
            }
            Self::NonUtf8TrackedPath => write!(formatter, "git returned a non-UTF-8 tracked path"),
            Self::ForbiddenPath { path } => {
                write!(formatter, "{}: forbidden tracked path", path.display())
            }
            Self::ForbiddenContent { path, line, kind } => {
                write!(
                    formatter,
                    "{}:{line}: forbidden {}",
                    path.display(),
                    forbidden_kind_name(*kind)
                )
            }
            Self::Workflow {
                path,
                line,
                message,
            } => write!(
                formatter,
                "{}:{line}: malformed workflow: {message}",
                path.display()
            ),
            Self::MutableAction {
                path,
                line,
                reference,
            } => write!(
                formatter,
                "{}:{line}: action reference must be pinned to a 40-character commit SHA: {reference}",
                path.display()
            ),
            Self::NonPublicIdentity { role, email } => write!(
                formatter,
                "non-public {} email in HEAD history: {email}",
                identity_role_name(*role)
            ),
        }
    }
}

impl std::error::Error for PublicationError {}

pub fn check(root: &Path) -> Result<(), PublicationError> {
    check_required_files(root)?;
    check_readme(root)?;
    check_tracked_tree(root)?;
    check_workflow_policy(root)?;
    check_head_identities(root)?;
    Ok(())
}

fn check_workflow_policy(root: &Path) -> Result<(), PublicationError> {
    for path in tracked_workflows(root)? {
        let contents =
            fs::read_to_string(root.join(&path)).map_err(|error| PublicationError::Read {
                path: path.clone(),
                message: error.to_string(),
            })?;
        for reference in
            workflow_action_references(&contents).map_err(|error| PublicationError::Workflow {
                path: path.clone(),
                line: error.line,
                message: error.message,
            })?
        {
            if !is_immutable_action_reference(&reference.value) {
                return Err(PublicationError::MutableAction {
                    path,
                    line: reference.line,
                    reference: reference.value,
                });
            }
        }
    }
    check_dependabot(root)
}

fn tracked_workflows(root: &Path) -> Result<Vec<PathBuf>, PublicationError> {
    let output = git(root, ["ls-files", "-z", "--", ".github/workflows"])?;
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path =
                std::str::from_utf8(path).map_err(|_| PublicationError::NonUtf8TrackedPath)?;
            let path = PathBuf::from(path);
            if path.extension().is_some_and(|extension| extension == "yml") {
                Ok(Some(path))
            } else {
                Ok(None)
            }
        })
        .filter_map(Result::transpose)
        .collect()
}

fn check_dependabot(root: &Path) -> Result<(), PublicationError> {
    let path = PathBuf::from(".github/dependabot.yml");
    let contents = fs::read_to_string(root.join(&path)).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PublicationError::MissingFile { path: path.clone() }
        } else {
            PublicationError::Read {
                path: path.clone(),
                message: error.to_string(),
            }
        }
    })?;
    for ecosystem in ["cargo", "github-actions"] {
        if !has_active_dependabot_ecosystem(&contents, ecosystem) {
            return Err(PublicationError::Workflow {
                path,
                line: 1,
                message: if ecosystem == "cargo" {
                    "missing active Dependabot cargo ecosystem"
                } else {
                    "missing active Dependabot github-actions ecosystem"
                },
            });
        }
    }
    Ok(())
}

fn has_active_dependabot_ecosystem(contents: &str, ecosystem: &str) -> bool {
    let mut updates_indent = None;
    let mut update = None;
    for raw_line in contents.lines() {
        let indentation = raw_line.bytes().take_while(|byte| *byte == b' ').count();
        let line = strip_yaml_comment(&raw_line[indentation..]).trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = yaml_mapping(line)
            && key == "updates"
            && value.is_empty()
        {
            updates_indent = Some(indentation);
            update = None;
            continue;
        }
        let Some(parent_indent) = updates_indent else {
            continue;
        };
        if indentation <= parent_indent {
            if update.is_some_and(|update: DependabotUpdate| {
                update.ecosystem == ecosystem && update.has_weekly_schedule
            }) {
                return true;
            }
            updates_indent = None;
            update = None;
            continue;
        }
        let Some(list) = line.strip_prefix('-') else {
            let Some(update) = update.as_mut() else {
                continue;
            };
            if indentation <= update.indentation {
                continue;
            }
            let Some((key, value)) = yaml_mapping(line.trim_start()) else {
                continue;
            };
            if indentation == update.mapping_indent {
                update.schedule =
                    (key == "schedule" && value.is_empty()).then_some(DependabotSchedule {
                        indentation,
                        child_indent: None,
                    });
            } else if let Some(schedule) = update.schedule.as_mut()
                && indentation > schedule.indentation
            {
                let child_indent = schedule.child_indent.get_or_insert(indentation);
                if indentation == *child_indent
                    && key == "interval"
                    && unquote_yaml_scalar(value) == Some("weekly")
                {
                    update.has_weekly_schedule = true;
                }
            }
            continue;
        };
        let marker_width = list.bytes().take_while(|byte| *byte == b' ').count();
        let line = &list[marker_width..];
        let mapping_indent = indentation + 1 + marker_width;
        if update.is_some_and(|update: DependabotUpdate| {
            update.ecosystem == ecosystem && update.has_weekly_schedule
        }) {
            return true;
        }
        if update.is_some_and(|update: DependabotUpdate| update.indentation == indentation) {
            update = None;
        }
        let Some((key, value)) = yaml_mapping(line.trim_start()) else {
            continue;
        };
        if key == "package-ecosystem" {
            update = Some(DependabotUpdate {
                indentation,
                mapping_indent,
                ecosystem: unquote_yaml_scalar(value).unwrap_or_default(),
                schedule: None,
                has_weekly_schedule: false,
            });
        }
    }
    update.is_some_and(|update: DependabotUpdate| {
        update.ecosystem == ecosystem && update.has_weekly_schedule
    })
}

#[derive(Clone, Copy)]
struct DependabotUpdate<'a> {
    indentation: usize,
    mapping_indent: usize,
    ecosystem: &'a str,
    schedule: Option<DependabotSchedule>,
    has_weekly_schedule: bool,
}

#[derive(Clone, Copy)]
struct DependabotSchedule {
    indentation: usize,
    child_indent: Option<usize>,
}

fn is_immutable_action_reference(reference: &str) -> bool {
    if reference.starts_with("./") {
        return true;
    }
    if reference.starts_with("docker://") {
        return false;
    }
    reference
        .rsplit_once('@')
        .is_some_and(|(_, pin)| pin.len() == 40 && pin.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn workflow_action_references(contents: &str) -> Result<Vec<ActionReference>, WorkflowPolicyError> {
    let mut state = WorkflowScannerState::default();
    let mut references = Vec::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = index + 1;
        let indentation = yaml_indentation(raw_line, line)?;
        let content = strip_yaml_comment(raw_line[indentation..].as_ref()).trim_end();
        if content.is_empty() {
            continue;
        }

        if state
            .block_scalar_owner
            .is_some_and(|owner| indentation > owner)
        {
            continue;
        }
        state.block_scalar_owner = None;

        let (is_list_item, mapping, mapping_indent) = yaml_list_item(content, indentation, line)?;

        let parsed = yaml_mapping(mapping);

        let Some((key, value)) = parsed else {
            state.leave_scopes(indentation, is_list_item);
            continue;
        };

        if !is_list_item
            && indentation == 0
            && parsed.is_some_and(|(key, value)| key == "jobs" && value.is_empty())
        {
            state.enter_jobs(indentation);
            continue;
        }

        state.leave_scopes(indentation, is_list_item);
        state.validate_mapping_indentation(line, indentation, mapping_indent, is_list_item)?;

        if state.enter_job(indentation, is_list_item) {
            continue;
        }
        state.observe_job_mapping(line, mapping_indent, value)?;
        if state.enter_steps(indentation, is_list_item, key, value) {
            continue;
        }
        if state.enter_step(line, indentation, mapping_indent, is_list_item)? {
            if key == "uses" {
                references.push(ActionReference {
                    line,
                    value: yaml_scalar(value, line)?,
                });
            }
            continue;
        }
        if state.is_current_step_mapping(mapping_indent, is_list_item) && key == "uses" {
            references.push(ActionReference {
                line,
                value: yaml_scalar(value, line)?,
            });
        }
    }

    Ok(references)
}

#[derive(Default)]
struct WorkflowScannerState {
    jobs_indent: Option<usize>,
    job_indent: Option<usize>,
    job_mapping_indent: Option<usize>,
    job_mappings: Vec<JobMappingScope>,
    steps_indent: Option<usize>,
    step_list_indent: Option<usize>,
    step_mapping_indent: Option<usize>,
    block_scalar_owner: Option<usize>,
}

struct JobMappingScope {
    indentation: usize,
    accepts_children: bool,
}

impl WorkflowScannerState {
    fn enter_jobs(&mut self, indentation: usize) {
        *self = Self {
            jobs_indent: Some(indentation),
            ..Self::default()
        };
    }

    fn leave_scopes(&mut self, indentation: usize, is_list_item: bool) {
        if self.jobs_indent.is_some_and(|jobs| indentation <= jobs) {
            *self = Self::default();
            return;
        }
        if self
            .job_indent
            .is_some_and(|job| indentation <= job && !is_list_item)
        {
            self.job_mapping_indent = None;
            self.job_mappings.clear();
            self.steps_indent = None;
            self.step_list_indent = None;
            self.step_mapping_indent = None;
        }
        if self
            .steps_indent
            .is_some_and(|steps| indentation <= steps && !is_list_item)
        {
            self.steps_indent = None;
            self.step_list_indent = None;
            self.step_mapping_indent = None;
        }
    }

    fn enter_job(&mut self, indentation: usize, is_list_item: bool) -> bool {
        let Some(jobs_indent) = self.jobs_indent else {
            return false;
        };
        if is_list_item || indentation <= jobs_indent {
            return false;
        }
        match self.job_indent {
            None => {
                self.job_indent = Some(indentation);
                true
            }
            Some(job_indent) if indentation == job_indent => {
                self.job_mapping_indent = None;
                self.job_mappings.clear();
                self.steps_indent = None;
                self.step_list_indent = None;
                self.step_mapping_indent = None;
                true
            }
            _ => false,
        }
    }

    fn enter_steps(
        &mut self,
        indentation: usize,
        is_list_item: bool,
        key: &str,
        value: &str,
    ) -> bool {
        let Some(job_indent) = self.job_indent else {
            return false;
        };
        if is_list_item || indentation <= job_indent {
            return false;
        }
        let mapping_indent = self.job_mapping_indent.get_or_insert(indentation);
        if indentation == *mapping_indent && key == "steps" && value.is_empty() {
            self.steps_indent = Some(indentation);
            self.step_list_indent = None;
            self.step_mapping_indent = None;
            return true;
        }
        false
    }

    fn enter_step(
        &mut self,
        line: usize,
        indentation: usize,
        mapping_indent: usize,
        is_list_item: bool,
    ) -> Result<bool, WorkflowPolicyError> {
        let Some(steps_indent) = self.steps_indent else {
            return Ok(false);
        };
        if !is_list_item || indentation <= steps_indent {
            return Ok(false);
        }
        match self.step_list_indent {
            None => {
                self.step_list_indent = Some(indentation);
                self.step_mapping_indent = Some(mapping_indent);
                Ok(true)
            }
            Some(step_indent) if indentation == step_indent => {
                self.step_mapping_indent = Some(mapping_indent);
                Ok(true)
            }
            Some(step_indent) if indentation < step_indent => Err(WorkflowPolicyError {
                line,
                message: "inconsistent step list indentation",
            }),
            _ => Ok(false),
        }
    }

    fn is_current_step_mapping(&self, mapping_indent: usize, is_list_item: bool) -> bool {
        !is_list_item && self.step_mapping_indent == Some(mapping_indent)
    }

    fn validate_mapping_indentation(
        &self,
        line: usize,
        indentation: usize,
        mapping_indent: usize,
        is_list_item: bool,
    ) -> Result<(), WorkflowPolicyError> {
        if is_list_item
            && self.steps_indent.is_some_and(|steps| indentation > steps)
            && self.step_list_indent.is_some_and(|step| indentation < step)
        {
            return Err(WorkflowPolicyError {
                line,
                message: "inconsistent step list indentation",
            });
        }
        if is_list_item
            && self.step_list_indent.is_some_and(|step| indentation > step)
            && self
                .job_mappings
                .iter()
                .rev()
                .find(|scope| scope.indentation < mapping_indent)
                .is_some_and(|scope| !scope.accepts_children)
        {
            return Err(WorkflowPolicyError {
                line,
                message: "inconsistent step list indentation",
            });
        }
        if !is_list_item
            && self
                .step_list_indent
                .is_some_and(|step| mapping_indent > step)
            && self
                .step_mapping_indent
                .is_some_and(|step| mapping_indent < step)
        {
            return Err(WorkflowPolicyError {
                line,
                message: "inconsistent step mapping indentation",
            });
        }
        Ok(())
    }

    fn observe_job_mapping(
        &mut self,
        line: usize,
        indentation: usize,
        value: &str,
    ) -> Result<(), WorkflowPolicyError> {
        let Some(job_indent) = self.job_indent else {
            return Ok(());
        };
        if indentation <= job_indent {
            return Ok(());
        }
        while self
            .job_mappings
            .last()
            .is_some_and(|scope| scope.indentation >= indentation)
        {
            self.job_mappings.pop();
        }
        if self
            .job_mappings
            .last()
            .is_some_and(|scope| !scope.accepts_children)
        {
            return Err(WorkflowPolicyError {
                line,
                message: "unexpected indentation after scalar value",
            });
        }
        self.job_mappings.push(JobMappingScope {
            indentation,
            accepts_children: value.is_empty(),
        });
        if is_block_scalar(value) {
            self.block_scalar_owner = Some(indentation);
        }
        Ok(())
    }
}

fn yaml_indentation(line: &str, number: usize) -> Result<usize, WorkflowPolicyError> {
    let indentation = line
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count();
    if line.as_bytes()[..indentation].contains(&b'\t') {
        return Err(WorkflowPolicyError {
            line: number,
            message: "tabs are not allowed in indentation",
        });
    }
    Ok(indentation)
}

fn yaml_list_item(
    content: &str,
    indentation: usize,
    line: usize,
) -> Result<(bool, &str, usize), WorkflowPolicyError> {
    let Some(rest) = content.strip_prefix('-') else {
        return Ok((false, content, indentation));
    };
    if !rest.is_empty() && !matches!(rest.as_bytes().first(), Some(b' ' | b'\t')) {
        return Ok((false, content, indentation));
    }
    let marker_width = rest
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count();
    if rest.as_bytes()[..marker_width].contains(&b'\t') {
        return Err(WorkflowPolicyError {
            line,
            message: "tabs are not allowed in indentation",
        });
    }
    Ok((true, &rest[marker_width..], indentation + 1 + marker_width))
}

fn strip_yaml_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (None, '\'' | '\"') => quote = Some(character),
            (Some(active), candidate) if active == candidate => quote = None,
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

fn yaml_mapping(line: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    for (index, character) in line.char_indices() {
        match (quote, character) {
            (None, '\'' | '\"') => quote = Some(character),
            (Some(active), candidate) if active == candidate => quote = None,
            (None, ':') => return Some((line[..index].trim(), line[index + 1..].trim())),
            _ => {}
        }
    }
    None
}

fn yaml_scalar(value: &str, line: usize) -> Result<String, WorkflowPolicyError> {
    unquote_yaml_scalar(value)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(WorkflowPolicyError {
            line,
            message: "uses value must be a non-empty scalar",
        })
}

fn unquote_yaml_scalar(value: &str) -> Option<&str> {
    let value = value.trim();
    match value.as_bytes() {
        [quote @ (b'\'' | b'\"'), middle @ .., end] if end == quote => {
            std::str::from_utf8(middle).ok()
        }
        [b'\'' | b'\"', ..] => None,
        _ => Some(value),
    }
}

fn is_block_scalar(value: &str) -> bool {
    matches!(value.as_bytes().first(), Some(b'|' | b'>'))
}

fn check_required_files(root: &Path) -> Result<(), PublicationError> {
    let canonical_root = canonical_root(root)?;
    for required in REQUIRED_PUBLICATION_FILES {
        let relative = PathBuf::from(required);
        let resolved = canonical_root.join(&relative);
        if !resolved.is_file() {
            return Err(PublicationError::MissingFile { path: relative });
        }
        let canonical = fs::canonicalize(&resolved).map_err(|error| PublicationError::Read {
            path: relative.clone(),
            message: error.to_string(),
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(PublicationError::FileEscapesRoot { path: relative });
        }
    }
    Ok(())
}

fn check_readme(root: &Path) -> Result<(), PublicationError> {
    let contents =
        fs::read_to_string(root.join("README.md")).map_err(|error| PublicationError::Read {
            path: PathBuf::from("README.md"),
            message: error.to_string(),
        })?;
    for text in REQUIRED_README_TEXT {
        if !contents.contains(text) {
            return Err(PublicationError::MissingReadmeText { text });
        }
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf, PublicationError> {
    fs::canonicalize(root).map_err(|error| PublicationError::Read {
        path: root.to_owned(),
        message: error.to_string(),
    })
}

fn check_tracked_tree(root: &Path) -> Result<(), PublicationError> {
    let output = git(root, ["ls-files", "-z"])?;
    for bytes in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let path = std::str::from_utf8(bytes).map_err(|_| PublicationError::NonUtf8TrackedPath)?;
        let relative = PathBuf::from(path);
        if relative.starts_with("docs/superpowers") || relative.starts_with(".superpowers") {
            return Err(PublicationError::ForbiddenPath { path: relative });
        }
        let resolved = root.join(&relative);
        let metadata = fs::symlink_metadata(&resolved).map_err(|error| PublicationError::Read {
            path: relative.clone(),
            message: error.to_string(),
        })?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let bytes = fs::read(&resolved).map_err(|error| PublicationError::Read {
            path: relative.clone(),
            message: error.to_string(),
        })?;
        let Ok(contents) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Some((line, kind)) = forbidden_content(contents) {
            return Err(PublicationError::ForbiddenContent {
                path: relative,
                line,
                kind,
            });
        }
    }
    Ok(())
}

fn check_head_identities(root: &Path) -> Result<(), PublicationError> {
    let command_args = ["log", "--format=%ae%x00%ce%x00", "HEAD"];
    let output = git(root, command_args)?;
    let contents = String::from_utf8(output.stdout).map_err(|_| {
        invalid_git_output(
            root,
            &command_args,
            "git log emitted non-UTF-8 identity data",
        )
    })?;
    if contents.is_empty() {
        return Err(invalid_git_output(
            root,
            &command_args,
            "git log returned an empty HEAD history",
        ));
    }
    for record in contents.lines() {
        let Some(record) = record.strip_suffix('\0') else {
            return Err(invalid_git_output(
                root,
                &command_args,
                "git log emitted a malformed identity record",
            ));
        };
        let identities = record.split('\0').collect::<Vec<_>>();
        if identities.len() != 2 {
            return Err(invalid_git_output(
                root,
                &command_args,
                "git log emitted a malformed identity record",
            ));
        }
        for (role, email) in [
            (IdentityRole::Author, identities[0]),
            (IdentityRole::Committer, identities[1]),
        ] {
            if !is_public_identity(email) {
                return Err(PublicationError::NonPublicIdentity {
                    role,
                    email: email.to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn git<const N: usize>(
    root: &Path,
    command_args: [&str; N],
) -> Result<std::process::Output, PublicationError> {
    let args = git_args(root, &command_args);
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(command_args)
        .output()
        .map_err(|error| PublicationError::Git {
            args: args.clone(),
            status: None,
            stderr: error.to_string(),
        })?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(PublicationError::Git {
            args,
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn invalid_git_output(root: &Path, command_args: &[&str], stderr: &str) -> PublicationError {
    PublicationError::Git {
        args: git_args(root, command_args),
        status: None,
        stderr: stderr.to_owned(),
    }
}

fn git_args(root: &Path, command_args: &[&str]) -> Vec<String> {
    std::iter::once("-C".to_owned())
        .chain(std::iter::once(root.display().to_string()))
        .chain(command_args.iter().copied().map(str::to_owned))
        .collect()
}

fn is_public_identity(email: &str) -> bool {
    email == "noreply@github.com" || email.ends_with("@users.noreply.github.com")
}

fn forbidden_needles() -> Vec<(String, ForbiddenKind)> {
    vec![
        (["/", "Users", "/"].concat(), ForbiddenKind::LocalPath),
        (["file", "://"].concat(), ForbiddenKind::LocalUrl),
        (["gmail", ".com"].concat(), ForbiddenKind::PersonalEmail),
        (
            ["BEGIN ", "PRIVATE KEY"].concat(),
            ForbiddenKind::PrivateKey,
        ),
        (["gh", "p_"].concat(), ForbiddenKind::GitHubToken),
        (["AK", "IA"].concat(), ForbiddenKind::AwsAccessKey),
    ]
}

fn forbidden_content(contents: &str) -> Option<(usize, ForbiddenKind)> {
    for (needle, kind) in forbidden_needles() {
        let offset = match kind {
            ForbiddenKind::GitHubToken => {
                token_offset(contents, &needle, 36, |byte| byte.is_ascii_alphanumeric())
            }
            ForbiddenKind::AwsAccessKey => token_offset(contents, &needle, 16, |byte| {
                byte.is_ascii_uppercase() || byte.is_ascii_digit()
            }),
            _ => contents.find(&needle),
        };
        if let Some(offset) = offset {
            return Some((line_at(contents, offset), kind));
        }
    }
    None
}

fn token_offset(
    contents: &str,
    prefix: &str,
    suffix_length: usize,
    valid: impl Fn(u8) -> bool,
) -> Option<usize> {
    let mut search_start = 0;
    while let Some(offset) = contents[search_start..].find(prefix) {
        let offset = search_start + offset;
        let suffix_start = offset + prefix.len();
        let suffix_end = suffix_start + suffix_length;
        if let Some(suffix) = contents.as_bytes().get(suffix_start..suffix_end)
            && suffix.iter().copied().all(&valid)
            && !contents
                .as_bytes()
                .get(suffix_end)
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
        {
            return Some(offset);
        }
        search_start = suffix_start;
    }
    None
}

fn line_at(contents: &str, offset: usize) -> usize {
    contents.as_bytes()[..offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn forbidden_kind_name(kind: ForbiddenKind) -> &'static str {
    match kind {
        ForbiddenKind::LocalPath => "local path",
        ForbiddenKind::LocalUrl => "local URL",
        ForbiddenKind::PersonalEmail => "personal email",
        ForbiddenKind::PrivateKey => "private key",
        ForbiddenKind::GitHubToken => "GitHub token",
        ForbiddenKind::AwsAccessKey => "AWS access key",
    }
}

fn identity_role_name(role: IdentityRole) -> &'static str {
    match role {
        IdentityRole::Author => "author",
        IdentityRole::Committer => "committer",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{self, Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_REPOSITORY_ID: AtomicU64 = AtomicU64::new(0);

    struct TestRepo {
        container: PathBuf,
        root: PathBuf,
    }

    impl TestRepo {
        fn public_fixture() -> Self {
            let id = NEXT_REPOSITORY_ID.fetch_add(1, Ordering::Relaxed);
            let container = std::env::temp_dir()
                .join(format!("minicontainer-publication-{}-{id}", process::id()));
            let root = container.join("repository");
            fs::create_dir_all(&root).expect("must create publication fixture repository");
            let repo = Self { container, root };
            repo.git(["init"]);
            for path in [
                "LICENSE-MIT",
                "LICENSE-APACHE",
                "SECURITY.md",
                "CONTRIBUTING.md",
                "README.md",
                "docs/design/architecture.md",
                "docs/guide/README.md",
                "docs/reference/threat-model.md",
            ] {
                repo.write(
                    path,
                    if path == "README.md" {
                        "MIT OR Apache-2.0\n本番用のセキュリティー境界ではありません\n"
                    } else {
                        "fixture\n"
                    },
                );
            }
            repo.write(
                ".github/dependabot.yml",
                "version: 2\nupdates:\n  - package-ecosystem: cargo\n    directory: \"/\"\n    schedule:\n      interval: weekly\n  - package-ecosystem: github-actions\n    directory: \"/\"\n    schedule:\n      interval: weekly\n",
            );
            repo.git(["add", "."]);
            repo
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("must create fixture parent");
            }
            fs::write(path, contents).expect("must write fixture file");
        }

        fn commit_as(&self, name: &str, email: &str) {
            let author = format!("{name} <{email}>");
            self.git([
                "-c",
                &format!("user.name={name}"),
                "-c",
                &format!("user.email={email}"),
                "commit",
                "--author",
                &author,
                "-m",
                "fixture",
            ]);
        }

        fn add_all(&self) {
            self.git(["add", "."]);
        }

        fn commit_noreply(&self) {
            self.commit_as("GitHub", "noreply@github.com");
        }

        fn commit_with_one_personal_identity(&self, role: IdentityRole) {
            let public_name = "GitHub";
            let public_email = "noreply@github.com";
            let personal_name = "Personal User";
            let personal_email = "person@example.com";
            let (author, committer_name, committer_email) = match role {
                IdentityRole::Author => (
                    format!("{personal_name} <{personal_email}>"),
                    public_name,
                    public_email,
                ),
                IdentityRole::Committer => (
                    format!("{public_name} <{public_email}>"),
                    personal_name,
                    personal_email,
                ),
            };
            self.git([
                "-c",
                &format!("user.name={committer_name}"),
                "-c",
                &format!("user.email={committer_email}"),
                "commit",
                "--author",
                &author,
                "-m",
                "fixture",
            ]);
        }

        fn git<const N: usize>(&self, args: [&str; N]) {
            let output = Command::new("git")
                .current_dir(&self.root)
                .args(args)
                .output()
                .expect("must run git for publication fixture");
            assert!(
                output.status.success(),
                "git fixture command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn git_with_input<const N: usize>(&self, args: [&str; N], input: &[u8]) -> Vec<u8> {
            let mut child = Command::new("git")
                .current_dir(&self.root)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("must run git for publication fixture");
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("fixture git must accept standard input")
                .write_all(input)
                .expect("must write git fixture input");
            let output = child
                .wait_with_output()
                .expect("must wait for git fixture command");
            assert!(
                output.status.success(),
                "git fixture command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.container);
        }
    }

    #[test]
    fn accepts_a_complete_public_repository_with_noreply_history() {
        let repo = TestRepo::public_fixture();
        repo.commit_as("Kunihiko Tanaka", "kunihiko-t@users.noreply.github.com");

        assert_eq!(check(repo.path()), Ok(()));
    }

    #[test]
    fn requires_the_public_architecture_document() {
        let repo = TestRepo::public_fixture();
        assert!(
            repo.path().join("docs/design/architecture.md").is_file(),
            "fixture must contain the public architecture document"
        );
        fs::remove_file(repo.path().join("docs/design/architecture.md"))
            .expect("must remove the public architecture document");

        assert_eq!(
            check(repo.path())
                .expect_err("missing public architecture document must fail")
                .to_string(),
            "docs/design/architecture.md: missing required publication file"
        );
    }

    #[test]
    fn requires_each_publication_file_with_a_precise_diagnostic() {
        for missing in [
            "LICENSE-MIT",
            "LICENSE-APACHE",
            "SECURITY.md",
            "CONTRIBUTING.md",
            "README.md",
            "docs/design/architecture.md",
            "docs/guide/README.md",
            "docs/reference/threat-model.md",
        ] {
            let repo = TestRepo::public_fixture();
            fs::remove_file(repo.path().join(missing)).expect("must remove required fixture");

            let error = check(repo.path()).expect_err("missing publication file must fail");
            assert_eq!(error.path(), Some(Path::new(missing)));
            assert_eq!(
                error.to_string(),
                format!("{missing}: missing required publication file")
            );
        }
    }

    #[test]
    fn requires_exact_readme_license_and_security_text() {
        for (contents, missing) in [
            (
                "本番用のセキュリティー境界ではありません\n",
                "MIT OR Apache-2.0",
            ),
            (
                "MIT OR Apache-2.0\n",
                "本番用のセキュリティー境界ではありません",
            ),
        ] {
            let repo = TestRepo::public_fixture();
            repo.write("README.md", contents);

            assert_eq!(
                check(repo.path())
                    .expect_err("missing README text must fail")
                    .to_string(),
                format!("README.md: missing required text: {missing}")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_required_publication_file_that_escapes_the_repository() {
        use std::os::unix::fs::symlink;

        let repo = TestRepo::public_fixture();
        fs::remove_file(repo.path().join("SECURITY.md")).expect("must remove fixture file");
        let outside = repo.container.join("outside-security.md");
        fs::write(&outside, "# External\n").expect("must write outside fixture");
        symlink(outside, repo.path().join("SECURITY.md")).expect("must create fixture symlink");

        assert_eq!(
            check(repo.path())
                .expect_err("publication file symlink must not escape")
                .to_string(),
            "SECURITY.md: required publication file escapes repository root"
        );
    }

    #[test]
    fn rejects_internal_agent_documents() {
        let repo = TestRepo::public_fixture();
        repo.write("docs/superpowers/plan.md", "internal\n");
        repo.add_all();
        repo.commit_noreply();

        assert!(matches!(
            check(repo.path()),
            Err(PublicationError::ForbiddenPath { path })
                if path == Path::new("docs/superpowers/plan.md")
        ));
    }

    #[test]
    fn rejects_tracked_docs_superpowers_with_all_public_documents_present() {
        let repo = TestRepo::public_fixture();
        assert!(repo.path().join("docs/design/architecture.md").is_file());
        assert!(repo.path().join("docs/guide/README.md").is_file());
        repo.write("docs/superpowers/plan.md", "internal\n");
        repo.add_all();
        repo.commit_noreply();

        assert!(matches!(
            check(repo.path()),
            Err(PublicationError::ForbiddenPath { path })
                if path == Path::new("docs/superpowers/plan.md")
        ));
    }

    #[test]
    fn rejects_root_internal_agent_documents() {
        let repo = TestRepo::public_fixture();
        repo.write(".superpowers/plan.md", "internal\n");
        repo.add_all();
        repo.commit_noreply();

        assert!(matches!(
            check(repo.path()),
            Err(PublicationError::ForbiddenPath { path })
                if path == Path::new(".superpowers/plan.md")
        ));
    }

    #[test]
    fn rejects_local_paths_without_embedding_the_pattern_in_source() {
        let repo = TestRepo::public_fixture();
        let local = ["/", "Users", "/private/project"].concat();
        repo.write("docs/reference/leak.md", &local);
        repo.add_all();
        repo.commit_noreply();

        let error = check(repo.path()).expect_err("local path must fail publication");
        assert_eq!(
            error.to_string(),
            "docs/reference/leak.md:1: forbidden local path"
        );
    }

    #[test]
    fn rejects_other_forbidden_text_patterns() {
        let fixtures = [
            (
                ["file", "://", "localhost/private"].concat(),
                ForbiddenKind::LocalUrl,
            ),
            (["gmail", ".com"].concat(), ForbiddenKind::PersonalEmail),
            (
                ["BEGIN ", "PRIVATE KEY"].concat(),
                ForbiddenKind::PrivateKey,
            ),
            (
                ["gh", "p_"].concat() + &"a".repeat(36),
                ForbiddenKind::GitHubToken,
            ),
            (
                ["AK", "IA"].concat() + &"A".repeat(16),
                ForbiddenKind::AwsAccessKey,
            ),
        ];
        for (contents, kind) in fixtures {
            let repo = TestRepo::public_fixture();
            repo.write("docs/reference/leak.md", &contents);
            repo.add_all();
            repo.commit_noreply();

            assert!(matches!(
                check(repo.path()),
                Err(PublicationError::ForbiddenContent { path, line: 1, kind: actual })
                    if path == Path::new("docs/reference/leak.md") && actual == kind
            ));
        }
    }

    #[test]
    fn does_not_reject_incomplete_token_prefixes() {
        let repo = TestRepo::public_fixture();
        let github_prefix = ["gh", "p_"].concat() + "short";
        let aws_prefix = ["AK", "IA"].concat() + "SHORT";
        repo.write(
            "docs/reference/token-prefixes.md",
            &format!("{github_prefix}\n{aws_prefix}\n"),
        );
        repo.add_all();
        repo.commit_noreply();

        assert_eq!(check(repo.path()), Ok(()));
    }

    #[test]
    fn skips_binary_files_and_untracked_files() {
        let binary = TestRepo::public_fixture();
        let local = ["/", "Users", "/private/project"].concat();
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(local.as_bytes());
        fs::write(binary.path().join("docs/reference/blob.bin"), bytes)
            .expect("must write binary fixture");
        binary.add_all();
        binary.commit_noreply();
        assert_eq!(check(binary.path()), Ok(()));

        let untracked = TestRepo::public_fixture();
        untracked.commit_noreply();
        untracked.write("docs/reference/untracked.md", &local);
        assert_eq!(check(untracked.path()), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_non_utf8_tracked_path() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let repo = TestRepo::public_fixture();
        let object = repo.git_with_input(["hash-object", "-w", "--stdin"], b"fixture\n");
        let object = String::from_utf8(object)
            .expect("fixture object ID must be UTF-8")
            .trim()
            .to_owned();
        let mut index_entry = format!("100644 {object}\t").into_bytes();
        index_entry.extend_from_slice(
            &OsString::from_vec(b"docs/reference/non-utf8-\xff.md".to_vec()).into_encoded_bytes(),
        );
        index_entry.push(0);
        repo.git_with_input(["update-index", "-z", "--index-info"], &index_entry);
        repo.commit_noreply();

        assert_eq!(
            check(repo.path()),
            Err(PublicationError::NonUtf8TrackedPath)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_tracked_required_file_symlink_that_escapes_the_repository() {
        use std::os::unix::fs::symlink;

        let repo = TestRepo::public_fixture();
        fs::remove_file(repo.path().join("SECURITY.md")).expect("must remove fixture file");
        let outside = repo.container.join("outside-security.md");
        fs::write(&outside, "# External\n").expect("must write outside fixture");
        symlink(outside, repo.path().join("SECURITY.md")).expect("must create fixture symlink");
        repo.add_all();
        repo.commit_noreply();

        assert!(matches!(
            check(repo.path()),
            Err(PublicationError::FileEscapesRoot { path }) if path == Path::new("SECURITY.md")
        ));
    }

    #[test]
    fn rejects_a_personal_author_or_committer_in_head_history() {
        for role in [IdentityRole::Author, IdentityRole::Committer] {
            let repo = TestRepo::public_fixture();
            repo.commit_with_one_personal_identity(role);

            assert!(matches!(
                check(repo.path()),
                Err(PublicationError::NonPublicIdentity { role: actual, .. })
                    if actual == role
            ));
        }
    }

    #[test]
    fn accepts_github_and_users_noreply_committers() {
        for email in ["kunihiko-t@users.noreply.github.com", "noreply@github.com"] {
            let repo = TestRepo::public_fixture();
            repo.commit_as("GitHub", email);
            assert_eq!(check(repo.path()), Ok(()));
        }
    }

    #[test]
    fn workflow_parser_returns_only_direct_step_actions() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let workflow = format!(
            "jobs:\n  check:\n    steps:\n      - uses: owner/action@{sha}\n      - name: nested\n        env:\n          uses: owner/not-a-step@v1\n        with:\n          uses: owner/not-a-step@v1\n        run: |\n          echo 'uses: owner/not-a-step@v1'\n"
        );

        let references = workflow_action_references(&workflow).unwrap();
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].value, format!("owner/action@{sha}"));
    }

    #[test]
    fn workflow_parser_accepts_direct_step_uses_forms() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let cases = [
            (
                "list item",
                4,
                format!("jobs:\n  check:\n    steps:\n      - uses: owner/action@{sha}\n"),
            ),
            (
                "following a step name",
                5,
                format!(
                    "jobs:\n  check:\n    steps:\n      - name: check\n        uses: owner/action@{sha}\n"
                ),
            ),
            (
                "quoted with a trailing comment",
                4,
                format!(
                    "jobs:\n  check:\n    steps:\n      - uses: \"owner/action@{sha}\" # stable\n"
                ),
            ),
        ];

        for (name, line, workflow) in cases {
            let references = workflow_action_references(&workflow)
                .unwrap_or_else(|error| panic!("{name} must parse: {error:?}"));
            assert_eq!(
                references,
                vec![ActionReference {
                    line,
                    value: format!("owner/action@{sha}"),
                }],
                "{name}"
            );
        }
    }

    #[test]
    fn workflow_parser_ignores_non_step_uses_text() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let workflow = format!(
            "uses: owner/not-a-step@{sha}\n# uses: owner/not-a-step@{sha}\njobs:\n  check:\n    steps:\n      - name: nested\n        env:\n          uses: owner/not-a-step@{sha}\n        with:\n          uses: owner/not-a-step@{sha}\n        run: |\n          echo 'uses: owner/not-a-step@{sha}'\n      - run: >\n          echo 'uses: owner/not-a-step@{sha}'\n"
        );

        assert_eq!(workflow_action_references(&workflow), Ok(Vec::new()));
    }

    #[test]
    fn workflow_parser_rejects_malformed_indentation() {
        let workflow =
            "jobs:\n  check:\n    steps:\n      - name: check\n\tuses: owner/action@v1\n";

        assert_eq!(
            workflow_action_references(workflow),
            Err(WorkflowPolicyError {
                line: 5,
                message: "tabs are not allowed in indentation",
            })
        );
    }

    #[test]
    fn workflow_parser_rejects_inconsistent_hierarchy_indentation() {
        let workflow = "jobs:\n  check:\n    runs-on: ubuntu-24.04\n     steps:\n      - uses: owner/action@v1\n";

        assert_eq!(
            workflow_action_references(workflow),
            Err(WorkflowPolicyError {
                line: 4,
                message: "unexpected indentation after scalar value",
            })
        );
    }

    #[test]
    fn workflow_parser_rejects_unaligned_step_mapping_continuations() {
        let workflow =
            "jobs:\n  check:\n    steps:\n      - name: build\n       uses: owner/action@v1\n";

        assert_eq!(
            workflow_action_references(workflow),
            Err(WorkflowPolicyError {
                line: 5,
                message: "inconsistent step mapping indentation",
            })
        );
    }

    #[test]
    fn workflow_parser_rejects_differently_indented_step_list_items() {
        let workflow =
            "jobs:\n  check:\n    steps:\n      - name: build\n     - uses: owner/action@v1\n";

        assert_eq!(
            workflow_action_references(workflow),
            Err(WorkflowPolicyError {
                line: 5,
                message: "inconsistent step list indentation",
            })
        );
    }

    #[test]
    fn workflow_parser_rejects_more_indented_sibling_step_list_items() {
        let workflow =
            "jobs:\n  check:\n    steps:\n      - name: build\n       - uses: owner/action@v1\n";

        assert_eq!(
            workflow_action_references(workflow),
            Err(WorkflowPolicyError {
                line: 5,
                message: "inconsistent step list indentation",
            })
        );
    }

    #[test]
    fn workflow_parser_ignores_nested_lists_owned_by_step_fields() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let workflow = format!(
            "jobs:\n  check:\n    steps:\n      - name: nested\n        env:\n          entries:\n            - uses: owner/not-a-step@{sha}\n            - uses: owner/still-not-a-step@{sha}\n"
        );

        assert_eq!(workflow_action_references(&workflow), Ok(Vec::new()));
    }

    #[test]
    fn workflow_parser_ignores_non_root_jobs_hierarchy() {
        let workflow =
            "workflow:\n  jobs:\n    check:\n      steps:\n        - uses: owner/action@v1\n";

        assert_eq!(workflow_action_references(workflow), Ok(Vec::new()));
    }

    #[test]
    fn workflow_parser_ignores_nested_input_named_jobs() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let workflow = format!(
            "on:\n  workflow_dispatch:\n    inputs:\n      jobs:\n        description: Select jobs\njobs:\n  check:\n    steps:\n      - uses: owner/action@{sha}\n"
        );

        assert_eq!(
            workflow_action_references(&workflow),
            Ok(vec![ActionReference {
                line: 9,
                value: format!("owner/action@{sha}"),
            }])
        );
    }

    #[test]
    fn workflow_parser_ignores_colons_in_non_step_block_scalars() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let workflow = format!(
            "on:\n  workflow_dispatch:\n    inputs:\n      task:\n        description: |\n          Select: all checks\njobs:\n  check:\n    steps:\n      - uses: owner/action@{sha}\n"
        );

        assert_eq!(
            workflow_action_references(&workflow),
            Ok(vec![ActionReference {
                line: 10,
                value: format!("owner/action@{sha}"),
            }])
        );
    }

    #[test]
    fn workflow_parser_skips_job_level_block_scalar_content() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let workflow = format!(
            "jobs:\n  check:\n    name: |\n      Check: Linux\n    steps:\n      - uses: owner/action@{sha}\n"
        );

        assert_eq!(
            workflow_action_references(&workflow),
            Ok(vec![ActionReference {
                line: 6,
                value: format!("owner/action@{sha}"),
            }])
        );
    }

    #[test]
    fn workflow_parser_rejects_mapping_below_nested_job_scalar() {
        let workflow = "jobs:\n  check:\n    strategy:\n      fail-fast: true\n       steps:\n        - uses: owner/action@v1\n";

        assert_eq!(
            workflow_action_references(workflow),
            Err(WorkflowPolicyError {
                line: 5,
                message: "unexpected indentation after scalar value",
            })
        );
    }

    #[test]
    fn publication_rejects_mutable_or_docker_step_actions() {
        let cases = [
            ("tag", "owner/action@v1"),
            (
                "short SHA",
                "owner/action@0123456789abcdef0123456789abcdef0123456",
            ),
            (
                "non-hex pin",
                "owner/action@0123456789abcdef0123456789abcdef0123456g",
            ),
            ("docker image", "docker://alpine:3.22"),
        ];
        for (name, reference) in cases {
            let repo = TestRepo::public_fixture();
            repo.write(
                ".github/workflows/ci.yml",
                &format!("jobs:\n  check:\n    steps:\n      - uses: {reference}\n"),
            );
            repo.add_all();
            repo.commit_noreply();

            assert!(
                matches!(
                    check(repo.path()),
                    Err(PublicationError::MutableAction { path, line: 4, reference: actual })
                        if path == Path::new(".github/workflows/ci.yml") && actual == reference
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn publication_rejects_mutable_action_at_the_actual_step_mapping_column() {
        let repo = TestRepo::public_fixture();
        repo.write(
            ".github/workflows/ci.yml",
            "jobs:\n  check:\n    steps:\n      -   name: build\n          uses: owner/action@v1\n",
        );
        repo.add_all();
        repo.commit_noreply();

        assert!(matches!(
            check(repo.path()),
            Err(PublicationError::MutableAction { path, line: 5, reference })
                if path == Path::new(".github/workflows/ci.yml") && reference == "owner/action@v1"
        ));
    }

    #[test]
    fn publication_accepts_full_sha_and_local_step_actions() {
        let repo = TestRepo::public_fixture();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        repo.write(
            ".github/workflows/ci.yml",
            &format!(
                "jobs:\n  check:\n    steps:\n      - uses: owner/action@{sha}\n      - uses: ./local-action\n"
            ),
        );
        repo.add_all();
        repo.commit_noreply();

        assert_eq!(check(repo.path()), Ok(()));
    }

    #[test]
    fn publication_converts_workflow_parser_errors_to_path_diagnostics() {
        let repo = TestRepo::public_fixture();
        repo.write(
            ".github/workflows/ci.yml",
            "jobs:\n  check:\n    steps:\n      - name: check\n\tuses: owner/action@v1\n",
        );
        repo.add_all();
        repo.commit_noreply();

        assert_eq!(
            check(repo.path()),
            Err(PublicationError::Workflow {
                path: PathBuf::from(".github/workflows/ci.yml"),
                line: 5,
                message: "tabs are not allowed in indentation",
            })
        );
    }

    #[test]
    fn publication_requires_active_dependabot_ecosystems() {
        let cases = [
            (
                "missing configuration",
                None,
                "missing required publication file".to_owned(),
            ),
            (
                "commented cargo",
                Some(
                    "version: 2\nupdates:\n  # - package-ecosystem: cargo\n  - package-ecosystem: github-actions\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
            (
                "commented GitHub Actions",
                Some(
                    "version: 2\nupdates:\n  - package-ecosystem: cargo\n    schedule:\n      interval: weekly\n  # - package-ecosystem: github-actions\n",
                ),
                "missing active Dependabot github-actions ecosystem".to_owned(),
            ),
            (
                "non-stanza cargo mapping",
                Some(
                    "version: 2\nupdates:\n  package-ecosystem: cargo\n  - package-ecosystem: github-actions\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
            (
                "cargo outside updates",
                Some(
                    "version: 2\n- package-ecosystem: cargo\nupdates:\n  - package-ecosystem: github-actions\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
            (
                "cargo without weekly schedule",
                Some(
                    "version: 2\nupdates:\n  - package-ecosystem: cargo\n    directory: \"/\"\n  - package-ecosystem: github-actions\n    directory: \"/\"\n    schedule:\n      interval: weekly\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
            (
                "weekly interval outside cargo schedule",
                Some(
                    "version: 2\nupdates:\n  - package-ecosystem: cargo\n    schedule:\n      interval: monthly\n    metadata:\n      interval: weekly\n  - package-ecosystem: github-actions\n    schedule:\n      interval: weekly\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
            (
                "weekly interval nested below schedule metadata",
                Some(
                    "version: 2\nupdates:\n  - package-ecosystem: cargo\n    schedule:\n      metadata:\n        interval: weekly\n  - package-ecosystem: github-actions\n    schedule:\n      interval: weekly\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
            (
                "weekly schedule nested below entry metadata",
                Some(
                    "version: 2\nupdates:\n  - package-ecosystem: cargo\n    metadata:\n      schedule:\n        interval: weekly\n  - package-ecosystem: github-actions\n    schedule:\n      interval: weekly\n",
                ),
                "missing active Dependabot cargo ecosystem".to_owned(),
            ),
        ];
        for (name, contents, diagnostic) in cases {
            let repo = TestRepo::public_fixture();
            let path = repo.path().join(".github/dependabot.yml");
            match contents {
                Some(contents) => repo.write(".github/dependabot.yml", contents),
                None => fs::remove_file(&path).expect("must remove Dependabot configuration"),
            }
            repo.add_all();
            repo.commit_noreply();

            let error = check(repo.path()).expect_err("Dependabot configuration must be active");
            assert_eq!(
                error.path(),
                Some(Path::new(".github/dependabot.yml")),
                "{name}"
            );
            assert!(error.to_string().contains(&diagnostic), "{name}: {error}");
        }
    }
}
