use anyhow::Result;
use anyhow::anyhow;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows_sys::Win32::Security::Cryptography::CRYPTPROTECT_LOCAL_MACHINE;
use windows_sys::Win32::Security::Cryptography::CRYPTPROTECT_UI_FORBIDDEN;
use windows_sys::Win32::Security::Cryptography::CryptProtectData;
use windows_sys::Win32::Security::Cryptography::CryptUnprotectData;

fn checked_blob_len(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| anyhow!("DPAPI input exceeds the 32-bit blob length limit"))
}

fn make_blob(data: &[u8]) -> Result<CRYPT_INTEGER_BLOB> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: checked_blob_len(data.len())?,
        pbData: data.as_ptr() as *mut u8,
    })
}

#[allow(clippy::unnecessary_mut_passed)]
pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
    let mut in_blob = make_blob(data)?;
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: in_blob borrows data for the call with a checked u32 length; out_blob is writable
    // output storage and optional inputs are null.
    let ok = unsafe {
        CryptProtectData(
            &mut in_blob,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Use machine scope so elevated and non-elevated processes can decrypt.
            CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(anyhow!("CryptProtectData failed: {}", unsafe {
            GetLastError()
        }));
    }
    let slice = if out_blob.cbData == 0 {
        Vec::new()
    } else {
        // SAFETY: the successful CryptProtectData call returned a live allocation containing cbData
        // initialized bytes. Empty output is handled separately so no null slice is formed.
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec()
    };
    // SAFETY: A successful CryptProtectData allocated out_blob.pbData with LocalAlloc; the
    // copied result no longer borrows it before LocalFree.
    unsafe {
        if !out_blob.pbData.is_null() {
            LocalFree(out_blob.pbData as HLOCAL);
        }
    }
    Ok(slice)
}

#[allow(clippy::unnecessary_mut_passed)]
pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>> {
    let mut in_blob = make_blob(blob)?;
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: in_blob borrows blob for the call with a checked u32 length; out_blob is writable
    // output storage and optional inputs are null.
    let ok = unsafe {
        CryptUnprotectData(
            &mut in_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            // Use machine scope so elevated and non-elevated processes can decrypt.
            CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(anyhow!("CryptUnprotectData failed: {}", unsafe {
            GetLastError()
        }));
    }
    let slice = if out_blob.cbData == 0 {
        Vec::new()
    } else {
        // SAFETY: the successful CryptUnprotectData call returned a live allocation containing cbData
        // initialized bytes. Empty output is handled separately so no null slice is formed.
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec()
    };
    // SAFETY: A successful CryptUnprotectData allocated out_blob.pbData with LocalAlloc; the
    // copied result no longer borrows it before LocalFree.
    unsafe {
        if !out_blob.pbData.is_null() {
            LocalFree(out_blob.pbData as HLOCAL);
        }
    }
    Ok(slice)
}

