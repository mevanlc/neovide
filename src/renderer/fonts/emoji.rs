use swash::FontRef;
use swash::text::Script;
use swash::text::cluster::CharCluster;
use swash::text::cluster::Parser;
use swash::text::cluster::Status;
use swash::text::cluster::Token;
use unicode_properties::EmojiStatus;
use unicode_properties::UnicodeEmoji;

/// Whether a Neovim grapheme requests emoji presentation. Inspect the original text:
/// Swash drops presentation selectors while building its shaping clusters.
pub(super) fn uses_emoji_presentation(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(base) = chars.next() else {
        return false;
    };
    if base.is_ascii() && !matches!(base, '#' | '*' | '0'..='9') {
        return false;
    }
    if !base.is_emoji_char() {
        return false;
    }

    match chars.next() {
        Some('\u{fe0e}') => false,
        Some('\u{fe0f}') => true,
        Some('\u{20e3}') if matches!(base, '#' | '*' | '0'..='9') => true,
        // Modifier sequences also give text-default bases (such as U+261D) emoji presentation.
        Some('\u{1f3fb}'..='\u{1f3ff}') => matches!(
            base.emoji_status(),
            EmojiStatus::EmojiModifierBase | EmojiStatus::EmojiPresentationAndModifierBase
        ),
        Some('\u{200d}') => {
            !base.is_emoji_component()
                && chars.next().is_some_and(|ch| ch.is_emoji_char() && !ch.is_emoji_component())
        }
        _ => matches!(
            base.emoji_status(),
            EmojiStatus::EmojiPresentation
                | EmojiStatus::EmojiPresentationAndModifierBase
                | EmojiStatus::EmojiPresentationAndEmojiComponent
                | EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent
        ),
    }
}

/// Require coverage of the entire grapheme, including all parts of joined emoji
/// and flags, before selecting an emoji font for any of its shaping clusters.
pub(super) fn supports_grapheme(font: FontRef<'_>, text: &str) -> bool {
    let mut parser = Parser::new(
        Script::Latin,
        text.char_indices().map(|(offset, ch)| Token {
            ch,
            offset: offset as u32,
            len: ch.len_utf8() as u8,
            info: ch.into(),
            data: 0,
        }),
    );
    let charmap = font.charmap();
    let mut cluster = CharCluster::new();
    while parser.next(&mut cluster) {
        if cluster.map(|ch| charmap.map(ch)) != Status::Complete {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_unicode_presentation_and_recognizes_sequences() {
        for text in ["✅", "😀", "⚠️", "©️", "👩🏽‍💻", "🇺🇸", "1️⃣", "1\u{20e3}", "☝🏽", "❤️‍🔥", "❤‍🔥"]
        {
            assert!(uses_emoji_presentation(text), "{text:?}");
        }
        for text in
            ["", "a", "1", "#", "*", "⚠", "⚠︎", "✅︎", "©", "a\u{fe0f}", "\u{fe0f}", "1🏽", "1‍😀"]
        {
            assert!(!uses_emoji_presentation(text), "{text:?}");
        }
    }
}
