// Creating, renaming and deleting files, as a language server's edit asks
// -- all of it, or none of it.
//
// Two halves, so that every refusal comes before anything is touched:
// `plan` works the operations out against the files as they will be at
// each step and refuses what cannot be done, and `commit` carries a plan
// out, taking back the steps it has already taken the moment one fails.
//
// A file an edit deletes is not deleted until `Done::finish`. It is
// moved aside, next to where it was, so that taking a delete back is a
// rename -- and the rest of the edit, its text, can still fail without
// having lost the file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// One file operation, its URIs already turned into paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Create { path: PathBuf, overwrite: bool, ignore_if_exists: bool },
    Rename { from: PathBuf, to: PathBuf, overwrite: bool, ignore_if_exists: bool },
    Delete { path: PathBuf, recursive: bool, ignore_if_not_exists: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Create(PathBuf),
    Rename(PathBuf, PathBuf),
    Delete(PathBuf),
}

// An action, with how many of the edit's text changes come before it --
// see `lsp::WorkspaceEdit::operations`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    position: usize,
    action: Action,
}

/// What an edit will do to the files, every check already made.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    steps: Vec<Step>,
}

impl Plan {
    /// Where the file a text change names ends up once the plan has run:
    /// followed through every rename that comes after the change, or
    /// `None` when a delete after it takes the file away. Renames before
    /// it are not followed -- a change after a rename names the file by
    /// its new name already.
    pub fn destination(&self, position: usize, path: &Path) -> Option<PathBuf> {
        let mut path = path.to_path_buf();
        for step in self.steps.iter().filter(|s| s.position > position) {
            match &step.action {
                Action::Rename(from, to) => path = moved(&path, from, to),
                Action::Delete(gone) if path.starts_with(gone) => return None,
                _ => {}
            }
        }
        Some(path)
    }

    /// Where an open buffer's file is once the plan has run: every rename
    /// followed, whenever it comes.
    pub fn follow(&self, path: &Path) -> PathBuf {
        self.steps.iter().fold(path.to_path_buf(), |path, step| match &step.action {
            Action::Rename(from, to) => moved(&path, from, to),
            _ => path,
        })
    }
}

