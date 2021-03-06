#[macro_use]
extern crate failure;
extern crate json;
extern crate wait_timeout;

mod runner;

use std::process::{self, Command, Stdio};
use std::fs::{File, remove_file};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::from_utf8;
use runner::{CoverageRunner, FullSuiteRunner, Runner, Status};

static TARGET_MUTAGEN: &'static str = "target/mutagen";
static MUTATIONS_LIST: &'static str = "mutations.txt";

type Result<T> = std::result::Result<T, failure::Error>;

/// Mutation is the structured form of the mutations that has been generated by mutagen's plugin.
struct Mutation<'a> {
    /// contains the count/identifier of the mutation generated by the plugin
    count: usize,
    /// contains the kind of mutation
    description: &'a str,
    /// span of the mutation, including the localization of the original expression
    span: &'a str,
}

impl<'a> Mutation<'a> {
    pub fn from(mutation: &'a str) -> Result<Mutation> {
        let mut split = mutation.splitn(2," - ");
        let str_count = split.next().ok_or(format_err!("Count separator not found on mutation {}", mutation))?;
        let tail = split.next().ok_or(format_err!("Count separator not found on mutation {}", mutation))?;

        let mut split = tail.splitn(2, " @ ");
        let description = split.next().ok_or(format_err!("Description separator not found {}", mutation))?;
        let span = split.next().ok_or(format_err!("Description separator not found {}", mutation))?;

        Ok(Mutation {
            count: str_count.parse()?,
            description,
            span,
        })
    }
}

fn run_mutations(runner: &mut Runner, list: &[String]) -> Result<()> {
    let max_mutation = list.len();
    let mut failures = 0usize;

    println!("Running {} mutations\n", max_mutation);
    for m in list {
        // Mutation count starts from 1 (0 is not mutations)
        let mutation = Mutation::from(m)?;

        print!("{} {} ({})", mutation.description, mutation.span, mutation.count);

        let result = runner.run(mutation.count)?;

        let status = if let Status::Success = result {
            // A succeeding test suite is actually a failure for us.
            // At least on test should have failed
            failures += 1;

            // change the output message to avoid the success<->failure inversion confusion. --bblum
            "SURVIVED :("
        } else {
            // "killed" in the google paper but let's avoid violent language
            "caught"
        };

        println!(" ... {}", status);
    }

    println!(
        "\nMutation results: {}. {} caught by existing tests; {} were undetected\n",
        if failures == 0 { "ok" } else { "FAILED" },
        list.len() - failures,
        failures
    );
    Ok(())
}

fn get_mutations_filename() -> Result<PathBuf> {
    let metadata = Command::new("cargo").arg("metadata").output()?;
    let stderr = from_utf8(&metadata.stderr)?;
    if !metadata.status.success() {
        bail!("{}", stderr);
    }
    let stdout = from_utf8(&metadata.stdout)?;
    let meta_json = json::parse(stdout)?;
    let root_dir = Path::new(
        meta_json["workspace_root"]
            .as_str()
            .expect("cargo metadata misses workspace_root"),
    );
    let mutagen_dir = root_dir.join(TARGET_MUTAGEN);
    if !mutagen_dir.exists() {
        bail!(format!("mutations are missing (i looked in {:?})", mutagen_dir))
    }
    Ok(mutagen_dir.join(MUTATIONS_LIST))
}

fn compile_tests() -> Result<Vec<PathBuf>> {
    let mut tests: Vec<PathBuf> = Vec::new();
    let compile_out = Command::new("cargo")
        .args(&["test", "--no-run", "--message-format=json"])
        // We need to skip first two arguments (path to mutagen binary and "mutagen" string)
        .args(std::env::args_os().skip(2))
        .stderr(Stdio::inherit())
        .output()?;

    if !compile_out.status.success() {
        bail!("cargo test returned non-zero status");
    }
    let compile_stdout = from_utf8(&compile_out.stdout)?;
    for line in compile_stdout.lines() {
        let msg_json = json::parse(line)?;
        if msg_json["reason"].as_str().unwrap() == "compiler-artifact"
            && msg_json["profile"]["test"].as_bool().unwrap_or(false)
        {
            for filename in msg_json["filenames"].members() {
                let f = filename.as_str().unwrap();
                if !f.ends_with(".rlib") && !f.ends_with(".dSYM") {
                    tests.push(f.to_string().into());
                }
            }
        }
    }
    Ok(tests)
}

fn read_mutations(filename: &PathBuf) -> Result<Vec<String>> {
    let mut file = File::open(filename)?;
    let mut s = String::new();
    file.read_to_string(&mut s)?;
    Ok(s.split("\n")
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect())
}

fn has_flag(flag: &str) -> bool {
    let mut args = std::env::args_os();

    args.find(|f| f == flag).is_some()
}

fn run() -> Result<()> {
    let tests_executable = compile_tests()?;
    if tests_executable.is_empty() {
        bail!("executable path not found");
    }
    let filename = get_mutations_filename()?;
    let list = read_mutations(&filename)?;

    let with_coverage = has_flag("--coverage");
    let (mut cov_runner, mut full_runner);
    let _res = remove_file("target/mutagen/loops.txt");
    for test_executable in tests_executable {
        println!("test executable at {:?}", test_executable);
        let runner: &mut Runner = if with_coverage {
            cov_runner = CoverageRunner::new(test_executable.clone());
            &mut cov_runner
        } else {
            full_runner = FullSuiteRunner::new(test_executable.clone());
            &mut full_runner
        };

        if let Err(e) = runner.run(0) {
            bail!(
                format!("Something horrible went wrong and I don't even know what: {:?}", e)
                // XXX: this doesn't even trigger if you *DO* have failing tests
                //"You need to make sure you don't have failing tests before running 'cargo mutagen'"
            );
        }

        run_mutations(runner, &list)?
    }
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{}", err);
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::Mutation;

    #[test]
    fn it_decodes_well_formed_mutations() {
        let mutation = "2 - add one to int constant @ src/lib.rs:27:21: 27:22";
        let mutation = Mutation::from(mutation).unwrap();

        assert_eq!(2, mutation.count);
        assert_eq!("add one to int constant", mutation.description);
        assert_eq!("src/lib.rs:27:21: 27:22", mutation.span);
    }

    #[test]
    fn it_fails_to_decode_malformed_mutations() {
        let malformed_mutations = [
            "non-numeric count - description ok @ span ok",
            "no count separator",
            "1 - no span separator",
        ];

        for mm in malformed_mutations.iter() {
            let mutation = Mutation::from(mm);
            assert!(mutation.is_err());
        }
    }
}
