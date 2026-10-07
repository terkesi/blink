use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions, ReadDir},
};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_file_bytes: usize,
    pub max_total_bytes: usize,
    pub max_entries: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            max_total_bytes: 64 * 1024 * 1024,
            max_entries: 100_000,
        }
    }
}
impl Limits {
    fn bounded(self) -> Self {
        let ceiling = Self::default();
        Self {
            max_file_bytes: self.max_file_bytes.min(ceiling.max_file_bytes),
            max_total_bytes: self.max_total_bytes.min(ceiling.max_total_bytes),
            max_entries: self.max_entries.min(ceiling.max_entries),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    Cancel,
    Deadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EntryLimit,
    ByteLimit,
    Cancelled,
    Deadline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    Sensitive,
    Metadata,
    Dependency,
    Build,
    Hidden,
    Gitignore,
    Blinkignore,
    Symlink,
    Special,
    NonUtf8Path,
    NonUtf8Content,
    Binary,
    TooLarge,
}

#[derive(Debug, Serialize)]
pub struct Issue {
    pub path: String,
    pub operation: &'static str,
    pub kind: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub complete: bool,
    pub visited_entries: usize,
    pub files_included: usize,
    pub bytes_included: usize,
    pub bytes_read: usize,
    pub excluded: BTreeMap<Exclusion, usize>,
    pub issues: Vec<Issue>,
    pub stops: Vec<StopReason>,
}

#[derive(Debug)]
pub struct SourceFile {
    path: String,
    bytes: Box<[u8]>,
    sha256: String,
    line_offsets: Box<[usize]>,
}
impl SourceFile {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes).expect("source text was validated")
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    /// Byte offsets of line starts, including an empty last line after a trailing newline.
    pub fn line_offsets(&self) -> &[usize] {
        &self.line_offsets
    }
}

pub struct Source {
    root: Arc<Dir>,
    root_path: PathBuf,
    ancestors: Vec<PinnedDirectory>,
}

struct PinnedDirectory {
    path: PathBuf,
    dir: Arc<Dir>,
}

pub struct Snapshot {
    root: Arc<Dir>,
    limits: Limits,
    files: Vec<SourceFile>,
    coverage: Coverage,
}
impl Snapshot {
    pub fn into_coverage(self) -> Coverage {
        self.coverage
    }
    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }
    pub fn coverage(&self) -> &Coverage {
        &self.coverage
    }
    /// Reopen through the original root, charging reads to this snapshot's run budget.
    /// A false result means changed content. Errors and stops update coverage.
    pub fn recheck(
        &mut self,
        index: usize,
        control: &mut dyn FnMut() -> Control,
    ) -> io::Result<bool> {
        let file = self.files.get(index).ok_or_else(invalid_path)?;
        if !checkpoint(&mut self.coverage, control, true) {
            self.coverage.complete = false;
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "source recheck stopped",
            ));
        }
        let path = Path::new(file.path());
        let result = (|| {
            let parent = open_relative_dir(&self.root, path.parent().unwrap_or(Path::new("")))?;
            read_bounded(
                &parent,
                Path::new(path.file_name().ok_or_else(invalid_path)?),
                self.limits,
                &mut self.coverage,
                control,
            )
        })();
        let result = match result {
            Ok(bytes) => Ok(bytes.len() == file.bytes.len() && digest(&bytes) == file.sha256),
            Err(ReadFailure::TooLarge | ReadFailure::Special) => Ok(false),
            Err(ReadFailure::Stopped) => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "source recheck stopped",
            )),
            Err(ReadFailure::Io(error)) => {
                self.coverage.issues.push(Issue {
                    path: file.path().into(),
                    operation: "recheck",
                    kind: format!("{:?}", error.kind()),
                });
                Err(error)
            }
        };
        self.coverage.complete = self.coverage.stops.is_empty() && self.coverage.issues.is_empty();
        result
    }
}

