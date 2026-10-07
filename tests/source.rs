use blink::source::{Control, Exclusion, Limits, Snapshot, Source, StopReason};
use std::{fs, path::Path};
use tempfile::TempDir;

fn put(root: &Path, path: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
fn scan(root: &Path) -> Snapshot {
    Source::open(root)
        .unwrap()
        .snapshot(Limits::default(), &mut || Control::Continue)
}
fn paths(snapshot: &Snapshot) -> Vec<&str> {
    snapshot.files().iter().map(|file| file.path()).collect()
}

#[test]
fn nested_gitignore_negation_and_pruned_directories() {
    let root = TempDir::new().unwrap();
    put(
        root.path(),
        ".gitignore",
        "*.log\nblocked/\n!blocked/keep.rs\n",
    );
    put(root.path(), "src/.gitignore", "!keep.log\n");
    put(root.path(), "src/keep.log", "included\n");
    put(root.path(), "src/drop.log", "excluded\n");
    put(root.path(), "blocked/keep.rs", "excluded\n");
    put(root.path(), "z.rs", "source\n");
    let result = scan(root.path());
    assert_eq!(paths(&result), ["src/keep.log", "z.rs"]);
    assert_eq!(result.coverage().excluded[&Exclusion::Gitignore], 2);
    assert!(result.coverage().complete);
}

#[test]
fn blinkignore_is_additional_and_only_root_blinkignore_applies() {
    let root = TempDir::new().unwrap();
    put(root.path(), ".blinkignore", "*.txt\n!keep.txt\n");
    put(root.path(), ".gitignore", "git.txt\n");
    put(root.path(), "sub/.blinkignore", "*.rs\n");
    put(root.path(), "sub/.gitignore", "!drop.txt\n");
    for path in [
        "keep.txt",
        "drop.txt",
        "git.txt",
        "sub/drop.txt",
        "sub/code.rs",
    ] {
        put(root.path(), path, "text");
    }
    assert_eq!(paths(&scan(root.path())), ["keep.txt", "sub/code.rs"]);
}

#[test]
fn ignores_dot_ignore_and_filters_hidden_and_sensitive_names() {
    let root = TempDir::new().unwrap();
    put(root.path(), ".ignore", "visible.rs\n");
    put(root.path(), ".gitignore", "!.env.example\n!private.pem\n");
    for path in [
        ".hidden/file.rs",
        ".env",
        ".env.example",
        "private.pem",
        "private-key",
        "ssh_host_rsa_key",
        "private_keys/key-data",
        "backup.asc",
        "id_ed25519.pub",
        "credentials.json",
        "secrets/data.rs",
        ".aws/config",
        "node_modules/a.js",
        "vendor/lib.rs",
        "target/out.rs",
        "build/a.rs",
        "dist/a.js",
        ".git/data",
        ".hg/data",
        ".svn/data",
    ] {
        put(root.path(), path, "excluded");
    }
    put(root.path(), "visible.rs", "source");
    let result = scan(root.path());
    assert_eq!(paths(&result), ["visible.rs"]);
    for reason in [
        Exclusion::Sensitive,
        Exclusion::Hidden,
        Exclusion::Dependency,
        Exclusion::Build,
        Exclusion::Metadata,
    ] {
        assert!(result.coverage().excluded[&reason] > 0);
    }
}

#[test]
fn selected_subdirectory_inherits_repository_rules() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    put(root.path(), ".gitignore", "*.log\nignored/\n");
    put(root.path(), "sub/keep.rs", "source");
    put(root.path(), "sub/drop.log", "excluded");
    put(root.path(), "ignored/deeper/code.rs", "excluded");
    assert_eq!(paths(&scan(&root.path().join("sub"))), ["keep.rs"]);
    let excluded = scan(&root.path().join("ignored/deeper"));
    assert!(excluded.files().is_empty());
    assert_eq!(excluded.coverage().excluded[&Exclusion::Gitignore], 1);
    assert!(excluded.coverage().complete);
}

#[test]
fn no_repository_means_no_parent_gitignore() {
    let root = TempDir::new().unwrap();
    put(root.path(), ".gitignore", "*.rs\n");
    put(root.path(), "sub/keep.rs", "source");
    assert_eq!(paths(&scan(&root.path().join("sub"))), ["keep.rs"]);
}

