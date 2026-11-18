//! Application-scoped WFP blocking. The Agent owns this session so loss of
//! the userspace proxy never releases the filters for a blocked application.
use std::{ffi::c_void, io, os::windows::ffi::OsStrExt, path::Path, ptr};
use windows_sys::Win32::{
    Foundation::HANDLE, NetworkManagement::WindowsFilteringPlatform::*,
    System::Rpc::RPC_C_AUTHN_WINNT,
};

fn checked(code: u32) -> io::Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

struct AppId(*mut FWP_BYTE_BLOB);
impl Drop for AppId {
    fn drop(&mut self) {
        unsafe { FwpmFreeMemory0((&mut self.0 as *mut *mut FWP_BYTE_BLOB).cast::<*mut c_void>()) }
    }
}

impl AppId {
    fn from_path(path: &Path) -> io::Result<Self> {
        let mut wide: Vec<_> = path.as_os_str().encode_wide().collect();
        if !path.is_absolute() || wide.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute executable path",
            ));
        }
        wide.push(0);
        let mut application = ptr::null_mut();
        checked(unsafe { FwpmGetAppIdFromFileName0(wide.as_ptr(), &mut application) })?;
        Ok(Self(application))
    }
}

struct Engine(HANDLE);
impl Engine {
    fn open() -> io::Result<Self> {
        let session = FWPM_SESSION0 {
            flags: FWPM_SESSION_FLAG_DYNAMIC,
            txnWaitTimeoutInMSec: 3000,
            ..Default::default()
        };
        let mut handle = ptr::null_mut();
        checked(unsafe {
            FwpmEngineOpen0(
                ptr::null(),
                RPC_C_AUTHN_WINNT,
                ptr::null(),
                &session,
                &mut handle,
            )
        })?;
        Ok(Self(handle))
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            FwpmEngineClose0(self.0);
        }
    }
}

/// A complete IPv4+IPv6 filter set owned by a dynamic WFP session.
pub struct BlockLease {
    _engine: Engine,
}

impl BlockLease {
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let application = AppId::from_path(path)?;
        let engine = Engine::open()?;
        let handle = engine.0;
        checked(unsafe { FwpmTransactionBegin0(handle, 0) })?;
        let mut title: Vec<u16> = "Astra process routing"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        for layer in [
            FWPM_LAYER_ALE_AUTH_CONNECT_V4,
            FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        ] {
            for protocol in [6, 17] {
                let mut conditions = [
                    FWPM_FILTER_CONDITION0 {
                        fieldKey: FWPM_CONDITION_ALE_APP_ID,
                        matchType: FWP_MATCH_EQUAL,
                        conditionValue: FWP_CONDITION_VALUE0 {
                            r#type: FWP_BYTE_BLOB_TYPE,
                            Anonymous: FWP_CONDITION_VALUE0_0 {
                                byteBlob: application.0,
                            },
                        },
                    },
                    FWPM_FILTER_CONDITION0 {
                        fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                        matchType: FWP_MATCH_EQUAL,
                        conditionValue: FWP_CONDITION_VALUE0 {
                            r#type: FWP_UINT8,
                            Anonymous: FWP_CONDITION_VALUE0_0 { uint8: protocol },
                        },
                    },
                    FWPM_FILTER_CONDITION0 {
                        fieldKey: FWPM_CONDITION_FLAGS,
                        matchType: FWP_MATCH_FLAGS_NONE_SET,
                        conditionValue: FWP_CONDITION_VALUE0 {
                            r#type: FWP_UINT32,
                            Anonymous: FWP_CONDITION_VALUE0_0 {
                                uint32: FWP_CONDITION_FLAG_IS_REAUTHORIZE,
                            },
                        },
                    },
                ];
                let filter = FWPM_FILTER0 {
                    displayData: FWPM_DISPLAY_DATA0 {
                        name: title.as_mut_ptr(),
                        description: ptr::null_mut(),
                    },
                    layerKey: layer,
                    subLayerKey: FWPM_SUBLAYER_UNIVERSAL,
                    numFilterConditions: conditions.len() as u32,
                    filterCondition: conditions.as_mut_ptr(),
                    action: FWPM_ACTION0 {
                        r#type: FWP_ACTION_BLOCK,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                checked(unsafe {
                    FwpmFilterAdd0(handle, &filter, ptr::null_mut(), ptr::null_mut())
                })?;
            }
        }
        checked(unsafe { FwpmTransactionCommit0(handle) })?;
        Ok(Self { _engine: engine })
    }
}

mod redirect;
pub use redirect::{RedirectContext, TcpRedirector};

#[cfg(test)]
#[path = "test/windows_capture_test.rs"]
mod tests;
