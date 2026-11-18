use super::*;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6},
    num::{NonZeroU16, NonZeroU32},
    os::windows::io::AsRawSocket,
};
use windows_sys::{core::GUID, Win32::Networking::WinSock::*};

const CALLOUTS: [(GUID, GUID); 2] = [
    (
        FWPM_LAYER_ALE_CONNECT_REDIRECT_V4,
        GUID::from_u128(0x2ee52311_055a_4c93_bd62_7b064717523e),
    ),
    (
        FWPM_LAYER_ALE_CONNECT_REDIRECT_V6,
        GUID::from_u128(0x68cc4fb9_df6b_4bfb_914f_09c2fe7876b9),
    ),
];

fn transaction<T>(engine: &Engine, apply: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    checked(unsafe { FwpmTransactionBegin0(engine.0, 0) })?;
    let result = apply().and_then(|value| {
        checked(unsafe { FwpmTransactionCommit0(engine.0) })?;
        Ok(value)
    });
    if result.is_err() {
        unsafe {
            FwpmTransactionAbort0(engine.0);
        }
    }
    result
}

/// A single Agent-owned session for all TCP redirect bindings. Dropping it
/// removes its filters. A core may exit while the Agent keeps this session.
pub struct TcpRedirector {
    engine: Engine,
    filters: HashMap<Vec<u8>, [u64; 2]>,
}

impl TcpRedirector {
    pub fn open() -> io::Result<Self> {
        let engine = Engine::open()?;
        let mut title: Vec<u16> = "Astra TCP redirect".encode_utf16().chain(Some(0)).collect();
        transaction(&engine, || {
            for (layer, key) in CALLOUTS {
                let object = FWPM_CALLOUT0 {
                    calloutKey: key,
                    applicableLayer: layer,
                    displayData: FWPM_DISPLAY_DATA0 {
                        name: title.as_mut_ptr(),
                        description: ptr::null_mut(),
                    },
                    ..Default::default()
                };
                checked(unsafe {
                    FwpmCalloutAdd0(engine.0, &object, ptr::null_mut(), ptr::null_mut())
                })?;
                let mut observed = ptr::null_mut();
                checked(unsafe { FwpmCalloutGetByKey0(engine.0, &key, &mut observed) })?;
                let registered = unsafe { (*observed).flags & FWPM_CALLOUT_FLAG_REGISTERED != 0 };
                unsafe { FwpmFreeMemory0((&mut observed as *mut *mut FWPM_CALLOUT0).cast()) };
                if !registered {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "Astra TCP callout driver is not running",
                    ));
                }
            }
            Ok(())
        })?;
        Ok(Self {
            engine,
            filters: HashMap::new(),
        })
    }

    /// The broker owns listeners on 127.0.0.1 and ::1 on the supplied port.
    /// Both filters change in one WFP transaction. The driver preserves the
    /// original destination and verifies the receiving PID with WFP.
    pub fn set(&mut self, path: &Path, broker: NonZeroU32, port: NonZeroU16) -> io::Result<()> {
        let app = AppId::from_path(path)?;
        let key =
            unsafe { std::slice::from_raw_parts((*app.0).data, (*app.0).size as usize) }.to_vec();
        let old = self.filters.get(&key);
        let mut title: Vec<u16> = "Astra process TCP route"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let ids = transaction(&self.engine, || {
            if let Some(ids) = old {
                for id in ids {
                    checked(unsafe { FwpmFilterDeleteById0(self.engine.0, *id) })?;
                }
            }
            let mut ids = [0; 2];
            for (index, (layer, callout)) in CALLOUTS.into_iter().enumerate() {
                let mut conditions = [
                    FWPM_FILTER_CONDITION0 {
                        fieldKey: FWPM_CONDITION_ALE_APP_ID,
                        matchType: FWP_MATCH_EQUAL,
                        conditionValue: FWP_CONDITION_VALUE0 {
                            r#type: FWP_BYTE_BLOB_TYPE,
                            Anonymous: FWP_CONDITION_VALUE0_0 { byteBlob: app.0 },
                        },
                    },
                    FWPM_FILTER_CONDITION0 {
                        fieldKey: FWPM_CONDITION_IP_PROTOCOL,
                        matchType: FWP_MATCH_EQUAL,
                        conditionValue: FWP_CONDITION_VALUE0 {
                            r#type: FWP_UINT8,
                            Anonymous: FWP_CONDITION_VALUE0_0 { uint8: 6 },
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
                        r#type: FWP_ACTION_CALLOUT_TERMINATING,
                        Anonymous: FWPM_ACTION0_0 {
                            calloutKey: callout,
                        },
                    },
                    Anonymous: FWPM_FILTER0_0 {
                        rawContext: (u64::from(broker.get()) << 32) | u64::from(port.get()),
                    },
                    ..Default::default()
                };
                checked(unsafe {
                    FwpmFilterAdd0(self.engine.0, &filter, ptr::null_mut(), &mut ids[index])
                })?;
            }
            Ok(ids)
        })?;
        self.filters.insert(key, ids);
        Ok(())
    }

    pub fn remove(&mut self, path: &Path) -> io::Result<()> {
        let app = AppId::from_path(path)?;
        let key = unsafe { std::slice::from_raw_parts((*app.0).data, (*app.0).size as usize) };
        let ids = self.filters.get(key).ok_or(io::ErrorKind::NotFound)?;
        transaction(&self.engine, || {
            for id in ids {
                checked(unsafe { FwpmFilterDeleteById0(self.engine.0, *id) })?;
            }
            Ok(())
        })?;
        self.filters.remove(key);
        Ok(())
    }
}