// `path` after `from` became `to`: the file itself, or one inside a
// directory that moved. Anything else is where it was.
fn moved(path: &Path, from: &Path, to: &Path) -> PathBuf {
    match path.strip_prefix(from) {
        Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
        Ok(rest) => to.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

// The files as the plan so far will have left them, answered from the
// changes planned and, past those, from the disk.
#[derive(Default)]
struct Model {
    changes: Vec<Action>,
}

enum Found {
    Missing,
    Created,
    // Where it is on disk now, which a planned rename may have changed.
    OnDisk(PathBuf),
}

impl Model {
    fn find(&self, path: &Path) -> Found {
        let mut path = path.to_path_buf();
        for change in self.changes.iter().rev() {
            match change {
                Action::Create(made) if path == *made => return Found::Created,
                Action::Delete(gone) if path.starts_with(gone) => return Found::Missing,
                Action::Rename(from, to) if path.starts_with(to) => path = moved(&path, to, from),
                Action::Rename(from, _) if path.starts_with(from) => return Found::Missing,
                _ => {}
            }
        }
        Found::OnDisk(path)
    }

    fn exists(&self, path: &Path) -> bool {
        match self.find(path) {
            Found::Missing => false,
            Found::Created => true,
            Found::OnDisk(at) => at.symlink_metadata().is_ok(),
        }
    }

    fn is_directory_with_files(&self, path: &Path) -> bool {
        match self.find(path) {
            Found::OnDisk(at) => {
                at.symlink_metadata().is_ok_and(|m| m.is_dir()) && std::fs::read_dir(&at).is_ok_and(|mut entries| entries.next().is_some())
            }
            _ => false,
        }
    }
}

/// Works `operations` out, in order, and refuses the first one that
/// cannot be done -- before any of them is.
///
/// `unsaved` is every file open with changes nobody has saved. Deleting
/// one, or a directory holding one, is refused: the edit would take away
/// the file those changes were meant for. A rename earlier in the same
/// edit is followed, so moving a file and then deleting it is caught too.
pub fn plan(operations: &[(usize, Operation)], unsaved: &[PathBuf]) -> Result<Plan, String> {
    let mut model = Model::default();
    let mut unsaved = unsaved.to_vec();
    let mut steps = Vec::new();
    let take = |model: &mut Model, steps: &mut Vec<Step>, position: usize, action: Action| {
        model.changes.push(action.clone());
        steps.push(Step { position, action });
    };
    let refuse_unsaved = |unsaved: &[PathBuf], path: &Path| match unsaved.iter().find(|open| open.starts_with(path)) {
        Some(open) => Err(format!("{} has unsaved changes", open.display())),
        None => Ok(()),
    };
    for (position, operation) in operations {
        let position = *position;
        match operation {
            Operation::Create { path, overwrite, ignore_if_exists } => {
                if model.exists(path) {
                    // `overwrite` wins over `ignoreIfExists`, as the spec
                    // says it does.
                    if !overwrite {
                        if *ignore_if_exists {
                            continue;
                        }
                        return Err(format!("{} already exists", path.display()));
                    }
                    refuse_unsaved(&unsaved, path)?;
                    take(&mut model, &mut steps, position, Action::Delete(path.clone()));
                }
                take(&mut model, &mut steps, position, Action::Create(path.clone()));
            }
            Operation::Rename { from, to, overwrite, ignore_if_exists } => {
                if !model.exists(from) {
                    return Err(format!("{} does not exist", from.display()));
                }
                if model.exists(to) {
                    if !overwrite {
                        if *ignore_if_exists {
                            continue;
                        }
                        return Err(format!("{} already exists", to.display()));
                    }
                    refuse_unsaved(&unsaved, to)?;
                    take(&mut model, &mut steps, position, Action::Delete(to.clone()));
                }
                take(&mut model, &mut steps, position, Action::Rename(from.clone(), to.clone()));
                unsaved = unsaved.iter().map(|open| moved(open, from, to)).collect();
            }
            Operation::Delete { path, recursive, ignore_if_not_exists } => {
                if !model.exists(path) {
                    if *ignore_if_not_exists {
                        continue;
                    }
                    return Err(format!("{} does not exist", path.display()));
                }
                if !recursive && model.is_directory_with_files(path) {
                    return Err(format!("{} is a directory with files in it", path.display()));
                }
                refuse_unsaved(&unsaved, path)?;
                take(&mut model, &mut steps, position, Action::Delete(path.clone()));
            }
        }
    }
    Ok(Plan { steps })
}

// What undoes one thing `commit` did.
enum Undo {
    RemoveFile(PathBuf),
    RemoveDirectory(PathBuf),
    Rename(PathBuf, PathBuf),
}

/// A plan carried out, and still possible to take back.
#[derive(Default)]
pub struct Done {
    undo: Vec<Undo>,
    aside: Vec<PathBuf>,
}

/// Carries `plan` out. The first step that fails takes back every step
/// before it, and says what failed.
pub fn commit(plan: &Plan) -> Result<Done, String> {
    let mut done = Done::default();
    for step in &plan.steps {
        if let Err(why) = done.take(&step.action) {
            done.undo();
            return Err(why);
        }
    }
    Ok(done)
}

impl Done {
    fn take(&mut self, action: &Action) -> Result<(), String> {
        let failed = |path: &Path, e: std::io::Error| format!("{}: {}", path.display(), crate::exec::os_message(&e));
        match action {
            Action::Create(path) => {
                self.parents(path)?;
                std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| failed(path, e))?;
                self.undo.push(Undo::RemoveFile(path.clone()));
            }
            Action::Rename(from, to) => {
                self.parents(to)?;
                std::fs::rename(from, to).map_err(|e| failed(from, e))?;
                self.undo.push(Undo::Rename(to.clone(), from.clone()));
            }
            Action::Delete(path) => {
                let aside = aside_for(path);
                std::fs::rename(path, &aside).map_err(|e| failed(path, e))?;
                self.undo.push(Undo::Rename(aside.clone(), path.clone()));
                self.aside.push(aside);
            }
        }
        Ok(())
    }

    // The directories a step needs and does not have, made and
    // remembered, so taking the step back takes them away too.
    fn parents(&mut self, path: &Path) -> Result<(), String> {
        let mut missing = Vec::new();
        let mut at = path.parent();
        while let Some(dir) = at.filter(|d| !d.as_os_str().is_empty() && d.symlink_metadata().is_err()) {
            missing.push(dir.to_path_buf());
            at = dir.parent();
        }
        for dir in missing.into_iter().rev() {
            std::fs::create_dir(&dir).map_err(|e| format!("{}: {}", dir.display(), crate::exec::os_message(&e)))?;
            self.undo.push(Undo::RemoveDirectory(dir));
        }
        Ok(())
    }

    /// Takes back everything done, last first. As far as it can: one
    /// step that will not go back does not stop the steps before it.
    pub fn undo(&mut self) {
        for undo in self.undo.drain(..).rev() {
            let _ = match undo {
                Undo::RemoveFile(path) => std::fs::remove_file(path),
                Undo::RemoveDirectory(path) => std::fs::remove_dir(path),
                Undo::Rename(from, to) => std::fs::rename(from, to),
            };
        }
        self.aside.clear();
    }

    /// The whole edit went through: what it deleted is deleted for good.
    pub fn finish(self) {
        for aside in self.aside {
            let _ = match aside.symlink_metadata() {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&aside),
                _ => std::fs::remove_file(&aside),
            };
        }
    }
}