#[test]
fn invalid_ignore_rules_fail_closed() {
    let root = TempDir::new().unwrap();
    put(root.path(), ".blinkignore", [255]);
    put(root.path(), "keep.rs", "source");
    let result = scan(root.path());
    assert!(result.files().is_empty());
    assert!(!result.coverage().complete);
    assert_eq!(result.coverage().issues[0].operation, "ignore_parse");
}

#[test]
fn snapshots_preserve_utf8_hash_lines_and_bytes() {
    let root = TempDir::new().unwrap();
    put(root.path(), "hello.txt", "hello\n");
    put(root.path(), "unicode.rs", "é\nλ");
    put(root.path(), "binary.dat", b"data\0more");
    put(root.path(), "invalid.dat", [0xff, 0xfe]);
    let mut result = scan(root.path());
    assert_eq!(paths(&result), ["hello.txt", "unicode.rs"]);
    let hello = &result.files()[0];
    assert_eq!(
        hello.sha256(),
        "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
    );
    assert_eq!(hello.line_offsets(), [0, 6]);
    assert_eq!(result.files()[1].line_offsets(), [0, 3]);
    assert_eq!(hello.text(), "hello\n");
    assert!(result.recheck(0, &mut || Control::Continue).unwrap());
    put(root.path(), "hello.txt", "other\n");
    assert_eq!(result.files()[0].bytes(), b"hello\n");
    assert!(!result.recheck(0, &mut || Control::Continue).unwrap());
    assert_eq!(result.coverage().excluded[&Exclusion::Binary], 1);
    assert_eq!(result.coverage().excluded[&Exclusion::NonUtf8Content], 1);
}

#[test]
fn file_limit_is_an_exclusion_and_read_budget_is_incomplete() {
    let root = TempDir::new().unwrap();
    put(root.path(), "large.rs", "0123456789");
    let source = Source::open(root.path()).unwrap();
    let result = source.snapshot(
        Limits {
            max_file_bytes: 9,
            ..Limits::default()
        },
        &mut || Control::Continue,
    );
    assert!(result.files().is_empty());
    assert!(result.coverage().complete);
    assert_eq!(result.coverage().excluded[&Exclusion::TooLarge], 1);
    assert_eq!(result.coverage().bytes_read, 0);
    let result = source.snapshot(
        Limits {
            max_total_bytes: 9,
            ..Limits::default()
        },
        &mut || Control::Continue,
    );
    assert!(!result.coverage().complete);
    assert_eq!(result.coverage().stops, [StopReason::ByteLimit]);
    assert!(result.coverage().bytes_read <= 9);
}

#[test]
fn visit_budget_includes_excluded_entries() {
    let root = TempDir::new().unwrap();
    for i in 0..10 {
        put(root.path(), &format!(".hidden{i}"), "excluded");
    }
    let result = Source::open(root.path()).unwrap().snapshot(
        Limits {
            max_entries: 3,
            ..Limits::default()
        },
        &mut || Control::Continue,
    );
    assert_eq!(result.coverage().visited_entries, 3);
    assert_eq!(result.coverage().stops, [StopReason::EntryLimit]);
    assert!(!result.coverage().complete);
}

#[test]
fn cancellation_and_deadline_stop_before_source_reads() {
    let root = TempDir::new().unwrap();
    put(root.path(), "a.rs", "source");
    for (control, reason) in [
        (Control::Cancel, StopReason::Cancelled),
        (Control::Deadline, StopReason::Deadline),
    ] {
        let result = Source::open(root.path())
            .unwrap()
            .snapshot(Limits::default(), &mut || control);
        assert_eq!(result.coverage().stops, [reason]);
        assert_eq!(result.coverage().bytes_read, 0);
        assert!(!result.coverage().complete);
    }
}