impl Source {
    /// Resolve the selected root once, then pin it with no-follow component opens.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root_path = root.as_ref().canonicalize()?;
        let (root, ancestors) = open_ancestry(&root_path)?;
        Ok(Self {
            root,
            root_path,
            ancestors,
        })
    }
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }
    pub fn snapshot(&self, limits: Limits, control: &mut dyn FnMut() -> Control) -> Snapshot {
        let mut scan = Scan {
            source: self,
            limits: limits.bounded(),
            control,
            coverage: Coverage::default(),
            files: Vec::new(),
        };
        scan.walk();
        scan.files.sort_by(|a, b| a.path.cmp(&b.path));
        scan.coverage.files_included = scan.files.len();
        scan.coverage.bytes_included = scan.files.iter().map(|file| file.bytes.len()).sum();
        scan.coverage.complete = scan.coverage.stops.is_empty() && scan.coverage.issues.is_empty();
        Snapshot {
            root: Arc::clone(&self.root),
            limits: limits.bounded(),
            files: scan.files,
            coverage: scan.coverage,
        }
    }
}

struct Frame {
    path: PathBuf,
    entries: ReadDir,
    rule_count: usize,
}
struct Scan<'a> {
    source: &'a Source,
    limits: Limits,
    control: &'a mut dyn FnMut() -> Control,
    coverage: Coverage,
    files: Vec<SourceFile>,
}

