//! The icon before a file's name, in every list that shows files (the download list, Download
//! File Info, Properties, a torrent's files, the Scheduler's): the system's own icon for its type,
//! the one its file manager shows. Windows' shell (win32.rs), macOS Finder (unix.rs), the Linux
//! desktop's icon theme (unix.rs, xdgicons.rs). Each type is looked up once per pixel size, on a
//! background thread (a type's first lookup can load its shell handler), and kept. Until its
//! icon is ready, and wherever the system has none (a Linux desktop without an icon theme), a
//! Tango icon the app carries is drawn (public domain, assets/icons/tango/).

use std::collections::HashMap;
use std::sync::mpsc::{Sender, channel};
use std::sync::{LazyLock, Mutex};

use egui::emath::GuiRounding as _;
use egui::{Color32, ImageSource, Painter, Rect, Ui, pos2};

/// What the icon is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    /// A file, by its name (the extension decides; case doesn't matter).
    File(&'a str),
    /// A folder (a torrent's folder, a tree's branch).
    Folder,
    /// An open folder (Download complete's: the file is in it). Windows has its own; elsewhere
    /// the theme's "folder-open", else its folder.
    FolderOpen,
}

/// What the system is asked for: a folder, or a file by its extension (lowercase; None: a file
/// without one).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum What {
    Folder,
    FolderOpen,
    File(Option<String>),
}

impl What {
    pub fn of(kind: Kind) -> Self {
        match kind {
            Kind::Folder => What::Folder,
            Kind::FolderOpen => What::FolderOpen,
            Kind::File(name) => What::File(extension(name)),
        }
    }
}

/// An icon as a system gives it.
pub enum Native {
    /// `side` × `side` RGBA pixels, alpha premultiplied or not; any size, fitted here.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Rgba {
        side: u32,
        rgba: Vec<u8>,
        premultiplied: bool,
    },
    /// A file of the desktop's icon theme (SVG or PNG), drawn through egui's image loaders.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    File(std::path::PathBuf),
}

/// The system's icon for `what` at about `px` pixels (None: it has none).
pub(crate) fn system(what: &What, px: u32) -> Option<Native> {
    // MDM_SYSTEM_ICONS=0: Tango's only, as a desktop without icons shows them (checks, shots).
    if std::env::var_os("MDM_SYSTEM_ICONS").is_some_and(|v| v == "0") {
        return None;
    }
    #[cfg(windows)]
    {
        crate::win32::file_icon(what, px)
    }
    #[cfg(unix)]
    {
        crate::unix::file_icon(what, px)
    }
}

/// The extension of `name` (after its last dot, lowercase), if it has one.
fn extension(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 16 => {
            Some(ext.to_ascii_lowercase())
        }
        _ => None,
    }
}

// ---- the cache and its lookup thread ----------------------------------------------------------

