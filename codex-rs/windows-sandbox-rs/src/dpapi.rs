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
    let slice =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
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
    let slice =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize) }.to_vec();
    unsafe {
        if !out_blob.pbData.is_null() {
            LocalFree(out_blob.pbData as HLOCAL);
        }
    }
    Ok(slice)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    #[test]
    fn public_dpapi_roundtrip_preserves_binary_input_and_rejects_invalid_ciphertext()
    -> anyhow::Result<()> {
        let plaintext = b"offline\0credential\xff\x80";
        let ciphertext = crate::dpapi_protect(plaintext)?;
        assert_ne!(ciphertext, plaintext);
        assert_eq!(crate::dpapi_unprotect(&ciphertext)?, plaintext);
        let error = crate::dpapi_unprotect(b"not a DPAPI ciphertext")
            .expect_err("invalid ciphertext must fail");
        assert!(error.to_string().contains("CryptUnprotectData failed"));
        Ok(())
    }

    #[test]
    fn blob_length_rejects_values_above_the_windows_abi_limit() -> anyhow::Result<()> {
        assert_eq!(super::checked_blob_len(0)?, 0);
        assert_eq!(super::checked_blob_len(u32::MAX as usize)?, u32::MAX);
        if let Some(too_large) = (u32::MAX as usize).checked_add(1) {
            let error = super::checked_blob_len(too_large)
                .expect_err("oversized input must not wrap to zero");
            assert_eq!(
                error.to_string(),
                "DPAPI input exceeds the 32-bit blob length limit"
            );
        }
        Ok(())
    }
}
