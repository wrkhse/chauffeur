//! A project's `.chauffeur/rules/`: loaded within its own bounds and cached
//! by the folder's signature and the load that made it.
//!
//! A project rule never brings its own models, so one that names a `model`
//! is refused, as is one using the person tier: only shipped and user
//! configuration may ask the user a question. So is one using System Two for
//! a project `projects.json` does not list. A rule whose ID a loaded rule
//! already has is skipped, with a line saying so: a cloned repository cannot
//! replace the user's own rules.
//!
//! A project's folder is untrusted, since a clone brings it. A project that
//! cannot be canonicalized has no rules, and a `.chauffeur/rules/` that
//! resolves outside the project is not read. The walk skips every symlink and
//! stops at [`MAX_ENTRIES`] entries, [`MAX_FOLDERS`] folders, [`MAX_RULES`]
//! rule files or [`MAX_BYTES`] of rule files, recording a refusal; the rules
//! found before the cap still load.
//!
//! An edit that keeps a rule file's length and modification time changes its
//! inode or change time, so the next navigation reloads. An edit within the
//! same change-time granularity that also keeps the inode and the length is
//! not seen until the file changes again.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chauffeur_judges::Tier;
use chauffeur_protocol::Rule;

use crate::state::{Loaded, MAX_RULES, load_rule};

/// The folder a project's rules are read from, below the project's root.
const RULES: &str = ".chauffeur/rules";

/// The most projects whose rules stay cached.
const MAX_CACHED: usize = 32;

/// The most directory entries one project walk examines.
const MAX_ENTRIES: usize = 4_096;

/// The most folders one project walk reads.
const MAX_FOLDERS: usize = 256;

/// The most MiB of rule files one project walk finds.
const MAX_MIB: u64 = 8;

/// The most bytes of rule files one project walk finds.
const MAX_BYTES: u64 = MAX_MIB * 1024 * 1024;

/// What one rule file looked like when it was walked: its length and
/// modification time, and the inode and change time that catch an edit
/// keeping the first two.
#[derive(Clone, PartialEq, Eq)]
struct State {
    len: u64,
    modified: Option<SystemTime>,
    inode: u64,
    changed: i64,
    changed_nsec: i64,
}

/// A project's rule files, each with its state, and why the walk stopped
/// early when it did.
#[derive(Clone, PartialEq, Eq)]
struct Signature {
    files: Vec<(PathBuf, Option<State>)>,
    refused: Option<String>,
}

/// A project's rules and the files refused while loading them.
pub struct Rules {
    /// The rules that run, in ID order.
    pub rules: Vec<Arc<Rule>>,
    /// Each refusal with its path and why.
    pub refused: Vec<String>,
}

impl Rules {
    /// No rules, as a project folder that cannot be read gives.
    const fn empty() -> Self {
        Self {
            rules: Vec::new(),
            refused: Vec::new(),
        }
    }
}

/// The rules of one project: which load they came from, its signature, and
/// what it loaded.
struct Cached {
    epoch: u64,
    signature: Signature,
    rules: Arc<Rules>,
    used: u64,
}

/// Projects whose rules are cached, at most [`MAX_CACHED`], the least
/// recently used replaced first.
struct Entries {
    cached: BTreeMap<PathBuf, Cached>,
    used: u64,
}

/// The per-daemon cache of project rules behind one lock. Every entry
/// records the load it was built from, so a reload's allowance or IDs are
/// never served from an older one.
pub struct Cache {
    entries: Mutex<Entries>,
}

