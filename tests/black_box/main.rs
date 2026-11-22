//! Black-box tests: they drive the release `senpai` binary from the outside and
//! check its exit codes, JSON, ledger, pages and inbox, the way an agent or a
//! person would meet them.
//!
//! The same test binary also plays every fake the tests need (the stub
//! reviewer, `rex`, `tern`): started by a fake's wrapper script with
//! `--senpai-fake=ROLE` as its first argument, it acts that part and exits, so
//! the shipped binary carries no test-only code. The role is an argument, not
//! an environment variable, so a stray export can never turn the run into a
//! fake that passes without testing anything.

mod client;
mod fakes;
mod inbox;
mod page;
mod skill;
mod support;

use libtest_mimic::{Arguments, Trial};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if let Some(role) = argv.get(1).and_then(|a| a.strip_prefix(fakes::ROLE_FLAG)) {
        let mut rest = vec![argv[0].clone()];
        rest.extend_from_slice(&argv[2..]);
        std::process::exit(fakes::run(role, &rest));
    }
    let args = Arguments::from_args();
    let mut trials = Vec::new();
    for (module, tests) in [("client", client::TESTS), ("page", page::TESTS), ("inbox", inbox::TESTS), ("skill", skill::TESTS)] {
        for &(name, test) in tests {
            trials.push(Trial::test(format!("{module}::{name}"), move || {
                test();
                Ok(())
            }));
        }
    }
    libtest_mimic::run(&args, trials).exit();
}
