use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineState {
    #[default]
    Unreviewed,
    Accepted,
    Rejected,
}

#[derive(Clone, Debug)]
pub struct FileReview {
    pub lines: Vec<String>,
    pub states: Vec<LineState>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReviewStats {
    pub total: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub unreviewed: usize,
}

impl ReviewStats {
    pub fn reviewed(self) -> usize {
        self.accepted + self.rejected
    }

    pub fn reviewed_percent(self) -> u16 {
        self.reviewed()
            .saturating_mul(100)
            .checked_div(self.total)
            .unwrap_or(100) as u16
    }
}

#[derive(Debug)]
pub struct ReviewStore {
    root: PathBuf,
    debth_dir: PathBuf,
    files_dir: PathBuf,
    cache: HashMap<PathBuf, FileReview>,
    config: StoreConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreConfig {
    #[serde(default)]
    gitignore_prompted: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedFileReview {
    #[serde(default)]
    lines: Vec<String>,
    #[serde(default)]
    state_codes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    states: Vec<LineState>,
}

impl PersistedFileReview {
    fn states(&self) -> Vec<LineState> {
        if !self.state_codes.is_empty() {
            decode_state_codes(&self.state_codes)
        } else {
            self.states.clone()
        }
    }
}

impl ReviewStore {
    pub fn open(root: PathBuf) -> Result<Self> {
        let debth_dir = root.join(".debth");
        let files_dir = debth_dir.join("files");
        fs::create_dir_all(&files_dir)
            .with_context(|| format!("failed to create {}", files_dir.display()))?;

        let config = read_json(&debth_dir.join("config.json"))?.unwrap_or_default();
        Ok(Self {
            root,
            debth_dir,
            files_dir,
            cache: HashMap::new(),
            config,
        })
    }

    pub fn needs_gitignore_prompt(&self) -> bool {
        self.is_git_repo()
            && !self.config.gitignore_prompted
            && !gitignore_contains_debth(&self.root.join(".gitignore"))
    }

    pub fn answer_gitignore_prompt(&mut self, add: bool) -> Result<()> {
        if add {
            append_debth_to_gitignore(&self.root.join(".gitignore"))?;
        }
        self.config.gitignore_prompted = true;
        self.save_config()
    }

    pub fn load_file(&mut self, path: &Path) -> Result<&FileReview> {
        let relative = self.relative_path(path)?;
        let content = read_text_lossy(path)?;
        let current_lines = split_lines(&content);
        let persisted = self.load_persisted(&relative)?;
        let states = reconcile_states(&persisted.lines, &persisted.states(), &current_lines);
        let review = FileReview {
            lines: current_lines,
            states,
        };
        self.cache.insert(relative.clone(), review);
        self.save_relative(&relative)?;
        self.cache
            .get(&relative)
            .context("review disappeared from cache after insert")
    }

    pub fn set_line_state(
        &mut self,
        path: &Path,
        line_index: usize,
        state: LineState,
    ) -> Result<()> {
        let relative = self.relative_path(path)?;
        if !self.cache.contains_key(&relative) {
            self.load_file(path)?;
        }

        if let Some(review) = self.cache.get_mut(&relative)
            && let Some(line_state) = review.states.get_mut(line_index)
        {
            *line_state = state;
        }

        self.save_relative(&relative)
    }

    pub fn set_file_state(&mut self, path: &Path, state: LineState) -> Result<usize> {
        let relative = self.relative_path(path)?;
        if !self.cache.contains_key(&relative) {
            self.load_file(path)?;
        }

        let mut changed_lines = 0;
        if let Some(review) = self.cache.get_mut(&relative) {
            changed_lines = review.states.len();
            for line_state in &mut review.states {
                *line_state = state;
            }
        }

        self.save_relative(&relative)?;
        Ok(changed_lines)
    }

    pub fn file_stats(&mut self, path: &Path) -> Result<ReviewStats> {
        let review = self.load_file(path)?;
        Ok(stats_for_states(&review.states))
    }

    fn is_git_repo(&self) -> bool {
        self.root.join(".git").exists()
    }

    fn relative_path(&self, path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        absolute
            .strip_prefix(&self.root)
            .with_context(|| format!("{} is outside {}", path.display(), self.root.display()))
            .map(Path::to_path_buf)
    }

    fn load_persisted(&self, relative: &Path) -> Result<PersistedFileReview> {
        Ok(read_json(&self.review_path(relative))?.unwrap_or_default())
    }

    fn save_relative(&self, relative: &Path) -> Result<()> {
        if let Some(review) = self.cache.get(relative) {
            let persisted = PersistedFileReview {
                lines: review.lines.clone(),
                state_codes: encode_state_codes(&review.states),
                states: Vec::new(),
            };
            write_json(&self.review_path(relative), &persisted)?;
        }
        Ok(())
    }

    fn save_config(&self) -> Result<()> {
        write_json(&self.debth_dir.join("config.json"), &self.config)
    }

    fn review_path(&self, relative: &Path) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(relative.to_string_lossy().as_bytes());
        let digest = hex::encode(hasher.finalize());
        self.files_dir.join(format!("{digest}.json"))
    }
}

fn read_json<T>(path: &Path) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    if !path.exists() {
        return Ok(None);
    }

    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(value))
}

fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    let content = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{content}\n"))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn split_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }

    content
        .split_inclusive('\n')
        .map(|line| line.trim_end_matches('\n').to_string())
        .collect()
}

fn reconcile_states(
    previous_lines: &[String],
    previous_states: &[LineState],
    current_lines: &[String],
) -> Vec<LineState> {
    if previous_lines.is_empty() {
        return vec![LineState::Unreviewed; current_lines.len()];
    }

    let old = previous_lines.join("\n");
    let new = current_lines.join("\n");
    let diff = TextDiff::from_lines(&old, &new);
    let mut states = Vec::with_capacity(current_lines.len());
    let mut old_index = 0;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => old_index += 1,
            ChangeTag::Equal => {
                states.push(
                    previous_states
                        .get(old_index)
                        .copied()
                        .unwrap_or(LineState::Unreviewed),
                );
                old_index += 1;
            }
            ChangeTag::Insert => states.push(LineState::Unreviewed),
        }
    }

    if states.len() < current_lines.len() {
        states.resize(current_lines.len(), LineState::Unreviewed);
    }
    states.truncate(current_lines.len());
    states
}

fn stats_for_states(states: &[LineState]) -> ReviewStats {
    let mut stats = ReviewStats {
        total: states.len(),
        ..ReviewStats::default()
    };
    for state in states {
        match state {
            LineState::Unreviewed => stats.unreviewed += 1,
            LineState::Accepted => stats.accepted += 1,
            LineState::Rejected => stats.rejected += 1,
        }
    }
    stats
}

fn encode_state_codes(states: &[LineState]) -> String {
    states.iter().map(|state| state.code()).collect()
}

fn decode_state_codes(codes: &str) -> Vec<LineState> {
    codes.chars().map(LineState::from_code).collect()
}

impl LineState {
    fn code(self) -> char {
        match self {
            Self::Unreviewed => 'u',
            Self::Accepted => 'a',
            Self::Rejected => 'r',
        }
    }

    fn from_code(code: char) -> Self {
        match code {
            'a' | 'A' => Self::Accepted,
            'r' | 'R' => Self::Rejected,
            _ => Self::Unreviewed,
        }
    }
}

fn gitignore_contains_debth(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|content| {
        content.lines().any(|line| {
            let line = line.trim();
            !line.starts_with('#') && matches!(line, ".debth" | ".debth/" | "/.debth" | "/.debth/")
        })
    })
}

fn append_debth_to_gitignore(path: &Path) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut next = existing.clone();
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(".debth/\n");
    fs::write(path, next).with_context(|| format!("failed to write {}", path.display()))
}

fn read_text_lossy(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn inserted_lines_start_unreviewed_and_existing_lines_keep_state() {
        let previous = vec!["fn main() {".into(), "}".into()];
        let previous_states = vec![LineState::Accepted, LineState::Rejected];
        let current = vec![
            "fn main() {".into(),
            "    println!(\"hi\");".into(),
            "}".into(),
        ];

        let states = reconcile_states(&previous, &previous_states, &current);

        assert_eq!(
            states,
            vec![
                LineState::Accepted,
                LineState::Unreviewed,
                LineState::Rejected
            ]
        );
    }

    #[test]
    fn changed_lines_become_unreviewed() {
        let previous = vec!["let x = 1;".into()];
        let previous_states = vec![LineState::Accepted];
        let current = vec!["let x = 2;".into()];

        assert_eq!(
            reconcile_states(&previous, &previous_states, &current),
            vec![LineState::Unreviewed]
        );
    }

    #[test]
    fn state_codes_round_trip_compactly() {
        let states = vec![
            LineState::Unreviewed,
            LineState::Accepted,
            LineState::Rejected,
        ];

        assert_eq!(encode_state_codes(&states), "uar");
        assert_eq!(decode_state_codes("uar"), states);
    }

    #[test]
    fn persisted_review_supports_legacy_state_array() {
        let persisted = PersistedFileReview {
            lines: Vec::new(),
            state_codes: String::new(),
            states: vec![LineState::Accepted, LineState::Rejected],
        };

        assert_eq!(
            persisted.states(),
            vec![LineState::Accepted, LineState::Rejected]
        );
    }

    #[test]
    fn persisted_review_prefers_compact_state_codes() {
        let persisted = PersistedFileReview {
            lines: Vec::new(),
            state_codes: "ra".to_string(),
            states: vec![LineState::Accepted, LineState::Accepted],
        };

        assert_eq!(
            persisted.states(),
            vec![LineState::Rejected, LineState::Accepted]
        );
    }

    #[test]
    fn set_file_state_marks_every_line() -> Result<()> {
        let root = unique_temp_dir();
        fs::create_dir_all(&root)?;
        let path = root.join("main.rs");
        fs::write(&path, "fn main() {\n}\n")?;

        let mut store = ReviewStore::open(root.clone())?;
        let changed = store.set_file_state(&path, LineState::Accepted)?;
        let review = store.load_file(&path)?;

        assert_eq!(changed, 2);
        assert_eq!(
            review.states,
            vec![LineState::Accepted, LineState::Accepted]
        );

        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn unique_temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("debth-review-test-{}-{nanos}", std::process::id()))
    }
}