#[test]
fn missing_and_non_directory_roots_are_errors() {
    let root = TempDir::new().unwrap();
    put(root.path(), "a.rs", "source");
    assert!(Source::open(root.path().join("missing")).is_err());
    assert!(Source::open(root.path().join("a.rs")).is_err());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn symlinks_at_every_depth_and_special_files_are_excluded() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        put(outside.path(), "secret.rs", "outside");
        put(root.path(), "safe/deep/keep.rs", "inside");
        symlink(outside.path(), root.path().join("linked")).unwrap();
        symlink(
            outside.path().join("secret.rs"),
            root.path().join("safe/deep/link.rs"),
        )
        .unwrap();
        symlink("../..", root.path().join("safe/deep/loop")).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(root.path().join("socket")).unwrap();
        let result = scan(root.path());
        assert_eq!(paths(&result), ["safe/deep/keep.rs"]);
        assert_eq!(result.coverage().excluded[&Exclusion::Symlink], 3);
        assert_eq!(result.coverage().excluded[&Exclusion::Special], 1);
        assert!(result.coverage().complete);
        drop(socket);
    }

    #[test]
    fn ignore_file_symlink_fails_closed() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        put(outside.path(), "rules", "*");
        put(root.path(), "keep.rs", "source");
        symlink(outside.path().join("rules"), root.path().join(".gitignore")).unwrap();
        let result = scan(root.path());
        assert!(result.files().is_empty());
        assert!(!result.coverage().complete);
        assert_eq!(result.coverage().issues[0].kind, "not_regular_file");
    }

    #[test]
    fn recheck_refuses_replaced_parent_symlink() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        put(root.path(), "sub/code.rs", "inside");
        put(outside.path(), "code.rs", "inside");
        let mut result = scan(root.path());
        fs::rename(root.path().join("sub"), root.path().join("original")).unwrap();
        symlink(outside.path(), root.path().join("sub")).unwrap();
        assert!(result.recheck(0, &mut || Control::Continue).is_err());
    }

    #[test]
    fn root_descriptor_survives_path_replacement() {
        let container = TempDir::new().unwrap();
        let root = container.path().join("selected");
        put(&root, "original.rs", "inside");
        let source = Source::open(&root).unwrap();
        fs::rename(&root, container.path().join("moved")).unwrap();
        put(&root, "replacement.rs", "outside");
        let mut result = source.snapshot(Limits::default(), &mut || Control::Continue);
        assert_eq!(paths(&result), ["original.rs"]);
        assert!(result.recheck(0, &mut || Control::Continue).unwrap());
    }

    #[test]
    fn ancestor_ignore_policy_survives_repository_path_replacement() {
        let container = TempDir::new().unwrap();
        let repository = container.path().join("repository");
        let selected = repository.join("selected");
        fs::create_dir_all(repository.join(".git")).unwrap();
        put(&repository, ".gitignore", "private.rs\n");
        put(
            &selected,
            "private.rs",
            "excluded by the original repository",
        );
        put(&selected, "public.rs", "eligible");
        let source = Source::open(&selected).unwrap();
        fs::rename(&repository, container.path().join("moved")).unwrap();
        fs::create_dir_all(repository.join(".git")).unwrap();
        fs::create_dir_all(&selected).unwrap();
        put(&repository, ".gitignore", "");

        let snapshot = source.snapshot(Limits::default(), &mut || Control::Continue);
        assert_eq!(paths(&snapshot), ["public.rs"]);
        assert!(snapshot.coverage().complete);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_names_are_counted_without_lossy_inventory_paths() {
        use std::os::unix::ffi::OsStringExt;
        let root = TempDir::new().unwrap();
        fs::write(
            root.path().join(std::ffi::OsString::from_vec(vec![0xff])),
            b"text",
        )
        .unwrap();
        let result = scan(root.path());
        assert!(result.files().is_empty());
        assert_eq!(result.coverage().excluded[&Exclusion::NonUtf8Path], 1);
    }

    #[test]
    fn unreadable_file_makes_coverage_incomplete() {
        let root = TempDir::new().unwrap();
        put(root.path(), "private.rs", "source");
        let path = root.path().join("private.rs");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o0)).unwrap();
        let can_read = fs::read(&path).is_ok();
        let result = scan(root.path());
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        if !can_read {
            assert!(!result.coverage().complete);
            assert!(result.files().is_empty());
            assert_eq!(result.coverage().issues[0].kind, "PermissionDenied");
        }
    }
}

