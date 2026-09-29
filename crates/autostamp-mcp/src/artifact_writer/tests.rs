use super::*;
use serde_json::json;

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/writer-test-data");
        fs::create_dir_all(&base).expect("create fixture-local test base");
        let base = fs::canonicalize(base).expect("resolve fixture-local test base");
        for _ in 0..100 {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = base.join(format!("case-{}-{sequence}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => return Self(root),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("create unique test directory: {error}"),
            }
        }
        panic!("could not allocate a unique test directory");
    }

    fn output(&self) -> PathBuf {
        self.0.join("output")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        // This is only the exclusively created, fixture-local test case tree.
        if let Err(error) = remove_test_tree(&self.0) {
            if std::thread::panicking() {
                eprintln!("test cleanup failed at {}: {error}", self.0.display());
            } else {
                panic!("test cleanup failed at {}: {error}", self.0.display());
            }
        }
    }
}

fn remove_test_tree(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_test_tree(&entry?.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

fn artifacts() -> BTreeMap<PathBuf, String> {
    [
        (
            "Cargo.toml",
            "[package]\nname = \"generated-client\"\nversion = \"0.0.0\"\n",
        ),
        ("wasm.toml", "[package]\nname = \"client\"\n"),
        ("README.md", "Generated client.\n"),
        ("wit/world.wit", "package test:client;\nworld client {}\n"),
        ("src/lib.rs", "pub mod client;\n"),
        ("src/client.rs", "pub fn hello() {}\n"),
    ]
    .into_iter()
    .map(|(path, content)| (PathBuf::from(path), content.into()))
    .collect()
}

#[test]
fn wit_dependencies_are_owned_and_removed_without_touching_neighbors() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    files.insert(
        PathBuf::from("wit/deps/wasmcloud-secrets/secrets.wit"),
        "package wasmcloud:secrets;".into(),
    );
    files.insert(
        PathBuf::from("wit/deps/another.wit"),
        "package test:other;".into(),
    );
    write_crate(&output, &files).unwrap();
    let neighbor = output.join("wit/deps/notes.txt");
    fs::write(&neighbor, "user notes").unwrap();
    write_crate(&output, &artifacts()).unwrap();
    assert!(
        !output
            .join("wit/deps/wasmcloud-secrets/secrets.wit")
            .exists()
    );
    assert!(!output.join("wit/deps/another.wit").exists());
    assert!(!output.join("wit/deps/wasmcloud-secrets").exists());
    assert_eq!(fs::read_to_string(neighbor).unwrap(), "user notes");
}

#[test]
fn wit_dependencies_require_portable_bounded_paths() {
    let sandbox = TestDirectory::new();
    for path in [
        "wit/deps/../world.wit",
        "wit/deps/pkg/../world.wit",
        "wit/deps/pkg/nested/other.wit",
        "wit/deps/pkg//other.wit",
        "wit/deps/CON.wit",
        "wit/deps/nul.wit",
        "wit/deps/.hidden.wit",
        "wit/deps/foo.txt",
        "wit/deps/foo.wit/README.md",
    ] {
        let files = BTreeMap::from([(PathBuf::from(path), String::new())]);
        assert_preflight_error(
            &sandbox,
            &sandbox.output(),
            &files,
            WriterErrorKind::InvalidPath,
        );
    }
}

#[test]
fn pruning_dependency_directories_rolls_back_with_files_and_ledger() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    files.insert(
        "wit/deps/secret/store.wit".into(),
        "package test:secret;".into(),
    );
    write_crate(&output, &files).unwrap();
    let before = snapshot(&sandbox.0);
    let mut pruned = false;
    let error = write_with_hook(&output, &artifacts(), &mut |event, _| {
        if event == ApplyEvent::DirectoryRemoved {
            pruned = true;
            return Err(io::Error::other("injected directory pruning failure"));
        }
        Ok(())
    })
    .unwrap_err();
    assert!(pruned);
    assert_eq!(error.kind(), WriterErrorKind::Io);
    assert_eq!(snapshot(&sandbox.0), before);
}

