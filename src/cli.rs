//! Command-line parsing with Python `argparse` behaviour: unique-prefix option
//! abbreviations, `--opt=value`, the same error wording, and exit code 2.

use std::collections::HashMap;

use crate::pystr::PyStr;

#[derive(Clone, Copy)]
pub struct OptionSpec {
    pub name: &'static str,
    pub metavar: &'static str,
    pub append: bool,
    pub required: bool,
    pub help: &'static str,
}

pub struct Subcommand {
    pub name: &'static str,
    pub help: &'static str,
    pub options: Vec<OptionSpec>,
}

const fn opt(name: &'static str, metavar: &'static str, help: &'static str) -> OptionSpec {
    OptionSpec { name, metavar, append: false, required: false, help }
}

fn repository_options(paths: bool, required: bool) -> Vec<OptionSpec> {
    let mut out = vec![OptionSpec {
        name: "--repo",
        metavar: "ROOT",
        append: true,
        required,
        help: "absolute path to a repository worktree root (repeatable)",
    }];
    if paths {
        out.push(opt("--paths", "A,B", "comma-separated repository-relative scope (single --repo only)"));
    }
    out.push(opt("--json", "REQUEST", "a complete ReviewRequest JSON document"));
    out
}

pub fn subcommands() -> Vec<Subcommand> {
    let recovery = opt(
        "--recovery-dir",
        "PATH",
        "new private directory outside reviewed inputs for client-owned recovery",
    );
    let criterion = opt("--criterion", "TEXT", "");
    let guide = opt("--guide", "PATH", "local authored guide JSON for the HTML Guided tab");
    let doc = OptionSpec { required: true, ..opt("--doc", "PATH", "") };
    let mut review = vec![criterion, guide, recovery];
    review.extend(repository_options(true, false));
    let mut preflight = vec![criterion, guide];
    preflight.extend(repository_options(true, false));
    vec![
        Subcommand { name: "review", help: "open a review and wait for a human decision", options: review },
        Subcommand {
            name: "preflight",
            help: "validate scope and guide without opening a review or changing approval",
            options: preflight,
        },
        Subcommand {
            name: "verify",
            help: "approve only when a matching approval is recorded",
            options: repository_options(true, false),
        },
        Subcommand {
            name: "status",
            help: "print the newest ledger entry and the fingerprint match",
            options: repository_options(true, false),
        },
        Subcommand {
            name: "consume",
            help: "record that an approval was spent by a commit",
            options: repository_options(false, true),
        },
        Subcommand {
            name: "revoke",
            help: "record that an approval no longer counts",
            options: repository_options(false, false),
        },
        Subcommand {
            name: "review-doc",
            help: "review a specification or plan in the browser and wait for a human decision",
            options: vec![OptionSpec { required: true, ..criterion }, doc, recovery],
        },
        Subcommand {
            name: "verify-doc",
            help: "approve only when the exact document text was approved",
            options: vec![criterion, doc],
        },
        Subcommand {
            name: "consume-doc",
            help: "record that a document approval was spent by the next phase",
            options: vec![criterion, doc],
        },
    ]
}

pub const PROG: &str = "senpai";
const DESCRIPTION: &str =
    "Run a pre-commit review through the configured reviewer and record a verifiable approval.";

#[derive(Debug, Default)]
pub struct Args {
    pub command: String,
    values: HashMap<&'static str, PyStr>,
    lists: HashMap<&'static str, Vec<PyStr>>,
}

impl Args {
    pub fn value(&self, name: &str) -> Option<&PyStr> {
        self.values.get(name)
    }

    pub fn list(&self, name: &str) -> &[PyStr] {
        self.lists.get(name).map_or(&[], Vec::as_slice)
    }
}

pub enum Parsed {
    Run(Args),
    /// Help text for stdout, exit 0.
    Help(String),
    /// Usage and error text for stderr, exit 2.
    Error(String),
}

