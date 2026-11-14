//! The tool store against built archives and a fake download; it needs
//! symbolic links, so it runs on Unix.
#![cfg(unix)]

use super::{
    ToolStore, ToolStoreError,
    fetch::{ByteLimit, Fetch},
};
use ren_depot::{
    Depot,
    job::{Failure, HostFailure, JobState, Stage},
    package::{Kind, Package, PackageId},
    source::{ArchiveFormat, Artifact, ArtifactUrl, Platform, Release, Sha256, Source},
    toolset::{Edit, Generation, Plan, Toolset},
};
use sha2::Digest as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
    path::Path,
};

/// Serves fixed bytes by URL.
#[derive(Default)]
struct FakeFetch(BTreeMap<String, Vec<u8>>);

impl Fetch for FakeFetch {
    async fn fetch(
        &self,
        url: &ArtifactUrl,
        into: &Path,
        limit: ByteLimit,
    ) -> Result<(), HostFailure> {
        let bytes = self.0.get(url.as_str()).ok_or(HostFailure::Refused)?;
        if u64::try_from(bytes.len()).unwrap() > limit.bytes() {
            return Err(HostFailure::TooLarge);
        }
        std::fs::write(into, bytes).map_err(|_| HostFailure::Filesystem)
    }
}

/// Run a program just written, retrying while a process another test
/// thread is starting still holds the file open ("text file busy").
fn run(program: impl AsRef<Path>) -> std::process::Output {
    for _ in 0..100 {
        match std::process::Command::new(program.as_ref()).output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other.unwrap(),
        }
    }
    panic!("{} stayed busy", program.as_ref().display());
}

fn sha256(bytes: &[u8]) -> Sha256 {
    Sha256::from_bytes(sha2::Sha256::digest(bytes).into())
}

/// A tar.gz with `jq-<version>/bin/jq`, a link beside it, and `extra`.
fn tarball(version: &str, extra: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let script = format!("#!/bin/sh\necho jq-{version}\n");
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(script.len()).unwrap());
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(
            &mut header,
            format!("jq-{version}/bin/jq"),
            script.as_bytes(),
        )
        .unwrap();
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    builder
        .append_link(&mut link, format!("jq-{version}/bin/jq-link"), "jq")
        .unwrap();
    extra(&mut builder);
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn release(
    version: &str,
    format: ArchiveFormat,
    bytes: &[u8],
    program_at: &str,
) -> (String, Source) {
    let url = format!("https://example.test/jq-{version}");
    let source = Source::Release(Release {
        version: version.parse().unwrap(),
        artifacts: vec![Artifact {
            platform: Platform::LinuxX64,
            url: url.parse().unwrap(),
            sha256: sha256(bytes),
            format,
            program_at: program_at.parse().unwrap(),
        }],
    });
    (url, source)
}

fn jq(source: Source) -> Depot {
    Depot::of(vec![Package::new(
        PackageId::JQ,
        Kind::Utility,
        "jq".parse().unwrap(),
        source,
    )])
}

fn plan(toolset: &Toolset, depot: &Depot, ids: &[PackageId]) -> Plan {
    let wanted: BTreeSet<PackageId> = ids.iter().cloned().collect();
    toolset.plan(depot, Platform::LinuxX64, &wanted).unwrap()
}

#[tokio::test]
async fn installs_a_verified_release_and_activates_it_as_a_generation() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = tarball("1.7.1", |_| {});
    let (url, source) = release("1.7.1", ArchiveFormat::TarGz, &bytes, "jq-1.7.1/bin/jq");
    let store = ToolStore::new(dir.path(), FakeFetch([(url, bytes)].into()));
    let depot = jq(source);

    let current = store.current().unwrap();
    assert_eq!(current.generation(), Generation::NONE);
    let plan = plan(&current, &depot, &[PackageId::JQ]);
    let ended = store.install_plan(&plan).await;
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].1, JobState::Installed);
    assert!(store.has(&ended[0].0));
    assert!(!dir.path().join("staging").read_dir().unwrap().any(|_| true));

    let one = store.activate(plan).unwrap();
    assert_eq!(one.generation().number(), 1);
    assert_eq!(store.current().unwrap(), one);
    let program = store.bin(one.generation()).join("jq");
    let output = run(&program);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "jq-1.7.1\n");
    // The archive's own link came along.
    let installed = std::fs::canonicalize(&program).unwrap();
    assert!(installed.with_file_name("jq-link").exists());
}