#[test]
fn dependency_pruning_preserves_preexisting_directories_and_user_files() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    fs::create_dir_all(output.join("wit/deps/preexisting")).unwrap();
    let mut files = artifacts();
    files.insert(
        "wit/deps/preexisting/store.wit".into(),
        "package test:existing;".into(),
    );
    files.insert(
        "wit/deps/created/store.wit".into(),
        "package test:created;".into(),
    );
    write_crate(&output, &files).unwrap();
    fs::write(output.join("wit/deps/created/user.txt"), "preserve").unwrap();
    write_crate(&output, &artifacts()).unwrap();
    assert!(output.join("wit/deps/preexisting").is_dir());
    assert_eq!(
        fs::read_to_string(output.join("wit/deps/created/user.txt")).unwrap(),
        "preserve"
    );
    assert!(!output.join("wit/deps/created/store.wit").exists());
}

#[test]
fn forged_directory_ownership_is_restricted_to_generated_dependency_packages() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    fs::create_dir(&output).unwrap();
    for directory in [
        "src",
        "wit/deps",
        "wit/deps/../other",
        "wit/deps/nul",
        "wit/deps/unowned",
    ] {
        let mut ledger = ledger_with(vec![]);
        ledger
            .as_object_mut()
            .unwrap()
            .insert("directories".into(), json!([directory]));
        fs::write(output.join(LEDGER_FILE), ledger.to_string()).unwrap();
        assert_preflight_error(
            &sandbox,
            &output,
            &artifacts(),
            WriterErrorKind::InvalidLedger,
        );
    }
}

