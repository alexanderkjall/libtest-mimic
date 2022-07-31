//! Write your own test scripts that look and behave like built-in tests!
//!
//! This is a simple and small testing framework that mimics the original
//! `libtest` (used by `cargo test`/`rustc --test`). That means: all output
//! looks pretty much like `cargo test` and most CLI arguments are understood
//! and used. With that plumbing work out of the way, your test runner can
//! concentrate on the actual testing.
//!
//! The central function of this crate is [`run`].
//!
//! # Example
//!
//! ```no_run
//! extern crate libtest_mimic;
//!
//! use libtest_mimic::{Arguments, Test};
//!
//!
//! // Parse command line arguments
//! let args = Arguments::from_args();
//!
//! // Create a list of tests (in this case: two dummy tests)
//! let tests = vec![
//!     Test::test("check_toph", move || { /* The test */ Ok(()) }),
//!     Test::test("check_sokka", move || { /* The test */ Err("Woops".into()) }),
//! ];
//!
//! // Run all tests and exit the application appropriatly.
//! libtest_mimic::run(&args, tests).exit();
//! ```
//!
//! For more examples, see [`examples/` in the repository][repo-examples].
//!
//!
//! [repo-examples]: https://github.com/LukasKalbertodt/libtest-mimic/tree/master/examples

use std::{process, sync::mpsc, fmt};

mod args;
mod printer;

use printer::Printer;
use threadpool::ThreadPool;

pub use crate::args::{Arguments, ColorSetting, FormatSetting};



/// A single test or benchmark.
///
/// Yes, `libtest` often counts benchmarks as "tests", which is a bit confusing.
/// The main parts of this definition is `name`, which is printed and used for
/// filtering, and `runner`, which is called when the test is executed to
/// determine its outcome.
pub struct Test {
    runner: Box<dyn FnOnce() -> Outcome + Send>,
    info: TestInfo,
}

impl Test {
    /// Creates a (non-benchmark) test with the given name and runner.
    pub fn test(
        name: impl Into<String>,
        runner: impl FnOnce() -> Result<(), Failed> + Send + 'static,
    ) -> Self {
        Self {
            runner: Box::new(move || match runner() {
                Ok(()) => Outcome::Passed,
                Err(failed) => Outcome::Failed(failed),
            }),
            info: TestInfo {
                name: name.into(),
                kind: String::new(),
                is_ignored: false,
                is_bench: false,
            },
        }
    }

    /// Creates a benchmark with the given name and runner.
    pub fn bench(
        name: impl Into<String>,
        runner: impl FnOnce() -> Result<Measurement, Failed> + Send + 'static,
    ) -> Self {
        Self {
            runner: Box::new(move || match runner() {
                Ok(measurement) => Outcome::Measured(measurement),
                Err(failed) => Outcome::Failed(failed),
            }),
            info: TestInfo {
                name: name.into(),
                kind: String::new(),
                is_ignored: false,
                is_bench: true,
            },
        }
    }

    /// Sets the "kind" of this test/benchmark. If this string is not
    /// empty, it is printed in brackets before the test name (e.g.
    /// `test [my-kind] test_name`). (Default: *empty*)
    pub fn with_kind(self, kind: impl Into<String>) -> Self {
        Self {
            info: TestInfo {
                kind: kind.into(),
                ..self.info
            },
            ..self
        }
    }

    /// Sets whether or not this test is considered "ignored". (Default: `false`)
    ///
    /// With the built-in test suite, you can annotate `#[ignore]` on tests to
    /// not execute them by default (for example because they take a long time
    /// or require a special environment). If the `--ignored` flag is set,
    /// ignored tests are executed, too.
    pub fn with_ignored_flag(self, is_ignored: bool) -> Self {
        Self {
            info: TestInfo {
                is_ignored,
                ..self.info
            },
            ..self
        }
    }
}