fn usage_line(sub: Option<&Subcommand>) -> String {
    match sub {
        None => {
            let names: Vec<&str> = subcommands().iter().map(|s| s.name).collect();
            format!("usage: {PROG} [-h] {{{}}} ...", names.join(","))
        }
        Some(sub) => {
            let mut parts = vec![format!("usage: {PROG} {} [-h]", sub.name)];
            for o in &sub.options {
                let item = format!("{} {}", o.name, o.metavar);
                parts.push(if o.required { item } else { format!("[{item}]") });
            }
            parts.join(" ")
        }
    }
}

fn help_text(sub: Option<&Subcommand>) -> String {
    let mut out = format!("{}\n\n", usage_line(sub));
    match sub {
        None => {
            out.push_str(&format!("{DESCRIPTION}\n\npositional arguments:\n"));
            for s in subcommands() {
                out.push_str(&format!("  {:<12} {}\n", s.name, s.help));
            }
            out.push_str("\noptions:\n  -h, --help   show this help message and exit\n");
        }
        Some(sub) => {
            out.push_str("options:\n  -h, --help            show this help message and exit\n");
            for o in &sub.options {
                let label = format!("{} {}", o.name, o.metavar);
                out.push_str(&format!("  {label:<21} {}\n", o.help));
            }
        }
    }
    out
}

fn error(sub: Option<&Subcommand>, message: &str) -> Parsed {
    let prog = match sub {
        Some(s) => format!("{PROG} {}", s.name),
        None => PROG.to_string(),
    };
    Parsed::Error(format!("{}\n{prog}: error: {message}\n", usage_line(sub)))
}

fn negative_number(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('-') else {
        return false;
    };
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    (!rest.is_empty() && digits(rest))
        || rest.split_once('.').is_some_and(|(a, b)| digits(a) && !b.is_empty() && digits(b))
}

/// `ArgumentParser._parse_optional`: whether a token is an option string.
fn is_option(sub: Option<&Subcommand>, token: &str) -> bool {
    if !token.starts_with('-') || token.len() == 1 {
        return false;
    }
    let names: Vec<&str> = match sub {
        Some(sub) => sub.options.iter().map(|o| o.name).chain(["-h", "--help"]).collect(),
        None => vec!["-h", "--help"],
    };
    if names.contains(&token) {
        return true;
    }
    if let Some((flag, _)) = token.split_once('=') {
        if names.contains(&flag) {
            return true;
        }
    }
    if token.starts_with("--") {
        let prefix = token.split_once('=').map_or(token, |(f, _)| f);
        if names.iter().any(|n| n.starts_with(prefix)) {
            return true;
        }
    }
    if negative_number(token) || token.contains(' ') {
        return false;
    }
    true
}

fn find_option<'a>(sub: &'a Subcommand, flag: &str) -> Result<Option<&'a OptionSpec>, String> {
    if let Some(exact) = sub.options.iter().find(|o| o.name == flag) {
        return Ok(Some(exact));
    }
    if !flag.starts_with("--") {
        return Ok(None);
    }
    let matches: Vec<&OptionSpec> = sub.options.iter().filter(|o| o.name.starts_with(flag)).collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches[0])),
        _ => {
            let names: Vec<&str> = matches.iter().map(|o| o.name).collect();
            Err(format!("ambiguous option: {flag} could match {}", names.join(", ")))
        }
    }
}

