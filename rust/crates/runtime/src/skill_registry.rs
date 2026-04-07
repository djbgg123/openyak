use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::read_skill_package_metadata;

const SKILL_DIRECTORY_LAYOUT_V1: &str = "skill_directory_v1";
const MANAGED_SKILLS_DIR_NAME: &str = ".managed";
const INSTALLED_SKILLS_REGISTRY_FILE: &str = "installed.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRegistry {
    pub path: PathBuf,
    pub registry_id: String,
    pub channel: String,
    pub skills: Vec<SkillRegistryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRegistryEntry {
    pub skill_id: String,
    pub version: String,
    pub description: String,
    pub placement: String,
    pub package_layout: String,
    pub package_dir: PathBuf,
    pub sha256: String,
    pub minimum_openyak_version: Option<String>,
    pub tags: Vec<String>,
    pub compatible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledSkillRecord {
    pub skill_id: String,
    pub version: String,
    pub registry_id: String,
    pub channel: String,
    pub placement: String,
    pub install_root: PathBuf,
    pub source_path: PathBuf,
    pub sha256: String,
    pub installed_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    #[serde(default)]
    pub pinned_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledSkillRegistry {
    #[serde(default)]
    pub skills: BTreeMap<String, InstalledSkillRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstallOutcome {
    pub status: SkillInstallStatus,
    pub record: InstalledSkillRecord,
    pub registry_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUpdateOutcome {
    pub status: SkillUpdateStatus,
    pub old_record: InstalledSkillRecord,
    pub new_record: InstalledSkillRecord,
    pub registry_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUninstallOutcome {
    pub record: InstalledSkillRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillInstallStatus {
    Installed,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillUpdateStatus {
    Updated,
    Unchanged,
    Pinned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillInstallRequest {
    pub skill_id: String,
    pub version: Option<String>,
    pub registry_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUpdateRequest {
    pub skill_id: String,
    pub version: Option<String>,
    pub registry_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSkillEntry {
    pub entry: SkillRegistryEntry,
    pub compatible: bool,
    pub installed: Option<InstalledSkillRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableSkillCatalog {
    pub registry_path: PathBuf,
    pub registry_id: String,
    pub channel: String,
    pub entries: Vec<AvailableSkillEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalogInfo {
    pub installed: Option<InstalledSkillRecord>,
    pub registry_path: Option<PathBuf>,
    pub available_versions: Vec<SkillRegistryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRegistryManager {
    config_home: PathBuf,
    configured_registry_path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum SkillRegistryError {
    Io(io::Error),
    Parse(String),
    Invalid(String),
}

impl SkillRegistryError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl Display for SkillRegistryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Parse(error) | Self::Invalid(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for SkillRegistryError {}

impl From<io::Error> for SkillRegistryError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawSkillRegistry {
    schema_version: u32,
    registry_id: String,
    channel: String,
    #[serde(default)]
    skills: Vec<RawSkillRegistryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawSkillRegistryEntry {
    skill_id: String,
    version: String,
    description: String,
    placement: String,
    package_layout: String,
    package_path: String,
    sha256: String,
    #[serde(default)]
    minimum_openyak_version: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[must_use]
pub fn default_packaged_skill_registry_path(cwd: &Path) -> PathBuf {
    packaged_skill_registry_candidates(cwd)
        .into_iter()
        .next()
        .unwrap_or_else(|| cwd.join("assets").join("skills").join("registry.json"))
}

#[must_use]
pub fn default_managed_skills_root(openyak_home: &Path) -> PathBuf {
    openyak_home.join("skills").join(MANAGED_SKILLS_DIR_NAME)
}

pub fn resolve_skill_registry_path(
    explicit_registry_path: Option<&str>,
    cwd: &Path,
    config_home: &Path,
    configured_registry_path: Option<&str>,
) -> Result<PathBuf, SkillRegistryError> {
    if let Some(path) = explicit_registry_path {
        return Ok(resolve_input_path(cwd, path));
    }
    if let Some(path) = configured_registry_path {
        return Ok(resolve_input_path(config_home, path));
    }

    for packaged_default in packaged_skill_registry_candidates(cwd) {
        if packaged_default.is_file() {
            return Ok(packaged_default);
        }
    }

    Err(SkillRegistryError::invalid(
        "no local skills registry configured",
    ))
}

impl SkillRegistryManager {
    #[must_use]
    pub fn new(config_home: PathBuf, configured_registry_path: Option<PathBuf>) -> Self {
        Self {
            config_home,
            configured_registry_path,
        }
    }

    pub fn list_available(
        &self,
        cwd: &Path,
        explicit_registry_path: Option<&Path>,
    ) -> Result<AvailableSkillCatalog, SkillRegistryError> {
        let registry_path = self.resolve_registry_path(cwd, explicit_registry_path)?;
        let registry = load_skill_registry(&registry_path)?;
        let installed_registry =
            load_installed_skill_registry(&default_managed_skills_root(&self.config_home))?;
        let entries = registry
            .skills
            .iter()
            .cloned()
            .map(|entry| AvailableSkillEntry {
                compatible: entry.compatible,
                installed: installed_registry
                    .skills
                    .get(&entry.skill_id.to_ascii_lowercase())
                    .cloned(),
                entry,
            })
            .collect();
        Ok(AvailableSkillCatalog {
            registry_path,
            registry_id: registry.registry_id,
            channel: registry.channel,
            entries,
        })
    }

    pub fn info(
        &self,
        cwd: &Path,
        skill_id: &str,
        explicit_registry_path: Option<&Path>,
    ) -> Result<SkillCatalogInfo, SkillRegistryError> {
        let installed =
            find_installed_skill_record(&default_managed_skills_root(&self.config_home), skill_id)?;
        match self.resolve_registry_path(cwd, explicit_registry_path) {
            Ok(registry_path) => {
                let registry = load_skill_registry(&registry_path)?;
                let available_versions = registry
                    .skills
                    .into_iter()
                    .filter(|entry| entry.skill_id.eq_ignore_ascii_case(skill_id))
                    .collect::<Vec<_>>();
                if installed.is_none() && available_versions.is_empty() {
                    return Err(SkillRegistryError::invalid(format!(
                        "skill `{skill_id}` is not installed and is not available in {}",
                        registry_path.display()
                    )));
                }
                Ok(SkillCatalogInfo {
                    installed,
                    registry_path: Some(registry_path),
                    available_versions,
                })
            }
            Err(error)
                if error.to_string() == "no local skills registry configured"
                    && installed.is_some() =>
            {
                Ok(SkillCatalogInfo {
                    installed,
                    registry_path: None,
                    available_versions: Vec::new(),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub fn install(
        &self,
        cwd: &Path,
        request: &SkillInstallRequest,
    ) -> Result<SkillInstallOutcome, SkillRegistryError> {
        let registry_path = self.resolve_registry_path(cwd, request.registry_path.as_deref())?;
        let registry = load_skill_registry(&registry_path)?;
        install_managed_skill(
            &self.config_home,
            &registry,
            &request.skill_id,
            request.version.as_deref(),
        )
    }

    pub fn update(
        &self,
        cwd: &Path,
        request: &SkillUpdateRequest,
    ) -> Result<SkillUpdateOutcome, SkillRegistryError> {
        let registry_path = self.resolve_registry_path(cwd, request.registry_path.as_deref())?;
        let registry = load_skill_registry(&registry_path)?;
        update_managed_skill(
            &self.config_home,
            &registry,
            &request.skill_id,
            request.version.as_deref(),
        )
    }

    pub fn uninstall(&self, skill_id: &str) -> Result<SkillUninstallOutcome, SkillRegistryError> {
        uninstall_managed_skill(&self.config_home, skill_id)
    }

    fn resolve_registry_path(
        &self,
        cwd: &Path,
        explicit_registry_path: Option<&Path>,
    ) -> Result<PathBuf, SkillRegistryError> {
        resolve_skill_registry_path(
            explicit_registry_path.and_then(Path::to_str),
            cwd,
            &self.config_home,
            self.configured_registry_path
                .as_deref()
                .and_then(Path::to_str),
        )
    }
}

pub fn load_skill_registry(path: &Path) -> Result<SkillRegistry, SkillRegistryError> {
    let contents = fs::read_to_string(path)?;
    let raw: RawSkillRegistry = serde_json::from_str(&contents)
        .map_err(|error| SkillRegistryError::Parse(format!("{}: {error}", path.display())))?;
    if raw.schema_version == 0 {
        return Err(SkillRegistryError::invalid(format!(
            "{}: schema_version must be a positive integer",
            path.display()
        )));
    }
    let registry_id = require_nonempty(&raw.registry_id, "registry_id", path)?;
    let channel = require_nonempty(&raw.channel, "channel", path)?;
    let registry_root = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let canonical_registry_root = fs::canonicalize(&registry_root)?;
    let mut seen = BTreeSet::new();
    let mut skills = Vec::new();

    for raw_entry in raw.skills {
        let skill_id = require_nonempty(&raw_entry.skill_id, "skill_id", path)?;
        let version = require_nonempty(&raw_entry.version, "version", path)?;
        let description = require_nonempty(&raw_entry.description, "description", path)?;
        let placement = require_nonempty(&raw_entry.placement, "placement", path)?;
        let package_layout = require_nonempty(&raw_entry.package_layout, "package_layout", path)?;
        if package_layout != SKILL_DIRECTORY_LAYOUT_V1 {
            return Err(SkillRegistryError::invalid(format!(
                "{}: unsupported package_layout `{package_layout}` for {skill_id}@{version}",
                path.display()
            )));
        }
        let package_path = require_nonempty(&raw_entry.package_path, "package_path", path)?;
        let sha256 = normalize_sha256(&raw_entry.sha256, path, &skill_id, &version)?;
        let duplicate_key = format!("{}@{}", skill_id.to_ascii_lowercase(), version);
        if !seen.insert(duplicate_key) {
            return Err(SkillRegistryError::invalid(format!(
                "{}: duplicate registry entry for {skill_id}@{version}",
                path.display()
            )));
        }

        let package_dir =
            resolve_registry_package_dir(&canonical_registry_root, &registry_root, &package_path)?;
        let (resolved_skill_id, _) =
            read_skill_package_metadata(&package_dir).map_err(|error| {
                SkillRegistryError::invalid(format!(
                    "{}: invalid skill package {}: {error}",
                    path.display(),
                    package_dir.display()
                ))
            })?;
        if !resolved_skill_id.eq_ignore_ascii_case(&skill_id) {
            return Err(SkillRegistryError::invalid(format!(
                "{}: registry skill_id `{skill_id}` does not match package id `{resolved_skill_id}`",
                path.display()
            )));
        }
        let actual_sha256 = compute_directory_sha256(&package_dir)?;
        if actual_sha256 != sha256 {
            return Err(SkillRegistryError::invalid(format!(
                "{}: sha256 mismatch for {skill_id}@{version}",
                path.display()
            )));
        }

        skills.push(SkillRegistryEntry {
            skill_id,
            version: version.clone(),
            description,
            placement,
            package_layout,
            package_dir,
            sha256: actual_sha256,
            minimum_openyak_version: raw_entry.minimum_openyak_version.clone(),
            tags: raw_entry.tags,
            compatible: is_skill_version_compatible(raw_entry.minimum_openyak_version.as_deref()),
        });
    }

    skills.sort_by(|left, right| {
        left.skill_id
            .to_ascii_lowercase()
            .cmp(&right.skill_id.to_ascii_lowercase())
            .then_with(|| compare_versions(&right.version, &left.version))
    });

    Ok(SkillRegistry {
        path: path.to_path_buf(),
        registry_id,
        channel,
        skills,
    })
}

pub fn load_installed_skill_registry(
    managed_root: &Path,
) -> Result<InstalledSkillRegistry, SkillRegistryError> {
    let registry_path = managed_root.join(INSTALLED_SKILLS_REGISTRY_FILE);
    let contents = match fs::read_to_string(&registry_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(InstalledSkillRegistry::default())
        }
        Err(error) => return Err(SkillRegistryError::Io(error)),
    };
    if contents.trim().is_empty() {
        return Ok(InstalledSkillRegistry::default());
    }
    serde_json::from_str(&contents)
        .map_err(|error| SkillRegistryError::Parse(format!("{}: {error}", registry_path.display())))
}

pub fn save_installed_skill_registry(
    managed_root: &Path,
    registry: &InstalledSkillRegistry,
) -> Result<(), SkillRegistryError> {
    fs::create_dir_all(managed_root)?;
    let registry_path = managed_root.join(INSTALLED_SKILLS_REGISTRY_FILE);
    let contents = serde_json::to_string_pretty(registry)
        .map_err(|error| SkillRegistryError::Parse(error.to_string()))?;
    fs::write(registry_path, format!("{contents}\n"))?;
    Ok(())
}

pub fn find_installed_skill_record(
    managed_root: &Path,
    skill_id: &str,
) -> Result<Option<InstalledSkillRecord>, SkillRegistryError> {
    let registry = load_installed_skill_registry(managed_root)?;
    Ok(registry
        .skills
        .values()
        .find(|record| record.skill_id.eq_ignore_ascii_case(skill_id))
        .cloned())
}

pub fn install_managed_skill(
    openyak_home: &Path,
    registry: &SkillRegistry,
    skill_id: &str,
    version: Option<&str>,
) -> Result<SkillInstallOutcome, SkillRegistryError> {
    let entry = select_registry_entry(registry, skill_id, version)?;
    ensure_standard_placement(entry, &registry.path)?;
    let managed_root = default_managed_skills_root(openyak_home);
    let install_path = managed_root.join(&entry.skill_id);
    let temp_root = managed_root.join(".tmp");
    let mut installed_registry = load_installed_skill_registry(&managed_root)?;

    if let Some(existing) = find_record_key(&installed_registry, &entry.skill_id)
        .and_then(|key| installed_registry.skills.get(&key))
    {
        if existing.version == entry.version && existing.sha256 == entry.sha256 {
            return Ok(SkillInstallOutcome {
                status: SkillInstallStatus::Unchanged,
                record: existing.clone(),
                registry_path: registry.path.clone(),
            });
        }
        return Err(SkillRegistryError::invalid(format!(
            "skill `{}` is already installed at version {}; use `skills update {}` instead",
            existing.skill_id, existing.version, existing.skill_id
        )));
    }

    let staged_path = stage_skill_package(&entry.package_dir, &temp_root, &entry.skill_id)?;
    commit_staged_skill(&staged_path, &install_path, &temp_root)?;
    let now = unix_ms_now();
    let record = InstalledSkillRecord {
        skill_id: entry.skill_id.clone(),
        version: entry.version.clone(),
        registry_id: registry.registry_id.clone(),
        channel: registry.channel.clone(),
        placement: entry.placement.clone(),
        install_root: managed_root.clone(),
        source_path: registry.path.clone(),
        sha256: entry.sha256.clone(),
        installed_at_unix_ms: now,
        updated_at_unix_ms: now,
        pinned_version: version.map(ToOwned::to_owned),
    };
    installed_registry
        .skills
        .insert(record.skill_id.to_ascii_lowercase(), record.clone());
    save_installed_skill_registry(&managed_root, &installed_registry)?;
    Ok(SkillInstallOutcome {
        status: SkillInstallStatus::Installed,
        record,
        registry_path: registry.path.clone(),
    })
}

pub fn update_managed_skill(
    openyak_home: &Path,
    registry: &SkillRegistry,
    skill_id: &str,
    version: Option<&str>,
) -> Result<SkillUpdateOutcome, SkillRegistryError> {
    let managed_root = default_managed_skills_root(openyak_home);
    let mut installed_registry = load_installed_skill_registry(&managed_root)?;
    let Some(record_key) = find_record_key(&installed_registry, skill_id) else {
        return Err(SkillRegistryError::invalid(format!(
            "skill `{skill_id}` is not registry-managed"
        )));
    };
    let Some(existing) = installed_registry.skills.get(&record_key).cloned() else {
        return Err(SkillRegistryError::invalid(format!(
            "skill `{skill_id}` is not registry-managed"
        )));
    };
    if version.is_none() && existing.pinned_version.is_some() {
        return Ok(SkillUpdateOutcome {
            status: SkillUpdateStatus::Pinned,
            old_record: existing.clone(),
            new_record: existing,
            registry_path: registry.path.clone(),
        });
    }
    let entry = select_registry_entry(registry, &existing.skill_id, version)?;
    ensure_standard_placement(entry, &registry.path)?;
    if existing.version == entry.version && existing.sha256 == entry.sha256 {
        return Ok(SkillUpdateOutcome {
            status: SkillUpdateStatus::Unchanged,
            old_record: existing.clone(),
            new_record: existing,
            registry_path: registry.path.clone(),
        });
    }

    let install_path = managed_root.join(&existing.skill_id);
    let temp_root = managed_root.join(".tmp");
    let staged_path = stage_skill_package(&entry.package_dir, &temp_root, &existing.skill_id)?;
    commit_staged_skill(&staged_path, &install_path, &temp_root)?;
    let updated_record = InstalledSkillRecord {
        skill_id: existing.skill_id.clone(),
        version: entry.version.clone(),
        registry_id: registry.registry_id.clone(),
        channel: registry.channel.clone(),
        placement: entry.placement.clone(),
        install_root: managed_root.clone(),
        source_path: registry.path.clone(),
        sha256: entry.sha256.clone(),
        installed_at_unix_ms: existing.installed_at_unix_ms,
        updated_at_unix_ms: unix_ms_now(),
        pinned_version: version
            .map(ToOwned::to_owned)
            .or(existing.pinned_version.clone()),
    };
    installed_registry
        .skills
        .insert(record_key, updated_record.clone());
    save_installed_skill_registry(&managed_root, &installed_registry)?;
    Ok(SkillUpdateOutcome {
        status: SkillUpdateStatus::Updated,
        old_record: existing,
        new_record: updated_record,
        registry_path: registry.path.clone(),
    })
}

pub fn uninstall_managed_skill(
    openyak_home: &Path,
    skill_id: &str,
) -> Result<SkillUninstallOutcome, SkillRegistryError> {
    let managed_root = default_managed_skills_root(openyak_home);
    let mut installed_registry = load_installed_skill_registry(&managed_root)?;
    let Some(record_key) = find_record_key(&installed_registry, skill_id) else {
        return Err(SkillRegistryError::invalid(format!(
            "skill `{skill_id}` is not registry-managed"
        )));
    };
    let record = installed_registry
        .skills
        .remove(&record_key)
        .ok_or_else(|| {
            SkillRegistryError::invalid(format!("skill `{skill_id}` is not registry-managed"))
        })?;
    let install_path = managed_root.join(&record.skill_id);
    if install_path.exists() {
        fs::remove_dir_all(&install_path)?;
    }
    save_installed_skill_registry(&managed_root, &installed_registry)?;
    Ok(SkillUninstallOutcome { record })
}

fn require_nonempty(
    value: &str,
    field_name: &str,
    registry_path: &Path,
) -> Result<String, SkillRegistryError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(SkillRegistryError::invalid(format!(
            "{}: {field_name} must not be empty",
            registry_path.display()
        )));
    }
    Ok(trimmed.to_string())
}

fn normalize_sha256(
    value: &str,
    registry_path: &Path,
    skill_id: &str,
    version: &str,
) -> Result<String, SkillRegistryError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(SkillRegistryError::invalid(format!(
            "{}: invalid sha256 for {skill_id}@{version}",
            registry_path.display()
        )));
    }
    Ok(normalized)
}

fn resolve_input_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn packaged_skill_registry_candidates(cwd: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_unique_candidate(
        &mut candidates,
        cwd.join("assets").join("skills").join("registry.json"),
    );
    if let Some(parent) = cwd.parent() {
        push_unique_candidate(
            &mut candidates,
            parent.join("assets").join("skills").join("registry.json"),
        );
    }
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(exe_dir) = current_exe.parent() {
            push_unique_candidate(
                &mut candidates,
                exe_dir.join("assets").join("skills").join("registry.json"),
            );
            if let Some(parent) = exe_dir.parent() {
                push_unique_candidate(
                    &mut candidates,
                    parent.join("assets").join("skills").join("registry.json"),
                );
            }
        }
    }
    candidates
}

fn push_unique_candidate(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn resolve_registry_package_dir(
    canonical_registry_root: &Path,
    registry_root: &Path,
    package_path: &str,
) -> Result<PathBuf, SkillRegistryError> {
    let package_path = PathBuf::from(package_path);
    if package_path.is_absolute() {
        return Err(SkillRegistryError::invalid(
            "registry package_path must be relative to the registry root",
        ));
    }
    let resolved_path = registry_root.join(&package_path);
    let canonical_package_dir = fs::canonicalize(&resolved_path)?;
    if !canonical_package_dir.starts_with(canonical_registry_root) {
        return Err(SkillRegistryError::invalid(format!(
            "registry package_path `{}` escapes the registry root",
            package_path.display()
        )));
    }
    if !canonical_package_dir.join("SKILL.md").is_file() {
        return Err(SkillRegistryError::invalid(format!(
            "registry package_path `{}` does not contain SKILL.md",
            package_path.display()
        )));
    }
    Ok(canonical_package_dir)
}

fn compute_directory_sha256(root: &Path) -> Result<String, SkillRegistryError> {
    let mut file_paths = WalkDir::new(root)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| SkillRegistryError::invalid(error.to_string()))?
        .into_iter()
        .map(|entry| {
            if entry.file_type().is_symlink() {
                return Err(SkillRegistryError::invalid(format!(
                    "skill package `{}` contains unsupported symlink `{}`",
                    root.display(),
                    entry.path().display()
                )));
            }
            Ok(entry)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path().to_path_buf())
        .collect::<Vec<_>>();
    file_paths.sort();

    let mut hasher = Sha256::new();
    for file_path in file_paths {
        let relative = file_path
            .strip_prefix(root)
            .map_err(|error| SkillRegistryError::invalid(error.to_string()))?;
        let normalized = relative.to_string_lossy().replace('\\', "/");
        hasher.update(normalized.as_bytes());
        hasher.update([0]);
        hasher.update(fs::read(&file_path)?);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn is_skill_version_compatible(minimum_openyak_version: Option<&str>) -> bool {
    minimum_openyak_version.is_none_or(|minimum| {
        compare_versions(env!("CARGO_PKG_VERSION"), minimum) != Ordering::Less
    })
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    parse_version(left)
        .cmp(&parse_version(right))
        .then_with(|| left.cmp(right))
}

fn parse_version(value: &str) -> (Vec<u64>, String) {
    let mut components = Vec::new();
    for part in value.split('.') {
        match part.parse::<u64>() {
            Ok(component) => components.push(component),
            Err(_) => return (Vec::new(), value.to_string()),
        }
    }
    (components, String::new())
}

fn select_registry_entry<'a>(
    registry: &'a SkillRegistry,
    skill_id: &str,
    version: Option<&str>,
) -> Result<&'a SkillRegistryEntry, SkillRegistryError> {
    let mut candidates = registry
        .skills
        .iter()
        .filter(|entry| entry.skill_id.eq_ignore_ascii_case(skill_id))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Err(SkillRegistryError::invalid(format!(
            "skill `{skill_id}` is not available in {}",
            registry.path.display()
        )));
    }
    if let Some(version) = version {
        return candidates
            .into_iter()
            .find(|entry| entry.version == version)
            .ok_or_else(|| {
                SkillRegistryError::invalid(format!(
                    "skill `{skill_id}` version `{version}` is not available in {}",
                    registry.path.display()
                ))
            });
    }
    candidates.retain(|entry| entry.compatible);
    candidates.sort_by(|left, right| compare_versions(&right.version, &left.version));
    candidates.into_iter().next().ok_or_else(|| {
        SkillRegistryError::invalid(format!(
            "skill `{skill_id}` has no compatible versions in {}",
            registry.path.display()
        ))
    })
}

fn ensure_standard_placement(
    entry: &SkillRegistryEntry,
    registry_path: &Path,
) -> Result<(), SkillRegistryError> {
    if entry.placement.eq_ignore_ascii_case("standard") {
        return Ok(());
    }
    Err(SkillRegistryError::invalid(format!(
        "{}: registry-managed installs only support `standard` placement in phase 1 (got `{}` for {}@{})",
        registry_path.display(),
        entry.placement,
        entry.skill_id,
        entry.version
    )))
}

fn find_record_key(registry: &InstalledSkillRegistry, skill_id: &str) -> Option<String> {
    registry
        .skills
        .keys()
        .find(|candidate| candidate.eq_ignore_ascii_case(skill_id))
        .cloned()
}

fn stage_skill_package(
    source_dir: &Path,
    temp_root: &Path,
    skill_id: &str,
) -> Result<PathBuf, SkillRegistryError> {
    fs::create_dir_all(temp_root)?;
    let staged_path = temp_root.join(format!("{skill_id}-{}", unix_ms_now()));
    copy_dir_recursive(source_dir, &staged_path)?;
    Ok(staged_path)
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> Result<(), SkillRegistryError> {
    for entry in WalkDir::new(source) {
        let entry = entry.map_err(|error| SkillRegistryError::invalid(error.to_string()))?;
        if entry.file_type().is_symlink() {
            return Err(SkillRegistryError::invalid(format!(
                "skill package `{}` contains unsupported symlink `{}`",
                source.display(),
                entry.path().display()
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| SkillRegistryError::invalid(error.to_string()))?;
        let target = destination.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

fn commit_staged_skill(
    staged_path: &Path,
    install_path: &Path,
    temp_root: &Path,
) -> Result<(), SkillRegistryError> {
    if let Some(parent) = install_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !install_path.exists() {
        fs::rename(staged_path, install_path)?;
        return Ok(());
    }
    let backup_path = temp_root.join(format!(
        "{}-backup-{}",
        install_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("skill"),
        unix_ms_now()
    ));
    fs::rename(install_path, &backup_path)?;
    match fs::rename(staged_path, install_path) {
        Ok(()) => {
            if backup_path.exists() {
                fs::remove_dir_all(backup_path)?;
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup_path, install_path);
            Err(SkillRegistryError::Io(error))
        }
    }
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time should be after epoch")
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::{
        compute_directory_sha256, find_installed_skill_record, install_managed_skill,
        load_skill_registry, resolve_skill_registry_path, uninstall_managed_skill,
        update_managed_skill, SkillInstallStatus, SkillRegistryError, SkillUpdateStatus,
    };
    use serde_json::json;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct RegistryEntryFixture<'a> {
        skill_id: &'a str,
        version: &'a str,
        description: &'a str,
        placement: &'a str,
        package_dir: PathBuf,
        minimum_openyak_version: Option<&'a str>,
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-skill-registry-{label}-{nanos}"))
    }

    fn write_skill_package(
        root: &Path,
        dir_name: &str,
        skill_id: &str,
        description: &str,
        body: &str,
    ) -> PathBuf {
        let package_dir = root.join(dir_name);
        fs::create_dir_all(&package_dir).expect("package dir");
        fs::write(
            package_dir.join("SKILL.md"),
            format!(
                "---\nname: {skill_id}\ndescription: {description}\n---\n\n# {skill_id}\n\n{body}\n"
            ),
        )
        .expect("write skill package");
        package_dir
    }

    fn write_registry(path: &Path, entries: &[RegistryEntryFixture<'_>]) {
        let registry_root = path.parent().expect("registry root");
        fs::create_dir_all(registry_root).expect("registry root dir");
        let skills = entries
            .iter()
            .map(|entry| {
                let relative_package_path = entry
                    .package_dir
                    .strip_prefix(registry_root)
                    .expect("package under registry root")
                    .to_string_lossy()
                    .replace('\\', "/");
                json!({
                    "skill_id": entry.skill_id,
                    "version": entry.version,
                    "description": entry.description,
                    "placement": entry.placement,
                    "package_layout": "skill_directory_v1",
                    "package_path": relative_package_path,
                    "sha256": compute_directory_sha256(&entry.package_dir)
                        .expect("package digest should compute"),
                    "minimum_openyak_version": entry.minimum_openyak_version,
                    "tags": [],
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({
                    "schema_version": 1,
                    "registry_id": "packaged-fixture",
                    "channel": "stable",
                    "skills": skills,
                }))
                .expect("registry json")
            ),
        )
        .expect("write registry");
    }

    fn create_test_symlink(link: &Path, target: &Path) -> io::Result<()> {
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link)
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link)
        }
    }

    #[test]
    fn resolve_skill_registry_path_prefers_explicit_then_configured_then_packaged_default() {
        let root = temp_dir("resolution");
        let cwd = root.join("rust");
        let config_home = root.join("openyak-home");
        let packaged_registry = root.join("assets").join("skills").join("registry.json");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(packaged_registry.parent().expect("packaged parent"))
            .expect("packaged parent");
        fs::write(
            &packaged_registry,
            "{\"schema_version\":1,\"registry_id\":\"packaged\",\"channel\":\"stable\",\"skills\":[]}\n",
        )
        .expect("packaged registry");

        assert_eq!(
            resolve_skill_registry_path(
                Some("custom/registry.json"),
                &cwd,
                &config_home,
                Some("configured/registry.json"),
            )
            .expect("explicit path"),
            cwd.join("custom").join("registry.json")
        );
        assert_eq!(
            resolve_skill_registry_path(None, &cwd, &config_home, Some("configured/registry.json"))
                .expect("configured path"),
            config_home.join("configured").join("registry.json")
        );
        assert_eq!(
            resolve_skill_registry_path(None, &cwd, &config_home, None).expect("packaged default"),
            packaged_registry
        );

        fs::remove_file(root.join("assets").join("skills").join("registry.json"))
            .expect("remove packaged registry");
        let error = resolve_skill_registry_path(None, &cwd, &config_home, None)
            .expect_err("missing registry should fail");
        assert_eq!(error.to_string(), "no local skills registry configured");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_skill_registry_rejects_digest_mismatch_after_package_changes() {
        let root = temp_dir("digest-mismatch");
        let registry_root = root.join("registry");
        let package_dir = write_skill_package(
            &registry_root.join("packages"),
            "release-checklist-v1",
            "release-checklist",
            "Release checklist",
            "Check build artifacts.",
        );
        let registry_path = registry_root.join("registry.json");
        write_registry(
            &registry_path,
            &[RegistryEntryFixture {
                skill_id: "release-checklist",
                version: "1.0.0",
                description: "Release checklist",
                placement: "standard",
                package_dir: package_dir.clone(),
                minimum_openyak_version: None,
            }],
        );
        fs::write(
            package_dir.join("SKILL.md"),
            "---\nname: release-checklist\ndescription: Release checklist\n---\n\nmutated\n",
        )
        .expect("mutate package");

        let error = load_skill_registry(&registry_path).expect_err("digest mismatch should fail");
        assert!(error
            .to_string()
            .contains("sha256 mismatch for release-checklist@1.0.0"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_skill_registry_rejects_package_paths_that_escape_the_registry_root() {
        let root = temp_dir("escape");
        let registry_root = root.join("registry");
        let outside_dir = write_skill_package(
            &root.join("outside"),
            "escaped-package",
            "escape-demo",
            "Escaped package",
            "Outside the registry root.",
        );
        fs::create_dir_all(&registry_root).expect("registry root");
        let registry_path = registry_root.join("registry.json");
        fs::write(
            &registry_path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({
                    "schema_version": 1,
                    "registry_id": "escape-fixture",
                    "channel": "stable",
                    "skills": [{
                        "skill_id": "escape-demo",
                        "version": "1.0.0",
                        "description": "Escaped package",
                        "placement": "standard",
                        "package_layout": "skill_directory_v1",
                        "package_path": "../outside/escaped-package",
                        "sha256": compute_directory_sha256(&outside_dir).expect("digest"),
                        "tags": [],
                    }],
                }))
                .expect("registry json")
            ),
        )
        .expect("write escape registry");

        let error = load_skill_registry(&registry_path).expect_err("escaped package should fail");
        assert!(error.to_string().contains("escapes the registry root"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_skill_registry_rejects_packages_with_symlinks() {
        let root = temp_dir("symlink-reject");
        let registry_root = root.join("registry");
        let packages_root = registry_root.join("packages");
        let package_dir = write_skill_package(
            &packages_root,
            "symlinked-skill-v1",
            "symlinked-skill",
            "Symlink demo",
            "Registry package should reject symlinks.",
        );
        let outside_file = root.join("outside-secret.txt");
        fs::write(&outside_file, "outside\n").expect("write outside file");
        let link_path = package_dir.join("linked.txt");
        match create_test_symlink(&link_path, &outside_file) {
            Ok(()) => {}
            Err(_error) if cfg!(windows) => {
                let _ = fs::remove_dir_all(root);
                return;
            }
            Err(error) => panic!("create test symlink: {error}"),
        }

        let digest_error =
            compute_directory_sha256(&package_dir).expect_err("symlinked package rejected");
        assert!(digest_error
            .to_string()
            .contains("contains unsupported symlink"));

        let registry_path = registry_root.join("registry.json");
        fs::write(
            &registry_path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(&json!({
                    "schema_version": 1,
                    "registry_id": "symlink-fixture",
                    "channel": "stable",
                    "skills": [{
                        "skill_id": "symlinked-skill",
                        "version": "1.0.0",
                        "description": "Symlink demo",
                        "placement": "standard",
                        "package_layout": "skill_directory_v1",
                        "package_path": "packages/symlinked-skill-v1",
                        "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
                        "tags": [],
                    }],
                }))
                .expect("registry json")
            ),
        )
        .expect("write registry");

        let error = load_skill_registry(&registry_path).expect_err("symlinked package should fail");
        assert!(error.to_string().contains("contains unsupported symlink"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_skill_registry_rejects_path_like_skill_ids() {
        let root = temp_dir("path-like-skill-id");
        let registry_root = root.join("registry");
        let package_dir = write_skill_package(
            &registry_root.join("packages"),
            "escaped-skill",
            "../escape-demo",
            "Escaped skill",
            "Should never install outside the managed root.",
        );
        let registry_path = registry_root.join("registry.json");
        write_registry(
            &registry_path,
            &[RegistryEntryFixture {
                skill_id: "../escape-demo",
                version: "1.0.0",
                description: "Escaped skill",
                placement: "standard",
                package_dir,
                minimum_openyak_version: None,
            }],
        );

        let error =
            load_skill_registry(&registry_path).expect_err("path-like skill id should fail");
        assert!(error
            .to_string()
            .contains("skill package id must not contain path separators"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_skill_lifecycle_supports_pinning_update_and_uninstall() {
        let root = temp_dir("lifecycle");
        let registry_root = root.join("registry");
        let packages_root = registry_root.join("packages");
        let v1_dir = write_skill_package(
            &packages_root,
            "release-checklist-v1",
            "release-checklist",
            "Release checklist v1",
            "Run unit tests first.",
        );
        let v2_dir = write_skill_package(
            &packages_root,
            "release-checklist-v2",
            "release-checklist",
            "Release checklist v2",
            "Run unit tests and smoke checks.",
        );
        let registry_path = registry_root.join("registry.json");
        write_registry(
            &registry_path,
            &[
                RegistryEntryFixture {
                    skill_id: "release-checklist",
                    version: "1.0.0",
                    description: "Release checklist v1",
                    placement: "standard",
                    package_dir: v1_dir,
                    minimum_openyak_version: None,
                },
                RegistryEntryFixture {
                    skill_id: "release-checklist",
                    version: "2.0.0",
                    description: "Release checklist v2",
                    placement: "standard",
                    package_dir: v2_dir,
                    minimum_openyak_version: None,
                },
            ],
        );
        let registry = load_skill_registry(&registry_path).expect("registry should load");
        let openyak_home = root.join("openyak-home");

        let install =
            install_managed_skill(&openyak_home, &registry, "release-checklist", Some("1.0.0"))
                .expect("install should succeed");
        assert_eq!(install.status, SkillInstallStatus::Installed);
        assert_eq!(install.record.version, "1.0.0");
        assert_eq!(install.record.pinned_version.as_deref(), Some("1.0.0"));
        assert!(openyak_home
            .join("skills")
            .join(".managed")
            .join("release-checklist")
            .join("SKILL.md")
            .is_file());

        let pinned_update =
            update_managed_skill(&openyak_home, &registry, "release-checklist", None)
                .expect("pinned update should return status");
        assert_eq!(pinned_update.status, SkillUpdateStatus::Pinned);
        assert_eq!(pinned_update.new_record.version, "1.0.0");

        let explicit_update =
            update_managed_skill(&openyak_home, &registry, "release-checklist", Some("2.0.0"))
                .expect("explicit update should succeed");
        assert_eq!(explicit_update.status, SkillUpdateStatus::Updated);
        assert_eq!(explicit_update.old_record.version, "1.0.0");
        assert_eq!(explicit_update.new_record.version, "2.0.0");
        assert_eq!(
            explicit_update.new_record.pinned_version.as_deref(),
            Some("2.0.0")
        );

        let uninstall = uninstall_managed_skill(&openyak_home, "release-checklist")
            .expect("uninstall should work");
        assert_eq!(uninstall.record.version, "2.0.0");
        assert!(!openyak_home
            .join("skills")
            .join(".managed")
            .join("release-checklist")
            .exists());
        assert_eq!(
            find_installed_skill_record(
                &openyak_home.join("skills").join(".managed"),
                "release-checklist"
            )
            .expect("installed registry should load"),
            None
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn managed_install_rejects_non_standard_placement_entries() {
        let root = temp_dir("non-standard");
        let registry_root = root.join("registry");
        let package_dir = write_skill_package(
            &registry_root.join("packages"),
            "system-helper",
            "system-helper",
            "System helper",
            "Should stay unmanaged.",
        );
        let registry_path = registry_root.join("registry.json");
        write_registry(
            &registry_path,
            &[RegistryEntryFixture {
                skill_id: "system-helper",
                version: "1.0.0",
                description: "System helper",
                placement: "system",
                package_dir,
                minimum_openyak_version: None,
            }],
        );
        let registry = load_skill_registry(&registry_path).expect("registry should load");

        let error =
            install_managed_skill(&root.join("openyak-home"), &registry, "system-helper", None)
                .expect_err("non-standard placement should fail");
        assert!(matches!(error, SkillRegistryError::Invalid(_)));
        assert!(error
            .to_string()
            .contains("registry-managed installs only support `standard` placement in phase 1"));

        let _ = fs::remove_dir_all(root);
    }
}
