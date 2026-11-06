//! The persistent broker only executes administrator-controlled installation files.
//! These checks are deliberately stricter than the per-launch UAC warning policy.
use std::{
    ffi::{OsStr, OsString},
    io::{self, Write},
    mem::{size_of, zeroed},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        LocalFree, GENERIC_ALL, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            ConvertStringSidToSidW, GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT,
            SE_KERNEL_OBJECT,
        },
        GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
        TokenElevationType, TokenElevationTypeFull, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW,
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
        FILE_WRITE_DATA, FILE_WRITE_EA, OPEN_EXISTING, READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC,
        WRITE_OWNER,
    },
    System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    },
};

const SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}
fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
fn status(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}
fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut data: Vec<_> = value.encode_wide().collect();
    if data.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in protected path or SID",
        ));
    }
    data.push(0);
    Ok(data)
}
unsafe fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}
struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}
fn sid_text(sid: PSID) -> io::Result<String> {
    let mut text = null_mut();
    check(unsafe { ConvertSidToStringSidW(sid, &mut text) })?;
    let _allocation = LocalAllocation(text.cast());
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            text, length,
        )))
    }
}
fn process_user(process: HANDLE) -> io::Result<String> {
    let mut token = null_mut();
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) })?;
    let token = unsafe { owned(token) }?;
    let mut length = 0;
    unsafe {
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length);
    }
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut data = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            data.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    })?;
    sid_text(unsafe { &*data.as_ptr().cast::<TOKEN_USER>() }.User.Sid)
}
pub(crate) fn current_user_sid() -> io::Result<String> {
    process_user(unsafe { GetCurrentProcess() })
}
/// Enrollment is for the installing administrator's own split-token account.
/// An over-the-shoulder credential prompt must never authorize a different user.
pub(super) fn validate_sid(owner: &str) -> io::Result<()> {
    let mut sid = null_mut();
    check(unsafe { ConvertStringSidToSidW(wide(OsStr::new(owner))?.as_ptr(), &mut sid) })?;
    let _allocation = LocalAllocation(sid);
    if sid_text(sid)? != owner || owner == SYSTEM {
        return Err(denied("A canonical Windows user SID is required"));
    }
    Ok(())
}
pub(super) fn validate_administrator_sid(owner: &str) -> io::Result<()> {
    validate_sid(owner)?;
    if owner != current_user_sid()? || !crate::elevation::is_elevated()? {
        return Err(denied(
            "Service changes require elevation by the registered Windows account",
        ));
    }
    Ok(())
}
pub(super) fn validate_owner_sid(owner: &str) -> io::Result<()> {
    validate_administrator_sid(owner)?;
    let mut token = null_mut();
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) })?;
    let token = unsafe { owned(token) }?;
    let (mut elevation, mut size) = (0i32, 0u32);
    check(unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevationType,
            (&mut elevation as *mut i32).cast(),
            size_of::<i32>() as u32,
            &mut size,
        )
    })?;
    if elevation != TokenElevationTypeFull {
        return Err(denied(
            "Service enrollment requires a UAC administrator account with a linked standard token",
        ));
    }
    Ok(())
}

fn service_read_descriptor(owner: &str, rights: u32) -> io::Result<LocalAllocation> {
    validate_sid(owner)?;
    let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x{rights:08X};;;{owner})");
    let mut descriptor = null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(OsStr::new(&sddl))?.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    })?;
    Ok(LocalAllocation(descriptor))
}
fn grant_service_read(handle: HANDLE, owner: &str, rights: u32) -> io::Result<()> {
    let descriptor = service_read_descriptor(owner, rights)?;
    let (mut defaulted, mut present, mut acl) = (0, 0, null_mut());
    check(unsafe {
        GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
    })?;
    if present == 0 || acl.is_null() {
        return Err(denied("Missing service verification ACL"));
    }
    status(unsafe {
        SetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        )
    })
}
/// SYSTEM processes and tokens do not normally permit queries by a medium
/// integrity client. Allow only the reads needed to authenticate this broker;
/// never delegate process duplication, token duplication or mutation rights.
pub(super) fn allow_service_verification(owner: &str) -> io::Result<()> {
    if current_user_sid()? != SYSTEM {
        return Err(denied(
            "Only the SYSTEM broker can grant verification access",
        ));
    }
    grant_service_read(
        unsafe { GetCurrentProcess() },
        owner,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
    )?;
    let mut token = null_mut();
    check(unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | WRITE_DAC, &mut token) })?;
    let token = unsafe { owned(token) }?;
    grant_service_read(token.as_raw_handle(), owner, TOKEN_QUERY)
}