pub fn parse(argv: &[PyStr]) -> Parsed {
    let tokens: Vec<String> = argv.iter().map(PyStr::display).collect();
    let mut index = 0;
    if let Some(token) = tokens.first().filter(|token| is_option(None, token)) {
        if token == "-h" || (token.len() > 2 && "--help".starts_with(token.as_str())) {
            return Parsed::Help(help_text(None));
        }
        return error(None, &format!("unrecognized arguments: {}", tokens.join(" ")));
    }
    let Some(name) = tokens.get(index) else {
        return error(None, "the following arguments are required: command");
    };
    let all = subcommands();
    let Some(sub) = all.iter().find(|s| s.name == name) else {
        let names: Vec<String> = all.iter().map(|s| format!("'{}'", s.name)).collect();
        return error(None, &format!("argument command: invalid choice: '{name}' (choose from {})", names.join(", ")));
    };
    index += 1;
    let mut args = Args { command: sub.name.to_string(), ..Default::default() };
    let mut unrecognized: Vec<String> = Vec::new();
    while index < tokens.len() {
        let token = tokens[index].clone();
        let raw = argv[index].clone();
        index += 1;
        if token == "--" {
            unrecognized.extend(tokens[index..].iter().cloned());
            break;
        }
        if !is_option(Some(sub), &token) {
            unrecognized.push(token);
            continue;
        }
        if token == "-h" || token == "--help" {
            return Parsed::Help(help_text(Some(sub)));
        }
        let (flag, inline) = match token.split_once('=') {
            Some((f, _)) if token.starts_with("--") => {
                let offset = f.len() + 1;
                let value = PyStr::from_wtf8_unchecked(raw.as_wtf8()[offset..].to_vec());
                (f.to_string(), Some(value))
            }
            _ => (token.clone(), None),
        };
        let spec = match find_option(sub, &flag) {
            Ok(Some(spec)) => spec,
            Ok(None) => {
                unrecognized.push(token);
                continue;
            }
            Err(message) => return error(Some(sub), &message),
        };
        let value = match inline {
            Some(v) => v,
            None => match (tokens.get(index), argv.get(index)) {
                (Some(next), Some(raw_next)) if !is_option(Some(sub), next) => {
                    index += 1;
                    raw_next.clone()
                }
                _ => return error(Some(sub), &format!("argument {}: expected one argument", spec.name)),
            },
        };
        if spec.append {
            args.lists.entry(spec.name).or_default().push(value);
        } else {
            args.values.insert(spec.name, value);
        }
    }
    let missing: Vec<&str> = sub
        .options
        .iter()
        .filter(|o| o.required && !args.values.contains_key(o.name) && !args.lists.contains_key(o.name))
        .map(|o| o.name)
        .collect();
    if !missing.is_empty() {
        return error(Some(sub), &format!("the following arguments are required: {}", missing.join(", ")));
    }
    if !unrecognized.is_empty() {
        return error(None, &format!("unrecognized arguments: {}", unrecognized.join(" ")));
    }
    Parsed::Run(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(items: &[&str]) -> Parsed {
        parse(&items.iter().map(|s| PyStr::from(*s)).collect::<Vec<_>>())
    }

    #[test]
    fn parses_values_prefixes_and_repeats() {
        let Parsed::Run(args) = run(&["review", "--crit", "ship it", "--repo=/a", "--repo", "/b", "--paths", "x,y"]) else {
            panic!("expected a run");
        };
        assert_eq!(args.value("--criterion"), Some(&PyStr::from("ship it")));
        assert_eq!(args.list("--repo"), &[PyStr::from("/a"), PyStr::from("/b")]);
        assert_eq!(args.value("--paths"), Some(&PyStr::from("x,y")));
    }

    #[test]
    fn reports_argparse_errors() {
        let Parsed::Error(text) = run(&["review-doc", "--doc", "x"]) else { panic!() };
        assert!(text.contains("required: --criterion"), "{text}");
        let Parsed::Error(text) = run(&["review", "--re", "x"]) else { panic!() };
        assert!(text.contains("ambiguous option"), "{text}");
        let Parsed::Error(text) = run(&["review", "--criterion"]) else { panic!() };
        assert!(text.contains("expected one argument"), "{text}");
        let Parsed::Error(text) = run(&[]) else { panic!() };
        assert!(text.starts_with("usage:"));
        assert!(matches!(run(&["--help"]), Parsed::Help(_)));
        assert!(matches!(run(&["verify", "-h"]), Parsed::Help(_)));
        let Parsed::Run(args) = run(&["review", "--criterion", "-5"]) else { panic!() };
        assert_eq!(args.value("--criterion"), Some(&PyStr::from("-5")));
    }
}
