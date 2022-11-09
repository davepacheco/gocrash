// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
// Copyright 2022 Oxide Computer Company

//! Command to run the Go test suite in parallel in a loop, using ZFS snapshots
//! and clones to quickly ensure a clean slate every time

// TODO: want handling for SIGINT

use anyhow::anyhow;
use anyhow::Context;
use clap::Parser;
use std::fmt::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

fn main() {
    let args = Args::parse();
    if let Err(error) = gocrash(&args) {
        eprintln!("gocrash: {:#}", error);
        std::process::exit(1);
    }
}

/// Run the Go test suite in a loop until it fails
#[derive(Parser)]
struct Args {
    /// how many concurrent threads to run the test suite
    #[arg(long, default_value_t = 2)]
    concurrency: u8,

    /// stop after each thread does this many runs
    /// (leave unspecified to run until failure)
    #[arg(long)]
    stop_after: Option<usize>,

    /// save output from successful test runs
    #[arg(long, default_value_t = false)]
    keep_success: bool,

    /// ZFS snapshot for dataset containing "goroot"
    snapshot: String,
}

/// Runs the guts of the `gocrash` command
fn gocrash(args: &Args) -> Result<(), anyhow::Error> {
    let (dataset_name, _) = args
        .snapshot
        .split_once('@')
        .ok_or_else(|| anyhow!("bad syntax for snapshot name (missing '@')"))?;

    // Determine a unique name for our working dataset.
    let timestamp_millis = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let gocrash_key = format!("gocrash-{}", timestamp_millis);
    let gocrash_dataset = format!("{}/{}", dataset_name, gocrash_key);

    let gocrash = Gocrash {
        source_snapshot: &args.snapshot,
        stop_after: args.stop_after,
        keep_success: args.keep_success,
        gocrash_dataset,
        stopping: AtomicBool::new(false),
    };

    // Print a summary of parameters.
    println!("using snapshot:  {}", args.snapshot);
    println!("working dataset: {}", gocrash.gocrash_dataset);
    println!("concurrency:     {}", args.concurrency);
    println!(
        "save results:    {}",
        if gocrash.keep_success {
            "for all runs"
        } else {
            "for failed runs only"
        }
    );
    println!(
        "stop:            {}",
        match args.stop_after {
            None => String::from("after any run fails"),
            Some(stop_after) => format!(
                "after all threads do {} run{}",
                stop_after,
                if stop_after == 1 { "" } else { "s" }
            ),
        }
    );
    println!("");

    // Create our working dataset
    let _ = run_command(
        Command::new("pfexec")
            .arg("zfs")
            .arg("create")
            .arg(&gocrash.gocrash_dataset),
    )?;

    println!("created zfs dataset {:?}", gocrash.gocrash_dataset);

    // Create threads to run the test suite.
    std::thread::scope(|scope| {
        let myref = &gocrash;
        let handles = (0..args.concurrency)
            .map(|i| scope.spawn(move || gocrash_worker(myref, i)))
            .collect::<Vec<_>>();

        // Wait for each thread to finish and print the results.
        let mut nerrors = 0;
        for (i, h) in handles.into_iter().enumerate() {
            let worker_result = h.join().map_err(|error| {
                anyhow!("thread {} panicked: {:?}", i, error)
            })?;
            println!(
                "thread {}: {} tries, result = {}",
                i,
                worker_result.ntries,
                match worker_result.result {
                    Ok(_) => String::from("ok"),
                    Err(error) => {
                        nerrors = nerrors + 1;
                        format!("{:#}", error)
                    }
                }
            )
        }

        if nerrors == 0 {
            Ok(())
        } else {
            Err(anyhow!("test failed"))
        }
    })
}

/// Describes the state of this "gocrash" run
struct Gocrash<'a> {
    // Immutable parameters
    /// user-provided snapshot that we'll clone for each test run
    source_snapshot: &'a str,
    /// each thread will do this number of attempts (None: infinite)
    stop_after: Option<usize>,
    /// whether to keep datasets for successful test runs
    keep_success: bool,
    /// name of our working ZFS dataset (containing per-run datasets)
    gocrash_dataset: String,

    // Runtime state
    /// whether we're stopping
    stopping: AtomicBool,
}

/// Describes the result of one worker thread
struct WorkerResult {
    /// number of times the test suite was run
    ntries: usize,
    /// result of the last test suite run
    result: Result<(), anyhow::Error>,
}

