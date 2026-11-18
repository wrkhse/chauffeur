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
//! A project's folder is untrusted, since a clone brings it. The navigation
//! resolves the project once, and a `.chauffeur/rules/` that resolves outside
//! it is not read. The walk skips every symlink and opens each queued folder,
//! reading it only while its device and inode are the ones it recorded, and
//! checks the path again after the listing. It stops at [`MAX_ENTRIES`]
//! entries, [`MAX_FOLDERS`] folders, [`MAX_RULES`] rule files or
//! [`MAX_BYTES`] of rule files, recording a refusal; the rules found before
//! the cap still load.
//!
//! A rule file is opened and its device and inode compared with the ones the
//! walk recorded, and its bytes are read from that handle. An entry whose
//! metadata cannot be read is refused with its path and the error, so a rule
//! file is never read without the identity that read checks. Standard library
//! cannot list a folder through an open handle, so a same-user process that
//! swaps a folder and swaps it back during one walk is a residual race:
//! closing it needs an fd-relative listing.
//!
//! An edit that keeps a rule file's length and modification time changes its
//! inode or change time, so the next navigation reloads. An edit within the
//! same change-time granularity that also keeps the inode and the length is
//! not seen until the file changes again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, Metadata};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use chauffeur_judges::Tier;
use chauffeur_protocol::Rule;

use crate::state::{FileId, Loaded, MAX_RULES, load_rule};

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

/// What one rule file looked like when it was walked: the device and inode
/// that name it, its length and modification time, and the change time that
/// catches an edit keeping the first two.
#[derive(Clone, PartialEq, Eq)]
struct State {
    id: FileId,
    len: u64,
    modified: Option<SystemTime>,
    changed: i64,
    changed_nsec: i64,
}

/// A project's rule files, each with its state, and why the walk stopped
/// early when it did.
#[derive(Clone, PartialEq, Eq)]
struct Signature {
    files: Vec<(PathBuf, State)>,
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
    /// `project` is the canonical path the navigation resolved, and its rules
    /// folder is canonicalized: a folder resolving outside the project has no
    /// rules. The walk and the load run outside the lock; only the cache
    /// lookup and the insert take it. Refusals print on every load, and a
    /// load happens only on a cache miss: a changed folder is reported once,
    /// though two misses racing on the same project may both load and both
    /// report.
    #[must_use]
    pub fn rules(&self, loaded: &Loaded, project: &Path) -> Arc<Rules> {
        let Some(folder) = rules_folder(project) else {
            return Arc::new(Rules::empty());
        };
        let epoch = loaded.epoch();
        let signature = signature(&folder);
        if let Ok(mut entries) = self.entries.lock()
            && let Some(cached) = entries.get(epoch, project, &signature)
        {
            return cached;
        }
        let rules = Arc::new(load(loaded, project, &folder, &signature));
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(epoch, project.to_path_buf(), signature, Arc::clone(&rules));
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

/// The project's canonical rules folder, when it is inside the project.
/// `project` is the canonical path the navigation resolved; only the folder
/// is canonicalized, so one that resolves outside the project is not read.
fn rules_folder(project: &Path) -> Option<PathBuf> {
    let folder = std::fs::canonicalize(project.join(RULES)).ok()?;
    folder.starts_with(project).then_some(folder)
}

/// The path quoted and escaped for a message, so a project's folder or a
/// rule file cannot forge a line.
pub fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.to_string_lossy().escape_debug())
}

/// A test's swap for a folder: the path it receives says which queued
/// folder to replace, just before that folder is listed.
#[cfg(test)]
type BeforeListing = std::cell::RefCell<Option<Box<dyn Fn(&Path)>>>;

#[cfg(test)]
std::thread_local! {
    /// A test's swap, run just before a folder is read, so the test can
    /// replace a queued folder after the walk checked it and before its
    /// listing.
    static BEFORE_LISTING: BeforeListing = const { std::cell::RefCell::new(None) };
}