#[tokio::test]
async fn a_download_with_another_digest_is_refused_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = tarball("1.7.1", |_| {});
    let (url, source) = release("1.7.1", ArchiveFormat::TarGz, &bytes, "jq-1.7.1/bin/jq");
    let tampered = tarball("6.6.6", |_| {});
    let store = ToolStore::new(dir.path(), FakeFetch([(url, tampered.clone())].into()));
    let plan = plan(&Toolset::empty(), &jq(source), &[PackageId::JQ]);
    let ended = store.install_plan(&plan).await;
    assert_eq!(
        ended[0].1,
        JobState::Failed {
            stage: Stage::Verify,
            failure: Failure::Mismatch {
                actual: sha256(&tampered)
            },
        }
    );
    assert!(!store.has(&ended[0].0));
    assert!(!dir.path().join("store").exists());
    assert!(matches!(
        store.activate(plan),
        Err(ToolStoreError::NotInstalled(id)) if id == PackageId::JQ
    ));
}

#[tokio::test]
async fn archives_that_reach_outside_their_folder_are_refused() {
    let escapes = [
        tarball("1.7.1", |builder| {
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            builder
                .append_link(&mut link, "jq-1.7.1/escape", "../../../etc/passwd")
                .unwrap();
        }),
        tarball("1.7.1", |builder| {
            let mut hard = tar::Header::new_gnu();
            hard.set_entry_type(tar::EntryType::Link);
            hard.set_size(0);
            builder
                .append_link(&mut hard, "jq-1.7.1/hard", "/etc/passwd")
                .unwrap();
        }),
    ];
    for bytes in escapes {
        let dir = tempfile::tempdir().unwrap();
        let (url, source) = release("1.7.1", ArchiveFormat::TarGz, &bytes, "jq-1.7.1/bin/jq");
        let store = ToolStore::new(dir.path(), FakeFetch([(url, bytes)].into()));
        let ended = store
            .install_plan(&plan(&Toolset::empty(), &jq(source), &[PackageId::JQ]))
            .await;
        assert_eq!(
            ended[0].1,
            JobState::Failed {
                stage: Stage::Unpack,
                failure: Failure::Host(HostFailure::BadArchive),
            }
        );
        assert!(!dir.path().join("store").exists());
    }
}

#[tokio::test]
async fn installs_zip_and_bare_releases_and_names_a_missing_program() {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    zip.start_file("jq-1.8.0/jq", options).unwrap();
    zip.write_all(b"#!/bin/sh\necho zipped\n").unwrap();
    let zipped = zip.finish().unwrap().into_inner();
    let bare = b"#!/bin/sh\necho bare\n".to_vec();

    for (bytes, format, program_at, said) in [
        (
            zipped.clone(),
            ArchiveFormat::Zip,
            "jq-1.8.0/jq",
            Some("zipped\n"),
        ),
        (bare, ArchiveFormat::Bare, "jq", Some("bare\n")),
        (zipped, ArchiveFormat::Zip, "bin/jq", None),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (url, source) = release("1.8.0", format, &bytes, program_at);
        let store = ToolStore::new(dir.path(), FakeFetch([(url, bytes)].into()));
        let plan = plan(&Toolset::empty(), &jq(source), &[PackageId::JQ]);
        let ended = store.install_plan(&plan).await;
        let Some(said) = said else {
            assert_eq!(
                ended[0].1,
                JobState::Failed {
                    stage: Stage::Unpack,
                    failure: Failure::ProgramMissing,
                }
            );
            continue;
        };
        assert_eq!(ended[0].1, JobState::Installed, "{format}");
        let one = store.activate(plan).unwrap();
        let output = run(store.bin(one.generation()).join("jq"));
        assert_eq!(String::from_utf8_lossy(&output.stdout), said, "{format}");
    }
}