enum ReadFailure {
    Io(io::Error),
    TooLarge,
    Special,
    Stopped,
}
impl From<io::Error> for ReadFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl Scan<'_> {
    fn checkpoint(&mut self) -> bool {
        checkpoint(&mut self.coverage, self.control, false)
    }
    fn exclude(&mut self, reason: Exclusion) {
        *self.coverage.excluded.entry(reason).or_default() += 1;
    }
    fn issue(&mut self, path: &Path, operation: &'static str, kind: impl ToString) {
        let path = if path.is_absolute() {
            match path.strip_prefix(&self.source.root_path) {
                Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
                Ok(relative) => relative.to_string_lossy().into_owned(),
                Err(_) => "<ancestor>".into(),
            }
        } else {
            path.to_string_lossy().into_owned()
        };
        self.coverage.issues.push(Issue {
            path,
            operation,
            kind: kind.to_string(),
        });
    }
    fn rules(&mut self, dir: &Dir, directory: &Path, name: &str) -> Result<Option<Gitignore>, ()> {
        let path = directory.join(name);
        match dir.symlink_metadata(name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                self.issue(&path, "ignore_metadata", format!("{:?}", error.kind()));
                return Err(());
            }
            Ok(metadata) if !metadata.is_file() => {
                self.issue(&path, "ignore_read", "not_regular_file");
                return Err(());
            }
            Ok(_) => {}
        }
        let bytes = match read_bounded(
            dir,
            Path::new(name),
            self.limits,
            &mut self.coverage,
            self.control,
        ) {
            Ok(bytes) => bytes,
            Err(ReadFailure::Stopped) => return Err(()),
            Err(failure) => {
                self.read_issue(&path, "ignore_read", failure);
                return Err(());
            }
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            self.issue(&path, "ignore_parse", "invalid_utf8");
            return Err(());
        };
        let mut builder = GitignoreBuilder::new(directory);
        for line in text.lines() {
            if builder.add_line(None, line).is_err() {
                self.issue(&path, "ignore_parse", "invalid_pattern");
                return Err(());
            }
        }
        match builder.build() {
            Ok(rules) => Ok(Some(rules)),
            Err(_) => {
                self.issue(&path, "ignore_parse", "invalid_pattern");
                Err(())
            }
        }
    }
    fn read_issue(&mut self, path: &Path, operation: &'static str, failure: ReadFailure) {
        let kind = match failure {
            ReadFailure::Io(error) => format!("{:?}", error.kind()),
            ReadFailure::TooLarge => "file_limit".into(),
            ReadFailure::Special => "not_regular_file".into(),
            ReadFailure::Stopped => return,
        };
        self.issue(path, operation, kind);
    }
    fn ancestor_rules(&mut self) -> Result<Vec<Gitignore>, ()> {
        let mut repository_index = None;
        for (index, ancestor) in self.source.ancestors.iter().enumerate().rev() {
            if !self.checkpoint() {
                return Err(());
            }
            let repository_root = match ancestor.dir.symlink_metadata(".git") {
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => {
                    self.issue(
                        &ancestor.path,
                        "repository_metadata",
                        format!("{:?}", error.kind()),
                    );
                    return Err(());
                }
            };
            if repository_root {
                repository_index = Some(index);
                break;
            }
        }
        let Some(repository_index) = repository_index else {
            return Ok(Vec::new());
        };
        let mut rules = Vec::new();
        for ancestor in &self.source.ancestors[repository_index..self.source.ancestors.len() - 1] {
            if git_ignored(&rules, &ancestor.path, true) {
                self.exclude(Exclusion::Gitignore);
                return Err(());
            }
            if let Some(rule) = self.rules(&ancestor.dir, &ancestor.path, ".gitignore")? {
                rules.push(rule);
            }
        }
        if git_ignored(&rules, &self.source.root_path, true) {
            self.exclude(Exclusion::Gitignore);
            return Err(());
        }
        Ok(rules)
    }
    fn frame(&mut self, path: PathBuf, rules: &mut Vec<Gitignore>) -> Option<Frame> {
        let dir = match open_relative_dir(&self.source.root, &path) {
            Ok(dir) => dir,
            Err(error) => {
                self.issue(&path, "directory_open", format!("{:?}", error.kind()));
                return None;
            }
        };
        let rule_count = rules.len();
        match self.rules(&dir, &self.source.root_path.join(&path), ".gitignore") {
            Ok(Some(rule)) => rules.push(rule),
            Ok(None) => {}
            Err(()) => return None,
        }
        match dir.entries() {
            Ok(entries) => Some(Frame {
                path,
                entries,
                rule_count,
            }),
            Err(error) => {
                rules.truncate(rule_count);
                self.issue(&path, "directory_read", format!("{:?}", error.kind()));
                None
            }
        }
    }
    fn walk(&mut self) {
        for component in self.source.root_path.components() {
            if let Component::Normal(name) = component
                && let Some(reason) = name.to_str().and_then(hard_exclusion)
            {
                self.exclude(reason);
                return;
            }
        }
        let Ok(mut rules) = self.ancestor_rules() else {
            return;
        };
        let root = Arc::clone(&self.source.root);
        let Ok(blink) = self.rules(&root, &self.source.root_path, ".blinkignore") else {
            return;
        };
        let Some(first) = self.frame(PathBuf::new(), &mut rules) else {
            return;
        };
        let mut stack = vec![first];
        while self.checkpoint() {
            let Some(frame) = stack.last_mut() else {
                break;
            };
            if self.coverage.visited_entries == self.limits.max_entries {
                self.coverage.stops.push(StopReason::EntryLimit);
                break;
            }
            let Some(entry) = frame.entries.next() else {
                let count = frame.rule_count;
                stack.pop();
                rules.truncate(count);
                continue;
            };
            self.coverage.visited_entries += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    let path = frame.path.clone();
                    self.issue(&path, "entry_read", format!("{:?}", error.kind()));
                    continue;
                }
            };
            let path = frame.path.join(entry.file_name());
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                self.exclude(Exclusion::NonUtf8Path);
                continue;
            };
            if let Some(reason) = hard_exclusion(&name) {
                self.exclude(reason);
                continue;
            }
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(error) => {
                    self.issue(&path, "entry_type", format!("{:?}", error.kind()));
                    continue;
                }
            };
            if kind.is_symlink() {
                self.exclude(Exclusion::Symlink);
                continue;
            }
            if !kind.is_dir() && !kind.is_file() {
                self.exclude(Exclusion::Special);
                continue;
            }
            if name.starts_with('.') {
                self.exclude(Exclusion::Hidden);
                continue;
            }
            let absolute = self.source.root_path.join(&path);
            if blink
                .as_ref()
                .is_some_and(|rule| rule.matched(&absolute, kind.is_dir()).is_ignore())
            {
                self.exclude(Exclusion::Blinkignore);
                continue;
            }
            if git_ignored(&rules, &absolute, kind.is_dir()) {
                self.exclude(Exclusion::Gitignore);
                continue;
            }
            if kind.is_dir() {
                if let Some(child) = self.frame(path, &mut rules) {
                    stack.push(child);
                }
                continue;
            }
            let parent = match open_relative_dir(
                &self.source.root,
                path.parent().unwrap_or(Path::new("")),
            ) {
                Ok(parent) => parent,
                Err(error) => {
                    self.issue(&path, "file_parent_open", format!("{:?}", error.kind()));
                    continue;
                }
            };
            let bytes = match read_bounded(
                &parent,
                Path::new(&name),
                self.limits,
                &mut self.coverage,
                self.control,
            ) {
                Ok(bytes) => bytes,
                Err(ReadFailure::TooLarge) => {
                    self.exclude(Exclusion::TooLarge);
                    continue;
                }
                Err(ReadFailure::Special) => {
                    self.exclude(Exclusion::Special);
                    continue;
                }
                Err(failure) => {
                    self.read_issue(&path, "file_read", failure);
                    continue;
                }
            };
            if std::str::from_utf8(&bytes).is_err() {
                self.exclude(Exclusion::NonUtf8Content);
                continue;
            }
            if bytes.iter().any(|byte| {
                *byte == 0 || (*byte < 0x20 && !matches!(*byte, b'\n' | b'\r' | b'\t' | 0x0c))
            }) {
                self.exclude(Exclusion::Binary);
                continue;
            }
            let mut line_offsets = vec![0];
            line_offsets.extend(
                bytes
                    .iter()
                    .enumerate()
                    .filter(|(_, byte)| **byte == b'\n')
                    .map(|(offset, _)| offset + 1),
            );
            self.files.push(SourceFile {
                path: path
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/"),
                sha256: digest(&bytes),
                bytes: bytes.into_boxed_slice(),
                line_offsets: line_offsets.into_boxed_slice(),
            });
        }
    }
}

