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

#[cfg(test)]
mod tests {
    use super::first_der_item;
    use super::trusted_certificate_for_server_auth;

    #[test]
    fn trusted_roots_cannot_discard_nonempty_or_malformed_aux() {
        let certificate = [0x30, 0x01, 0xaa];
        assert_eq!(
            trusted_certificate_for_server_auth(&certificate),
            Ok(&certificate[..])
        );
        let mut empty_aux = certificate.to_vec();
        empty_aux.extend_from_slice(&[0x30, 0]);
        assert_eq!(
            trusted_certificate_for_server_auth(&empty_aux),
            Ok(&certificate[..])
        );
        for aux in [
            &[0x30, 3, 0x06, 1, 0x2a][..],
            &[0x30, 2, 0x06][..],
            &[0x05, 0][..],
        ] {
            let mut restricted = certificate.to_vec();
            restricted.extend_from_slice(aux);
            assert!(trusted_certificate_for_server_auth(&restricted).is_err());
        }
    }

    #[test]
    fn rejects_invalid_lengths_and_trims_trailing_x509_aux() {
        let exact_short = [0x30, 0x01, 0xaa];
        let trailing_short = [0x30, 0x01, 0xaa, 0xbb];
        let exact_long = [0x30, 0x81, 0x01, 0xaa];
        let overflow_octets = usize::BITS as usize / 8 + 1;
        let mut overflowing = vec![0x30, 0x80 | overflow_octets as u8];
        overflowing.extend(std::iter::repeat_n(0xff, overflow_octets));

        assert_eq!(first_der_item(&[]), None);
        assert_eq!(first_der_item(&[0x30]), None);
        assert_eq!(first_der_item(&exact_short), Some(exact_short.as_slice()));
        assert_eq!(
            first_der_item(&trailing_short),
            Some(exact_short.as_slice())
        );
        assert_eq!(first_der_item(&exact_long), Some(exact_long.as_slice()));
        assert_eq!(first_der_item(&[0x30, 0x80, 0x00, 0x00]), None);
        assert_eq!(first_der_item(&[0x30, 0x82, 0x01]), None);
        assert_eq!(first_der_item(&[0x30, 0x02, 0xaa]), None);
        assert_eq!(first_der_item(&[0x30, 0x81, 0x02, 0xaa]), None);
        assert_eq!(first_der_item(&overflowing), None);
    }
}
