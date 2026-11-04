use super::*;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

struct Fixture {
    _temp: tempfile::TempDir,
    source: PathBuf,
    store: ReleaseStore,
}
#[test]
fn package_roles_preserve_legacy_clients_and_reject_service_bundles() {
    let fixture = Fixture::new("roles");
    let manifest = fixture.manifest();
    let mut legacy = serde_json::to_value(&manifest).unwrap();
    legacy.as_object_mut().unwrap().remove("role");
    let decoded: ReleaseManifest = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.role, ReleaseRole::Client);
    decoded.validate(&ImportOptions::default()).unwrap();
    for role in [
        ReleaseRole::Server,
        ReleaseRole::Controller,
        ReleaseRole::Web,
    ] {
        let mut candidate = manifest.clone();
        candidate.role = role;
        assert!(candidate
            .validate(&ImportOptions::default())
            .unwrap_err()
            .to_string()
            .contains("service maintenance"));
    }
    let mut unknown = serde_json::to_value(manifest).unwrap();
    unknown["role"] = "unknown".into();
    assert!(serde_json::from_value::<ReleaseManifest>(unknown).is_err());
}

#[test]
fn legacy_manifests_roundtrip_without_build_metadata() {
    let fixture = Fixture::new("legacy-build");
    let manifest = fixture.manifest();
    let encoded = serde_json::to_value(&manifest).unwrap();
    for field in ["profile", "mode", "cargo_profile"] {
        assert!(encoded.get(field).is_none());
    }
    let decoded: ReleaseManifest = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded, manifest);
    decoded.validate(&ImportOptions::default()).unwrap();
    assert_eq!(fixture.import().unwrap().manifest, manifest);

    let mut unknown = encoded;
    unknown["unknown_build_field"] = "not supported".into();
    assert!(serde_json::from_value::<ReleaseManifest>(unknown).is_err());
}

#[test]
fn imports_all_build_modes_and_preserves_metadata() {
    for (profile, mode, cargo_profile) in [
        ("debug", "fast", "dev"),
        ("release", "fast", "release-fast"),
        ("debug", "runtime", "dev-runtime"),
        ("release", "runtime", "release-runtime"),
    ] {
        let fixture = Fixture::new("build-metadata");
        fixture.edit(|manifest| {
            manifest.profile = Some(profile.into());
            manifest.mode = Some(mode.into());
            manifest.cargo_profile = Some(cargo_profile.into());
        });
        let manifest = fixture.manifest();
        let encoded = serde_json::to_value(&manifest).unwrap();
        assert_eq!(encoded["profile"], profile);
        assert_eq!(encoded["mode"], mode);
        assert_eq!(encoded["cargo_profile"], cargo_profile);
        let imported = fixture.import().unwrap();
        assert_eq!(imported.manifest, manifest);
        assert_eq!(fixture.import().unwrap().manifest, manifest);
    }
}

#[test]
fn rejects_incomplete_or_mismatched_build_metadata() {
    let fixture = Fixture::new("invalid-build-metadata");
    for (profile, mode, cargo_profile) in [
        (None, Some("fast"), Some("dev")),
        (Some("debug"), None, Some("dev")),
        (Some("debug"), Some("fast"), None),
        (Some("debug"), Some("fast"), Some("release-fast")),
        (Some("release"), Some("runtime"), Some("dev-runtime")),
        (Some("release"), Some("unknown"), Some("release")),
        (Some("unknown"), Some("fast"), Some("dev")),
        (Some("debug"), Some("fast"), Some("unknown")),
    ] {
        fixture.edit(|manifest| {
            manifest.profile = profile.map(str::to_owned);
            manifest.mode = mode.map(str::to_owned);
            manifest.cargo_profile = cargo_profile.map(str::to_owned);
        });
        let error = fixture.import().unwrap_err();
        assert!(format!("{error:#}")
            .to_lowercase()
            .contains("build metadata"));
        fixture.assert_not_installed();
    }
}

