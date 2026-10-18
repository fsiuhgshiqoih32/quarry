//! egui's built-in fonts cover Latin, Greek and Cyrillic. File names in Chinese, Japanese, Korean
//! and other scripts need a system font as a fallback, or they render as boxes.

use std::sync::Arc;

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

#[cfg(windows)]
const CANDIDATES: &[&[&str]] = &[
    &[
        r"C:\Windows\Fonts\msyh.ttc",
        r"C:\Windows\Fonts\YuGothR.ttc",
        r"C:\Windows\Fonts\malgun.ttf",
        r"C:\Windows\Fonts\simsun.ttc",
    ],
    &[r"C:\Windows\Fonts\seguisym.ttf"],
    &[r"C:\Windows\Fonts\Nirmala.ttc", r"C:\Windows\Fonts\Nirmala.ttf"],
];

#[cfg(target_os = "macos")]
const CANDIDATES: &[&[&str]] = &[
    &[
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
    ],
    &["/System/Library/Fonts/Apple Symbols.ttf"],
];

#[cfg(not(any(windows, target_os = "macos")))]
const CANDIDATES: &[&[&str]] = &[
    &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ],
    &["/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", "/usr/share/fonts/TTF/DejaVuSans.ttf"],
];

pub fn install(ctx: &egui::Context) {
    let mut defs = FontDefinitions::default();
    for (i, group) in CANDIDATES.iter().enumerate() {
        let Some(bytes) = group.iter().find_map(|p| std::fs::read(p).ok()) else { continue };
        let name = format!("system-fallback-{i}");
        defs.font_data.insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            defs.families.entry(family).or_default().push(name.clone());
        }
    }
    ctx.set_fonts(defs);
}
