//! Project configuration: `pgmig.toml`.
//!
//! Every key is named after the command-line flag it stands for, so a flag and its key never
//! disagree about what they mean:
//!
//! | key | flag | type |
//! |---|---|---|
//! | `url` | `--url` (the target database) | string |
//! | `shadow` | `--shadow` | string |
//! | `from` | `--from` | string |
//! | `to` | `--to`; also the `SOURCE` of `schema inspect` and `schema validate` | string |
//! | `include` | `--include` | array of strings |
//! | `exclude` | `--exclude` | array of strings |
//! | `schema` | `--schema` (manage only this schema's objects) | string |
//! | `format` | `--format` | `"text"` or `"json"` |
//! | `migrations.dir` | `--dir` | string |
//! | `migrations.revisions_schema` | (none) the schema of the revisions table, never managed | string |
//!
//! Top-level keys apply everywhere; an `[env.<name>]` section overrides them key by key (an array
//! replaces the one below it, it does not extend it). [`Config::resolve`] applies the precedence
//! below the command line (`[env.<name>]` over top level); the caller puts its flags on top and
//! its built-in defaults underneath. Any other key is an error naming its path
//! (`env.prod.urll`); diff policy and lint rules will get their own sections.
//!
//! `${NAME}` in a string value is replaced by a `--var NAME=value`, or else by the environment
//! variable `NAME` ([`Vars`]); an undefined name is an error naming the key. `$${` is a literal
//! `${`, and a `$` not followed by `{` is itself. Placeholders are checked for syntax in every
//! section when the file is parsed, but only the values in use (the top level and the selected
//! environment) are interpolated, so an environment's variables need only be set when it is
//! selected.
//!
//! Values may hold secrets (a password in a URL): errors name keys and variables, never values.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// The file looked for in the current directory when no path is given.
pub const FILE_NAME: &str = "pgmig.toml";

/// One level of settings (the top level or an environment), or the merged result. Its `Debug`
/// redacts passwords in the values that may be connection strings.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub url: Option<String>,
    pub shadow: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub schema: Option<String>,
    /// `"text"` or `"json"` (checked when parsed).
    pub format: Option<String>,
    pub migrations_dir: Option<String>,
    /// `[migrations] revisions_schema`: where the revisions table lives (pgmig's default is `pgmig`).
    pub migrations_revisions_schema: Option<String>,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = |v: &Option<String>| v.as_deref().map(redact);
        f.debug_struct("Settings")
            .field("url", &r(&self.url))
            .field("shadow", &r(&self.shadow))
            .field("from", &r(&self.from))
            .field("to", &r(&self.to))
            .field("include", &self.include)
            .field("exclude", &self.exclude)
            .field("schema", &self.schema)
            .field("format", &self.format)
            .field("migrations_dir", &self.migrations_dir)
            .field("migrations_revisions_schema", &self.migrations_revisions_schema)
            .finish()
    }
}

/// A parsed `pgmig.toml`: values not yet interpolated. `Debug` is safe to print: [`Settings`]
/// redacts its connection strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub path: PathBuf,
    pub top: Settings,
    pub envs: BTreeMap<String, Settings>,
}

/// One thing wrong with a file. `key` is the dotted path of the offending key, when there is one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Problem {
    pub key: Option<String>,
    pub message: String,
    /// 1-based position in the file, for syntax errors.
    pub line: Option<u32>,
    pub col: Option<u32>,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let (Some(line), Some(col)) = (self.line, self.col) {
            write!(f, "{line}:{col}: ")?;
        }
        if let Some(key) = &self.key {
            write!(f, "{key}: ")?;
        }
        f.write_str(&self.message)
    }
}

impl Problem {
    /// `pgmig.toml:2:5: message` or `pgmig.toml: key: message`.
    pub fn in_file(&self, path: &Path) -> String {
        let sep = if self.line.is_some() { "" } else { " " };
        format!("{}:{sep}{self}", path.display())
    }

    fn key(key: &str, message: impl Into<String>) -> Problem {
        Problem { key: Some(key.to_owned()), message: message.into(), line: None, col: None }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: std::io::Error },
    /// Everything wrong with the file, in key order.
    #[error("{}", render(path, problems))]
    Invalid { path: PathBuf, problems: Vec<Problem> },
    /// A `--var` argument without `=`. Its text is not kept: it may be a secret.
    #[error("--var expects NAME=VALUE")]
    VarSyntax,
    /// A `--var NAME=value` whose name is not valid.
    #[error("--var {name:?}: expected NAME=value, NAME made of letters, digits and `_`, not starting with a digit")]
    Var { name: String },
}

