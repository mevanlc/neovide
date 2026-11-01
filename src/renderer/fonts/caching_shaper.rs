use std::cell::OnceCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::rc::Rc;

use itertools::Itertools;
use log::debug;
use log::error;
use log::info;
use log::trace;
use log::warn;
use lru::LruCache;
use skia_safe::TextBlob;
use skia_safe::TextBlobBuilder;
use skia_safe::graphics::font_cache_limit;
use skia_safe::graphics::font_cache_used;
use skia_safe::graphics::set_font_cache_limit;
use swash::Metrics;
use swash::shape::ShapeContext;
use swash::text::Script;
use swash::text::cluster::CharCluster;
use swash::text::cluster::Parser;
use swash::text::cluster::Status;
use swash::text::cluster::Token;

use crate::editor::Word;
use crate::error_msg;
use crate::profiling::tracy_zone;
use crate::renderer::fonts::emoji::supports_grapheme;
use crate::renderer::fonts::emoji::uses_emoji_presentation;
use crate::renderer::fonts::font_loader::*;
use crate::renderer::fonts::font_options::*;
use crate::units::PixelSize;

#[derive(Clone, Hash, PartialEq, Eq, Debug)]
struct ShapeKey {
    pub text: String,
    pub style: CoarseStyle,
}

impl ShapeKey {
    pub fn new(text: String, style: CoarseStyle) -> Self { Self { text, style } }
}

const FONT_CACHE_SIZE: usize = 8 * 1024 * 1024;

pub struct CachingShaper {
    options: FontOptions,
    font_loader: FontLoader,
    // Keep emoji-only candidates out of ordinary fallback's loaded-font search.
    emoji_font_loader: FontLoader,
    system_emoji_font: OnceCell<Option<Rc<FontPair>>>,
    blob_cache: LruCache<ShapeKey, Vec<TextBlob>>,
    shape_context: ShapeContext,
    scale_factor: f32,
    linespace: f32,
    font_info: Option<(Metrics, f32)>,
}

fn filter_failed_fonts(mut options: FontOptions, font_keys: &HashSet<FontKey>) -> FontOptions {
    let original_normal = options.normal.clone();
    let hinting = options.hinting.clone();
    let edging = options.edging.clone();

    options.normal.retain(|font_desc| {
        let key = FontKey {
            font_desc: Some(font_desc.clone()),
            hinting: hinting.clone(),
            edging: edging.clone(),
        };

        !font_keys.contains(&key)
    });

    if options.normal.is_empty() {
        options.normal = original_normal;
    }

    options
}

impl CachingShaper {
    pub fn new(scale_factor: f32) -> CachingShaper {
        let options = FontOptions::default();
        let font_size = options.size * scale_factor;
        let mut shaper = CachingShaper {
            options,
            font_loader: FontLoader::new(font_size),
            emoji_font_loader: FontLoader::new(font_size),
            system_emoji_font: OnceCell::new(),
            blob_cache: LruCache::new(NonZeroUsize::new(10000).unwrap()),
            shape_context: ShapeContext::new(),
            scale_factor,
            linespace: 0.0,
            font_info: None,
        };
        shaper.reset_font_loader();
        shaper
    }

    fn current_font_pair(&mut self) -> Rc<FontPair> {
        self.font_loader
            .get_or_load(&FontKey {
                font_desc: self.options.primary_font(),
                hinting: self.options.hinting.clone(),
                edging: self.options.edging.clone(),
            })
            .unwrap_or_else(|| {
                self.font_loader
                    .get_or_load(&FontKey::default())
                    .expect("Could not load default font")
            })
    }

    pub fn current_size(&self) -> f32 {
        let min_font_size = 1.0;
        (self.options.size * self.scale_factor).max(min_font_size)
    }

    pub fn update_scale_factor(&mut self, scale_factor: f32) {
        debug!("scale_factor changed: {scale_factor:.2}");
        self.scale_factor = scale_factor;
        self.reset_font_loader();
    }

    pub fn update_font(&mut self, guifont_setting: &str) {
        debug!("Updating font: {guifont_setting}");

        let mut options = match FontOptions::parse(guifont_setting) {
            Ok(opt) => opt,
            Err(msg) => {
                error_msg!("Failed to parse guifont: {}", msg);
                return;
            }
        };

        // guifont controls text fonts; it has no syntax for the configured emoji policy.
        options.emoji.clone_from(&self.options.emoji);
        self.update_font_options(options);
    }

