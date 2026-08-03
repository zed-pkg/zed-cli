//! Recoverable project-tree mutations.
//!
//! Each transaction gets a UUID-v4 directory under `.zpkg-staging/`. Before a
//! managed path is changed, its previous value is renamed into that staging
//! directory and recorded in durable JSON metadata. Normal errors roll back
//! through `Drop`; a hard exit leaves the metadata behind and the next Zed
//! lifecycle operation restores it before doing new work.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zed_interfaces::paths::LOCKFILE_FILE;

pub const STAGING_DIR: &str = ".zpkg-staging";
const METADATA_FILE: &str = "transaction.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Active,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    relative: PathBuf,
    backup: PathBuf,
    existed: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Metadata {
    id: Uuid,
    state: State,
    entries: Vec<Entry>,
}

/// A transaction over paths contained by one project.
pub struct ProjectTransaction {
    project: PathBuf,
    root: PathBuf,
    metadata: Metadata,
    finished: bool,
}

impl ProjectTransaction {
    /// Recover older interrupted operations, then create a fresh UUID-v4
    /// staging directory.
    pub fn begin(project: &Path) -> Result<Self> {
        recover_pending(project)?;
        let id = Uuid::new_v4();
        let root = project.join(STAGING_DIR).join(id.to_string());
        fs::create_dir_all(root.join("backups"))?;
        let transaction = Self {
            project: project.to_path_buf(),
            root,
            metadata: Metadata {
                id,
                state: State::Active,
                entries: Vec::new(),
            },
            finished: false,
        };
        transaction.save()?;
        Ok(transaction)
    }

    pub fn id(&self) -> Uuid {
        self.metadata.id
    }

    /// Move the current value of a project-relative path into the staging
    /// area. If the path does not exist, recovery records that it must remove
    /// any newly created value.
    ///
    /// The project lockfile is the one managed path that is both a mutable
    /// output during normal resolution and an immutable input during a frozen
    /// install. Its rollback snapshot is therefore copied atomically while the
    /// source remains visible. A non-frozen caller may overwrite it normally;
    /// a frozen caller may commit without rewriting it; and either caller still
    /// gets exact-byte rollback after an error or interrupted process.
    pub fn backup(&mut self, path: &Path) -> Result<()> {
        let relative = project_relative(&self.project, path)?;
        if self
            .metadata
            .entries
            .iter()
            .any(|entry| covers(&entry.relative, &relative))
        {
            return Ok(());
        }
        if self
            .metadata
            .entries
            .iter()
            .any(|entry| covers(&relative, &entry.relative))
        {
            bail!(
                "cannot stage parent `{}` after one of its children",
                relative.display()
            );
        }

        let current_metadata = fs::symlink_metadata(path).ok();
        let existed = current_metadata.is_some();
        let preserve_source = relative == Path::new(LOCKFILE_FILE)
            && current_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.file_type().is_file());
        let backup = PathBuf::from("backups").join(format!(
            "{:04}-{}",
            self.metadata.entries.len(),
            Uuid::new_v4()
        ));
        let entry = Entry {
            relative,
            backup,
            existed,
        };
        // Persist intent first. Recovery can distinguish all crash windows by
        // checking which side of the rename/copy exists.
        self.metadata.entries.push(entry.clone());
        self.save()?;
        if existed {
            let backup_path = self.root.join(&entry.backup);
            fs::create_dir_all(backup_path.parent().context("backup parent")?)?;
            if preserve_source {
                copy_file_atomically(path, &backup_path).with_context(|| {
                    format!(
                        "checkpointing immutable transaction input {} as {}",
                        path.display(),
                        backup_path.display()
                    )
                })?;
            } else {
                fs::rename(path, &backup_path).with_context(|| {
                    format!("staging {} as {}", path.display(), backup_path.display())
                })?;
            }
        }
        Ok(())
    }

    /// Make the new tree authoritative and remove the recovery area.
    pub fn commit(mut self) -> Result<()> {
        self.metadata.state = State::Committed;
        self.save()?;
        // Once the committed marker is durable, old backups must never be
        // restored. Cleanup can be retried by recover_pending on the next
        // invocation if an antivirus/indexer temporarily holds a file open.
        self.finished = true;
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!(
                "warning: committed transaction {} but could not remove {}: {error}; \
                 the next zed invocation will finish cleanup",
                self.metadata.id,
                self.root.display()
            );
        } else {
            remove_empty_staging_parent(&self.project);
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let encoded = serde_json::to_vec_pretty(&self.metadata)?;
        let temporary = self
            .root
            .join(format!(".{METADATA_FILE}.{}", Uuid::new_v4()));
        fs::write(&temporary, encoded)?;
        fs::rename(&temporary, self.root.join(METADATA_FILE))?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<()> {
        restore(&self.project, &self.root, &self.metadata)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for ProjectTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback();
        }
    }
}

