use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDirectory {
    pub name: String,
    pub path: PathBuf,
}

pub fn discover_skill_directories(root: &Path) -> io::Result<Vec<SkillDirectory>> {
    let mut skills = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let entry_path = entry.path();
        if !entry_path.is_dir() {
            continue;
        }

        let direct_skill = entry_path.join("SKILL.md");
        if direct_skill.is_file() {
            skills.push(SkillDirectory {
                name: entry.file_name().to_string_lossy().to_string(),
                path: direct_skill,
            });
            continue;
        }

        for child in fs::read_dir(&entry_path)? {
            let child = child?;
            let child_path = child.path();
            if !child_path.is_dir() {
                continue;
            }
            let nested_skill = child_path.join("SKILL.md");
            if nested_skill.is_file() {
                skills.push(SkillDirectory {
                    name: child.file_name().to_string_lossy().to_string(),
                    path: nested_skill,
                });
            }
        }
    }
    skills.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| {
                left.path
                    .components()
                    .count()
                    .cmp(&right.path.components().count())
            })
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(skills)
}

pub fn resolve_skill_path_from_roots(
    skill: &str,
    roots: &[PathBuf],
) -> io::Result<Option<PathBuf>> {
    let requested = skill.trim().trim_start_matches('/').trim_start_matches('$');
    if requested.is_empty() {
        return Ok(None);
    }

    for root in roots {
        let direct_skill = root.join(requested).join("SKILL.md");
        if direct_skill.is_file() {
            return Ok(Some(direct_skill));
        }

        for candidate in discover_skill_directories(root)? {
            if candidate.name.eq_ignore_ascii_case(requested) {
                return Ok(Some(candidate.path));
            }
        }
    }

    Ok(None)
}

pub fn parse_skill_frontmatter(contents: &str) -> (Option<String>, Option<String>) {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = unquote_frontmatter_value(value.trim());
            if !value.is_empty() {
                name = Some(value);
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            let value = unquote_frontmatter_value(value.trim());
            if !value.is_empty() {
                description = Some(value);
            }
        }
    }

    (name, description)
}

pub fn read_skill_package_metadata(root: &Path) -> io::Result<(String, Option<String>)> {
    let skill_path = root.join("SKILL.md");
    let contents = fs::read_to_string(&skill_path)?;
    let (name, description) = parse_skill_frontmatter(&contents);
    let skill_id = name
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            root.file_name()
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "skill package is missing an id")
        })?;
    Ok((validate_skill_id(&skill_id)?, description))
}

fn validate_skill_id(value: &str) -> io::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "skill package is missing an id",
        ));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill package id must not contain path separators: {trimmed}"),
        ));
    }
    let mut components = Path::new(trimmed).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(trimmed.to_string()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("skill package id must be a single path segment: {trimmed}"),
        )),
    }
}

fn unquote_frontmatter_value(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|trimmed| trimmed.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|trimmed| trimmed.strip_suffix('\''))
        })
        .unwrap_or(value)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        discover_skill_directories, parse_skill_frontmatter, read_skill_package_metadata,
        resolve_skill_path_from_roots,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-skills-{nanos}"))
    }

    #[test]
    fn discovers_nested_system_skill_directories() {
        let root = temp_dir();
        let nested = root.join(".system").join("openai-docs");
        fs::create_dir_all(&nested).expect("create nested skill");
        fs::write(nested.join("SKILL.md"), "# docs\n").expect("write skill");

        let skills = discover_skill_directories(&root).expect("skills should load");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "openai-docs");
        assert!(skills[0]
            .path
            .to_string_lossy()
            .replace('\\', "/")
            .ends_with("/.system/openai-docs/SKILL.md"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn resolves_nested_system_skill_from_roots() {
        let root = temp_dir();
        let nested = root.join(".system").join("openai-docs");
        fs::create_dir_all(&nested).expect("create nested skill");
        fs::write(nested.join("SKILL.md"), "# docs\n").expect("write skill");

        let resolved = resolve_skill_path_from_roots("openai-docs", std::slice::from_ref(&root))
            .expect("resolution should succeed")
            .expect("skill should exist");
        assert_eq!(resolved, nested.join("SKILL.md"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parse_skill_frontmatter_reads_name_and_description() {
        let (name, description) =
            parse_skill_frontmatter("---\nname: demo\ndescription: sample\n---\nbody\n");
        assert_eq!(name.as_deref(), Some("demo"));
        assert_eq!(description.as_deref(), Some("sample"));
    }

    #[test]
    fn read_skill_package_metadata_falls_back_to_directory_name() {
        let root = temp_dir();
        let nested = root.join("demo-skill");
        fs::create_dir_all(&nested).expect("create nested skill");
        fs::write(nested.join("SKILL.md"), "---\ndescription: docs\n---\n").expect("write skill");

        let (skill_id, description) =
            read_skill_package_metadata(&nested).expect("metadata should load");
        assert_eq!(skill_id, "demo-skill");
        assert_eq!(description.as_deref(), Some("docs"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_skill_package_metadata_rejects_path_like_frontmatter_name() {
        let root = temp_dir();
        let nested = root.join("demo-skill");
        fs::create_dir_all(&nested).expect("create nested skill");
        fs::write(
            nested.join("SKILL.md"),
            "---\nname: ../escape\ndescription: docs\n---\n",
        )
        .expect("write skill");

        let error = read_skill_package_metadata(&nested).expect_err("metadata should reject path");
        assert!(error
            .to_string()
            .contains("must not contain path separators"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn direct_skills_sort_ahead_of_nested_variants_with_same_name() {
        let root = temp_dir();
        let direct = root.join("demo");
        fs::create_dir_all(&direct).expect("create direct skill");
        fs::write(direct.join("SKILL.md"), "---\nname: demo\n---\n").expect("write direct skill");

        let nested = root.join(".managed").join("demo");
        fs::create_dir_all(&nested).expect("create nested skill");
        fs::write(nested.join("SKILL.md"), "---\nname: demo\n---\n").expect("write nested skill");

        let skills = discover_skill_directories(&root).expect("skills should load");
        assert_eq!(skills[0].path, direct.join("SKILL.md"));

        let _ = fs::remove_dir_all(root);
    }
}
