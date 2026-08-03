from pathlib import Path


store = Path("src/store.rs")
text = store.read_text()
test_name = "fn install_process_lock_is_released_when_owner_process_is_terminated()"

if test_name not in text:
    marker = '''    #[test]
    fn gc_survives_hostile_max_age() {
'''
    replacement = '''    #[test]
    fn install_process_lock_is_released_when_owner_process_is_terminated() {
        const CHILD_ROLE: &str = "ZED_PKG_TEST_STORE_LOCK_EXIT_ROLE";
        const CHILD_HOME: &str = "ZED_PKG_TEST_STORE_LOCK_EXIT_HOME";
        const OWNER_ACQUIRED: &str = "ZED_PKG_TEST_STORE_LOCK_OWNER_ACQUIRED";
        const WAITER_ATTEMPTING: &str = "ZED_PKG_TEST_STORE_LOCK_WAITER_ATTEMPTING";
        const WAITER_ACQUIRED: &str = "ZED_PKG_TEST_STORE_LOCK_WAITER_ACQUIRED";
        const TEST_NAME: &str =
            "store::tests::install_process_lock_is_released_when_owner_process_is_terminated";

        if let Some(role) = std::env::var_os(CHILD_ROLE) {
            let home = PathBuf::from(std::env::var_os(CHILD_HOME).unwrap());
            let store = Store::new(&home);
            match role.to_string_lossy().as_ref() {
                "owner" => {
                    let _owner = store.install_lock().unwrap();
                    fs::write(std::env::var_os(OWNER_ACQUIRED).unwrap(), b"acquired").unwrap();
                    loop {
                        std::thread::sleep(Duration::from_secs(60));
                    }
                }
                "waiter" => {
                    fs::write(std::env::var_os(WAITER_ATTEMPTING).unwrap(), b"attempting")
                        .unwrap();
                    let _waiter = store.install_lock().unwrap();
                    fs::write(std::env::var_os(WAITER_ACQUIRED).unwrap(), b"acquired").unwrap();
                    return;
                }
                other => panic!("unexpected child role: {other}"),
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let owner_acquired = temp.path().join("store-lock-owner-acquired");
        let waiter_attempting = temp.path().join("store-lock-waiter-attempting-after-exit");
        let waiter_acquired = temp.path().join("store-lock-waiter-acquired-after-exit");

        let mut owner = Command::new(std::env::current_exe().unwrap())
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_ROLE, "owner")
            .env(CHILD_HOME, temp.path())
            .env(OWNER_ACQUIRED, &owner_acquired)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let owner_deadline = Instant::now() + Duration::from_secs(5);
        while !owner_acquired.is_file() {
            if let Some(status) = owner.try_wait().unwrap() {
                panic!("store-lock owner exited before acquiring install.lock: {status}");
            }
            if Instant::now() >= owner_deadline {
                let _ = owner.kill();
                let _ = owner.wait();
                panic!("store-lock owner did not acquire install.lock");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let mut waiter = Command::new(std::env::current_exe().unwrap())
            .arg(TEST_NAME)
            .arg("--exact")
            .arg("--nocapture")
            .env(CHILD_ROLE, "waiter")
            .env(CHILD_HOME, temp.path())
            .env(WAITER_ATTEMPTING, &waiter_attempting)
            .env(WAITER_ACQUIRED, &waiter_acquired)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let attempting_deadline = Instant::now() + Duration::from_secs(5);
        while !waiter_attempting.is_file() {
            if let Some(status) = waiter.try_wait().unwrap() {
                let _ = owner.kill();
                let _ = owner.wait();
                panic!("store-lock waiter exited before attempting acquisition: {status}");
            }
            if Instant::now() >= attempting_deadline {
                let _ = waiter.kill();
                let _ = waiter.wait();
                let _ = owner.kill();
                let _ = owner.wait();
                panic!("store-lock waiter did not reach acquisition");
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !waiter_acquired.exists(),
            "waiter acquired install.lock while the owner process was alive"
        );
        assert!(
            waiter.try_wait().unwrap().is_none(),
            "store-lock waiter exited before owner termination"
        );

        owner.kill().unwrap();
        let owner_status = owner.wait().unwrap();
        assert!(
            !owner_status.success(),
            "owner process unexpectedly exited successfully instead of being terminated"
        );

        let release_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = waiter.try_wait().unwrap() {
                assert!(status.success(), "store-lock waiter failed after owner exit: {status}");
                break;
            }
            if Instant::now() >= release_deadline {
                let _ = waiter.kill();
                let _ = waiter.wait();
                panic!("store-lock waiter did not wake after owner process termination");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            waiter_acquired.is_file(),
            "waiter exited without acquiring install.lock after owner termination"
        );
    }

    #[test]
    fn gc_survives_hostile_max_age() {
'''
    if text.count(marker) != 1:
        raise RuntimeError("could not locate insertion point for owner-exit lock test")
    text = text.replace(marker, replacement, 1)

store.write_text(text)
