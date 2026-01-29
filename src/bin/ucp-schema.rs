//! UCP Schema CLI
//!
//! Command-line interface for resolving and validating UCP schemas.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ucp_schema::{
    bundle_refs, bundle_refs_with_url_mapping, compose_from_payload, compose_schema,
    detect_direction, extract_capabilities_from_profile, extract_jsonrpc_payload, is_url, lint,
    load_schema, load_schema_auto, resolve, validate, ComposeError, DetectedDirection, Direction,
    FileStatus, ResolveError, ResolveOptions, SchemaBaseConfig, ValidateError,
};

/// Errors with associated CLI exit codes.
trait CliExitCode {
    fn exit_code(&self) -> u8;
}

impl CliExitCode for ResolveError {
    fn exit_code(&self) -> u8 {
        ResolveError::exit_code(self) as u8
    }
}

impl CliExitCode for ComposeError {
    fn exit_code(&self) -> u8 {
        ComposeError::exit_code(self) as u8
    }
}

/// Map an error to a CLI exit code, reporting it in the configured format.
fn cli_err<E: std::fmt::Display + CliExitCode>(json_output: bool) -> impl FnOnce(E) -> u8 {
    move |e| {
        report_error(json_output, &e.to_string());
        e.exit_code()
    }
}

/// Like cli_err but with a message prefix for additional context.
fn cli_err_ctx<'a, E: std::fmt::Display + CliExitCode>(
    json_output: bool,
    context: &'a str,
) -> impl FnOnce(E) -> u8 + 'a {
    move |e| {
        report_error(json_output, &format!("{}: {}", context, e));
        e.exit_code()
    }
}

/// Determine direction from CLI flags and optional inference.
///
/// Priority: explicit --request/--response flags override inference.
/// When neither flag is set, uses inferred direction or defaults to Request.
fn determine_direction(
    request_flag: bool,
    response_flag: bool,
    inferred: Option<Direction>,
) -> Direction {
    if request_flag {
        Direction::Request
    } else if response_flag {
        Direction::Response
    } else {
        inferred.unwrap_or(Direction::Request)
    }
}

#[cfg(feature = "remote")]
use ucp_schema::bundle_refs_remote;

#[derive(Parser)]
#[command(name = "ucp-schema")]
#[command(about = "Resolve and validate UCP schema annotations")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Resolve a schema for a specific direction and operation
    Resolve {
        /// Schema source: file path or URL (http:// or https://)
        schema: String,

        /// Resolve for request direction
        #[arg(
            long,
            conflicts_with = "response",
            required_unless_present = "response"
        )]
        request: bool,

        /// Resolve for response direction
        #[arg(long, conflicts_with = "request", required_unless_present = "request")]
        response: bool,

        /// Operation to resolve for (e.g., create, update, read)
        #[arg(long, short)]
        op: String,

        /// Output file (stdout if not specified)
        #[arg(long)]
        output: Option<PathBuf>,

        /// Pretty-print JSON output
        #[arg(long)]
        pretty: bool,

        /// Dereference all $ref pointers (bundle into single schema)
        #[arg(long)]
        bundle: bool,

        /// Strict mode: set additionalProperties=false to reject unknown fields (default: false)
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        strict: bool,
    },

    /// Validate a payload against a resolved schema
    Validate {
        /// Payload file to validate
        payload: PathBuf,

        /// Explicit schema (default: infer from payload's UCP metadata)
        #[arg(long)]
        schema: Option<String>,

        /// Local directory containing schema files
        #[arg(long)]
        schema_local_base: Option<PathBuf>,

        /// URL prefix to strip when mapping to local (e.g., https://ucp.dev/draft)
        #[arg(long, requires = "schema_local_base")]
        schema_remote_base: Option<String>,

        /// Agent profile URL (REST pattern: profile via header, payload is raw object)
        #[arg(long, conflicts_with = "schema")]
        profile: Option<String>,

        /// Validate as request (auto-inferred if omitted)
        #[arg(long, conflicts_with = "response")]
        request: bool,

        /// Validate as response (auto-inferred if omitted)
        #[arg(long, conflicts_with = "request")]
        response: bool,

        /// Operation to validate for (e.g., create, update, read)
        #[arg(long, short)]
        op: String,

        /// Output results as JSON (for automation)
        #[arg(long)]
        json: bool,

        /// Strict mode: reject unknown fields (default: false)
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        strict: bool,
    },

    /// Lint schema files for errors (syntax, broken refs, invalid annotations)
    Lint {
        /// File or directory to lint
        path: PathBuf,

        /// Output format: text (default) or json
        #[arg(long, default_value = "text")]
        format: String,

        /// Treat warnings as errors
        #[arg(long)]
        strict: bool,

        /// Suppress progress output, only show errors
        #[arg(long, short)]
        quiet: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Resolve {
            schema,
            request,
            response: _,
            op,
            output,
            pretty,
            bundle,
            strict,
        } => run_resolve(&schema, request, op, output, pretty, bundle, strict),

        Commands::Validate {
            payload,
            schema,
            schema_local_base,
            schema_remote_base,
            profile,
            request,
            response,
            op,
            json,
            strict,
        } => run_validate(ValidateArgs {
            payload,
            schema,
            schema_local_base,
            schema_remote_base,
            profile,
            request,
            response,
            op,
            json_output: json,
            strict,
        }),

        Commands::Lint {
            path,
            format,
            strict,
            quiet,
        } => run_lint(&path, &format, strict, quiet),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => ExitCode::from(code),
    }
}

