//! UCP Schema CLI
//!
//! Command-line interface for resolving and validating UCP schemas.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ucp_schema::{
    bundle_refs, bundle_refs_with_url_mapping, compose_from_payload, compose_schema,
    detect_direction, extract_capabilities, extract_capabilities_from_profile,
    extract_jsonrpc_payload, is_url, lint, load_schema, load_schema_auto, resolve, validate,
    ComposeError, DetectedDirection, Direction, FileStatus, ResolveError, ResolveOptions,
    SchemaBaseConfig, ValidateError,
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
/// When neither flag is set, uses inferred direction if available.
fn determine_direction(
    request_flag: bool,
    response_flag: bool,
    inferred: Option<Direction>,
) -> Option<Direction> {
    if request_flag {
        Some(Direction::Request)
    } else if response_flag {
        Some(Direction::Response)
    } else {
        inferred
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
    /// Resolve a schema for a specific direction and operation.
    /// Accepts a schema file or a self-describing payload (auto-composes if payload detected).
    Resolve {
        /// Schema or payload source: file path or URL (http:// or https://)
        schema: String,

        /// Resolve for request direction (auto-inferred for payloads)
        #[arg(long, conflicts_with = "response")]
        request: bool,

        /// Resolve for response direction (auto-inferred for payloads)
        #[arg(long, conflicts_with = "request")]
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

        /// Dereference all $ref pointers (bundle into single schema; schema input only)
        #[arg(long)]
        bundle: bool,

        /// Local directory containing schema files (used when input is a payload)
        #[arg(long)]
        schema_local_base: Option<PathBuf>,

        /// URL prefix to strip when mapping to local (e.g., https://ucp.dev/draft)
        #[arg(long, requires = "schema_local_base")]
        schema_remote_base: Option<String>,

        /// Strict mode: set additionalProperties=false to reject unknown fields (default: false)
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        strict: bool,

        /// Print pipeline stages to stderr for debugging
        #[arg(long, short)]
        verbose: bool,
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

        /// Print pipeline stages to stderr for debugging
        #[arg(long, short)]
        verbose: bool,
    },

    /// Compose capability schemas from a self-describing payload (annotations preserved)
    Compose {
        /// Payload file with UCP capabilities metadata
        payload: PathBuf,

        /// Local directory containing schema files
        #[arg(long)]
        schema_local_base: Option<PathBuf>,

        /// URL prefix to strip when mapping to local (e.g., https://ucp.dev/draft)
        #[arg(long, requires = "schema_local_base")]
        schema_remote_base: Option<String>,

        /// Output file (stdout if not specified)
        #[arg(long)]
        output: Option<PathBuf>,

        /// Pretty-print JSON output
        #[arg(long)]
        pretty: bool,

        /// Print pipeline stages to stderr for debugging
        #[arg(long, short)]
        verbose: bool,
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
            response,
            op,
            output,
            pretty,
            bundle,
            schema_local_base,
            schema_remote_base,
            strict,
            verbose,
        } => run_resolve(
            &schema,
            request,
            response,
            op,
            output,
            pretty,
            bundle,
            schema_local_base,
            schema_remote_base,
            strict,
            verbose,
        ),

        Commands::Compose {
            payload,
            schema_local_base,
            schema_remote_base,
            output,
            pretty,
            verbose,
        } => run_compose(
            &payload,
            schema_local_base,
            schema_remote_base,
            output,
            pretty,
            verbose,
        ),

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
            verbose,
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
            verbose,
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

/// Resolve a schema for a specific direction and operation.
///
/// Auto-detects input type: if the input is a self-describing payload (has
/// ucp.capabilities or meta.profile), composes schemas first then resolves.
/// Otherwise resolves the schema directly.
#[allow(clippy::too_many_arguments)]
fn run_resolve(
    schema_source: &str,
    request: bool,
    response: bool,
    op: String,
    output: Option<PathBuf>,
    pretty: bool,
    bundle: bool,
    schema_local_base: Option<PathBuf>,
    schema_remote_base: Option<String>,
    strict: bool,
    verbose: bool,
) -> Result<(), u8> {
    if verbose {
        eprintln!("[load] reading {}", schema_source);
    }
    let mut input = load_schema_auto(schema_source).map_err(cli_err(false))?;

    // Auto-detect: is this a payload (needs compose) or a schema (resolve directly)?
    let detected = detect_direction(&input);

    // Flag validation: reject flags that don't apply to the detected input type
    if detected.is_some() {
        if bundle {
            report_error(false, "--bundle does not apply to payload input (schemas are auto-composed from capabilities). Remove --bundle, or pass a schema file instead of a payload.");
            return Err(2);
        }
    } else if schema_local_base.is_some() || schema_remote_base.is_some() {
        report_error(false, "--schema-local-base/--schema-remote-base only apply to payload input. Remove these flags, or pass a self-describing payload instead of a schema file.");
        return Err(2);
    }

    let schema = if detected.is_some() {
        // Input is a self-describing payload — compose schemas from capabilities
        let config = SchemaBaseConfig {
            local_base: schema_local_base.as_deref(),
            remote_base: schema_remote_base.as_deref(),
        };
        if verbose {
            verbose_capabilities(&input, &config);
            eprintln!("[compose] composing schemas from payload capabilities");
        }
        compose_from_payload(&input, &config).map_err(cli_err(false))?
    } else {
        if verbose {
            eprintln!("[detect] input is a schema file (no ucp.capabilities)");
        }
        // Input is a schema file — bundle $refs if requested
        if bundle {
            if verbose {
                eprintln!("[bundle] inlining $ref pointers");
            }
            let base_dir = Path::new(schema_source).parent().unwrap_or(Path::new("."));
            bundle_refs(&mut input, base_dir).map_err(cli_err_ctx(false, "bundling refs"))?;
        }
        input
    };

    // Direction: explicit flag > auto-inferred from payload > require explicit
    let direction = determine_direction(request, response, detected.map(Direction::from))
        .ok_or_else(|| {
            report_error(
                false,
                "--request or --response is required for schema input",
            );
            2u8
        })?;

    let options = ResolveOptions::new(direction, &op).strict(strict);
    if verbose {
        eprintln!(
            "[resolve] resolving for {}/{}{}",
            direction
                .annotation_key()
                .strip_prefix("ucp_")
                .unwrap_or(direction.annotation_key()),
            op,
            if strict { " (strict)" } else { "" }
        );
    }
    let resolved = resolve(&schema, &options).map_err(cli_err(false))?;

    write_json_output(&resolved, output, pretty)
}

/// Pure composition: merge capability schemas from a self-describing payload.
/// Output preserves UCP annotations (no resolve step).
fn run_compose(
    payload_path: &Path,
    schema_local_base: Option<PathBuf>,
    schema_remote_base: Option<String>,
    output: Option<PathBuf>,
    pretty: bool,
    verbose: bool,
) -> Result<(), u8> {
    if verbose {
        eprintln!("[load] reading {}", payload_path.display());
    }
    let payload = load_schema(payload_path).map_err(cli_err_ctx(false, "loading payload"))?;

    // Verify input is a self-describing payload
    if detect_direction(&payload).is_none() {
        report_error(false, "input is not a self-describing payload (missing ucp.capabilities or meta.profile). Use `resolve` for schema files.");
        return Err(2);
    }

    let config = SchemaBaseConfig {
        local_base: schema_local_base.as_deref(),
        remote_base: schema_remote_base.as_deref(),
    };
    if verbose {
        verbose_capabilities(&payload, &config);
        eprintln!("[compose] composing schemas (annotations preserved)");
    }
    let schema = compose_from_payload(&payload, &config).map_err(cli_err(false))?;

    write_json_output(&schema, output, pretty)
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
    verbose: bool,
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
        verbose,
    } = args;

    // Flag validation: --schema-local-base/--schema-remote-base don't apply with
    // explicit --schema (composition is bypassed, so these would silently do nothing)
    if schema_source.is_some() && (schema_local_base.is_some() || schema_remote_base.is_some()) {
        report_error(json_output, "--schema-local-base/--schema-remote-base do not apply with explicit --schema (composition is bypassed). Remove these flags, or remove --schema to use self-describing mode.");
        return Err(2);
    }

    let config = SchemaBaseConfig {
        local_base: schema_local_base.as_deref(),
        remote_base: schema_remote_base.as_deref(),
    };

    // Load payload file
    if verbose {
        eprintln!("[load] reading payload {}", payload_path.display());
    }
    let payload_file =
        load_schema(&payload_path).map_err(cli_err_ctx(json_output, "loading payload"))?;

    // Determine validation mode and extract actual payload to validate:
    // 1. --profile: REST pattern, payload is raw object
    // 2. --schema: explicit schema, payload is raw object
    // 3. JSONRPC: meta.profile in payload, extract nested payload
    // 4. Response: ucp.capabilities in payload, payload is self-describing
    let (schema, payload, direction) = if let Some(ref profile) = profile_url {
        // REST pattern: --profile flag provides profile URL, payload is raw
        if verbose {
            eprintln!("[detect] REST pattern: using --profile {}", profile);
        }
        let direction = determine_direction(request, response, None).unwrap_or(Direction::Request);

        let capabilities =
            extract_capabilities_from_profile(profile, &config).map_err(cli_err(json_output))?;

        if verbose {
            eprintln!(
                "[compose] composing {} capability schemas from profile",
                capabilities.len()
            );
        }
        let schema = compose_schema(&capabilities, &config).map_err(cli_err(json_output))?;

        (schema, payload_file, direction)
    } else if let Some(ref source) = schema_source {
        // Explicit schema: try to infer direction from payload
        if verbose {
            eprintln!("[load] using explicit schema: {}", source);
        }
        let inferred = detect_direction(&payload_file).map(Direction::from);
        let direction =
            determine_direction(request, response, inferred).unwrap_or(Direction::Request);

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
                if verbose {
                    verbose_capabilities(&payload_file, &config);
                    eprintln!("[compose] composing schemas from payload capabilities");
                }
                let direction = determine_direction(request, response, Some(Direction::Response))
                    .unwrap_or(Direction::Response);
                let schema =
                    compose_from_payload(&payload_file, &config).map_err(cli_err(json_output))?;
                (schema, payload_file, direction)
            }
            Some(DetectedDirection::Request) => {
                // JSONRPC request: meta.profile, extract nested payload
                let direction = determine_direction(request, response, Some(Direction::Request))
                    .unwrap_or(Direction::Request);

                // Get profile URL from meta.profile
                let profile = payload_file
                    .get("meta")
                    .and_then(|m| m.get("profile"))
                    .and_then(|p| p.as_str())
                    .ok_or_else(|| {
                        report_error(json_output, "JSONRPC request missing meta.profile");
                        2u8
                    })?;

                if verbose {
                    eprintln!("[detect] JSONRPC request: fetching profile {}", profile);
                }

                let capabilities = extract_capabilities_from_profile(profile, &config)
                    .map_err(cli_err(json_output))?;

                // Extract actual payload from envelope (e.g., "checkout" key)
                let (nested_payload, _key) = extract_jsonrpc_payload(&payload_file, &capabilities)
                    .map_err(cli_err(json_output))?;

                if verbose {
                    eprintln!(
                        "[compose] composing {} capability schemas from profile",
                        capabilities.len()
                    );
                }
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
    if verbose {
        eprintln!(
            "[resolve] resolving for {}/{}",
            direction
                .annotation_key()
                .strip_prefix("ucp_")
                .unwrap_or(direction.annotation_key()),
            options.operation
        );
        eprintln!("[validate] validating payload against resolved schema");
    }

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

/// Shared helper: serialize JSON and write to output or stdout.
fn write_json_output(
    value: &serde_json::Value,
    output: Option<PathBuf>,
    pretty: bool,
) -> Result<(), u8> {
    let json = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .map_err(|e| {
        eprintln!("Error serializing output: {}", e);
        2u8
    })?;

    match output {
        Some(path) => {
            std::fs::write(&path, &json).map_err(|e| {
                eprintln!("Error writing to {}: {}", path.display(), e);
                3u8
            })?;
        }
        None => {
            println!("{}", json);
        }
    }

    Ok(())
}

/// Print capability details to stderr for --verbose mode.
/// Best-effort: silently skips if extraction fails (errors surface later).
fn verbose_capabilities(payload: &serde_json::Value, config: &SchemaBaseConfig) {
    if let Ok(caps) = extract_capabilities(payload, config) {
        let roots: Vec<_> = caps.iter().filter(|c| c.extends.is_none()).collect();
        let exts: Vec<_> = caps.iter().filter(|c| c.extends.is_some()).collect();
        eprintln!(
            "[detect] payload with {} capabilities ({} root, {} extensions)",
            caps.len(),
            roots.len(),
            exts.len()
        );
        for cap in &caps {
            let kind = if cap.extends.is_some() { "ext" } else { "root" };
            eprintln!("[detect]   {} {} → {}", kind, cap.name, cap.schema_url);
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
