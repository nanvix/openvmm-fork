// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::ObjectAttributes;
use super::UnicodeString;
use super::chk_status;
use super::dos_to_nt_path;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::mem::zeroed;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsHandle;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr;
use std::ptr::null_mut;
use widestring::U16CString;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem as ntioapi;
use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::OBJ_CASE_INSENSITIVE;
use windows_sys::Win32::Foundation::STATUS_NO_MORE_FILES;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_ID_INFO;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_STANDARD_INFO;
use windows_sys::Win32::Storage::FileSystem::FileIdInfo;
use windows_sys::Win32::Storage::FileSystem::FileStandardInfo;
use windows_sys::Win32::Storage::FileSystem::FindClose;
use windows_sys::Win32::Storage::FileSystem::FindFirstFileW;
use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandleEx;
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::Ioctl::DUPLICATE_EXTENTS_DATA;
use windows_sys::Win32::System::Ioctl::FILE_ALLOCATED_RANGE_BUFFER;
use windows_sys::Win32::System::Ioctl::FILE_ZERO_DATA_INFORMATION;
use windows_sys::Win32::System::Ioctl::FSCTL_DUPLICATE_EXTENTS_TO_FILE;
use windows_sys::Win32::System::Ioctl::FSCTL_QUERY_ALLOCATED_RANGES;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_SPARSE;
use windows_sys::Win32::System::Ioctl::FSCTL_SET_ZERO_DATA;

/// Stable identity and EOF for one opened file generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub volume_serial_number: u64,
    pub file_id: [u8; 16],
    pub end_of_file: u64,
}

/// Opens an existing regular file relative to an opened directory.
///
/// The resulting handle allows read sharing only, so writers and
/// delete/rename attempts are rejected while the handle remains open.
pub fn open_relative_read_only(
    directory: &fs::File,
    name: &std::ffi::OsStr,
) -> io::Result<fs::File> {
    open_relative_file(
        directory,
        name,
        FILE_GENERIC_READ,
        ntioapi::FILE_OPEN,
        "open",
    )
}

/// Creates a new file relative to an opened directory.
pub fn create_relative_new(directory: &fs::File, name: &std::ffi::OsStr) -> io::Result<fs::File> {
    open_relative_file(
        directory,
        name,
        FILE_GENERIC_WRITE,
        ntioapi::FILE_CREATE,
        "create",
    )
}

fn open_relative_file(
    directory: &fs::File,
    name: &std::ffi::OsStr,
    desired_access: u32,
    create_disposition: u32,
    operation: &str,
) -> io::Result<fs::File> {
    let name_string = name.to_string_lossy();
    let name = UnicodeString::try_from(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let mut attributes = ObjectAttributes::new();
    attributes
        .name(&name)
        .root(directory.as_handle())
        .attributes(OBJ_CASE_INSENSITIVE);
    let mut handle = null_mut();
    let mut io_status = IO_STATUS_BLOCK::default();
    let status = unsafe {
        ntioapi::NtCreateFile(
            &mut handle,
            desired_access | SYNCHRONIZE,
            attributes.as_ptr(),
            &mut io_status,
            ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ,
            create_disposition,
            ntioapi::FILE_NON_DIRECTORY_FILE
                | ntioapi::FILE_OPEN_REPARSE_POINT
                | ntioapi::FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    chk_status(status).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to {operation} {name_string}: {error}"),
        )
    })?;
    Ok(unsafe { fs::File::from_raw_handle(handle) })
}

