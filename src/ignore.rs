use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

const IGNORE_HEADER: &str = "# debth ignore file
# Edit this file to unignore paths or add more patterns.
# Blank lines and # comments are ignored. Use !pattern to unignore.
";

#[derive(Clone, Debug, Default)]
pub struct IgnoreRules {
    root: PathBuf,
    patterns: Vec<IgnorePattern>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IgnorePattern {
    pattern: String,
    negated: bool,
    anchored: bool,
    dir_only: bool,
    has_slash: bool,
}

impl IgnoreRules {
    pub fn load(root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        let ignore_path = ignore_path(&root);
        let content = fs::read_to_string(&ignore_path).unwrap_or_default();
        let patterns = content.lines().filter_map(IgnorePattern::parse).collect();
        Ok(Self { root, patterns })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        let Some(relative) = normalize_relative(relative) else {
            return false;
        };

        let mut ignored = false;
        for pattern in &self.patterns {
            if pattern.matches(&relative, is_dir) {
                ignored = !pattern.negated;
            }
        }
        ignored
    }

    pub fn add_path(&mut self, path: &Path, is_dir: bool) -> Result<String> {
        let pattern = ignore_pattern_for_path(&self.root, path, is_dir)?;
        let ignore_path = ignore_path(&self.root);
        if let Some(parent) = ignore_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let existing = fs::read_to_string(&ignore_path).unwrap_or_default();
        let already_present = existing
            .lines()
            .map(str::trim)
            .any(|line| line == pattern.as_str());

        if !already_present {
            let mut next = String::new();
            if existing.trim().is_empty() {
                next.push_str(IGNORE_HEADER);
            } else {
                next.push_str(&existing);
                if !next.ends_with('\n') {
                    next.push('\n');
                }
            }
            next.push_str(&pattern);
            next.push('\n');
            fs::write(&ignore_path, next)
                .with_context(|| format!("failed to write {}", ignore_path.display()))?;
        }

        *self = Self::load(&self.root)?;
        Ok(pattern)
    }
}

impl IgnorePattern {
    fn parse(line: &str) -> Option<Self> {
        let mut line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let negated = line.starts_with('!');
        if negated {
            line = line[1..].trim_start();
        }

        let anchored = line.starts_with('/');
        if anchored {
            line = &line[1..];
        }

        let dir_only = line.ends_with('/');
        if dir_only {
            line = line.trim_end_matches('/');
        }

        let pattern = line.trim();
        if pattern.is_empty() {
            return None;
        }

        Some(Self {
            pattern: pattern.replace('\\', "/"),
            negated,
            anchored,
            dir_only,
            has_slash: pattern.contains('/'),
        })
    }

    fn matches(&self, relative: &str, is_dir: bool) -> bool {
        if self.dir_only {
            return self.matches_directory_pattern(relative, is_dir);
        }

        if self.anchored {
            return wildcard_match(&self.pattern, relative);
        }

        if self.has_slash {
            return suffixes(relative).any(|suffix| wildcard_match(&self.pattern, &suffix));
        }

        relative
            .split('/')
            .any(|component| wildcard_match(&self.pattern, component))
    }

    fn matches_directory_pattern(&self, relative: &str, is_dir: bool) -> bool {
        let directory_candidates = directory_candidates(relative, is_dir);
        if self.anchored {
            return directory_candidates
                .into_iter()
                .any(|candidate| wildcard_match(&self.pattern, &candidate));
        }

        if self.has_slash {
            return directory_candidates.into_iter().any(|candidate| {
                suffixes(&candidate).any(|suffix| wildcard_match(&self.pattern, &suffix))
            });
        }

        let directory_components = if is_dir {
            relative.split('/').collect::<Vec<_>>()
        } else {
            relative
                .rsplit_once('/')
                .map(|(parent, _)| parent.split('/').collect())
                .unwrap_or_default()
        };
        directory_components
            .into_iter()
            .any(|component| wildcard_match(&self.pattern, component))
    }
}

pub fn ignore_path(root: &Path) -> PathBuf {
    root.join(".debth").join("ignore")
}

fn ignore_pattern_for_path(root: &Path, path: &Path, is_dir: bool) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
    let relative =
        normalize_relative(relative).context("cannot ignore the workspace root itself")?;
    let mut pattern = format!("/{relative}");
    if is_dir {
        pattern.push('/');
    }
    Ok(pattern)
}

fn normalize_relative(path: &Path) -> Option<String> {
    let normalized = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    (!normalized.is_empty()).then_some(normalized)
}

fn directory_candidates(relative: &str, is_dir: bool) -> Vec<String> {
    let components = relative.split('/').collect::<Vec<_>>();
    let last_directory = if is_dir {
        components.len()
    } else {
        components.len().saturating_sub(1)
    };

    (1..=last_directory)
        .map(|count| components[..count].join("/"))
        .collect()
}

fn suffixes(path: &str) -> impl Iterator<Item = String> + '_ {
    path.split('/')
        .collect::<Vec<_>>()
        .into_iter()
        .enumerate()
        .map(|(index, _)| path.split('/').skip(index).collect::<Vec<_>>().join("/"))
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern = pattern.as_bytes();
    let text = text.as_bytes();
    let (mut pattern_index, mut text_index) = (0, 0);
    let mut star_index = None;
    let mut star_text_index = 0;

    while text_index < text.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == text[text_index])
        {
            pattern_index += 1;
            text_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            star_text_index = text_index;
            pattern_index += 1;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_text_index += 1;
            text_index = star_text_index;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rooted_file_and_directory_patterns() {
        let file = IgnorePattern::parse("/src/main.rs").unwrap();
        assert!(file.matches("src/main.rs", false));
        assert!(!file.matches("other/src/main.rs", false));

        let directory = IgnorePattern::parse("/src/").unwrap();
        assert!(directory.matches("src", true));
        assert!(directory.matches("src/main.rs", false));
        assert!(!directory.matches("other/src/main.rs", false));
    }

    #[test]
    fn parses_gitignore_style_wildcards_and_negations() {
        let wildcard = IgnorePattern::parse("*.log").unwrap();
        assert!(wildcard.matches("debug/app.log", false));
        assert!(!wildcard.matches("debug/app.txt", false));

        let negated = IgnorePattern::parse("!keep.log").unwrap();
        assert!(negated.negated);
        assert!(negated.matches("logs/keep.log", false));
    }

    #[test]
    fn generated_patterns_are_readable_and_rooted() -> Result<()> {
        let root = PathBuf::from("/tmp/project");
        let file = ignore_pattern_for_path(&root, &root.join("src/main.rs"), false)?;
        let directory = ignore_pattern_for_path(&root, &root.join("src"), true)?;

        assert_eq!(file, "/src/main.rs");
        assert_eq!(directory, "/src/");
        Ok(())
    }
}
