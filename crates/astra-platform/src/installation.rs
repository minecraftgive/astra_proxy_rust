//! Fixed installation locations and operating-system launch entries.
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::ffi::OsString;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallScope {
    User,
    System,
}

#[derive(Debug, Clone)]
pub struct InstallPaths {
    pub scope: InstallScope,
    pub role: String,
    pub root: PathBuf,
    pub entry: PathBuf,
}

fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}
pub fn is_elevated() -> io::Result<bool> {
    #[cfg(windows)]
    {
        crate::elevation::is_elevated()
    }
    #[cfg(unix)]
    {
        Ok(unsafe { libc::geteuid() } == 0)
    }
}
pub fn account_name() -> String {
    #[cfg(windows)]
    {
        crate::windows_service::security::current_user_sid()
            .unwrap_or_else(|_| "unknown Windows account".into())
    }
    #[cfg(unix)]
    {
        format!("uid {}", unsafe { libc::geteuid() })
    }
}
pub fn has_permission(scope: InstallScope) -> io::Result<bool> {
    Ok(scope == InstallScope::User || is_elevated()?)
}

pub fn layout(scope: InstallScope, role: &str) -> io::Result<InstallPaths> {
    if !matches!(role, "client" | "server" | "controller" | "web") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Unknown installation role",
        ));
    }
    #[cfg(windows)]
    let (base, entry_dir) = {
        use windows_sys::Win32::UI::Shell::{
            FOLDERID_CommonPrograms, FOLDERID_LocalAppData, FOLDERID_ProgramFiles,
            FOLDERID_Programs,
        };
        match scope {
            InstallScope::User => (
                known_folder(&FOLDERID_LocalAppData)?.join("Programs/Astra"),
                known_folder(&FOLDERID_Programs)?,
            ),
            InstallScope::System => (
                known_folder(&FOLDERID_ProgramFiles)?.join("Astra"),
                known_folder(&FOLDERID_CommonPrograms)?,
            ),
        }
    };
    #[cfg(unix)]
    let (base, entry_dir) = match scope {
        InstallScope::User => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .ok_or_else(|| denied("User installation requires an absolute home directory"))?;
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::symlink_metadata(&home)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err(denied("User installation home must be a real directory owned by the executing account"));
            }
            #[cfg(target_os = "macos")]
            let base = home.join("Library/Application Support/Astra");
            #[cfg(not(target_os = "macos"))]
            let base = home.join(".local/share/astra");
            (base, home.join(".local/bin"))
        }
        InstallScope::System => {
            #[cfg(target_os = "macos")]
            let base = PathBuf::from("/Library/Application Support/Astra");
            #[cfg(not(target_os = "macos"))]
            let base = PathBuf::from("/opt/astra");
            (base, PathBuf::from("/usr/local/bin"))
        }
    };
    #[cfg(windows)]
    let name = format!(
        "Astra {}.lnk",
        match role {
            "client" => "Client",
            "server" => "Server",
            "controller" => "Controller",
            _ => "Web",
        }
    );
    #[cfg(unix)]
    let name = format!("astra-{role}");
    Ok(InstallPaths {
        scope,
        role: role.into(),
        root: base.join(role),
        entry: entry_dir.join(name),
    })
}

