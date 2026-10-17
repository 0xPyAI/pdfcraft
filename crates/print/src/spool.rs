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
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        printer_options_via_within(printer, std::path::Path::new("/etc/cups/ppd"), std::process::Command::new("lpoptions"), LPOPTIONS_LIMIT)
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        craft_print::printer_options(printer)
    }
}

/// How long `lpoptions` may take: a CUPS that has stopped answering must not hold whoever asked
/// (#1818). After this the PPD's own defaults stand.
#[cfg(all(unix, not(target_arch = "wasm32")))]
const LPOPTIONS_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The most of a command's output that is kept (`lpoptions -l` is a few kilobytes).
#[cfg(any(test, all(unix, not(target_arch = "wasm32"))))]
const OUTPUT_CAP: u64 = 4 << 20;

/// Run `command` and collect its standard output, giving it `limit`: a command still running
/// then is killed and reaped, and the error is `TimedOut`. Standard input and error are not
/// connected. The exit status is not checked, as `Command::output` leaves it to the caller.
/// Only CUPS runs a command today; the tests cover every desktop platform.
#[cfg(any(test, all(unix, not(target_arch = "wasm32"))))]
pub(crate) fn output_within(command: &mut std::process::Command, limit: std::time::Duration) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    use std::process::Stdio;
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
    let stop = |child: &mut std::process::Child| {
        let _ = child.kill();
        let _ = child.wait();
    };
    let Some(mut stdout) = child.stdout.take() else {
        stop(&mut child);
        return Err(std::io::Error::other("the command's output could not be read"));
    };
    // Read on a thread of its own, so a full pipe never holds the command while the deadline is
    // kept here. Past the cap the rest is drained and dropped, so the command still finishes.
    // The output comes back over a channel rather than through `join`: a process the command
    // started can keep the pipe open after the command itself has exited or been killed, and
    // the wait for the output must end at the deadline too. Such a reader is left to finish on
    // its own when that process closes the pipe.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::Builder::new().name("command output".into()).spawn(move || {
        let mut out = Vec::new();
        let read = (&mut stdout).take(OUTPUT_CAP).read_to_end(&mut out);
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        let _ = tx.send(read.map(|_| out));
    });
    if let Err(e) = reader {
        stop(&mut child);
        return Err(e);
    }
    let timed_out = || std::io::Error::new(std::io::ErrorKind::TimedOut, format!("the command did not answer within {limit:?}"));
    let deadline = std::time::Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                // A moment's grace, so output written just before an exit at the deadline counts.
                let left = deadline.saturating_duration_since(std::time::Instant::now()).max(std::time::Duration::from_millis(100));
                return match rx.recv_timeout(left) {
                    Ok(read) => read,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(timed_out()),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(std::io::Error::other("the command's output was lost")),
                };
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                stop(&mut child);
                return Err(timed_out());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                stop(&mut child);
                return Err(e);
            }
        }
    }
}

/// Read a destination's options with the PPD directory and command supplied, so tests need
/// neither a system queue nor a physical printer. CUPS instances share their queue's PPD but
/// have their own defaults, queried with the complete `queue/instance` destination. `limit` is
/// how long the command may take before the PPD's defaults stand on their own.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) fn printer_options_via_within(
    printer: &str,
    ppd_dir: &std::path::Path,
    mut lpoptions: std::process::Command,
    limit: std::time::Duration,
) -> Vec<PrinterOption> {
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
    let mut options = ppd.map(|b| parse_ppd(&ppd_text(&b))).unwrap_or_default();
    let current =
        output_within(lpoptions.args(["-p", printer, "-l"]), limit).map(|out| parse_lpoptions(&String::from_utf8_lossy(&out))).unwrap_or_default();
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