impl Fixture {
    fn new(id: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("package");
        fs::create_dir(&source).unwrap();
        let suffix = if TARGET.contains("windows") {
            ".exe"
        } else {
            ""
        };
        let mut manifest = ReleaseManifest {
            schema_version: 1,
            role: Default::default(),
            release_id: id.into(),
            target: TARGET.into(),
            host_protocol: ProtocolRange { min: 1, max: 1 },
            components: BTreeMap::new(),
            files: Vec::new(),
            profile: None,
            mode: None,
            cargo_profile: None,
        };
        for component in ["astra-proxy", "astra-rule", "astra-tui", "astra-host"] {
            let bytes = format!("test-only-{id}-{component}").into_bytes();
            let name = format!("{component}{suffix}");
            fs::write(source.join(&name), &bytes).unwrap();
            manifest.components.insert(component.into(), "0.1.0".into());
            manifest.files.push(ReleaseFile {
                path: name,
                size: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            });
        }
        fs::write(
            source.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let store = ReleaseStore::new(temp.path().join("state"));
        Self {
            _temp: temp,
            source,
            store,
        }
    }
    fn manifest(&self) -> ReleaseManifest {
        serde_json::from_slice(&fs::read(self.source.join(MANIFEST_FILE)).unwrap()).unwrap()
    }
    fn edit(&self, change: impl FnOnce(&mut ReleaseManifest)) {
        let mut manifest = self.manifest();
        change(&mut manifest);
        fs::write(
            self.source.join(MANIFEST_FILE),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
    fn import(&self) -> anyhow::Result<ImportResult> {
        self.store.import(&self.source, &ImportOptions::default())
    }
    fn assert_not_installed(&self) {
        assert_eq!(
            fs::read_dir(self.store.root().join("releases"))
                .unwrap()
                .count(),
            0
        );
        assert!(self.store.current().unwrap().is_none());
        assert_eq!(
            fs::read_dir(self.store.root().join("staging"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn imports_without_activation_and_deduplicates_verified_content() {
    let f = Fixture::new("release-a");
    let first = f.import().unwrap();
    assert!(!first.already_installed);
    assert_eq!(first.manifest.release_id, "release-a");
    assert!(f.store.current().unwrap().is_none());
    assert!(f.import().unwrap().already_installed);
    let current = f.store.commit_current("release-a").unwrap();
    assert_eq!(current.previous_release_id, None);
    assert_eq!(f.store.current().unwrap(), Some(current));
    assert_eq!(
        f.store.journal().unwrap().unwrap().phase,
        TransactionPhase::Committed
    );
}

#[test]
fn rejects_wrong_target_and_can_stage_a_future_host_release() {
    let f = Fixture::new("release-a");
    f.edit(|m| m.target = "other-platform".into());
    assert!(f.import().unwrap_err().to_string().contains("target"));
    f.edit(|m| {
        m.target = TARGET.into();
        m.host_protocol.min = 2;
        m.host_protocol.max = 3;
    });
    assert!(f
        .store
        .import(
            &f.source,
            &ImportOptions {
                enforce_host_protocol: true,
                ..Default::default()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("protocol"));
    f.assert_not_installed();
    let staged = f.import().unwrap();
    assert!(!staged.compatible_host);
    assert!(f
        .store
        .commit_current("release-a")
        .unwrap_err()
        .to_string()
        .contains("protocol"));
    assert!(f.store.current().unwrap().is_none());
}

#[test]
fn rejects_corrupt_and_unlisted_files_and_missing_components() {
    let f = Fixture::new("release-a");
    let entry = &f.manifest().files[0];
    let path = f.source.join(&entry.path);
    let original = fs::read(&path).unwrap();
    let mut corrupt = original.clone();
    corrupt[0] ^= 1;
    fs::write(&path, corrupt).unwrap();
    assert!(f.import().unwrap_err().to_string().contains("SHA-256"));
    fs::write(path, original).unwrap();
    fs::write(f.source.join("unlisted.dll"), "extra").unwrap();
    assert!(f.import().unwrap_err().to_string().contains("unlisted"));
    fs::remove_file(f.source.join("unlisted.dll")).unwrap();
    f.edit(|m| {
        m.components.remove("astra-rule");
    });
    assert!(f
        .import()
        .unwrap_err()
        .to_string()
        .contains("Missing component"));
    f.assert_not_installed();
}

#[test]
fn rejects_packages_without_the_resident_host() {
    for remove_version in [true, false] {
        let f = Fixture::new("release-without-host");
        f.edit(|m| {
            if remove_version {
                m.components.remove("astra-host");
            } else {
                m.files
                    .retain(|file| file.path != "astra-host" && file.path != "astra-host.exe");
            }
        });
        let error = f.import().unwrap_err().to_string();
        assert!(error.contains("astra-host"), "{error}");
        f.assert_not_installed();
    }
}

#[test]
fn rejects_id_collision_without_overwriting_installed_executables() {
    let f = Fixture::new("release-a");
    let installed = f.import().unwrap();
    let file = &installed.manifest.files[0];
    let original = fs::read(installed.directory.join(&file.path)).unwrap();
    f.edit(|m| {
        m.components.insert("astra-proxy".into(), "0.2.0".into());
    });
    assert!(f
        .import()
        .unwrap_err()
        .to_string()
        .contains("different content"));
    assert_eq!(
        fs::read(installed.directory.join(&file.path)).unwrap(),
        original
    );
}

#[test]
fn rejects_file_count_total_and_file_size_limits() {
    let f = Fixture::new("release-a");
    for options in [
        ImportOptions {
            max_entries: 2,
            ..ImportOptions::default()
        },
        ImportOptions {
            max_total_bytes: 4,
            ..ImportOptions::default()
        },
        ImportOptions {
            max_file_bytes: 4,
            ..ImportOptions::default()
        },
    ] {
        assert!(f.store.import(&f.source, &options).is_err());
        f.assert_not_installed();
    }
}

fn zip_files(path: &Path, entries: &[(&str, &[u8])]) {
    let mut archive = zip::ZipWriter::new(File::create(path).unwrap());
    for (name, data) in entries {
        archive
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(data).unwrap();
    }
    archive.finish().unwrap();
}

#[test]
fn imports_zip_and_tar_gz_with_client_wrapper() {
    for format in ["zip", "tar.gz"] {
        let f = Fixture::new("release-a");
        let path = f._temp.path().join(format!("release.{format}"));
        if format == "zip" {
            let entries: Vec<(String, Vec<u8>)> = fs::read_dir(&f.source)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    (
                        format!("client/{}", entry.file_name().to_str().unwrap()),
                        fs::read(entry.path()).unwrap(),
                    )
                })
                .collect();
            let entries: Vec<_> = entries
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
                .collect();
            zip_files(&path, &entries);
        } else {
            let encoder = flate2::write::GzEncoder::new(
                File::create(&path).unwrap(),
                flate2::Compression::default(),
            );
            let mut archive = tar::Builder::new(encoder);
            archive.append_dir_all("client", &f.source).unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let installed = f.store.import(path, &ImportOptions::default()).unwrap();
        assert_eq!(installed.manifest.release_id, "release-a");
        assert!(installed.directory.join(MANIFEST_FILE).is_file());
    }
}

#[test]
fn rejects_archive_traversal_absolute_paths_and_windows_aliases() {
    for name in [
        "../escape",
        "/absolute",
        "client/../../escape",
        "C:/escape",
        "client\\escape",
        "file:stream",
        "CON",
        "trailing.",
    ] {
        let f = Fixture::new("release-a");
        let path = f._temp.path().join("bad.zip");
        zip_files(&path, &[(name, b"invalid")]);
        assert!(
            f.store.import(path, &ImportOptions::default()).is_err(),
            "{name}"
        );
        assert!(!f._temp.path().join("escape").exists());
        f.assert_not_installed();
    }
}

#[test]
fn rejects_case_aliases_and_zip_links() {
    let f = Fixture::new("release-a");
    let path = f._temp.path().join("bad.zip");
    zip_files(&path, &[("entry", b"1"), ("ENTRY", b"2")]);
    assert!(f.store.import(&path, &ImportOptions::default()).is_err());
    let mut archive = zip::ZipWriter::new(File::create(&path).unwrap());
    archive
        .add_symlink("link", "outside", zip::write::SimpleFileOptions::default())
        .unwrap();
    archive.finish().unwrap();
    assert!(f.store.import(path, &ImportOptions::default()).is_err());
    f.assert_not_installed();
}

#[test]
fn rejects_tar_symlink_and_hardlink_entries() {
    for kind in [tar::EntryType::Symlink, tar::EntryType::Link] {
        let f = Fixture::new("release-a");
        let path = f._temp.path().join("bad.tar.gz");
        let encoder = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::default(),
        );
        let mut archive = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_entry_type(kind);
        header.set_link_name("../outside").unwrap();
        header.set_cksum();
        archive
            .append_data(&mut header, "link", std::io::empty())
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
        assert!(f.store.import(path, &ImportOptions::default()).is_err());
        f.assert_not_installed();
    }
}

#[test]
fn rejects_truncated_archives_and_compressed_size_limit() {
    let f = Fixture::new("release-a");
    let zip_path = f._temp.path().join("truncated.zip");
    zip_files(&zip_path, &[("entry", b"data")]);
    assert!(f
        .store
        .import(
            &zip_path,
            &ImportOptions {
                max_archive_bytes: 1,
                ..ImportOptions::default()
            }
        )
        .is_err());
    let zip = fs::read(&zip_path).unwrap();
    fs::write(&zip_path, &zip[..zip.len() / 2]).unwrap();
    assert!(f
        .store
        .import(&zip_path, &ImportOptions::default())
        .is_err());
    let tar_path = f._temp.path().join("truncated.tar.gz");
    let encoder = flate2::write::GzEncoder::new(
        File::create(&tar_path).unwrap(),
        flate2::Compression::default(),
    );
    let mut archive = tar::Builder::new(encoder);
    archive.append_dir_all("client", &f.source).unwrap();
    archive.into_inner().unwrap().finish().unwrap();
    let tar = fs::read(&tar_path).unwrap();
    fs::write(&tar_path, &tar[..tar.len() - 4]).unwrap();
    assert!(f
        .store
        .import(&tar_path, &ImportOptions::default())
        .is_err());
    f.assert_not_installed();
}

#[cfg(unix)]
#[test]
fn rejects_directory_symlinks() {
    let f = Fixture::new("release-a");
    std::os::unix::fs::symlink(&f.source, f.source.join("loop")).unwrap();
    assert!(f.import().is_err());
    f.assert_not_installed();
}

#[test]
fn activation_revalidates_files_and_preserves_previous_release() {
    let f = Fixture::new("release-a");
    f.import().unwrap();
    f.store.commit_current("release-a").unwrap();
    f.edit(|m| m.release_id = "release-b".into());
    let second = f.import().unwrap();
    let selected = f.store.commit_current("release-b").unwrap();
    assert_eq!(selected.previous_release_id.as_deref(), Some("release-a"));
    f.store.commit_current("release-a").unwrap();
    let executable = second.directory.join(&second.manifest.files[0].path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(executable, b"corrupt").unwrap();
    assert!(f.store.commit_current("release-b").is_err());
    assert_eq!(f.store.current().unwrap().unwrap().release_id, "release-a");
}

#[test]
fn journal_recovers_both_sides_of_atomic_commit() {
    for published in [false, true] {
        let f = Fixture::new("release-a");
        f.import().unwrap();
        let after = CurrentRelease {
            release_id: "release-a".into(),
            previous_release_id: None,
        };
        let journal = TransactionJournal {
            phase: TransactionPhase::Prepared,
            before: None,
            after: Some(after.clone()),
        };
        fs::write(
            f.store.root().join("transaction.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        if published {
            fs::write(
                f.store.root().join("current.json"),
                serde_json::to_vec(&after).unwrap(),
            )
            .unwrap();
        }
        f.store.recover().unwrap();
        if published {
            assert_eq!(f.store.current().unwrap(), Some(after));
            assert_eq!(
                f.store.journal().unwrap().unwrap().phase,
                TransactionPhase::Committed
            );
        } else {
            assert!(f.store.current().unwrap().is_none());
            assert!(f.store.journal().unwrap().is_none());
        }
    }
}

#[test]
fn journal_disagreement_is_not_silently_repaired() {
    let f = Fixture::new("release-a");
    f.import().unwrap();
    f.store.commit_current("release-a").unwrap();
    let other = CurrentRelease {
        release_id: "unexpected".into(),
        previous_release_id: None,
    };
    fs::write(
        f.store.root().join("current.json"),
        serde_json::to_vec(&other).unwrap(),
    )
    .unwrap();
    assert!(f.store.recover().is_err());
}

#[test]
fn clear_current_is_a_recoverable_next_launch_reset() {
    let f = Fixture::new("a");
    let installed = f.import().unwrap();
    f.store.commit_current("a").unwrap();
    f.store.clear_current().unwrap();
    assert!(f.store.current().unwrap().is_none());
    assert!(f.store.resolve_current().unwrap().is_none());
    assert!(
        installed.directory.is_dir(),
        "reset must not delete a running release"
    );
    let journal = f.store.journal().unwrap().unwrap();
    assert_eq!(journal.phase, TransactionPhase::Committed);
    assert!(journal.after.is_none());
    assert_eq!(journal.before.unwrap().release_id, "a");
    assert_eq!(
        fs::read(f.store.root().join("current.json")).unwrap(),
        b"null"
    );
    f.store.clear_current().unwrap();
    f.store.commit_current("a").unwrap();
    assert_eq!(
        f.store
            .resolve_current()
            .unwrap()
            .unwrap()
            .manifest
            .release_id,
        "a"
    );
}

#[test]
fn clear_current_recovers_before_and_after_publishing_null() {
    for published in [false, true] {
        let f = Fixture::new("a");
        f.import().unwrap();
        let before = f.store.commit_current("a").unwrap();
        let journal = TransactionJournal {
            phase: TransactionPhase::Prepared,
            before: Some(before.clone()),
            after: None,
        };
        fs::write(
            f.store.root().join("transaction.json"),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        if published {
            fs::write(f.store.root().join("current.json"), b"null").unwrap();
        }
        f.store.recover().unwrap();
        if published {
            assert!(f.store.current().unwrap().is_none());
            assert_eq!(
                f.store.journal().unwrap().unwrap().phase,
                TransactionPhase::Committed
            );
        } else {
            assert_eq!(f.store.current().unwrap(), Some(before));
            assert!(f.store.journal().unwrap().is_none());
        }
        assert!(f.store.release_dir("a").unwrap().is_dir());
    }
}

#[test]
fn startup_resolution_rechecks_compatibility_and_content() {
    let f = Fixture::new("a");
    let installed = f.import().unwrap();
    f.store.commit_current("a").unwrap();
    assert_eq!(
        f.store.resolve_current().unwrap().unwrap().directory,
        installed.directory
    );
    let mut manifest = installed.manifest.clone();
    manifest.host_protocol.min = HOST_PROTOCOL_VERSION + 1;
    manifest.host_protocol.max = HOST_PROTOCOL_VERSION + 1;
    fs::write(
        installed.directory.join(MANIFEST_FILE),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(f
        .store
        .resolve_current()
        .unwrap_err()
        .to_string()
        .contains("protocol"));
    fs::write(
        installed.directory.join(MANIFEST_FILE),
        serde_json::to_vec(&installed.manifest).unwrap(),
    )
    .unwrap();
    let executable = installed.directory.join(&manifest.files[0].path);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(executable, b"changed after activation").unwrap();
    assert!(f.store.resolve_current().is_err());
}

#[test]
fn gc_keeps_live_draining_current_previous_and_explicit_rollback() {
    let f = Fixture::new("a");
    for id in ["a", "b", "c", "draining", "rollback", "unused"] {
        f.edit(|m| m.release_id = id.into());
        f.import().unwrap();
    }
    f.store.commit_current("a").unwrap();
    f.store.commit_current("b").unwrap();
    let references = ReleaseReferences {
        live: ["c".into(), "draining".into()].into(),
        rollback: ["rollback".into()].into(),
    };
    assert_eq!(f.store.gc(&references).unwrap(), ["unused"]);
    for id in ["a", "b", "c", "draining", "rollback"] {
        assert!(f.store.release_dir(id).unwrap().exists());
    }
}

#[test]
fn store_operations_fail_when_another_owner_holds_the_lock() {
    let f = Fixture::new("a");
    f.import().unwrap();
    let lock = File::options()
        .read(true)
        .write(true)
        .open(f.store.root().join("store.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(f.import().is_err());
    drop(lock);
    assert!(f.import().is_ok());
}

#[test]
fn rejects_malformed_manifest_and_unsafe_release_ids() {
    let f = Fixture::new("a");
    for id in ["..", "../outside", "CON", "trailing."] {
        f.edit(|m| m.release_id = id.into());
        assert!(f.import().is_err());
    }
    fs::write(f.source.join(MANIFEST_FILE), b"not-json").unwrap();
    assert!(f.import().is_err());
    f.assert_not_installed();
}