/// Enumerates names relative to an opened directory handle.
pub fn directory_entry_names(directory: &fs::File) -> io::Result<Vec<std::ffi::OsString>> {
    const BUFFER_SIZE: usize = 4096;
    const BUFFER_WORDS: usize = BUFFER_SIZE / size_of::<u64>();

    let mut names = Vec::new();
    let mut restart_scan = true;
    loop {
        let mut buffer = [0_u64; BUFFER_WORDS];
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            ntioapi::NtQueryDirectoryFile(
                directory.as_raw_handle(),
                null_mut(),
                None,
                ptr::null(),
                &mut io_status,
                buffer.as_mut_ptr().cast(),
                BUFFER_SIZE as u32,
                ntioapi::FileNamesInformation,
                true,
                ptr::null(),
                restart_scan,
            )
        };
        restart_scan = false;
        if status == STATUS_NO_MORE_FILES {
            break;
        }
        chk_status(status)?;

        let returned = io_status.Information;
        let header_size = std::mem::offset_of!(ntioapi::FILE_NAMES_INFORMATION, FileName);
        if returned < header_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory query returned a truncated entry",
            ));
        }
        let entry = unsafe { &*buffer.as_ptr().cast::<ntioapi::FILE_NAMES_INFORMATION>() };
        let name_length = usize::try_from(entry.FileNameLength).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry name length exceeds usize",
            )
        })?;
        if !name_length.is_multiple_of(2) || header_size + name_length > returned {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory query returned an invalid entry name",
            ));
        }
        let name = unsafe {
            std::slice::from_raw_parts(entry.FileName.as_ptr(), name_length / size_of::<u16>())
        };
        let name = std::ffi::OsString::from_wide(name);
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    Ok(names)
}

/// Queries `FILE_ID_INFO` and the current EOF for an opened file.
pub fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
    let mut identity = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut identity).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut standard = FILE_STANDARD_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&raw mut standard).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    if standard.Directory || standard.DeletePending {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "handle is not an available regular file",
        ));
    }
    let end_of_file = u64::try_from(standard.EndOfFile)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file has a negative EOF"))?;
    Ok(FileIdentity {
        volume_serial_number: identity.VolumeSerialNumber,
        file_id: identity.FileId.Identifier,
        end_of_file,
    })
}

/// Marks a file as sparse.
pub fn set_sparse(file: &fs::File) -> io::Result<()> {
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            ptr::null(),
            0,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Returns the physical allocation charged to a file.
pub fn allocation_size(file: &fs::File) -> io::Result<u64> {
    let mut info = FILE_STANDARD_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&raw mut info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        u64::try_from(info.AllocationSize).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "negative file allocation size")
        })
    }
}

/// Block-clones all source extents into an independently writable destination.
pub fn duplicate_extents(source: &fs::File, destination: &fs::File, length: u64) -> io::Result<()> {
    let byte_count = i64::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file is too large"))?;
    let input = DUPLICATE_EXTENTS_DATA {
        FileHandle: source.as_raw_handle(),
        SourceFileOffset: 0,
        TargetFileOffset: 0,
        ByteCount: byte_count,
    };
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            destination.as_raw_handle(),
            FSCTL_DUPLICATE_EXTENTS_TO_FILE,
            (&raw const input).cast(),
            size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Returns allocated file ranges clipped to `[0, length)`.
pub fn allocated_ranges(file: &fs::File, length: u64) -> io::Result<Vec<(u64, u64)>> {
    const RANGE_CAPACITY: usize = 64;

    let mut ranges = Vec::new();
    let mut cursor = 0_u64;
    while cursor < length {
        let input = FILE_ALLOCATED_RANGE_BUFFER {
            FileOffset: i64::try_from(cursor).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds i64")
            })?,
            Length: i64::try_from(length - cursor).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "file length exceeds i64")
            })?,
        };
        let mut output = [FILE_ALLOCATED_RANGE_BUFFER::default(); RANGE_CAPACITY];
        let mut returned = 0_u32;
        let result = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_QUERY_ALLOCATED_RANGES,
                (&raw const input).cast(),
                size_of::<FILE_ALLOCATED_RANGE_BUFFER>() as u32,
                output.as_mut_ptr().cast(),
                size_of_val(&output) as u32,
                &mut returned,
                null_mut(),
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_MORE_DATA as i32) {
                return Err(error);
            }
        }
        if !(returned as usize).is_multiple_of(size_of::<FILE_ALLOCATED_RANGE_BUFFER>()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query returned a partial record",
            ));
        }
        let count = returned as usize / size_of::<FILE_ALLOCATED_RANGE_BUFFER>();
        if count > output.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query overflowed its buffer",
            ));
        }
        for range in &output[..count] {
            let offset = u64::try_from(range.FileOffset).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "negative allocated range offset",
                )
            })?;
            let range_length = u64::try_from(range.Length).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "negative allocated range length",
                )
            })?;
            let end = offset.checked_add(range_length).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "allocated range overflowed u64")
            })?;
            if offset < cursor || range_length == 0 || end > length {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "allocated-range query returned an invalid range",
                ));
            }
            ranges.push((offset, range_length));
            cursor = end;
        }
        if result != 0 {
            break;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "allocated-range query made no progress",
            ));
        }
    }
    Ok(ranges)
}

