use std::collections::BTreeMap;
use std::fmt::Display;
use std::fmt::Formatter;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use clap::builder::PossibleValue;
use clap::Parser;
use lsp_types::Diagnostic;
use lsp_types::DiagnosticSeverity;
use lsp_types::Location;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::Url;
use miette::miette;
use miette::Context;
use miette::IntoDiagnostic;
use owo_colors::OwoColorize;
use owo_colors::Stream::Stdout;
use path_absolutize::Absolutize;

/// Check project diagnostics using `lua-language-server`.
#[derive(Debug, Clone, Parser)]
struct Opts {
    /// Path to `lua-language-server` executable.
    #[arg(short = 'c', long, default_value = "lua-language-server")]
    lua_language_server: PathBuf,

    /// Error if any diagnostics at or greater than this severity are found.
    #[arg(long, default_value = "warning")]
    fail: Severity,

    /// Display diagnostics at or greater than this severity.
    #[arg(long, default_value = "hint")]
    show: Severity,

    /// Path to the project to check.
    #[arg(default_value = ".")]
    project: PathBuf,
}

#[derive(Debug, Clone)]
enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Display for Severity {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Severity::Error => "error",
                Severity::Warning => "warning",
                Severity::Information => "info",
                Severity::Hint => "hint",
            }
        )
    }
}

impl clap::ValueEnum for Severity {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Error, Self::Warning, Self::Information, Self::Hint]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        match self {
            Severity::Error => Some(PossibleValue::new("error")),
            Severity::Warning => Some(PossibleValue::new("warning")),
            Severity::Information => Some(PossibleValue::new("info")),
            Severity::Hint => Some(PossibleValue::new("hint")),
        }
    }
}

impl From<Severity> for DiagnosticSeverity {
    fn from(value: Severity) -> Self {
        match value {
            Severity::Error => DiagnosticSeverity::ERROR,
            Severity::Warning => DiagnosticSeverity::WARNING,
            Severity::Information => DiagnosticSeverity::INFORMATION,
            Severity::Hint => DiagnosticSeverity::HINT,
        }
    }
}

fn main() -> miette::Result<()> {
    let opts = Opts::parse();
    pretty_env_logger::init();

    let fail: DiagnosticSeverity = opts.fail.into();
    let mut show: DiagnosticSeverity = opts.show.into();

    if fail > show {
        show = fail;
    }

    let current_dir = std::env::current_dir().into_diagnostic()?;
    let project_absolute = opts
        .project
        .absolutize_from(&current_dir)
        .into_diagnostic()
        .wrap_err_with(|| format!("Failed to make path absolute: {:?}", opts.project))?;

    let mut cmd = Command::new(opts.lua_language_server);
    cmd.arg("--check")
        .arg(&opts.project)
        .arg("--checklevel")
        .arg("Information")
        .stdout(Stdio::piped());

    let mut child = cmd.spawn().into_diagnostic()?;

    let mut luals_stdout = child
        .stdout
        .take()
        .ok_or_else(|| miette!("lua-language-server process doesn't have a stdout handle"))?;

    let join_handle = std::thread::spawn(move || {
        let mut stdout_contents = Vec::<u8>::with_capacity(4096);
        let mut buffer = vec![0; 1024];
        loop {
            match luals_stdout.read(&mut buffer) {
                Ok(0) => {
                    // EOF
                    break;
                }
                Ok(n) => {
                    stdout_contents.extend(&buffer[..n]);
                    std::io::stdout()
                        .write_all(&buffer[..n])
                        .into_diagnostic()?;
                }
                Err(err) => {
                    return Err(err).into_diagnostic();
                }
            }
        }
        Ok(stdout_contents)
    });

    let exit_code = child.wait().into_diagnostic()?;

    if !exit_code.success() {
        return Err(miette!("lua-language-server failed: {exit_code}"));
    }

    let result = match join_handle.join() {
        Ok(result) => result?,
        Err(panic_value) => {
            std::panic::resume_unwind(panic_value);
        }
    };

    let stdout = String::from_utf8(result).map_err(|err| {
        miette!(
            "lua-language-server wrote invalid UTF-8 to stdout: {}",
            String::from_utf8_lossy(err.as_bytes())
        )
    })?;

    let last_line = stdout
        .lines()
        .last()
        .ok_or_else(|| miette!("lua-language-server didn't write any lines: {stdout:?}"))?;

    let last_token = last_line.split_ascii_whitespace().last().ok_or_else(|| {
        miette!("Last line of lua-language-server output doesn't contain any data: {last_line:?}")
    })?;

    let path = Path::new(last_token);

    if !path.exists() {
        return Err(miette!(
            "lua-language-server diagnostics file doesn't exist: {path:?}"
        ));
    }

    let diagnostics: BTreeMap<String, Vec<Diagnostic>> = serde_json::from_str(
        &std::fs::read_to_string(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to read diagnostics file: {path:?}"))?,
    )
    .into_diagnostic()
    .wrap_err_with(|| format!("Failed to deserialize diagnostics file: {path:?}"))?;

    let mut found_diagnostics = 0;

    for (path, diagnostics) in &diagnostics {
        let url = lsp_types::Url::parse(path)
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to parse URL: {path:?}"))?;

        let relative_path = to_relative_path(&url, &project_absolute)?;

        if !url
            .to_file_path()
            .map(|p| p.starts_with(&project_absolute))
            .unwrap_or(true)
        {
            log::debug!("Ignoring diagnostics in out-of-project path {relative_path:?}");
            continue;
        }

        for diagnostic in diagnostics {
            if diagnostic
                .severity
                .map(|severity| severity > show)
                .unwrap_or(false)
            {
                continue;
            }
            if diagnostic
                .severity
                .map(|severity| severity <= fail)
                .unwrap_or(false)
            {
                found_diagnostics += 1;
            }

            let path_diagnostic = PathDiagnostic {
                cwd: &project_absolute,
                path: &relative_path,
                diagnostic,
            };
            write!(std::io::stdout(), "\n{path_diagnostic}").into_diagnostic()?;
        }
    }

    if found_diagnostics > 0 {
        let _ = writeln!(std::io::stdout());
        Err(miette!(
            "lua-language-server found {} problems",
            found_diagnostics
        ))
    } else {
        Ok(())
    }
}

struct PathDiagnostic<'a> {
    path: &'a Path,
    cwd: &'a Path,
    diagnostic: &'a Diagnostic,
}