/// What is drawn for a type at a size.
#[derive(Clone)]
enum Shown {
    Texture(egui::TextureHandle),
    Image(ImageSource<'static>),
    /// The system has none: the Tango icon.
    Fallback,
}

#[derive(Default)]
struct Cache {
    /// None while the lookup runs.
    icons: HashMap<(What, u32), Option<Shown>>,
    lookups: Option<Sender<(What, u32, egui::Context)>>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

fn cache() -> std::sync::MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// What to draw for `what` at `px` now: the system's icon when it is ready (or the same type's
/// at another size while this one is looked up, so a zoom doesn't flash), else None (Tango's).
/// Asks for the lookup the first time.
fn shown(ctx: &egui::Context, what: &What, px: u32) -> Option<Shown> {
    let mut c = cache();
    let key = (what.clone(), px);
    match c.icons.get(&key) {
        Some(Some(Shown::Fallback)) => return None,
        Some(Some(s)) => return Some(s.clone()),
        Some(None) => {}
        None => {
            c.icons.insert(key, None);
            let tx = c.lookups.get_or_insert_with(start_lookups);
            let _ = tx.send((what.clone(), px, ctx.clone()));
        }
    }
    c.icons
        .iter()
        .filter_map(|((w, p), s)| match s {
            Some(s @ (Shown::Texture(_) | Shown::Image(_))) if w == what => Some((*p, s)),
            _ => None,
        })
        .min_by_key(|(p, _)| p.abs_diff(px))
        .map(|(_, s)| s.clone())
}

/// The lookup thread: one at a time, in the order asked.
fn start_lookups() -> Sender<(What, u32, egui::Context)> {
    let (tx, rx) = channel::<(What, u32, egui::Context)>();
    let spawned = std::thread::Builder::new()
        .name("file icons".into())
        .spawn(move || {
            for (what, px, ctx) in rx {
                // A failing lookup leaves that type on Tango's icon, not the app down.
                let got = std::panic::catch_unwind(|| system(&what, px))
                    .ok()
                    .flatten();
                let shown = to_shown(&ctx, &what, px, got);
                cache().icons.insert((what, px), Some(shown));
                ctx.request_repaint();
            }
        });
    if spawned.is_err() {
        // No thread: every type keeps Tango's icon (the requests are dropped).
        return channel().0;
    }
    tx
}

fn to_shown(ctx: &egui::Context, what: &What, px: u32, got: Option<Native>) -> Shown {
    match got {
        Some(Native::Rgba {
            side,
            rgba,
            premultiplied,
        }) if side > 0 && rgba.len() == (side * side * 4) as usize => {
            let pixels = fit(&rgba, side, premultiplied, px);
            let img =
                egui::ColorImage::from_rgba_premultiplied([px as usize, px as usize], &pixels);
            let name = format!("file-icon-{what:?}-{px}");
            Shown::Texture(ctx.load_texture(name, img, egui::TextureOptions::LINEAR))
        }
        Some(Native::File(path)) => match std::fs::read(&path) {
            Ok(bytes) => Shown::Image(ImageSource::Bytes {
                // The file's own name ends the address: the loaders go by its extension.
                uri: format!("bytes://mdm/file-icon/system/{}", path.display()).into(),
                bytes: egui::load::Bytes::Shared(bytes.into()),
            }),
            Err(_) => Shown::Fallback,
        },
        _ => Shown::Fallback,
    }
}

/// Premultiply straight RGBA pixels in place.
fn premultiply(rgba: &mut [u8]) {
    for c in rgba.as_chunks_mut::<4>().0 {
        let a = c[3] as u32;
        for v in &mut c[..3] {
            *v = ((*v as u32 * a + 127) / 255) as u8;
        }
    }
}

/// `rgba` (`side` square) at `px` square, alpha premultiplied: resized with a triangle filter
/// (an average of the pixels each covers when smaller), on premultiplied pixels so transparent
/// edges don't darken.
pub(crate) fn fit(rgba: &[u8], side: u32, premultiplied: bool, px: u32) -> Vec<u8> {
    let mut pre = rgba.to_vec();
    if !premultiplied {
        premultiply(&mut pre);
    }
    if side == px {
        return pre;
    }
    match image::RgbaImage::from_raw(side, side, pre) {
        Some(img) => {
            image::imageops::resize(&img, px, px, image::imageops::FilterType::Triangle).into_raw()
        }
        None => vec![0; (px * px * 4) as usize],
    }
}

// ---- file thumbnails ---------------------------------------------------------------------------

/// Cached thumbnail textures for an existing file on disk.
#[derive(Clone)]
pub struct Thumbnail {
    pub small: egui::TextureHandle,
    pub large: egui::TextureHandle,
    pub size: [u32; 2],
}

#[derive(Default)]
struct ThumbCache {
    /// `None` while the lookup runs; `Some(None)` when the file has no thumbnail.
    items: HashMap<std::path::PathBuf, Option<Option<Thumbnail>>>,
    lookups: Option<Sender<(std::path::PathBuf, u64, egui::Context)>>,
    generations: HashMap<std::path::PathBuf, u64>,
    next_gen: u64,
}

static THUMB_CACHE: LazyLock<Mutex<ThumbCache>> = LazyLock::new(Default::default);

fn thumb_cache() -> std::sync::MutexGuard<'static, ThumbCache> {
    THUMB_CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Return the cached thumbnail for `path`, or queue a background lookup on first ask.
pub fn thumbnail(ctx: &egui::Context, path: &std::path::Path) -> Option<Thumbnail> {
    let mut c = thumb_cache();
    if let Some(entry) = c.items.get(path) {
        return entry.as_ref().and_then(|t| t.clone());
    }
    c.next_gen = c.next_gen.wrapping_add(1);
    let generation = c.next_gen;
    let key = path.to_path_buf();
    c.items.insert(key.clone(), None);
    c.generations.insert(key.clone(), generation);
    let tx = c.lookups.get_or_insert_with(start_thumb_lookups);
    let _ = tx.send((key, generation, ctx.clone()));
    None
}

/// Drop any cached thumbnail for `path` so the next paint reads it afresh.
pub fn invalidate_thumbnail(path: &std::path::Path) {
    let mut c = thumb_cache();
    c.items.remove(path);
    c.next_gen = c.next_gen.wrapping_add(1);
    let generation = c.next_gen;
    c.generations.insert(path.to_path_buf(), generation);
}

fn start_thumb_lookups() -> Sender<(std::path::PathBuf, u64, egui::Context)> {
    let (tx, rx) = channel::<(std::path::PathBuf, u64, egui::Context)>();
    let spawned = std::thread::Builder::new()
        .name("file thumbnails".into())
        .spawn(move || {
            for (path, generation, ctx) in rx {
                let got = std::panic::catch_unwind(|| load_thumbnail(&path, 256))
                    .ok()
                    .flatten();
                let thumb = got.and_then(|raw| to_thumbnail(&ctx, &path, raw));
                let mut c = thumb_cache();
                if c.generations.get(&path).copied() == Some(generation) {
                    c.items.insert(path, Some(thumb));
                    ctx.request_repaint();
                }
            }
        });
    if spawned.is_err() {
        return channel().0;
    }
    tx
}

fn fit_rect_size(w: u32, h: u32, max_px: u32) -> (u32, u32) {
    if w == 0 || h == 0 || (w <= max_px && h <= max_px) {
        return (w.max(1), h.max(1));
    }
    if w >= h {
        let nh = ((h as u64 * max_px as u64 + (w as u64 / 2)) / w as u64).max(1) as u32;
        (max_px, nh)
    } else {
        let nw = ((w as u64 * max_px as u64 + (h as u64 / 2)) / h as u64).max(1) as u32;
        (nw, max_px)
    }
}

fn load_thumbnail(path: &std::path::Path, max_px: u32) -> Option<(u32, u32, Vec<u8>)> {
    if !path.exists() {
        return None;
    }
    #[cfg(windows)]
    if std::env::var_os("MDM_SYSTEM_ICONS").is_none_or(|v| v != "0")
        && let Some(got) = crate::win32::file_thumbnail(path, max_px)
    {
        return Some(got);
    }
    image_thumbnail(path, max_px)
}

/// Portable image thumbnail decoder for formats supported by the `image` crate (up to 32 MiB).
pub(crate) fn image_thumbnail(path: &std::path::Path, max_px: u32) -> Option<(u32, u32, Vec<u8>)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > 32 * 1024 * 1024 {
        return None;
    }
    let img = image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let (nw, nh) = fit_rect_size(w, h, max_px);
    let out = if (nw, nh) == (w, h) {
        rgba.into_raw()
    } else {
        image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle).into_raw()
    };
    Some((nw, nh, out))
}