/// Deallocates a range in a sparse file and makes reads return zeroes.
pub fn zero_range(file: &fs::File, start: u64, end: u64) -> io::Result<()> {
    if start >= end {
        return Ok(());
    }
    let input = FILE_ZERO_DATA_INFORMATION {
        FileOffset: i64::try_from(start).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "zero range offset exceeds i64")
        })?,
        BeyondFinalZero: i64::try_from(end).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "zero range end exceeds i64")
        })?,
    };
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_ZERO_DATA,
            (&raw const input).cast(),
            size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            null_mut(),
            0,
            &mut returned,
            null_mut(),
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn query_stat_lx_by_name(path: &Path) -> io::Result<ntioapi::FILE_STAT_LX_INFORMATION> {
    let mut pathu = dos_to_nt_path(path)?;

    let oa = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: null_mut(),
        ObjectName: pathu.as_mut_ptr(),
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null_mut(),
        SecurityQualityOfService: null_mut(),
    };

    unsafe {
        let mut iosb = zeroed();
        let mut info: ntioapi::FILE_STAT_LX_INFORMATION = zeroed();
        let info_ptr = ptr::from_mut(&mut info).cast::<c_void>();
        chk_status(ntioapi::NtQueryInformationByName(
            &oa,
            &mut iosb,
            info_ptr,
            size_of_val(&info) as u32,
            ntioapi::FileStatLxInformation,
        ))?;
        Ok(info)
    }
}

pub fn query_stat_lx(file: &fs::File) -> io::Result<ntioapi::FILE_STAT_LX_INFORMATION> {
    let handle = file.as_raw_handle();
    unsafe {
        let mut iosb = zeroed();
        let mut info: ntioapi::FILE_STAT_LX_INFORMATION = zeroed();
        let info_ptr = ptr::from_mut(&mut info).cast::<c_void>();
        chk_status(ntioapi::NtQueryInformationFile(
            handle.cast::<c_void>(),
            &mut iosb,
            info_ptr,
            size_of_val(&info) as u32,
            ntioapi::FileStatLxInformation,
        ))?;
        Ok(info)
    }
}

/// Wrapper for Win32 FindFirstFileW which only returns the data.
fn find_first_file_data(path: &Path) -> io::Result<WIN32_FIND_DATAW> {
    let path = U16CString::from_os_str(path.as_os_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul character in string"))?;

    unsafe {
        let mut data = zeroed();
        let handle = FindFirstFileW(path.as_ptr(), &mut data);

        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::from_raw_os_error(GetLastError() as i32))
        } else {
            // Close the handle opened by FindFirstfileW.
            FindClose(handle);
            Ok(data)
        }
    }
}

/// Checks if the given path is a AF_UNIX socket.
pub fn is_unix_socket(path: &Path) -> io::Result<bool> {
    const IO_REPARSE_TAG_AF_UNIX: u32 = 0x80000023;

    let data = find_first_file_data(path)?;
    Ok(data.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        && data.dwReserved0 == IO_REPARSE_TAG_AF_UNIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_stat_lx() {
        let result = query_stat_lx_by_name(r"C:\\".as_ref()).unwrap();
        assert_ne!(0, result.FileId);
    }
}