    pub fn update_font_options(&mut self, options: FontOptions) {
        debug!("Updating font options: {options:?}");

        let keys = options
            .possible_fonts()
            .iter()
            .map(|desc| FontKey {
                font_desc: Some(desc.clone()),
                hinting: options.hinting.clone(),
                edging: options.edging.clone(),
            })
            .unique()
            .collect::<Vec<_>>();

        let failed_fonts =
            keys.iter().filter(|key| self.font_loader.get_or_load(key).is_none()).collect_vec();

        if !keys.is_empty() && failed_fonts.len() == keys.len() {
            error_msg!(
                "Font can't be updated to: {:#?}\n\
                No candidate fonts could be loaded: {}",
                options,
                failed_fonts.iter().join(",\n"),
            );
            return;
        }

        if !failed_fonts.is_empty() {
            warn!(
                "Some configured fonts could not be loaded and will be skipped: {}",
                failed_fonts.iter().join(", ")
            );
        }

        let failed_font_keys = failed_fonts.into_iter().cloned().collect::<HashSet<_>>();
        let options = filter_failed_fonts(options, &failed_font_keys);

        debug!("Font updated to: {options:?}");
        self.options = options;
        self.reset_font_loader();
    }

    pub fn update_linespace(&mut self, linespace: f32) {
        debug!("Updating linespace: {linespace}");

        let font_height = self.font_base_dimensions().height;
        let impossible_linespace = font_height + linespace <= 0.0;

        if !impossible_linespace {
            debug!("Linespace updated to: {linespace}");
            self.linespace = linespace;
            self.reset_font_loader();
        } else {
            let reason = if impossible_linespace {
                "Linespace too negative, would make font invisible"
            } else {
                "Font not found"
            };
            error!("Linespace can't be updated to {linespace} due to: {reason}");
        }
    }

    fn reset_font_loader(&mut self) {
        tracy_zone!("reset_font_loader");
        self.font_info = None;
        let font_size = self.current_size();

        self.font_loader = FontLoader::new(font_size);
        self.emoji_font_loader = FontLoader::new(font_size);
        self.system_emoji_font.take();
        let (_, font_width) = self.info();
        info!("Reset Font Loader: font_size: {font_size:.2}px, font_width: {font_width:.2}px");

        self.blob_cache.clear();
    }

    pub fn font_names(&self) -> Vec<String> { self.font_loader.font_names() }

    fn info(&mut self) -> (Metrics, f32) {
        if let Some(info) = self.font_info {
            return info;
        }

        let font_pair = self.current_font_pair();
        let size = self.current_size();
        let mut shaper =
            self.shape_context.builder(font_pair.swash_font.as_ref()).size(size).build();
        shaper.add_str("M");
        let metrics = shaper.metrics();
        let mut advance = metrics.average_width;
        shaper.shape_with(|cluster| {
            advance = cluster.glyphs.first().map_or(metrics.average_width, |g| g.advance);
        });
        self.font_info = Some((metrics, advance));
        (metrics, advance)
    }

    fn metrics(&mut self) -> Metrics {
        tracy_zone!("font_metrics");
        self.info().0
    }

    pub fn font_base_dimensions(&mut self) -> PixelSize<f32> {
        let (metrics, glyph_advance) = self.info();

        let bare_font_height = metrics.ascent + metrics.descent + metrics.leading;
        // assuming that linespace is checked on receive for validity
        let font_height = (bare_font_height + self.linespace).ceil();
        let font_width = glyph_advance + self.options.width;

        (font_width, font_height).into()
    }

    pub fn underline_offset(&mut self) -> f32 {
        let metrics = self.metrics();
        if let Some(underline_offset) = self.options.underline_offset {
            -underline_offset
        } else if metrics.underline_offset != 0. {
            metrics.underline_offset
        } else {
            // If a font does not have an underline_offset, use the stroke_size as offset
            // A negative offset places the underline below the baseline
            -metrics.stroke_size
        }
    }

    pub fn stroke_size(&mut self) -> f32 { self.metrics().stroke_size }