impl fmt::Debug for Test {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct OpaqueRunner;
        impl fmt::Debug for OpaqueRunner {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("<runner>")
            }
        }

        f.debug_struct("Test")
            .field("runner", &OpaqueRunner)
            .field("name", &self.info.name)
            .field("kind", &self.info.kind)
            .field("is_ignored", &self.info.is_ignored)
            .field("is_bench", &self.info.is_bench)
            .finish()
    }
}

#[derive(Debug)]
struct TestInfo {
    name: String,
    kind: String,
    is_ignored: bool,
    is_bench: bool,
}

/// Output of a benchmark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measurement {
    /// Average time in ns.
    pub avg: u64,

    /// Variance in ns.
    pub variance: u64,
}

/// Indicates that a test/benchmark has failed. Optionally carries a message.
#[derive(Debug, Clone)]
pub struct Failed {
    msg: Option<String>,
}

impl Failed {
    pub fn without_message() -> Self {
        Self { msg: None }
    }

    pub fn message(&self) -> Option<&str> {
        self.msg.as_deref()
    }
}

impl<M: std::fmt::Display> From<M> for Failed {
    fn from(msg: M) -> Self {
        Self {
            msg: Some(msg.to_string())
        }
    }
}



/// The outcome of performing a test/benchmark.
#[derive(Debug, Clone)]
enum Outcome {
    /// The test passed.
    Passed,

    /// The test or benchmark failed.
    Failed(Failed),

    /// The test or benchmark was ignored.
    Ignored,

    /// The benchmark was successfully run.
    Measured(Measurement),
}

/// Contains information about the entire test run. Is returned by[`run`].
///
/// This type is marked as `#[must_use]`. Usually, you just call
/// [`exit()`][Conclusion::exit] on the result of `run` to exit the application
/// with the correct exit code. But you can also store this value and inspect
/// its data.
#[derive(Clone, Debug)]
#[must_use = "Call `exit()` or `exit_if_failed()` to set the correct return code"]
pub struct Conclusion {
    /// Number of tests and benchmarks that were filtered out (either by the
    /// filter-in pattern or by `--skip` arguments).
    pub num_filtered_out: u64,

    /// Number of passed tests.
    pub num_passed: u64,

    /// Number of failed tests and benchmarks.
    pub num_failed: u64,

    /// Number of ignored tests and benchmarks.
    pub num_ignored: u64,

    /// Number of benchmarks that successfully ran.
    pub num_benches: u64,
}

impl Conclusion {
    /// Exits the application with an appropriate error code (0 if all tests
    /// have passed, 101 if there have been failures).
    pub fn exit(&self) -> ! {
        self.exit_if_failed();
        process::exit(0);
    }

    /// Exits the application with error code 101 if there were any failures.
    /// Otherwise, returns normally.
    pub fn exit_if_failed(&self) {
        if self.has_failed() {
            process::exit(101)
        }
    }

    /// Returns whether there have been any failures.
    pub fn has_failed(&self) -> bool {
        self.num_failed > 0
    }

    fn empty() -> Self {
        Self {
            num_filtered_out: 0,
            num_passed: 0,
            num_failed: 0,
            num_ignored: 0,
            num_benches: 0,
        }
    }
}

impl Arguments {
    /// Returns `true` if the given test should be ignored.
    fn is_ignored(&self, test: &Test) -> bool {
        (test.info.is_ignored && !self.ignored)
            || (test.info.is_bench && self.test)
            || (!test.info.is_bench && self.bench)
    }

    fn is_filtered_out(&self, test: &Test) -> bool {
        let test_name = &test.info.name;

        // If a filter was specified, apply this
        if let Some(filter) = &self.filter_string {
            match self.exact {
                true if test_name != filter => return true,
                false if !test_name.contains(filter) => return true,
                _ => {}
            };
        }

        // If any skip pattern were specified, test for all patterns.
        for skip_filter in &self.skip {
            match self.exact {
                true if test_name == skip_filter => return true,
                false if test_name.contains(skip_filter) => return true,
                _ => {}
            }
        }

        false
    }
}

