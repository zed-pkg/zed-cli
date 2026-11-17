#[test]
    fn rejects_zip_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let archive = temp.path().join("symlink.zip");
        let file = fs::File::create(&archive).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        // Use the ZIP writer's symlink primitive so external attributes and
        // payload match a genuine Unix symlink entry, not a regular file with
        // only permission bits set.
        zip.add_symlink("package/link", "../../outside", options)
            .unwrap();
        zip.finish().unwrap();

        // Prove the fixture really carries S_IFLNK before testing the importer.
        let mut input = zip::ZipArchive::new(fs::File::open(&archive).unwrap()).unwrap();
        let entry = input.by_name("package/link").unwrap();
        let mode = entry.unix_mode().expect("symlink entry has Unix mode");
        assert_eq!(mode & 0o170000, 0o120000);
        drop(entry);
        drop(input);

        let sha = digest(&archive);
        let store = Store::new(&temp.path().join("home"));
        assert!(add_native_artifact(&store, &archive, &sha).is_err());
        assert!(!store.has(&sha), "rejected symlink archive must not enter the store");
        assert!(
            !store.entry_dir(&sha).exists(),
            "rejected symlink archive must not publish a cache entry"
        );
    }
