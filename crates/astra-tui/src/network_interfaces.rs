//! Local interface names, including disconnected and virtual adapters.
use std::io;

pub fn names(exclude: Option<&str>) -> io::Result<Vec<String>> {
    let mut names = platform_names(exclude)?;
    names.retain(|name| !name.is_empty() && exclude != Some(name.as_str()));
    names.sort();
    names.dedup();
    Ok(names)
}

#[cfg(windows)]
fn platform_names(exclude: Option<&str>) -> io::Result<Vec<String>> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
    };
    let mut pointer = std::ptr::null_mut();
    let status = unsafe { GetIfTable2(&mut pointer) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    struct Table(*mut MIB_IF_TABLE2);
    impl Drop for Table {
        fn drop(&mut self) {
            unsafe { FreeMibTable(self.0.cast()) };
        }
    }
    let table = Table(pointer);
    // GetIfTable2 owns a variable-length array until FreeMibTable is called.
    let rows = unsafe {
        std::slice::from_raw_parts((*table.0).Table.as_ptr(), (*table.0).NumEntries as usize)
    };
    Ok(rows
        .iter()
        .filter(|row| {
            row.Type != 24 // Software loopback is not an egress adapter.
            && !exclude.is_some_and(|name| name == row.InterfaceIndex.to_string())
        })
        .map(|row| {
            let end = row
                .Alias
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(row.Alias.len());
            String::from_utf16_lossy(&row.Alias[..end])
        })
        .collect())
}

#[cfg(unix)]
fn platform_names(_exclude: Option<&str>) -> io::Result<Vec<String>> {
    let pointer = unsafe { libc::if_nameindex() };
    if pointer.is_null() {
        return Err(io::Error::last_os_error());
    }
    struct Names(*mut libc::if_nameindex);
    impl Drop for Names {
        fn drop(&mut self) {
            unsafe { libc::if_freenameindex(self.0) };
        }
    }
    let names = Names(pointer);
    let mut result = Vec::new();
    let mut row = names.0;
    unsafe {
        while (*row).if_index != 0 && !(*row).if_name.is_null() {
            let name = std::ffi::CStr::from_ptr((*row).if_name).to_string_lossy();
            if name != "lo" && name != "lo0" {
                result.push(name.into_owned());
            }
            row = row.add(1);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn local_adapters_are_sorted_unique_and_excludable() {
        let names = super::names(None).unwrap();
        assert!(
            !names.is_empty(),
            "expected at least one local network adapter"
        );
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(!super::names(Some(&names[0])).unwrap().contains(&names[0]));
        println!("Local adapters: {names:?}");
    }
}