    pub fn baseline_offset(&mut self) -> f32 {
        let metrics = self.metrics();
        // NOTE: leading is also called linegap and should be equally distributed on the top and
        // bottom, so it's centered like our linespace settings. That's how it works on the web,
        // but some desktop applications only use the top according to:
        // https://googlefonts.github.io/gf-guide/metrics.html#8-linegap-values-must-be-0
        metrics.ascent + (metrics.leading + self.linespace) / 2.0
    }

    fn emoji_font_for_grapheme(&mut self, text: &str) -> Option<Rc<FontPair>> {
        if !uses_emoji_presentation(text) {
            return None;
        }
        match &self.options.emoji {
            EmojiFont::Disabled => None,
            EmojiFont::Auto => self
                .system_emoji_font
                .get_or_init(|| {
                    self.emoji_font_loader.load_system_emoji_font(
                        self.options.hinting.clone(),
                        self.options.edging.clone(),
                    )
                })
                .as_ref()
                .filter(|font| supports_grapheme(font.swash_font.as_ref(), text))
                .cloned(),
            EmojiFont::Fonts(fonts) => fonts.iter().find_map(|font| {
                let key = FontKey {
                    font_desc: Some(font.clone()),
                    hinting: self.options.hinting.clone(),
                    edging: self.options.edging.clone(),
                };
                let font = self.emoji_font_loader.get_or_load(&key)?;
                supports_grapheme(font.swash_font.as_ref(), text).then_some(font)
            }),
        }
    }

    fn build_clusters(
        &mut self,
        word: Word<'_>,
        style: CoarseStyle,
    ) -> Vec<(Vec<CharCluster>, Rc<FontPair>)> {
        let mut cluster = CharCluster::new();

        // Neovim's graphemes may span multiple shaping clusters (for example,
        // regional indicators). Select one font covering the entire grapheme.
        let emoji_fonts: HashMap<_, _> =
            if self.options.emoji == EmojiFont::Disabled || word.text.is_ascii() {
                HashMap::new()
            } else {
                word.grapheme_clusters()
                    .filter_map(|(cell, text)| {
                        self.emoji_font_for_grapheme(text).map(|font| (cell as u32, font))
                    })
                    .collect()
            };

        // Enumerate the characters storing the glyph index in the user data so that we can position
        // glyphs according to Neovim's grid rules
        let mut parser = Parser::new(
            Script::Latin,
            word.grapheme_clusters().flat_map(|(cell_index, cluster)| {
                cluster.char_indices().map(move |(offset, character)| Token {
                    ch: character,
                    offset: offset as u32,
                    len: character.len_utf8() as u8,
                    info: character.into(),
                    data: cell_index as u32,
                })
            }),
        );

        let mut results = Vec::new();
        'cluster: while parser.next(&mut cluster) {
            if let Some(font) = emoji_fonts.get(&cluster.user_data()) {
                results.push((cluster.to_owned(), font.clone()));
                continue;
            }

            // TODO: Don't redo this work for every cluster. Save it some how
            // Create font fallback list
            let mut font_fallback_keys = Vec::new();

            // Add parsed fonts from guifont or config file
            font_fallback_keys.extend(
                self.options
                    .font_list(style)
                    .iter()
                    .map(|font_desc| FontKey {
                        font_desc: Some(font_desc.clone()),
                        hinting: self.options.hinting.clone(),
                        edging: self.options.edging.clone(),
                    })
                    .unique(),
            );

            // Add default font
            font_fallback_keys.push(FontKey {
                font_desc: None,
                hinting: self.options.hinting.clone(),
                edging: self.options.edging.clone(),
            });

            // Use the cluster.map function to select a viable font from the fallback list and loaded fonts

            let mut best = None;
            // Search through the configured and default fonts for a match
            for fallback_key in font_fallback_keys.iter() {
                if let Some(font_pair) = self.font_loader.get_or_load(fallback_key) {
                    let charmap = font_pair.swash_font.as_ref().charmap();
                    match cluster.map(|ch| charmap.map(ch)) {
                        Status::Complete => {
                            results.push((cluster.to_owned(), font_pair.clone()));
                            continue 'cluster;
                        }
                        Status::Keep => best = Some(font_pair),
                        Status::Discard => {}
                    }
                }
            }

            // Configured font/default didn't work. Search through currently loaded ones
            for loaded_font in self.font_loader.loaded_fonts() {
                let charmap = loaded_font.swash_font.as_ref().charmap();
                match cluster.map(|ch| charmap.map(ch)) {
                    Status::Complete => {
                        results.push((cluster.to_owned(), loaded_font.clone()));
                        self.font_loader.refresh(loaded_font.as_ref());
                        continue 'cluster;
                    }
                    Status::Keep => best = Some(loaded_font),
                    Status::Discard => {}
                }
            }

            if let Some(best) = best {
                results.push((cluster.to_owned(), best.clone()));
            } else {
                let fallback_character = cluster.chars()[0].ch;
                if let Some(fallback_font) =
                    self.font_loader.load_font_for_character(style, fallback_character)
                {
                    results.push((cluster.to_owned(), fallback_font));
                } else {
                    // Last Resort covers all of the unicode space so we will always have a fallback
                    results.push((
                        cluster.to_owned(),
                        self.font_loader.get_or_load_last_resort().unwrap(),
                    ));
                }
            }
        }