#[derive(Debug, Eq, PartialEq)]
enum TreeEntry {
    Directory,
    File(Vec<u8>),
    Symlink(PathBuf),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, TreeEntry> {
    fn walk(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, TreeEntry>) {
        let metadata = fs::symlink_metadata(path).expect("snapshot metadata");
        let relative = path
            .strip_prefix(root)
            .expect("snapshot relative path")
            .to_path_buf();
        if metadata.file_type().is_symlink() {
            entries.insert(
                relative,
                TreeEntry::Symlink(fs::read_link(path).expect("read link")),
            );
        } else if metadata.is_dir() {
            entries.insert(relative, TreeEntry::Directory);
            for entry in fs::read_dir(path).expect("snapshot directory") {
                walk(root, &entry.expect("snapshot entry").path(), entries);
            }
        } else {
            entries.insert(
                relative,
                TreeEntry::File(fs::read(path).expect("snapshot file")),
            );
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn assert_preflight_error(
    sandbox: &TestDirectory,
    output: &Path,
    files: &BTreeMap<PathBuf, String>,
    kind: WriterErrorKind,
) {
    let before = snapshot(&sandbox.0);
    let error = write_crate(output, files).expect_err("generation should fail preflight");
    assert_eq!(error.kind(), kind, "{error}");
    assert!(error.recovery_dir().is_none(), "{error}");
    assert_eq!(snapshot(&sandbox.0), before, "{error}");
}

fn ledger_entry(path: &str) -> serde_json::Value {
    json!({ "path": path, "sha256": digest(b"original") })
}

fn ledger_with(entries: Vec<serde_json::Value>) -> serde_json::Value {
    json!({ "generator": "autostamp", "version": 1, "files": entries })
}

#[test]
fn first_and_repeated_generations_are_deterministic() {
    let sandbox = TestDirectory::new();
    let output = sandbox.0.join("missing/parents/output");
    let files = artifacts();
    write_crate(&output, &files).expect("first generation");
    for (relative, expected) in &files {
        assert_eq!(
            fs::read_to_string(output.join(relative)).expect("read artifact"),
            *expected
        );
    }
    let first = snapshot(&sandbox.0);
    write_crate(&output, &files).expect("identical repeated generation");
    assert_eq!(snapshot(&sandbox.0), first);
    let bytes = fs::read(output.join(LEDGER_FILE)).expect("read ledger");
    let ledger: Ledger = serde_json::from_slice(&bytes).expect("parse ledger");
    assert_eq!(ledger.generator, "autostamp");
    assert_eq!(ledger.version, 1);
    let names: Vec<_> = ledger
        .files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    let expected: Vec<_> = files
        .keys()
        .map(|path| path.to_str().expect("UTF-8 path"))
        .collect();
    assert_eq!(names, expected);
    for entry in ledger.files {
        assert_eq!(
            entry.sha256,
            file_digest(&output.join(entry.path)).expect("digest")
        );
    }
    let other = sandbox.0.join("other");
    write_crate(&other, &files).expect("independent generation");
    assert_eq!(
        fs::read(other.join(LEDGER_FILE)).expect("read other ledger"),
        bytes
    );
}

#[test]
fn updates_owned_files_removes_stale_modules_and_preserves_unrelated_files() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    write_crate(&output, &files).expect("initial generation");
    fs::write(output.join("notes.txt"), "user notes").expect("write unrelated file");
    fs::write(output.join("src/manual.rs"), "user code").expect("write unrelated source");
    fs::create_dir(output.join("unrelated")).expect("create unrelated directory");
    files.remove(Path::new("src/client.rs"));
    files.insert("src/lib.rs".into(), "pub mod api;\n".into());
    files.insert("src/api/mod.rs".into(), "pub mod types;\n".into());
    files.insert("src/api/types.rs".into(), "pub struct Client;\n".into());
    write_crate(&output, &files).expect("update generation");
    assert!(!output.join("src/client.rs").exists());
    assert_eq!(
        fs::read_to_string(output.join("src/lib.rs")).expect("read lib"),
        "pub mod api;\n"
    );
    assert_eq!(
        fs::read_to_string(output.join("src/api/types.rs")).expect("read nested module"),
        "pub struct Client;\n"
    );
    assert_eq!(
        fs::read_to_string(output.join("notes.txt")).expect("read notes"),
        "user notes"
    );
    assert_eq!(
        fs::read_to_string(output.join("src/manual.rs")).expect("read manual"),
        "user code"
    );
    assert!(output.join("unrelated").is_dir());
    let before = snapshot(&sandbox.0);
    write_crate(&output, &files).expect("repeat updated generation");
    assert_eq!(snapshot(&sandbox.0), before);
}

#[test]
fn existing_directory_without_conflicting_artifacts_is_allowed() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    fs::create_dir(&output).expect("create output");
    fs::write(output.join("keep.txt"), "keep").expect("write unrelated file");
    write_crate(&output, &artifacts()).expect("generate alongside unrelated file");
    assert_eq!(
        fs::read_to_string(output.join("keep.txt")).expect("read unrelated file"),
        "keep"
    );
}

#[test]
fn refuses_unowned_file_even_when_bytes_match() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    fs::create_dir(&output).expect("create output");
    let files = artifacts();
    fs::write(
        output.join("Cargo.toml"),
        files
            .get(Path::new("Cargo.toml"))
            .expect("manifest artifact"),
    )
    .expect("write matching unowned file");
    assert_preflight_error(&sandbox, &output, &files, WriterErrorKind::Conflict);
}

#[test]
fn new_file_conflict_prevents_updates_and_stale_deletions() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    write_crate(&output, &files).expect("first generation");
    files.remove(Path::new("src/client.rs"));
    files.insert("src/lib.rs".into(), "new contents".into());
    files.insert("src/new.rs".into(), "generated".into());
    fs::write(output.join("src/new.rs"), "user file").expect("new conflict");
    assert_preflight_error(&sandbox, &output, &files, WriterErrorKind::Conflict);
}

#[test]
fn edited_owned_retained_and_stale_files_block_every_mutation() {
    for stale in [false, true] {
        let sandbox = TestDirectory::new();
        let output = sandbox.output();
        let mut files = artifacts();
        write_crate(&output, &files).expect("first generation");
        fs::write(output.join("src/client.rs"), "user edits").expect("edit generated file");
        if stale {
            files.remove(Path::new("src/client.rs"));
        }
        files.insert("README.md".into(), "would change".into());
        assert_preflight_error(&sandbox, &output, &files, WriterErrorKind::Modified);
    }
}

#[test]
fn missing_retained_files_are_recreated_and_missing_stale_files_are_ignored() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    write_crate(&output, &files).expect("first generation");
    fs::remove_file(output.join("src/client.rs")).expect("remove stale file");
    fs::remove_file(output.join("src/lib.rs")).expect("remove retained file");
    files.remove(Path::new("src/client.rs"));
    write_crate(&output, &files).expect("generation with missing files");
    assert!(!output.join("src/client.rs").exists());
    assert_eq!(
        fs::read_to_string(output.join("src/lib.rs")).expect("read recreated file"),
        "pub mod client;\n"
    );
}