fn run_resolve(
    schema_source: &str,
    request: bool,
    op: String,
    output: Option<PathBuf>,
    pretty: bool,
    bundle: bool,
    strict: bool,
) -> Result<(), u8> {
    // run_resolve has no --json flag, so errors always go to stderr (json_output=false)
    let direction = Direction::from_request_flag(request);

    let mut schema = load_schema_auto(schema_source).map_err(cli_err(false))?;

    // Bundle: dereference all $refs before resolving annotations
    if bundle {
        // Resolve external file refs and their internal refs using our loader
        // Note: $ref: "#" (self-refs) are left as-is since they're recursive
        let base_dir = std::path::Path::new(schema_source)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        bundle_refs(&mut schema, base_dir).map_err(cli_err_ctx(false, "bundling refs"))?;
    }

    let options = ResolveOptions::new(direction, op).strict(strict);
    let resolved = resolve(&schema, &options).map_err(cli_err(false))?;

    let json_output = if pretty {
        serde_json::to_string_pretty(&resolved)
    } else {
        serde_json::to_string(&resolved)
    }
    .map_err(|e| {
        eprintln!("Error serializing output: {}", e);
        2u8
    })?;

    match output {
        Some(path) => {
            std::fs::write(&path, &json_output).map_err(|e| {
                eprintln!("Error writing to {}: {}", path.display(), e);
                3u8
            })?;
        }
        None => {
            println!("{}", json_output);
        }
    }

    Ok(())
}

struct ValidateArgs {
    payload: PathBuf,
    schema: Option<String>,
    schema_local_base: Option<PathBuf>,
    schema_remote_base: Option<String>,
    profile: Option<String>,
    request: bool,
    response: bool,
    op: String,
    json_output: bool,
    strict: bool,
}

fn run_validate(args: ValidateArgs) -> Result<(), u8> {
    let ValidateArgs {
        payload: payload_path,
        schema: schema_source,
        schema_local_base,
        schema_remote_base,
        profile: profile_url,
        request,
        response,
        op,
        json_output,
        strict,
    } = args;

    let config = SchemaBaseConfig {
        local_base: schema_local_base.as_deref(),
        remote_base: schema_remote_base.as_deref(),
    };

    // Load payload file
    let payload_file =
        load_schema(&payload_path).map_err(cli_err_ctx(json_output, "loading payload"))?;

    // Determine validation mode and extract actual payload to validate:
    // 1. --profile: REST pattern, payload is raw object
    // 2. --schema: explicit schema, payload is raw object
    // 3. JSONRPC: meta.profile in payload, extract nested payload
    // 4. Response: ucp.capabilities in payload, payload is self-describing
    let (schema, payload, direction) = if let Some(ref profile) = profile_url {
        // REST pattern: --profile flag provides profile URL, payload is raw
        let direction = determine_direction(request, response, None);

        let capabilities =
            extract_capabilities_from_profile(profile, &config).map_err(cli_err(json_output))?;

        let schema = compose_schema(&capabilities, &config).map_err(cli_err(json_output))?;

        (schema, payload_file, direction)
    } else if let Some(ref source) = schema_source {
        // Explicit schema: try to infer direction from payload
        let inferred = detect_direction(&payload_file).map(Direction::from);
        let direction = determine_direction(request, response, inferred);

        let mut schema =
            load_schema_auto(source).map_err(cli_err_ctx(json_output, "loading schema"))?;

        // Bundle refs based on source type and available mappings
        #[cfg(feature = "remote")]
        {
            if is_url(source) {
                bundle_refs_remote(&mut schema, source)
                    .map_err(cli_err_ctx(json_output, "bundling refs"))?;
            } else {
                bundle_local_refs(
                    &mut schema,
                    source,
                    &schema_local_base,
                    &schema_remote_base,
                    json_output,
                )?;
            }
        }
        #[cfg(not(feature = "remote"))]
        {
            bundle_local_refs(
                &mut schema,
                source,
                &schema_local_base,
                &schema_remote_base,
                json_output,
            )?;
        }

        (schema, payload_file, direction)
    } else {
        // Self-describing mode - detect from payload structure
        match detect_direction(&payload_file) {
            Some(DetectedDirection::Response) => {
                // Response: ucp.capabilities, compose and validate full payload
                let direction = determine_direction(request, response, Some(Direction::Response));
                let schema =
                    compose_from_payload(&payload_file, &config).map_err(cli_err(json_output))?;
                (schema, payload_file, direction)
            }
            Some(DetectedDirection::Request) => {
                // JSONRPC request: meta.profile, extract nested payload
                let direction = determine_direction(request, response, Some(Direction::Request));

                // Get profile URL from meta.profile
                let profile = payload_file
                    .get("meta")
                    .and_then(|m| m.get("profile"))
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        report_error(json_output, "JSONRPC request missing meta.profile");
                        2u8
                    })?;

                let capabilities = extract_capabilities_from_profile(profile, &config)
                    .map_err(cli_err(json_output))?;

                // Extract actual payload from envelope (e.g., "checkout" key)
                let (nested_payload, _key) = extract_jsonrpc_payload(&payload_file, &capabilities)
                    .map_err(cli_err(json_output))?;

                let schema =
                    compose_schema(&capabilities, &config).map_err(cli_err(json_output))?;

                (schema, nested_payload.clone(), direction)
            }
            None => {
                report_error(
                    json_output,
                    "cannot infer direction: payload has no ucp.capabilities (response) or meta.profile (request). Use --schema, --profile, --request, or --response.",
                );
                return Err(2);
            }
        }
    };

    let options = ResolveOptions::new(direction, op).strict(strict);

    match validate(&schema, &payload, &options) {
        Ok(()) => {
            if json_output {
                println!(r#"{{"valid":true}}"#);
            } else {
                println!("Valid");
            }
            Ok(())
        }
        Err(ValidateError::Invalid { errors, .. }) => {
            if json_output {
                let output = serde_json::json!({
                    "valid": false,
                    "errors": errors
                });
                println!("{}", output);
            } else {
                eprintln!("Validation failed:");
                for error in errors {
                    eprintln!("  {}", error);
                }
            }
            Err(1)
        }
        Err(ValidateError::Resolve(e)) => {
            report_error(json_output, &e.to_string());
            Err(e.exit_code() as u8)
        }
    }
}

