use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chauffeur_judges::Models;
use chauffeur_protocol::Rule;
use chauffeur_runtime::{Context, Redactor};

use crate::DaemonError;
use crate::paths::Paths;

/// The most rule files loaded from one source, and from one project's
/// folder. The module is private, so this is no more visible than the crate.
pub const MAX_RULES: usize = 512;

/// The most folders read for rules in one source.
const MAX_FOLDERS: usize = 256;

/// The largest rule file read; a larger one is refused with its path.
const MAX_RULE_BYTES: u64 = 1 << 20;

/// The largest `projects.json` read; a larger one is refused.
const MAX_PROJECTS_BYTES: u64 = 64 * 1024;

/// The most projects `projects.json` may list.
const MAX_PROJECTS: usize = 256;

/// The epoch the next loaded configuration gets.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Everything loaded from the distribution and the user's configuration.
pub struct Loaded {
    /// Which load this is, so a cache can tell it from a later one.
    epoch: u64,
    /// Rules by ID; a user rule replaces a shipped one with the same ID.
    pub rules: Vec<Arc<Rule>>,
    /// Each rule file that was refused, with why.
    pub refused: Vec<String>,
    /// The projects whose inline System Two is allowed, from `projects.json`.
    projects: BTreeSet<PathBuf>,
    /// Models, redaction and keys.
    pub context: Arc<Context>,
    /// The rule each file holds, so a file that stops loading keeps it.
    files: BTreeMap<PathBuf, Arc<Rule>>,
}

/// Loads models, redaction, projects and rules for `paths`.
///
/// A rule file that cannot load is refused on its own, with its path, and
/// the rest still load. When `previous` loaded it, that rule still runs, so
/// a file saved halfway through an edit changes nothing.
///
/// # Errors
///
/// Models, redaction or projects that cannot load.
pub fn load(paths: &Paths, previous: Option<&Loaded>) -> Result<Loaded, DaemonError> {
    let model_files = paths.models.clone().map_or_else(
        || {
            let mut files = json_files(&paths.distribution.join("models"));
            files.extend(existing(paths.config.join("models.json")));
            files
        },
        |models| vec![models],
    );
    let models =
        Models::load(&model_files).map_err(|error| DaemonError::Config(error.to_string()))?;

    let mut redaction_files = vec![paths.distribution.join("safety").join("redaction.json")];
    redaction_files.extend(existing(paths.config.join("redaction.json")));
    let redactor =
        Redactor::load(&redaction_files).map_err(|error| DaemonError::Config(error.to_string()))?;

    let projects_file = paths.config.join("projects.json");
    let projects = load_projects(&projects_file)
        .map_err(|error| DaemonError::Config(format!("{}: {error}", projects_file.display())))?;

    let keys = Box::new(|name: &str| std::env::var(name).ok().filter(|key| !key.is_empty()));
    let mut loaded = Loaded {
        epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
        rules: Vec::new(),
        refused: Vec::new(),
        projects,
        context: Arc::new(Context {
            models,
            redactor,
            keys,
        }),
        files: BTreeMap::new(),
    };
    for folder in [paths.distribution.join("rules"), paths.config.join("rules")] {
        for path in json_files_below(&folder) {
            let kept = previous.and_then(|previous| previous.files.get(&path));
            match (load_rule(&folder, &path), kept) {
                (Ok(rule), _) => loaded.add(path, Arc::new(rule)),
                (Err(reason), Some(kept)) => {
                    loaded.refused.push(format!(
                        "{}: {reason}; the version loaded before still runs",
                        path.display()
                    ));
                    loaded.add(path, Arc::clone(kept));
                }
                (Err(reason), None) => {
                    loaded.refused.push(format!("{}: {reason}", path.display()));
                }
            }
        }
    }
    Ok(loaded)
}

impl Loaded {
    /// Which load this is; a later load has another epoch.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The projects whose inline System Two is allowed.
    #[must_use]
    pub const fn projects(&self) -> &BTreeSet<PathBuf> {
        &self.projects
    }