#[test]
fn empty_generation_removes_only_owned_files_and_preserves_directories() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let files = artifacts();
    write_crate(&output, &files).expect("first generation");
    fs::write(output.join("src/manual.rs"), "keep").expect("unrelated module");
    write_crate(&output, &BTreeMap::new()).expect("empty generation");
    for relative in files.keys() {
        assert!(!output.join(relative).exists());
    }
    assert!(output.join("wit").is_dir());
    assert_eq!(
        fs::read_to_string(output.join("src/manual.rs")).expect("read unrelated module"),
        "keep"
    );
    let ledger: Ledger =
        serde_json::from_slice(&fs::read(output.join(LEDGER_FILE)).expect("read ledger"))
            .expect("parse empty ledger");
    assert!(ledger.files.is_empty());
}

#[test]
fn invalid_artifact_paths_do_not_create_output() {
    for path in [
        "",
        "../escape.rs",
        "/absolute.rs",
        "//server/share/file.rs",
        "C:/outside.rs",
        "C:relative.rs",
        "src\\escape.rs",
        "src/../escape.rs",
        "src/./lib.rs",
        "src//lib.rs",
        "src/lib.rs/",
        "src/lib.txt",
        "other.rs",
        "Cargo.lock",
        "wit/other.wit",
        "src",
        "src/.hidden.rs",
        "src/Upper.rs",
        "src/1module.rs",
        "src/con.rs",
        "src/nul/mod.rs",
        "src/aux.rs",
        "src/prn.rs",
        "src/com1.rs",
        "src/lpt9/mod.rs",
        "src/COM1.rs",
        "src/name .rs",
        "src/name..rs",
        ".autostamp-stage-anything/file",
        LEDGER_FILE,
    ] {
        let sandbox = TestDirectory::new();
        let files = BTreeMap::from([(PathBuf::from(path), "test".into())]);
        assert_preflight_error(
            &sandbox,
            &sandbox.output(),
            &files,
            WriterErrorKind::InvalidPath,
        );
        assert!(!sandbox.output().exists(), "{path}");
    }
}

#[test]
fn invalid_output_parent_traversal_is_rejected_before_creation() {
    let sandbox = TestDirectory::new();
    assert_preflight_error(
        &sandbox,
        &sandbox.0.join("child/../output"),
        &artifacts(),
        WriterErrorKind::InvalidPath,
    );
    assert!(resolve_root(Path::new("")).is_err());
    #[cfg(unix)]
    assert!(resolve_root(Path::new("/")).is_err());
}

#[test]
fn artifact_count_and_generated_ledger_size_are_bounded_before_mutation() {
    let sandbox = TestDirectory::new();
    let too_many: BTreeMap<_, _> = (0..=MAX_FILES)
        .map(|index| {
            (
                PathBuf::from(format!("src/module_{index}.rs")),
                String::new(),
            )
        })
        .collect();
    assert_preflight_error(
        &sandbox,
        &sandbox.output(),
        &too_many,
        WriterErrorKind::InvalidPath,
    );
    let too_large: BTreeMap<_, _> = (0..MAX_FILES)
        .map(|index| {
            (
                PathBuf::from(format!("src/{}_{index}.rs", "a".repeat(200))),
                String::new(),
            )
        })
        .collect();
    assert_preflight_error(
        &sandbox,
        &sandbox.output(),
        &too_large,
        WriterErrorKind::InvalidLedger,
    );
}

#[test]
fn portable_module_names_allow_underscores_digits_and_non_device_suffixes() {
    let sandbox = TestDirectory::new();
    let files = BTreeMap::from([
        (PathBuf::from("src/_private/mod.rs"), String::new()),
        (PathBuf::from("src/_private/com10.rs"), String::new()),
        (PathBuf::from("src/module_2.rs"), String::new()),
    ]);
    write_crate(&sandbox.output(), &files).expect("portable module paths");
    for relative in files.keys() {
        assert!(sandbox.output().join(relative).is_file());
    }
}

