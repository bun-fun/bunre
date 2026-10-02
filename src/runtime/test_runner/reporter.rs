//! Console/JUnit reporters for the test runner.
//!
//! Moved out of `cli/test_command.rs` when `bun test` was removed: the
//! `test_runner` module (the `bun:test` implementation) owns these types and
//! calls into them, so they cannot live behind the deleted CLI command.

use bun_io::Write as _;

use bun_collections::{BoundedArray, index_sort};
use bun_core::strings;
use bun_core::{self as bun, Global, Output, env_var, fmt as bun_fmt, pretty_error};
use bun_jsc::virtual_machine::VirtualMachine;
use bun_jsc::{self as jsc};
use bun_options_types::code_coverage_options::CodeCoverageOptions;
use bun_paths as bun_path;
use bun_paths::resolve_path;
use bun_paths::string_paths::without_leading_path_separator;
use bun_resolver::fs::FileSystem;
use bun_sys::{self, Fd, File};

use super::jest::{self, FileColumns as _, Summary, TestRunner};

/// Test-file name suffixes (`bun test` treated `<name>.test.ts` and friends as
/// test files).
const TEST_NAME_SUFFIXES: [&[u8]; 4] = [b".test", b"_test", b".spec", b"_spec"];
mod coverage {
    pub(super) use bun_sourcemap_jsc::code_coverage::{
        ByteRangeMapping, Fraction, Report as CodeCoverageReport, lcov, text,
    };

    /// Less-than predicate adapted to the `Ordering` shape `sort_by` wants.
    #[inline]
    pub(super) fn is_less_than_cmp(
        a: &&mut ByteRangeMapping,
        b: &&mut ByteRangeMapping,
    ) -> core::cmp::Ordering {
        bun_core::order(a.source_url.slice(), b.source_url.slice())
    }

    pub(super) fn is_ignored(
        opts: &bun_options_types::code_coverage_options::CodeCoverageOptions,
        relative_dir: &[u8],
        source_url: &[u8],
    ) -> bool {
        if opts.ignore_patterns.is_empty() {
            return false;
        }
        let relative_path = bun_paths::resolve_path::relative(relative_dir, source_url);
        opts.ignore_patterns
            .iter()
            .any(|pattern| bun_glob::r#match(pattern, relative_path).matches())
    }
}
use coverage::{ByteRangeMapping, CodeCoverageReport, Fraction};
#[allow(non_snake_case)]
mod bun_test {
    //! Façade over `crate::test_runner` that preserves the legacy paths
    //! the body uses (`bun_test::Execution::Result`, `bun_test::BasicResult`,
    //! `bun_test::DescribeScope`, …). Drop once the body is normalised.