fn render(path: &Path, problems: &[Problem]) -> String {
    let lines: Vec<String> = problems.iter().map(|p| p.in_file(path)).collect();
    lines.join("\n")
}

/// Where `${NAME}` placeholders get their values: `--var` first, then (unless disabled) the
/// process environment. `Debug` shows the names only.
#[derive(Clone)]
pub struct Vars {
    vars: BTreeMap<String, String>,
    environment: bool,
}

impl fmt::Debug for Vars {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vars").field("names", &self.vars.keys().collect::<Vec<_>>()).field("environment", &self.environment).finish()
    }
}

impl Default for Vars {
    fn default() -> Vars {
        Vars { vars: BTreeMap::new(), environment: true }
    }
}

impl Vars {
    /// From `--var NAME=value` arguments, falling back to the environment.
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Result<Vars, ConfigError> {
        parse_vars(args).map(Vars::new)
    }

    /// These values, falling back to the environment.
    pub fn new(vars: BTreeMap<String, String>) -> Vars {
        Vars { vars, environment: true }
    }

    /// Only these values; the environment is not consulted.
    pub fn only(vars: BTreeMap<String, String>) -> Vars {
        Vars { vars, environment: false }
    }

    pub fn get(&self, name: &str) -> Option<String> {
        match self.vars.get(name) {
            Some(v) => Some(v.clone()),
            None if self.environment => std::env::var(name).ok(),
            None => None,
        }
    }
}

/// Parses `--var NAME=value` arguments (a later one wins).
pub fn parse_vars<S: AsRef<str>>(args: &[S]) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut vars = BTreeMap::new();
    for arg in args {
        let arg = arg.as_ref();
        // Only the name goes into the error, and without `=` nothing: the value may be a secret.
        let Some((name, value)) = arg.split_once('=') else {
            return Err(ConfigError::VarSyntax);
        };
        if !valid_name(name) {
            return Err(ConfigError::Var { name: name.to_owned() });
        }
        vars.insert(name.to_owned(), value.to_owned());
    }
    Ok(vars)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Replaces `${NAME}` placeholders; `$${` is a literal `${`. `key` names the value in errors.
fn interpolate(s: &str, key: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, Problem> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(after) = rest.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return Err(Problem::key(key, "unterminated `${` (write `$${` for a literal `${`)"));
            };
            let name = &after[..end];
            if !valid_name(name) {
                return Err(Problem::key(key, "invalid variable name in `${...}`: use letters, digits and `_`, not starting with a digit"));
            }
            match lookup(name) {
                Some(value) => out.push_str(&value),
                None => {
                    return Err(Problem::key(
                        key,
                        format!("undefined variable `{name}` (set it in the environment or pass --var {name}=...)"),
                    ));
                }
            }
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Writes a key the way TOML would need it in a dotted path.
fn key_segment(key: &str) -> String {
    if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        key.to_owned()
    } else {
        format!("{key:?}")
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() { key_segment(key) } else { format!("{prefix}.{}", key_segment(key)) }
}

const SETTING_KEYS: &[&str] = &["url", "shadow", "from", "to", "include", "exclude", "schema", "format", "migrations"];