#[test]
fn invalid_ledgers_cannot_authorize_mutation() {
    let good = ledger_entry("src/lib.rs");
    let bad_ledgers = [
        b"not json".to_vec(),
        b"{}".to_vec(),
        b"null".to_vec(),
        br#"{"generator":"autostamp","version":1,"version":1,"files":[]}"#.to_vec(),
        serde_json::to_vec(&json!({ "generator": "other", "version": 1, "files": [] }))
            .expect("json"),
        serde_json::to_vec(&json!({ "generator": "autostamp", "version": 2, "files": [] }))
            .expect("json"),
        serde_json::to_vec(
            &json!({ "generator": "autostamp", "version": 1, "files": [], "extra": true }),
        )
        .expect("json"),
        serde_json::to_vec(&ledger_with(vec![good.clone(), good])).expect("json"),
        serde_json::to_vec(&ledger_with(vec![
            json!({ "path": "src/lib.rs", "sha256": "bad" }),
        ]))
        .expect("json"),
        serde_json::to_vec(&ledger_with(vec![
            json!({ "path": "src/lib.rs", "sha256": "F".repeat(64) }),
        ]))
        .expect("json"),
        serde_json::to_vec(&ledger_with(vec![
            json!({ "path": "src/lib.rs", "sha256": digest(b"original"), "extra": true }),
        ]))
        .expect("json"),
        vec![b' '; (MAX_LEDGER_BYTES + 1).try_into().expect("test size")],
    ];
    for bytes in bad_ledgers {
        let sandbox = TestDirectory::new();
        let output = sandbox.output();
        fs::create_dir_all(output.join("src")).expect("create output");
        fs::write(output.join("src/lib.rs"), "original").expect("existing file");
        fs::write(output.join(LEDGER_FILE), bytes).expect("write invalid ledger");
        assert_preflight_error(
            &sandbox,
            &output,
            &artifacts(),
            WriterErrorKind::InvalidLedger,
        );
    }
}

#[test]
fn escaping_reserved_and_unexpected_ledger_entries_are_rejected() {
    for path in [
        "../victim",
        "/absolute",
        "src/../../victim",
        "C:/victim",
        "C:victim",
        "src\\lib.rs",
        "src/./lib.rs",
        "src//lib.rs",
        "README.md/child",
        "Cargo.lock",
        "notes.txt",
        "src/nul.rs",
        ".autostamp-stage-1-1/file",
        LEDGER_FILE,
        "src/z.rs/child.rs",
    ] {
        let sandbox = TestDirectory::new();
        let output = sandbox.output();
        fs::create_dir(&output).expect("create output");
        fs::write(sandbox.0.join("victim"), "original").expect("external victim");
        fs::write(output.join("notes.txt"), "original").expect("unrelated victim");
        fs::write(
            output.join(LEDGER_FILE),
            serde_json::to_vec(&ledger_with(vec![ledger_entry(path)])).expect("json"),
        )
        .expect("write malicious ledger");
        assert_preflight_error(
            &sandbox,
            &output,
            &artifacts(),
            WriterErrorKind::InvalidLedger,
        );
    }
}

#[test]
fn mismatched_ledger_digest_cannot_claim_existing_file() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    fs::create_dir_all(output.join("src")).expect("create output");
    fs::write(output.join("src/lib.rs"), "not the claimed bytes").expect("existing file");
    fs::write(
        output.join(LEDGER_FILE),
        serde_json::to_vec(&ledger_with(vec![ledger_entry("src/lib.rs")])).expect("json"),
    )
    .expect("write ledger");
    assert_preflight_error(
        &sandbox,
        &output,
        &BTreeMap::new(),
        WriterErrorKind::Modified,
    );
}

#[test]
fn wrong_filesystem_types_are_rejected_without_mutation() {
    for kind in ["root", "ancestor", "ledger", "owned", "new"] {
        let sandbox = TestDirectory::new();
        let output = sandbox.output();
        let mut files = artifacts();
        let expected = match kind {
            "root" => {
                fs::write(&output, "file, not directory").expect("wrong root type");
                WriterErrorKind::UnexpectedFileType
            }
            "ancestor" => {
                fs::create_dir(&output).expect("output");
                fs::write(output.join("src"), "file, not directory").expect("wrong ancestor type");
                WriterErrorKind::UnexpectedFileType
            }
            "ledger" => {
                fs::create_dir_all(output.join(LEDGER_FILE)).expect("wrong ledger type");
                WriterErrorKind::UnexpectedFileType
            }
            "owned" => {
                write_crate(&output, &files).expect("first generation");
                fs::remove_file(output.join("src/client.rs")).expect("remove owned file");
                fs::create_dir(output.join("src/client.rs")).expect("wrong owned type");
                files.remove(Path::new("src/client.rs"));
                WriterErrorKind::UnexpectedFileType
            }
            "new" => {
                fs::create_dir_all(output.join("src/client.rs"))
                    .expect("conflicting new directory");
                WriterErrorKind::Conflict
            }
            _ => unreachable!("fixed test cases"),
        };
        assert_preflight_error(&sandbox, &output, &files, expected);
    }
}

