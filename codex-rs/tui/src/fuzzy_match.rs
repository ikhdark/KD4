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
    let score = subsequence_score(
        first_lower_position,
        last_lower_position,
        lowered_needle.len(),
    );
    indices.sort_unstable();
    indices.dedup();
    Some((indices, score))
}

fn subsequence_score(first: usize, last: usize, matched: usize) -> i32 {
    let gap = last
        .saturating_sub(first)
        .saturating_sub(matched.saturating_sub(1));
    let prefix_bonus = if first == 0 { 100 } else { 0 };
    if gap >= prefix_bonus {
        i32::try_from(gap - prefix_bonus).unwrap_or(i32::MAX)
    } else {
        // The negative prefix bonus is bounded by100, independent of input size.
        -i32::try_from(prefix_bonus - gap).unwrap_or(100)
    }
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
        assert_eq!(fuzzy_match("abc", "abc"), Some((vec![0, 1, 2], -100)));
        assert_eq!(fuzzy_match("a-b-c", "abc"), Some((vec![0, 2, 4], -98)));
    }

    #[test]
    fn subsequence_scoring_preserves_large_positions_and_prefix_bias() {
        assert_eq!(subsequence_score(0, 2, 3), -100);
        assert_eq!(subsequence_score(2, 6, 3), 2);
        let max_score = usize::try_from(i32::MAX).expect("i32 max fits usize");
        assert_eq!(subsequence_score(1, max_score + 3, 2), i32::MAX);
        assert_eq!(subsequence_score(0, max_score + 50, 1), i32::MAX - 50);
        assert_eq!(subsequence_score(usize::MAX - 4, usize::MAX - 1, 3), 1);
        assert_eq!(subsequence_score(0, usize::MAX - 1, 1), i32::MAX);
    }
}
