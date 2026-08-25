//! Minimal DER boundary parsing shared by certificate consumers.

/// Returns the first complete top-level DER item, excluding trailing data.
pub fn first_der_item(der: &[u8]) -> Option<&[u8]> {
    der_item_length(der).map(|length| &der[..length])
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