/// GUI packages publish native desktop entries; terminal-only packages keep their existing paths.
pub fn layout_for_frontend(
    scope: InstallScope,
    role: &str,
    graphical: bool,
) -> io::Result<InstallPaths> {
    let paths = layout(scope, role)?;
    if !graphical {
        return Ok(paths);
    }
    if !matches!(role, "client" | "server") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "This role has no graphical frontend",
        ));
    }
    #[cfg(windows)]
    {
        Ok(paths)
    }
    #[cfg(unix)]
    {
        let mut paths = paths;
        #[cfg(target_os = "macos")]
        let directory = match scope {
            InstallScope::User => {
                PathBuf::from(std::env::var_os("HOME").unwrap()).join("Applications")
            }
            InstallScope::System => PathBuf::from("/Applications"),
        };
        #[cfg(not(target_os = "macos"))]
        let directory = match scope {
            InstallScope::User => {
                PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/share/applications")
            }
            InstallScope::System => PathBuf::from("/usr/local/share/applications"),
        };
        #[cfg(target_os = "macos")]
        {
            paths.entry = directory
                .join(format!("{}.app", gui_title(role)))
                .join("Contents/MacOS/astra-launch");
        }
        #[cfg(not(target_os = "macos"))]
        {
            paths.entry = directory.join(format!("astra-{role}.desktop"));
        }
        Ok(paths)
    }
}
#[cfg(unix)]
fn gui_title(role: &str) -> &'static str {
    if role == "server" {
        "Astra Server"
    } else {
        "Astra"
    }
}
#[cfg(unix)]
fn desktop_entry(paths: &InstallPaths) -> bool {
    paths
        .entry
        .extension()
        .is_some_and(|extension| extension == "desktop")
}
#[cfg(target_os = "macos")]
fn app_contents(paths: &InstallPaths) -> Option<&Path> {
    let macos = paths.entry.parent()?;
    let contents = macos.parent()?;
    (paths.entry.file_name()? == "astra-launch"
        && macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && contents.parent()?.extension()? == "app")
        .then_some(contents)
}
#[cfg(unix)]
fn desktop(paths: &InstallPaths, executable: &Path) -> io::Result<Vec<u8>> {
    let path = executable.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Installation path is not UTF-8",
        )
    })?;
    if path.contains(['\n', '\r']) {
        return Err(denied("Installation path contains a line break"));
    }
    // Desktop Exec is parsed directly, not by a shell; escape both desktop and field-code syntax.
    let quoted = path
        .replace('\\', r"\\\\")
        .replace('"', r#"\\\""#)
        .replace('`', r"\\`")
        .replace('$', r"\\$")
        .replace('%', "%%");
    let icon = paths.root.join("icon.svg");
    let icon = icon.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Installation path is not UTF-8",
        )
    })?;
    if icon.contains(['\n', '\r']) {
        return Err(denied("Installation path contains a line break"));
    }
    Ok(format!("[Desktop Entry]\n# {}\nType=Application\nName={}\nExec=\"{}\"\nIcon={}\nTerminal=false\nCategories=Network;Utility;\n",marker(paths),gui_title(&paths.role),quoted,icon).into_bytes())
}
#[cfg(target_os = "macos")]
fn app_info(paths: &InstallPaths) -> Vec<u8> {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- {} -->\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>CFBundleName</key><string>{}</string><key>CFBundleIdentifier</key><string>org.astra.installed.{}</string><key>CFBundleExecutable</key><string>astra-launch</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleIconFile</key><string>Astra.icns</string><key>NSHighResolutionCapable</key><true/><key>LSUIElement</key><false/></dict></plist>\n",marker(paths),gui_title(&paths.role),paths.role).into_bytes()
}
#[cfg(unix)]
const GUI_ICON_SVG: &[u8] = include_bytes!("../../astra-gui/assets/icon.svg");
#[cfg(target_os = "macos")]
const GUI_ICON_ICNS: &[u8] = include_bytes!("../../astra-gui/assets/icon.icns");
#[cfg(unix)]
fn native_assets_match(paths: &InstallPaths) -> io::Result<bool> {
    if desktop_entry(paths) {
        return Ok(
            read_plain_file(&paths.root.join("icon.svg"), paths.scope, 1024 * 1024)
                .is_ok_and(|bytes| bytes == GUI_ICON_SVG),
        );
    }
    #[cfg(target_os = "macos")]
    if let Some(contents) = app_contents(paths) {
        return Ok(
            read_plain_file(&contents.join("Info.plist"), paths.scope, 64 * 1024)
                .is_ok_and(|bytes| bytes == app_info(paths))
                && read_plain_file(
                    &contents.join("Resources/Astra.icns"),
                    paths.scope,
                    1024 * 1024,
                )
                .is_ok_and(|bytes| bytes == GUI_ICON_ICNS),
        );
    }
    Ok(true)
}
#[cfg(unix)]
fn publish_native_assets(paths: &InstallPaths) -> io::Result<()> {
    if desktop_entry(paths) {
        atomic_file(
            &paths.root.join("icon.svg"),
            GUI_ICON_SVG,
            paths.scope,
            false,
        )?;
    }
    #[cfg(target_os = "macos")]
    if let Some(contents) = app_contents(paths) {
        atomic_file(
            &contents.join("Info.plist"),
            &app_info(paths),
            paths.scope,
            false,
        )?;
        atomic_file(
            &contents.join("Resources/Astra.icns"),
            GUI_ICON_ICNS,
            paths.scope,
            false,
        )?;
    }
    Ok(())
}