        // Now we have to group clusters by the font used so that the shaper can actually form
        // ligatures across clusters
        let mut grouped_results = Vec::new();
        let mut current_group = Vec::new();
        let mut current_font_option = None;
        for (cluster, font) in results {
            if let Some(current_font) = current_font_option.clone() {
                if current_font == font {
                    current_group.push(cluster);
                } else {
                    grouped_results.push((current_group, current_font));
                    current_group = vec![cluster];
                    current_font_option = Some(font);
                }
            } else {
                current_group = vec![cluster];
                current_font_option = Some(font);
            }
        }

        if !current_group.is_empty() {
            grouped_results.push((current_group, current_font_option.unwrap()));
        }

        grouped_results
    }

    pub fn cleanup_font_cache(&self) {
        // Only purge if we are truly about to exhaust the cache.
        // See: https://github.com/neovide/neovide/issues/3299
        // On high-DPI displays the old unconditional purge invalidated
        // glyphs every frame, which forced the GPU to keep
        // re-uploading textures and tanked performance.
        let limit = font_cache_limit();
        let used = font_cache_used();

        if used > limit * 9 / 10 {
            tracy_zone!("purge_font_cache");
            set_font_cache_limit(FONT_CACHE_SIZE / 2);
            set_font_cache_limit(FONT_CACHE_SIZE);
        }
    }

    pub fn shape(&mut self, word: Word<'_>, style: CoarseStyle) -> Vec<TextBlob> {
        let current_size = self.current_size();
        let glyph_width = self.font_base_dimensions().width;

        let mut resulting_blobs = Vec::new();

        for (cluster_group, font_pair) in self.build_clusters(word, style) {
            let features = self.get_font_features(
                font_pair.as_ref().key.font_desc.as_ref().map(|desc| desc.family.as_str()),
            );

            let mut shaper = self
                .shape_context
                .builder(font_pair.swash_font.as_ref())
                .features(features.iter().map(|(name, value)| (name.as_ref(), *value)))
                .size(current_size)
                .build();

            let charmap = font_pair.swash_font.as_ref().charmap();
            for mut cluster in cluster_group {
                cluster.map(|ch| charmap.map(ch));
                shaper.add_cluster(&cluster);
            }

            let mut glyph_data = Vec::new();

            shaper.shape_with(|glyph_cluster| {
                //Align to the grid at the start of each cluster
                let mut x_offset = glyph_width * glyph_cluster.data as f32;

                for glyph in glyph_cluster.glyphs {
                    let position = (x_offset + glyph.x, -glyph.y);
                    glyph_data.push((glyph.id, position));
                    x_offset += glyph.advance;
                }
            });

            if glyph_data.is_empty() {
                continue;
            }

            let mut blob_builder = TextBlobBuilder::new();
            let (glyphs, positions) =
                blob_builder.alloc_run_pos(&font_pair.skia_font, glyph_data.len(), None);
            for (i, (glyph_id, glyph_position)) in glyph_data.iter().enumerate() {
                glyphs[i] = *glyph_id;
                positions[i] = (*glyph_position).into();
            }

            let blob = blob_builder.make();
            resulting_blobs.push(blob.expect("Could not create textblob"));
        }

        resulting_blobs
    }

    pub fn shape_cached(&mut self, word: Word<'_>, style: CoarseStyle) -> &Vec<TextBlob> {
        tracy_zone!("shape_cached");
        let text = word.text;
        let key = ShapeKey::new(text.to_string(), style);

        if !self.blob_cache.contains(&key) {
            trace!("Shaping text: {text:?}");
            let blobs = self.shape(word, style);
            self.blob_cache.put(key.clone(), blobs);
        }

        self.blob_cache.get(&key).unwrap()
    }

    fn get_font_features(&self, name: Option<&str>) -> Vec<(String, u16)> {
        if let Some(name) = name {
            self.options
                .features
                .get(name)
                .map(|features| {
                    features
                        .iter()
                        .map(|feature| (feature.0.clone(), feature.1))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn automatic_emoji_font_is_loaded_only_when_needed() {
        let mut shaper = CachingShaper::new(1.0);
        assert!(shaper.system_emoji_font.get().is_none());
        shaper.shape_cached(Word::new("text", &[1, 1, 1, 1]), CoarseStyle::default());
        assert!(shaper.system_emoji_font.get().is_none());
        shaper.emoji_font_for_grapheme("⚠️");
        assert!(shaper.system_emoji_font.get().is_some());
    }

    #[test]
    fn emoji_policy_can_be_configured_with_the_bundled_normal_font() {
        let mut shaper = CachingShaper::new(1.0);
        let dimensions = shaper.font_base_dimensions();
        shaper.update_font_options(FontOptions {
            emoji: EmojiFont::Disabled,
            ..FontOptions::default()
        });
        assert_eq!(shaper.options.emoji, EmojiFont::Disabled);
        assert_eq!(shaper.font_base_dimensions(), dimensions);
    }

    #[test]
    fn changing_guifont_preserves_the_emoji_policy() {
        let mut shaper = CachingShaper::new(1.0);
        for emoji in [
            EmojiFont::Disabled,
            EmojiFont::Fonts(vec![FontDescription { family: "Custom Emoji".into(), style: None }]),
        ] {
            shaper.update_font_options(FontOptions {
                emoji: emoji.clone(),
                size: points_to_pixels(16.0),
                ..FontOptions::default()
            });
            shaper.update_font("");
            assert_eq!(shaper.options.emoji, emoji);
            assert_eq!(shaper.current_size(), points_to_pixels(DEFAULT_FONT_SIZE));
        }
    }

    #[cfg(target_os = "macos")]
    mod macos_emoji {
        use skia_safe::Color;
        use skia_safe::Paint;
        use skia_safe::surfaces;

        use super::*;

        fn options(emoji: EmojiFont) -> FontOptions {
            let mut options = FontOptions::parse("Menlo:h16").unwrap();
            options.emoji = emoji;
            options
        }

        fn family(name: &str) -> FontDescription {
            FontDescription { family: name.into(), style: None }
        }

        fn fonts_for_text(shaper: &mut CachingShaper, text: &str) -> Vec<FontDescription> {
            shaper
                .build_clusters(Word::new(text, &[text.len() as u8, 0]), CoarseStyle::default())
                .into_iter()
                .map(|(_, font)| font.key.font_desc.clone().unwrap())
                .collect()
        }

        #[test]
        fn automatic_emoji_priority_respects_text_presentation() {
            let mut shaper = CachingShaper::new(1.0);
            shaper.update_font_options(options(EmojiFont::Auto));
            for text in ["⚠️", "✅", "👩🏽‍💻", "🇺🇸", "1️⃣", "☝🏽"] {
                assert_eq!(
                    fonts_for_text(&mut shaper, text),
                    [family("Apple Color Emoji")],
                    "{text}"
                );
            }
            for text in ["⚠", "⚠︎", "1", "#", "*", "a"] {
                assert_eq!(fonts_for_text(&mut shaper, text), [family("Menlo")], "{text}");
            }
        }

        #[test]
        fn custom_emoji_fonts_use_order_and_require_whole_grapheme_coverage() {
            let mut shaper = CachingShaper::new(1.0);
            shaper.update_font_options(options(EmojiFont::Fonts(vec![
                family("Missing Emoji Font"),
                family("Menlo"),
                family("Apple Color Emoji"),
            ])));
            assert_eq!(fonts_for_text(&mut shaper, "⚠️"), [family("Menlo")]);
            assert_eq!(fonts_for_text(&mut shaper, "👩🏽‍💻"), [family("Apple Color Emoji")]);
            assert_eq!(fonts_for_text(&mut shaper, "🇺🇸"), [family("Apple Color Emoji")]);

            // Explicit emoji fonts keep their own style, even in bold/italic text.
            for style in CoarseStyle::permutations() {
                let groups = shaper.build_clusters(Word::new("⚠️", &[6, 0]), style);
                assert_eq!(groups[0].1.key.font_desc, Some(family("Menlo")));
            }
            assert!(shaper.font_loader.loaded_fonts().iter().all(|font| {
                font.key.font_desc.as_ref().unwrap().family != "Apple Color Emoji"
            }));

            shaper.update_font_options(options(EmojiFont::Fonts(vec![
                family("Apple Color Emoji"),
                family("Menlo"),
            ])));
            assert_eq!(fonts_for_text(&mut shaper, "⚠️"), [family("Apple Color Emoji")]);
        }

        #[test]
        fn missing_or_incomplete_emoji_fonts_leave_ordinary_fallback_available() {
            let mut shaper = CachingShaper::new(1.0);
            for emoji in [
                EmojiFont::Disabled,
                EmojiFont::Fonts(Vec::new()),
                EmojiFont::Fonts(vec![family("Missing Emoji Font")]),
                EmojiFont::Fonts(vec![family("Apple Symbols")]),
            ] {
                shaper.update_font_options(options(emoji));
                assert_eq!(fonts_for_text(&mut shaper, "✅"), [family("Apple Color Emoji")]);
            }
            shaper
                .update_font_options(options(EmojiFont::Fonts(vec![family("Missing Emoji Font")])));
            assert_eq!(fonts_for_text(&mut shaper, "⚠️"), [family("Menlo")]);
        }

        fn warning_has_color(shaper: &mut CachingShaper) -> bool {
            let mut surface = surfaces::raster_n32_premul((96, 96)).unwrap();
            let canvas = surface.canvas();
            canvas.clear(Color::TRANSPARENT);
            let mut paint = Paint::default();
            paint.set_color(Color::WHITE);
            for blob in shaper.shape_cached(Word::new("⚠️", &[6, 0]), CoarseStyle::default()) {
                canvas.draw_text_blob(blob, (16.0, 64.0), &paint);
            }
            let pixels = surface.peek_pixels().unwrap();
            (0..pixels.height()).any(|y| {
                (0..pixels.width()).any(|x| {
                    let color = pixels.get_color((x, y));
                    color.a() > 0 && (color.r() != color.g() || color.g() != color.b())
                })
            })
        }

        #[test]
        fn reloading_emoji_policy_updates_color_without_changing_grid_metrics() {
            let mut shaper = CachingShaper::new(1.0);
            shaper.update_font_options(options(EmojiFont::Disabled));
            let dimensions = shaper.font_base_dimensions();
            assert!(!warning_has_color(&mut shaper));

            shaper.update_font_options(options(EmojiFont::Auto));
            assert!(warning_has_color(&mut shaper));
            assert_eq!(shaper.font_base_dimensions(), dimensions);

            shaper.update_font_options(options(EmojiFont::Disabled));
            assert!(!warning_has_color(&mut shaper));
            assert_eq!(shaper.font_base_dimensions(), dimensions);
        }
    }

    struct PlatformDefault {
        guifont: String,
        failed_family: String,
        expected: Vec<String>,
    }

    fn filter_normal_families(guifont: &str, failed_families: Vec<String>) -> Vec<String> {
        let options = FontOptions::parse(guifont).unwrap();
        let failed_font_keys = failed_families
            .into_iter()
            .map(|family| FontKey {
                font_desc: Some(FontDescription { family, style: None }),
                hinting: options.hinting.clone(),
                edging: options.edging.clone(),
            })
            .collect::<HashSet<_>>();

        filter_failed_fonts(options, &failed_font_keys)
            .normal
            .into_iter()
            .map(|font| font.family)
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn platform_default_guifont_case() -> PlatformDefault {
        PlatformDefault {
            guifont: "Source Code Pro,DejaVu Sans Mono,Courier New,monospace".to_string(),
            failed_family: "Source Code Pro".to_string(),
            expected: vec![
                "DejaVu Sans Mono".to_string(),
                "Courier New".to_string(),
                "monospace".to_string(),
            ],
        }
    }

    #[cfg(target_os = "macos")]
    fn platform_default_guifont_case() -> PlatformDefault {
        PlatformDefault {
            guifont: "SF Mono,Menlo,Monaco,Courier New,monospace".to_string(),
            failed_family: "SF Mono".to_string(),
            expected: vec![
                "Menlo".to_string(),
                "Monaco".to_string(),
                "Courier New".to_string(),
                "monospace".to_string(),
            ],
        }
    }

    #[cfg(target_os = "windows")]
    fn platform_default_guifont_case() -> PlatformDefault {
        PlatformDefault {
            guifont: "Cascadia Code,Cascadia Mono,Consolas,Courier New,monospace".to_string(),
            failed_family: "Cascadia Code".to_string(),
            expected: vec![
                "Cascadia Mono".to_string(),
                "Consolas".to_string(),
                "Courier New".to_string(),
                "monospace".to_string(),
            ],
        }
    }

    #[test]
    fn filters_failed_normal_fonts_and_preserves_order() {
        let options = FontOptions {
            normal: vec![
                FontDescription { family: "Missing".to_string(), style: None },
                FontDescription { family: "DejaVu Sans Mono".to_string(), style: None },
                FontDescription { family: "Courier New".to_string(), style: None },
            ],
            ..FontOptions::default()
        };

        let failed_font_keys = HashSet::from([FontKey {
            font_desc: Some(FontDescription { family: "Missing".to_string(), style: None }),
            hinting: options.hinting.clone(),
            edging: options.edging.clone(),
        }]);

        let filtered = filter_failed_fonts(options, &failed_font_keys);

        assert_eq!(
            filtered.normal,
            vec![
                FontDescription { family: "DejaVu Sans Mono".to_string(), style: None },
                FontDescription { family: "Courier New".to_string(), style: None },
            ]
        );
    }

    #[test]
    fn does_not_empty_normal_fonts_when_every_normal_font_fails() {
        let options = FontOptions {
            normal: vec![FontDescription { family: "Missing".to_string(), style: None }],
            ..FontOptions::default()
        };

        let failed_font_keys = HashSet::from([FontKey {
            font_desc: Some(FontDescription { family: "Missing".to_string(), style: None }),
            hinting: options.hinting.clone(),
            edging: options.edging.clone(),
        }]);

        let filtered = filter_failed_fonts(options.clone(), &failed_font_keys);

        assert_eq!(filtered.normal, options.normal);
    }

    #[test]
    fn filters_failed_font_from_neovim_platform_default_guifont() {
        let case = platform_default_guifont_case();

        assert_eq!(
            filter_normal_families(&case.guifont, vec![case.failed_family.clone()]),
            case.expected
        );
    }

    #[test]
    fn filters_failed_font_from_neovim_linux_default_guifont() {
        assert_eq!(
            filter_normal_families(
                "Source Code Pro,DejaVu Sans Mono,Courier New,monospace",
                vec!["Source Code Pro".to_string()]
            ),
            vec!["DejaVu Sans Mono", "Courier New", "monospace"]
        );
    }

    #[test]
    fn filters_failed_font_from_neovim_macos_default_guifont() {
        assert_eq!(
            filter_normal_families(
                "SF Mono,Menlo,Monaco,Courier New,monospace",
                vec!["SF Mono".to_string()]
            ),
            vec!["Menlo", "Monaco", "Courier New", "monospace"]
        );
    }

    #[test]
    fn filters_failed_font_from_neovim_windows_default_guifont() {
        assert_eq!(
            filter_normal_families(
                "Cascadia Code,Cascadia Mono,Consolas,Courier New,monospace",
                vec!["Cascadia Code".to_string()]
            ),
            vec!["Cascadia Mono", "Consolas", "Courier New", "monospace"]
        );
    }

    #[test]
    fn filters_failed_font_from_neovim_fallback_default_guifont() {
        assert_eq!(
            filter_normal_families(
                "DejaVu Sans Mono,Courier New,monospace",
                vec!["DejaVu Sans Mono".to_string()]
            ),
            vec!["Courier New", "monospace"]
        );
    }
}