#[tokio::test]
async fn collection_keeps_what_live_generations_hold() {
    let dir = tempfile::tempdir().unwrap();
    let old = tarball("1.7.1", |_| {});
    let new = tarball("1.8.0", |_| {});
    let (old_url, old_source) = release("1.7.1", ArchiveFormat::TarGz, &old, "jq-1.7.1/bin/jq");
    let (new_url, new_source) = release("1.8.0", ArchiveFormat::TarGz, &new, "jq-1.8.0/bin/jq");
    let store = ToolStore::new(
        dir.path(),
        FakeFetch([(old_url, old), (new_url, new)].into()),
    );

    let first = plan(&Toolset::empty(), &jq(old_source), &[PackageId::JQ]);
    store.install_plan(&first).await;
    let one = store.activate(first).unwrap();
    let second = plan(&one, &jq(new_source), &[PackageId::JQ]);
    store.install_plan(&second).await;
    let two = store.activate(second).unwrap();
    assert_eq!(store.current().unwrap(), two);

    // A run still uses generation 1: nothing goes.
    let collected = store.collect(&[one.generation()]).unwrap();
    assert_eq!(collected, super::Collected::default());
    assert!(store.bin(one.generation()).join("jq").exists());

    // Once it ends, generation 1 and its jq go; the current one stays whole.
    let collected = store.collect(&[]).unwrap();
    assert_eq!(collected.generations, [one.generation()]);
    assert_eq!(
        collected
            .installs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [one.install(&PackageId::JQ).unwrap().dir().to_string()]
    );
    assert!(!store.generation_dir(one.generation()).exists());
    assert!(dir.path().join("store/jq").is_dir());
    let output = run(store.bin(two.generation()).join("jq"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "jq-1.8.0\n");

    // Nothing to remove the second time.
    assert_eq!(store.collect(&[]).unwrap(), super::Collected::default());
}

#[tokio::test]
async fn a_stale_plan_is_refused_at_activation() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = tarball("1.7.1", |_| {});
    let (url, source) = release("1.7.1", ArchiveFormat::TarGz, &bytes, "jq-1.7.1/bin/jq");
    let store = ToolStore::new(dir.path(), FakeFetch([(url, bytes)].into()));
    let depot = jq(source);
    let first = plan(&Toolset::empty(), &depot, &[PackageId::JQ]);
    let stale = plan(&Toolset::empty(), &depot, &[PackageId::JQ]);
    store.install_plan(&first).await;
    store.activate(first).unwrap();
    assert!(matches!(
        store.activate(stale),
        Err(ToolStoreError::Plan(_))
    ));
    assert_eq!(store.current().unwrap().generation().number(), 1);
}

#[tokio::test]
async fn a_change_installs_and_activates_under_the_store_lock() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = tarball("1.7.1", |_| {});
    let (url, source) = release("1.7.1", ArchiveFormat::TarGz, &bytes, "jq-1.7.1/bin/jq");
    let store = ToolStore::new(dir.path(), FakeFetch([(url, bytes)].into()));
    let depot = jq(source);
    let wanted = Edit {
        add: [PackageId::JQ].into(),
        remove: BTreeSet::new(),
    };

    // Another writer holds the lock: refused, nothing changes.
    let held = store.lock().unwrap();
    assert!(matches!(
        store.change(&depot, Platform::LinuxX64, &wanted).await,
        Err(ToolStoreError::Busy)
    ));
    drop(held);

    let changed = store
        .change(&depot, Platform::LinuxX64, &wanted)
        .await
        .unwrap_or_else(|error| panic!("{error:?}"));
    assert!(matches!(
        &changed,
        super::Changed::Activated { toolset, changes } if toolset.generation().number() == 1 && changes.len() == 1
    ));
    assert!(matches!(
        store.change(&depot, Platform::LinuxX64, &wanted).await.unwrap(),
        super::Changed::Unchanged(toolset) if toolset.generation().number() == 1
    ));
}

#[tokio::test]
async fn a_failed_install_leaves_the_current_generation() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = tarball("1.7.1", |_| {});
    let (_, source) = release("1.7.1", ArchiveFormat::TarGz, &bytes, "jq-1.7.1/bin/jq");
    // The server refuses the download.
    let store = ToolStore::new(dir.path(), FakeFetch::default());
    let changed = store
        .change(
            &jq(source),
            Platform::LinuxX64,
            &Edit {
                add: [PackageId::JQ].into(),
                remove: BTreeSet::new(),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        &changed,
        super::Changed::Failed { current, jobs }
            if current.generation() == Generation::NONE
                && jobs[0].1 == JobState::Failed { stage: Stage::Fetch, failure: Failure::Host(HostFailure::Refused) }
    ));
    assert_eq!(store.current().unwrap().generation(), Generation::NONE);
}