/// Runs all given tests with the given test runner.
///
/// This is the central function of this crate. It provides the framework for
/// the testing harness. It does all the printing and house keeping.
///
/// This function tries to respect most options configured via CLI args. For
/// example, filtering, output format and coloring are respected. However, some
/// things cannot be handled by this function and *you* (as a user) need to
/// take care of it yourself. The following options are ignored by this
/// function and need to be manually checked:
///
/// - `--nocapture` and capturing in general. It is expected that during the
///   test, nothing writes to `stdout` and `stderr`, unless `--nocapture` was
///   specified. If the test is ran as a seperate process, this is fairly easy.
///   If however, the test is part of the current application and it uses
///   `println!()` and friends, it might be impossible to capture the output.
///
/// Currently, the following CLI arg is ignored, but is planned to be used
/// in the future:
/// - `--format=json`. If specified, this function will panic.
///
/// All other flags and options are used properly.
///
/// The returned value contains a couple of useful information. See the
/// [`Conclusion`] documentation for more information. If `--list` was
/// specified, a list is printed and a dummy `Conclusion` is returned.
pub fn run(args: &Arguments, mut tests: Vec<Test>) -> Conclusion {
    let mut conclusion = Conclusion::empty();

    // Apply filtering
    if args.filter_string.is_some() || !args.skip.is_empty() {
        let len_before = tests.len() as u64;
        tests.retain(|test| !args.is_filtered_out(test));
        conclusion.num_filtered_out = len_before - tests.len() as u64;
    }
    let tests = tests;

    // Create printer which is used for all output.
    let mut printer = printer::Printer::new(args, &tests);

    // If `--list` is specified, just print the list and return.
    if args.list {
        printer.print_list(&tests, args.ignored);
        return Conclusion::empty();
    }

    // Print number of tests
    printer.print_title(tests.len() as u64);

    let mut failed_tests = Vec::new();
    let mut handle_outcome = |outcome: Outcome, test: TestInfo, printer: &mut Printer| {
        printer.print_single_outcome(&outcome);

        if test.is_bench {
            conclusion.num_benches += 1;
        }

        // Handle outcome
        match outcome {
            Outcome::Passed => conclusion.num_passed += 1,
            Outcome::Failed(failed) => {
                failed_tests.push((test, failed.msg));
                conclusion.num_failed += 1;
            },
            Outcome::Ignored => conclusion.num_ignored += 1,
            Outcome::Measured(_) => {}
        }
    };

    // Execute all tests.
    if args.num_threads == Some(1) {
        // Run test sequentially in main thread
        for test in tests {
            // Print `test foo    ...`, run the test, then print the outcome in
            // the same line.
            printer.print_test(&test.info);
            let outcome = if args.is_ignored(&test) {
                Outcome::Ignored
            } else {
                (test.runner)()
            };
            handle_outcome(outcome, test.info, &mut printer);
        }
    } else {
        // Run test in thread pool.
        let pool = ThreadPool::default();
        let (sender, receiver) = mpsc::channel();

        let num_tests = tests.len();
        for test in tests {
            if args.is_ignored(&test) {
                sender.send((Outcome::Ignored, test.info)).unwrap();
            } else {
                let sender = sender.clone();
                pool.execute(move || {
                    // It's fine to ignore the result of sending. If the
                    // receiver has hung up, everything will wind down soon
                    // anyway.
                    let outcome = (test.runner)();
                    let _ = sender.send((outcome, test.info));
                });
            }
        }

        for (outcome, test_info) in receiver.iter().take(num_tests) {
            // In multithreaded mode, we do only print the start of the line
            // after the test ran, as otherwise it would lead to terribly
            // interleaved output.
            printer.print_test(&test_info);
            handle_outcome(outcome, test_info, &mut printer);
        }
    }

    // Print failures if there were any, and the final summary.
    if !failed_tests.is_empty() {
        printer.print_failures(&failed_tests);
    }

    printer.print_summary(&conclusion);

    conclusion
}