impl Cache {
    /// An empty cache.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(Entries {
                cached: BTreeMap::new(),
                used: 0,
            }),
        }
    }

    /// `project`'s rules, reloaded when its folder's signature changed.
    ///
    /// The project and its rules folder are canonicalized first: a project
    /// that cannot be, or a folder resolving outside it, has no rules. The
    /// walk and the load run outside the lock; only the cache lookup and the
    /// insert take it. Refusals print on every load, and a load happens only
    /// on a cache miss: a changed folder is reported once, though two misses
    /// racing on the same project may both load and both report.
    #[must_use]
    pub fn rules(&self, loaded: &Loaded, project: &Path) -> Arc<Rules> {
        let Some((project, folder)) = rules_folder(project) else {
            return Arc::new(Rules::empty());
        };
        let epoch = loaded.epoch();
        let signature = signature(&folder);
        if let Ok(mut entries) = self.entries.lock()
            && let Some(cached) = entries.get(epoch, &project, &signature)
        {
            return cached;
        }
        let rules = Arc::new(load(loaded, &project, &folder, &signature));
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(epoch, project, signature, Arc::clone(&rules));
        }
        for refused in &rules.refused {
            eprintln!("chauffeur: {refused}");
        }
        rules
    }
}

impl Entries {
    /// The project's rules when they came from `epoch` and its signature is
    /// still `signature`, marked as just used.
    fn get(&mut self, epoch: u64, project: &Path, signature: &Signature) -> Option<Arc<Rules>> {
        if self
            .cached
            .get(project)
            .is_none_or(|cached| cached.epoch != epoch || &cached.signature != signature)
        {
            self.cached.remove(project);
            return None;
        }
        self.used = self.used.saturating_add(1);
        let used = self.used;
        self.cached.get_mut(project).map(|cached| {
            cached.used = used;
            Arc::clone(&cached.rules)
        })
    }

    /// Stores the project's rules as just used, replacing any entry for the
    /// project, and forgets the least recently used when it is one over.
    fn insert(&mut self, epoch: u64, project: PathBuf, signature: Signature, rules: Arc<Rules>) {
        self.used = self.used.saturating_add(1);
        let used = self.used;
        self.cached.insert(
            project,
            Cached {
                epoch,
                signature,
                rules,
                used,
            },
        );
        // An insert adds at most one entry, so one eviction restores the
        // bound.
        if self.cached.len() > MAX_CACHED {
            self.evict();
        }
    }

    /// Forgets the least recently used project.
    fn evict(&mut self) {
        let Some(oldest) = self
            .cached
            .iter()
            .min_by_key(|(_, cached)| cached.used)
            .map(|(project, _)| project.clone())
        else {
            return;
        };
        self.cached.remove(&oldest);
    }
}

/// The project's canonical path and its canonical rules folder, when the
/// folder is inside the project. A project that cannot be canonicalized, or
/// a rules folder that resolves outside it, has no rules.
fn rules_folder(project: &Path) -> Option<(PathBuf, PathBuf)> {
    let project = std::fs::canonicalize(project).ok()?;
    let folder = std::fs::canonicalize(project.join(RULES)).ok()?;
    folder.starts_with(&project).then_some((project, folder))
}

/// The path quoted and escaped for a line on stderr, so a rule file cannot
/// forge one.
fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().escape_debug())
}