    /// `add_result()` queue payload.
    pub(super) use crate::test_runner::bun_test::*;
    pub(super) use crate::test_runner::execution::{
        Basic as BasicResult, ExpectAssertions, PendingIs as PendingMode,
    };
    #[allow(non_snake_case)]
    pub(super) mod Execution {
        pub(crate) use crate::test_runner::execution::*;
    }
}

pub(crate) fn escape_xml(str_: &[u8], writer: &mut impl bun_io::Write) -> crate::Result<()> {
    let mut last: usize = 0;
    let mut i: usize = 0;
    let len = str_.len();
    while i < len {
        let c = str_[i];
        match c {
            b'&' | b'<' | b'>' | b'"' | b'\'' => {
                if i > last {
                    writer.write_all(&str_[last..i])?;
                }
                writer.write_all(bun_core::strings::xml_escape_entity(c).unwrap())?;
                last = i + 1;
            }
            b'\t' | b'\n' | b'\r' => {
                // Valid XML 1.0 Char. Emit as a numeric reference so the literal
                // byte survives attribute-value normalisation (XML 1.0 §3.3.3).
                if i > last {
                    writer.write_all(&str_[last..i])?;
                }
                write!(writer, "&#{};", c)?;
                last = i + 1;
            }
            0..=0x1f => {
                // Any other C0 control character is not a valid XML 1.0 Char and
                // cannot be represented even as a numeric reference, so drop it.
                if i > last {
                    writer.write_all(&str_[last..i])?;
                }
                last = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if len > last {
        writer.write_all(&str_[last..])?;
    }
    Ok(())
}

fn fmt_status_text_line(
    status: bun_test::Execution::Result,
    emoji_or_color: bool,
) -> Output::PrettyBuf {
    // emoji and color might be split into two different options in the future
    // some terminals support color, but not emoji.
    // For now, they are the same.
    match emoji_or_color {
        true => match status.basic_result() {
            bun_test::BasicResult::Pending => Output::pretty_fmt::<true>("<r><d>…<r>"),
            bun_test::BasicResult::Pass => Output::pretty_fmt::<true>("<r><green>✓<r>"),
            bun_test::BasicResult::Fail => Output::pretty_fmt::<true>("<r><red>✗<r>"),
            bun_test::BasicResult::Skip => Output::pretty_fmt::<true>("<r><yellow>»<d>"),
            bun_test::BasicResult::Todo => Output::pretty_fmt::<true>("<r><magenta>✎<r>"),
        },
        false => match status.basic_result() {
            bun_test::BasicResult::Pending => Output::pretty_fmt::<false>("<r><d>(pending)<r>"),
            bun_test::BasicResult::Pass => Output::pretty_fmt::<false>("<r><green>(pass)<r>"),
            bun_test::BasicResult::Fail => Output::pretty_fmt::<false>("<r><red>(fail)<r>"),
            bun_test::BasicResult::Skip => Output::pretty_fmt::<false>("<r><yellow>(skip)<d>"),
            bun_test::BasicResult::Todo => Output::pretty_fmt::<false>("<r><magenta>(todo)<r>"),
        },
    }
}

// `Output::error_writer()` / `Output::writer()` already return an unbounded
// `&mut io::Writer`; the previous local `err_w`/`out_w` wrappers were no-op
// reborrows. Call sites use the `Output` accessors directly.

/// Name, message and stack of the error(s) thrown by the test that is
/// currently failing, captured as they are printed so structured reporters
/// get them without re-running the exception formatter.
#[derive(Default)]
pub struct TestFailure {
    pub name: Vec<u8>,
    pub(crate) message: Vec<u8>,
    pub(crate) body: Vec<u8>,
}

/// How a test file is named in the JUnit document: relative to the project
/// root when inside it, else as given.
pub(crate) fn junit_file_name(path: &[u8]) -> &[u8] {
    let top = FileSystem::instance().top_level_dir;
    if strings::has_prefix(path, top) {
        without_leading_path_separator(&path[top.len()..])
    } else {
        path
    }
}

/// One finished test as structured reporters see it.
pub(crate) struct TestCaseReport<'a> {
    /// Relative to the project root.
    pub file: &'a [u8],
    /// Enclosing named `describe` blocks, outermost first: (name, line).
    pub scopes: Vec<(&'a [u8], u32)>,
    pub name: &'a [u8],
    pub status: bun_test::Execution::Result,
    pub assertions: u32,
    pub elapsed_ns: u64,
    pub line_number: u32,
    pub failure: Option<TestFailure>,
}

/// Append `input` to `out`, dropping CSI sequences (`ESC '[' ... final`), so a
/// matcher message built with colour does not reach the report as SGR residue.
fn push_stripping_ansi(out: &mut Vec<u8>, input: &[u8]) {
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'[' {
            i += 2;
            while i < input.len() && !(0x40..=0x7e).contains(&input[i]) {
                i += 1;
            }
            if i < input.len() {
                i += 1;
            }
            continue;
        }
        out.push(input[i]);
        i += 1;
    }
}

// Remaining TODOs:
// - Add stdout/stderr to the JUnit report
// - Add timestamp field to the JUnit report
#[derive(Default)]
pub struct JunitReporter {
    pub(crate) contents: Vec<u8>,
    pub(crate) total_metrics: Metrics,
    pub(crate) offset_of_testsuites_value: usize,
    pub(crate) current_file: Box<[u8]>,
    pub(crate) file_start_ns: u64,
    pub(crate) properties_list_to_repeat_in_every_test_suite: Option<Box<[u8]>>,

    pub(crate) suite_stack: Vec<SuiteInfo>,
    pub(crate) current_depth: u32,

    pub(crate) hostname_value: Option<Box<[u8]>>,
}

#[derive(Default)]
pub struct SuiteInfo {
    pub name: Box<[u8]>,
    pub(crate) offset_of_attributes: usize,
    pub(crate) metrics: Metrics,
    pub(crate) is_file_suite: bool,
    pub(crate) started_ns: u64,
}

// We dupe the name unconditionally in begin_test_suite_with_line, so the
// unconditional drop is correct.

#[derive(Default, Clone, Copy)]
pub struct Metrics {
    pub(crate) test_cases: u32,
    pub(crate) assertions: u32,
    pub(crate) failures: u32,
    pub(crate) skipped: u32,
    pub(crate) elapsed_time: u64,
}

impl Metrics {
    fn add(&mut self, other: &Metrics) {
        self.test_cases += other.test_cases;
        self.assertions += other.assertions;
        self.failures += other.failures;
        self.skipped += other.skipped;
    }
}

impl JunitReporter {
    pub(crate) fn get_hostname(&mut self) -> Option<&[u8]> {
        if self.hostname_value.is_none() {
            #[cfg(windows)]
            {
                return None;
            }

            #[cfg(not(windows))]
            {
                const HOST_NAME_MAX: usize = 256;
                let mut name_buffer = [0u8; HOST_NAME_MAX];
                if bun_sys::posix::gethostname(&mut name_buffer).is_err() {
                    self.hostname_value = Some(Box::default());
                    return None;
                }
                let hostname = bun_core::slice_to_nul(&name_buffer);

                let mut arraylist_writer: Vec<u8> = Vec::new();
                if escape_xml(hostname, &mut arraylist_writer).is_err() {
                    self.hostname_value = Some(Box::default());
                    return None;
                }
                self.hostname_value = Some(arraylist_writer.into_boxed_slice());
            }
        }

        if let Some(hostname) = &self.hostname_value {
            if !hostname.is_empty() {
                return Some(hostname);
            }
        }
        None
    }

    pub(crate) fn init() -> Box<JunitReporter> {
        Box::new(JunitReporter::default())
    }
}

impl TestFailure {
    /// Fold in a `ZigException` that `print_error_instance_body` has already
    /// populated. A test can throw more than once (body, then `afterEach`);
    /// the first name/message win and every stack is appended.
    pub(crate) fn record(slot: &mut Option<TestFailure>, exception: &jsc::ZigException) {
        let failure = slot.get_or_insert_default();
        let name = exception.name.to_utf8();
        let raw_message = exception.message.to_utf8();
        let mut message = Vec::with_capacity(raw_message.slice().len());
        push_stripping_ansi(&mut message, raw_message.slice());

        let is_assertion = strings::has_prefix_comptime(&message, b"expect(")
            && (name.slice().is_empty() || strings::eql(name.slice(), b"Error"));

        if failure.name.is_empty() {
            if is_assertion {
                failure.name.extend_from_slice(b"AssertionError");
            } else {
                failure.name.extend_from_slice(name.slice());
            }
        }
        if failure.message.is_empty() {
            failure.message.extend_from_slice(&message);
        }

        let body = &mut failure.body;
        if !body.is_empty() {
            body.push(b'\n');
        }
        let header: &[u8] = if is_assertion {
            b"AssertionError"
        } else {
            name.slice()
        };
        match (header.is_empty(), message.is_empty()) {
            (true, true) => body.extend_from_slice(b"error"),
            (true, false) => body.extend_from_slice(&message),
            (false, true) => body.extend_from_slice(header),
            (false, false) => {
                body.extend_from_slice(header);
                body.extend_from_slice(b": ");
                body.extend_from_slice(&message);
            }
        }
        body.push(b'\n');
        let dir = FileSystem::instance().top_level_dir;
        for frame in exception.stack.frames() {
            let source_url = frame.source_url.to_utf8();
            let file = jsc::ZigStackFrame::relative_source_url(dir, source_url.slice());
            let func = frame.function_name.to_utf8();
            if file.is_empty() && func.slice().is_empty() {
                continue;
            }
            body.extend_from_slice(b"      at ");
            if !func.slice().is_empty() {
                let _ = write!(body, "{} (", frame.name_formatter(false));
            }
            let file_start = body.len();
            body.extend_from_slice(file);
            if cfg!(windows) {
                for b in &mut body[file_start..] {
                    if *b == b'\\' {
                        *b = b'/';
                    }
                }
            }
            let pos = frame.position;
            if pos.line.is_valid() && pos.column.is_valid() {
                let _ = write!(body, ":{}:{}", pos.line.one_based(), pos.column.one_based());
            } else if pos.line.is_valid() {
                let _ = write!(body, ":{}", pos.line.one_based());
            }
            if !func.slice().is_empty() {
                body.push(b')');
            }
            body.push(b'\n');
        }
    }

    /// VirtualMachine::on_print_error_zig_exception thunk.
    pub(crate) fn record_cb(ctx: *mut core::ffi::c_void, exception: &jsc::ZigException) {
        // SAFETY: `ctx` was set to `&mut CommandLineReporter.test_failure` by
        // `on_uncaught_exception` for the duration of a single
        // `run_error_handler` call; single-threaded, no other borrow live.
        let slot = unsafe { &mut *ctx.cast::<Option<TestFailure>>() };
        TestFailure::record(slot, exception);
    }
}

impl JunitReporter {
    fn generate_properties_list(&mut self) -> crate::Result<()> {
        struct PropertiesList<'a> {
            ci: &'a [u8],
            commit: &'a [u8],
        }

        let ci_buf: Vec<u8>;
        let ci: &[u8] = 'brk: {
            if let Some(github_run_id) = env_var::GITHUB_RUN_ID.get() {
                if let Some(github_server_url) = env_var::GITHUB_SERVER_URL.get() {
                    if let Some(github_repository) = env_var::GITHUB_REPOSITORY.get() {
                        if !github_run_id.is_empty()
                            && !github_server_url.is_empty()
                            && !github_repository.is_empty()
                        {
                            let mut v = Vec::new();
                            // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
                            let _ = write!(
                                &mut v,
                                "{}/{}/actions/runs/{}",
                                bstr::BStr::new(github_server_url),
                                bstr::BStr::new(github_repository),
                                bstr::BStr::new(github_run_id)
                            );
                            ci_buf = v;
                            break 'brk &ci_buf[..];
                        }
                    }
                }
            }

            if let Some(ci_job_url) = env_var::CI_JOB_URL.get() {
                if !ci_job_url.is_empty() {
                    break 'brk ci_job_url;
                }
            }

            break 'brk b"";
        };

        let commit: &[u8] = 'brk: {
            if let Some(github_sha) = env_var::GITHUB_SHA.get() {
                if !github_sha.is_empty() {
                    break 'brk github_sha;
                }
            }

            if let Some(sha) = env_var::CI_COMMIT_SHA.get() {
                if !sha.is_empty() {
                    break 'brk sha;
                }
            }

            if let Some(git_sha) = env_var::GIT_SHA.get() {
                if !git_sha.is_empty() {
                    break 'brk git_sha;
                }
            }

            break 'brk b"";
        };

        let properties = PropertiesList { ci, commit };

        if properties.ci.is_empty() && properties.commit.is_empty() {
            self.properties_list_to_repeat_in_every_test_suite = Some(Box::default());
            return Ok(());
        }

        let mut buffer: Vec<u8> = Vec::new();
        let writer = &mut buffer;

        writer.write_all(b"    <properties>\n")?;

        if !properties.ci.is_empty() {
            writer.write_all(b"      <property name=\"ci\" value=\"")?;
            escape_xml(properties.ci, writer)?;
            writer.write_all(b"\" />\n")?;
        }
        if !properties.commit.is_empty() {
            writer.write_all(b"      <property name=\"commit\" value=\"")?;
            escape_xml(properties.commit, writer)?;
            writer.write_all(b"\" />\n")?;
        }

        writer.write_all(b"    </properties>\n")?;

        self.properties_list_to_repeat_in_every_test_suite = Some(buffer.into_boxed_slice());
        Ok(())
    }