/// Runs a test's swap for `path`, when one is set. A production walk
/// compiles without this, so no test seam reaches it.
#[cfg(test)]
fn before_listing(path: &Path) {
    BEFORE_LISTING.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook(path);
        }
    });
}

/// The folder's rule files and the state that says whether they changed,
/// from one bounded walk that skips symlinks, opens each queued folder,
/// checks its device and inode before and after the listing, and reads each
/// rule file from the handle it checked.
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
        let Some(checked) = inside(&next, folder) else {
            refused = Some(format!(
                "{shown}: {} is not a folder inside the project",
                quoted(&next)
            ));
            break;
        };
        let Some(opened) = open_folder(&next, &checked) else {
            refused = Some(format!(
                "{shown}: {} is not the folder the walk checked",
                quoted(&next)
            ));
            break;
        };
        // A test can replace the folder here, after its identity was
        // checked and before it is read.
        #[cfg(test)]
        before_listing(&next);
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        let files_before = files.len();
        let listed = read_folder(entries, &mut files, &mut bytes, &mut examined, &shown);
        // The handle stays open across the listing, so its inode cannot be
        // reused while the path is checked again.
        let unchanged = still_same(&next, &checked);
        drop(opened);
        if !unchanged {
            files.truncate(files_before);
            refused = Some(format!(
                "{shown}: {} changed while it was listed",
                quoted(&next)
            ));
            break;
        }
        match listed {
            Ok(mut subfolders) => {
                // Popped in name order, so the same folders always load the
                // same IDs.
                subfolders.sort_by(|a, b| b.cmp(a));
                pending.extend(subfolders);
            }
            Err(reason) => {
                refused = Some(reason);
                break;
            }
        }
    }
    files.sort_by(|(one, _), (other, _)| one.cmp(other));
    Signature { files, refused }
}

/// Opens `path` and returns the handle only when it is the folder
/// `checked` describes. Standard library cannot list a folder through an
/// open handle, so a same-user process that swaps a folder and swaps it back
/// during one walk is a residual race: `read_dir` sees whatever folder is at
/// the path then. Closing it needs an fd-relative listing.
fn open_folder(path: &Path, checked: &Metadata) -> Option<File> {
    let opened = File::open(path).ok()?;

    same_file(&opened.metadata().ok()?, checked).then_some(opened)
}

/// Whether both metadata name the same file: the same device and inode.
fn same_file(one: &Metadata, other: &Metadata) -> bool {
    one.dev() == other.dev() && one.ino() == other.ino()
}

/// Records the rule file `path` with the metadata the walk read, or says
/// which bound it is over. An entry whose metadata cannot be read is
/// refused with its path and the error, so no rule file is recorded without
/// the identity its read checks.
fn record_file(
    path: PathBuf,
    metadata: io::Result<Metadata>,
    files: &mut Vec<(PathBuf, State)>,
    bytes: &mut u64,
    shown: &str,
) -> Result<(), String> {
    let metadata = metadata.map_err(|error| format!("{}: {error}", quoted(&path)))?;
    if files.len() >= MAX_RULES {
        return Err(format!(
            "{shown}: the project's rules folder is over {MAX_RULES} rule files"
        ));
    }
    let len = metadata.len();
    if bytes.saturating_add(len) > MAX_BYTES {
        return Err(format!(
            "{shown}: the project's rules folder is over {MAX_MIB} MiB of rule files"
        ));
    }
    *bytes = bytes.saturating_add(len);
    files.push((path, State::of(&metadata)));

    Ok(())
}