/// The folder's rule files and the state that says whether they changed,
/// from one bounded walk that skips symlinks.
fn signature(folder: &Path) -> Signature {
    let shown = quoted(folder);
    let mut files = Vec::new();
    let mut pending = vec![folder.to_owned()];
    let mut folders = 0_usize;
    let mut examined = 0_usize;
    let mut bytes = 0_u64;
    let mut refused = None;
    // Only folders examined as entries are queued, at most `MAX_ENTRIES` of
    // them, so `folders` alone bounds what is read.
    while let Some(next) = pending.pop() {
        if folders >= MAX_FOLDERS {
            refused = Some(format!(
                "{shown}: the project's rules folder is over {MAX_FOLDERS} folders"
            ));
            break;
        }
        folders = folders.saturating_add(1);
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        let mut subfolders = Vec::new();
        for entry in entries.flatten() {
            if examined >= MAX_ENTRIES {
                refused = Some(format!(
                    "{shown}: the project's rules folder is over {MAX_ENTRIES} entries"
                ));
                break;
            }
            examined = examined.saturating_add(1);
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                subfolders.push(path);
            } else if kind.is_file() && is_json(&path) {
                if files.len() >= MAX_RULES {
                    refused = Some(format!(
                        "{shown}: the project's rules folder is over {MAX_RULES} rule files"
                    ));
                    break;
                }
                let metadata = entry.metadata().ok();
                let len = metadata.as_ref().map_or(0, std::fs::Metadata::len);
                if bytes.saturating_add(len) > MAX_BYTES {
                    refused = Some(format!(
                        "{shown}: the project's rules folder is over {MAX_MIB} MiB of rule files"
                    ));
                    break;
                }
                bytes = bytes.saturating_add(len);
                files.push((path, metadata.as_ref().map(State::of)));
            }
        }
        if refused.is_some() {
            break;
        }
        // Popped in name order, so the same folders always load the same IDs.
        subfolders.sort_by(|a, b| b.cmp(a));
        pending.extend(subfolders);
    }
    files.sort_by(|(one, _), (other, _)| one.cmp(other));
    Signature { files, refused }
}

impl State {
    /// The state `metadata` records.
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode: metadata.ino(),
            changed: metadata.ctime(),
            changed_nsec: metadata.ctime_nsec(),
        }
    }
}

/// Whether the file is a rule file.
fn is_json(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "json")
}

/// Loads the project's rules from `folder`, in ID order, refusing the files
/// that may not run and skipping the IDs a loaded rule already has.
fn load(loaded: &Loaded, project: &Path, folder: &Path, signature: &Signature) -> Rules {
    let ids: BTreeSet<String> = loaded
        .rules
        .iter()
        .map(|rule| rule.id().to_owned())
        .collect();
    let listed = loaded.projects().contains(project);
    let mut rules = Vec::new();
    let mut refused: Vec<String> = signature.refused.iter().cloned().collect();
    for (path, _) in &signature.files {
        let rule_id = id(folder, path);
        if ids.contains(&rule_id) {
            refused.push(format!(
                "{}: skipped, a loaded rule already has the ID `{}`",
                quoted(path),
                rule_id.escape_debug()
            ));
            continue;
        }
        match load_rule(folder, path) {
            Ok(rule) => {
                if let Err(reason) = allowed(&rule, listed) {
                    refused.push(format!("{}: {}", quoted(path), reason.escape_debug()));
                } else {
                    rules.push(Arc::new(rule));
                }
            }
            Err(reason) => refused.push(format!("{}: {}", quoted(path), reason.escape_debug())),
        }
    }
    rules.sort_by(|one, other| one.id().cmp(other.id()));
    Rules { rules, refused }
}

/// Why the project rule may not run, if it may not: a judge naming a `model`,
/// since models load only from shipped and user configuration, the person
/// tier, since only shipped and user configuration may ask a person, or
/// System Two for a project `projects.json` does not list. A rule cannot name
/// a placeholder at all: a judge entry holds only a tier, an optional model
/// and a confidence, and refuses every other field.
fn allowed(rule: &Rule, listed: bool) -> Result<(), &'static str> {
    if rule.judges().values().any(|judge| judge.model.is_some()) {
        return Err(
            "a project rule may not name a model: models load only from shipped and user configuration",
        );
    }
    if rule.strategy().uses_tier(Tier::Person) {
        return Err(
            "a project rule may not use the person tier: only shipped and user configuration may ask a person",
        );
    }
    if !listed && rule.strategy().uses_tier(Tier::SystemTwo) {
        return Err("System Two runs only for a project listed in projects.json");
    }
    Ok(())
}

