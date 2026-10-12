//! Embeds the vendored Lucide SVGs and builds their lookup tables.
//!
//! GPUI's `Svg::path` is an asset key, not SVG drawing instructions. Preserve
//! the complete documents so its renderer can honor strokes, circles, and
//! rounded corners. Flattened paths remain available for geometry checks and
//! callers of `Icon::path`, but are never passed to `Svg::path`.
//!
//! Icons are Lucide (ISC), vendored under `assets/icons/`. Names use the same
//! SF Symbols spelling as the Swift app so call sites port unchanged.

use std::fmt::Write as _;
use std::{collections::BTreeMap, env, fs, path::Path};

/// The viewBox every Lucide icon is authored against.
const VIEW_BOX: f64 = 24.0;

/// Round to a fixed number of decimals and drop trailing zeros, so generated
/// path data stays small and deterministic.
fn n(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".to_string()
    } else {
        s.to_string()
    }
}

/// Read `name="value"` for one attribute out of a tag's attribute text.
fn attr(tag: &str, name: &str) -> Option<f64> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    rest[..end].parse::<f64>().ok()
}

/// `cx`,`cy`,`r` to a full-circle arc path.
fn circle(tag: &str) -> Option<String> {
    let (cx, cy, r) = (attr(tag, "cx")?, attr(tag, "cy")?, attr(tag, "r")?);
    if r <= 0.0 {
        return None;
    }
    let d = 2.0 * r;
    Some(format!(
        "M{nx},{ny}a{nr},{nr} 0 1,0 {d},0a{nr},{nr} 0 1,0 -{d},0",
        nx = n(cx - r),
        ny = n(cy),
        nr = n(r),
        d = n(d),
    ))
}

/// `x`,`y`,`width`,`height` (+ optional `rx`) to a rectangle path.
fn rect(tag: &str) -> Option<String> {
    let (x, y, w, h) = (
        attr(tag, "x").unwrap_or(0.0),
        attr(tag, "y").unwrap_or(0.0),
        attr(tag, "width")?,
        attr(tag, "height")?,
    );
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    // Lucide uses a small corner radius purely for optical softness. Keep it:
    // squaring the corners makes these icons read as different glyphs.
    let rx = attr(tag, "rx").unwrap_or(0.0).min(w / 2.0).min(h / 2.0);
    if rx <= 0.0 {
        return Some(format!(
            "M{nx},{ny}h{nw}v{nh}h-{nw}z",
            nx = n(x),
            ny = n(y),
            nw = n(w),
            nh = n(h)
        ));
    }
    let straight = n(w - 2.0 * rx);
    let upright = n(h - 2.0 * rx);
    let r = n(rx);
    Some(format!(
        "M{x0},{y0}h{s}a{r},{r} 0 0,1 {r},{r}v{u}a{r},{r} 0 0,1 {r},{r}h-{s}a{r},{r} 0 0,1 -{r},{r}v-{u}a{r},{r} 0 0,1 -{r},-{r}z",
        x0 = n(x + rx),
        y0 = n(y),
        s = straight,
        u = upright,
        r = r,
    ))
}

/// `x1`,`y1`,`x2`,`y2` to a line path.
fn line(tag: &str) -> Option<String> {
    Some(format!(
        "M{nx1},{ny1}L{nx2},{ny2}",
        nx1 = n(attr(tag, "x1")?),
        ny1 = n(attr(tag, "y1")?),
        nx2 = n(attr(tag, "x2")?),
        ny2 = n(attr(tag, "y2")?),
    ))
}

/// `points` to a polyline path.
fn polyline(tag: &str) -> Option<String> {
    let needle = "points=\"";
    let start = tag.find(needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    let mut coords: Vec<String> = Vec::new();
    for pair in rest[..end].split_whitespace().collect::<Vec<_>>().chunks(2) {
        let [x, y] = pair else { continue };
        let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else {
            continue;
        };
        coords.push(format!("{},{}", n(x), n(y)));
    }
    if coords.len() < 2 {
        return None;
    }
    Some(format!("M{}", coords.join("L")))
}

/// Flatten one SVG document into a single `d` string.
fn flatten(svg: &str) -> Option<String> {
    let mut out = String::with_capacity(svg.len());

    let mut rest = svg;
    while let Some(open) = rest.find('<') {
        let after = &rest[open + 1..];
        let close = after.find('>')?;
        let tag = &after[..close];
        // Include the closing `>` in the attribute slice so `attr` can see
        // attributes declared right up to the end of the tag.
        let tag_with_end = &after[..=close];
        rest = &after[close + 1..];

        if let Some(d) = tag
            .strip_prefix("path")
            .and_then(|_| extract_d(tag_with_end))
        {
            out.push_str(&d);
            out.push(' ');
            continue;
        }

        let converted = if tag.starts_with("circle") {
            circle(tag_with_end)
        } else if tag.starts_with("rect") {
            rect(tag_with_end)
        } else if tag.starts_with("line") {
            line(tag_with_end)
        } else if tag.starts_with("polyline") {
            polyline(tag_with_end)
        } else if tag.starts_with("polygon") {
            // Close the polyline.
            polyline(tag_with_end).map(|d| format!("{d}z"))
        } else {
            None
        };
        if let Some(d) = converted {
            out.push_str(&d);
            out.push(' ');
        }
    }

    let trimmed = out.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_d(tag: &str) -> Option<String> {
    let needle = "d=\"";
    let start = tag.find(needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let icon_dir = Path::new(&manifest).join("assets/icons");
    println!("cargo:rerun-if-changed={}", icon_dir.display());

    let mut icons: BTreeMap<String, String> = BTreeMap::new();
    for entry in fs::read_dir(&icon_dir).expect("read assets/icons") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("svg") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("utf-8 icon name")
            .to_string();

        let svg = fs::read_to_string(&path).expect("read icon");
        match flatten(&svg) {
            Some(d) => {
                icons.insert(stem, d);
            }
            None => panic!("could not flatten icon {}", path.display()),
        }
    }
    assert!(
        !icons.is_empty(),
        "no icons found in {}",
        icon_dir.display()
    );

    let mut src = String::with_capacity(icons.len() * 400);
    src.push_str("// @generated by build.rs from assets/icons (Lucide, ISC). Do not edit.\n");
    src.push_str(&format!(
        "/// Authored viewBox for every icon in this set: {VIEW_BOX}x{VIEW_BOX}.\n\
         pub const VIEW_BOX: f32 = {VIEW_BOX}.0;\n\n\
         /// Name to flattened path data.\n\
         pub(crate) static ICON_PATHS: &[(&str, &str)] = &[\n"
    ));
    for (name, d) in &icons {
        // Escape the quotes Lucide does not emit but a hand-edited file might.
        let escaped = d.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = writeln!(src, "    ({name:?}, \"{escaped}\"),");
    }
    src.push_str("];\n");

    src.push_str("\n/// Asset key to embedded original SVG document.\n");
    src.push_str("pub(crate) static ICON_ASSETS: &[(&str, &[u8])] = &[\n");
    for name in icons.keys() {
        let _ = writeln!(
            src,
            "    (\"icons/{name}.svg\", include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/assets/icons/{name}.svg\"))),"
        );
    }
    src.push_str("];\n");

    let out = Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("icon_paths.rs");
    fs::write(out, src).expect("write generated icon paths");
}