#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY},
    };
    let mut pointer = std::ptr::null_mut();
    let code = unsafe {
        SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut pointer,
        )
    };
    if code < 0 || pointer.is_null() {
        return Err(io::Error::other(format!(
            "Cannot locate installation folder (HRESULT {code:#x})"
        )));
    }
    let mut len = 0;
    unsafe {
        while *pointer.add(len) != 0 {
            len += 1;
        }
    }
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(pointer, len)
    }));
    unsafe { CoTaskMemFree(pointer.cast()) };
    Ok(path)
}

/// Check every existing component before creating or writing installation data.
pub fn validate_destination(path: &Path, scope: InstallScope) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(denied(
            "Installation destination must be absolute without parent traversal",
        ));
    }
    for item in path.ancestors() {
        let metadata = match fs::symlink_metadata(item) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() {
            return Err(denied(format!(
                "Installation destination contains a link: {}",
                item.display()
            )));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(denied("Installation destination contains a reparse point"));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if scope == InstallScope::System
                && (metadata.uid() != 0 || metadata.mode() & 0o022 != 0)
            {
                return Err(denied(format!(
                    "System installation destination is not root-controlled: {}",
                    item.display()
                )));
            }
        }
    }
    #[cfg(windows)]
    if scope == InstallScope::System {
        if let Some(existing) = path.ancestors().find(|item| item.exists()) {
            drop(crate::windows_service::security::pin_protected(existing)?);
        }
    }
    Ok(())
}