/// Body of one worker thread that runs the test suite
fn gocrash_worker<'a>(gocrash: &'a Gocrash<'a>, which: u8) -> WorkerResult {
    let mut ntries = 0;
    while !gocrash.stopping.load(Ordering::SeqCst) {
        // Carry out one run of the test suite.
        if let Err(error) = gocrash_worker_run_one(gocrash, which, ntries) {
            gocrash.stopping.store(true, Ordering::SeqCst);
            return WorkerResult { ntries, result: Err(error) };
        }

        ntries = ntries + 1;

        // If the user specified a limit, and we've reached it, we're done.
        if let Some(stop_after) = gocrash.stop_after {
            if ntries >= stop_after {
                break;
            }
        }
    }

    WorkerResult { ntries, result: Ok(()) }
}

/// Carries out one run of the test suite
fn gocrash_worker_run_one<'a>(
    gocrash: &'a Gocrash<'a>,
    which_thread: u8,
    which_run: usize,
) -> Result<(), anyhow::Error> {
    // Clone the original snapshot to a new dataset.
    let test_run_key = format!("thread-{}-run-{}", which_thread, which_run);
    let test_run_dataset =
        format!("{}/{}", gocrash.gocrash_dataset, test_run_key);

    let _ = run_command(
        Command::new("pfexec")
            .arg("zfs")
            .arg("clone")
            .arg(&gocrash.source_snapshot)
            .arg(&test_run_dataset),
    )?;

    // Get its mountpoint.
    let mountpoint_output = run_command(
        Command::new("zfs")
            .arg("list")
            .arg("-H")
            .arg("-omountpoint")
            .arg(&test_run_dataset),
    )?;

    let mountpoint = std::path::Path::new(mountpoint_output.trim());

    // Run the Go build and test suite with stdout and stderr redirected to
    // files in the new dataset.
    let stdout_file_path = mountpoint.join("test_run_stdout");
    let stderr_file_path = mountpoint.join("test_run_stderr");
    println!(
        "{}: thread {}: attempt {}: start (see {})",
        chrono::Utc::now(),
        which_thread,
        which_run,
        stdout_file_path.display(),
    );

    let stdout_file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(stdout_file_path)?;

    let stderr_file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(stderr_file_path)?;

    run_command(
        Command::new("bash")
            .arg("./all.bash")
            .current_dir(format!("{}/goroot/src", mountpoint.display()))
            .stdout(stdout_file)
            .stderr(stderr_file),
    )?;

    // If that succeeded, destroy the dataset.
    if !gocrash.keep_success {
        run_command(
            Command::new("pfexec")
                .arg("zfs")
                .arg("destroy")
                .arg(&test_run_dataset),
        )?;
    }

    Ok(())
}

/// Construct a human-readable label for use in log and error messages.
fn command_label(cmd: &Command) -> String {
    std::iter::once(cmd.get_program().to_string_lossy())
        .chain(cmd.get_args().map(|s| s.to_string_lossy()))
        .map(|s| format!("{:?}", s))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs the given command, buffering stdout and stderr and returning UTF-8
/// decoded stdout.
///
/// On failure, a detailed error message is produced.
fn run_command(cmd: &mut Command) -> Result<String, anyhow::Error> {
    // Construct a human-readable label for use in error messages.
    let label = command_label(cmd);

    let result =
        cmd.output().with_context(|| format!("failed to exec {}", label))?;
    if result.status.success() {
        Ok(String::from_utf8_lossy(&result.stdout).to_string())
    } else {
        let result_summary = if let Some(code) = result.status.code() {
            format!("exited with code {}", code)
        } else {
            let signal = result
                .status
                .signal()
                .expect("process exited with no code or signal");
            format!("terminated by signal {}", signal)
        };

        let mut output = String::new();
        write!(&mut output, "command failed: {}: {}", label, result_summary)
            .unwrap();

        let stderr = String::from_utf8_lossy(&result.stderr);
        if stderr.len() > 0 {
            write!(&mut output, "\nstderr:\n{}\n", stderr).unwrap();
        }

        let stdout = String::from_utf8_lossy(&result.stdout);
        if stdout.len() > 0 {
            write!(&mut output, "\nstdout:\n{}\n", stdout).unwrap();
        }

        Err(anyhow!("{}", output))
    }
}