/// Reads `entries` and returns the folders among them, recording the rule
/// files, or says which bound it is over or which entry's metadata could not
/// be read. `examined` is the walk's count of entries, so it stays bounded
/// across folders.
fn read_folder(
    entries: std::fs::ReadDir,
    files: &mut Vec<(PathBuf, State)>,
    bytes: &mut u64,
    examined: &mut usize,
    shown: &str,
) -> Result<Vec<PathBuf>, String> {
    let mut subfolders = Vec::new();
    for entry in entries.flatten() {
        if *examined >= MAX_ENTRIES {
            return Err(format!(
                "{shown}: the project's rules folder is over {MAX_ENTRIES} entries"
            ));
        }
        *examined = examined.saturating_add(1);
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
            record_file(path, entry.metadata(), files, bytes, shown)?;
        }
    }

    Ok(subfolders)
}

/// The metadata of `path` while it is a folder inside `folder`: the walk
/// reads a folder it queued only while it is not a symlink, still a folder
/// and still resolves inside the project's rules.
fn inside(path: &Path, folder: &Path) -> Option<Metadata> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return None;
    }

    std::fs::canonicalize(path)
        .is_ok_and(|path| path.starts_with(folder))
        .then_some(metadata)
}

/// Whether `path` still names the folder `checked` describes. A folder
/// swapped while it was listed is refused, and a swap back before this check
/// is the residual race the open's comment names.
fn still_same(path: &Path, checked: &Metadata) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|now| same_file(&now, checked))
}

impl State {
    /// The state `metadata` records.
    fn of(metadata: &Metadata) -> Self {
        Self {
            id: FileId::of(metadata),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            changed: metadata.ctime(),
            changed_nsec: metadata.ctime_nsec(),
        }
    }