pub fn prepare_directory(path: &Path, scope: InstallScope) -> io::Result<()> {
    validate_destination(path, scope)?;
    if !has_permission(scope)? {
        return Err(denied(
            "System installation requires local administrator approval",
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_dir() {
            return Err(denied("Installation directory is not a directory"));
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| denied("Installation directory has no parent"))?;
    prepare_directory(parent, scope)?;
    #[cfg(windows)]
    {
        if scope == InstallScope::System {
            return crate::windows_service::security::prepare_admin_directory(path);
        }
        crate::elevation::create_private_directory(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(if scope == InstallScope::System {
                0o755
            } else {
                0o700
            })
            .create(path)?;
    }
    validate_destination(path, scope)
}

/// Unverified copies remain inaccessible to other accounts until every digest passes.
pub fn prepare_staging_directory(path: &Path, scope: InstallScope) -> io::Result<()> {
    prepare_directory(path, scope)?;
    #[cfg(windows)]
    if scope == InstallScope::System {
        crate::windows_service::security::secure_admin_directory(path, false)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn finish_staging_directory(path: &Path, scope: InstallScope) -> io::Result<()> {
    validate_destination(path, scope)?;
    #[cfg(windows)]
    if scope == InstallScope::System {
        crate::windows_service::security::secure_admin_directory(path, true)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if scope == InstallScope::System {
                0o755
            } else {
                0o700
            }),
        )?;
    }
    Ok(())
}

/// Open source bytes without following a final link; check the opened object,
/// rather than relying on a pathname check before opening it.
pub fn open_package_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(denied("Package payload is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(denied("Package payload has multiple hard links"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || info.nNumberOfLinks != 1 {
            return Err(denied("Package payload is a link or reparse point"));
        }
    }
    Ok(file)
}

/// Apply the intended ACL/mode to new files, never to pre-existing unrelated files.
pub fn protect_file(path: &Path, scope: InstallScope, executable: bool) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = executable;
        if scope == InstallScope::System {
            crate::windows_service::security::secure_admin_file(path)?;
        } else {
            crate::platform::set_private(path, 0o600)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = match (scope, executable) {
            (InstallScope::System, true) => 0o755,
            (InstallScope::System, false) => 0o644,
            (InstallScope::User, true) => 0o700,
            _ => 0o600,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

pub fn protect_staging_file(path: &Path, scope: InstallScope, executable: bool) -> io::Result<()> {
    #[cfg(unix)]
    let _ = scope;
    #[cfg(windows)]
    if scope == InstallScope::System {
        return crate::windows_service::security::secure_admin_private_file(path);
    }
    // On Unix every staging directory stays 0700; private files add defence in depth.
    protect_file(path, InstallScope::User, executable)
}

pub fn read_plain_file(path: &Path, scope: InstallScope, limit: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    validate_destination(path, scope)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(denied("Invalid installation metadata file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(denied("Installation metadata has multiple links"));
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut bytes = Vec::new();
    let file = options.open(path)?;
    #[cfg(windows)]
    if scope == InstallScope::User {
        crate::elevation::validate_user_file(&file, path)?;
    }
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(denied("Installation metadata exceeds limit"));
    }
    Ok(bytes)
}

pub fn atomic_file(
    path: &Path,
    bytes: &[u8],
    scope: InstallScope,
    executable: bool,
) -> io::Result<()> {
    atomic_file_with_visibility(path, bytes, scope, executable, true)
}
pub fn atomic_staging_file(path: &Path, bytes: &[u8], scope: InstallScope) -> io::Result<()> {
    atomic_file_with_visibility(path, bytes, scope, false, false)
}
fn atomic_file_with_visibility(
    path: &Path,
    bytes: &[u8],
    scope: InstallScope,
    executable: bool,
    readable: bool,
) -> io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| denied("Installation file has no parent"))?;
    prepare_directory(parent, scope)?;
    validate_destination(path, scope)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".astra-install-")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    if readable {
        protect_file(temporary.path(), scope, executable)?;
    } else {
        protect_staging_file(temporary.path(), scope, executable)?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn marker(paths: &InstallPaths) -> String {
    format!("Astra managed installation v1: {}", paths.role)
}

#[cfg(unix)]
fn launcher(paths: &InstallPaths, executable: &Path) -> io::Result<Vec<u8>> {
    if desktop_entry(paths) {
        return desktop(paths, executable);
    }
    let text = executable.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Installation path is not UTF-8",
        )
    })?;
    Ok(format!(
        "#!/bin/sh\n# {}\nexec '{}' \"$@\"\n",
        marker(paths),
        text.replace('\'', "'\\''")
    )
    .into_bytes())
}

/// A collision with an unrelated executable/shortcut is never overwritten.
pub fn check_entry(paths: &InstallPaths) -> io::Result<()> {
    validate_destination(&paths.entry, paths.scope)?;
    #[cfg(target_os = "macos")]
    if let Some(contents) = app_contents(paths) {
        let bundle = contents.parent().unwrap();
        if bundle.try_exists()? {
            // An existing bundle is ours only when its own metadata carries our marker.
            let info = read_plain_file(&contents.join("Info.plist"), paths.scope, 64 * 1024)?;
            if info != app_info(paths) {
                return Err(denied(
                    "Installation application belongs to another program",
                ));
            }
        }
    }
    match fs::symlink_metadata(&paths.entry) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    #[cfg(unix)]
    {
        let bytes = read_plain_file(&paths.entry, paths.scope, 64 * 1024)?;
        let prefix = format!(
            "{}\n# {}\n",
            if desktop_entry(paths) {
                "[Desktop Entry]"
            } else {
                "#!/bin/sh"
            },
            marker(paths)
        );
        if !bytes.starts_with(prefix.as_bytes()) {
            return Err(denied("Installation entry belongs to another program"));
        }
    }
    #[cfg(windows)]
    {
        let (_, description, _) = windows_entry::read(&paths.entry)?;
        if description != marker(paths) {
            return Err(denied("Installation shortcut belongs to another program"));
        }
    }
    Ok(())
}
pub fn entry_matches(paths: &InstallPaths, executable: &Path) -> io::Result<bool> {
    if !paths.entry.try_exists()? {
        return Ok(false);
    }
    check_entry(paths)?;
    #[cfg(unix)]
    if !executable_ready(&paths.entry, paths.scope)? {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        Ok(
            read_plain_file(&paths.entry, paths.scope, 64 * 1024)? == launcher(paths, executable)?
                && native_assets_match(paths)?,
        )
    }
    #[cfg(windows)]
    {
        let (target, _, no_arguments) = windows_entry::read(&paths.entry)?;
        Ok(target == executable && no_arguments)
    }
}

pub fn executable_ready(path: &Path, scope: InstallScope) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = if scope == InstallScope::System {
            0
        } else {
            unsafe { libc::geteuid() }
        };
        let mode = if scope == InstallScope::System {
            0o755
        } else {
            0o700
        };
        Ok(metadata.uid() == uid && metadata.mode() & 0o7777 == mode && metadata.nlink() == 1)
    }
    #[cfg(windows)]
    {
        let _ = scope;
        use std::os::windows::fs::MetadataExt;
        Ok(metadata.file_attributes() & 0x400 == 0)
    }
}
pub fn publish_entry(paths: &InstallPaths, executable: &Path) -> io::Result<()> {
    check_entry(paths)?;
    #[cfg(unix)]
    {
        publish_native_assets(paths)?;
        atomic_file(
            &paths.entry,
            &launcher(paths, executable)?,
            paths.scope,
            true,
        )
    }
    #[cfg(windows)]
    {
        let parent = paths
            .entry
            .parent()
            .ok_or_else(|| denied("Shortcut has no parent"))?;
        prepare_directory(parent, paths.scope)?;
        let temporary = tempfile::Builder::new()
            .prefix(".astra-shortcut-")
            .suffix(".lnk")
            .tempfile_in(parent)?
            .into_temp_path();
        windows_entry::write(&temporary, executable, &marker(paths))?;
        let bytes = fs::read(&temporary)?;
        check_entry(paths)?;
        atomic_file(&paths.entry, &bytes, paths.scope, false)
    }
}