fn git_ignored(rules: &[Gitignore], path: &Path, directory: bool) -> bool {
    rules
        .iter()
        .rev()
        .find_map(|rule| {
            let matched = rule.matched(path, directory);
            (!matched.is_none()).then(|| matched.is_ignore())
        })
        .unwrap_or(false)
}
fn hard_exclusion(name: &str) -> Option<Exclusion> {
    let name = name.to_ascii_lowercase();
    if matches!(name.as_str(), ".git" | ".hg" | ".svn") {
        return Some(Exclusion::Metadata);
    }
    if matches!(name.as_str(), "node_modules" | "vendor") {
        return Some(Exclusion::Dependency);
    }
    if matches!(name.as_str(), "target" | "dist" | "build") {
        return Some(Exclusion::Build);
    }
    let extension = name.rsplit('.').next().unwrap_or("");
    if name.starts_with(".env")
        || matches!(
            name.as_str(),
            ".ssh"
                | ".aws"
                | ".azure"
                | ".gcp"
                | ".gnupg"
                | "gcloud"
                | "credentials"
                | ".credentials"
                | "secrets"
                | ".secrets"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".git-credentials"
                | ".dockercfg"
                | ".kube"
                | ".docker"
                | ".vault"
                | ".password-store"
                | "private_keys"
                | "private-keys"
        )
        || [
            "id_rsa",
            "id_dsa",
            "id_ecdsa",
            "id_ed25519",
            "credentials.",
            "service-account",
            "service_account",
            "client_secret",
            "private_key",
            "private-key",
            "privatekey",
            "ssh_host_",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || matches!(
            extension,
            "pem"
                | "key"
                | "p12"
                | "pfx"
                | "pkcs12"
                | "keystore"
                | "jks"
                | "ppk"
                | "pgp"
                | "gpg"
                | "asc"
        )
    {
        return Some(Exclusion::Sensitive);
    }
    None
}
fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected a normal relative path",
    )
}
fn open_relative_dir(root: &Dir, path: &Path) -> io::Result<Dir> {
    let mut dir = root.try_clone()?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(invalid_path());
        };
        dir = dir.open_dir_nofollow(name)?;
    }
    Ok(dir)
}
fn open_ancestry(path: &Path) -> io::Result<(Arc<Dir>, Vec<PinnedDirectory>)> {
    let mut components = path.components();
    let Some(Component::RootDir) = components.next() else {
        return Err(invalid_path());
    };
    let mut dir = Arc::new(Dir::open_ambient_dir(Path::new("/"), ambient_authority())?);
    let mut current_path = PathBuf::from("/");
    let mut ancestors = vec![PinnedDirectory {
        path: current_path.clone(),
        dir: Arc::clone(&dir),
    }];
    for component in components {
        let Component::Normal(name) = component else {
            return Err(invalid_path());
        };
        dir = Arc::new(dir.open_dir_nofollow(name)?);
        current_path.push(name);
        ancestors.push(PinnedDirectory {
            path: current_path.clone(),
            dir: Arc::clone(&dir),
        });
    }
    Ok((dir, ancestors))
}
fn open_file(dir: &Dir, path: &Path) -> io::Result<cap_std::fs::File> {
    if path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(invalid_path());
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    dir.open_with(path, &options)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_bounded(
    dir: &Dir,
    name: &Path,
    limits: Limits,
    coverage: &mut Coverage,
    control: &mut dyn FnMut() -> Control,
) -> Result<Vec<u8>, ReadFailure> {
    if !checkpoint(coverage, control, true) {
        return Err(ReadFailure::Stopped);
    }
    let mut file = open_file(dir, name)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(ReadFailure::Special);
    }
    if metadata.len() > limits.max_file_bytes as u64 {
        return Err(ReadFailure::TooLarge);
    }
    let allowance = limits.max_total_bytes.saturating_sub(coverage.bytes_read);
    if metadata.len() > allowance as u64 {
        coverage.stops.push(StopReason::ByteLimit);
        return Err(ReadFailure::Stopped);
    }
    let cap = limits.max_file_bytes.min(allowance);
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    while bytes.len() < cap {
        if !checkpoint(coverage, control, true) {
            return Err(ReadFailure::Stopped);
        }
        let request = chunk.len().min(cap - bytes.len());
        let count = file.read(&mut chunk[..request])?;
        coverage.bytes_read += count;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let after = file.metadata()?;
    if after.len() > limits.max_file_bytes as u64 {
        return Err(ReadFailure::TooLarge);
    }
    if after.len() > bytes.len() as u64 {
        coverage.stops.push(StopReason::ByteLimit);
        return Err(ReadFailure::Stopped);
    }
    Ok(bytes)
}

fn checkpoint(
    coverage: &mut Coverage,
    control: &mut dyn FnMut() -> Control,
    allow_entry_limit: bool,
) -> bool {
    if coverage
        .stops
        .iter()
        .any(|reason| !allow_entry_limit || *reason != StopReason::EntryLimit)
    {
        return false;
    }
    let reason = match control() {
        Control::Continue => return true,
        Control::Cancel => StopReason::Cancelled,
        Control::Deadline => StopReason::Deadline,
    };
    coverage.stops.push(reason);
    false
}
