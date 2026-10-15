//! System print spooler supplied by craft-print. PDF composition and permission checks
//! stay in PdfCraft; these adapters retain the application's print-error surface.

pub use craft_print::{
    Duplex, HANDLED_OPTIONS, HAS_PRINTER_PREFERENCES, Job, Printer, PrinterOption, Scaling, lp_args, lpstat_command, parse_lpoptions, parse_lpstat,
    parse_lpstat_e, parse_ppd, parse_windows_printers, ppd_text, printers, printers_parsed,
};

/// The driver's job options for a printer (CUPS: its PPD, with the queue's current defaults);
/// empty when it has none or on other platforms. Reads files and runs `lpoptions`, so call it
/// when the user asks, not every frame.
pub fn printer_options(printer: &str) -> Vec<PrinterOption> {
    printer_options_in(printer, "en")
}

/// [`printer_options`] with the driver's own names in `lang` (a UI language code such as `bg`,
/// `pt-br` or `zh-hans`) where its PPD has them (`*bg.Translation`, `*bg.InputSlot Tray2/…`);
/// the driver's default names otherwise.
pub fn printer_options_in(printer: &str, lang: &str) -> Vec<PrinterOption> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        printer_options_via(printer, std::path::Path::new("/etc/cups/ppd"), std::process::Command::new("lpoptions"), lang)
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        let _ = lang;
        craft_print::printer_options(printer)
    }
}

/// Read a destination's options with the PPD directory and command supplied, so tests need
/// neither a system queue nor a physical printer. CUPS instances share their queue's PPD but
/// have their own defaults, queried with the complete `queue/instance` destination.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) fn printer_options_via(printer: &str, ppd_dir: &std::path::Path, mut lpoptions: std::process::Command, lang: &str) -> Vec<PrinterOption> {
    let (queue, instance) = printer.split_once('/').map_or((printer, None), |(q, i)| (q, Some(i)));
    // Retain the conservative name checks, applied to each component. Neither component is
    // a path: reject dot components, backslashes and any additional separator before I/O.
    let valid = |name: &str| {
        !name.is_empty()
            && name.len() <= 64
            && !matches!(name, "." | "..")
            && name.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'\\' | b':' | b'=' | b'"'))
    };
    if !valid(queue) || instance.is_some_and(|name| !valid(name)) {
        return Vec::new();
    }
    const PPD_CAP: u64 = 16 << 20;
    let ppd = std::fs::File::open(ppd_dir.join(format!("{queue}.ppd"))).ok().and_then(|f| {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        f.take(PPD_CAP).read_to_end(&mut bytes).ok().map(|_| bytes)
    });
    let mut options = ppd.as_deref().map(|b| parse_ppd(&ppd_text(b))).unwrap_or_default();
    if let Some(b) = &ppd {
        localize(&mut options, b, lang);
    }
    let current = lpoptions.args(["-p", printer, "-l"]).output().map(|o| parse_lpoptions(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default();
    for (key, value) in current {
        if let Some(o) = options.iter_mut().find(|o| o.key == key)
            && o.choices.iter().any(|(c, _)| *c == value)
        {
            o.default = value;
        }
    }
    options
}

pub fn submit(pdf: &[u8], job: &Job) -> Result<String, crate::PrintError> {
    craft_print::submit(pdf, job).map_err(|e| crate::PrintError::Spool(e.to_string()))
}

/// [`submit`], told `(sent, sheets)` as each sheet reaches the spooler (Windows; CUPS takes the
/// whole PDF at once and reports nothing).
pub fn submit_with_progress(pdf: &[u8], job: &Job, progress: &mut dyn FnMut(usize, usize)) -> Result<String, crate::PrintError> {
    craft_print::submit_with_progress(pdf, job, progress).map_err(|e| crate::PrintError::Spool(e.to_string()))
}

pub fn open_printer_preferences(printer: &str) -> Result<(), crate::PrintError> {
    craft_print::open_printer_preferences(printer).map_err(|e| crate::PrintError::Spool(e.to_string()))
}

/// The PPD language codes (`*cupsLanguages`, the `ll` of `*ll.Translation`) to try for a UI
/// language code, most specific first: `pt-br` → `pt_BR`, `pt`; `zh-hans` → `zh_CN`, `zh`.
// Read only by the CUPS path; the tests exercise it everywhere.
#[cfg_attr(not(all(unix, not(target_arch = "wasm32"))), allow(dead_code))]
fn ppd_languages(lang: &str) -> Vec<String> {
    let lang = lang.to_ascii_lowercase();
    let mut parts = lang.split(['-', '_']).filter(|p| !p.is_empty());
    let Some(primary) = parts.next() else { return Vec::new() };
    let region = match (primary, parts.next()) {
        ("zh", Some("hans")) => Some("CN".to_string()),
        ("zh", Some("hant")) => Some("TW".to_string()),
        (_, Some(r)) if r.len() == 2 => Some(r.to_ascii_uppercase()),
        _ => None,
    };
    region.map(|r| format!("{primary}_{r}")).into_iter().chain(std::iter::once(primary.to_string())).collect()
}

/// Give `options` the driver's own names in `lang` from the PPD's localization lines
/// (`*ll.Translation Key/Label: ""` for options and groups, `*ll.Key choice/Label: ""` for
/// choices). Each line is read as UTF-8 on its own: drivers write them so in files whose other
/// text is Latin-1. Names with no translation, or an empty one, stay as they are.
// Read only by the CUPS path; the tests exercise it everywhere.
#[cfg_attr(not(all(unix, not(target_arch = "wasm32"))), allow(dead_code))]
pub(crate) fn localize(options: &mut [PrinterOption], ppd: &[u8], lang: &str) {
    use std::collections::HashMap;
    let Some(code) = ppd_languages(lang).into_iter().find(|code| {
        let prefix = format!("*{code}.");
        ppd.split(|b| *b == b'\n').any(|l| l.starts_with(prefix.as_bytes()))
    }) else {
        return;
    };
    if code == "en" {
        return;
    }
    let prefix = format!("*{code}.");
    let mut names: HashMap<String, String> = HashMap::new();
    let mut choices: HashMap<(String, String), String> = HashMap::new();
    let mut groups: HashMap<String, String> = HashMap::new();
    for line in ppd.split(|b| *b == b'\n') {
        let Some(text) = std::str::from_utf8(line).ok() else { continue };
        let text = text.trim_end_matches('\r');
        if let Some(rest) = text.strip_prefix("*OpenGroup:") {
            // Group label → keyword, since options carry their group's label.
            let rest = rest.trim();
            if let Some((key, label)) = rest.split_once('/') {
                groups.insert(label.trim().to_string(), key.trim().to_string());
            }
            continue;
        }
        let Some(rest) = text.strip_prefix(prefix.as_str()) else { continue };
        let Some((head, _)) = rest.split_once(':') else { continue };
        let Some((main, spec)) = head.split_once(' ') else { continue };
        let Some((key, label)) = spec.split_once('/') else { continue };
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        if main == "Translation" {
            names.insert(key.trim().to_string(), label.to_string());
        } else {
            choices.insert((main.to_string(), key.trim().to_string()), label.to_string());
        }
    }
    for o in options.iter_mut() {
        if let Some(label) = names.get(&o.key) {
            o.label = label.clone();
        }
        if let Some(label) = groups.get(&o.group).and_then(|key| names.get(key)) {
            o.group = label.clone();
        }
        for (choice, label) in o.choices.iter_mut() {
            if let Some(t) = choices.get(&(o.key.clone(), choice.clone())) {
                *label = t.clone();
            }
        }
    }
}