pub struct RedirectContext {
    pub source_pid: u32,
    pub destination: SocketAddr,
    records: Vec<u8>,
}

fn ioctl(
    socket: &impl AsRawSocket,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let mut count = 0;
    let result = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as SOCKET,
            code,
            if input.is_empty() {
                ptr::null()
            } else {
                input.as_ptr().cast()
            },
            input.len() as u32,
            if output.is_empty() {
                ptr::null_mut()
            } else {
                output.as_mut_ptr().cast()
            },
            output.len() as u32,
            &mut count,
            ptr::null_mut(),
            None,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(count as usize)
}

impl RedirectContext {
    pub fn read(accepted: &impl AsRawSocket) -> io::Result<Self> {
        let mut data = [0u8; 32];
        let len = ioctl(
            accepted,
            SIO_QUERY_WFP_CONNECTION_REDIRECT_CONTEXT,
            &[],
            &mut data,
        )?;
        if len != data.len() || u32::from_le_bytes(data[..4].try_into().unwrap()) != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Astra WFP redirect context",
            ));
        }
        let source_pid = u32::from_le_bytes(data[4..8].try_into().unwrap());
        let port = u16::from_be_bytes(data[10..12].try_into().unwrap());
        let destination = match u16::from_le_bytes(data[8..10].try_into().unwrap()) {
            AF_INET => SocketAddr::from((
                Ipv4Addr::from(<[u8; 4]>::try_from(&data[16..20]).unwrap()),
                port,
            )),
            AF_INET6 => SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::from(<[u8; 16]>::try_from(&data[16..]).unwrap()),
                port,
                0,
                u32::from_le_bytes(data[12..16].try_into().unwrap()),
            )),
            _ => return Err(io::ErrorKind::InvalidData.into()),
        };
        let mut records = vec![0; 65536];
        let size = ioctl(
            accepted,
            SIO_QUERY_WFP_CONNECTION_REDIRECT_RECORDS,
            &[],
            &mut records,
        )?;
        if size == 0 || size > records.len() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        records.truncate(size);
        Ok(Self {
            source_pid,
            destination,
            records,
        })
    }

    /// Propagate the WFP redirect history before connecting the proxy socket.
    pub fn apply_to_outgoing(&self, socket: &impl AsRawSocket) -> io::Result<()> {
        ioctl(
            socket,
            SIO_SET_WFP_CONNECTION_REDIRECT_RECORDS,
            &self.records,
            &mut [],
        )
        .map(drop)
    }
}