    /// The device and inode the file had when the walk saw it.
    const fn identity(&self) -> FileId {
        self.id
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
    for (path, state) in &signature.files {
        let rule_id = id(folder, path);
        if ids.contains(&rule_id) {
            refused.push(format!(
                "{}: skipped, a loaded rule already has the ID `{}`",
                quoted(path),
                rule_id.escape_debug()
            ));
            continue;
        }
        match load_rule(folder, path, Some(state.identity())) {
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
        BEFORE_LISTING, Entries, MAX_BYTES, MAX_CACHED, MAX_ENTRIES, MAX_MIB, MAX_RULES, Rules,
        Signature, allowed, inside, load, open_folder, quoted, record_file, rules_folder,
        signature, still_same,
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

    /// A uniquely named temporary folder, emptied, as the filesystem
    /// resolves it.
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
        fs::canonicalize(&path).expect("the temporary folder should resolve")
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

    /// A valid rule, for a project rule file.
    const RULE: &str = concat!(
        r#"{"on":"x","strategy":[{"id":"s","judge":"system-one","#,
        r#""ask":{"outcomes":["yes","no"],"instructions":"?"}}]}"#
    );

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

        let folder = rules_folder(&project).expect("the rules folder is inside it");
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

        let folder = rules_folder(&project).expect("the rules folder is inside it");
        let walked = signature(&folder);

        assert!(walked.files.is_empty(), "the symlinked folder was skipped");
        let _ = fs::remove_dir_all(&root);
    }

    /// A folder the walk queued is checked again before it is read: one that
    /// is a symlink by then is refused, not followed.
    #[test]
    fn a_folder_that_is_a_symlink_when_it_is_read_is_refused() {
        let root = temporary("swapped");
        let real = root.join("real");
        fs::create_dir_all(&real).expect("the real rules folder");
        fs::write(real.join("x.json"), "{}").expect("a rule file");
        let link = root.join("rules");
        std::os::unix::fs::symlink(&real, &link).expect("the symlink");

        let walked = signature(&link);

        assert!(walked.files.is_empty(), "the symlinked folder was not read");
        assert_eq!(
            walked.refused,
            Some(format!(
                "{}: {} is not a folder inside the project",
                quoted(&link),
                quoted(&link)
            ))
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A queued path is inside only as a real folder inside the rules: one
    /// outside is not, and neither is a symlink, even to one, or a plain
    /// file.
    #[test]
    fn a_queued_path_is_inside_only_as_a_folder_inside_the_rules() {
        let root = temporary("inside");
        let folder = root.join("rules");
        let outside = root.join("outside");
        let file = folder.join("x.json");
        fs::create_dir_all(folder.join("sub")).expect("a folder inside the rules");
        fs::create_dir_all(&outside).expect("the outside folder");
        fs::write(&file, "{}").expect("a plain file inside the rules");
        let link = folder.join("linked");
        std::os::unix::fs::symlink(&outside, &link).expect("the symlink");

        assert!(
            inside(&folder.join("sub"), &folder).is_some(),
            "a folder inside the rules is inside"
        );
        assert!(
            inside(&outside, &folder).is_none(),
            "a folder outside the rules is not inside"
        );
        assert!(
            inside(&link, &folder).is_none(),
            "a symlink to an outside folder is not inside"
        );
        assert!(
            inside(&file, &folder).is_none(),
            "a plain file inside the rules is not inside"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A folder opened for its listing is the one checked only while its
    /// device and inode are the same; one replaced in between is not.
    #[test]
    fn a_folder_replaced_between_the_check_and_the_open_is_not_the_same() {
        let root = temporary("reopened");
        let path = root.join("rules");
        fs::create_dir_all(&path).expect("the rules folder");
        let checked = fs::symlink_metadata(&path).expect("the folder's metadata");
        assert!(
            open_folder(&path, &checked).is_some(),
            "the folder is the one checked"
        );
        assert!(
            still_same(&path, &checked),
            "the path still names the folder checked"
        );

        fs::rename(&path, root.join("moved")).expect("the folder moves away");
        fs::create_dir_all(&path).expect("another folder at the path");

        assert!(
            open_folder(&path, &checked).is_none(),
            "another folder at the path is not the one checked"
        );
        assert!(
            !still_same(&path, &checked),
            "the path no longer names the folder checked"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A folder swapped for a symlink after it was queued still resolves to
    /// the folder checked, so the open takes it; the path after the listing
    /// is not that folder, so the walk refuses it.
    #[test]
    fn a_folder_swapped_for_a_symlink_is_refused_after_the_listing() {
        let root = temporary("opened-link");
        let real = root.join("real");
        fs::create_dir_all(&real).expect("the real rules folder");
        let checked = fs::symlink_metadata(&real).expect("the folder's metadata");
        let link = root.join("rules");
        std::os::unix::fs::symlink(&real, &link).expect("the symlink");

        assert!(
            open_folder(&link, &checked).is_some(),
            "the link resolves to the folder checked"
        );
        assert!(
            !still_same(&link, &checked),
            "the path is not the folder checked after the listing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A queued folder that changes between the open's check and its
    /// listing is refused, and the files recorded from it are dropped.
    #[test]
    fn a_folder_swapped_before_its_listing_is_refused_and_its_files_dropped() {
        let root = temporary("swap-listing");
        let folder = root.join("rules");
        let queued = folder.join("sub");
        let kept = folder.join("kept.json");
        fs::create_dir_all(&queued).expect("the queued folder");
        fs::write(&kept, "{}").expect("the rule file before the swap");
        let moved = folder.join("moved");
        let swapped = queued.join("swapped.json");
        let swap: Box<dyn Fn(&Path)> = {
            let queued = queued.clone();
            Box::new(move |path: &Path| {
                if path == queued.as_path() {
                    fs::rename(&queued, &moved).expect("the queued folder moves away");
                    fs::create_dir_all(&queued).expect("another folder at its path");
                    fs::write(&swapped, "{}").expect("the swapped folder's rule file");
                }
            })
        };
        BEFORE_LISTING.with(|hook| *hook.borrow_mut() = Some(swap));

        let walked = signature(&folder);

        BEFORE_LISTING.with(|hook| *hook.borrow_mut() = None);
        let paths: Vec<&Path> = walked
            .files
            .iter()
            .map(|(path, _)| path.as_path())
            .collect();
        assert_eq!(
            paths,
            [kept.as_path()],
            "the file recorded from the swapped folder is dropped"
        );
        assert_eq!(
            walked.refused,
            Some(format!(
                "{}: {} changed while it was listed",
                quoted(&folder),
                quoted(&queued)
            )),
            "the queued folder is refused, not read"
        );
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

    /// A project that is not there has no rules folder.
    #[test]
    fn a_project_that_is_not_there_has_no_rules_folder() {
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
        assert_eq!(
            walked.refused,
            Some(format!(
                "{}: the project's rules folder is over {MAX_ENTRIES} entries",
                quoted(&folder)
            ))
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A rule file whose metadata cannot be read is refused with its path
    /// and the error, so it is never recorded without the identity its read
    /// checks.
    #[test]
    fn a_rule_file_whose_metadata_cannot_be_read_is_refused() {
        let folder = Path::new("/work/rules");
        let path = folder.join("x.json");
        let shown = quoted(folder);
        let expected = format!("{}: denied", quoted(&path));
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let mut files = Vec::new();
        let mut bytes = 0;

        let refused = record_file(path, Err(error), &mut files, &mut bytes, &shown);

        assert_eq!(refused, Err(expected), "the entry and the error are named");
        assert!(files.is_empty(), "no file is recorded without its metadata");
        assert_eq!(bytes, 0, "nothing counts toward the folder's bytes");
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
        assert_eq!(
            walked.refused,
            Some(format!(
                "{}: the project's rules folder is over {MAX_RULES} rule files",
                quoted(&folder)
            ))
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A path is shown wrapped in quotes with the quote and newline inside
    /// it escaped, so a project's folder or a rule file's name cannot forge a
    /// line.
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
        assert_eq!(
            walked.refused,
            Some(format!(
                "{}: the project's rules folder is over {MAX_MIB} MiB of rule files",
                quoted(&folder)
            ))
        );
        let _ = fs::remove_dir_all(&folder);
    }

    /// A project rule file whose ID a loaded rule already has is skipped
    /// with a line saying so, and the file is never parsed.
    #[test]
    fn a_project_rule_with_a_loaded_rules_id_is_skipped_with_a_line() {
        let (root, loaded) = fixture("collision", &[("taken", RULE)]);
        let project = root.join("project");
        fs::create_dir_all(project.join(".chauffeur/rules")).expect("the rules folder");
        fs::write(project.join(".chauffeur/rules/taken.json"), "{}").expect("the colliding rule");
        let folder = rules_folder(&project).expect("the rules folder is inside it");
        let project = fs::canonicalize(&project).expect("the project resolves");
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

    /// A rule file replaced between the walk and the read is refused, so a
    /// path cannot substitute another file after the walk saw it.
    #[test]
    fn a_rule_file_replaced_after_the_walk_is_refused() {
        let (root, loaded) = fixture("replaced-file", &[]);
        let project = root.join("project");
        fs::create_dir_all(project.join(".chauffeur/rules")).expect("the rules folder");
        let path = project.join(".chauffeur/rules/x.json");
        fs::write(&path, RULE).expect("a rule file");
        let folder = rules_folder(&project).expect("the rules folder is inside it");
        let project = fs::canonicalize(&project).expect("the project resolves");
        let walked = signature(&folder);
        assert_eq!(walked.files.len(), 1, "the walk found the rule file");

        // A same-user process replaces the file after the walk saw it.
        let replacement = root.join("replacement.json");
        fs::write(&replacement, RULE).expect("the replacement rule file");
        fs::rename(&replacement, &path).expect("the rule is replaced");

        let loaded_rules = load(&loaded, &project, &folder, &walked);

        assert!(
            loaded_rules.rules.is_empty(),
            "the replacement does not run"
        );
        assert_eq!(
            loaded_rules.refused,
            [format!(
                "{}: the rule file is not the one the walk recorded",
                quoted(&path)
            )]
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