fn local_absolute(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(denied(
            "Protected installation paths must be absolute and contain no parent traversal",
        ));
    }
    if !matches!(path.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
    {
        return Err(denied(
            "Protected installation paths must be on a local Windows drive",
        ));
    }
    Ok(path.to_owned())
}
fn open(path: &Path, access: u32, share: u32) -> io::Result<OwnedHandle> {
    unsafe {
        owned(CreateFileW(
            wide(path.as_os_str())?.as_ptr(),
            access,
            share,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        ))
    }
}
fn information(handle: HANDLE) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = unsafe { zeroed() };
    check(unsafe { GetFileInformationByHandle(handle, &mut info) })?;
    Ok(info)
}
fn final_path(handle: HANDLE) -> io::Result<PathBuf> {
    let mut data = vec![0u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle,
                data.as_mut_ptr(),
                data.len() as u32,
                VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < data.len() {
            return Ok(PathBuf::from(OsString::from_wide(&data[..length])));
        }
        data.resize(length + 1, 0);
    }
}
fn administrator(sid: &str) -> bool {
    matches!(sid, SYSTEM | ADMINISTRATORS | TRUSTED_INSTALLER)
}
fn trusted_writer(sid: &str) -> bool {
    // These owner aliases are safe only after the object's owner was validated.
    administrator(sid) || matches!(sid, "S-1-3-0" | "S-1-3-4")
}
fn check_acl(owner: PSID, acl: *const ACL, leaf: bool) -> io::Result<()> {
    if owner.is_null() || !administrator(&sid_text(owner)?) {
        return Err(denied(
            "Protected service files and ancestors must be owned by Administrators or SYSTEM",
        ));
    }
    if acl.is_null() {
        return Err(denied("Protected service path has an unrestricted ACL"));
    }
    // Ancestors may allow creating unrelated children (e.g. C:\), but never
    // replacing this subtree or changing its ownership/security/reparse data.
    let dangerous = WRITE_DAC
        | WRITE_OWNER
        | GENERIC_ALL
        | GENERIC_WRITE
        | DELETE
        | FILE_DELETE_CHILD
        | FILE_WRITE_ATTRIBUTES
        | if leaf {
            FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA
        } else {
            0
        };
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let mut ace = null_mut();
        check(unsafe { GetAce(acl, index, &mut ace) })?;
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match header.AceType {
            // Deny ACEs cannot grant access. Ignore them conservatively; an
            // overlapping allow still fails rather than approximating AccessCheck.
            1 | 6 | 10 | 12 => continue,
            0 | 9 => {
                if usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
                    return Err(denied("Malformed installation ACL"));
                }
                let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
                let sid =
                    unsafe { std::ptr::addr_of!((*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart) }
                        .cast_mut()
                        .cast();
                if allowed.Mask & dangerous != 0 && !trusted_writer(&sid_text(sid)?) {
                    return Err(denied(
                        "Protected service path is writable or replaceable by a non-administrator",
                    ));
                }
            }
            // Object/conditional grant layouts are not guessed at this trust
            // boundary. Our installed ACL uses only ordinary allowed ACEs.
            _ => {
                return Err(denied(
                    "Unsupported access grant in protected installation ACL",
                ))
            }
        }
    }
    Ok(())
}
fn check_object(handle: HANDLE, leaf: bool) -> io::Result<()> {
    let info = information(handle)?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(denied("Service installation cannot contain reparse points"));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 && info.nNumberOfLinks != 1 {
        return Err(denied(
            "Protected service files cannot have additional hard links",
        ));
    }
    let (mut owner, mut acl, mut descriptor) = (null_mut(), null_mut(), null_mut());
    status(unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut acl,
            null_mut(),
            &mut descriptor,
        )
    })?;
    let _allocation = LocalAllocation(descriptor);
    check_acl(owner, acl, leaf)
}