#[test]
fn freshness_checks_share_read_budget_and_control() {
    let root = TempDir::new().unwrap();
    put(root.path(), "code.rs", "text");
    let source = Source::open(root.path()).unwrap();
    let mut result = source.snapshot(
        Limits {
            max_total_bytes: 8,
            ..Limits::default()
        },
        &mut || Control::Continue,
    );
    assert_eq!(result.coverage().bytes_read, 4);
    assert!(result.recheck(0, &mut || Control::Continue).unwrap());
    assert_eq!(result.coverage().bytes_read, 8);
    assert!(result.recheck(0, &mut || Control::Continue).is_err());
    assert_eq!(result.coverage().bytes_read, 8);
    assert_eq!(result.coverage().stops, [StopReason::ByteLimit]);
    assert!(!result.coverage().complete);
    let mut result = source.snapshot(Limits::default(), &mut || Control::Continue);
    assert!(result.recheck(0, &mut || Control::Deadline).is_err());
    assert_eq!(result.coverage().bytes_read, 4);
    assert_eq!(result.coverage().stops, [StopReason::Deadline]);
    assert!(!result.coverage().complete);
}

#[test]
fn selecting_a_sensitive_directory_cannot_bypass_policy() {
    let root = TempDir::new().unwrap();
    put(root.path(), "secrets/deep/data.rs", "secret");
    let result = scan(&root.path().join("secrets/deep"));
    assert!(result.files().is_empty());
    assert_eq!(result.coverage().excluded[&Exclusion::Sensitive], 1);
    assert!(result.coverage().complete);
}

#[test]
fn entry_limit_preserves_captured_files_for_budgeted_rechecks() {
    let root = TempDir::new().unwrap();
    for name in ["a.rs", "b.rs", "c.rs"] {
        put(root.path(), name, "text");
    }
    let mut result = Source::open(root.path()).unwrap().snapshot(
        Limits {
            max_entries: 1,
            ..Limits::default()
        },
        &mut || Control::Continue,
    );
    assert_eq!(result.files().len(), 1);
    assert_eq!(result.coverage().stops, [StopReason::EntryLimit]);
    assert_eq!(result.coverage().bytes_read, 4);
    assert!(result.recheck(0, &mut || Control::Continue).unwrap());
    assert_eq!(result.coverage().bytes_read, 8);
    assert_eq!(result.coverage().stops, [StopReason::EntryLimit]);
    assert!(!result.coverage().complete);
}

#[test]
fn recheck_after_entry_limit_retains_later_hard_stops() {
    let root = TempDir::new().unwrap();
    for name in ["a.rs", "b.rs"] {
        put(root.path(), name, "text");
    }
    let source = Source::open(root.path()).unwrap();
    for (control, reason) in [
        (Control::Cancel, StopReason::Cancelled),
        (Control::Deadline, StopReason::Deadline),
    ] {
        let mut result = source.snapshot(
            Limits {
                max_entries: 1,
                ..Limits::default()
            },
            &mut || Control::Continue,
        );
        assert!(result.recheck(0, &mut || control).is_err());
        assert_eq!(result.coverage().bytes_read, 4);
        assert_eq!(result.coverage().stops, [StopReason::EntryLimit, reason]);
        assert!(result.recheck(0, &mut || Control::Continue).is_err());
        assert_eq!(result.coverage().bytes_read, 4);
        assert!(!result.coverage().complete);
    }
    let mut result = source.snapshot(
        Limits {
            max_entries: 1,
            max_total_bytes: 4,
            ..Limits::default()
        },
        &mut || Control::Continue,
    );
    assert!(result.recheck(0, &mut || Control::Continue).is_err());
    assert_eq!(
        result.coverage().stops,
        [StopReason::EntryLimit, StopReason::ByteLimit]
    );
    assert_eq!(result.coverage().bytes_read, 4);
    assert!(!result.coverage().complete);
}

#[test]
fn ancestor_ignore_errors_never_include_absolute_local_paths() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(root.path().join(".gitignore"), [255]).unwrap();
    let source = Source::open(root.path().join("nested")).unwrap();
    let snapshot = source.snapshot(Limits::default(), &mut || Control::Continue);
    assert!(!snapshot.coverage().complete);
    assert!(!snapshot.coverage().issues.is_empty());
    for issue in &snapshot.coverage().issues {
        assert!(!std::path::Path::new(&issue.path).is_absolute());
        assert_eq!(issue.path, "<ancestor>");
    }
    let encoded = serde_json::to_string(snapshot.coverage()).unwrap();
    assert!(!encoded.contains(root.path().to_str().unwrap()));
}
