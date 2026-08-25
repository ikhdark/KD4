/// Case-insensitive subsequence matching for TUI filtering and highlighting.
pub(crate) fn fuzzy_match(haystack: &str, needle: &str) -> Option<(Vec<usize>, i32)> {
    if needle.is_empty() {
        return Some((Vec::new(), i32::MAX));
    }
    let mut lowered_chars: Vec<char> = Vec::new();
    let mut lowered_to_original: Vec<usize> = Vec::new();
    for (original_index, character) in haystack.chars().enumerate() {
        for lowered_character in character.to_lowercase() {
            lowered_chars.push(lowered_character);
            lowered_to_original.push(original_index);
        }
    }
    let lowered_needle: Vec<char> = needle.to_lowercase().chars().collect();
    let mut indices: Vec<usize> = Vec::with_capacity(lowered_needle.len());
    let mut last_lower_position = None;
    let mut cursor = 0usize;
    for needle_character in lowered_needle.iter().copied() {
        let mut found_at = None;
        while cursor < lowered_chars.len() {
            if lowered_chars[cursor] == needle_character {
                found_at = Some(cursor);
                cursor += 1;
                break;
            }
            cursor += 1;
        }
        let position = found_at?;
        indices.push(lowered_to_original[position]);
        last_lower_position = Some(position);
    }

    let first_lower_position = if indices.is_empty() {
        0
    } else {
        let first_original_index = indices[0];
        lowered_to_original
            .iter()
            .position(|index| *index == first_original_index)
            .unwrap_or(0)
    };
    let last_lower_position = last_lower_position.unwrap_or(first_lower_position);
    let window = (last_lower_position as i32 - first_lower_position as i32 + 1)
        - lowered_needle.len() as i32;
    let mut score = window.max(0);
    if first_lower_position == 0 {
        score -= 100;
    }
    indices.sort_unstable();
    indices.dedup();
    Some((indices, score))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_original_unicode_indices() {
        assert_eq!(fuzzy_match("İstanbul", "is"), Some((vec![0, 1], -99)));
    }

    #[test]
    fn contiguous_prefixes_rank_first() {
        assert!(fuzzy_match("abc", "abc").unwrap().1 < fuzzy_match("a-b-c", "abc").unwrap().1);
    }
}