/// Reads one level's settings from `table`, recording what is wrong. `top` allows `env`.
fn settings(table: &toml::Table, prefix: &str, problems: &mut Vec<Problem>) -> Settings {
    let mut s = Settings::default();
    for (key, value) in table {
        let path = join(prefix, key);
        let string = |problems: &mut Vec<Problem>| match value {
            toml::Value::String(v) => {
                // Syntax only: values are looked up when the level is in use.
                if let Err(p) = interpolate(v, &path, &|_| Some(String::new())) {
                    problems.push(p);
                }
                Some(v.clone())
            }
            _ => {
                problems.push(Problem::key(&path, "expected a string"));
                None
            }
        };
        let strings = |problems: &mut Vec<Problem>| match value {
            toml::Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    match item {
                        toml::Value::String(v) => {
                            if let Err(p) = interpolate(v, &format!("{path}[{i}]"), &|_| Some(String::new())) {
                                problems.push(p);
                            }
                            out.push(v.clone());
                        }
                        _ => problems.push(Problem::key(&format!("{path}[{i}]"), "expected a string")),
                    }
                }
                Some(out)
            }
            _ => {
                problems.push(Problem::key(&path, "expected an array of strings"));
                None
            }
        };
        match key.as_str() {
            "url" => s.url = string(problems),
            "shadow" => s.shadow = string(problems),
            "from" => s.from = string(problems),
            "to" => s.to = string(problems),
            "schema" => s.schema = string(problems),
            "include" => s.include = strings(problems),
            "exclude" => s.exclude = strings(problems),
            "format" => {
                s.format = string(problems);
                // A value with a placeholder is checked once interpolated.
                if s.format.as_deref().is_some_and(|f| !f.contains('$') && f != "text" && f != "json") {
                    problems.push(Problem::key(&path, "expected \"text\" or \"json\""));
                    s.format = None;
                }
            }
            "migrations" => match value {
                toml::Value::Table(m) => {
                    for (k, v) in m {
                        let path = join(&path, k);
                        match (k.as_str(), v) {
                            (key @ ("dir" | "revisions_schema"), toml::Value::String(v)) => {
                                if let Err(p) = interpolate(v, &path, &|_| Some(String::new())) {
                                    problems.push(p);
                                }
                                if key == "dir" {
                                    s.migrations_dir = Some(v.clone());
                                } else if v.is_empty() {
                                    problems.push(Problem::key(&path, "expected a schema name, not an empty string"));
                                } else {
                                    s.migrations_revisions_schema = Some(v.clone());
                                }
                            }
                            ("dir" | "revisions_schema", _) => problems.push(Problem::key(&path, "expected a string")),
                            _ => problems.push(Problem::key(&path, "unknown key (expected `dir` or `revisions_schema`)")),
                        }
                    }
                }
                _ => problems.push(Problem::key(&path, "expected a table")),
            },
            "env" if prefix.is_empty() => {} // handled by the caller
            _ => problems.push(Problem::key(&path, unknown_key_message(prefix.is_empty()))),
        }
    }
    s
}

fn unknown_key_message(top: bool) -> String {
    let mut keys: Vec<&str> = SETTING_KEYS.to_vec();
    if top {
        keys.push("env");
    }
    format!("unknown key (expected one of {})", keys.iter().map(|k| format!("`{k}`")).collect::<Vec<_>>().join(", "))
}

/// 1-based line and character column of byte offset `at`.
fn position(text: &str, at: usize) -> (u32, u32) {
    let before = &text[..text.floor_char_boundary(at.min(text.len()))];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (u32::try_from(line).unwrap_or(u32::MAX), u32::try_from(col).unwrap_or(u32::MAX))
}

impl Config {
    /// Parses `text`; `path` is where it came from, for messages.
    pub fn parse(text: &str, path: impl Into<PathBuf>) -> Result<Config, ConfigError> {
        let path = path.into();
        let table = match text.parse::<toml::Table>() {
            Ok(t) => t,
            Err(e) => {
                // The message only: the rendered error would quote the line, which may hold a secret.
                let (line, col) = e.span().map(|s| position(text, s.start)).unzip();
                let problem = Problem { key: None, message: e.message().to_owned(), line, col };
                return Err(ConfigError::Invalid { path, problems: vec![problem] });
            }
        };
        let mut problems = Vec::new();
        let top = settings(&table, "", &mut problems);
        let mut envs = BTreeMap::new();
        match table.get("env") {
            None => {}
            Some(toml::Value::Table(sections)) => {
                for (name, section) in sections {
                    let prefix = join("env", name);
                    match section {
                        toml::Value::Table(t) => {
                            envs.insert(name.clone(), settings(t, &prefix, &mut problems));
                        }
                        _ => problems.push(Problem::key(&prefix, "expected a table ([env.<name>])")),
                    }
                }
            }
            Some(_) => problems.push(Problem::key("env", "expected a table of environments ([env.<name>])")),
        }
        problems.sort_by(|a, b| a.key.cmp(&b.key));
        if problems.is_empty() { Ok(Config { path, top, envs }) } else { Err(ConfigError::Invalid { path, problems }) }
    }

