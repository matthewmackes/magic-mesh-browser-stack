//! CUPS printing for the Browser surface — the print-job value types (a queued
//! print request, a discovered printer, and the print settings the chrome exposes)
//! plus the free helpers that talk to the system `lpstat`/`lp` CLI: printer
//! discovery, a sanitized job title, and PDF submission. The `*_with_runner`
//! variants take an injected process runner so the tests exercise the argv
//! assembly without a live CUPS. `use super::*` pulls in the parent's
//! `run_process_with_timeout` / `process_error` / `ProcessOutput` /
//! `CUPS_PRINT_TIMEOUT` / `host_of`. A pure relocation from the `web` god-module.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CupsPrintRequest {
    pub(super) path: String,
    pub(super) title: String,
    pub(super) settings: CupsPrintSettings,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CupsPrinter {
    pub(super) name: String,
    pub(super) is_default: bool,
}

/// Page orientation for a CUPS job (the `orientation-requested` IPP attribute).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum PrintOrientation {
    #[default]
    Portrait,
    Landscape,
}

impl PrintOrientation {
    /// The `-o` value, or `None` for portrait (the CUPS default — no arg needed).
    pub(super) const fn lp_option(self) -> Option<&'static str> {
        match self {
            Self::Portrait => None,
            // IPP orientation-requested: 3 = portrait, 4 = landscape.
            Self::Landscape => Some("orientation-requested=4"),
        }
    }
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Portrait => "Portrait",
            Self::Landscape => "Landscape",
        }
    }
}

/// Paper size for a CUPS job (the `media` IPP attribute); `Default` defers to the
/// printer's own default so we never force a size the printer lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum PaperSize {
    #[default]
    Default,
    A4,
    Letter,
    Legal,
}

impl PaperSize {
    pub(super) const fn lp_option(self) -> Option<&'static str> {
        match self {
            Self::Default => None,
            Self::A4 => Some("media=A4"),
            Self::Letter => Some("media=Letter"),
            Self::Legal => Some("media=Legal"),
        }
    }
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Default => "Printer default",
            Self::A4 => "A4",
            Self::Letter => "Letter",
            Self::Legal => "Legal",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CupsPrintSettings {
    pub(super) destination: Option<String>,
    pub(super) copies: u16,
    pub(super) duplex: bool,
    pub(super) grayscale: bool,
    pub(super) orientation: PrintOrientation,
    pub(super) paper_size: PaperSize,
    /// Page range like `1-5,8` — empty means all pages.
    pub(super) page_ranges: String,
}

impl Default for CupsPrintSettings {
    fn default() -> Self {
        Self {
            destination: None,
            copies: 1,
            duplex: false,
            grayscale: false,
            orientation: PrintOrientation::Portrait,
            paper_size: PaperSize::Default,
            page_ranges: String::new(),
        }
    }
}

pub(super) fn cups_job_title(url: &str, title: &str, unix_ms: u64) -> String {
    let seed = {
        let title = title.trim();
        if title.is_empty() {
            host_of(url).unwrap_or_else(|| "Browser page".to_owned())
        } else {
            title.to_owned()
        }
    };
    let mut out = String::new();
    let mut last_space = false;
    for ch in seed.chars() {
        let next = if ch.is_ascii_graphic() {
            last_space = false;
            Some(ch)
        } else if ch.is_whitespace() && !last_space {
            last_space = true;
            Some(' ')
        } else {
            None
        };
        if let Some(ch) = next {
            out.push(ch);
        }
        if out.len() >= 80 {
            break;
        }
    }
    let out = out.trim();
    if out.is_empty() {
        format!("{} {unix_ms}", browser_product_label())
    } else {
        format!("{} - {out}", browser_product_label())
    }
}

pub(super) fn discover_cups_printers() -> Result<Vec<CupsPrinter>, String> {
    discover_cups_printers_with_runner(run_process_with_timeout)
}

