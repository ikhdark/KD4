//! Minimal DER boundary parsing shared by certificate consumers.

/// Returns the leading encoded item with a one-octet tag and definite length.
/// Checks length boundaries only, excluding trailing data; does not validate
/// the tag, contents, or canonical DER encoding.
pub fn first_der_item(der: &[u8]) -> Option<&[u8]> {
    der_item_length(der).map(|length| &der[..length])
}

/// Extracts an OpenSSL trusted certificate without broadening AUX trust policy.
/// Rustls cannot enforce X509_AUX purposes. Only absent or empty AUX is supported;
/// nonempty (including unknown) metadata is rejected before trust insertion/export.
pub fn trusted_certificate_for_server_auth(der: &[u8]) -> Result<&[u8], &'static str> {
    let certificate = first_der_item(der)
        .filter(|item| item.first() == Some(&0x30))
        .ok_or("invalid trusted certificate DER length or tag")?;
    match &der[certificate.len()..] {
        [] | [0x30, 0x00] => Ok(certificate),
        _ => Err(
            "unsupported OpenSSL trusted certificate AUX policy; use an explicitly unrestricted CERTIFICATE root or remove this root from the bundle",
        ),
    }
}

fn der_item_length(der: &[u8]) -> Option<usize> {
    let &length_octet = der.get(1)?;
    if length_octet & 0x80 == 0 {
        return Some(2 + usize::from(length_octet)).filter(|length| *length <= der.len());
    }

    let length_octets = usize::from(length_octet & 0x7f);
    if length_octets == 0 {
        return None;
    }

    let length_start = 2usize;
    let length_end = length_start.checked_add(length_octets)?;
    let length_bytes = der.get(length_start..length_end)?;
    let mut content_length = 0usize;
    for &byte in length_bytes {
        content_length = content_length
            .checked_mul(256)?
            .checked_add(usize::from(byte))?;
    }

    length_end
        .checked_add(content_length)
        .filter(|length| *length <= der.len())
}