    /// Reads and parses the file at `path`.
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io { path: path.to_owned(), source })?;
        Config::parse(&text, path)
    }

    /// `path` if given (it must exist), else `pgmig.toml` in `dir` if there is one.
    pub fn discover(path: Option<&Path>, dir: &Path) -> Result<Option<Config>, ConfigError> {
        match path {
            Some(path) => Config::load(path).map(Some),
            None => {
                let path = dir.join(FILE_NAME);
                if path.is_file() { Config::load(&path).map(Some) } else { Ok(None) }
            }
        }
    }

    /// The key `url` is read from for `env`: `env.<name>.url` or `url`; `None` when neither is set.
    pub fn url_key(&self, env: Option<&str>) -> Option<String> {
        match env.and_then(|name| self.envs.get(name).map(|s| (name, s))) {
            Some((name, s)) if s.url.is_some() => Some(join(&join("env", name), "url")),
            _ => self.top.url.is_some().then(|| "url".to_owned()),
        }
    }

    /// The settings in use for `env` (none: the top level only): the environment's keys over the
    /// top level's, interpolated with `vars`. Reports every problem at once.
    pub fn resolve(&self, env: Option<&str>, vars: &Vars) -> Result<Settings, ConfigError> {
        let empty = Settings::default();
        let (section, prefix) = match env {
            None => (&empty, String::new()),
            Some(name) => match self.envs.get(name) {
                Some(s) => (s, join("env", name)),
                None => {
                    let known = if self.envs.is_empty() {
                        "the file defines none".to_owned()
                    } else {
                        format!("defined: {}", self.envs.keys().map(String::as_str).collect::<Vec<_>>().join(", "))
                    };
                    let message = format!("no environment `{name}` (--env); {known}");
                    return Err(ConfigError::Invalid {
                        path: self.path.clone(),
                        problems: vec![Problem::key(&join("env", name), message)],
                    });
                }
            },
        };
        let mut problems = Vec::new();
        let lookup = |name: &str| vars.get(name);
        let mut one = |env: &Option<String>, top: &Option<String>, key: &str| -> Option<String> {
            let (value, path) = match (env, top) {
                (Some(v), _) => (v, join(&prefix, key)),
                (None, Some(v)) => (v, key.to_owned()),
                (None, None) => return None,
            };
            interpolate(value, &path, &lookup).map_err(|p| problems.push(p)).ok()
        };
        let url = one(&section.url, &self.top.url, "url");
        let shadow = one(&section.shadow, &self.top.shadow, "shadow");
        let from = one(&section.from, &self.top.from, "from");
        let to = one(&section.to, &self.top.to, "to");
        let schema = one(&section.schema, &self.top.schema, "schema");
        let format = one(&section.format, &self.top.format, "format");
        let migrations_dir = one(&section.migrations_dir, &self.top.migrations_dir, "migrations.dir");
        let migrations_revisions_schema =
            one(&section.migrations_revisions_schema, &self.top.migrations_revisions_schema, "migrations.revisions_schema");
        let mut many = |env: &Option<Vec<String>>, top: &Option<Vec<String>>, key: &str| -> Option<Vec<String>> {
            let (values, path) = match (env, top) {
                (Some(v), _) => (v, join(&prefix, key)),
                (None, Some(v)) => (v, key.to_owned()),
                (None, None) => return None,
            };
            let mut out = Vec::with_capacity(values.len());
            for (i, v) in values.iter().enumerate() {
                match interpolate(v, &format!("{path}[{i}]"), &lookup) {
                    Ok(v) => out.push(v),
                    Err(p) => problems.push(p),
                }
            }
            Some(out)
        };
        let include = many(&section.include, &self.top.include, "include");
        let exclude = many(&section.exclude, &self.top.exclude, "exclude");
        if !problems.is_empty() {
            return Err(ConfigError::Invalid { path: self.path.clone(), problems });
        }
        if format.as_deref().is_some_and(|f| f != "text" && f != "json") {
            let key = if section.format.is_some() { join(&prefix, "format") } else { "format".to_owned() };
            return Err(ConfigError::Invalid {
                path: self.path.clone(),
                problems: vec![Problem::key(&key, "expected \"text\" or \"json\"")],
            });
        }
        Ok(Settings { url, shadow, from, to, include, exclude, schema, format, migrations_dir, migrations_revisions_schema })
    }
}

