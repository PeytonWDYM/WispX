//! Access control. The equivalent of a 0700 directory, and of checking who is on the
//! other end of a socket whose directory only we could reach.

use super::ffi::*;
use std::ffi::c_void;
use std::io;
use std::path::Path;
use std::ptr;

/// The SID of the user that owns `process`, in a buffer it points into.
pub struct UserSid {
    buffer: Vec<u64>,
}

impl UserSid {
    fn sid(&self) -> Psid {
        // SAFETY: the buffer was filled with a `TOKEN_USER`, and is aligned for one.
        unsafe { (*self.buffer.as_ptr().cast::<TokenUser>()).sid }
    }

    /// As text, for a security descriptor.
    fn text(&self) -> io::Result<String> {
        let mut text = ptr::null_mut();
        // SAFETY: the SID is valid while `self` lives, and the result is freed here.
        unsafe {
            if ConvertSidToStringSidW(self.sid(), &raw mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let result = from_wide_ptr(text);
            LocalFree(text.cast());
            Ok(result)
        }
    }

    pub fn same_as(&self, other: &UserSid) -> bool {
        // SAFETY: both SIDs are valid while their owners live.
        unsafe { EqualSid(self.sid(), other.sid()) != 0 }
    }
}

/// Who owns a process.
///
/// # Safety
///
/// `process` has to be a handle allowing at least limited queries on the process.
pub unsafe fn user_of(process: Handle) -> io::Result<UserSid> {
    let mut token = ptr::null_mut();
    // SAFETY: the caller vouches for the handle. The token is checked and owned from
    // here, and the sizes passed are those of the buffer.
    unsafe {
        if OpenProcessToken(process, TOKEN_QUERY, &raw mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Owned(token);
        let mut needed = 0u32;
        GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            ptr::null_mut(),
            0,
            &raw mut needed,
        );
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        let ok = GetTokenInformation(
            token.0,
            TOKEN_USER_CLASS,
            buffer.as_mut_ptr().cast(),
            needed,
            &raw mut needed,
        );
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(UserSid { buffer })
    }
}

pub fn current_user() -> io::Result<UserSid> {
    // SAFETY: this names the current process, and needs no closing.
    unsafe { user_of(GetCurrentProcess()) }
}

/// A security descriptor, for as long as something needs pointing to it.
pub struct Descriptor(*mut c_void);

impl Descriptor {
    /// Only this user. `inherit` passes that on to everything created inside.
    pub fn only_this_user(rights: &str, inherit: bool) -> io::Result<Descriptor> {
        let sid = current_user()?.text()?;
        let flags = if inherit { "OICI" } else { "" };
        let text = wide(&format!("D:P(A;{flags};{rights};;;{sid})"));
        let mut descriptor = ptr::null_mut();
        // SAFETY: the text is NUL-terminated, and `Drop` frees the result.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                SDDL_REVISION_1,
                &raw mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Descriptor(descriptor))
    }

    pub fn attributes(&self) -> SecurityAttributes {
        SecurityAttributes {
            length: size_of::<SecurityAttributes>() as u32,
            descriptor: self.0,
            inherit: 0,
        }
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by the conversion above, and only freed here.
        unsafe {
            LocalFree(self.0);
        }
    }
}

/// Creates a directory that only this user can reach. One that exists is left as it is.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    let descriptor = Descriptor::only_this_user("FA", true)?;
    let attributes = descriptor.attributes();
    let path = wide_os(dir.as_os_str());
    // SAFETY: the path is NUL-terminated and the attributes outlive the call.
    let created = unsafe { CreateDirectoryW(path.as_ptr(), &raw const attributes) };
    if created == 0 && last_error() != ERROR_ALREADY_EXISTS {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
