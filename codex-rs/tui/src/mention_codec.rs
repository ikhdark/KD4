use std::collections::HashMap;
use std::collections::VecDeque;

use crate::bottom_pane::MentionBinding;
use codex_core_skills::injection::LinkedToolMention;
use codex_core_skills::injection::ToolMentionKind;
use codex_core_skills::injection::is_common_env_var;
use codex_core_skills::injection::is_mention_name_char;
use codex_core_skills::injection::is_mention_name_char_char;
use codex_core_skills::injection::parse_linked_tool_mention;
use codex_core_skills::injection::tool_kind_for_path;
use codex_plugin::mention_syntax::PLUGIN_TEXT_MENTION_SIGIL;
use codex_plugin::mention_syntax::TOOL_MENTION_SIGIL;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecodedHistoryText {
    pub(crate) text: String,
    pub(crate) mentions: Vec<MentionBinding>,
}

#[allow(dead_code)]
pub(crate) fn encode_history_mentions(text: &str, mentions: &[MentionBinding]) -> String {
    if mentions.is_empty() || text.is_empty() {
        return text.to_string();
    }

    let mut mentions_by_token: HashMap<(char, &str), VecDeque<&str>> = HashMap::new();
    for mention in mentions {
        if !matches!(
            mention.sigil,
            TOOL_MENTION_SIGIL | PLUGIN_TEXT_MENTION_SIGIL
        ) {
            continue;
        }
        mentions_by_token
            .entry((mention.sigil, mention.mention.as_str()))
            .or_default()
            .push_back(mention.path.as_str());
    }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;

    while index < bytes.len() {
        if matches!(
            bytes[index],
            byte if byte == TOOL_MENTION_SIGIL as u8 || byte == PLUGIN_TEXT_MENTION_SIGIL as u8
        ) {
            let sigil = bytes[index] as char;
            if sigil == TOOL_MENTION_SIGIL || starts_plaintext_mention(text, index) {
                let name_start = index + 1;
                if let Some(first) = bytes.get(name_start)
                    && is_mention_name_char(*first)
                {
                    let mut name_end = name_start + 1;
                    while let Some(next) = bytes.get(name_end)
                        && is_mention_name_char(*next)
                    {
                        name_end += 1;
                    }

                    let name = &text[name_start..name_end];
                    if (sigil == TOOL_MENTION_SIGIL || ends_plaintext_mention(bytes, name_end))
                        && let Some(path) = mentions_by_token
                            .get_mut(&(sigil, name))
                            .and_then(VecDeque::pop_front)
                    {
                        out.push('[');
                        out.push(sigil);
                        out.push_str(name);
                        out.push_str("](");
                        out.push_str(path);
                        out.push(')');
                        index = name_end;
                        continue;
                    }
                }
            }
        }

        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }

    out
}

pub(crate) fn decode_history_mentions(text: &str) -> DecodedHistoryText {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut mentions = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some((sigil, name, path, end_index)) = parse_history_linked_mention(text, index)
        {
            out.push(sigil);
            out.push_str(name);
            mentions.push(MentionBinding {
                sigil,
                mention: name.to_string(),
                path: path.to_string(),
            });
            index = end_index;
            continue;
        }

        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }

    DecodedHistoryText {
        text: out,
        mentions,
    }
}

fn parse_history_linked_mention(text: &str, start: usize) -> Option<(char, &str, &str, usize)> {
    // TUI historically wrote `$name`, but selected unified `@` mentions should preserve `@` on
    // history round-trip for any canonical tool path.
    if let Some(LinkedToolMention {
        name,
        path,
        end: end_index,
    }) = parse_linked_tool_mention(text, start, TOOL_MENTION_SIGIL)
        && !is_common_env_var(name)
        && is_tool_path(path)
    {
        return Some((TOOL_MENTION_SIGIL, name, path, end_index));
    }

    if let Some(LinkedToolMention {
        name,
        path,
        end: end_index,
    }) = parse_linked_tool_mention(text, start, PLUGIN_TEXT_MENTION_SIGIL)
        && !is_common_env_var(name)
        && is_tool_path(path)
    {
        return Some((PLUGIN_TEXT_MENTION_SIGIL, name, path, end_index));
    }

    None
}

fn starts_plaintext_mention(text: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }

    text.get(..index)
        .and_then(|prefix| prefix.chars().next_back())
        .is_some_and(|ch| ch.is_whitespace() || !is_mention_name_char_char(ch))
}