/// What `config validate` checks: the file parses, has only known keys, its placeholders are
/// well formed, and the settings for `env` (the top level when `None`) resolve. Returns the
/// problems; an unreadable file is an error.
pub fn validate(path: &Path, env: Option<&str>, vars: &Vars) -> Result<Vec<Problem>, ConfigError> {
    match Config::load(path).and_then(|c| c.resolve(env, vars)) {
        Ok(_) => Ok(Vec::new()),
        Err(ConfigError::Invalid { problems, .. }) => Ok(problems),
        Err(e) => Err(e),
    }
}

/// A connection string fit to print: the password replaced by `***`. A URL
/// (`postgres://user:pw@host/db?password=pw`) keeps everything else as written; a key/value string
/// (`host=h password='p w'`) is re-rendered with `password=***`. A string that is neither is
/// returned as is, except a key/value string that does not parse, which is `***` whole.
pub fn redact(target: &str) -> String {
    if let Some(i) = target.find("://") {
        return redact_url(target, i + 3);
    }
    if !target.contains('=') {
        return target.to_owned();
    }
    redact_kv(target).unwrap_or_else(|| "***".to_owned())
}

fn redact_url(s: &str, start: usize) -> String {
    let (head, rest) = s.split_at(start);
    let (main, query) = match rest.split_once('?') {
        Some((m, q)) => (m, Some(q)),
        None => (rest, None),
    };
    let mut out = head.to_owned();
    // The last `@` before the query ends the user info: an unencoded `@` in the password stays hidden.
    match main.rfind('@') {
        Some(at) => {
            let userinfo = &main[..at];
            match userinfo.split_once(':') {
                Some((user, _)) => {
                    out.push_str(user);
                    out.push_str(":***");
                }
                None => out.push_str(userinfo),
            }
            out.push_str(&main[at..]);
        }
        None => out.push_str(main),
    }
    if let Some(query) = query {
        let params: Vec<String> = query
            .split('&')
            .map(|p| match p.split_once('=') {
                Some((k, _)) if k == "password" => format!("{k}=***"),
                _ => p.to_owned(),
            })
            .collect();
        out.push('?');
        out.push_str(&params.join("&"));
    }
    out
}

/// libpq's `key = value` syntax: values unquoted or in single quotes, `\` escaping.
fn redact_kv(s: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    let skip_ws = |chars: &mut std::iter::Peekable<std::str::Chars<'_>>| while chars.next_if(|c| c.is_whitespace()).is_some() {};
    loop {
        skip_ws(&mut chars);
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        skip_ws(&mut chars);
        if key.is_empty() || chars.next() != Some('=') {
            return None;
        }
        skip_ws(&mut chars);
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next()? {
                    '\\' => value.push(chars.next()?),
                    '\'' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                if c == '\\' { value.push(chars.next()?) } else { value.push(c) }
            }
        }
        match key.as_str() {
            "password" => out.push("password=***".to_owned()),
            // `dbname` may itself be a connection string.
            "dbname" => out.push(format!("dbname={}", quote_kv(&redact(&value)))),
            _ => out.push(format!("{key}={}", quote_kv(&value))),
        }
    }
    Some(out.join(" "))
}

fn quote_kv(v: &str) -> String {
    if !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || c == '\'' || c == '\\') {
        return v.to_owned();
    }
    format!("'{}'", v.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(text, "pgmig.toml").expect("parse")
    }

    fn problems(text: &str) -> Vec<String> {
        match Config::parse(text, "pgmig.toml") {
            Err(ConfigError::Invalid { problems, .. }) => problems.iter().map(ToString::to_string).collect(),
            other => panic!("expected problems, got {other:?}"),
        }
    }

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        Vars::only(pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect())
    }

    fn resolve_err(config: &Config, env: Option<&str>, vars: &Vars) -> Vec<String> {
        match config.resolve(env, vars) {
            Err(ConfigError::Invalid { problems, .. }) => problems.iter().map(ToString::to_string).collect(),
            other => panic!("expected problems, got {other:?}"),
        }
    }

    const FILE: &str = r#"
url = "host=top"
shadow = "pgmem"
to = "schema/"
include = ["app"]
format = "json"

[migrations]
dir = "migrations"

[env.local]
url = "host=local"
include = ["app", "audit"]

