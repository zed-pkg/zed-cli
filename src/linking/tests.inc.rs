#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(root: &Path, org: &str, name: &str) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(
            root.join(MANIFEST_FILE),
            format!("[package]\norg = \"{org}\"\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )?;
        Ok(())
    }

    fn symlink_target(path: &Path) -> Result<PathBuf> {
        let raw = fs::read_link(path)?;
        let resolved = if raw.is_absolute() {
            raw
        } else {
            path.parent()
                .context("symlink test path has no parent")?
                .join(raw)
        };
        Ok(resolved.canonicalize()?)
    }

    fn require_error<T>(result: Result<T>, message: &str) -> Result<anyhow::Error> {
        match result {
            Ok(_) => bail!("{message}"),
            Err(error) => Ok(error),
        }
    }

    #[cfg(unix)]
    #[test]
    fn register_consume_and_unlink_restore_previous_links() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        let old = temp.path().join("old");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        write_manifest(&old, "acme", "widget")?;
        fs::write(source.join("payload.txt"), "one")?;
        fs::write(consumer.join("package.json"), "{}")?;

        let zed_destination = consumer.join(MODULES_DIR).join("acme/widget");
        let node_destination = consumer.join("node_modules/@acme/widget");
        create_directory_symlink(&old, &zed_destination)?;
        create_directory_symlink(&old, &node_destination)?;

        register(&home, &source)?;
        let receipt = consume(&consumer, &home, "acme/widget", LocalLinkAdapter::Auto)?;
        assert_eq!(receipt.projections.len(), 2);
        assert_eq!(symlink_target(&zed_destination)?, source.canonicalize()?);
        assert_eq!(symlink_target(&node_destination)?, source.canonicalize()?);

        fs::write(source.join("payload.txt"), "two")?;
        assert_eq!(
            fs::read_to_string(zed_destination.join("payload.txt"))?,
            "two"
        );

        unlink_consumer(&consumer, "acme/widget")?;
        assert_eq!(symlink_target(&zed_destination)?, old.canonicalize()?);
        assert_eq!(symlink_target(&node_destination)?, old.canonicalize()?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn regular_directory_is_backed_up_and_restored() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        let destination = consumer.join(MODULES_DIR).join("acme/widget");
        fs::create_dir_all(&destination)?;
        fs::write(destination.join("old.txt"), "old")?;

        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;
        assert_eq!(symlink_target(&destination)?, source.canonicalize()?);

        unlink_consumer(&consumer, "acme/widget")?;
        assert!(destination.is_dir());
        assert_eq!(fs::read_to_string(destination.join("old.txt"))?, "old");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unlink_succeeds_after_source_checkout_is_deleted() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        let old = temp.path().join("old");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        write_manifest(&old, "acme", "old")?;
        let destination = consumer.join(MODULES_DIR).join("acme/widget");
        create_directory_symlink(&old, &destination)?;

        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;
        fs::remove_dir_all(&source)?;
        assert!(fs::symlink_metadata(&destination)?.file_type().is_symlink());

        unlink_consumer(&consumer, "acme/widget")?;
        assert_eq!(symlink_target(&destination)?, old.canonicalize()?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn state_parent_symlink_escape_is_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        let outside = temp.path().join("outside");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        fs::create_dir_all(&outside)?;
        fs::create_dir_all(consumer.join(".zed"))?;
        std::os::unix::fs::symlink(&outside, consumer.join(CONSUMER_STATE_DIR))?;

        register(&home, &source)?;
        require_error(
            consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None),
            "symlinked state parent must be rejected",
        )?;
        assert!(!outside.join("acme/widget.json").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unlink_preflights_all_projections_before_restoring_any() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let other = temp.path().join("other");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&other, "acme", "other")?;
        write_manifest(&consumer, "acme", "consumer")?;
        fs::write(consumer.join("package.json"), "{}")?;

        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::Auto)?;
        let zed_destination = consumer.join(MODULES_DIR).join("acme/widget");
        let node_destination = consumer.join("node_modules/@acme/widget");
        fs::remove_file(&node_destination)?;
        create_directory_symlink(&other, &node_destination)?;

        let error = require_error(
            unlink_consumer(&consumer, "acme/widget"),
            "changed projection must fail closed",
        )?;
        assert!(format!("{error:#}").contains("changed by another tool"));
        assert_eq!(symlink_target(&zed_destination)?, source.canonicalize()?);
        assert_eq!(symlink_target(&node_destination)?, other.canonicalize()?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unlink_by_bare_name_survives_global_unregister() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;
        unregister(&home, "acme/widget")?;

        let key = resolve_consumer_key(&consumer, &home, "widget")?;
        assert_eq!(key, "acme/widget");
        unlink_consumer(&consumer, &key)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn link_does_not_mutate_manifest_or_lock() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        let lock = consumer.join(".zpkg.lock");
        fs::write(&lock, "sentinel-lock\n")?;
        let before_manifest = fs::read(consumer.join(MANIFEST_FILE))?;
        let before_lock = fs::read(&lock)?;

        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;

        assert_eq!(fs::read(consumer.join(MANIFEST_FILE))?, before_manifest);
        assert_eq!(fs::read(&lock)?, before_lock);
        Ok(())
    }

    #[test]
    fn stale_registration_identity_is_rejected() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&consumer, "acme", "consumer")?;
        register(&home, &source)?;
        write_manifest(&source, "other", "widget")?;

        let error = require_error(
            consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None),
            "identity drift must be rejected",
        )?;
        assert!(format!("{error:#}").contains("instead of `acme/widget`"));
        Ok(())
    }

    #[test]
    fn bare_names_must_be_unique() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let left = temp.path().join("left");
        let right = temp.path().join("right");
        write_manifest(&left, "one", "widget")?;
        write_manifest(&right, "two", "widget")?;
        register(&home, &left)?;
        register(&home, &right)?;

        let error = require_error(
            resolve_registered_key(&home, "widget"),
            "ambiguous bare name must be rejected",
        )?;
        assert!(format!("{error:#}").contains("ambiguous"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unlink_refuses_a_projection_changed_by_another_tool() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        let source = temp.path().join("source");
        let other = temp.path().join("other");
        let consumer = temp.path().join("consumer");
        write_manifest(&source, "acme", "widget")?;
        write_manifest(&other, "acme", "other")?;
        write_manifest(&consumer, "acme", "consumer")?;
        register(&home, &source)?;
        consume(&consumer, &home, "acme/widget", LocalLinkAdapter::None)?;
        let destination = consumer.join(MODULES_DIR).join("acme/widget");
        fs::remove_file(&destination)?;
        create_directory_symlink(&other, &destination)?;

        let error = require_error(
            unlink_consumer(&consumer, "acme/widget"),
            "changed projection must be preserved",
        )?;
        assert!(format!("{error:#}").contains("changed by another tool"));
        assert_eq!(symlink_target(&destination)?, other.canonicalize()?);
        Ok(())
    }

    #[test]
    fn npm_style_scoped_key_normalizes_to_zed_identity() -> Result<()> {
        assert_eq!(normalize_key("@acme/widget")?, "acme/widget");
        Ok(())
    }
}