#[cfg(windows)]
mod windows_entry {
    use super::*;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::{
        core::{Interface, PCWSTR},
        Win32::{
            System::Com::{
                CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, STGM_READ,
            },
            UI::Shell::{IShellLinkW, ShellLink},
        },
    };
    fn error(error: windows::core::Error) -> io::Error {
        io::Error::other(error.to_string())
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() }
        }
    }
    fn wide(path: &Path) -> Vec<u16> {
        let value = path.as_os_str().to_string_lossy();
        let path = value
            .strip_prefix(r"\\?\")
            .filter(|rest| rest.as_bytes().get(1) == Some(&b':'))
            .map(OsString::from)
            .unwrap_or_else(|| path.as_os_str().to_owned());
        path.encode_wide().chain(Some(0)).collect()
    }
    fn in_apartment<T: Send>(work: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
                        .ok()
                        .map_err(error)?;
                    let _apartment = Apartment;
                    work()
                })
                .join()
                .map_err(|_| io::Error::other("Shortcut worker panicked"))?
        })
    }
    pub(super) fn write(path: &Path, executable: &Path, description: &str) -> io::Result<()> {
        in_apartment(|| unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(error)?;
            link.SetPath(PCWSTR(wide(executable).as_ptr()))
                .map_err(error)?;
            link.SetIconLocation(PCWSTR(wide(executable).as_ptr()), 0)
                .map_err(error)?;
            let description: Vec<u16> = description.encode_utf16().chain(Some(0)).collect();
            link.SetDescription(PCWSTR(description.as_ptr()))
                .map_err(error)?;
            if let Some(parent) = executable.parent() {
                link.SetWorkingDirectory(PCWSTR(wide(parent).as_ptr()))
                    .map_err(error)?;
            }
            let file: IPersistFile = link.cast().map_err(error)?;
            file.Save(PCWSTR(wide(path).as_ptr()), true).map_err(error)
        })
    }
    pub(super) fn read(path: &Path) -> io::Result<(PathBuf, String, bool)> {
        in_apartment(|| unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(error)?;
            let file: IPersistFile = link.cast().map_err(error)?;
            file.Load(PCWSTR(wide(path).as_ptr()), STGM_READ)
                .map_err(error)?;
            let mut target = vec![0u16; 32768];
            link.GetPath(&mut target, std::ptr::null_mut(), 0)
                .map_err(error)?;
            let mut description = vec![0u16; 1024];
            link.GetDescription(&mut description).map_err(error)?;
            let mut arguments = vec![0u16; 32768];
            link.GetArguments(&mut arguments).map_err(error)?;
            let target = PathBuf::from(OsString::from_wide(
                &target[..target.iter().position(|c| *c == 0).unwrap_or(target.len())],
            ));
            let description = String::from_utf16_lossy(
                &description[..description
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(description.len())],
            );
            Ok((target, description, arguments[0] == 0))
        })
    }
}

#[cfg(test)]
#[path = "test/installation_test.rs"]
mod tests;