[env.ci.migrations]
dir = "ci/migrations"
revisions_schema = "ci_${CI_JOB}"
"#;

    #[test]
    fn parses_every_key() {
        let c = parse(
            r#"url = "u"
shadow = "s"
from = "f"
to = "t"
include = ["i"]
exclude = ["e"]
schema = "app"
format = "text"
migrations = { dir = "m", revisions_schema = "ops" }
"#,
        );
        let s = c.resolve(None, &Vars::only(BTreeMap::new())).expect("resolve");
        let want = Settings {
            url: Some("u".into()),
            shadow: Some("s".into()),
            from: Some("f".into()),
            to: Some("t".into()),
            include: Some(vec!["i".into()]),
            exclude: Some(vec!["e".into()]),
            schema: Some("app".into()),
            format: Some("text".into()),
            migrations_dir: Some("m".into()),
            migrations_revisions_schema: Some("ops".into()),
        };
        assert_eq!(s, want);
        assert!(c.envs.is_empty());
        assert_eq!(parse("").resolve(None, &vars(&[])).expect("empty"), Settings::default());
    }

    #[test]
    fn an_environment_overrides_the_top_level_key_by_key() {
        let c = parse(FILE);
        let top = c.resolve(None, &vars(&[])).expect("top");
        assert_eq!((top.url.as_deref(), top.include.as_deref()), (Some("host=top"), Some(&["app".to_owned()][..])));
        assert_eq!(top.migrations_dir.as_deref(), Some("migrations"));

        let local = c.resolve(Some("local"), &vars(&[])).expect("local");
        assert_eq!(local.url.as_deref(), Some("host=local"));
        // Arrays replace, they do not extend.
        assert_eq!(local.include, Some(vec!["app".to_owned(), "audit".to_owned()]));
        // Inherited from the top level.
        assert_eq!((local.shadow.as_deref(), local.to.as_deref(), local.format.as_deref()), (Some("pgmem"), Some("schema/"), Some("json")));
        assert_eq!(local.migrations_dir.as_deref(), Some("migrations"));

        assert_eq!((top.migrations_revisions_schema.as_deref(), local.migrations_revisions_schema.as_deref()), (None, None));
        let ci = c.resolve(Some("ci"), &vars(&[("CI_JOB", "7")])).expect("ci");
        assert_eq!((ci.url.as_deref(), ci.migrations_dir.as_deref()), (Some("host=top"), Some("ci/migrations")));
        assert_eq!(ci.migrations_revisions_schema.as_deref(), Some("ci_7"));
    }

    #[test]
    fn an_unknown_environment_is_an_error_listing_the_defined_ones() {
        let c = parse(FILE);
        assert_eq!(resolve_err(&c, Some("prod"), &vars(&[])), ["env.prod: no environment `prod` (--env); defined: ci, local"]);
        assert_eq!(resolve_err(&parse(""), Some("x"), &vars(&[])), ["env.x: no environment `x` (--env); the file defines none"]);
    }

    #[test]
    fn unknown_keys_are_errors_with_their_path() {
        let got = problems(
            r#"
urll = "x"
[migrations]
dri = "m"
[env.prod]
url = "u"
env = "nested"
[env."my env".diff]
skip = true
"#,
        );
        assert_eq!(got.len(), 4, "{got:?}");
        assert!(got[0].starts_with("env.\"my env\".diff: unknown key (expected one of `url`"), "{got:?}");
        assert!(got[1].starts_with("env.prod.env: unknown key"), "{got:?}");
        assert!(!got[1].contains("`env`)"), "env is a top-level key only: {got:?}");
        assert_eq!(got[2], "migrations.dri: unknown key (expected `dir` or `revisions_schema`)");
        assert!(got[3].starts_with("urll: unknown key") && got[3].contains("`env`"), "{got:?}");
    }

    #[test]
    fn wrong_types_name_the_key() {
        let got = problems("url = 1\ninclude = \"app\"\nexclude = [\"a\", 2]\nformat = \"yaml\"\nmigrations = \"m\"\nenv = []\n");
        assert_eq!(
            got,
            [
                "env: expected a table of environments ([env.<name>])",
                "exclude[1]: expected a string",
                "format: expected \"text\" or \"json\"",
                "include: expected an array of strings",
                "migrations: expected a table",
                "url: expected a string",
            ]
        );
        assert_eq!(problems("[env]\nlocal = 1\n"), ["env.local: expected a table ([env.<name>])"]);
        assert_eq!(
            problems("[migrations]\nrevisions_schema = 1\n[env.a.migrations]\nrevisions_schema = \"\"\n"),
            [
                "env.a.migrations.revisions_schema: expected a schema name, not an empty string",
                "migrations.revisions_schema: expected a string"
            ]
        );
    }

    #[test]
    fn syntax_errors_have_a_position_and_no_snippet() {
        let got = problems("shadow = \"pgmem\"\nurl = \"host=h password=hunter2\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("2:"), "{got:?}");
        assert!(!got[0].contains("hunter2"), "{got:?}");
        assert_eq!(position("ab\ncdé\nf", 7), (2, 4));
    }

    #[test]
    fn placeholders_come_from_vars_then_the_environment() {
        let c = parse("url = \"host=${HOST} user=${USER_NAME}\"\nto = \"${PATH}/schema\"\n");
        let v = vars(&[("HOST", "db"), ("USER_NAME", "app"), ("PATH", "p")]);
        assert_eq!(c.resolve(None, &v).expect("resolve").url.as_deref(), Some("host=db user=app"));

        // PATH comes from the environment; --var wins over it.
        let path = std::env::var("PATH").expect("PATH is set");
        let env = Vars::from_args(&["HOST=db", "USER_NAME=app"]).expect("vars");
        assert_eq!(c.resolve(None, &env).expect("resolve").to, Some(format!("{path}/schema")));
        let over = Vars::from_args(&["HOST=db", "USER_NAME=app", "PATH=mine"]).expect("vars");
        assert_eq!(c.resolve(None, &over).expect("resolve").to.as_deref(), Some("mine/schema"));
        // A value may contain `=`.
        assert_eq!(Vars::from_args(&["A=b=c"]).expect("vars").get("A").as_deref(), Some("b=c"));
    }

    #[test]
    fn interpolation_escapes_and_literals() {
        let lookup = |n: &str| (n == "X").then(|| "1".to_owned());
        let i = |s: &str| interpolate(s, "k", &lookup);
        assert_eq!(i("a${X}b${X}").expect("ok"), "a1b1");
        assert_eq!(i("$${X}").expect("ok"), "${X}");
        // Left to right: a `$`, then the escape.
        assert_eq!(i("$$${X}").expect("ok"), "$${X}");
        assert_eq!(i("cost $5 $").expect("ok"), "cost $5 $");
        assert_eq!(i("{X}").expect("ok"), "{X}");
        assert_eq!(i("${X").expect_err("open").to_string(), "k: unterminated `${` (write `$${` for a literal `${`)");
        assert!(i("${1X}").expect_err("bad name").message.starts_with("invalid variable name"));
        assert!(i("${}").is_err());
        assert_eq!(
            i("${Y}").expect_err("undefined").to_string(),
            "k: undefined variable `Y` (set it in the environment or pass --var Y=...)"
        );
    }

    #[test]
    fn undefined_variables_name_the_key_and_never_the_value() {
        let c = parse("url = \"password=${PW}\"\n[env.prod]\nurl = \"password=${PROD_PW}\"\nexclude = [\"a\", \"${PAT}\"]\n");
        assert_eq!(resolve_err(&c, None, &vars(&[])), ["url: undefined variable `PW` (set it in the environment or pass --var PW=...)"]);
        let got = resolve_err(&c, Some("prod"), &vars(&[("PW", "secret")]));
        assert_eq!(got.len(), 2);
        assert!(got[0].starts_with("env.prod.url: undefined variable `PROD_PW`"), "{got:?}");
        assert!(got[1].starts_with("env.prod.exclude[1]: undefined variable `PAT`"), "{got:?}");
        assert!(got.iter().all(|m| !m.contains("secret")));
        // The overridden top-level url is not interpolated, so PW need not be set for prod.
        let ok = c.resolve(Some("prod"), &vars(&[("PROD_PW", "p"), ("PAT", "x")])).expect("prod");
        assert_eq!((ok.url.as_deref(), ok.exclude), (Some("password=p"), Some(vec!["a".to_owned(), "x".to_owned()])));
        // Unused sections are checked for syntax only.
        assert!(problems("[env.other]\nurl = \"${\"\n")[0].starts_with("env.other.url: unterminated"));
    }

    #[test]
    fn interpolated_format_is_checked() {
        let c = parse("[env.a]\nformat = \"${F}\"\n");
        assert_eq!(c.resolve(Some("a"), &vars(&[("F", "json")])).expect("ok").format.as_deref(), Some("json"));
        assert_eq!(resolve_err(&c, Some("a"), &vars(&[("F", "yaml")])), ["env.a.format: expected \"text\" or \"json\""]);
    }

    #[test]
    fn bad_var_arguments_hide_their_value() {
        let e = Vars::from_args(&["NOEQUALS_secret"]).expect_err("no =");
        assert_eq!(e.to_string(), "--var expects NAME=VALUE");
        let e = Vars::from_args(&["1X=secret"]).expect_err("bad name");
        assert!(e.to_string().starts_with("--var \"1X\":") && !e.to_string().contains("secret"));
    }

    #[test]
    fn redact_hides_passwords() {
        // URLs: with and without a password, percent-encoded, an `@` in the password, in the query.
        assert_eq!(redact("postgres://app:hunter2@db.prod:5432/app"), "postgres://app:***@db.prod:5432/app");
        assert_eq!(redact("postgresql://app@db/app?sslmode=require"), "postgresql://app@db/app?sslmode=require");
        assert_eq!(redact("postgres://db/app"), "postgres://db/app");
        assert_eq!(redact("postgres://a%40b:p%3Ass%40w@db/app"), "postgres://a%40b:***@db/app");
        assert_eq!(redact("postgres://app:p@ss@db/app"), "postgres://app:***@db/app");
        assert_eq!(
            redact("postgres://db/app?user=a&password=hunter2&sslmode=require"),
            "postgres://db/app?user=a&password=***&sslmode=require"
        );
        // Key/value: plain, quoted with escapes and spaces, around `=`, none, a URL as dbname.
        assert_eq!(redact("host=db password=hunter2 user=app"), "host=db password=*** user=app");
        assert_eq!(redact("host=db password = 'hun ter\\'2' user='a b'"), "host=db password=*** user='a b'");
        assert_eq!(redact("host=127.0.0.1 port=5432 user=postgres"), "host=127.0.0.1 port=5432 user=postgres");
        assert_eq!(redact("dbname=postgres://u:hunter2@h/d"), "dbname=postgres://u:***@h/d");
        // Not a connection string, and one that does not parse.
        assert_eq!(redact("pgmem"), "pgmem");
        assert_eq!(redact("host=db password='hunter2"), "***");
        let s =
            Settings { url: Some("host=h password=hunter2".into()), to: Some("postgres://u:hunter2@h/d".into()), ..Settings::default() };
        assert!(!format!("{s:?} {:?}", Vars::only([("PW".to_owned(), "hunter2".to_owned())].into())).contains("hunter2"));
    }

    #[test]
    fn url_key_names_where_url_comes_from() {
        let c = parse(FILE);
        assert_eq!(c.url_key(None).as_deref(), Some("url"));
        assert_eq!(c.url_key(Some("local")).as_deref(), Some("env.local.url"));
        assert_eq!(c.url_key(Some("ci")).as_deref(), Some("url"));
        assert_eq!(parse("").url_key(None), None);
    }

    #[test]
    fn validate_and_discover_read_files() {
        let dir = std::env::temp_dir().join(format!("pgmig_config_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert!(Config::discover(None, &dir).expect("no file").is_none());
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, "url = \"${PGMIG_CONFIG_TEST_UNSET}\"\nshadow = \"pgmem\"\n").expect("write");
        let found = Config::discover(None, &dir).expect("found").expect("some");
        assert_eq!(found.top.shadow.as_deref(), Some("pgmem"));
        let v = vars(&[]);
        assert_eq!(validate(&path, None, &v).expect("validate").len(), 1);
        assert!(validate(&path, None, &vars(&[("PGMIG_CONFIG_TEST_UNSET", "x")])).expect("validate").is_empty());
        assert!(matches!(validate(&dir.join("missing.toml"), None, &v), Err(ConfigError::Io { .. })));
        assert!(matches!(Config::discover(Some(&dir.join("missing.toml")), &dir), Err(ConfigError::Io { .. })));
        std::fs::remove_file(&path).expect("rm");
        std::fs::remove_dir(&dir).expect("rmdir");
    }
}