/// The rule's ID: its path below `folder`, without `.json`.
fn id(folder: &Path, path: &Path) -> String {
    path.trim_prefix(folder)
        .with_extension("")
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use chauffeur_protocol::Rule;

    use super::{
        Entries, MAX_BYTES, MAX_CACHED, MAX_ENTRIES, MAX_MIB, MAX_RULES, Rules, Signature, allowed,
        load, quoted, rules_folder, signature,
    };
    use crate::Paths;
    use crate::state::Loaded;

    /// A project path for a test.
    fn project(index: usize) -> PathBuf {
        PathBuf::from(format!("/work/{index}"))
    }

    /// Empty rules, as a project without a `.chauffeur/rules/` loads.
    fn rules() -> Arc<Rules> {
        Arc::new(Rules {
            rules: Vec::new(),
            refused: Vec::new(),
        })
    }

    /// No files, as a folder whose rules did not change gives.
    fn unchanged() -> Signature {
        Signature {
            files: Vec::new(),
            refused: None,
        }
    }

    /// A uniquely named temporary folder, emptied.
    fn temporary(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let path = std::env::temp_dir().join(format!(
            "chauffeur-project-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the temporary folder should be writable");
        path
    }

    /// A load whose shipped rules are `rules`, each a name and its file's
    /// text, in a temporary root returned with it.
    fn fixture(name: &str, rules: &[(&str, &str)]) -> (PathBuf, Loaded) {
        let root = temporary(name);
        let distribution = root.join("distribution");
        fs::create_dir_all(distribution.join("rules")).expect("the shipped rules folder");
        fs::create_dir_all(distribution.join("safety")).expect("the shipped safety folder");
        fs::write(
            distribution.join("safety/redaction.json"),
            r#"{ "prefixes": [] }"#,
        )
        .expect("the redaction file");
        for (id, text) in rules {
            fs::write(distribution.join("rules").join(format!("{id}.json")), text)
                .expect("a shipped rule file");
        }
        let models = root.join("models.json");
        fs::write(&models, r#"{ "models": {}, "tiers": {} }"#).expect("the models file");
        let paths = Paths {
            name: "chauffeur-test".into(),
            socket: root.join("run/daemon.sock"),
            config: root.join("config"),
            distribution,
            state: root.join("state"),
            models: Some(models),
        };
        let loaded = crate::state::load(&paths, None).expect("the fixture loads");

        (root, loaded)
    }

    /// 33 projects fill a cache of 32, which keeps the one used most
    /// recently and forgets the one used least.
    #[test]
    fn past_32_projects_the_least_recently_used_is_forgotten() {
        let mut entries = Entries {
            cached: BTreeMap::new(),
            used: 0,
        };
        for index in 0..MAX_CACHED {
            entries.insert(1, project(index), unchanged(), rules());
        }
        assert_eq!(
            entries.cached.len(),
            MAX_CACHED,
            "the first 32 projects stay"
        );
        assert!(
            entries.get(1, Path::new("/work/0"), &unchanged()).is_some(),
            "the first project is used again"
        );

        entries.insert(1, project(MAX_CACHED), unchanged(), rules());

        assert_eq!(entries.cached.len(), MAX_CACHED);
        assert!(
            entries.get(1, Path::new("/work/0"), &unchanged()).is_some(),
            "the most recently used stays"
        );
        assert!(
            !entries.cached.contains_key(&project(1)),
            "the least recently used is gone"
        );
    }

    /// The same folder signature under another load is a different
    /// configuration, so its entry is not reused.
    #[test]
    fn an_entry_from_another_load_is_not_reused() {
        let mut entries = Entries {
            cached: BTreeMap::new(),
            used: 0,
        };
        entries.insert(1, project(0), unchanged(), rules());

        assert!(
            entries.get(2, Path::new("/work/0"), &unchanged()).is_none(),
            "the signature matches but the load does not"
        );
    }

    /// A rule file that is a symlink outside the project is not walked.
    #[test]
    fn a_symlinked_rule_file_outside_the_project_is_not_read() {
        let root = temporary("symlink-file");
        let project = root.join("project");
        let outside = root.join("outside.json");
        fs::create_dir_all(project.join(".chauffeur/rules")).expect("the rules folder");
        fs::write(&outside, "{}").expect("the outside rule file");
        std::os::unix::fs::symlink(&outside, project.join(".chauffeur/rules/x.json"))
            .expect("the symlink");

        let (_, folder) = rules_folder(&project).expect("the rules folder is inside it");
        let walked = signature(&folder);

        assert!(walked.files.is_empty(), "the symlink was skipped");
        assert_eq!(walked.refused, None);
        let _ = fs::remove_dir_all(&root);
    }

    /// A folder that is a symlink outside the project is not walked.
    #[test]
    fn a_symlinked_folder_outside_the_project_is_not_read() {
        let root = temporary("symlink-folder");
        let project = root.join("project");
        let outside = root.join("outside");
        fs::create_dir_all(project.join(".chauffeur/rules")).expect("the rules folder");
        fs::create_dir_all(&outside).expect("the outside folder");
        fs::write(outside.join("x.json"), "{}").expect("the outside rule file");
        std::os::unix::fs::symlink(&outside, project.join(".chauffeur/rules/linked"))
            .expect("the symlink");

        let (_, folder) = rules_folder(&project).expect("the rules folder is inside it");
        let walked = signature(&folder);

        assert!(walked.files.is_empty(), "the symlinked folder was skipped");
        let _ = fs::remove_dir_all(&root);
    }

    /// A `.chauffeur/rules/` that resolves outside the project has no rules,
    /// even when it holds some.
    #[test]
    fn a_rules_folder_symlinked_outside_the_project_has_no_rules() {
        let root = temporary("symlink-rules");
        let project = root.join("project");
        let outside = root.join("outside");
        fs::create_dir_all(project.join(".chauffeur")).expect("the project's folder");
        fs::create_dir_all(&outside).expect("the outside folder");
        fs::write(outside.join("x.json"), "{}").expect("the outside rule file");
        std::os::unix::fs::symlink(&outside, project.join(".chauffeur/rules"))
            .expect("the symlink");

        assert!(
            rules_folder(&project).is_none(),
            "the rules folder resolves outside the project"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A project that is not there has no rules.
    #[test]
    fn a_project_that_cannot_be_canonicalized_has_no_rules() {
        let root = temporary("missing");

        assert!(rules_folder(&root.join("not-there")).is_none());
        let _ = fs::remove_dir_all(&root);
    }

    /// 4,097 entries are one more than a walk examines, even when none is a
    /// rule file.
    #[test]
    fn a_rules_folder_over_the_entry_bound_is_refused() {
        let folder = temporary("entries");
        for index in 0..=MAX_ENTRIES {
            fs::write(folder.join(format!("file-{index}")), b"").expect("an empty file");
        }

        let walked = signature(&folder);

        assert!(walked.files.is_empty(), "none is a rule file");
        let refused = walked.refused.expect("the walk stopped at its bound");
        assert!(
            refused.contains(&format!("over {MAX_ENTRIES} entries")),
            "{refused}"
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// Exactly the rule-file bound is taken, not refused: 512 tiny rule
    /// files all load.
    #[test]
    fn a_rules_folder_at_the_rule_file_bound_is_taken() {
        let folder = temporary("rule-files-at-bound");
        for index in 0..MAX_RULES {
            fs::write(folder.join(format!("{index}.json")), b"{}").expect("a tiny rule file");
        }

        let walked = signature(&folder);

        assert_eq!(walked.refused, None, "the folder is at its rule-file bound");
        assert_eq!(
            walked.files.len(),
            MAX_RULES,
            "every file at the bound is taken"
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A rule file one over the bound is refused before it is read, and the
    /// 512 found before it are still taken.
    #[test]
    fn a_rules_folder_over_the_rule_file_bound_is_refused() {
        let folder = temporary("rule-files");
        for index in 0..=MAX_RULES {
            fs::write(folder.join(format!("{index}.json")), b"{}").expect("a tiny rule file");
        }

        let walked = signature(&folder);

        assert_eq!(
            walked.files.len(),
            MAX_RULES,
            "the files before the cap are taken"
        );
        let refused = walked.refused.expect("the walk stopped at its bound");
        assert!(
            refused.contains(&format!("over {MAX_RULES} rule files")),
            "{refused}"
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A path is shown wrapped in quotes with the quote and newline inside
    /// it escaped, so a rule file's name cannot forge a line on stderr.
    #[test]
    fn a_quoted_path_escapes_a_quote_and_a_newline() {
        assert_eq!(
            quoted(Path::new("a\"b\nc.json")),
            "\"a\\\"b\\nc.json\"",
            "the path is wrapped and both characters are escaped"
        );
    }

    /// Exactly the byte bound of rule files is taken, not refused: eight
    /// files of 1 MiB each, the most a rule file may hold, reach 8 MiB with
    /// nothing over it.
    #[test]
    fn a_rules_folder_at_the_size_bound_is_taken() {
        let folder = temporary("bytes-at-bound");
        // The bound and the per-file cap as the docs promise them, written
        // out rather than read from `MAX_BYTES`, so a change to the
        // constant's arithmetic fails here instead of moving the test.
        let per_file = 1024 * 1024;
        let bound = 8 * 1024 * 1024;
        for index in 0..bound / per_file {
            fs::write(folder.join(format!("{index}.json")), vec![b' '; per_file])
                .expect("a rule file at the per-file cap");
        }

        let walked = signature(&folder);

        assert_eq!(walked.refused, None, "the folder is at its byte bound");
        assert_eq!(
            walked.files.len(),
            bound / per_file,
            "every file at the bound is taken"
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A rule file over the byte bound is refused before it is read.
    #[test]
    fn a_rules_folder_over_the_size_bound_is_refused() {
        let folder = temporary("bytes");
        let over = usize::try_from(MAX_BYTES.saturating_add(1)).unwrap_or(0);
        fs::write(folder.join("big.json"), vec![b' '; over]).expect("the oversized file");

        let walked = signature(&folder);

        assert!(walked.files.is_empty(), "the file was not taken");
        let refused = walked.refused.expect("the walk stopped at its bound");
        assert!(
            refused.contains(&format!("over {MAX_MIB} MiB of rule files")),
            "{refused}"
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A project rule file whose ID a loaded rule already has is skipped
    /// with a line saying so, and the file is never parsed.
    #[test]
    fn a_project_rule_with_a_loaded_rules_id_is_skipped_with_a_line() {
        let (root, loaded) = fixture(
            "collision",
            &[(
                "taken",
                r#"{"on":"x","strategy":[{"id":"s","judge":"system-one","ask":{"outcomes":["yes","no"],"instructions":"?"}}]}"#,
            )],
        );
        let project = root.join("project");
        fs::create_dir_all(project.join(".chauffeur/rules")).expect("the rules folder");
        fs::write(project.join(".chauffeur/rules/taken.json"), "{}").expect("the colliding rule");
        let (project, folder) = rules_folder(&project).expect("the rules folder is inside it");
        let walked = signature(&folder);

        let rules = load(&loaded, &project, &folder, &walked);

        assert!(rules.rules.is_empty(), "the colliding rule does not run");
        assert_eq!(
            rules.refused,
            [format!(
                "{}: skipped, a loaded rule already has the ID `taken`",
                quoted(&folder.join("taken.json"))
            )],
            "the file is reported as skipped, not parsed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A project rule using the person tier is refused, since only shipped
    /// and user configuration may ask a person.
    #[test]
    fn a_project_rule_using_the_person_tier_is_refused() {
        let rule = Rule::parse(
            "ask",
            r#"{"on":"x","strategy":[{"id":"ask","judge":"person","ask":{"outcomes":["yes","no"],"instructions":"?"}}]}"#,
        )
        .expect("the rule parses");

        assert_eq!(
            allowed(&rule, false),
            Err(
                "a project rule may not use the person tier: only shipped and user configuration may ask a person"
            )
        );
    }
}