    fn get_indent(depth: u32) -> &'static [u8] {
        const SPACES: &[u8] =
            b"                                                                                ";
        const INDENT_SIZE: u32 = 2;
        let total_spaces = (depth + 1) * INDENT_SIZE;
        &SPACES[0..(total_spaces as usize).min(SPACES.len())]
    }

    fn start_document(&mut self) {
        if self.contents.is_empty() {
            self.contents
                .extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
            self.contents
                .extend_from_slice(b"<testsuites name=\"bun test\" ");
            self.offset_of_testsuites_value = self.contents.len();
            self.contents.extend_from_slice(b">\n");
        }
    }

    pub(crate) fn begin_test_suite(&mut self, name: &[u8]) -> crate::Result<()> {
        self.begin_test_suite_with_line(name, 0, true)
    }

    pub(crate) fn begin_test_suite_with_line(
        &mut self,
        name: &[u8],
        line_number: u32,
        is_file_suite: bool,
    ) -> crate::Result<()> {
        self.start_document();

        let indent = Self::get_indent(self.current_depth);
        self.contents.extend_from_slice(indent);
        self.contents.extend_from_slice(b"<testsuite name=\"");
        escape_xml(name, &mut self.contents)?;
        self.contents.extend_from_slice(b"\"");

        if is_file_suite {
            self.contents.extend_from_slice(b" file=\"");
            escape_xml(name, &mut self.contents)?;
            self.contents.extend_from_slice(b"\"");
        } else if !self.current_file.is_empty() {
            self.contents.extend_from_slice(b" file=\"");
            // Reshaped for borrowck — clone current_file slice before mutable borrow of contents
            let cf = self.current_file.clone();
            escape_xml(&cf, &mut self.contents)?;
            self.contents.extend_from_slice(b"\"");
        }

        if line_number > 0 {
            // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
            let _ = write!(&mut self.contents, " line=\"{}\"", line_number);
        }

        self.contents.extend_from_slice(b" ");
        let offset_of_attributes = self.contents.len();
        self.contents.extend_from_slice(b">\n");

        if is_file_suite {
            if self.properties_list_to_repeat_in_every_test_suite.is_none() {
                self.generate_properties_list()?;
            }

            if let Some(properties_list) = &self.properties_list_to_repeat_in_every_test_suite {
                if !properties_list.is_empty() {
                    self.contents.extend_from_slice(properties_list);
                }
            }
        }

        self.suite_stack.push(SuiteInfo {
            name: Box::<[u8]>::from(name),
            offset_of_attributes,
            metrics: Metrics::default(),
            is_file_suite,
            started_ns: if is_file_suite { self.file_start_ns } else { 0 },
        });

        self.current_depth += 1;
        if is_file_suite {
            self.current_file = Box::<[u8]>::from(name);
        }
        Ok(())
    }

    /// Close every suite still open for the current file. The file suite's
    /// `time` is `elapsed_ns` when the caller measured it (the `--parallel`
    /// coordinator replaying a worker's file), else now − `file_start_ns`.
    pub(crate) fn end_file(&mut self, elapsed_ns: Option<u64>) -> crate::Result<()> {
        while !self.suite_stack.is_empty() {
            self.end_test_suite(elapsed_ns)?;
        }
        self.current_file = Box::default();
        Ok(())
    }

    fn end_test_suite(&mut self, file_elapsed_ns: Option<u64>) -> crate::Result<()> {
        if self.suite_stack.is_empty() {
            return Ok(());
        }

        self.current_depth -= 1;
        let suite_info = self.suite_stack.swap_remove(self.suite_stack.len() - 1);

        let elapsed_time_seconds = if suite_info.is_file_suite
            && let Some(ns) = file_elapsed_ns
        {
            ns as f64 / bun::time::NS_PER_S as f64
        } else if suite_info.is_file_suite && suite_info.started_ns > 0 {
            bun::Timespec::now(bun::TimespecMockMode::ForceRealTime)
                .ns()
                .saturating_sub(suite_info.started_ns) as f64
                / bun::time::NS_PER_S as f64
        } else {
            suite_info.metrics.elapsed_time as f64 / bun::time::MS_PER_S as f64
        };

        // Reshaped for borrowck — get hostname first
        let hostname = self.get_hostname().map(|h| h.to_vec()).unwrap_or_default();

        // Insert the summary attributes
        let mut summary = Vec::new();
        {
            // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
            let _ = write!(
                &mut summary,
                "tests=\"{}\" assertions=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{}\" hostname=\"{}\"",
                suite_info.metrics.test_cases,
                suite_info.metrics.assertions,
                suite_info.metrics.failures,
                suite_info.metrics.skipped,
                elapsed_time_seconds,
                bstr::BStr::new(&hostname),
            );
        }

        self.contents.splice(
            suite_info.offset_of_attributes..suite_info.offset_of_attributes,
            summary.iter().copied(),
        );

        let indent = Self::get_indent(self.current_depth);
        self.contents.extend_from_slice(indent);
        self.contents.extend_from_slice(b"</testsuite>\n");

        if !self.suite_stack.is_empty() {
            let last = self.suite_stack.len() - 1;
            self.suite_stack[last].metrics.add(&suite_info.metrics);
        } else {
            self.total_metrics.add(&suite_info.metrics);
        }
        Ok(())
    }

    /// Open/close `<testsuite>` elements so the stack matches `t.file` and
    /// `t.scopes`, then write the `<testcase>`.
    pub(crate) fn record_test_case(&mut self, t: &TestCaseReport<'_>) -> crate::Result<()> {
        if !strings::eql(&self.current_file, t.file) {
            while let Some(top) = self.suite_stack.last()
                && !top.is_file_suite
            {
                self.end_test_suite(None)?;
            }
            if !self.current_file.is_empty() {
                self.end_test_suite(None)?;
            }
            self.begin_test_suite(t.file)?;
        }

        // Keep the longest prefix of open describe suites that matches
        // `t.scopes`; close the rest, then open what is missing.
        let open = &self.suite_stack[1..];
        let keep = open
            .iter()
            .zip(&t.scopes)
            .take_while(|(suite, (name, _))| strings::eql(&suite.name, name))
            .count();
        for _ in keep..open.len() {
            self.end_test_suite(None)?;
        }
        for &(name, line) in &t.scopes[keep..] {
            self.begin_test_suite_with_line(name, line, false)?;
        }

        // Innermost scope first, matching what the serial reporter always did.
        let mut class_name: Vec<u8> = Vec::new();
        for (i, &(name, _)) in t.scopes.iter().rev().enumerate() {
            if i > 0 {
                class_name.extend_from_slice(b" > ");
            }
            class_name.extend_from_slice(name);
        }

        self.write_test_case(t, &class_name)
    }

    fn write_test_case(&mut self, t: &TestCaseReport<'_>, class_name: &[u8]) -> crate::Result<()> {
        let TestCaseReport {
            file,
            name,
            status,
            assertions,
            elapsed_ns,
            line_number,
            ..
        } = *t;
        let failure = t.failure.as_ref();
        // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
        let elapsed_ns_f64: f64 = elapsed_ns as f64;
        let elapsed_ms = elapsed_ns_f64 / bun::time::NS_PER_MS as f64;

        if !self.suite_stack.is_empty() {
            let last = self.suite_stack.len() - 1;
            let current_suite = &mut self.suite_stack[last];
            current_suite.metrics.elapsed_time = current_suite
                .metrics
                .elapsed_time
                .saturating_add(elapsed_ms as u64);
            current_suite.metrics.test_cases += 1;
            current_suite.metrics.assertions += assertions;
        }

        let indent = Self::get_indent(self.current_depth);
        self.contents.extend_from_slice(indent);
        self.contents.extend_from_slice(b"<testcase");
        self.contents.extend_from_slice(b" name=\"");
        escape_xml(name, &mut self.contents)?;
        self.contents.extend_from_slice(b"\" classname=\"");
        escape_xml(class_name, &mut self.contents)?;
        self.contents.extend_from_slice(b"\"");

        let elapsed_seconds = elapsed_ms / bun::time::MS_PER_S as f64;
        let _ = write!(
            &mut self.contents,
            " time=\"{}\"",
            bun_fmt::trimmed_precision::<6>(elapsed_seconds)
        );

        self.contents.extend_from_slice(b" file=\"");
        escape_xml(file, &mut self.contents)?;
        self.contents.extend_from_slice(b"\"");

        if line_number > 0 {
            let _ = write!(&mut self.contents, " line=\"{}\"", line_number);
        }

        let _ = write!(&mut self.contents, " assertions=\"{}\"", assertions);

        use bun_test::Execution::Result as R;
        match status {
            R::Pass => {
                self.contents.extend_from_slice(b" />\n");
            }
            R::Fail => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let type_name: &[u8] = failure
                    .as_ref()
                    .map(|f| f.name.as_slice())
                    .filter(|n| !n.is_empty())
                    .unwrap_or(b"Error");
                self.contents.extend_from_slice(b"  <failure type=\"");
                escape_xml(type_name, &mut self.contents)?;
                self.contents.extend_from_slice(b"\"");
                if let Some(f) = failure.as_ref() {
                    if !f.message.is_empty() {
                        self.contents.extend_from_slice(b" message=\"");
                        escape_xml(&f.message, &mut self.contents)?;
                        self.contents.extend_from_slice(b"\"");
                    }
                }
                match failure.as_ref().filter(|f| !f.body.is_empty()) {
                    Some(f) => {
                        self.contents.extend_from_slice(b">");
                        escape_xml(&f.body, &mut self.contents)?;
                        self.contents.extend_from_slice(b"</failure>\n");
                    }
                    None => {
                        self.contents.extend_from_slice(b" />\n");
                    }
                }
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseFailingTestPassed => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"test marked with .failing() did not throw\" type=\"AssertionError\"/>"
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseExpectedAssertionCount => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"Expected more assertions, but only received {}\" type=\"AssertionError\"/>",
                    assertions
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseTodoPassed => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"TODO passed\" type=\"AssertionError\"/>"
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseExpectedHasAssertions => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                let _ = writeln!(
                    &mut self.contents,
                    "  <failure message=\"Expected to have assertions, but none were run\" type=\"AssertionError\"/>"
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::SkippedBecauseLabel | R::Skip => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.skipped += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"  <skipped />\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::Todo => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.skipped += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                self.contents
                    .extend_from_slice(b"  <skipped message=\"TODO\" />\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::FailBecauseTimeout
            | R::FailBecauseTimeoutWithDoneCallback
            | R::FailBecauseHookTimeout
            | R::FailBecauseHookTimeoutWithDoneCallback => {
                if !self.suite_stack.is_empty() {
                    let last = self.suite_stack.len() - 1;
                    self.suite_stack[last].metrics.failures += 1;
                }
                self.contents.extend_from_slice(b">\n");
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(
                    b"  <failure type=\"TimeoutError\" message=\"test timed out\" />\n",
                );
                self.contents.extend_from_slice(indent);
                self.contents.extend_from_slice(b"</testcase>\n");
            }
            R::Pending => unreachable!(),
        }
        Ok(())
    }

    pub(crate) fn write_to_file(&mut self, path: &[u8]) -> crate::Result<()> {
        if self.contents.is_empty() {
            return Ok(());
        }

        self.end_file(None)?;

        {
            let metrics = self.total_metrics;
            let elapsed_time = (bun::time::nano_timestamp() - bun::start_time()) as f64
                / bun::time::NS_PER_S as f64;
            let mut summary = Vec::new();
            {
                // Std::io::Write removed; bun_io::Write (top-level) provides write_fmt.
                let _ = write!(
                    &mut summary,
                    "tests=\"{}\" assertions=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{}\"",
                    metrics.test_cases,
                    metrics.assertions,
                    metrics.failures,
                    metrics.skipped,
                    elapsed_time,
                );
            }
            self.contents.splice(
                self.offset_of_testsuites_value..self.offset_of_testsuites_value,
                summary.iter().copied(),
            );
            self.contents.extend_from_slice(b"</testsuites>\n");
        }

        let mut junit_path_buf = bun_paths::path_buffer_pool::get();

        junit_path_buf[..path.len()].copy_from_slice(path);
        junit_path_buf[path.len()] = 0;

        // SAFETY: junit_path_buf[path.len()] == 0 written above
        let zpath = bun_core::ZStr::from_buf(&junit_path_buf[..], path.len());
        match File::openat(
            Fd::cwd(),
            zpath,
            bun_sys::O::WRONLY | bun_sys::O::CREAT | bun_sys::O::TRUNC,
            0o664,
        ) {
            bun_sys::Result::Err(err) => {
                Output::err(
                    crate::Error::JUnitReportFailed,
                    "Failed to write JUnit report to {}\n{}",
                    (bstr::BStr::new(path), err),
                );
            }
            bun_sys::Result::Ok(fd) => match File::write_all(&fd, &self.contents) {
                bun_sys::Result::Ok(()) => {}
                bun_sys::Result::Err(err) => {
                    Output::err(
                        crate::Error::JUnitReportFailed,
                        "Failed to write JUnit report to {}\n{}",
                        (bstr::BStr::new(path), err),
                    );
                }
            },
        }
        Ok(())
    }
}

/// Drain the event loop after a file's tests finish, like a node process
/// would before exiting; the vendored-node-test runner opts in via
/// `BUN_TEST_DRAIN_EVENT_LOOP=1` so mustCall()-style exit checks see
/// completed async work. Off by default: bun suites keep exit-after-tests.
fn should_drain_event_loop() -> bool {
    env_var::BUN_TEST_DRAIN_EVENT_LOOP.get().unwrap_or(false)
}

/// jest and vitest never run a test file's `process.on('exit')` listeners; node's test harness asserts from them.
pub(crate) fn skip_exit_listeners(reporter: &CommandLineReporter) -> bool {
    !(reporter.jest.node_test_used || should_drain_event_loop())
}

pub struct CommandLineReporter {
    // `TestRunner<'a>` borrows `TestOptions`/regex from the CLI ctx; the
    // reporter is held in a `Box` local to `TestCommand::exec` which never
    // returns before process exit, so `'static` is sound here. Revisit if the
    // reporter ever becomes scoped.
    pub(crate) jest: TestRunner<'static>,
    pub(crate) repeat_count: u32,
    /// Interior-mut: written from `BunTestRoot::on_before_print` via `&CommandLineReporter`
    pub(crate) last_printed_dot: core::cell::Cell<bool>,


    /// Errors thrown by the test now running, captured while a reporter
    /// that attaches them to the test case (JUnit) is on. Taken by
    /// `handle_test_completed`.
    pub(crate) test_failure: Option<TestFailure>,

    pub(crate) failures_to_repeat_buf: Vec<u8>,
    pub(crate) skips_to_repeat_buf: Vec<u8>,
    pub(crate) todos_to_repeat_buf: Vec<u8>,

    pub(crate) reporters: ReportersConfig,


}

#[derive(Default)]
pub struct ReportersConfig {
    pub(crate) dots: bool,
    pub(crate) only_failures: bool,
    pub(crate) junit: Option<Box<JunitReporter>>,
}

impl CommandLineReporter {
    fn print_test_line<const DIM: bool>(
        status: bun_test::Execution::Result,
        sequence: &mut bun_test::Execution::ExecutionSequence,
        test_entry: &mut bun_test::ExecutionEntry,
        elapsed_ns: u64,
        writer: &mut impl bun_io::Write,
    ) {
        let initial_retry_count = test_entry.retry_count;
        let attempts = (initial_retry_count - sequence.remaining_retry_count) + 1;
        let initial_repeat_count = test_entry.repeat_count;
        let repeats = (initial_repeat_count - sequence.remaining_repeat_count) + 1;
        let mut scopes_stack: BoundedArray<*const bun_test::DescribeScope, 64> =
            BoundedArray::default();
        let mut parent_: Option<*const bun_test::DescribeScope> =
            test_entry.base.parent.map(|p| p.cast_const());

        while let Some(scope) = parent_ {
            if scopes_stack.push(scope).is_err() {
                break;
            }
            // SAFETY: scope is a live DescribeScope pointer kept alive for the test run
            parent_ = unsafe { (*scope).base.parent.map(|p| p.cast_const()) };
        }

        let scopes: &[*const bun_test::DescribeScope] = scopes_stack.as_slice();
        let display_label: &[u8] = test_entry.base.name.as_deref().unwrap_or(b"(unnamed)");

        // Quieter output when claude code is in use.
        if !Output::is_ai_agent() || !status.is_pass(bun_test::PendingMode::PendingIsFail) {
            // `color_code`/`line_color_code` literals are inlined at use sites
            // below via `if DIM { ... } else { ... }` to avoid runtime `format!`.

            // `switch (Output.enable_ansi_colors_stderr) { inline else => |_| ... }` — the
            // captured bool was unused except for monomorphization; collapsed to runtime.
            match status {
                bun_test::Execution::Result::FailBecauseExpectedAssertionCount => {
                    // not sent to writer so it doesn't get printed twice
                    let expected_count =
                        if let bun_test::ExpectAssertions::Exact(n) = sequence.expect_assertions {
                            n
                        } else {
                            12345
                        };
                    Output::err(
                        crate::Error::AssertionError,
                        "expected <green>{} assertion{}<r>, but test ended with <red>{} assertion{}<r>\n",
                        (
                            expected_count,
                            if expected_count == 1 { "" } else { "s" },
                            sequence.expect_call_count,
                            if sequence.expect_call_count == 1 {
                                ""
                            } else {
                                "s"
                            },
                        ),
                    );
                    Output::flush();
                }
                bun_test::Execution::Result::FailBecauseExpectedHasAssertions => {
                    Output::err(
                        crate::Error::AssertionError,
                        "received <red>0 assertions<r>, but expected <green>at least one assertion<r> to be called\n",
                        (),
                    );
                    Output::flush();
                }
                bun_test::Execution::Result::FailBecauseTimeout
                | bun_test::Execution::Result::FailBecauseHookTimeout
                | bun_test::Execution::Result::FailBecauseTimeoutWithDoneCallback
                | bun_test::Execution::Result::FailBecauseHookTimeoutWithDoneCallback => {
                    if Output::is_github_action() {
                        Output::print_error(format_args!(
                            "::error title=error: Test \"{}\" timed out after {}ms::\n",
                            bun_fmt::github_action_property(display_label),
                            test_entry.timeout
                        ));
                        Output::flush();
                    }
                }
                _ => {}
            }

            if Output::enable_ansi_colors_stderr() {
                for i in 0..scopes.len() {
                    let index = (scopes.len() - 1) - i;
                    let scope = scopes[index];
                    // SAFETY: scope is alive for duration of test run
                    let name: &[u8] = unsafe { (*scope).base.name.as_deref() }.unwrap_or(b"");
                    if name.is_empty() {
                        continue;
                    }
                    let _ = writer.write_all(b" ");

                    let prefix = if DIM {
                        Output::pretty_fmt::<true>("<r><d>")
                    } else {
                        Output::pretty_fmt::<true>("<r>")
                    };
                    let _ = writer.write_all(&prefix);
                    let _ = writer.write_all(name);
                    let _ = writer.write_all(&Output::pretty_fmt::<true>("<d>"));
                    let _ = writer.write_all(b" >");
                }
            } else {
                for i in 0..scopes.len() {
                    let index = (scopes.len() - 1) - i;
                    let scope = scopes[index];
                    // SAFETY: scope is alive for duration of test run
                    let name: &[u8] = unsafe { (*scope).base.name.as_deref() }.unwrap_or(b"");
                    if name.is_empty() {
                        continue;
                    }
                    let _ = writer.write_all(b" ");
                    let _ = writer.write_all(name);
                    let _ = writer.write_all(b" >");
                }
            }

            if Output::enable_ansi_colors_stderr() {
                let label_prefix = if DIM {
                    Output::pretty_fmt::<true>("<r><d> ")
                } else {
                    Output::pretty_fmt::<true>("<r><b> ")
                };
                let _ = writer.write_all(&label_prefix);
                let _ = writer.write_all(display_label);
                let _ = writer.write_all(&Output::pretty_fmt::<true>("<r>"));
            } else {
                let _ = writer.write_all(b" ");
                let _ = writer.write_all(display_label);
            }

            // Print attempt count if test was retried (attempts > 1)
            if attempts > 1 {
                let _ = bun_core::write_pretty!(
                    writer,
                    Output::enable_ansi_colors_stderr(),
                    " <d>(attempt {d})<r>",
                    attempts,
                );
            }

            // Print repeat count if test failed on a repeat (repeats > 1)
            if repeats > 1 {
                let _ = bun_core::write_pretty!(
                    writer,
                    Output::enable_ansi_colors_stderr(),
                    " <d>(run {d})<r>",
                    repeats,
                );
            }

            if elapsed_ns > (bun::time::NS_PER_US * 10) {
                let _ = write!(
                    writer,
                    " {}",
                    Output::ElapsedFormatter {
                        colors: Output::enable_ansi_colors_stderr(),
                        duration_ns: elapsed_ns,
                    }
                );
            }

            let _ = writer.write_all(b"\n");

            let colors = Output::enable_ansi_colors_stderr();
            use bun_test::Execution::Result as R;
            match status {
                R::Pending | R::Pass | R::Skip | R::SkippedBecauseLabel | R::Todo | R::Fail => {}

                R::FailBecauseFailingTestPassed => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test is marked as failing but it passed.<r> <d>Remove `.failing` if tested behavior now works<r>\n"
                    );
                }
                R::FailBecauseTodoPassed => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test is marked as todo but passes.<r> <d>Remove `.todo` if tested behavior now works<r>\n"
                    );
                }
                R::FailBecauseExpectedAssertionCount | R::FailBecauseExpectedHasAssertions => {} // printed above
                R::FailBecauseTimeout => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test timed out after {d}ms.<r>\n",
                        test_entry.timeout
                    );
                }
                R::FailBecauseHookTimeout => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>a beforeEach/afterEach hook timed out for this test.<r>\n"
                    );
                }
                R::FailBecauseTimeoutWithDoneCallback => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>this test timed out after {d}ms, before its done callback was called.<r> <d>If a done callback was not intended, remove the last parameter from the test callback function<r>\n",
                        test_entry.timeout
                    );
                }
                R::FailBecauseHookTimeoutWithDoneCallback => {
                    let _ = bun_core::write_pretty!(
                        writer,
                        colors,
                        "  <d>^<r> <red>a beforeEach/afterEach hook timed out before its done callback was called.<r> <d>If a done callback was not intended, remove the last parameter from the hook callback function<r>\n"
                    );
                }
            }
        }
    }

    /// Everything a structured reporter needs about one finished test. Built
    /// once in `handle_test_completed`; the serial runner hands it straight
    /// to `JunitReporter::record_test_case`, a `--parallel` worker encodes it
    /// on its `TestDone` frame and the coordinator replays it.
    fn test_case_report<'a>(
        status: bun_test::Execution::Result,
        buntest: &bun_test::BunTest,
        sequence: &bun_test::Execution::ExecutionSequence,
        test_entry: &'a bun_test::ExecutionEntry,
        elapsed_ns: u64,
        failure: Option<TestFailure>,
    ) -> TestCaseReport<'a> {
        let file: &[u8] = if let Some(runner) = jest::Jest::runner() {
            runner.files.items_source()[buntest.file_id as usize]
                .path
                .text
        } else {
            b""
        };
        let file = junit_file_name(file);

        // Innermost first while walking up; reversed below.
        let mut scopes: Vec<(&'a [u8], u32)> = Vec::new();
        let mut parent = test_entry.base.parent.map(|p| p.cast_const());
        while let Some(scope) = parent {
            if scopes.len() == 64 {
                break;
            }
            // SAFETY: describe scopes outlive the file's test run.
            let scope: &'a bun_test::DescribeScope = unsafe { &*scope };
            if let Some(name) = scope.base.name.as_deref()
                && !name.is_empty()
            {
                scopes.push((name, scope.base.line_no));
            }
            parent = scope.base.parent.map(|p| p.cast_const());
        }
        scopes.reverse();

        TestCaseReport {
            file,
            scopes,
            name: test_entry.base.name.as_deref().unwrap_or(b"(unnamed)"),
            status,
            assertions: sequence.expect_call_count,
            elapsed_ns,
            line_number: test_entry.base.line_no,
            failure,
        }
    }

    #[inline]
    pub(crate) fn summary(&mut self) -> &mut Summary {
        &mut self.jest.summary
    }

    pub(crate) fn handle_test_completed(
        buntest: &mut bun_test::BunTest,
        sequence: &mut bun_test::Execution::ExecutionSequence,
        test_entry: &mut bun_test::ExecutionEntry,
        elapsed_ns: u64,
    ) {
        let mut output_buf: Vec<u8> = Vec::new();

        let initial_length = output_buf.len();
        let writer = &mut output_buf;

        let result = sequence.result;
        if result != bun_test::Execution::Result::SkippedBecauseLabel {
            // SAFETY: `BunTest.reporter` is `NonNull<CommandLineReporter>` with write
            // provenance from `enter_file`'s `&mut`; single-threaded; reporter outlives
            // every BunTest. Scoped to this block so the SharedReadOnly tag is dead
            // before the `&mut` derived from the same `NonNull` below
            // (stacked-borrows hygiene).
            let reporter_ref: Option<&CommandLineReporter> =
                buntest.reporter.map(|p| unsafe { &*p.as_ptr() });
            let basic = result.basic_result();
            let dots_branch = reporter_ref.is_some_and(|r| r.reporters.dots)
                && matches!(
                    basic,
                    bun_test::BasicResult::Pass
                        | bun_test::BasicResult::Skip
                        | bun_test::BasicResult::Todo
                        | bun_test::BasicResult::Pending
                );
            if dots_branch {
                let colors = Output::enable_ansi_colors_stderr();
                match basic {
                    bun_test::BasicResult::Pass => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><green>.<r>");
                    }
                    bun_test::BasicResult::Skip => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><yellow>.<d>");
                    }
                    bun_test::BasicResult::Todo => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><magenta>.<r>");
                    }
                    bun_test::BasicResult::Pending => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><d>.<r>");
                    }
                    bun_test::BasicResult::Fail => {
                        let _ = bun_core::write_pretty!(writer, colors, "<r><red>.<r>");
                    }
                }
                reporter_ref.unwrap().last_printed_dot.set(true);
            } else if basic != bun_test::BasicResult::Fail
                && reporter_ref.is_some_and(|r| r.reporters.only_failures)
            {
                // when using --only-failures, only print failures
            } else {
                buntest.bun_test_root.on_before_print();

                if Output::enable_ansi_colors_stderr() {
                    let _ = writer.write_all(&fmt_status_text_line(result, true));
                } else {
                    let _ = writer.write_all(&fmt_status_text_line(result, false));
                }
                let dim = match basic {
                    bun_test::BasicResult::Todo => {
                        if let Some(runner) = jest::Jest::runner() {
                            !runner.run_todo
                        } else {
                            true
                        }
                    }
                    bun_test::BasicResult::Skip | bun_test::BasicResult::Pending => true,
                    bun_test::BasicResult::Pass | bun_test::BasicResult::Fail => false,
                };
                if dim {
                    Self::print_test_line::<true>(result, sequence, test_entry, elapsed_ns, writer);
                } else {
                    Self::print_test_line::<false>(
                        result, sequence, test_entry, elapsed_ns, writer,
                    );
                }
            }
        }
        let formatted_line = &output_buf[initial_length..];

        let Some(this) = buntest.reporter else {
            let _ = Output::error_writer().write_all(formatted_line);
            return;
        };
        // SAFETY: `BunTest.reporter` is `NonNull<CommandLineReporter>` with write
        // provenance from `enter_file`'s `&mut`; single-threaded test runner,
        // sole writer for the duration of this completion callback, and the
        // shared borrow above is dead.
        let this: &mut CommandLineReporter = unsafe { &mut *this.as_ptr() };

        // Under `--parallel` the flag is forwarded to workers, whose records
        // the coordinator replays into its own `JunitReporter`.
        let report = this.jest.test_options.reporters.junit.then(|| {
            let failure = this.test_failure.take();
            Self::test_case_report(result, buntest, sequence, test_entry, elapsed_ns, failure)
        });
        let _ = Output::error_writer().write_all(formatted_line);
        if let (Some(junit), Some(report)) = (this.reporters.junit.as_mut(), &report) {
            junit.record_test_case(report).expect("oom");
        }

        if !this.reporters.dots && !this.reporters.only_failures {
            match sequence.result.basic_result() {
                bun_test::BasicResult::Skip => this
                    .skips_to_repeat_buf
                    .extend_from_slice(&output_buf[initial_length..]),
                bun_test::BasicResult::Todo => this
                    .todos_to_repeat_buf
                    .extend_from_slice(&output_buf[initial_length..]),
                bun_test::BasicResult::Fail => this
                    .failures_to_repeat_buf
                    .extend_from_slice(&output_buf[initial_length..]),
                bun_test::BasicResult::Pass | bun_test::BasicResult::Pending => {}
            }
        }

        use bun_test::Execution::Result as R;
        match sequence.result {
            R::Pending => {}
            R::Pass => this.summary().pass += 1,
            R::Skip => this.summary().skip += 1,
            R::Todo => this.summary().todo += 1,
            R::SkippedBecauseLabel => this.summary().skipped_because_label += 1,

            R::Fail
            | R::FailBecauseFailingTestPassed
            | R::FailBecauseTodoPassed
            | R::FailBecauseExpectedHasAssertions
            | R::FailBecauseExpectedAssertionCount
            | R::FailBecauseTimeout
            | R::FailBecauseTimeoutWithDoneCallback
            | R::FailBecauseHookTimeout
            | R::FailBecauseHookTimeoutWithDoneCallback => {
                this.summary().fail += 1;

                if this.summary().fail == this.jest.bail {
                    this.print_summary();
                    pretty_error!(
                        "\nBailed out after {} failure{}<r>\n",
                        this.jest.bail,
                        if this.jest.bail == 1 { "" } else { "s" }
                    );
                    Output::flush();
                    this.write_junit_report_if_needed();
                    Global::exit(1);
                }
            }
        }
        this.summary().expectations = this
            .summary()
            .expectations
            .saturating_add(sequence.expect_call_count);
    }

    pub(crate) fn print_summary(&mut self) {
        let summary_ = self.summary();
        let tests = summary_.fail + summary_.pass + summary_.skip + summary_.todo;
        let files = summary_.files;

        pretty_error!(
            "Ran {} test{} across {} file{}. ",
            tests,
            if tests == 1 { "" } else { "s" },
            files,
            if files == 1 { "" } else { "s" }
        );

        Output::print_start_end(bun::start_time(), bun::time::nano_timestamp());
    }



    /// Writes the JUnit reporter output file if a JUnit reporter is active and
    /// an outfile path was configured. This must be called before any early exit
    /// (e.g. bail) so that the report is not lost.
    pub(crate) fn write_junit_report_if_needed(&mut self) {
        if let Some(junit) = self.reporters.junit.as_mut() {
            if let Some(outfile) = self.jest.test_options.reporter_outfile.as_deref() {
                let _ = junit.write_to_file(outfile);
            }
        }
    }

    /// This process's coverage, one `Report` per instrumented file, sorted by
    /// path, with `coveragePathIgnorePatterns` applied.
    pub(crate) fn for_each_coverage_report(
        vm: &mut VirtualMachine,
        opts: &CodeCoverageOptions,
        mut each: impl FnMut(CodeCoverageReport<'_>),
    ) {
        let Some(map) = ByteRangeMapping::map() else {
            return;
        };
        // SAFETY: thread-local Box pinned for the thread; sole `&mut` for the
        // collection loop below (single-threaded CLI report path).
        let map = unsafe { &mut *map.as_ptr() };
        let relative_dir = FileSystem::get().top_level_dir;
        let mut byte_ranges: Vec<&mut ByteRangeMapping> = Vec::with_capacity(map.len());
        for entry in map.values_mut() {
            if !coverage::is_ignored(opts, relative_dir, entry.source_url.slice()) {
                byte_ranges.push(entry);
            }
        }
        index_sort::sort_slice_by(&mut byte_ranges, coverage::is_less_than_cmp);

        for entry in byte_ranges {
            if let Some(report) =
                CodeCoverageReport::generate(vm.global(), entry, opts.ignore_sourcemap)
            {
                each(report);
            }
        }
    }

    pub(crate) fn generate_code_coverage(
        &mut self,
        vm: &mut VirtualMachine,
        opts: &mut CodeCoverageOptions,
    ) {
        let _trace = bun::perf::trace("TestCommand.printCodeCoverage");
        if ByteRangeMapping::map().is_none_or(|m| {
            // SAFETY: see `for_each_coverage_report`.
            unsafe { m.as_ref() }.is_empty()
        }) {
            return;
        }
        let mut reports: Vec<CodeCoverageReport<'static>> = Vec::new();
        Self::for_each_coverage_report(vm, opts, |report| reports.push(report.into_owned()));
        if let Err(err) = print_coverage_reports(opts, &reports) {
            Output::err(err, "Failed to write lcov.info", ());
            Global::exit(1);
        }
    }
}