impl<'a> PathDiagnostic<'a> {
    fn write_location(&self, f: &mut Formatter<'_>, location: &Location) -> std::fmt::Result {
        match to_relative_path(&location.uri, self.cwd) {
            Ok(path) => {
                write!(f, "{}:", path.display())?;
            }
            Err(_) => {
                write!(f, "{}:", location.uri)?;
            }
        }
        write_range(f, location.range)
    }
}

impl<'a> Display for PathDiagnostic<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:", self.path.display())?;
        write_range(f, self.diagnostic.range)?;
        if let Some(code) = &self.diagnostic.code {
            write!(f, " [")?;
            match code {
                lsp_types::NumberOrString::Number(code) => {
                    write!(f, "{}", code.if_supports_color(Stdout, |text| text.bold()))?;
                }
                lsp_types::NumberOrString::String(code) => {
                    write!(f, "{}", code.if_supports_color(Stdout, |text| text.bold()))?;
                }
            }
            writeln!(f, "]")?;
        } else {
            writeln!(f)?;
        }

        let mut message = String::new();
        if let Some(severity) = self.diagnostic.severity {
            message = write_severity(severity);
        }
        message.push_str(": ");
        message.push_str(&self.diagnostic.message);
        let opts = textwrap_opts();
        writeln!(f, "{}", textwrap::fill(&message, opts))?;

        if let Some(related_information) = &self.diagnostic.related_information {
            for information in related_information {
                if information.location.range == self.diagnostic.range
                    && (information.message.is_empty()
                        || information.message == self.diagnostic.message)
                {
                    // Ignore redundant related information.
                    continue;
                }
                write!(f, "    • ")?;
                self.write_location(f, &information.location)?;
                if !information.message.is_empty() {
                    writeln!(f, ": {}", information.message)?;
                }
            }
        }

        // TODO: Anything useful in the `data` field?
        // TODO: The `source` field seems mostly unhelpful.
        // TODO: Worth rendering the diagnostic tags (showing unecessary or deprecated
        // code)?
        Ok(())
    }
}

fn write_range(f: &mut Formatter<'_>, range: Range) -> std::fmt::Result {
    if range.start == range.end {
        write_position(f, range.start)
    } else {
        write_position(f, range.start)?;
        write!(f, "-")?;
        write_position(f, range.end)?;
        Ok(())
    }
}

fn write_position(f: &mut Formatter<'_>, position: Position) -> std::fmt::Result {
    write!(f, "{}:{}", position.line, position.character)
}

fn to_relative_path(url: &Url, cwd: &Path) -> miette::Result<PathBuf> {
    let scheme = url.scheme();
    if scheme != "file" {
        return Err(miette!(
            "URL has unknown scheme {scheme:?}; expected \"file\""
        ));
    }
    let path = url
        .to_file_path()
        .map_err(|()| miette!("Failed to convert URL to file path: {url:?}"))?;

    Ok(pathdiff::diff_paths(&path, cwd).unwrap_or(path))
}

fn write_severity(severity: DiagnosticSeverity) -> String {
    if severity == DiagnosticSeverity::ERROR {
        "error"
            .if_supports_color(Stdout, |text| text.bright_red())
            .to_string()
    } else if severity == DiagnosticSeverity::WARNING {
        "warning"
            .if_supports_color(Stdout, |text| text.bright_yellow())
            .to_string()
    } else if severity == DiagnosticSeverity::INFORMATION {
        "info"
            .if_supports_color(Stdout, |text| text.bright_white())
            .to_string()
    } else if severity == DiagnosticSeverity::HINT {
        "hint"
            .if_supports_color(Stdout, |text| text.bright_cyan())
            .to_string()
    } else {
        // Unknown severity
        String::new()
    }
}

fn textwrap_opts() -> textwrap::Options<'static> {
    let indent = "    ";
    let mut opts = textwrap::Options::with_termwidth()
        .initial_indent(indent)
        .subsequent_indent(indent);
    opts.width -= indent.len();
    opts
}