fn to_thumbnail(
    ctx: &egui::Context,
    path: &std::path::Path,
    (w, h, mut rgba): (u32, u32, Vec<u8>),
) -> Option<Thumbnail> {
    if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    premultiply(&mut rgba);
    let large_img = egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba);
    let (sw, sh) = fit_rect_size(w, h, 64);
    let small_img = if (sw, sh) == (w, h) {
        large_img.clone()
    } else {
        let src = image::RgbaImage::from_raw(w, h, rgba)?;
        let small = image::imageops::resize(&src, sw, sh, image::imageops::FilterType::Triangle);
        egui::ColorImage::from_rgba_premultiplied([sw as usize, sh as usize], &small.into_raw())
    };
    let key = path.display();
    let small = ctx.load_texture(
        format!("file-thumb-small-{key}"),
        small_img,
        egui::TextureOptions::LINEAR,
    );
    let large = ctx.load_texture(
        format!("file-thumb-large-{key}"),
        large_img,
        egui::TextureOptions::LINEAR,
    );
    Some(Thumbnail {
        small,
        large,
        size: [w, h],
    })
}

// ---- drawing -----------------------------------------------------------------------------------

/// Draw the thumbnail for `path` in `rc` (on screen) when available, else its file-type icon.
pub fn paint_file(ui: &Ui, painter: &Painter, path: &std::path::Path, rc: Rect) {
    let ctx = ui.ctx();
    if let Some(thumb) = thumbnail(ctx, path) {
        let ppp = ctx.pixels_per_point();
        let rc = rc.round_to_pixels(ppp);
        let [tw, th] = thumb.size;
        let scale = (rc.width() / tw.max(1) as f32).min(rc.height() / th.max(1) as f32);
        let draw_size = egui::vec2((tw as f32 * scale).max(1.0), (th as f32 * scale).max(1.0));
        let dst = Rect::from_center_size(rc.center(), draw_size).round_to_pixels(ppp);
        let tex = if rc.width() * ppp > 48.0 {
            &thumb.large
        } else {
            &thumb.small
        };
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        painter.image(tex.id(), dst, uv, Color32::WHITE);
        painter.rect_stroke(
            dst,
            0.0,
            egui::Stroke::new(1.0, Color32::from_black_alpha(40)),
            egui::StrokeKind::Inside,
        );
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    paint(ui, painter, Kind::File(&name), rc);
}

/// Show a floating thumbnail preview popup near the pointer when `path` has a loaded thumbnail.
pub fn show_thumbnail_preview(ctx: &egui::Context, path: &std::path::Path, file_name: &str) {
    let Some(thumb) = thumbnail(ctx, path) else {
        return;
    };
    let Some(pos) = ctx.pointer_hover_pos() else {
        return;
    };
    let [tw, th] = thumb.size;
    if tw == 0 || th == 0 {
        return;
    }
    let (fw, fh) = (tw as f32, th as f32);
    let max_dim = fw.max(fh);
    let scale = max_dim.clamp(48.0, 160.0) / max_dim;
    let img_w = (fw * scale).round().max(1.0);
    let img_h = (fh * scale).round().max(1.0);
    let pad = 8.0;
    let card_w = img_w.max(120.0) + pad * 2.0;
    let card_h = pad + img_h + 6.0 + 30.0 + pad;
    let screen = ctx.content_rect();
    let x = (pos.x + 14.0)
        .min(screen.right() - card_w - 6.0)
        .max(screen.left() + 4.0);
    let y = (pos.y + 14.0)
        .min(screen.bottom() - card_h - 6.0)
        .max(screen.top() + 4.0);
    egui::Area::new(egui::Id::unique("mdm-thumb-preview"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(pos2(x.round(), y.round()))
        .show(ctx, |ui| {
            let (rect, _) =
                ui.allocate_exact_size(egui::vec2(card_w, card_h), egui::Sense::hover());
            let _surface = crate::winui::Surface::enter(rect.min.x, rect.max.x);
            let p = ui.painter();
            crate::chrome::popup_surface(p, rect);
            let img_rect = Rect::from_center_size(
                pos2(rect.center().x, rect.top() + pad + img_h / 2.0),
                egui::vec2(img_w, img_h),
            );
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            p.image(thumb.large.id(), img_rect, uv, Color32::WHITE);
            p.rect_stroke(
                img_rect,
                0.0,
                egui::Stroke::new(1.0, Color32::from_black_alpha(50)),
                egui::StrokeKind::Inside,
            );
            let font = crate::winui::tahoma();
            let text_room = card_w - pad * 2.0;
            let name =
                crate::winui::ellipsize(p, &font, &crate::i18n::isolate(file_name), text_room);
            crate::winui::text_font(
                p,
                font.clone(),
                rect.left() + pad,
                rect.top() + pad + img_h + 16.0,
                &name,
                crate::winui::TEXT(),
            );
            let dims = crate::i18n::ltr(&format!("{tw} × {th}"));
            crate::winui::text_font(
                p,
                font,
                rect.left() + pad,
                rect.top() + pad + img_h + 30.0,
                &dims,
                crate::winui::MUTED(),
            );
        });
}

// ---- drawing -----------------------------------------------------------------------------------

/// Draw `kind`'s icon in `rc` (on screen) through `painter`, so it keeps to the list's clip. At
/// the screen's pixel size, on whole pixels, so it stays sharp at every scale.
pub fn paint(ui: &Ui, painter: &Painter, kind: Kind, rc: Rect) {
    let ctx = ui.ctx();
    let ppp = ctx.pixels_per_point();
    let rc = rc.round_to_pixels(ppp);
    let px = (rc.width() * ppp).round().clamp(1.0, 512.0) as u32;
    let what = What::of(kind);
    // A theme with an icon set draws its own for the type's kind (#45); the system's otherwise.
    if crate::theme::current().icon_set.is_some() {
        let ext = match &what {
            What::File(Some(e)) => Some(e.as_str()),
            _ => None,
        };
        let need = crate::themeicons::file_need(tango_name(&what), ext);
        if crate::themeicons::paint(ui, painter, need, rc, true) {
            return;
        }
    }
    match shown(ctx, &what, px) {
        Some(Shown::Texture(t)) => {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            painter.image(t.id(), rc, uv, Color32::WHITE);
        }
        Some(Shown::Image(src)) => paint_source(ui, painter, src, rc),
        _ => paint_source(ui, painter, fallback(&what, px), rc),
    }
}

fn paint_source(ui: &Ui, painter: &Painter, src: ImageSource<'static>, rc: Rect) {
    let img = egui::Image::new(src)
        .fit_to_exact_size(rc.size())
        .texture_options(egui::TextureOptions::LINEAR);
    if let Ok(egui::load::TexturePoll::Ready { texture }) = img.load_for_size(ui.ctx(), rc.size()) {
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        painter.image(texture.id, rc, uv, Color32::WHITE);
    }
}

/// The Tango icon `name` (not a file's: a place, such as the Server row's computer and local
/// network, #140) in `rc` on screen, at the screen's pixel size, so it stays sharp at every scale.
pub fn paint_tango(ui: &Ui, painter: &Painter, name: &str, rc: Rect) {
    let ppp = ui.ctx().pixels_per_point();
    let rc = rc.round_to_pixels(ppp);
    let px = (rc.width() * ppp).round().clamp(1.0, 512.0) as u32;
    paint_source(ui, painter, tango_source(name, px), rc);
}

/// [`paint`] at English point `min` (a list's layout), `side` points square: placed where that
/// is on screen, so in a right-to-left window the icon is at the row's start there too.
pub fn paint_at(ui: &Ui, painter: &Painter, kind: Kind, min: egui::Pos2, side: f32) {
    let rc = crate::winui::rect(Rect::from_min_size(min, egui::vec2(side, side)));
    paint(ui, painter, kind, rc);
}

/// How far a name moves to make room for its 16-point icon (the icon and a gap).
pub const INDENT: f32 = 20.0;

// ---- Tango, where the system has nothing -------------------------------------------------------

/// One Tango icon: its hand-drawn 16 and 32 pixel PNGs and its scalable SVG.
struct Tango {
    name: &'static str,
    png16: &'static [u8],
    png32: &'static [u8],
    svg: &'static [u8],
}

macro_rules! tango {
    ($($n:literal),* $(,)?) => {
        &[$(Tango {
            name: $n,
            png16: include_bytes!(concat!("../assets/icons/tango/16/", $n, ".png")),
            png32: include_bytes!(concat!("../assets/icons/tango/32/", $n, ".png")),
            svg: include_bytes!(concat!("../assets/icons/tango/scalable/", $n, ".svg")),
        }),*]
    };
}

static TANGO: &[Tango] = tango!(
    "application-certificate",
    "application-x-executable",
    "audio-x-generic",
    "computer",
    "folder",
    "folder-open",
    "font-x-generic",
    "image-x-generic",
    "media-optical",
    "network-workgroup",
    "package-x-generic",
    "text-html",
    "text-x-generic",
    "text-x-script",
    "video-x-generic",
    "x-office-address-book",
    "x-office-calendar",
    "x-office-document",
    "x-office-document-template",
    "x-office-drawing",
    "x-office-drawing-template",
    "x-office-presentation",
    "x-office-presentation-template",
    "x-office-spreadsheet",
    "x-office-spreadsheet-template",
);

/// Tango's icon for `what`: by the type's kind, the plain page when it is none of them.
fn tango_name(what: &What) -> &'static str {
    let ext = match what {
        What::Folder => return "folder",
        What::FolderOpen => return "folder-open",
        What::File(None) => return "text-x-generic",
        What::File(Some(e)) => e.as_str(),
    };
    match ext {
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "mpg" | "mpeg" | "ts"
        | "m2ts" | "3gp" | "ogv" | "vob" | "rm" | "rmvb" | "asf" => "video-x-generic",
        "mp3" | "flac" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "wma" | "ape"
        | "alac" | "aif" | "aiff" | "mid" | "midi" => "audio-x-generic",
        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "tif" | "tiff" | "ico" | "heic"
        | "heif" | "avif" | "psd" | "raw" | "cr2" | "nef" | "dng" => "image-x-generic",
        "svg" | "eps" | "ai" | "odg" | "vsd" | "vsdx" => "x-office-drawing",
        "otg" => "x-office-drawing-template",
        "zip" | "rar" | "7z" | "gz" | "tgz" | "bz2" | "tbz2" | "xz" | "txz" | "zst" | "tar"
        | "cab" | "lz" | "lzma" | "z" | "arj" | "deb" | "rpm" | "pkg" | "apk" | "jar" | "whl"
        | "nupkg" | "snap" | "flatpak" => "package-x-generic",
        "exe" | "msi" | "msix" | "appx" | "com" | "scr" | "appimage" | "run" | "bin" | "app"
        | "dll" | "so" | "dylib" | "sys" => "application-x-executable",
        "iso" | "img" | "dmg" | "vhd" | "vhdx" | "vmdk" | "nrg" | "mdf" | "cue" | "toast" => {
            "media-optical"
        }
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "wpd" | "pages" | "epub" | "mobi" | "azw"
        | "azw3" | "djvu" | "xps" | "fb2" | "cbz" | "cbr" => "x-office-document",
        "dot" | "dotx" | "ott" => "x-office-document-template",
        "xls" | "xlsx" | "xlsm" | "ods" | "csv" | "tsv" | "numbers" => "x-office-spreadsheet",
        "xlt" | "xltx" | "ots" => "x-office-spreadsheet-template",
        "ppt" | "pptx" | "odp" | "key" | "pps" | "ppsx" => "x-office-presentation",
        "pot" | "potx" | "otp" => "x-office-presentation-template",
        "html" | "htm" | "xhtml" | "mht" | "mhtml" | "xml" => "text-html",
        "sh" | "bash" | "py" | "js" | "mjs" | "ps1" | "bat" | "cmd" | "pl" | "rb" | "php"
        | "lua" | "vbs" => "text-x-script",
        "ttf" | "otf" | "woff" | "woff2" | "fon" | "pfb" => "font-x-generic",
        "crt" | "cer" | "pem" | "der" | "p12" | "pfx" | "p7b" | "sig" | "asc" | "gpg" => {
            "application-certificate"
        }
        "ics" | "ical" | "vcs" => "x-office-calendar",
        "vcf" | "vcard" => "x-office-address-book",
        _ => "text-x-generic",
    }
}

/// Tango's image for `what` at `px` pixels: the hand-drawn PNG at 16 and 32, the SVG otherwise.
fn fallback(what: &What, px: u32) -> ImageSource<'static> {
    tango_source(tango_name(what), px)
}

/// The Tango icon `name` at `px` pixels (the first one when there is none of that name).
fn tango_source(name: &str, px: u32) -> ImageSource<'static> {
    let icon = TANGO.iter().find(|t| t.name == name).unwrap_or(&TANGO[0]);
    let name = icon.name;
    let (bytes, file) = match px {
        16 => (icon.png16, format!("16/{name}.png")),
        32 => (icon.png32, format!("32/{name}.png")),
        _ => (icon.svg, format!("scalable/{name}.svg")),
    };
    ImageSource::Bytes {
        uri: format!("bytes://mdm/file-icon/tango/{file}").into(),
        bytes: egui::load::Bytes::Static(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tango_icon_is_there_in_its_three_forms() {
        for t in TANGO {
            assert!(t.png16.starts_with(b"\x89PNG"), "{}", t.name);
            assert!(t.png32.starts_with(b"\x89PNG"), "{}", t.name);
            assert!(
                t.svg.starts_with(b"<?xml") || t.svg.starts_with(b"<svg"),
                "{}",
                t.name
            );
            for (png, side) in [(t.png16, 16), (t.png32, 32)] {
                let img = image::load_from_memory(png).unwrap();
                assert_eq!((img.width(), img.height()), (side, side), "{}", t.name);
            }
        }
        // Every name the table gives is one of them.
        for ext in [
            "mp4", "mp3", "png", "svg", "otg", "zip", "exe", "iso", "pdf", "dotx", "xlsx", "xltx",
            "pptx", "potx", "html", "py", "ttf", "pem", "ics", "vcf", "qqqq",
        ] {
            let name = tango_name(&What::File(Some(ext.into())));
            assert!(TANGO.iter().any(|t| t.name == name), "{ext} → {name}");
        }
        assert!(TANGO.windows(2).all(|w| w[0].name < w[1].name));
    }

    #[test]
    fn a_download_managers_files_get_their_kind() {
        let of = |name: &str| tango_name(&What::of(Kind::File(name)));
        for (name, icon) in [
            ("movie.mp4", "video-x-generic"),
            ("stream.TS", "video-x-generic"),
            ("song.flac", "audio-x-generic"),
            ("setup.exe", "application-x-executable"),
            ("setup.msi", "application-x-executable"),
            ("app.dmg", "media-optical"),
            ("ubuntu.iso", "media-optical"),
            ("tool.deb", "package-x-generic"),
            ("photos.7z", "package-x-generic"),
            ("backup.tar.gz", "package-x-generic"),
            ("report.pdf", "x-office-document"),
            ("C:\\dl\\Letter.DOCX", "x-office-document"),
            ("sheet.xlsx", "x-office-spreadsheet"),
            ("talk.pptx", "x-office-presentation"),
            ("page.html", "text-html"),
        ] {
            assert_eq!(of(name), icon, "{name}");
        }
        // No extension, a dot file, an unknown type: the plain page; a folder: Tango's folder.
        assert_eq!(What::of(Kind::File("README")), What::File(None));
        assert_eq!(What::of(Kind::File(".hidden")), What::File(None));
        assert_eq!(of("README"), "text-x-generic");
        assert_eq!(of("data.xyz"), "text-x-generic");
        assert_eq!(tango_name(&What::Folder), "folder");
    }

    #[test]
    fn the_fallback_is_the_hand_drawn_png_at_16_and_32_else_the_svg() {
        let uri = |s: ImageSource| match s {
            ImageSource::Bytes { uri, .. } => uri.to_string(),
            _ => String::new(),
        };
        let pdf = What::File(Some("pdf".into()));
        assert!(uri(fallback(&pdf, 16)).ends_with("/16/x-office-document.png"));
        assert!(uri(fallback(&pdf, 32)).ends_with("/32/x-office-document.png"));
        assert!(uri(fallback(&pdf, 24)).ends_with("/scalable/x-office-document.svg"));
        assert!(uri(fallback(&What::Folder, 48)).ends_with("/scalable/folder.svg"));
    }

    #[test]
    fn fitting_premultiplies_and_averages() {
        // 2 × 2: opaque red, transparent (white), opaque red, transparent → 1 × 1.
        let src = [
            255, 0, 0, 255, 255, 255, 255, 0, //
            255, 0, 0, 255, 255, 255, 255, 0,
        ];
        let same = fit(&src, 2, false, 2);
        assert_eq!(
            &same[4..8],
            &[0, 0, 0, 0],
            "transparent white premultiplies to nothing"
        );
        let one = fit(&src, 2, false, 1);
        assert_eq!(one.len(), 4);
        // Half covered, red only: no white bleeding in from the transparent pixels.
        assert!((120..=135).contains(&one[3]), "{one:?}");
        assert!(
            (120..=135).contains(&one[0]) && one[1] == 0 && one[2] == 0,
            "{one:?}"
        );
        // Already premultiplied: kept as is at the same size.
        assert_eq!(fit(&[10, 20, 30, 40], 1, true, 1), vec![10, 20, 30, 40]);
    }

    #[test]
    fn a_lookup_is_asked_once_and_answers_later() {
        let ctx = egui::Context::default();
        let what = What::File(Some("mdm-test-type".into()));
        // First ask: nothing yet (Tango's is drawn), the lookup queued.
        assert!(shown(&ctx, &what, 16).is_none());
        assert!(cache().icons.contains_key(&(what.clone(), 16)));
        // The answer arrives: drawn from then on, and at another size while that one is asked.
        let rgba = vec![200; 16 * 16 * 4];
        let tex = to_shown(
            &ctx,
            &what,
            16,
            Some(Native::Rgba {
                side: 16,
                rgba,
                premultiplied: true,
            }),
        );
        cache().icons.insert((what.clone(), 16), Some(tex));
        assert!(matches!(shown(&ctx, &what, 16), Some(Shown::Texture(_))));
        assert!(matches!(shown(&ctx, &what, 24), Some(Shown::Texture(_))));
        // A system with nothing for a type: Tango's, for good.
        let none = What::File(Some("mdm-test-none".into()));
        cache()
            .icons
            .insert((none.clone(), 16), Some(to_shown(&ctx, &none, 16, None)));
        assert!(shown(&ctx, &none, 16).is_none());
    }

    /// This system's own icons, for real (Windows' shell here; macOS Finder and the Linux
    /// desktop's theme on their CI runners, where one is installed). Every type at 16 and 32
    /// pixels, the sizes at 100 %; larger ones as the system has them (a CI runner's session,
    /// without a desktop, may lack the larger lists). What is missing is named.
    #[test]
    fn the_system_gives_its_icons_at_the_size_asked() {
        let mut missing = Vec::new();
        let mut small = Vec::new();
        for what in [
            What::File(Some("pdf".into())),
            What::File(Some("zip".into())),
            What::File(Some("txt".into())),
            What::File(Some("xyz".into())),
            What::File(None),
            What::Folder,
            What::FolderOpen,
        ] {
            for px in [16, 24, 32, 48, 64] {
                match system(&what, px) {
                    Some(Native::Rgba { side, rgba, .. }) => {
                        assert_eq!(rgba.len(), (side * side * 4) as usize, "{what:?} {px}");
                        assert!(
                            rgba.as_chunks::<4>().0.iter().any(|c| c[3] > 0),
                            "{what:?} {px}: blank"
                        );
                        // The list that fits: as big as asked when the system has one.
                        if side < px && px <= 48 {
                            small.push(format!("{what:?} at {px}: {side}"));
                        }
                    }
                    Some(Native::File(path)) => assert!(path.is_file(), "{path:?}"),
                    None => missing.push((what.clone(), px)),
                }
            }
        }
        if cfg!(any(windows, target_os = "macos")) {
            let needed: Vec<_> = missing
                .iter()
                .filter(|(_, px)| *px == 16 || *px == 32)
                .collect();
            assert!(
                needed.is_empty(),
                "no icon: {needed:?} (all missing: {missing:?})"
            );
        }
        if !missing.is_empty() || !small.is_empty() {
            eprintln!("missing: {missing:?}; smaller than asked: {small:?}");
        }
    }

    #[test]
    fn thumbnail_loads_png_and_caches_small_and_large_textures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.png");
        let mut img = image::RgbaImage::new(120, 80);
        for p in img.pixels_mut() {
            *p = image::Rgba([40, 120, 200, 255]);
        }
        img.save(&path).unwrap();

        // Portable decoder produces scaled dimensions and RGBA pixels.
        let (w, h, rgba) = image_thumbnail(&path, 60).unwrap();
        assert_eq!((w, h), (60, 40));
        assert_eq!(rgba.len(), 60 * 40 * 4);

        // Background cache loads and caches both small and large textures.
        let ctx = egui::Context::default();
        assert!(thumbnail(&ctx, &path).is_none(), "queued on first ask");
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let thumb = loop {
            if let Some(t) = thumbnail(&ctx, &path) {
                break t;
            }
            assert!(std::time::Instant::now() < until, "thumbnail never arrived");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(thumb.small.size(), [64, 43]);
        assert!(thumb.large.size()[0] >= 120 && thumb.large.size()[1] >= 80);

        invalidate_thumbnail(&path);
        assert!(thumbnail(&ctx, &path).is_none(), "cleared by invalidate");
    }
}