/// Bundle refs for a local schema file.
fn bundle_local_refs(
    schema: &mut serde_json::Value,
    source: &str,
    schema_local_base: &Option<PathBuf>,
    schema_remote_base: &Option<String>,
    json_output: bool,
) -> Result<(), u8> {
    let schema_dir = Path::new(source).parent().unwrap_or(Path::new("."));

    if let (Some(local_base), Some(remote_base)) = (schema_local_base, schema_remote_base) {
        bundle_refs_with_url_mapping(schema, schema_dir, local_base, remote_base)
            .map_err(cli_err_ctx(json_output, "bundling refs"))?;
    } else {
        bundle_refs(schema, schema_dir).map_err(cli_err_ctx(json_output, "bundling refs"))?;
    }

    Ok(())
}

/// Output an error message in plain text or JSON format.
///
/// Uses same shape as validation errors for consistent API:
/// `{"valid": false, "errors": [{"path": "", "message": "..."}]}`
fn report_error(json_output: bool, msg: &str) {
    if json_output {
        let output = serde_json::json!({
            "valid": false,
            "errors": [{"path": "", "message": msg}]
        });
        println!("{}", output);
    } else {
        eprintln!("Error: {}", msg);
    }
}

fn run_lint(path: &Path, format: &str, strict: bool, quiet: bool) -> Result<(), u8> {
    use ucp_schema::Severity;

    if !path.exists() {
        eprintln!("Error: path not found: {}", path.display());
        return Err(2);
    }

    let result = lint(path, strict);

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        // Text output
        if !quiet {
            println!("Linting {} ...\n", path.display());
        }

        for file_result in &result.results {
            let status_icon = match file_result.status {
                FileStatus::Ok => "\x1b[32m✓\x1b[0m",
                FileStatus::Warning => "\x1b[33m⚠\x1b[0m",
                FileStatus::Error => "\x1b[31m✗\x1b[0m",
            };

            if !quiet || file_result.status != FileStatus::Ok {
                println!("  {} {}", status_icon, file_result.file.display());
            }

            for diag in &file_result.diagnostics {
                let color = match diag.severity {
                    Severity::Error => "\x1b[31m",
                    Severity::Warning => "\x1b[33m",
                };
                if !quiet || diag.severity == Severity::Error {
                    println!(
                        "    {}{}[{}]\x1b[0m: {} - {}",
                        color,
                        match diag.severity {
                            Severity::Error => "error",
                            Severity::Warning => "warning",
                        },
                        diag.code,
                        diag.path,
                        diag.message
                    );
                }
            }
        }

        println!();
        if result.is_ok() && (!strict || result.warnings == 0) {
            println!(
                "\x1b[32m✓ {} files checked, all passed\x1b[0m",
                result.files_checked
            );
        } else {
            println!(
                "\x1b[31m✗ {} files checked: {} passed, {} failed ({} errors, {} warnings)\x1b[0m",
                result.files_checked, result.passed, result.failed, result.errors, result.warnings
            );
        }
    }

    if result.is_ok() && (!strict || result.warnings == 0) {
        Ok(())
    } else {
        Err(1)
    }
}