pub(super) fn discover_cups_printers_with_runner(
    runner: impl Fn(&str, &[String], Duration) -> Result<ProcessOutput, String>,
) -> Result<Vec<CupsPrinter>, String> {
    let names = runner("lpstat", &["-e".to_owned()], CUPS_PRINT_TIMEOUT)?;
    if !names.success {
        return Err(process_error("lpstat -e", &names));
    }
    let default = runner("lpstat", &["-d".to_owned()], CUPS_PRINT_TIMEOUT).ok();
    let default_name = default
        .as_ref()
        .filter(|output| output.success)
        .and_then(|output| parse_cups_default_destination(&output.stdout));
    let mut printers = parse_cups_printer_names(&names.stdout)
        .into_iter()
        .map(|name| CupsPrinter {
            is_default: default_name.as_deref() == Some(name.as_str()),
            name,
        })
        .collect::<Vec<_>>();
    printers.sort_by(|a, b| {
        b.is_default
            .cmp(&a.is_default)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(printers)
}

fn parse_cups_printer_names(stdout: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    stdout
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter(|name| seen.insert((*name).to_owned()))
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_cups_default_destination(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| {
            line.rsplit_once(':')
                .map(|(_, name)| name.trim().to_owned())
        })
        .filter(|name| !name.is_empty())
}

/// Build the `lp` argv for a job from its settings — the destination, copies,
/// duplex/color, and the print-preview OPTIONS (orientation, paper size, page
/// range). Pure so the argv is unit-tested without touching CUPS or the filesystem.
fn cups_lp_args(path_arg: &str, title: &str, settings: &CupsPrintSettings) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(destination) = settings
        .destination
        .as_deref()
        .map(str::trim)
        .filter(|destination| !destination.is_empty())
    {
        args.push("-d".to_owned());
        args.push(destination.to_owned());
    }
    if settings.copies > 1 {
        args.push("-n".to_owned());
        args.push(settings.copies.min(99).to_string());
    }
    if settings.duplex {
        args.push("-o".to_owned());
        args.push("sides=two-sided-long-edge".to_owned());
    }
    if settings.grayscale {
        args.push("-o".to_owned());
        args.push("ColorModel=Gray".to_owned());
    }
    if let Some(orientation) = settings.orientation.lp_option() {
        args.push("-o".to_owned());
        args.push(orientation.to_owned());
    }
    if let Some(media) = settings.paper_size.lp_option() {
        args.push("-o".to_owned());
        args.push(media.to_owned());
    }
    let ranges = settings.page_ranges.trim();
    if !ranges.is_empty() {
        args.push("-o".to_owned());
        args.push(format!("page-ranges={ranges}"));
    }
    args.push("-t".to_owned());
    args.push(title.to_owned());
    args.push(path_arg.to_owned());
    args
}

pub(super) fn submit_pdf_to_cups(
    path: &Path,
    title: &str,
    settings: &CupsPrintSettings,
) -> Result<String, String> {
    submit_pdf_to_cups_with_runner(path, title, settings, run_process_with_timeout)
}

pub(super) fn submit_pdf_to_cups_with_runner(
    path: &Path,
    title: &str,
    settings: &CupsPrintSettings,
    runner: impl FnOnce(&str, &[String], Duration) -> Result<ProcessOutput, String>,
) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let path_arg = path.to_string_lossy().into_owned();
    let args = cups_lp_args(&path_arg, title, settings);
    let output = runner("lp", &args, CUPS_PRINT_TIMEOUT)?;
    if output.success {
        let job = output.stdout.trim();
        if job.is_empty() {
            Ok(path_arg)
        } else {
            Ok(job.to_owned())
        }
    } else {
        let err = output.stderr.trim();
        if err.is_empty() {
            Err("lp failed without an error message".to_owned())
        } else {
            Err(err.to_owned())
        }
    }
}

/// CUPS print actions on the Browser surface state — kept beside the printing
/// value types and lp/lpstat helpers they drive.
impl WebState {
    pub(super) fn refresh_cups_printers(&mut self) {
        match discover_cups_printers() {
            Ok(printers) => {
                if self.cups_settings.destination.is_none() {
                    self.cups_settings.destination = printers
                        .iter()
                        .find(|printer| printer.is_default)
                        .or_else(|| printers.first())
                        .map(|printer| printer.name.clone());
                }
                self.cups_printers = printers;
                self.cups_notice = None;
            }
            Err(err) => {
                self.cups_printers.clear();
                self.cups_notice = Some(err);
            }
        }
    }

    pub(super) fn queue_active_page_cups_print_to_dir(
        &mut self,
        dir: impl AsRef<Path>,
    ) -> Result<PathBuf, String> {
        if !self.can_drive_page_tools() {
            return Err("no live page".to_owned());
        }
        let (url, title) = {
            let Some(tab) = self.tabs.get(self.active) else {
                return Err("no active tab".to_owned());
            };
            (
                tab.session.nav().url.clone(),
                tab.session.title().to_owned(),
            )
        };
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let now_ms = unix_ms();
        let name = print_pdf_filename_for(&url, &title, now_ms);
        let path = dir.join(name);
        let key = path.to_string_lossy().into_owned();
        let request = CupsPrintRequest {
            path: key.clone(),
            title: cups_job_title(&url, &title, now_ms),
            settings: self.cups_settings.clone(),
        };
        self.pending_cups_prints.insert(key.clone(), request);
        if let Some(tab) = self.active_tab() {
            tab.session.save_pdf(key);
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cups_lp_args_emits_orientation_paper_and_page_range_options() {
        let settings = CupsPrintSettings {
            orientation: PrintOrientation::Landscape,
            paper_size: PaperSize::A4,
            page_ranges: " 1-5,8 ".to_owned(),
            copies: 3,
            ..Default::default()
        };
        let args = cups_lp_args("/tmp/page.pdf", "Page", &settings);
        // The print-preview OPTIONS reach `lp` as -o attributes.
        assert!(args
            .windows(2)
            .any(|w| w == ["-o", "orientation-requested=4"]));
        assert!(args.windows(2).any(|w| w == ["-o", "media=A4"]));
        assert!(args.windows(2).any(|w| w == ["-o", "page-ranges=1-5,8"]));
        assert!(args.windows(2).any(|w| w == ["-n", "3"]));
        // Portrait + printer-default paper + no range emit NO extra -o attrs.
        let plain = cups_lp_args("/tmp/page.pdf", "Page", &CupsPrintSettings::default());
        assert!(!plain.iter().any(|a| a.starts_with("orientation-requested")));
        assert!(!plain.iter().any(|a| a.starts_with("media=")));
        assert!(!plain.iter().any(|a| a.starts_with("page-ranges")));
    }
}
