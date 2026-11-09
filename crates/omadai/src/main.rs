//! `omadai`: AI API spend per platform and per key, from the OS keyring and a local cache.

use lexopt::prelude::*;
use omadai_core::cache::Store;
use omadai_core::config::{Config, GeminiConfig, KeyEntry, XaiConfig};
use omadai_core::date::{Date, Month, now_rfc3339};
use omadai_core::errlog;
use omadai_core::fetch::{self, FetchOutcome, FetchRequest};
use omadai_core::fsutil;
use omadai_core::google::{self, Loopback};
use omadai_core::http::{Transport, default_transport};
use omadai_core::keyring_file::{self, KeyringEncryption};
use omadai_core::keystore::{self, Keystore, KeystoreError};
use omadai_core::lock;
use omadai_core::paths::Paths;
use omadai_core::platform::{self, CredentialShape, Platform};
use omadai_core::provider::{CheckOutcome, Registry};
use omadai_core::providers::gemini::{valid_location, valid_name, valid_project};
use omadai_core::providers::xai::valid_team_id;
use omadai_core::report::{self, Range};
use omadai_core::scrub::{printable, printable_lines};
use omadai_core::{Error, Secret};
use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage:
  omadai fetch [--platform P] [--month YYYY-MM]
  omadai report [--month YYYY-MM | --from YYYY-MM-DD --to YYYY-MM-DD] [--platform P] [--json]
  omadai key add <platform> [--label L] [--team-id ID] [--allow-unencrypted-keyring]
  omadai key rename <platform> <label> <new-label>
  omadai key remove <platform> <label>
  omadai key list [--json]
  omadai log [--json]
  omadai log export
  omadai log clear
  omadai doctor";

enum Fail {
    Usage(String),
    Run(String),
}

impl From<lexopt::Error> for Fail {
    fn from(e: lexopt::Error) -> Self {
        Fail::Usage(e.to_string())
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Run(e.to_string())
    }
}

impl From<KeystoreError> for Fail {
    fn from(e: KeystoreError) -> Self {
        Fail::Run(e.to_string())
    }
}

type Result<T> = std::result::Result<T, Fail>;

fn main() -> ExitCode {
    match run(lexopt::Parser::from_env()) {
        Ok(code) => code,
        Err(Fail::Usage(m)) => {
            eprintln!("omadai: {m}\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Fail::Run(m)) => {
            eprintln!("{}", run_failure_line(&m));
            ExitCode::from(1)
        }
    }
}

/// The stderr lines for a failed command, whose text may carry a vendor's words; each line is escaped, so omadai's own line breaks stay.
fn run_failure_line(m: &str) -> String {
    format!("omadai: {}", printable_lines(m))
}

/// One numbered line of the export-table choice, whose name comes from BigQuery.
fn table_line(i: usize, t: &str) -> String {
    format!("  {}. {}", i + 1, printable(t))
}

fn run(mut p: lexopt::Parser) -> Result<ExitCode> {
    let command = match p.next()? {
        Some(Value(v)) => v.string()?,
        Some(Long("help") | Short('h')) => {
            println!("{USAGE}");
            return Ok(ExitCode::SUCCESS);
        }
        Some(Long("version")) => {
            println!("omadai {}", env!("CARGO_PKG_VERSION"));
            return Ok(ExitCode::SUCCESS);
        }
        Some(arg) => return Err(arg.unexpected().into()),
        None => return Err(Fail::Usage("missing command".into())),
    };
    match command.as_str() {
        "fetch" => cmd_fetch(p),
        "report" => cmd_report(p),
        "key" => cmd_key(p),
        "doctor" => cmd_doctor(p),
        "log" => cmd_log(p),
        other => Err(Fail::Usage(format!("unknown command {other}"))),
    }
}

fn string(v: OsString) -> Result<String> {
    Ok(v.string()?)
}

fn platform_arg(v: OsString) -> Result<&'static Platform> {
    let id = string(v)?;
    platform::find(&id).ok_or_else(|| {
        Fail::Usage(format!(
            "unknown platform {id}; expected one of {}",
            platform::ids()
        ))
    })
}

fn month_arg(v: OsString) -> Result<Month> {
    let s = string(v)?;
    Month::parse(&s).ok_or_else(|| Fail::Usage(format!("invalid month {s}, expected YYYY-MM")))
}

fn date_arg(v: OsString) -> Result<Date> {
    let s = string(v)?;
    Date::parse(&s).ok_or_else(|| Fail::Usage(format!("invalid date {s}, expected YYYY-MM-DD")))
}

fn label_arg(v: OsString) -> Result<String> {
    let s = string(v)?;
    if !platform::valid_label(&s) {
        return Err(Fail::Usage(format!(
            "invalid label {s:?}: use 1 to 64 letters, digits, spaces, '_', '.' or '-'"
        )));
    }
    Ok(s)
}