#[cfg(unix)]
#[test]
fn symlink_roots_ancestors_files_and_ledgers_are_rejected() {
    use std::os::unix::fs::symlink;
    for kind in [
        "root",
        "root-ancestor",
        "ancestor",
        "new-file",
        "owned-file",
        "stale-file",
        "ledger",
        "dangling-ledger",
    ] {
        let sandbox = TestDirectory::new();
        let mut output = sandbox.output();
        let outside = sandbox.0.join("outside");
        fs::create_dir(&outside).expect("outside directory");
        fs::write(outside.join("victim"), "outside bytes").expect("outside file");
        let mut files = artifacts();
        match kind {
            "root" => symlink(&outside, &output).expect("root symlink"),
            "root-ancestor" => {
                symlink(&outside, &output).expect("root ancestor symlink");
                output = output.join("nested");
            }
            "ancestor" => {
                fs::create_dir(&output).expect("output");
                symlink(&outside, output.join("src")).expect("ancestor symlink");
            }
            "new-file" => {
                fs::create_dir_all(output.join("src")).expect("output");
                symlink(outside.join("victim"), output.join("src/lib.rs"))
                    .expect("new file symlink");
            }
            "owned-file" | "stale-file" => {
                write_crate(&output, &files).expect("first generation");
                fs::remove_file(output.join("src/client.rs")).expect("remove generated file");
                symlink(outside.join("victim"), output.join("src/client.rs"))
                    .expect("owned file symlink");
                if kind == "stale-file" {
                    files.remove(Path::new("src/client.rs"));
                }
            }
            "ledger" | "dangling-ledger" => {
                fs::create_dir(&output).expect("output");
                let target = if kind == "ledger" {
                    "victim"
                } else {
                    "missing"
                };
                symlink(outside.join(target), output.join(LEDGER_FILE)).expect("ledger symlink");
            }
            _ => unreachable!("fixed test cases"),
        }
        assert_preflight_error(&sandbox, &output, &files, WriterErrorKind::Symlink);
    }
}