pub(crate) struct ProtectedPath {
    path: PathBuf,
    _handles: Vec<OwnedHandle>,
}
impl ProtectedPath {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}
/// Pin each ancestor against rename, and the final file against writes, until
/// the caller finishes launching/loading it. Validate from the volume root down.
pub(crate) fn pin_protected(path: &Path) -> io::Result<ProtectedPath> {
    let path = local_absolute(path)?;
    let ancestors: Vec<_> = path.ancestors().collect();
    let mut handles = Vec::new();
    for (index, item) in ancestors.iter().rev().enumerate() {
        let leaf = index + 1 == ancestors.len();
        let handle = open(
            item,
            READ_CONTROL | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | if leaf { 0 } else { FILE_SHARE_WRITE },
        )?;
        check_object(handle.as_raw_handle(), leaf)?;
        if !leaf
            && information(handle.as_raw_handle())?.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(denied("Service installation ancestor is not a directory"));
        }
        handles.push(handle);
    }
    let path = final_path(
        handles
            .last()
            .ok_or_else(|| denied("Empty installation path"))?
            .as_raw_handle(),
    )?;
    Ok(ProtectedPath {
        path,
        _handles: handles,
    })
}
pub(super) fn validate_protected(path: &Path) -> io::Result<PathBuf> {
    Ok(pin_protected(path)?.path)
}

