//! The keyboard layouts the system is set to.
//!
//! Both OSes keep the desktop keyboard in `/etc/X11/xorg.conf.d/` (the
//! installer writes `00-keyboard.conf`; `localectl` writes the same file):
//! `Option "XkbLayout" "us,de"`, `"XkbVariant"`, `"XkbModel"`,
//! `"XkbOptions"`. Without it the layout is `us`. The names shown come from
//! xkeyboard-config's rules file.

use std::path::Path;

use reclogin::protocol::Layout;

pub const XORG_CONF_DIR: &str = "/etc/X11/xorg.conf.d";
pub const RULES_XML: &str = "/usr/share/X11/xkb/rules/evdev.xml";

#[derive(Default)]
struct Setting {
    layout: String,
    variant: String,
    model: String,
    options: String,
}

pub fn configured(conf_dir: &Path, rules_xml: &Path) -> Vec<Layout> {
    let setting = read_setting(conf_dir);
    let rules = std::fs::read_to_string(rules_xml).unwrap_or_default();
    let layouts: Vec<&str> = setting.layout.split(',').map(str::trim).collect();
    let variants: Vec<&str> = setting.variant.split(',').map(str::trim).collect();
    let mut out = Vec::new();
    for (i, layout) in layouts.iter().enumerate() {
        if layout.is_empty() {
            continue;
        }
        let variant = variants.get(i).copied().unwrap_or("");
        let name = if variant.is_empty() {
            layout.to_string()
        } else {
            format!("{layout}({variant})")
        };
        let label = describe(&rules, layout, variant).unwrap_or_else(|| name.clone());
        out.push(Layout {
            name,
            label,
            xkb_layout: layout.to_string(),
            xkb_variant: variant.to_string(),
            xkb_model: setting.model.clone(),
            xkb_options: setting.options.clone(),
        });
    }
    if out.is_empty() {
        out.push(Layout {
            name: "us".into(),
            label: describe(&rules, "us", "").unwrap_or_else(|| "us".into()),
            xkb_layout: "us".into(),
            xkb_variant: String::new(),
            xkb_model: setting.model,
            xkb_options: setting.options,
        });
    }
    out
}

fn read_setting(conf_dir: &Path) -> Setting {
    let mut setting = Setting::default();
    let Ok(entries) = std::fs::read_dir(conf_dir) else {
        return setting;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.extension().and_then(|e| e.to_str()) != Some("conf") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some(rest) = line.strip_prefix("Option") else {
                continue;
            };
            let quoted: Vec<&str> = rest.split('"').skip(1).step_by(2).collect();
            let [key, value] = quoted[..] else { continue };
            let slot = match key.to_ascii_lowercase().as_str() {
                "xkblayout" => &mut setting.layout,
                "xkbvariant" => &mut setting.variant,
                "xkbmodel" => &mut setting.model,
                "xkboptions" => &mut setting.options,
                _ => continue,
            };
            *slot = value.to_string();
        }
    }
    setting
}

/// The `<description>` of a layout (or of its variant) in the rules file.
fn describe(rules: &str, layout: &str, variant: &str) -> Option<String> {
    let layout_tag = format!("<name>{layout}</name>");
    for block in rules.split("<layout>").skip(1) {
        let block = block.split("</layout>").next().unwrap_or(block);
        let (head, variants) = block.split_once("<variantList>").unwrap_or((block, ""));
        if !first_name_is(head, &layout_tag) {
            continue;
        }
        if variant.is_empty() {
            return description(head);
        }
        let variant_tag = format!("<name>{variant}</name>");
        for v in variants.split("<variant>").skip(1) {
            if first_name_is(v, &variant_tag) {
                return description(v);
            }
        }
        return None;
    }
    None
}

fn first_name_is(block: &str, tag: &str) -> bool {
    block
        .find("<name>")
        .is_some_and(|at| block[at..].starts_with(tag))
}

fn description(block: &str) -> Option<String> {
    let start = block.find("<description>")? + "<description>".len();
    let end = block[start..].find("</description>")? + start;
    Some(unescape(&block[start..end]))
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