/// Recover every active transaction left by an interrupted process.
pub fn recover_pending(project: &Path) -> Result<usize> {
    let staging = project.join(STAGING_DIR);
    if !staging.is_dir() {
        return Ok(0);
    }
    let mut roots: Vec<PathBuf> = fs::read_dir(&staging)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    roots.sort();
    let mut recovered = 0usize;
    for root in roots {
        let metadata_path = root.join(METADATA_FILE);
        let encoded = fs::read(&metadata_path).with_context(|| {
            format!(
                "interrupted transaction at {} has no readable metadata; inspect it before removing it",
                root.display()
            )
        })?;
        let metadata: Metadata = serde_json::from_slice(&encoded)
            .with_context(|| format!("invalid transaction metadata {}", metadata_path.display()))?;
        if metadata.state == State::Active {
            restore(project, &root, &metadata)?;
            recovered += 1;
            eprintln!(
                "recovered interrupted zed transaction {} in {}",
                metadata.id,
                project.display()
            );
        } else {
            fs::remove_dir_all(&root)?;
        }
    }
    remove_empty_staging_parent(project);
    Ok(recovered)
}

fn restore(project: &Path, root: &Path, metadata: &Metadata) -> Result<()> {
    for entry in metadata.entries.iter().rev() {
        validate_relative(&entry.relative)?;
        validate_relative(&entry.backup)?;
        let destination = project.join(&entry.relative);
        let backup = root.join(&entry.backup);
        if entry.existed {
            if fs::symlink_metadata(&backup).is_ok() {
                remove_path(&destination)?;
                fs::create_dir_all(destination.parent().context("destination parent")?)?;
                fs::rename(&backup, &destination)?;
            }
            // With no backup, interruption happened before the source rename
            // or before an atomic lockfile checkpoint completed; the original
            // destination is still authoritative.
        } else {
            remove_path(&destination)?;
        }
    }
    fs::remove_dir_all(root)?;
    remove_empty_staging_parent(project);
    Ok(())
}

fn project_relative(project: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path.strip_prefix(project).with_context(|| {
        format!(
            "transaction path {} is outside project {}",
            path.display(),
            project.display()
        )
    })?;
    validate_relative(relative)?;
    if relative.as_os_str().is_empty() || relative.starts_with(STAGING_DIR) {
        bail!("refusing to transact over `{}`", relative.display());
    }
    Ok(relative.to_path_buf())
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe transaction-relative path `{}`", path.display());
    }
    Ok(())
}

fn covers(parent: &Path, child: &Path) -> bool {
    child == parent || child.starts_with(parent)
}

fn copy_file_atomically(source: &Path, destination: &Path) -> Result<()> {
    let file_name = destination
        .file_name()
        .context("transaction backup has no filename")?
        .to_string_lossy();
    let temporary = destination.with_file_name(format!(".{file_name}.{}", Uuid::new_v4()));
    fs::copy(source, &temporary)?;
    fs::rename(&temporary, destination)?;
    Ok(())
}

fn remove_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_empty_staging_parent(project: &Path) {
    let _ = fs::remove_dir(project.join(STAGING_DIR));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_restores_replaced_and_removes_new_paths() {
        let temp = tempfile::tempdir().unwrap();
        let old = temp.path().join("tree/old.txt");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, "old").unwrap();
        let new = temp.path().join("new.txt");
        {
            let mut transaction = ProjectTransaction::begin(temp.path()).unwrap();
            transaction.backup(&temp.path().join("tree")).unwrap();
            transaction.backup(&new).unwrap();
            fs::create_dir_all(temp.path().join("tree")).unwrap();
            fs::write(temp.path().join("tree/new.txt"), "new").unwrap();
            fs::write(&new, "new").unwrap();
        }
        assert_eq!(fs::read_to_string(&old).unwrap(), "old");
        assert!(!new.exists());
        assert!(!temp.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn interrupted_uuid_transaction_is_recovered_on_next_begin() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        fs::write(&path, "before").unwrap();
        let mut transaction = ProjectTransaction::begin(temp.path()).unwrap();
        let id = transaction.id();
        assert_eq!(id.get_version(), Some(uuid::Version::Random));
        transaction.backup(&path).unwrap();
        fs::write(&path, "partial").unwrap();
        // Simulate a hard exit: suppress Drop while leaving durable staging.
        std::mem::forget(transaction);

        assert_eq!(recover_pending(temp.path()).unwrap(), 1);
        assert_eq!(fs::read_to_string(path).unwrap(), "before");
    }

    #[test]
    fn commit_keeps_new_value_and_cleans_staging() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state");
        fs::write(&path, "before").unwrap();
        let mut transaction = ProjectTransaction::begin(temp.path()).unwrap();
        transaction.backup(&path).unwrap();
        fs::write(&path, "after").unwrap();
        transaction.commit().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "after");
        assert!(!temp.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn lockfile_checkpoint_stays_visible_and_exact_after_commit() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join(LOCKFILE_FILE);
        let original = b"# retained provenance\nversion = 1\n";
        fs::write(&lock, original).unwrap();

        let mut transaction = ProjectTransaction::begin(temp.path()).unwrap();
        transaction.backup(&lock).unwrap();
        assert_eq!(fs::read(&lock).unwrap(), original);
        transaction.commit().unwrap();

        assert_eq!(fs::read(&lock).unwrap(), original);
        assert!(!temp.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn lockfile_checkpoint_restores_exact_bytes_after_failed_rewrite() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join(LOCKFILE_FILE);
        let original = b"# retained provenance\nversion = 1\n";
        fs::write(&lock, original).unwrap();

        {
            let mut transaction = ProjectTransaction::begin(temp.path()).unwrap();
            transaction.backup(&lock).unwrap();
            fs::write(&lock, b"version = 999\n").unwrap();
        }

        assert_eq!(fs::read(&lock).unwrap(), original);
        assert!(!temp.path().join(STAGING_DIR).exists());
    }
}