#[cfg(unix)]
#[test]
fn unrelated_symlinks_are_preserved_without_following_them() {
    use std::os::unix::fs::symlink;
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    fs::create_dir(&output).expect("output");
    let missing = sandbox.0.join("does-not-exist");
    symlink(&missing, output.join("unrelated")).expect("unrelated dangling symlink");
    write_crate(&output, &artifacts()).expect("generation alongside unrelated symlink");
    assert_eq!(
        fs::read_link(output.join("unrelated")).expect("read unrelated link"),
        missing
    );
    write_crate(&output, &BTreeMap::new()).expect("remove generation");
    assert!(
        fs::symlink_metadata(output.join("unrelated"))
            .expect("preserved link")
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_artifact_paths_are_rejected() {
    use std::os::unix::ffi::OsStringExt;
    let sandbox = TestDirectory::new();
    let path = PathBuf::from(std::ffi::OsString::from_vec(vec![
        b's', b'r', b'c', b'/', 0xff,
    ]));
    assert_preflight_error(
        &sandbox,
        &sandbox.output(),
        &BTreeMap::from([(path, "source".into())]),
        WriterErrorKind::InvalidPath,
    );
}

#[test]
fn apply_failure_restores_all_original_bytes_and_ledger() {
    for fail_at in ["src/client.rs", "src/new.rs", LEDGER_FILE] {
        let sandbox = TestDirectory::new();
        let output = sandbox.output();
        let mut files = artifacts();
        write_crate(&output, &files).expect("first generation");
        fs::write(output.join("unrelated"), "keep").expect("unrelated");
        let before = snapshot(&sandbox.0);
        files.remove(Path::new("src/client.rs"));
        files.insert("README.md".into(), "changed readme".into());
        files.insert("src/new.rs".into(), "new module".into());
        files.insert("src/nested/child.rs".into(), "nested new module".into());
        let mut injected = false;
        let error = write_with_hook(&output, &files, &mut |event, path| {
            if path == output.join(fail_at)
                && (event == ApplyEvent::AfterInstall || fail_at == "src/client.rs")
            {
                injected = true;
                Err(io::Error::other("injected apply failure"))
            } else {
                Ok(())
            }
        })
        .expect_err("injected failure");
        assert!(injected, "hook was exercised for {fail_at}");
        assert_eq!(error.kind(), WriterErrorKind::Io, "{error}");
        assert!(error.recovery_dir().is_none(), "{error}");
        assert_eq!(snapshot(&sandbox.0), before, "{error}");
    }
}

#[test]
fn initial_apply_failure_removes_only_new_files_and_created_directories() {
    let sandbox = TestDirectory::new();
    let output = sandbox.0.join("new/parents/output");
    let before = snapshot(&sandbox.0);
    let error = write_with_hook(&output, &artifacts(), &mut |event, _| {
        if event == ApplyEvent::AfterInstall {
            Err(io::Error::other("injected first-generation failure"))
        } else {
            Ok(())
        }
    })
    .expect_err("injected failure");
    assert_eq!(error.kind(), WriterErrorKind::Io, "{error}");
    assert_eq!(snapshot(&sandbox.0), before);
}

#[test]
fn apply_failure_after_backup_before_install_restores_original() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    write_crate(&output, &files).expect("first generation");
    let before = snapshot(&sandbox.0);
    files.insert("src/lib.rs".into(), "changed".into());
    let error = write_with_hook(&output, &files, &mut |event, _| {
        if event == ApplyEvent::AfterBackup {
            Err(io::Error::other("injected backup failure"))
        } else {
            Ok(())
        }
    })
    .expect_err("injected failure");
    assert_eq!(error.kind(), WriterErrorKind::Io, "{error}");
    assert_eq!(snapshot(&sandbox.0), before);
}

#[test]
fn blocked_rollback_retains_backups_and_actionable_recovery_journal() {
    let sandbox = TestDirectory::new();
    let output = sandbox.output();
    let mut files = artifacts();
    write_crate(&output, &files).expect("first generation");
    let old_bytes = fs::read(output.join("src/lib.rs")).expect("old bytes");
    let old_ledger = fs::read(output.join(LEDGER_FILE)).expect("old ledger");
    files.insert("src/lib.rs".into(), "changed".into());
    let error = write_with_hook(&output, &files, &mut |event, path| {
        if event == ApplyEvent::AfterInstall {
            fs::write(path, "concurrent user edit")?;
            Err(io::Error::other("injected failure with blocked rollback"))
        } else {
            Ok(())
        }
    })
    .expect_err("rollback should be blocked");
    assert_eq!(error.kind(), WriterErrorKind::RecoveryRequired, "{error}");
    assert!(!error.recovery_errors().is_empty());
    let stage = error.recovery_dir().expect("retained recovery directory");
    assert!(stage.is_dir());
    assert_eq!(stage.parent(), Some(output.as_path()));
    let records: serde_json::Value =
        serde_json::from_slice(&fs::read(stage.join(JOURNAL_FILE)).expect("recovery journal"))
            .expect("parse recovery journal");
    let entry = records
        .as_array()
        .expect("journal entries")
        .iter()
        .find(|record| {
            record.get("target").and_then(serde_json::Value::as_str) == Some("src/lib.rs")
        })
        .expect("source recovery entry");
    let backup = entry
        .get("backup")
        .and_then(serde_json::Value::as_str)
        .expect("backup name");
    assert_eq!(
        fs::read(stage.join(backup)).expect("retained backup"),
        old_bytes
    );
    assert_eq!(
        fs::read(output.join(LEDGER_FILE)).expect("unchanged ledger"),
        old_ledger
    );
    assert_eq!(
        fs::read_to_string(output.join("src/lib.rs")).expect("preserved concurrent edit"),
        "concurrent user edit"
    );
    assert!(error.to_string().contains("inspect recovery.json"));
}