/// Write the `--coverage` text table to stderr and/or `lcov.info` for
/// `reports` (sorted by path), and set `opts.fractions.failing`. The serial
/// runner passes this process's reports; the `--parallel` coordinator passes
/// reports merged from every worker. Errors only from writing `lcov.info`.
pub(crate) fn print_coverage_reports(
    opts: &mut CodeCoverageOptions,
    reports: &[CodeCoverageReport<'_>],
) -> bun_sys::Result<()> {
    if Output::enable_ansi_colors_stderr() {
        print_coverage_reports_::<true>(opts, reports)
    } else {
        print_coverage_reports_::<false>(opts, reports)
    }
}

fn print_coverage_reports_<const COLORS: bool>(
    opts: &mut CodeCoverageOptions,
    reports: &[CodeCoverageReport<'_>],
) -> bun_sys::Result<()> {
    let relative_dir = FileSystem::get().top_level_dir;
    let thresholds = opts.fractions;

    let fractions: Vec<Fraction> = reports.iter().map(|r| r.fraction(&thresholds)).collect();
    let failing = fractions.iter().any(|f| f.failing);
    opts.fractions.failing = failing;

    if opts.reporters.text {
        print_coverage_table::<COLORS>(relative_dir, &thresholds, reports, &fractions, failing);
    }
    if opts.reporters.lcov {
        write_lcov_report(opts, relative_dir, reports)?;
    }
    Ok(())
}

fn print_coverage_table<const COLORS: bool>(
    relative_dir: &[u8],
    thresholds: &Fraction,
    reports: &[CodeCoverageReport<'_>],
    fractions: &[Fraction],
    failing: bool,
) {
    use coverage::text;
    let thresholds = *thresholds;
    let max_filepath_length = reports
        .iter()
        .map(|r| resolve_path::relative(relative_dir, &r.source_url).len())
        .fold(b"All files".len(), usize::max);

    let zero = Fraction {
        functions: 0.0,
        lines: 0.0,
        stmts: 0.0,
        failing: false,
    };
    let mut avg = zero;
    for f in fractions {
        avg.functions += f.functions;
        avg.lines += f.lines;
        avg.stmts += f.stmts;
    }
    let avg_thresholds = if fractions.is_empty() {
        zero
    } else {
        let n = fractions.len() as f64;
        avg.functions /= n;
        avg.lines /= n;
        avg.stmts /= n;
        thresholds
    };

    // Writes below target a Vec and cannot fail.
    let mut out: Vec<u8> = Vec::new();
    let separator = |out: &mut Vec<u8>| {
        let _ = bun_core::write_pretty!(out, COLORS, "<r><d>");
        out.resize(out.len() + max_filepath_length + 2, b'-');
        let _ =
            bun_core::write_pretty!(out, COLORS, "|---------|---------|-------------------<r>\n");
    };
    separator(&mut out);
    out.extend_from_slice(b"File");
    out.resize(out.len() + max_filepath_length - b"File".len() + 1, b' ');
    let _ = bun_core::write_pretty!(
        out,
        COLORS,
        " <d>|<r> % Funcs <d>|<r> % Lines <d>|<r> Uncovered Line #s\n"
    );
    separator(&mut out);
    let _ = text::write_format_with_values::<COLORS>(
        b"All files",
        max_filepath_length,
        avg,
        avg_thresholds,
        failing,
        &mut out,
        false,
    );
    let _ = bun_core::write_pretty!(out, COLORS, "<r><d> |<r>\n");
    for (report, fraction) in reports.iter().zip(fractions) {
        let _ = text::write_format::<COLORS>(
            report,
            max_filepath_length,
            fraction,
            &thresholds,
            relative_dir,
            &mut out,
        );
        out.push(b'\n');
    }
    separator(&mut out);

    // A closed stderr is not a reason to fail the run.
    let _ = Output::error_writer().write_all(&out);
    Output::flush();
}

/// Render to `<reports_directory>/.lcov.info.<hex>.tmp` and rename it over
/// `lcov.info`, so an interrupted run never leaves a truncated report.
fn write_lcov_report(
    opts: &CodeCoverageOptions,
    relative_dir: &[u8],
    reports: &[CodeCoverageReport<'_>],
) -> bun_sys::Result<()> {
    let mut contents: Vec<u8> = Vec::with_capacity(64 * 1024);
    for report in reports {
        // Writing into a Vec cannot fail.
        let _ = coverage::lcov::write_format(report, relative_dir, &mut contents);
    }

    let mut fs = crate::node::fs::NodeFS::default();
    let _ = fs.mkdir_recursive(&crate::node::fs::args::Mkdir {
        path: crate::node::PathLike::borrowed(&opts.reports_directory),
        always_return_none: true,
        recursive: true,
        ..Default::default()
    });

    let mut rand = [0u8; 8];
    bun_boringssl_sys::rand_bytes(&mut rand);
    let mut tmpname: Vec<u8> = Vec::new();
    let _ = write!(
        &mut tmpname,
        ".lcov.info.{}.tmp",
        bun_core::fmt::hex_lower(&rand)
    );
    let mut buf = bun_paths::path_buffer_pool::get();
    let tmp_path = resolve_path::join_abs_string_buf_z::<bun_path::platform::Auto>(
        relative_dir,
        &mut buf,
        &[&opts.reports_directory, &tmpname],
    );
    let file = File::openat(
        Fd::cwd(),
        tmp_path,
        bun_sys::O::CREAT | bun_sys::O::WRONLY | bun_sys::O::TRUNC | bun_sys::O::CLOEXEC,
        0o644,
    )?;
    let written = file.write_all(&contents);
    drop(file);
    let moved = match written {
        Ok(()) => bun_sys::move_file_z(
            Fd::cwd(),
            tmp_path,
            Fd::cwd(),
            resolve_path::join_abs_string_z::<bun_path::platform::Auto>(
                relative_dir,
                &[&opts.reports_directory, b"lcov.info"],
            ),
        ),
        err => err,
    };
    if moved.is_err() {
        let _ = bun_sys::unlink(tmp_path);
    }
    moved
}

#[unsafe(no_mangle)]
extern "C" fn BunTest__shouldGenerateCodeCoverage(test_name_str: &bun_core::String) -> bool {
    let zig_slice = test_name_str.to_utf8();
    // In this particular case, we don't actually care about non-ascii latin1 characters.
    // so we skip the ascii check
    let slice: &[u8] = zig_slice.slice();

    // always ignore node_modules.
    if strings::contains(slice, b"/node_modules/") || strings::contains(slice, b"\\node_modules\\")
    {
        return false;
    }

    let ext = bun_path::extension(slice);
    // SAFETY: `VirtualMachine::get()` returns the process-lifetime VM pointer; only
    // called from the JS thread once a VM exists.
    let loader_by_ext = VirtualMachine::get()
        .as_mut()
        .transpiler
        .options
        .loader(ext);

    // allow file loader just incase they use a custom loader with a non-standard extension
    if !(loader_by_ext.is_javascript_like() || loader_by_ext == bun_ast::Loader::File) {
        return false;
    }

    if let Some(runner) = jest::Jest::runner() {
        if runner.test_options.coverage.skip_test_files {
            let name_without_extension = &slice[0..slice.len() - ext.len()];
            for suffix in TEST_NAME_SUFFIXES {
                if strings::ends_with(name_without_extension, suffix) {
                    return false;
                }
            }
        }
    }

    true
}

pub(crate) fn handle_top_level_test_error_before_javascript_start(err: &crate::Error) -> ! {
    if cfg!(debug_assertions) {
        if !matches!(err, crate::Error::ModuleNotFound) {
            bun_core::debug_warn!("Unhandled error: {}", err.name());
        }
    }
    Global::exit(1);
}
