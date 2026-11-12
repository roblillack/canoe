//! Splitting text into clusters that have to be drawn with a single font,
//! and deciding which of them ask for emoji presentation.
//!
//! This is a simplified take on extended grapheme clusters (UAX #29) that
//! covers what matters for font selection: combining marks, variation
//! selectors, emoji modifiers, ZWJ sequences, tag sequences and flags stay
//! with the character they modify, so the font picked for that character
//! can turn the whole sequence into a single glyph.

use std::ops::Range;

use unicode_properties::emoji::{EmojiStatus, UnicodeEmoji};
use unicode_properties::general_category::{GeneralCategoryGroup, UnicodeGeneralCategory};

const ZWJ: char = '\u{200D}';
const ZWNJ: char = '\u{200C}';
const TEXT_PRESENTATION: char = '\u{FE0E}';
const EMOJI_PRESENTATION: char = '\u{FE0F}';
const KEYCAP: char = '\u{20E3}';

fn is_regional_indicator(ch: char) -> bool {
    matches!(ch, '\u{1F1E6}'..='\u{1F1FF}')
}

fn is_emoji_modifier(ch: char) -> bool {
    matches!(ch, '\u{1F3FB}'..='\u{1F3FF}')
}

fn is_variation_selector(ch: char) -> bool {
    matches!(ch, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

fn is_tag(ch: char) -> bool {
    matches!(ch, '\u{E0020}'..='\u{E007F}')
}

/// Whether `ch` attaches to the character before it.
fn extends(ch: char) -> bool {
    ch == ZWJ
        || ch == ZWNJ
        || is_variation_selector(ch)
        || is_emoji_modifier(ch)
        || is_tag(ch)
        || ch.general_category_group() == GeneralCategoryGroup::Mark
}

/// Byte ranges of the clusters in `text`, in order.
pub fn clusters(text: &str) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut prev: Option<char> = None;
    // Whether the current cluster is a lone regional indicator still
    // waiting for the second half of its flag.
    let mut open_flag = false;

    for (i, ch) in text.char_indices() {
        let joins = match prev {
            None => false,
            Some(prev) => {
                if prev == ZWJ || extends(ch) {
                    true
                } else if open_flag && is_regional_indicator(ch) {
                    open_flag = false;
                    true
                } else {
                    false
                }
            }
        };

        match out.last_mut() {
            Some(last) if joins => last.end = i + ch.len_utf8(),
            _ => {
                open_flag = is_regional_indicator(ch);
                out.push(i..i + ch.len_utf8());
            }
        }
        prev = Some(ch);
    }
    out
}

fn has_emoji_presentation(ch: char) -> bool {
    matches!(
        ch.emoji_status(),
        EmojiStatus::EmojiPresentation
            | EmojiStatus::EmojiPresentationAndModifierBase
            | EmojiStatus::EmojiPresentationAndEmojiComponent
            | EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent
    )
}

/// Whether `cluster` should be drawn with a color emoji font: either its
/// first character defaults to emoji presentation (😀), or the sequence
/// asks for it (❤️, 1️⃣, 👍🏽, 🇩🇪, ZWJ sequences). U+FE0E forces text
/// presentation.
pub fn wants_emoji(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(base) = chars.next() else {
        return false;
    };
    let mut joined = false;
    for ch in chars {
        match ch {
            TEXT_PRESENTATION => return false,
            EMOJI_PRESENTATION | KEYCAP => return true,
            ZWJ => joined = true,
            _ if is_emoji_modifier(ch) => return true,
            _ => {}
        }
    }
    has_emoji_presentation(base) || is_regional_indicator(base) || (joined && base.is_emoji_char())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(text: &str) -> Vec<&str> {
        clusters(text).into_iter().map(|r| &text[r]).collect()
    }

    #[test]
    fn plain_text_is_one_cluster_per_char() {
        assert_eq!(split("abc"), ["a", "b", "c"]);
        assert_eq!(split(""), Vec::<&str>::new());
    }

    #[test]
    fn combining_marks_stay_with_their_base() {
        assert_eq!(split("e\u{301}x"), ["e\u{301}", "x"]);
    }

    #[test]
    fn emoji_sequences_form_single_clusters() {
        // ZWJ family, skin tone modifier, keycap, emoji presentation
        assert_eq!(
            split("👨\u{200D}👩\u{200D}👧!"),
            ["👨\u{200D}👩\u{200D}👧", "!"]
        );
        assert_eq!(split("👍🏽a"), ["👍🏽", "a"]);
        assert_eq!(split("1\u{FE0F}\u{20E3}2"), ["1\u{FE0F}\u{20E3}", "2"]);
        assert_eq!(split("❤\u{FE0F}"), ["❤\u{FE0F}"]);
    }

    #[test]
    fn regional_indicators_pair_up_into_flags() {
        assert_eq!(split("🇩🇪🇯🇵"), ["🇩🇪", "🇯🇵"]);
        assert_eq!(split("🇩🇪🇯"), ["🇩🇪", "🇯"]);
    }

    #[test]
    fn tag_sequences_form_single_clusters() {
        // Flag of England: black flag + tags "gbeng" + cancel tag.
        let england = "🏴\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}";
        assert_eq!(split(england), [england]);
    }

    #[test]
    fn emoji_presentation() {
        assert!(wants_emoji("😀"));
        assert!(wants_emoji("🎉"));
        assert!(wants_emoji("❤\u{FE0F}"));
        assert!(wants_emoji("1\u{FE0F}\u{20E3}"));
        assert!(wants_emoji("👍🏽"));
        assert!(wants_emoji("🇩🇪"));
        assert!(wants_emoji("👨\u{200D}👩\u{200D}👧"));

        assert!(!wants_emoji("a"));
        assert!(!wants_emoji("1"));
        assert!(!wants_emoji("❤"));
        assert!(!wants_emoji("☃"));
        assert!(!wants_emoji("😀\u{FE0E}"));
        assert!(!wants_emoji("漢"));
    }
}