// A name next to `path`, in the same directory and so on the same file
// system -- a rename there cannot fail for crossing one -- that nothing
// else will be using.
fn aside_for(path: &Path) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.bish-deleted-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bish-fileops-{}-{tag}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn a_plan_runs_in_order_and_a_delete_is_final_only_once_finished() {
        let dir = scratch("in-order");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye").unwrap();
        let plan = plan(
            &[
                (0, Operation::Create { path: dir.join("new.txt"), overwrite: false, ignore_if_exists: false }),
                (0, Operation::Rename { from: dir.join("a.txt"), to: dir.join("sub/b.txt"), overwrite: false, ignore_if_exists: false }),
                (1, Operation::Delete { path: dir.join("gone.txt"), recursive: false, ignore_if_not_exists: false }),
            ],
            &[],
        )
        .unwrap();
        let done = commit(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("sub/b.txt")).unwrap(), "a", "renamed into a directory made for it");
        assert!(dir.join("new.txt").exists() && !dir.join("a.txt").exists() && !dir.join("gone.txt").exists());
        assert!(names(&dir).iter().any(|n| n.starts_with(".gone.txt.bish-deleted-")), "deleted, but not yet for good: {:?}", names(&dir));
        done.finish();
        assert_eq!(names(&dir), ["new.txt", "sub"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_step_that_fails_takes_back_every_step_before_it() {
        let dir = scratch("undo");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye").unwrap();
        // A file where the rename wants a directory: nothing a plan can
        // see, and exactly the failure `commit` has to take back after.
        std::fs::write(dir.join("blocker"), "").unwrap();
        let plan = plan(
            &[
                (0, Operation::Create { path: dir.join("made/new.txt"), overwrite: false, ignore_if_exists: false }),
                (0, Operation::Delete { path: dir.join("gone.txt"), recursive: false, ignore_if_not_exists: false }),
                (0, Operation::Rename { from: dir.join("a.txt"), to: dir.join("blocker/b.txt"), overwrite: false, ignore_if_exists: false }),
            ],
            &[],
        )
        .unwrap();
        assert!(commit(&plan).is_err());
        assert_eq!(names(&dir), ["a.txt", "blocker", "gone.txt"], "the file made, its directory, and the delete, all taken back");
        assert_eq!(std::fs::read_to_string(dir.join("gone.txt")).unwrap(), "bye");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_refusal_comes_before_anything_is_touched() {
        let dir = scratch("refusals");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::create_dir(dir.join("full")).unwrap();
        std::fs::write(dir.join("full/inside.txt"), "").unwrap();
        let create = |overwrite, ignore_if_exists| Operation::Create { path: dir.join("a.txt"), overwrite, ignore_if_exists };

        assert_eq!(
            plan(&[(0, Operation::Rename { from: dir.join("nope"), to: dir.join("x"), overwrite: false, ignore_if_exists: false })], &[])
                .unwrap_err(),
            format!("{} does not exist", dir.join("nope").display())
        );
        assert!(plan(&[(0, create(false, false))], &[]).unwrap_err().ends_with("already exists"));
        assert!(plan(&[(0, create(false, true))], &[]).unwrap().steps.is_empty(), "ignoreIfExists: nothing to do");
        assert_eq!(plan(&[(0, create(true, true))], &[]).unwrap().steps.len(), 2, "overwrite: the old one goes and a new one comes");
        assert!(
            plan(&[(0, Operation::Delete { path: dir.join("full"), recursive: false, ignore_if_not_exists: false })], &[])
                .unwrap_err()
                .ends_with("a directory with files in it")
        );
        // Moved, then deleted under its new name: still the file whose
        // unsaved changes would be lost.
        let moved_then_deleted = [
            (0, Operation::Rename { from: dir.join("a.txt"), to: dir.join("b.txt"), overwrite: false, ignore_if_exists: false }),
            (0, Operation::Delete { path: dir.join("b.txt"), recursive: false, ignore_if_not_exists: false }),
        ];
        assert!(plan(&moved_then_deleted, &[dir.join("a.txt")]).unwrap_err().ends_with("has unsaved changes"));
        assert_eq!(names(&dir), ["a.txt", "full"], "and not one of them touched anything");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_text_change_follows_its_file_through_what_comes_after_it() {
        let dir = scratch("destination");
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        let plan = plan(
            &[
                (
                    1,
                    Operation::Rename {
                        from: dir.clone(),
                        to: dir.with_file_name(format!("{}-moved", dir.file_name().unwrap().to_string_lossy())),
                        overwrite: false,
                        ignore_if_exists: false,
                    },
                ),
                (
                    2,
                    Operation::Delete {
                        path: dir.with_file_name(format!("{}-moved", dir.file_name().unwrap().to_string_lossy())).join("a.txt"),
                        recursive: false,
                        ignore_if_not_exists: false,
                    },
                ),
            ],
            &[],
        )
        .unwrap();
        let moved = dir.with_file_name(format!("{}-moved", dir.file_name().unwrap().to_string_lossy()));
        assert_eq!(plan.destination(0, &dir.join("a.txt")), None, "moved with its directory, then deleted");
        assert_eq!(plan.destination(1, &moved.join("a.txt")), None, "named by its new name, then deleted");
        assert_eq!(plan.destination(2, &moved.join("a.txt")), Some(moved.join("a.txt")), "nothing after it");
        assert_eq!(plan.follow(&dir.join("a.txt")), moved.join("a.txt"), "an open buffer follows every rename");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