fn cmd_fetch(mut p: lexopt::Parser) -> Result<ExitCode> {
    let (mut only, mut month) = (None, None);
    while let Some(arg) = p.next()? {
        match arg {
            Long("platform") => only = Some(platform_arg(p.value()?)?.id),
            Long("month") => month = Some(month_arg(p.value()?)?),
            _ => return Err(arg.unexpected().into()),
        }
    }
    let paths = Paths::from_env()?;
    let config = Config::load(&paths.config_file()).map_err(|e| fetch::log_failure(&paths, e))?;
    let keystore = keystore::open_default();
    let registry = Registry::builtin();
    let transport = default_transport();
    let request = FetchRequest {
        paths: &paths,
        config: &config,
        keystore: keystore.as_ref(),
        registry: &registry,
        transport: transport.as_ref(),
        platform: only,
        month: month.unwrap_or_else(Month::current_utc),
    };
    match fetch::run(&request)? {
        FetchOutcome::AlreadyRunning => {
            eprintln!("omadai: another fetch is running");
            Ok(ExitCode::SUCCESS)
        }
        FetchOutcome::Ran(summary) => {
            for p in &summary.no_provider {
                eprintln!("{p}: no provider yet");
            }
            for (p, e) in &summary.failed {
                eprintln!("{p}: {}", printable_lines(e));
            }
            // The log is a convenience, so its trouble is said here and never changes the exit code.
            if let Some(problem) = &summary.log_problem {
                eprintln!("log: {}", printable_lines(problem));
            }
            // A requirement waits on the person, so an hourly run does not treat it as a failure.
            for (p, n) in &summary.needs {
                let brief = n.split_once(". ").map_or(n.as_str(), |(first, _)| first);
                eprintln!(
                    "{p}: needs: {} (omadai report has the rest)",
                    printable(brief)
                );
            }
            Ok(if summary.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

fn cmd_report(mut p: lexopt::Parser) -> Result<ExitCode> {
    let (mut month, mut from, mut to, mut only, mut json) = (None, None, None, None, false);
    while let Some(arg) = p.next()? {
        match arg {
            Long("month") => month = Some(month_arg(p.value()?)?),
            Long("from") => from = Some(date_arg(p.value()?)?),
            Long("to") => to = Some(date_arg(p.value()?)?),
            Long("platform") => only = Some(platform_arg(p.value()?)?.id),
            Long("json") => json = true,
            _ => return Err(arg.unexpected().into()),
        }
    }
    let range = match (month, from, to) {
        (Some(m), None, None) => Range::month(m),
        (None, None, None) => Range::month(Month::current_utc()),
        (None, Some(f), Some(t)) => Range::new(f, t).map_err(|e| Fail::Usage(e.to_string()))?,
        (Some(_), _, _) => {
            return Err(Fail::Usage(
                "--month cannot be combined with --from or --to".into(),
            ));
        }
        _ => return Err(Fail::Usage("--from and --to go together".into())),
    };
    let paths = Paths::from_env()?;
    let r = report::build(&Store::new(&paths.state_dir), range, only)?;
    let out = if json {
        report::to_json(&r) + "\n"
    } else {
        report::to_text(&r)
    };
    std::io::stdout()
        .lock()
        .write_all(out.as_bytes())
        .map_err(|e| Fail::Run(format!("write report: {e}")))?;
    Ok(ExitCode::SUCCESS)
}

/// Reads, exports or empties the error log; touches neither the network nor the keyring.
fn cmd_log(mut p: lexopt::Parser) -> Result<ExitCode> {
    let (mut sub, mut json) = (None, false);
    while let Some(arg) = p.next()? {
        match arg {
            Value(v) if sub.is_none() && !json => sub = Some(string(v)?),
            Long("json") if sub.is_none() => json = true,
            _ => return Err(arg.unexpected().into()),
        }
    }
    let paths = Paths::from_env()?;
    let now = now_rfc3339();
    let out = match sub.as_deref() {
        None => {
            let entries = errlog::read(&paths, &now)?;
            if json {
                errlog::to_json(&entries) + "\n"
            } else {
                errlog::to_text(&entries)
            }
        }
        Some("export") => {
            let text = errlog::to_text(&errlog::read(&paths, &now)?);
            // No path argument, so the plugin can never direct where this writes.
            let home = paths
                .home
                .as_ref()
                .ok_or_else(|| Fail::Run("log export needs HOME set to an absolute path".into()))?;
            let name = format!("omadai-error-log-{}.txt", Date::today_utc());
            fsutil::atomic_write_in(home, &name, text.as_bytes())?;
            format!("{}\n", home.join(name).display())
        }
        Some("clear") => {
            if let Some(moved) = errlog::clear(&paths, &now)? {
                eprintln!("log: {}", printable_lines(&moved));
            }
            String::new()
        }
        Some(other) => return Err(Fail::Usage(format!("unknown log subcommand {other}"))),
    };
    std::io::stdout()
        .lock()
        .write_all(out.as_bytes())
        .map_err(|e| Fail::Run(format!("write: {e}")))?;
    Ok(ExitCode::SUCCESS)
}

fn cmd_key(mut p: lexopt::Parser) -> Result<ExitCode> {
    let sub = match p.next()? {
        Some(Value(v)) => string(v)?,
        Some(arg) => return Err(arg.unexpected().into()),
        None => return Err(Fail::Usage("missing key subcommand".into())),
    };
    match sub.as_str() {
        "add" => key_add(p),
        "rename" => key_rename(p),
        "remove" => key_remove(p),
        "list" => key_list(p),
        other => Err(Fail::Usage(format!("unknown key subcommand {other}"))),
    }
}

/// Collects exactly `n` positional arguments, refusing any option.
fn positionals(p: &mut lexopt::Parser, n: usize) -> Result<Vec<OsString>> {
    let mut out = Vec::new();
    while let Some(arg) = p.next()? {
        match arg {
            Value(v) if out.len() < n => out.push(v),
            _ => return Err(arg.unexpected().into()),
        }
    }
    if out.len() != n {
        return Err(Fail::Usage(format!("expected {n} arguments")));
    }
    Ok(out)
}

fn key_add(mut p: lexopt::Parser) -> Result<ExitCode> {
    let (mut platform, mut label, mut team_id, mut allow_unencrypted) = (None, None, None, false);
    while let Some(arg) = p.next()? {
        match arg {
            Value(v) if platform.is_none() => platform = Some(platform_arg(v)?),
            Long("label") => label = Some(label_arg(p.value()?)?),
            Long("team-id") => team_id = Some(team_id_arg(p.value()?)?),
            Long("allow-unencrypted-keyring") => allow_unencrypted = true,
            _ => return Err(arg.unexpected().into()),
        }
    }
    let platform = platform.ok_or_else(|| Fail::Usage("missing platform".into()))?;
    if team_id.is_some() && platform.id != "xai" {
        return Err(Fail::Usage("--team-id applies only to xai".into()));
    }
    let label = label.unwrap_or_else(|| "default".to_string());
    let result = key_add_inner(platform, &label, team_id, allow_unencrypted);
    if let Err(Fail::Run(m)) = &result {
        eprintln!("{}", run_failure_line(m));
    }
    // The panel opens this in its own terminal window, so keep the outcome on screen.
    if std::io::stdin().is_terminal() {
        print!("Press Enter to close.");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
    match result {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(Fail::Run(_)) => Ok(ExitCode::from(1)),
        Err(usage) => Err(usage),
    }
}

fn key_add_inner(
    platform: &Platform,
    label: &str,
    team_id: Option<String>,
    allow_unencrypted: bool,
) -> Result<()> {
    let paths = Paths::from_env()?;
    let existing = Config::load(&paths.config_file())?;
    if existing.has_key(platform.id, label) {
        return Err(taken(platform, label));
    }
    if let Some(refused) = one_export(platform, &existing) {
        return Err(refused);
    }
    let store = keystore::open_default();
    if store.is_locked()? {
        return Err(KeystoreError::Locked.into());
    }
    let unencrypted = match keyring_file::detect(&paths.keyrings_dir) {
        KeyringEncryption::Unencrypted(path) if !allow_unencrypted => {
            return Err(Fail::Run(format!(
                "refusing to store a credential: your default keyring has no password, so its file {} holds every secret in plain text.\n\
                 Set a password on that keyring (for example in Passwords and Keys, Change Password on the Login keyring), then retry.\n\
                 To store it anyway, rerun with --allow-unencrypted-keyring.",
                path.display()
            )));
        }
        KeyringEncryption::Unencrypted(_) => {
            eprintln!(
                "warning: storing in an unencrypted keyring because --allow-unencrypted-keyring was given"
            );
            true
        }
        KeyringEncryption::Encrypted => false,
        KeyringEncryption::Unknown(why) => {
            eprintln!("note: could not tell whether the keyring is encrypted ({why})");
            false
        }
    };
    // The team ID is not a secret: it goes in config.toml, asked before the hidden key prompt.
    let team_id = match team_id {
        None if platform.id == "xai" && std::io::stdin().is_terminal() => {
            prompt_team_id(existing.xai.as_ref())?
        }
        given => given,
    };
    let transport = default_transport();
    let (secret, gemini) = read_credential(platform, transport.as_ref(), existing.gemini.as_ref())?;
    let signed_in = gemini.is_some();
    add_key(
        &paths,
        store.as_ref(),
        platform,
        label,
        &secret,
        |config| {
            if let Some(team_id) = team_id {
                config.xai = Some(XaiConfig { team_id });
            }
            if let Some(gemini) = gemini {
                config.gemini = Some(gemini);
            }
            if unencrypted {
                config.allow_unencrypted_keyring = true;
            }
        },
        |config| Registry::builtin().check(platform.id, transport.as_ref(), &secret, config),
    )
    .map_err(|e| if signed_in { with_revoke_hint(e) } else { e })
}

/// Google has issued a refresh token by now, so a failure says how to revoke it.
fn with_revoke_hint(e: Fail) -> Fail {
    let hint = |m: String| {
        format!(
            "{m}\nGoogle already issued a token: {}",
            google::REVOKE_HINT
        )
    };
    match e {
        Fail::Run(m) => Fail::Run(hint(m)),
        Fail::Usage(m) => Fail::Usage(hint(m)),
    }
}

fn taken(platform: &Platform, label: &str) -> Fail {
    Fail::Run(format!(
        "{} already has a key labelled {label:?}; remove it or pick another --label",
        platform.name
    ))
}

/// omadai reads one Google billing export per install, so a second Gemini key is refused.
fn one_export(platform: &Platform, config: &Config) -> Option<Fail> {
    let first = config
        .entries("gemini")
        .next()
        .filter(|_| platform.id == "gemini")?;
    let label = if first.label.contains(' ') {
        format!("'{}'", first.label)
    } else {
        first.label.clone()
    };
    Some(Fail::Run(format!(
        "Gemini already has a key labelled {:?}, and omadai reads one Google billing export per install; \
         remove it first with omadai key remove gemini {label}",
        first.label
    )))
}

/// Stores the credential and its label under the key lock, then runs the one-time check without it.
fn add_key(
    paths: &Paths,
    store: &dyn Keystore,
    platform: &Platform,
    label: &str,
    secret: &Secret,
    settings: impl FnOnce(&mut Config),
    check: impl FnOnce(&Config) -> CheckOutcome,
) -> Result<()> {
    let config = {
        let _lock = keys_lock(paths, &mut std::io::stderr())?;
        // Another key command may have saved the config while the prompts waited.
        let mut config = Config::load(&paths.config_file())?;
        if config.has_key(platform.id, label) {
            return Err(taken(platform, label));
        }
        if let Some(refused) = one_export(platform, &config) {
            return Err(refused);
        }
        store.put(platform.id, label, secret)?;
        config.keys.push(KeyEntry::new(platform.id, label));
        settings(&mut config);
        if let Err(e) = config.save(&paths.config_file()) {
            if let Err(back) = store.delete(platform.id, label) {
                return Err(Fail::Run(format!(
                    "{e}; the keyring still holds the key as {label:?}, and removing it failed: {back}"
                )));
            }
            return Err(e.into());
        }
        config
    };
    println!(
        "Stored {} key {label:?} in {}.",
        platform.name,
        store.kind()
    );
    match check(&config) {
        CheckOutcome::Ok(what) => println!("Check passed: {}", printable(&what)),
        CheckOutcome::Requirement(what) => {
            println!("Stored, but spend cannot be read yet: {}", printable(&what))
        }
        CheckOutcome::Failed(e) => println!("Check failed: {}", printable(e.as_str())),
        CheckOutcome::NoProvider => {
            println!("Check skipped: no provider yet for {}.", platform.name)
        }
    }
    Ok(())
}

/// Takes the credential-change lock, saying once on `notice` when another key command holds it.
fn keys_lock(paths: &Paths, notice: &mut dyn Write) -> Result<lock::Lock> {
    let path = paths.keys_lock_file();
    if let Some(held) = lock::try_acquire(&path)? {
        return Ok(held);
    }
    let _ = writeln!(notice, "waiting: another omadai key command is running");
    Ok(lock::acquire(&path)?)
}

fn team_id_arg(v: OsString) -> Result<String> {
    let s = string(v)?;
    if !valid_team_id(&s) {
        return Err(Fail::Usage(format!(
            "invalid team ID {s:?}: use letters, digits and '-'"
        )));
    }
    Ok(s)
}

/// Plain-text prompt for the xAI team ID; Enter alone leaves the configured one, if any, as it is.
fn prompt_team_id(current: Option<&XaiConfig>) -> Result<Option<String>> {
    let keep = current.map_or(String::new(), |x| format!("; Enter keeps {}", x.team_id));
    let line = ask_line(&format!(
        "xAI team ID (xAI Console > Settings > Team{keep})"
    ))?;
    if line.is_empty() {
        return Ok(None);
    }
    if !valid_team_id(&line) {
        return Err(Fail::Run(format!(
            "invalid team ID {line:?}: use letters, digits and '-'"
        )));
    }
    Ok(Some(line))
}

fn prompt(what: &str) -> Result<Secret> {
    let raw = rpassword::prompt_password(format!("{what} (input hidden): "))
        .map_err(|e| Fail::Run(format!("key add needs a terminal: {e}")))?;
    let mut s = Secret::new(raw);
    s.trim_in_place();
    if s.is_empty() {
        return Err(Fail::Run("nothing entered".into()));
    }
    Ok(s)
}

/// The secret to store, plus the Gemini export settings where the platform has them.
fn read_credential(
    platform: &Platform,
    http: &dyn Transport,
    gemini: Option<&GeminiConfig>,
) -> Result<(Secret, Option<GeminiConfig>)> {
    match platform.shape {
        CredentialShape::Single { prompt: what } => Ok((prompt(what)?, None)),
        CredentialShape::Pair { first, second } => {
            let a = prompt(first)?;
            let b = prompt(second)?;
            if a.expose().contains(':') || b.expose().contains(':') {
                return Err(Fail::Run("neither value may contain ':'".into()));
            }
            Ok((Secret::join(&[&a, &b]), None))
        }
        CredentialShape::GoogleSignIn => {
            let (secret, export) = google_sign_in(http, gemini)?;
            Ok((secret, Some(export)))
        }
    }
}

/// One plain-text line, trimmed; empty when the person pressed Enter alone.
fn ask_line(question: &str) -> Result<String> {
    print!("{question}: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Fail::Run(format!("could not read the answer: {e}")))?;
    Ok(line.trim().to_string())
}

/// One plain-text answer; Enter alone takes `default` where there is one.
fn ask(question: &str, default: Option<&str>) -> Result<String> {
    let line = match default {
        Some(d) => ask_line(&format!("{question} (Enter for {d})"))?,
        None => ask_line(question)?,
    };
    match (line.is_empty(), default) {
        (true, Some(d)) => Ok(d.to_string()),
        (true, None) => Err(Fail::Run("nothing entered".into())),
        (false, _) => Ok(line),
    }
}

fn provider_fail(what: &str, e: omadai_core::provider::ProviderError) -> Fail {
    match e.requirement {
        Some(r) => Fail::Run(format!("{what}: {r}")),
        None => Fail::Run(format!("{what}: {}", e.message)),
    }
}

/// Asks for the export and the person's own OAuth client, signs in with Google, and finds the table.
fn google_sign_in(
    http: &dyn Transport,
    current: Option<&GeminiConfig>,
) -> Result<(Secret, GeminiConfig)> {
    if !std::io::stdin().is_terminal() {
        return Err(Fail::Run("key add gemini needs a terminal".into()));
    }
    let project = ask(
        "Google Cloud project that holds the billing export",
        current.map(|g| g.project.as_str()),
    )?;
    if !valid_project(&project) {
        return Err(Fail::Run(format!(
            "invalid project ID {project:?}: use lower-case letters, digits, '-', '.' or ':'"
        )));
    }
    let dataset = ask(
        "BigQuery dataset of the export",
        Some(current.map_or("billing_export", |g| g.dataset.as_str())),
    )?;
    if !valid_name(&dataset) {
        return Err(Fail::Run(format!(
            "invalid dataset {dataset:?}: use letters, digits and '_'"
        )));
    }
    // A client ID is public: it is printed in the consent URL below.
    let client_id = Secret::new(ask("OAuth client ID (Desktop app)", None)?);
    let client_secret = prompt("OAuth client secret")?;
    if client_id.expose().contains(':') || client_secret.expose().contains(':') {
        return Err(Fail::Run(
            "neither the client ID nor the secret may contain ':'".into(),
        ));
    }

    let loopback = Loopback::bind().map_err(Fail::Run)?;
    let port = loopback.port().map_err(Fail::Run)?;
    let pkce = google::Pkce::new().map_err(Fail::Run)?;
    let url = google::auth_url(client_id.expose(), port, &pkce.challenge, &pkce.state);
    println!("Opening your browser to sign in with Google:\n{url}");
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(&url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let code = loopback.wait_for_code(&pkce.state).map_err(Fail::Run)?;
    let tokens = google::exchange(
        http,
        &client_id,
        &client_secret,
        &code,
        &pkce.verifier,
        &google::redirect_uri(port),
    )
    .map_err(|e| provider_fail("Google sign-in failed", e))?;
    let scrub = Secret::join(&[
        &client_id,
        &client_secret,
        &code,
        &pkce.verifier,
        &tokens.refresh,
    ]);
    let stored = Secret::join(&[&client_id, &client_secret, &tokens.refresh]);
    find_export(http, &tokens.access, &scrub, stored, project, dataset).map_err(with_revoke_hint)
}

/// Reads the signed-in export's location and table; `scrub` joins every secret of the sign-in.
fn find_export(
    http: &dyn Transport,
    access: &Secret,
    scrub: &Secret,
    stored: Secret,
    project: String,
    dataset: String,
) -> Result<(Secret, GeminiConfig)> {
    let location = google::dataset_location(http, access, scrub, &project, &dataset)
        .map_err(|e| provider_fail("could not read the dataset", e))?;
    let mut tables = google::export_tables(http, access, scrub, &project, &dataset)
        .map_err(|e| provider_fail("could not list the dataset's tables", e))?;
    let table = match tables.len() {
        0 => {
            return Err(Fail::Run(format!(
                "dataset {dataset} in project {project} holds no {}* table; turn on Billing → Billing export → \
                 BigQuery export → Standard usage cost with this dataset, then run key add again",
                google::EXPORT_PREFIX
            )));
        }
        1 => tables.remove(0),
        _ => {
            for (i, t) in tables.iter().enumerate() {
                println!("{}", table_line(i, t));
            }
            let pick = ask("Which export table (number)", None)?;
            pick.parse::<usize>()
                .ok()
                .filter(|n| (1..=tables.len()).contains(n))
                .map(|n| tables.swap_remove(n - 1))
                .ok_or_else(|| Fail::Run(format!("no table numbered {pick:?}")))?
        }
    };
    if !valid_name(&table) || !valid_location(&location) {
        return Err(Fail::Run(format!(
            "unexpected table {table:?} or location {location:?}"
        )));
    }
    Ok((
        stored,
        GeminiConfig {
            project,
            dataset,
            table,
            location,
        },
    ))
}

fn key_rename(mut p: lexopt::Parser) -> Result<ExitCode> {
    let mut args = positionals(&mut p, 3)?.into_iter();
    let platform = platform_arg(args.next().unwrap())?;
    let from = label_arg(args.next().unwrap())?;
    let to = label_arg(args.next().unwrap())?;
    let paths = Paths::from_env()?;
    let _lock = keys_lock(&paths, &mut std::io::stderr())?;
    let mut config = Config::load(&paths.config_file())?;
    rename_key(
        keystore::open_default().as_ref(),
        &mut config,
        |c| c.save(&paths.config_file()),
        platform,
        &from,
        &to,
    )?;
    println!("Renamed {} key {from:?} to {to:?}.", platform.name);
    Ok(ExitCode::SUCCESS)
}

/// Relabels in the keystore, then in the config, putting the keystore back if the config cannot be saved.
fn rename_key(
    store: &dyn Keystore,
    config: &mut Config,
    save: impl FnOnce(&Config) -> std::result::Result<(), Error>,
    platform: &Platform,
    from: &str,
    to: &str,
) -> Result<()> {
    if !config.has_key(platform.id, from) {
        return Err(Fail::Run(format!(
            "{} has no key labelled {from:?}",
            platform.name
        )));
    }
    if config.has_key(platform.id, to) {
        return Err(Fail::Run(format!(
            "{} already has a key labelled {to:?}",
            platform.name
        )));
    }
    store.rename(platform.id, from, to)?;
    for k in config
        .keys
        .iter_mut()
        .filter(|k| k.platform == platform.id && k.label == from)
    {
        k.label = to.to_string();
    }
    if let Err(e) = save(config) {
        if let Err(back) = store.rename(platform.id, to, from) {
            return Err(Fail::Run(format!(
                "{e}; the keyring now holds the key as {to:?}, and renaming it back to {from:?} failed: {back}"
            )));
        }
        return Err(e.into());
    }
    Ok(())
}

fn key_remove(mut p: lexopt::Parser) -> Result<ExitCode> {
    let mut args = positionals(&mut p, 2)?.into_iter();
    let platform = platform_arg(args.next().unwrap())?;
    let label = label_arg(args.next().unwrap())?;
    let paths = Paths::from_env()?;
    let lock = keys_lock(&paths, &mut std::io::stderr())?;
    let mut config = Config::load(&paths.config_file())?;
    let (deleted, secret) = remove_key(
        keystore::open_default().as_ref(),
        &mut config,
        |c| c.save(&paths.config_file()),
        platform,
        &label,
    )?;
    drop(lock);
    if !deleted {
        eprintln!("note: the keyring held no credential for it; removed the label only");
    }
    println!("Removed {} key {label:?}.", platform.name);
    if platform.id == "gemini" {
        let answer = || {
            std::io::stdin()
                .is_terminal()
                .then(|| ask_line(REVOKE_QUESTION).ok())
                .flatten()
        };
        revoke_removed(
            default_transport().as_ref(),
            deleted,
            secret.as_ref(),
            answer,
            &mut std::io::stderr(),
        );
    }
    Ok(ExitCode::SUCCESS)
}

const REVOKE_QUESTION: &str = "Also revoke omadai's access at Google? This signs this Google account out of omadai \
     on every machine, and may sign it out of other apps whose OAuth client is in the same Google Cloud project. [y/N]";

/// After a Gemini key is removed here, revokes its grant at Google only on an explicit yes, saying each outcome in one stderr line, since the panel shows only stderr's first line.
fn revoke_removed(
    http: &dyn Transport,
    deleted: bool,
    stored: Option<&Secret>,
    answer: impl FnOnce() -> Option<String>,
    err: &mut dyn Write,
) {
    const BY_HAND: &str = "revoke omadai's access at myaccount.google.com/permissions";
    // A missing item already got its note from key_remove.
    if !deleted {
        return;
    }
    let Some(stored) = stored else {
        let _ = writeln!(
            err,
            "note: omadai could not read the stored sign-in, so Google's access was not revoked; to do it by hand, {BY_HAND}"
        );
        return;
    };
    let yes = answer().is_some_and(|a| matches!(a.to_ascii_lowercase().as_str(), "y" | "yes"));
    if !yes {
        let _ = writeln!(
            err,
            "Google's access is unchanged; revoke it at myaccount.google.com/permissions"
        );
        return;
    }
    let _ = match google::revoke_stored(http, stored) {
        Ok(google::Revoked::Done) => writeln!(
            err,
            "Revoked omadai's access at Google: this Google account is signed out of omadai on every machine, \
             and possibly of other apps whose OAuth client is in the same Google Cloud project."
        ),
        Ok(google::Revoked::AlreadyGone) => writeln!(
            err,
            "Google no longer accepts this sign-in's token, but other sign-ins may still have access; {BY_HAND}"
        ),
        Err(e) => writeln!(
            err,
            "warning: revoking omadai's access at Google failed ({}); {BY_HAND}",
            printable(e.message.as_str())
        ),
    };
}

/// Deletes from the keystore, then from the config, storing the credential again if the config cannot be saved; returns whether the keystore held it and the credential it read.
fn remove_key(
    store: &dyn Keystore,
    config: &mut Config,
    save: impl FnOnce(&Config) -> std::result::Result<(), Error>,
    platform: &Platform,
    label: &str,
) -> Result<(bool, Option<Secret>)> {
    // Best effort: an item the keystore cannot read back must still be removable.
    let secret = store.get(platform.id, label).ok().flatten();
    let deleted = store.delete(platform.id, label)?;
    let before = config.keys.len();
    config
        .keys
        .retain(|k| !(k.platform == platform.id && k.label == label));
    if config.keys.len() == before {
        if !deleted {
            return Err(Fail::Run(format!(
                "{} has no key labelled {label:?}",
                platform.name
            )));
        }
        return Ok((deleted, secret));
    }
    if let Err(e) = save(config) {
        if deleted {
            let again = "config.toml still lists the label, so run omadai key remove again";
            // The refresh token is gone, so a retry cannot offer the revoke; the panel keeps 300 characters, so this leads.
            let live = if platform.id == "gemini" {
                "the Google sign-in it held is still live, so revoke omadai's access at myaccount.google.com/permissions; "
            } else {
                ""
            };
            match &secret {
                Some(secret) => {
                    if let Err(back) = store.put(platform.id, label, secret) {
                        return Err(Fail::Run(format!(
                            "{live}{e}; the keyring no longer holds the key, and storing it again failed: {back}; {again}"
                        )));
                    }
                }
                None => {
                    return Err(Fail::Run(format!(
                        "{live}{e}; the keyring no longer holds the key, which could not be read back to restore; {again}"
                    )));
                }
            }
        }
        return Err(e.into());
    }
    Ok((deleted, secret))
}

fn key_list(mut p: lexopt::Parser) -> Result<ExitCode> {
    let mut json = false;
    while let Some(arg) = p.next()? {
        match arg {
            Long("json") => json = true,
            _ => return Err(arg.unexpected().into()),
        }
    }
    let paths = Paths::from_env()?;
    let config = Config::load(&paths.config_file())?;
    let mut out = String::new();
    if json {
        out = config.keys_json() + "\n";
    } else {
        for k in &config.keys {
            out.push_str(&format!("{}\t{}\n", k.platform, k.label));
        }
    }
    std::io::stdout()
        .lock()
        .write_all(out.as_bytes())
        .map_err(|e| Fail::Run(format!("write: {e}")))?;
    Ok(ExitCode::SUCCESS)
}

fn mode_line(label: &str, path: &Path, want: u32) -> (String, bool) {
    match fsutil::mode_of(path) {
        None => (
            format!("{label}: {} (not created yet)", path.display()),
            true,
        ),
        Some(m) if m == want => (format!("{label}: {} (mode {m:o})", path.display()), true),
        Some(m) => (
            format!(
                "{label}: {} (mode {m:o}, expected {want:o}) FAIL",
                path.display()
            ),
            false,
        ),
    }
}

/// Every file under `dir` not 0600 and every directory not 0700.
fn bad_modes(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let want = if meta.is_dir() {
            fsutil::DIR_MODE
        } else {
            fsutil::FILE_MODE
        };
        let mode = fsutil::mode_of(&path).unwrap_or(0);
        if mode != want {
            out.push(format!(
                "{} (mode {mode:o}, expected {want:o})",
                path.display()
            ));
        }
        if meta.is_dir() {
            bad_modes(&path, out);
        }
    }
}

fn cmd_doctor(mut p: lexopt::Parser) -> Result<ExitCode> {
    if let Some(arg) = p.next()? {
        return Err(arg.unexpected().into());
    }
    let paths = Paths::from_env()?;
    let mut ok = true;
    let mut lines = Vec::new();
    let store = keystore::open_default();
    lines.push(format!("keystore: {}", store.kind()));
    match store.is_locked() {
        Ok(false) => lines.push("keyring: unlocked".into()),
        Ok(true) => {
            ok = false;
            lines.push("keyring: LOCKED; fetch fails until it is unlocked FAIL".into());
        }
        Err(e) => {
            ok = false;
            lines.push(format!("keyring: {e} FAIL"));
        }
    }
    let config = Config::load(&paths.config_file());
    let opted_in = config
        .as_ref()
        .map(|c| c.allow_unencrypted_keyring)
        .unwrap_or(false);
    match keyring_file::detect(&paths.keyrings_dir) {
        KeyringEncryption::Encrypted => lines.push("keyring file: encrypted".into()),
        KeyringEncryption::Unencrypted(path) => lines.push(format!(
            "keyring file: UNENCRYPTED ({}); set a password on the keyring. WARN{}",
            path.display(),
            if opted_in {
                " (opt-in recorded in config)"
            } else {
                ""
            }
        )),
        KeyringEncryption::Unknown(why) => lines.push(format!("keyring file: unknown ({why})")),
    }
    let (l, good) = mode_line("config dir", &paths.config_dir, fsutil::DIR_MODE);
    ok &= good;
    lines.push(l);
    let (l, good) = mode_line("config", &paths.config_file(), fsutil::FILE_MODE);
    ok &= good;
    lines.push(l);
    match &config {
        Ok(c) => lines.push(format!("keys configured: {}", c.keys.len())),
        Err(e) => {
            ok = false;
            lines.push(format!("config: {e} FAIL"));
        }
    }
    let (l, good) = mode_line("state dir", &paths.state_dir, fsutil::DIR_MODE);
    ok &= good;
    lines.push(l);
    let mut bad = Vec::new();
    bad_modes(&paths.state_dir, &mut bad);
    if bad.is_empty() {
        lines.push("state files: private".into());
    } else {
        ok = false;
        for b in bad {
            lines.push(format!("state file: {b} FAIL"));
        }
    }
    println!("{}", lines.join("\n"));
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use omadai_core::keystore::memory::MemoryKeystore;

    fn config_with(label: &str) -> Config {
        Config {
            keys: vec![KeyEntry::new("openai", label)],
            ..Config::default()
        }
    }

    #[test]
    fn rename_puts_the_keystore_back_when_the_config_cannot_be_saved() {
        let store = MemoryKeystore::new();
        store
            .put("openai", "work", &Secret::new("fk-rename-1".into()))
            .unwrap();
        let openai = platform::find("openai").unwrap();
        let mut config = config_with("work");
        let failed = rename_key(
            &store,
            &mut config,
            |_| Err(Error::invalid("disk full")),
            openai,
            "work",
            "home",
        );
        assert!(matches!(failed, Err(Fail::Run(m)) if m == "disk full"));
        assert!(store.get("openai", "work").unwrap().is_some());
        assert!(store.get("openai", "home").unwrap().is_none());

        let mut config = config_with("work");
        assert!(rename_key(&store, &mut config, |_| Ok(()), openai, "work", "home").is_ok());
        assert!(store.get("openai", "home").unwrap().is_some());
        assert!(config.has_key("openai", "home"));
    }

    /// A memory keystore whose compensating steps fail: renaming back from `home`, and every `put`.
    struct NoUndo(MemoryKeystore);

    impl Keystore for NoUndo {
        fn kind(&self) -> &'static str {
            self.0.kind()
        }
        fn is_locked(&self) -> std::result::Result<bool, KeystoreError> {
            self.0.is_locked()
        }
        fn get(&self, p: &str, l: &str) -> std::result::Result<Option<Secret>, KeystoreError> {
            if l == "unreadable" {
                return Err(KeystoreError::Backend(
                    "stored credential is not UTF-8".into(),
                ));
            }
            self.0.get(p, l)
        }
        fn put(&self, _: &str, _: &str, _: &Secret) -> std::result::Result<(), KeystoreError> {
            Err(KeystoreError::Backend("put refused".into()))
        }
        fn delete(&self, p: &str, l: &str) -> std::result::Result<bool, KeystoreError> {
            self.0.delete(p, l)
        }
        fn rename(&self, p: &str, from: &str, to: &str) -> std::result::Result<(), KeystoreError> {
            if from == "home" {
                return Err(KeystoreError::Backend("rename refused".into()));
            }
            self.0.rename(p, from, to)
        }
        fn list(&self) -> std::result::Result<Vec<omadai_core::keystore::Entry>, KeystoreError> {
            self.0.list()
        }
    }

    #[test]
    fn rename_reports_both_errors_when_it_cannot_put_the_keystore_back() {
        let store = NoUndo(MemoryKeystore::new());
        store
            .0
            .put("openai", "work", &Secret::new("fk-rename-2".into()))
            .unwrap();
        let openai = platform::find("openai").unwrap();
        let failed = rename_key(
            &store,
            &mut config_with("work"),
            |_| Err(Error::invalid("disk full")),
            openai,
            "work",
            "home",
        );
        let Err(Fail::Run(m)) = failed else {
            panic!("rename succeeded")
        };
        assert!(m.starts_with("disk full; "), "{m}");
        assert!(m.contains("rename refused"), "{m}");
    }

    #[test]
    fn remove_still_removes_an_item_the_keystore_cannot_read() {
        let store = NoUndo(MemoryKeystore::new());
        store
            .0
            .put("openai", "unreadable", &Secret::new("fk-remove-2".into()))
            .unwrap();
        let openai = platform::find("openai").unwrap();
        let removed = remove_key(
            &store,
            &mut config_with("unreadable"),
            |_| Ok(()),
            openai,
            "unreadable",
        );
        assert!(matches!(removed, Ok((true, None))), "removal was blocked");
        assert!(store.0.get("openai", "unreadable").unwrap().is_none());
    }

    #[test]
    fn remove_says_so_when_an_unreadable_item_is_gone_and_the_config_save_fails() {
        let store = NoUndo(MemoryKeystore::new());
        store
            .0
            .put("openai", "unreadable", &Secret::new("fk-remove-3".into()))
            .unwrap();
        let openai = platform::find("openai").unwrap();
        let failed = remove_key(
            &store,
            &mut config_with("unreadable"),
            |_| Err(Error::invalid("disk full")),
            openai,
            "unreadable",
        );
        let Err(Fail::Run(m)) = failed else {
            panic!("expected a reported failure")
        };
        assert!(
            m.contains("could not be read back") && m.contains("key remove again"),
            "{m}"
        );
    }

    #[test]
    fn remove_puts_the_credential_back_when_the_config_cannot_be_saved() {
        let store = MemoryKeystore::new();
        store
            .put("openai", "work", &Secret::new("fk-remove-1".into()))
            .unwrap();
        let openai = platform::find("openai").unwrap();
        let mut config = config_with("work");
        let failed = remove_key(
            &store,
            &mut config,
            |_| Err(Error::invalid("disk full")),
            openai,
            "work",
        );
        assert!(matches!(failed, Err(Fail::Run(m)) if m == "disk full"));
        assert_eq!(
            store.get("openai", "work").unwrap().unwrap().expose(),
            "fk-remove-1"
        );

        let mut config = config_with("work");
        assert!(matches!(
            remove_key(&store, &mut config, |_| Ok(()), openai, "work"),
            Ok((true, Some(_)))
        ));
        assert!(store.get("openai", "work").unwrap().is_none());
        assert!(!config.has_key("openai", "work"));

        let undo = NoUndo(MemoryKeystore::new());
        undo.0
            .put("openai", "work", &Secret::new("fk-remove-2".into()))
            .unwrap();
        let failed = remove_key(
            &undo,
            &mut config_with("work"),
            |_| Err(Error::invalid("disk full")),
            openai,
            "work",
        );
        let Err(Fail::Run(m)) = failed else {
            panic!("remove succeeded")
        };
        assert!(m.starts_with("disk full; "), "{m}");
        assert!(m.contains("put refused"), "{m}");
        assert!(!m.contains("fk-remove-2"), "{m}");
    }

    /// Answers every request with `status`, recording each URL.
    struct FakeGoogle {
        status: u16,
        error: &'static str,
        urls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeGoogle {
        fn new(status: u16) -> Self {
            Self::refusing(status, r#"{"error":"backend_error"}"#)
        }

        fn refusing(status: u16, error: &'static str) -> Self {
            FakeGoogle {
                status,
                error,
                urls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn urls(&self) -> Vec<String> {
            self.urls.lock().unwrap().clone()
        }
    }

    impl Transport for FakeGoogle {
        fn send(
            &self,
            request: &omadai_core::http::Request,
            _credential: &Secret,
        ) -> std::result::Result<omadai_core::http::Response, omadai_core::http::HttpError>
        {
            self.urls.lock().unwrap().push(request.url.clone());
            if self.status == 200 {
                return Ok(omadai_core::http::Response {
                    status: 200,
                    body: b"{}".to_vec(),
                });
            }
            Err(omadai_core::http::HttpError::Status {
                status: self.status,
                body: omadai_core::scrub::plain(self.error),
            })
        }
    }

    const SIGNED_IN: &str = "fk-client-id:fk-client-secret-0123:fk-refresh-0123456789";

    fn text(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    fn signed_in() -> Secret {
        Secret::new(SIGNED_IN.into())
    }

    /// Runs `revoke_removed` for a removed item holding `stored`, answering `answer`; returns what it wrote to stderr.
    fn remove_answering(
        google: &FakeGoogle,
        deleted: bool,
        stored: Option<&Secret>,
        answer: Option<&str>,
    ) -> String {
        let mut err = Vec::new();
        revoke_removed(
            google,
            deleted,
            stored,
            || answer.map(str::to_string),
            &mut err,
        );
        text(err)
    }

    /// The panel keeps only the first stderr line, so every outcome must be exactly one line.
    fn one_line(err: &str) -> &str {
        assert_eq!(err.lines().count(), 1, "{err}");
        err.trim_end()
    }

    #[test]
    fn a_failure_keeps_its_own_line_breaks_and_escapes_each_line() {
        let m = "refusing to store a credential: plain text.\nSet a password, then retry.\nTo store it anyway, rerun with --allow-unencrypted-keyring.";
        let line = run_failure_line(m);
        assert_eq!(line.lines().count(), 3, "{line}");
        assert!(!line.contains("\\n"), "{line}");
        assert_eq!(
            run_failure_line("first\nbad \u{1b}[2Jkey"),
            "omadai: first\nbad \\u{1b}[2Jkey"
        );
    }

    #[test]
    fn vendor_text_reaches_the_terminal_with_escape_sequences_escaped() {
        assert_eq!(
            run_failure_line("bad \u{1b}[2Jkey"),
            "omadai: bad \\u{1b}[2Jkey"
        );
        assert_eq!(
            table_line(0, "t\u{1b}[31m\u{202e}x"),
            "  1. t\\u{1b}[31m\\u{202e}x"
        );
        let google = FakeGoogle::refusing(500, "{\"error\":\"\u{1b}[31mbackend\"}");
        let err = remove_answering(&google, true, Some(&signed_in()), Some("y"));
        assert!(!err.contains('\u{1b}'), "{err:?}");
    }

    #[test]
    fn removing_a_gemini_key_revokes_only_on_yes_after_the_local_delete() {
        let store = MemoryKeystore::new();
        store.put("gemini", "default", &signed_in()).unwrap();
        let gemini = platform::find("gemini").unwrap();
        let mut config = Config {
            keys: vec![KeyEntry::new("gemini", "default")],
            ..Config::default()
        };
        let Ok((deleted, secret)) = remove_key(&store, &mut config, |_| Ok(()), gemini, "default")
        else {
            panic!("removal failed")
        };
        assert!(deleted);
        assert!(store.get("gemini", "default").unwrap().is_none());
        for yes in ["y", "Y", "yes"] {
            let google = FakeGoogle::new(200);
            let err = remove_answering(&google, true, secret.as_ref(), Some(yes));
            assert_eq!(google.urls(), [google::REVOKE_URL]);
            let line = one_line(&err);
            assert!(
                line.starts_with("Revoked omadai's access at Google"),
                "{line}"
            );
            assert!(line.contains("Google Cloud project"), "{line}");
        }
    }

    #[test]
    fn removing_without_a_yes_leaves_googles_access_alone() {
        // None is a run with no terminal to ask on.
        for answer in [Some("n"), Some(""), Some("sure"), None] {
            let google = FakeGoogle::new(200);
            let err = remove_answering(&google, true, Some(&signed_in()), answer);
            assert!(google.urls().is_empty(), "{answer:?} revoked");
            let line = one_line(&err);
            assert!(line.starts_with("Google's access is unchanged"), "{line}");
            assert!(line.contains("myaccount.google.com/permissions"), "{line}");
            assert!(
                line.len() <= 300,
                "the panel cuts its note at 300 characters: {line}"
            );
        }
    }

    #[test]
    fn a_sign_in_google_no_longer_knows_is_not_reported_as_revoked() {
        let google = FakeGoogle::refusing(400, r#"{"error":"invalid_token"}"#);
        let err = remove_answering(&google, true, Some(&signed_in()), Some("y"));
        let line = one_line(&err);
        assert!(
            line.contains("other sign-ins may still have access"),
            "{line}"
        );
        assert!(line.contains("myaccount.google.com/permissions"), "{line}");
        assert!(!line.contains("Revoked"), "{line}");
    }

    #[test]
    fn a_failed_revoke_on_remove_only_warns() {
        let google = FakeGoogle::new(500);
        let err = remove_answering(&google, true, Some(&signed_in()), Some("y"));
        let line = one_line(&err);
        assert!(line.starts_with("warning: revoking"), "{line}");
        assert!(err.contains("myaccount.google.com/permissions"), "{err}");
        for part in ["fk-client-secret", "fk-refresh"] {
            assert!(!err.contains(part), "{err}");
        }
    }

    #[test]
    fn an_unreadable_sign_in_is_noted_and_a_missing_one_is_not() {
        let google = FakeGoogle::new(200);
        let err = remove_answering(&google, true, None, Some("y"));
        assert!(
            one_line(&err).contains("could not read the stored sign-in"),
            "{err}"
        );
        // A missing item already got key_remove's "held no credential" note, and nothing more.
        let err = remove_answering(&google, false, None, Some("y"));
        assert!(err.is_empty(), "{err}");
        assert!(google.urls().is_empty());
    }

    #[test]
    fn the_revoke_question_names_the_project_wide_reach_on_one_line() {
        assert!(!REVOKE_QUESTION.contains('\n'), "{REVOKE_QUESTION}");
        assert!(
            REVOKE_QUESTION.contains("every machine"),
            "{REVOKE_QUESTION}"
        );
        assert!(
            REVOKE_QUESTION.contains("same Google Cloud project"),
            "{REVOKE_QUESTION}"
        );
        assert!(REVOKE_QUESTION.ends_with("[y/N]"), "{REVOKE_QUESTION}");
    }

    #[test]
    fn a_gemini_removal_that_loses_the_sign_in_says_it_is_still_live() {
        let gemini = platform::find("gemini").unwrap();
        let config = || Config {
            keys: vec![KeyEntry::new("gemini", "default")],
            ..Config::default()
        };
        // The credential read back but could not be stored again.
        let undo = NoUndo(MemoryKeystore::new());
        undo.0.put("gemini", "default", &signed_in()).unwrap();
        let Err(Fail::Run(m)) = remove_key(
            &undo,
            &mut config(),
            |_| Err(Error::invalid("disk full")),
            gemini,
            "default",
        ) else {
            panic!("expected a reported failure")
        };
        assert!(m.contains("put refused"), "{m}");
        // The panel keeps the first 300 characters, so the way out must lead and must not say to sign in again.
        let link = m
            .find("myaccount.google.com/permissions")
            .expect("revoke link");
        assert!(
            link + "myaccount.google.com/permissions".len() <= 300,
            "{m}"
        );
        assert!(!m.contains("sign in again"), "{m}");
        assert!(
            !m.contains("fk-refresh") && !m.contains("fk-client-secret"),
            "{m}"
        );
        // The credential could not be read back at all.
        let unreadable = NoUndo(MemoryKeystore::new());
        unreadable
            .0
            .put("gemini", "unreadable", &signed_in())
            .unwrap();
        let mut c = Config {
            keys: vec![KeyEntry::new("gemini", "unreadable")],
            ..Config::default()
        };
        let Err(Fail::Run(m)) = remove_key(
            &unreadable,
            &mut c,
            |_| {
                Err(Error::invalid(format!(
                    "write {}/.config/omadai/config.toml: No space left on device (os error 28)",
                    "/var/a-rather-long-user-name/and/a/deeper/var/directory/than/usual"
                )))
            },
            gemini,
            "unreadable",
        ) else {
            panic!("expected a reported failure")
        };
        assert!(m.contains("could not be read back"), "{m}");
        // The panel keeps the first 300 characters, so the way out must lead and must not say to sign in again.
        let link = m
            .find("myaccount.google.com/permissions")
            .expect("revoke link");
        assert!(
            link + "myaccount.google.com/permissions".len() <= 300,
            "{m}"
        );
        assert!(!m.contains("sign in again"), "{m}");
        // Other platforms keep their message as it was.
        let openai = platform::find("openai").unwrap();
        let undo = NoUndo(MemoryKeystore::new());
        undo.0
            .put("openai", "work", &Secret::new("fk-remove-9".into()))
            .unwrap();
        let Err(Fail::Run(m)) = remove_key(
            &undo,
            &mut config_with("work"),
            |_| Err(Error::invalid("disk full")),
            openai,
            "work",
        ) else {
            panic!("expected a reported failure")
        };
        assert!(!m.contains(google::REVOKE_HINT), "{m}");
    }

    #[test]
    fn a_gemini_add_refused_before_storing_revokes_nothing() {
        let t = TestPaths::new("gemini-refused");
        let store = MemoryKeystore::new();
        let gemini = platform::find("gemini").unwrap();
        Config {
            keys: vec![KeyEntry::new("gemini", "first")],
            ..Config::default()
        }
        .save(&t.0.config_file())
        .unwrap();
        let google = FakeGoogle::new(200);
        let added = add_key(
            &t.0,
            &store,
            gemini,
            "second",
            &signed_in(),
            |_| {},
            |c| Registry::builtin().check("gemini", &google, &signed_in(), c),
        );
        let Err(Fail::Run(m)) = added.map_err(with_revoke_hint) else {
            panic!("a second Gemini key was added")
        };
        assert!(google.urls().is_empty(), "a refused add called Google");
        assert!(m.contains(google::REVOKE_HINT), "{m}");
    }

    #[test]
    fn a_failed_export_lookup_after_sign_in_revokes_nothing() {
        let google = FakeGoogle::new(500);
        let scrub = signed_in();
        let found = find_export(
            &google,
            &Secret::new("fk-access-0123456789".into()),
            &scrub,
            signed_in(),
            "example-studio-1234".into(),
            "billing_export".into(),
        );
        let Err(Fail::Run(m)) = found.map_err(with_revoke_hint) else {
            panic!("the export was found")
        };
        assert!(!google.urls().is_empty(), "find_export called nothing");
        assert!(
            !google.urls().iter().any(|u| u == google::REVOKE_URL),
            "a failed lookup revoked the sign-in"
        );
        assert!(m.contains(google::REVOKE_HINT), "{m}");
        for part in ["fk-client-secret", "fk-refresh", "fk-access"] {
            assert!(!m.contains(part), "{m}");
        }
    }

    /// Private, empty directories for one test, removed on drop.
    struct TestPaths(Paths);

    impl TestPaths {
        fn new(tag: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("omadai-cli-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            TestPaths(Paths {
                home: Some(root.join("home")),
                config_dir: root.join("config"),
                state_dir: root.join("state"),
                keyrings_dir: root.join("keyrings"),
            })
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            if let Some(root) = self.0.state_dir.parent() {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }

    #[test]
    fn add_runs_the_check_without_the_key_lock() {
        let t = TestPaths::new("add-lock");
        let store = MemoryKeystore::new();
        let openai = platform::find("openai").unwrap();
        let secret = Secret::new("fk-add-1".into());
        let mut checked = false;
        let added = add_key(
            &t.0,
            &store,
            openai,
            "work",
            &secret,
            |_| {},
            |config| {
                assert!(config.has_key("openai", "work"));
                let free = lock::try_acquire(&t.0.keys_lock_file()).unwrap();
                assert!(free.is_some(), "the key lock is held during the check");
                checked = true;
                CheckOutcome::Ok("fake".into())
            },
        );
        assert!(added.is_ok());
        assert!(checked);
        assert!(store.get("openai", "work").unwrap().is_some());
        assert!(
            Config::load(&t.0.config_file())
                .unwrap()
                .has_key("openai", "work")
        );
    }

    #[test]
    fn add_refuses_a_second_gemini_key_saved_while_the_prompts_waited() {
        let t = TestPaths::new("gemini-one");
        let store = MemoryKeystore::new();
        let gemini = platform::find("gemini").unwrap();
        // The check before the prompts saw no Gemini key; another add saved one since.
        Config {
            keys: vec![KeyEntry::new("gemini", "first")],
            ..Config::default()
        }
        .save(&t.0.config_file())
        .unwrap();
        let mut checked = false;
        let added = add_key(
            &t.0,
            &store,
            gemini,
            "second",
            &Secret::new("fk-id:fk-secret:fk-refresh".into()),
            |_| {},
            |_| {
                checked = true;
                CheckOutcome::NoProvider
            },
        );
        let Err(Fail::Run(m)) = added else {
            panic!("a second Gemini key was added")
        };
        assert!(m.contains("omadai key remove gemini first"), "{m}");
        assert!(!checked);
        assert!(store.get("gemini", "second").unwrap().is_none());
        let config = Config::load(&t.0.config_file()).unwrap();
        assert!(!config.has_key("gemini", "second"));
    }

    #[test]
    fn rename_keeps_the_series_and_a_reused_label_gets_a_new_one() {
        let t = TestPaths::new("series");
        let store = MemoryKeystore::new();
        let flux = platform::find("flux").unwrap();
        let series = |config: &Config| config.keys[0].series.clone();
        let add = |label: &str, key: &str| {
            add_key(
                &t.0,
                &store,
                flux,
                label,
                &Secret::new(key.into()),
                |_| {},
                |_| CheckOutcome::NoProvider,
            )
            .unwrap_or_else(|_| panic!("add failed"));
            Config::load(&t.0.config_file()).unwrap()
        };
        let mut config = add("render", "fk-bfl-SERIES0001");
        let first = series(&config);
        rename_key(
            &store,
            &mut config,
            |c| c.save(&t.0.config_file()),
            flux,
            "render",
            "studio",
        )
        .unwrap_or_else(|_| panic!("rename failed"));
        let renamed = Config::load(&t.0.config_file()).unwrap();
        assert_eq!(series(&renamed), first, "a rename keeps the history");
        let mut config = renamed;
        remove_key(
            &store,
            &mut config,
            |c| c.save(&t.0.config_file()),
            flux,
            "studio",
        )
        .unwrap_or_else(|_| panic!("remove failed"));
        let again = add("studio", "fk-bfl-SERIES0002");
        assert_ne!(
            series(&again),
            first,
            "a reused label starts a fresh series"
        );
    }

    #[test]
    fn key_lock_says_once_that_it_is_waiting() {
        let t = TestPaths::new("keys-wait");
        let mut quiet = Vec::new();
        assert!(keys_lock(&t.0, &mut quiet).is_ok());
        assert!(quiet.is_empty(), "no notice without contention");

        let held = lock::acquire(&t.0.keys_lock_file()).unwrap();
        let said = std::thread::scope(|s| {
            let waiter = s.spawn(|| {
                let mut said = Vec::new();
                let waited = keys_lock(&t.0, &mut said);
                assert!(waited.is_ok());
                said
            });
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(held);
            waiter.join().unwrap()
        });
        assert_eq!(
            String::from_utf8(said).unwrap(),
            "waiting: another omadai key command is running\n"
        );
    }
}