fn install_descriptor(user: &str) -> io::Result<LocalAllocation> {
    installation_descriptor(user, true)
}
fn installation_descriptor(user: &str, readable: bool) -> io::Result<LocalAllocation> {
    // Owner is never the installing user's SID: a user owner can rewrite the
    // DACL even when its explicit ACE only grants read/execute.
    // The shared Program Files ancestor must be readable by every enrolled
    // account. These immutable executables contain no per-user secrets; only
    // the per-user pipe grants permission to request a privileged launch.
    let sddl = if readable {
        format!(
            "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)(A;OICI;GRGX;;;{user})"
        )
    } else {
        "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)".into()
    };
    let mut descriptor = null_mut();
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(OsStr::new(&sddl))?.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    })?;
    Ok(LocalAllocation(descriptor))
}
pub(super) fn prepare_install_directory(path: &Path) -> io::Result<()> {
    let user = current_user_sid()?;
    validate_owner_sid(&user)?;
    prepare_admin_directory(path)
}
/// General system installation uses the same protected ACL without enrolling a broker.
pub(crate) fn prepare_admin_directory(path: &Path) -> io::Result<()> {
    if !crate::elevation::is_elevated()? {
        return Err(denied(
            "System installation requires administrator permission",
        ));
    }
    let user = current_user_sid()?;
    let path = local_absolute(path)?;
    if path.exists() {
        let checked = pin_protected(&path)?;
        if information(checked._handles.last().unwrap().as_raw_handle())?.dwFileAttributes
            & FILE_ATTRIBUTE_DIRECTORY
            == 0
        {
            return Err(denied("Service installation path is not a directory"));
        }
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| denied("Service installation has no parent"))?;
    if !parent.exists() {
        prepare_admin_directory(parent)?;
    }
    let _parent = pin_protected(parent)?;
    let descriptor = install_descriptor(&user)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    check(unsafe { CreateDirectoryW(wide(path.as_os_str())?.as_ptr(), &attributes) })?;
    validate_protected(&path)?;
    Ok(())
}
pub(super) fn secure_file(path: &Path) -> io::Result<()> {
    let user = current_user_sid()?;
    validate_owner_sid(&user)?;
    secure_admin_file(path)
}
pub(crate) fn secure_admin_file(path: &Path) -> io::Result<()> {
    secure_admin_object(path, false, true)
}
pub(crate) fn secure_admin_private_file(path: &Path) -> io::Result<()> {
    secure_admin_object(path, false, false)
}
pub(crate) fn secure_admin_directory(path: &Path, readable: bool) -> io::Result<()> {
    secure_admin_object(path, true, readable)
}
fn secure_admin_object(path: &Path, directory: bool, readable: bool) -> io::Result<()> {
    if !crate::elevation::is_elevated()? {
        return Err(denied(
            "System installation requires administrator permission",
        ));
    }
    let user = current_user_sid()?;
    let path = local_absolute(path)?;
    let _parent = pin_protected(
        path.parent()
            .ok_or_else(|| denied("Installed file has no parent"))?,
    )?;
    let handle = open(
        &path,
        READ_CONTROL | FILE_READ_ATTRIBUTES | WRITE_DAC | WRITE_OWNER,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )?;
    let info = information(handle.as_raw_handle())?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(denied("Installed file must be a regular, unlinked file"));
    }
    let descriptor = installation_descriptor(&user, readable)?;
    let (mut owner, mut defaulted, mut present, mut acl) = (null_mut(), 0, 0, null_mut());
    check(unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner, &mut defaulted) })?;
    check(unsafe {
        GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
    })?;
    if present == 0 || acl.is_null() {
        return Err(denied("Missing installation ACL"));
    }
    status(unsafe {
        SetSecurityInfo(
            handle.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            null_mut(),
            acl,
            null(),
        )
    })?;
    check_object(handle.as_raw_handle(), true)
}
/// Snapshot bytes without copying a user-controlled source security descriptor.
pub(super) fn copy_protected(source: &Path, destination: &Path) -> io::Result<()> {
    let user = current_user_sid()?;
    validate_owner_sid(&user)?;
    let parent = destination
        .parent()
        .ok_or_else(|| denied("Installed file has no parent"))?;
    let _parent = pin_protected(parent)?;
    if destination.exists() {
        validate_protected(destination)?;
    }
    let mut source = std::fs::File::open(source)?;
    let descriptor = install_descriptor(&user)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // Apply the trusted owner and protected ACL at creation. Tightening a
    // user-owned temporary file afterwards leaves an ACL-takeover race.
    let mut temporary =
        tempfile::Builder::new()
            .prefix(".astra-install-")
            .make_in(parent, |path| {
                let handle = unsafe {
                    owned(CreateFileW(
                        wide(path.as_os_str())?.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_DELETE,
                        &attributes,
                        CREATE_NEW,
                        FILE_ATTRIBUTE_NORMAL,
                        null_mut(),
                    ))
                }?;
                Ok(std::fs::File::from(handle))
            })?;
    secure_file(temporary.path())?;
    io::copy(&mut source, &mut temporary)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    validate_protected(destination)?;
    Ok(())
}

/// Authenticate a pipe server using a live process handle, not a claimed PID.
/// The caller compares this PID with both SCM's service PID and the pipe peer.
pub(super) fn verify_service_process(
    pid: u32,
    expected_executable: &Path,
) -> io::Result<OwnedHandle> {
    let expected = pin_protected(expected_executable)?;
    let process = unsafe { owned(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)) }?;
    if process_user(process.as_raw_handle())? != SYSTEM {
        return Err(denied("Broker pipe peer does not run as SYSTEM"));
    }
    let mut image = vec![0u16; 32768];
    let mut length = image.len() as u32;
    check(unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, image.as_mut_ptr(), &mut length)
    })?;
    let image = pin_protected(&PathBuf::from(OsString::from_wide(
        &image[..length as usize],
    )))?;
    let expected_info = information(expected._handles.last().unwrap().as_raw_handle())?;
    let image_info = information(image._handles.last().unwrap().as_raw_handle())?;
    if (
        image_info.dwVolumeSerialNumber,
        image_info.nFileIndexHigh,
        image_info.nFileIndexLow,
    ) != (
        expected_info.dwVolumeSerialNumber,
        expected_info.nFileIndexHigh,
        expected_info.nFileIndexLow,
    ) {
        return Err(denied(
            "Broker pipe peer is not the installed service executable",
        ));
    }
    Ok(process)
}

#[cfg(test)]
#[path = "../test/windows_service_security_test.rs"]
mod tests;