    /// Adds the rule `path` holds, in place of any with its ID.
    fn add(&mut self, path: PathBuf, rule: Arc<Rule>) {
        self.rules.retain(|loaded| loaded.id() != rule.id());
        self.rules.push(Arc::clone(&rule));
        self.files.insert(path, rule);
    }
}

pub fn load_rule(folder: &Path, path: &Path) -> Result<Rule, String> {
    let id = path.trim_prefix(folder).with_extension("");
    let id = id.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(MAX_RULE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;

    // The size is checked before the bytes are read as text, so a cut inside
    // a multi-byte character is refused for its size, not as not UTF-8.
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RULE_BYTES {
        return Err("the rule file is over 1 MiB".to_owned());
    }

    let text =
        String::from_utf8(bytes).map_err(|error| format!("the rule file is not UTF-8: {error}"))?;
    Rule::parse(&id, &text).map_err(|error| error.to_string())
}

/// The projects `path` lists, each an absolute path, canonicalized when it
/// exists: at most [`MAX_PROJECTS`] from a file of at most
/// [`MAX_PROJECTS_BYTES`].
///
/// A file that is not there lists none, so no project may run inline System
/// Two until the user writes one. Any other problem refuses the file, so a
/// typo cannot quietly allow a project.
fn load_projects(path: &Path) -> Result<BTreeSet<PathBuf>, String> {
    if !path.is_file() {
        return Ok(BTreeSet::new());
    }

    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|error| error.to_string())?
        .take(MAX_PROJECTS_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    // Counted before it is read as text: a file cut inside a multi-byte
    // character is over the limit, not a broken character.
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROJECTS_BYTES {
        return Err("the file is over 64 KiB".to_owned());
    }
    let text =
        String::from_utf8(bytes).map_err(|error| format!("the file is not UTF-8: {error}"))?;

    let listed: Vec<String> = serde_json::from_str(&text)
        .map_err(|error| format!("it is not a JSON array of paths: {error}"))?;
    if listed.len() > MAX_PROJECTS {
        return Err(format!(
            "it lists {} projects; the most is {MAX_PROJECTS}",
            listed.len()
        ));
    }

    let mut projects = BTreeSet::new();
    for entry in listed {
        let project = PathBuf::from(entry);
        if !project.is_absolute() {
            return Err(format!(
                "`\"{}\"` is not an absolute path",
                project.to_string_lossy().escape_debug()
            ));
        }
        // A canonical entry matches the canonical request, so a symlinked
        // worktree or `/var` against `/private/var` still counts. An entry
        // that does not exist yet stays as written.
        projects.insert(std::fs::canonicalize(&project).unwrap_or(project));
    }
    Ok(projects)
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

/// The `.json` files directly in `folder`, sorted.
fn json_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
}

/// The `.json` files in `folder` and below, sorted, at most [`MAX_RULES`]
/// from at most [`MAX_FOLDERS`] folders. A folder reached again through a
/// link is read once, by the path that reaches it first, so a link back to
/// a folder above it neither repeats its rules nor walks forever.
pub fn json_files_below(folder: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut visited = BTreeSet::new();
    let mut pending = vec![folder.to_owned()];
    while let Some(next) = pending.pop() {
        if found.len() >= MAX_RULES || visited.len() >= MAX_FOLDERS {
            break;
        }
        let Ok(real) = std::fs::canonicalize(&next) else {
            continue;
        };
        if !visited.insert(real) {
            continue;
        }
        found.extend(json_files(&next));
        let mut subfolders: Vec<PathBuf> = std::fs::read_dir(&next)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        // Popped in name order, so the same folders always load the same IDs.
        subfolders.sort_by(|a, b| b.cmp(a));
        pending.extend(subfolders);
    }
    found.sort();
    found.truncate(MAX_RULES);
    found
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{MAX_PROJECTS, MAX_PROJECTS_BYTES, MAX_RULE_BYTES, load_projects, load_rule};

    /// A uniquely named temporary path, so tests run in parallel.
    fn temporary(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        std::env::temp_dir().join(format!(
            "chauffeur-{name}-{}-{nanos}.json",
            std::process::id()
        ))
    }

    /// Writes `bytes` to a uniquely named temporary file.
    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let path = temporary(name);
        let written = fs::write(&path, bytes);
        assert!(written.is_ok(), "the temporary file should be writable");
        path
    }

    /// Writes `bytes` to a uniquely named temporary rule file, returning its
    /// folder and path.
    fn temp_rule(bytes: &[u8]) -> (PathBuf, PathBuf) {
        (std::env::temp_dir(), temp_file("rule", bytes))
    }

    /// The smallest file over the limit is refused for its size, before it is
    /// read as a rule: its contents are not JSON.
    #[test]
    fn a_rule_file_over_one_mebibyte_is_refused() {
        let over = usize::try_from(MAX_RULE_BYTES.saturating_add(1)).unwrap_or(0);
        let bytes = vec![b' '; over];

        let (folder, path) = temp_rule(&bytes);
        let refused = load_rule(&folder, &path);
        let _ = fs::remove_file(&path);

        assert_eq!(
            refused.err(),
            Some("the rule file is over 1 MiB".to_owned())
        );
    }

    /// A file over the limit whose cut falls inside a multi-byte character is
    /// refused for its size, not for not being UTF-8.
    #[test]
    fn a_rule_file_of_multibyte_characters_over_one_mebibyte_is_refused() {
        let bytes = "€".repeat(349_526).into_bytes();
        let limit = usize::try_from(MAX_RULE_BYTES).unwrap_or(0);
        assert!(bytes.len() > limit, "the sample is over the limit");

        let (folder, path) = temp_rule(&bytes);
        let refused = load_rule(&folder, &path);
        let _ = fs::remove_file(&path);

        assert_eq!(
            refused.err(),
            Some("the rule file is over 1 MiB".to_owned())
        );
    }

    /// A valid rule padded to exactly the limit is read in full: the cap only
    /// refuses a larger file.
    #[test]
    fn a_rule_file_at_one_mebibyte_is_accepted() {
        let rule = concat!(
            r#"{"on":"x","strategy":[{"id":"s","judge":"system-one","#,
            r#""ask":{"outcomes":["yes","no"],"instructions":"?"}}]}"#
        );
        let exact = usize::try_from(MAX_RULE_BYTES).unwrap_or(0);
        let mut bytes = rule.as_bytes().to_vec();
        assert!(
            bytes.len() <= exact,
            "the sample rule fits within the limit"
        );
        bytes.resize(exact, b' ');

        let (folder, path) = temp_rule(&bytes);
        let loaded = load_rule(&folder, &path);
        let _ = fs::remove_file(&path);

        assert!(loaded.is_ok(), "a rule at the limit should load");
    }

    /// A file over the limit is refused for its size, before it is read as
    /// JSON: its contents are not a list of paths.
    #[test]
    fn a_projects_file_over_64_kib_is_refused() {
        let over = usize::try_from(MAX_PROJECTS_BYTES.saturating_add(1)).unwrap_or(0);
        let bytes = vec![b' '; over];
        let path = temp_file("projects-size", &bytes);

        let refused = load_projects(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(refused.err(), Some("the file is over 64 KiB".to_owned()));
    }

    /// A valid list padded to exactly the limit is read in full: the cap only
    /// refuses a larger file.
    #[test]
    fn a_projects_file_at_64_kib_is_accepted() {
        let exact = usize::try_from(MAX_PROJECTS_BYTES).unwrap_or(0);
        let mut bytes = br#"["/a"]"#.to_vec();
        assert!(
            bytes.len() <= exact,
            "the sample list fits within the limit"
        );
        bytes.resize(exact, b' ');

        let path = temp_file("projects-exact-size", &bytes);
        let loaded = load_projects(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(
            loaded,
            Ok(BTreeSet::from([PathBuf::from("/a")])),
            "a list at the limit should load"
        );
    }

    /// A file cut inside a multi-byte character is refused for its size:
    /// the bytes are counted before they are read as UTF-8, so a cut
    /// character cannot be reported as broken text first.
    #[test]
    fn a_projects_file_over_64_kib_cut_inside_a_character_is_refused_for_its_size() {
        let over = usize::try_from(MAX_PROJECTS_BYTES.saturating_add(1)).unwrap_or(0);
        let mut bytes = format!("[\"/{}\"]", "€".repeat(30_000)).into_bytes();
        bytes.truncate(over);
        assert!(
            std::str::from_utf8(&bytes).is_err(),
            "the cut lands inside a three-byte character"
        );
        let path = temp_file("projects-utf8-cut", &bytes);

        let refused = load_projects(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(refused.err(), Some("the file is over 64 KiB".to_owned()));
    }

    /// A file within the limit that is not UTF-8 says so.
    #[test]
    fn a_projects_file_that_is_not_utf8_is_refused_as_such() {
        let path = temp_file("projects-not-utf8", b"[\"/\xff\"]");

        let refused = load_projects(&path);
        let _ = fs::remove_file(&path);

        let message = refused.err().unwrap_or_default();
        assert!(message.starts_with("the file is not UTF-8: "), "{message}");
    }

    /// 257 paths are one more than the list may hold.
    #[test]
    fn a_projects_file_listing_257_projects_is_refused() {
        let listed: Vec<String> = (0..=256)
            .map(|index| format!("\"/work/{index}\""))
            .collect();
        let bytes = format!("[{}]", listed.join(","));
        let path = temp_file("projects-count", bytes.as_bytes());

        let refused = load_projects(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(
            refused.err(),
            Some("it lists 257 projects; the most is 256".to_owned())
        );
    }

    /// 256 paths are as many as the list may hold.
    #[test]
    fn a_projects_file_listing_256_projects_is_accepted() {
        let listed: Vec<String> = (0..MAX_PROJECTS)
            .map(|index| format!("\"/work/{index}\""))
            .collect();
        let bytes = format!("[{}]", listed.join(","));
        let path = temp_file("projects-count-exact", bytes.as_bytes());

        let loaded = load_projects(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(
            loaded.map(|projects| projects.len()),
            Ok(MAX_PROJECTS),
            "a list at the limit should load"
        );
    }

    /// An entry that exists is stored as it resolves, so a symlinked
    /// spelling and its target are one project.
    #[test]
    fn a_projects_file_entry_is_canonicalized_when_it_exists() {
        let folder = std::env::temp_dir().join(format!(
            "chauffeur-projects-canonical-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).expect("the project folder");
        let bytes = serde_json::json!([folder.display().to_string()]).to_string();
        let path = temp_file("projects-canonical", bytes.as_bytes());

        let loaded = load_projects(&path);
        let _ = fs::remove_file(&path);
        let canonical = fs::canonicalize(&folder).expect("the canonical path");
        let _ = fs::remove_dir_all(&folder);

        assert_eq!(loaded, Ok(BTreeSet::from([canonical])));
    }

    /// An entry that does not exist yet stays as written, so a project can be
    /// listed before it is cloned.
    #[test]
    fn a_projects_file_entry_that_does_not_exist_stays_as_written() {
        let missing = std::env::temp_dir().join(format!(
            "chauffeur-projects-missing-entry-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&missing);
        assert!(!missing.exists(), "the sample entry should not exist");
        let bytes = serde_json::json!([missing.display().to_string()]).to_string();
        let path = temp_file("projects-missing-entry", bytes.as_bytes());

        let loaded = load_projects(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(loaded, Ok(BTreeSet::from([missing])));
    }

    /// A file that is not there lists no projects.
    #[test]
    fn a_missing_projects_file_lists_none() {
        let path = temporary("projects-missing");
        assert!(!path.exists(), "the temporary path should not exist");

        let loaded = load_projects(&path);

        assert_eq!(loaded, Ok(BTreeSet::new()));
    }
}