fn ends_plaintext_mention(text_bytes: &[u8], index: usize) -> bool {
    text_bytes.get(index).is_none_or(|byte| {
        byte.is_ascii_whitespace()
            || *byte == b'.'
                && text_bytes.get(index + 1).is_none_or(|next| {
                    next.is_ascii_whitespace()
                        || !next.is_ascii_alphanumeric() && *next != b'_' && *next != b'-'
                })
            || !matches!(*byte, b'.' | b'/' | b'\\')
                && !byte.is_ascii_alphanumeric()
                && *byte != b'_'
                && *byte != b'-'
    })
}

fn is_tool_path(path: &str) -> bool {
    tool_kind_for_path(path) != ToolMentionKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn decoded_mentions_use_the_composer_binding_type() {
        fn accept_composer_binding(binding: crate::bottom_pane::MentionBinding) -> MentionBinding {
            binding
        }

        let mut decoded = decode_history_mentions("Use [$figma](app://figma-1).");
        let binding = accept_composer_binding(decoded.mentions.remove(0));
        assert_eq!(binding.mention, "figma");
    }

    #[test]
    fn decode_history_mentions_restores_visible_tokens() {
        let decoded = decode_history_mentions(
            "Use [$figma](app://figma-1), [$sample](plugin://sample@test), and [$figma](/tmp/figma/SKILL.md).",
        );
        assert_eq!(decoded.text, "Use $figma, $sample, and $figma.");
        assert_eq!(
            decoded.mentions,
            vec![
                MentionBinding {
                    sigil: '$',
                    mention: "figma".to_string(),
                    path: "app://figma-1".to_string(),
                },
                MentionBinding {
                    sigil: '$',
                    mention: "sample".to_string(),
                    path: "plugin://sample@test".to_string(),
                },
                MentionBinding {
                    sigil: '$',
                    mention: "figma".to_string(),
                    path: "/tmp/figma/SKILL.md".to_string(),
                },
            ]
        );
    }

    #[test]
    fn decode_history_mentions_restores_plugin_links_with_at_sigil() {
        let decoded = decode_history_mentions(
            "Use [@sample](plugin://sample@test) and [$figma](app://figma-1).",
        );
        assert_eq!(decoded.text, "Use @sample and $figma.");
        assert_eq!(
            decoded.mentions,
            vec![
                MentionBinding {
                    sigil: '@',
                    mention: "sample".to_string(),
                    path: "plugin://sample@test".to_string(),
                },
                MentionBinding {
                    sigil: '$',
                    mention: "figma".to_string(),
                    path: "app://figma-1".to_string(),
                },
            ]
        );
    }

    #[test]
    fn decode_history_mentions_restores_at_sigil_for_tool_paths() {
        let decoded = decode_history_mentions("Use [@figma](app://figma-1).");

        assert_eq!(decoded.text, "Use @figma.");
        assert_eq!(
            decoded.mentions,
            vec![MentionBinding {
                sigil: '@',
                mention: "figma".to_string(),
                path: "app://figma-1".to_string(),
            }]
        );
    }

    #[test]
    fn encode_history_mentions_links_bound_mentions_in_order() {
        let text = "$figma then $sample then $figma then $other";
        let encoded = encode_history_mentions(
            text,
            &[
                MentionBinding {
                    sigil: '$',
                    mention: "figma".to_string(),
                    path: "app://figma-app".to_string(),
                },
                MentionBinding {
                    sigil: '$',
                    mention: "sample".to_string(),
                    path: "plugin://sample@test".to_string(),
                },
                MentionBinding {
                    sigil: '$',
                    mention: "figma".to_string(),
                    path: "/tmp/figma/SKILL.md".to_string(),
                },
            ],
        );
        assert_eq!(
            encoded,
            "[$figma](app://figma-app) then [$sample](plugin://sample@test) then [$figma](/tmp/figma/SKILL.md) then $other"
        );
    }

    #[test]
    fn encode_history_mentions_preserves_namespaced_skill_binding() {
        let encoded = encode_history_mentions(
            "$google-calendar:availability",
            &[MentionBinding {
                sigil: '$',
                mention: "google-calendar:availability".to_string(),
                path: "/tmp/google-calendar/availability/SKILL.md".to_string(),
            }],
        );

        assert_eq!(
            encoded,
            "[$google-calendar:availability](/tmp/google-calendar/availability/SKILL.md)"
        );
    }

    #[test]
    fn encode_history_mentions_links_dollar_mentions_after_punctuation() {
        let encoded = encode_history_mentions(
            "($figma)",
            &[MentionBinding {
                sigil: '$',
                mention: "figma".to_string(),
                path: "app://figma".to_string(),
            }],
        );
        assert_eq!(encoded, "([$figma](app://figma))");
    }

    #[test]
    fn encode_history_mentions_links_dollar_mentions_with_path_like_suffixes() {
        let mention = MentionBinding {
            sigil: '$',
            mention: "figma".to_string(),
            path: "app://figma".to_string(),
        };

        assert_eq!(
            encode_history_mentions("$figma/docs", std::slice::from_ref(&mention)),
            "[$figma](app://figma)/docs"
        );
        assert_eq!(
            encode_history_mentions("$figma.suffix", std::slice::from_ref(&mention)),
            "[$figma](app://figma).suffix"
        );
        assert_eq!(
            encode_history_mentions("$figma\\docs", &[mention]),
            "[$figma](app://figma)\\docs"
        );
    }

    #[test]
    fn encode_history_mentions_preserves_at_sigils() {
        let text = "@figma then @sample then $other";
        let encoded = encode_history_mentions(
            text,
            &[
                MentionBinding {
                    sigil: '@',
                    mention: "figma".to_string(),
                    path: "/tmp/figma/SKILL.md".to_string(),
                },
                MentionBinding {
                    sigil: '@',
                    mention: "sample".to_string(),
                    path: "plugin://sample@test".to_string(),
                },
            ],
        );
        assert_eq!(
            encoded,
            "[@figma](/tmp/figma/SKILL.md) then [@sample](plugin://sample@test) then $other"
        );
    }

    #[test]
    fn encode_history_mentions_links_both_sigils_for_same_name() {
        let text = "@figma then $figma";
        let encoded = encode_history_mentions(
            text,
            &[
                MentionBinding {
                    sigil: '@',
                    mention: "figma".to_string(),
                    path: "plugin://figma@test".to_string(),
                },
                MentionBinding {
                    sigil: '$',
                    mention: "figma".to_string(),
                    path: "app://figma".to_string(),
                },
            ],
        );
        assert_eq!(
            encoded,
            "[@figma](plugin://figma@test) then [$figma](app://figma)"
        );
    }

    #[test]
    fn encode_history_mentions_does_not_let_at_token_steal_later_tool_binding() {
        let text = "@figma then $figma";
        let encoded = encode_history_mentions(
            text,
            &[MentionBinding {
                sigil: '$',
                mention: "figma".to_string(),
                path: "app://figma-app".to_string(),
            }],
        );
        assert_eq!(encoded, "@figma then [$figma](app://figma-app)");
    }

    #[test]
    fn encode_history_mentions_links_at_mentions_after_unicode_whitespace() {
        // Fix coverage: full-width space should remain a valid plaintext boundary for `@` links.
        let text = "foo　@sample";
        let encoded = encode_history_mentions(
            text,
            &[MentionBinding {
                sigil: '@',
                mention: "sample".to_string(),
                path: "plugin://sample@test".to_string(),
            }],
        );
        assert_eq!(encoded, "foo　[@sample](plugin://sample@test)");
    }

    #[test]
    fn encode_history_mentions_links_sentence_ending_at_mentions() {
        let text = "Please ask @figma.";
        let encoded = encode_history_mentions(
            text,
            &[MentionBinding {
                sigil: '@',
                mention: "figma".to_string(),
                path: "/tmp/figma/SKILL.md".to_string(),
            }],
        );
        assert_eq!(encoded, "Please ask [@figma](/tmp/figma/SKILL.md).");
    }

    #[test]
    fn encode_history_mentions_links_parenthesized_at_mentions() {
        let text = "Please ask (@figma)";
        let encoded = encode_history_mentions(
            text,
            &[MentionBinding {
                sigil: '@',
                mention: "figma".to_string(),
                path: "plugin://figma@test".to_string(),
            }],
        );
        assert_eq!(encoded, "Please ask ([@figma](plugin://figma@test))");
    }

    #[test]
    fn encode_history_mentions_skips_embedded_at_substrings() {
        let text = "foo@sample.com npx @sample/pkg then @sample";
        let encoded = encode_history_mentions(
            text,
            &[MentionBinding {
                sigil: '@',
                mention: "sample".to_string(),
                path: "plugin://sample@test".to_string(),
            }],
        );
        assert_eq!(
            encoded,
            "foo@sample.com npx @sample/pkg then [@sample](plugin://sample@test)"
        );
    }
}
