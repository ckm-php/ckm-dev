//! `ck-release`: the release tool of the PHP build pipeline. Never shipped.
//!
//! Exit codes: 0 ok, 1 any failure (one `ck-release: <message>` line on
//! stderr), 2 usage (from clap). Data goes to stdout only.

use std::io::{self, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use ck_core::catalog::schema::{Arch, Os, Platform, RuntimeName, RuntimeVersion, Sha256Digest};
use ck_core::download::policy::TransportPolicy;
use ck_core::paths::Profile;
use ck_release::lock::{
    Inputs, LockEntry, SignatureKind, Stack, default_lock_path, default_manifest_path,
};
use ck_release::pack::{self, PackSpec};
use ck_release::payload::{self, PayloadSpec};
use ck_release::{Error, Result, parse_cpu, parse_os};
use clap::{Args, Parser, Subcommand};

/// Release tool for ck-dev's own runtime builds.
#[derive(Parser)]
#[command(name = "ck-release", arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read or change the pins (catalog/sources.lock.json) and the PHP lines
    /// (catalog/php-lines.json).
    Lock {
        #[command(flatten)]
        files: LockFiles,
        #[command(subcommand)]
        command: LockCommand,
    },
    /// Pack a relocated runtime folder and write its descriptor.
    Pack {
        /// The folder whose contents become the archive.
        root: PathBuf,
        /// The archive; its file name must be <name>-<version>-<os>-<cpu>.tar.gz.
        #[arg(long)]
        out: PathBuf,
        /// Where to write the descriptor JSON.
        #[arg(long)]
        descriptor: PathBuf,
        #[arg(long, value_parser = parse_name)]
        name: RuntimeName,
        #[arg(long, value_parser = parse_version)]
        version: RuntimeVersion,
        /// linux or macos.
        #[arg(long, value_parser = parse_os)]
        os: Os,
        /// x86_64 or aarch64.
        #[arg(long, value_parser = parse_cpu)]
        cpu: Arch,
        /// SPDX licence expression for the catalog entry.
        #[arg(long)]
        license: String,
    },
    /// Build the unsigned catalog payload.
    Payload {
        #[command(subcommand)]
        command: PayloadCommand,
    },
}

#[derive(Args)]
struct LockFiles {
    /// The lock file [default: catalog/sources.lock.json in this repository].
    #[arg(long, global = true)]
    lock: Option<PathBuf>,
    /// The PHP lines [default: catalog/php-lines.json in this repository].
    #[arg(long, global = true)]
    manifest: Option<PathBuf>,
}

#[derive(Subcommand)]
enum LockCommand {
    /// Check both files and the rules between them.
    Check,
    /// Print entries, one tab-separated line each:
    /// name version url sha256 size license signature file.
    Export {
        /// Only the libraries and tools this stack builds, one per name.
        #[arg(long, conflicts_with = "line")]
        stack: Option<Stack>,
        /// Only the php source of this line, such as 8.5.
        #[arg(long)]
        line: Option<String>,
    },
    /// Add or replace one pin. The only writer of the lock.
    Set {
        #[arg(value_parser = parse_name)]
        name: RuntimeName,
        #[arg(value_parser = parse_version)]
        version: RuntimeVersion,
        #[arg(long)]
        url: String,
        /// 64 lower-case hex characters.
        #[arg(long, value_parser = parse_sha256)]
        sha256: Sha256Digest,
        /// Exact size in bytes.
        #[arg(long)]
        size: u64,
        /// SPDX licence expression.
        #[arg(long)]
        license: String,
        /// What was checked upstream besides the pin.
        #[arg(long, value_enum)]
        signature: SignatureKind,
    },
    /// Print what the build needs about one PHP line, as key<TAB>value lines.
    Line {
        /// The minor line, such as 8.5.
        line: String,
    },
}

#[derive(Subcommand)]
enum PayloadCommand {
    /// Turn every descriptor in a folder into one unsigned payload.
    Build {
        /// Folder of descriptor JSON files, and nothing else.
        #[arg(long)]
        descriptors: PathBuf,
        /// The release download URL the archives are uploaded under.
        #[arg(long)]
        base_url: String,
        /// Higher than every catalog published before.
        #[arg(long)]
        sequence: u64,
        /// RFC 3339 time, such as 2027-01-31T00:00:00Z.
        #[arg(long)]
        expires: String,
        /// The payload JSON to write.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ck-release: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Lock { files, command } => {
            let lock_path = files.lock.unwrap_or_else(default_lock_path);
            let manifest_path = files.manifest.unwrap_or_else(default_manifest_path);
            let mut inputs = Inputs::load(&lock_path, &manifest_path)?;
            match command {
                LockCommand::Check => print(&format!(
                    "ok: {} pins, {} PHP lines\n",
                    inputs.lock.entries.len(),
                    inputs.manifest.lines.len()
                )),
                LockCommand::Export { stack, line } => {
                    let entries = match (stack, line) {
                        (Some(stack), _) => inputs.stack_entries(stack)?,
                        (None, Some(line)) => vec![inputs.php_entry(&line)?],
                        (None, None) => inputs.lock.entries.iter().collect(),
                    };
                    let text: String = entries.iter().map(|entry| entry.export_line()).collect();
                    print(&text)
                }
                LockCommand::Set {
                    name,
                    version,
                    url,
                    sha256,
                    size,
                    license,
                    signature,
                } => {
                    inputs.set(LockEntry {
                        name,
                        version,
                        url,
                        sha256,
                        size,
                        license,
                        signature,
                    })?;
                    inputs.lock.save(&lock_path)
                }
                LockCommand::Line { line } => print(&inputs.line_info(&line)?.export()),
            }
        }
        Command::Pack {
            root,
            out,
            descriptor,
            name,
            version,
            os,
            cpu,
            license,
        } => {
            let spec = PackSpec {
                name,
                version,
                platform: Platform { os, arch: cpu },
                license,
            };
            pack::pack(&root, &out, &spec)?.write(&descriptor)
        }
        Command::Payload {
            command:
                PayloadCommand::Build {
                    descriptors,
                    base_url,
                    sequence,
                    expires,
                    out,
                },
        } => {
            let spec = PayloadSpec {
                base_url: &base_url,
                sequence,
                expires: payload::parse_expires(&expires)?,
            };
            let descriptors = payload::read_descriptors(&descriptors)?;
            let policy = TransportPolicy::current(Profile::Standard);
            payload::write(&payload::build(&descriptors, &spec, &policy)?, &out)
        }
    }
}

/// Writes all of `text` to stdout, so a closed pipe is an error, not a panic.
fn print(text: &str) -> Result<()> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| Error::Invalid(format!("cannot write to stdout: {error}")))
}

fn parse_name(text: &str) -> Result<RuntimeName> {
    Ok(RuntimeName::new(text)?)
}

fn parse_version(text: &str) -> Result<RuntimeVersion> {
    Ok(RuntimeVersion::new(text)?)
}

fn parse_sha256(text: &str) -> Result<Sha256Digest> {
    Ok(Sha256Digest::from_hex(text)?)
}
