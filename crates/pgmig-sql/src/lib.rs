//! SQL text handling: split a file into statements, keeping where each one starts, and collect
//! `-- pgmig:<name> args` directives.
//!
//! The splitter only finds statement boundaries; it never interprets SQL. It understands what can
//! hide a `;`: single-quoted strings (`''` escapes), `E''` strings (backslash escapes), quoted
//! identifiers, dollar quoting (`$$`, `$tag$`), line comments, nested block comments, and the
//! `BEGIN ATOMIC ... END` bodies of SQL-standard functions and procedures.
//!
//! Directives directly above a statement (no blank line between) belong to that statement, as do
//! directives inside it. Any other directive belongs to the file.

use std::fmt;

/// A 1-based line and column; columns count characters, not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// A `-- pgmig:<name> args` comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub name: String,
    /// The rest of the line, trimmed.
    pub args: String,
    pub pos: Pos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// The statement text as written, without the terminating `;` and surrounding whitespace or
    /// comments.
    pub sql: String,
    /// Where `sql` starts in the file.
    pub pos: Pos,
    pub directives: Vec<Directive>,
}

impl Statement {
    /// The file position of the `n`th character (1-based) of `sql`, the form Postgres reports
    /// error positions in. Out-of-range values clamp to the statement's start or end.
    pub fn locate(&self, n: u32) -> Pos {
        let mut pos = self.pos;
        for c in self.sql.chars().take(n.saturating_sub(1) as usize) {
            if c == '\n' {
                pos = Pos { line: pos.line + 1, col: 1 };
            } else {
                pos.col += 1;
            }
        }
        pos
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct File {
    pub directives: Vec<Directive>,
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{pos}: {kind}")]
pub struct Error {
    /// Where the offending token starts.
    pub pos: Pos,
    pub kind: ErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
    #[error("unterminated quoted string")]
    UnterminatedString,
    #[error("unterminated quoted identifier")]
    UnterminatedIdentifier,
    #[error("unterminated dollar-quoted string {0}")]
    UnterminatedDollarQuote(String),
    #[error("unterminated /* comment")]
    UnterminatedComment,
    #[error("directive without a name; expected `-- pgmig:<name>`")]
    DirectiveWithoutName,
}

/// Split SQL text into statements and directives.
pub fn split(text: &str) -> Result<File, Error> {
    Splitter::new(text).run()
}

struct Splitter<'a> {
    text: &'a str,
    b: &'a [u8],
    i: usize,
    line_starts: Vec<usize>,
    file: File,
    /// Directives seen since the last statement, not yet known to belong to the next one.
    pending: Vec<Directive>,
    /// Newlines since the last comment, outside a statement: two mean a blank line.
    newlines: u32,
    stmt: Option<Stmt>,
}

struct Stmt {
    start: usize,
    end: usize,
    directives: Vec<Directive>,
    /// The first four words, uppercased, to recognize `CREATE [OR REPLACE] FUNCTION|PROCEDURE`.
    words: Vec<String>,
    begin_depth: u32,
}

impl<'a> Splitter<'a> {
    fn new(text: &'a str) -> Splitter<'a> {
        let line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        Splitter { text, b: text.as_bytes(), i: 0, line_starts, file: File::default(), pending: Vec::new(), newlines: 0, stmt: None }
    }

    fn pos(&self, at: usize) -> Pos {
        let line = self.line_starts.partition_point(|&s| s <= at) - 1;
        let col = self.text[self.line_starts[line]..at].chars().count() + 1;
        Pos { line: line as u32 + 1, col: col as u32 }
    }

    fn err(&self, at: usize, kind: ErrorKind) -> Error {
        Error { pos: self.pos(at), kind }
    }

    fn peek(&self, k: usize) -> Option<u8> {
        self.b.get(self.i + k).copied()
    }

    fn run(mut self) -> Result<File, Error> {
        while let Some(c) = self.peek(0) {
            let start = self.i;
            match c {
                b'\n' => {
                    self.i += 1;
                    if self.stmt.is_none() {
                        self.newlines += 1;
                        if self.newlines >= 2 {
                            self.file.directives.append(&mut self.pending);
                        }
                    }
                }
                c if c.is_ascii_whitespace() => self.i += 1,
                b'-' if self.peek(1) == Some(b'-') => {
                    let end = self.text[start..].find('\n').map_or(self.b.len(), |n| start + n);
                    self.i = end;
                    self.comment(start, end, true)?;
                }
                b'/' if self.peek(1) == Some(b'*') => {
                    self.block_comment()?;
                    self.comment(start, self.i, false)?;
                }
                b';' => {
                    self.i += 1;
                    match &self.stmt {
                        Some(s) if s.begin_depth > 0 => self.token(start),
                        Some(_) => self.finish(),
                        None => {}
                    }
                }
                b'\'' => {
                    self.quoted(b'\'', false, ErrorKind::UnterminatedString)?;
                    self.token(start);
                }
                b'"' => {
                    self.quoted(b'"', false, ErrorKind::UnterminatedIdentifier)?;
                    self.token(start);
                }
                b'$' => {
                    self.dollar()?;
                    self.token(start);
                }
                c if is_ident_start(c) => {
                    while self.peek(0).is_some_and(is_ident_char) {
                        self.i += 1;
                    }
                    let word = &self.text[start..self.i];
                    if word.eq_ignore_ascii_case("e") && self.peek(0) == Some(b'\'') {
                        self.quoted(b'\'', true, ErrorKind::UnterminatedString)?;
                        self.token(start);
                    } else {
                        self.token(start);
                        self.word(word);
                    }
                }
                _ => {
                    // Multi-byte characters only appear inside identifiers, handled above.
                    self.i += 1;
                    self.token(start);
                }
            }
        }
        self.finish();
        self.file.directives.append(&mut self.pending);
        Ok(self.file)
    }

    /// Statement text from `start` to the current position.
    fn token(&mut self, start: usize) {
        let pending = &mut self.pending;
        let s = self.stmt.get_or_insert_with(|| Stmt {
            start,
            end: start,
            directives: std::mem::take(pending),
            words: Vec::new(),
            begin_depth: 0,
        });
        s.end = self.i;
    }

    fn word(&mut self, word: &str) {
        let Some(s) = &mut self.stmt else { return };
        if s.words.len() < 4 {
            s.words.push(word.to_ascii_uppercase());
        }
        let routine = s.words.first().is_some_and(|w| w == "CREATE")
            && (matches!(s.words.get(1).map(String::as_str), Some("FUNCTION" | "PROCEDURE"))
                || (s.words.get(1).is_some_and(|w| w == "OR")
                    && matches!(s.words.get(3).map(String::as_str), Some("FUNCTION" | "PROCEDURE"))));
        let opens = (word.eq_ignore_ascii_case("begin") && routine) || (word.eq_ignore_ascii_case("case") && s.begin_depth > 0);
        if opens {
            s.begin_depth += 1;
        } else if word.eq_ignore_ascii_case("end") && s.begin_depth > 0 {
            s.begin_depth -= 1;
        }
    }

    fn finish(&mut self) {
        if let Some(s) = self.stmt.take() {
            self.file.statements.push(Statement {
                sql: self.text[s.start..s.end].to_owned(),
                pos: self.pos(s.start),
                directives: s.directives,
            });
        }
        self.newlines = 0;
    }

    fn comment(&mut self, start: usize, end: usize, line: bool) -> Result<(), Error> {
        let directive = if line { self.directive(start, end)? } else { None };
        match &mut self.stmt {
            Some(s) => {
                s.end = end;
                s.directives.extend(directive);
            }
            None => {
                self.newlines = 0;
                self.pending.extend(directive);
            }
        }
        Ok(())
    }

    /// Parse the line comment `text[start..end]` as a directive, if it is one.
    fn directive(&self, start: usize, end: usize) -> Result<Option<Directive>, Error> {
        let Some(rest) = self.text[start + 2..end].trim_start().strip_prefix("pgmig:") else {
            return Ok(None);
        };
        let rest = rest.trim_end();
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if name.is_empty() {
            return Err(self.err(start, ErrorKind::DirectiveWithoutName));
        }
        Ok(Some(Directive { name: name.to_owned(), args: args.trim().to_owned(), pos: self.pos(start) }))
    }

    fn block_comment(&mut self) -> Result<(), Error> {
        let start = self.i;
        self.i += 2;
        let mut depth = 1;
        while depth > 0 {
            match (self.peek(0), self.peek(1)) {
                (None, _) => return Err(self.err(start, ErrorKind::UnterminatedComment)),
                (Some(b'/'), Some(b'*')) => {
                    depth += 1;
                    self.i += 2;
                }
                (Some(b'*'), Some(b'/')) => {
                    depth -= 1;
                    self.i += 2;
                }
                _ => self.i += 1,
            }
        }
        Ok(())
    }

    /// A quoted string or identifier starting at the current position; a doubled quote is an
    /// escaped one, and with `backslash` so is `\'`.
    fn quoted(&mut self, quote: u8, backslash: bool, kind: ErrorKind) -> Result<(), Error> {
        let start = self.i;
        self.i += 1;
        loop {
            match self.peek(0) {
                None => return Err(self.err(start, kind)),
                Some(b'\\') if backslash => self.i += 2,
                Some(c) if c == quote => {
                    self.i += 1;
                    if self.peek(0) != Some(quote) {
                        return Ok(());
                    }
                    self.i += 1;
                }
                Some(_) => self.i += 1,
            }
        }
    }

    /// `$tag$...$tag$`, a parameter like `$1`, or a lone `$`.
    fn dollar(&mut self) -> Result<(), Error> {
        let start = self.i;
        let mut j = start + 1;
        if self.b.get(j).is_some_and(|&c| is_ident_start(c)) {
            while self.b.get(j).is_some_and(|&c| is_ident_char(c) && c != b'$') {
                j += 1;
            }
        }
        if self.b.get(j) != Some(&b'$') {
            // A parameter (`$1`) or a lone `$`: consume the `$` and any digits.
            self.i += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                self.i += 1;
            }
            return Ok(());
        }
        let tag = &self.text[start..=j];
        match self.text[j + 1..].find(tag) {
            Some(n) => {
                self.i = j + 1 + n + tag.len();
                Ok(())
            }
            None => Err(self.err(start, ErrorKind::UnterminatedDollarQuote(tag.to_owned()))),
        }
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqls(text: &str) -> Vec<String> {
        split(text).expect("split").statements.into_iter().map(|s| s.sql).collect()
    }

    fn err(text: &str) -> Error {
        split(text).expect_err("split should fail")
    }

    fn pos(line: u32, col: u32) -> Pos {
        Pos { line, col }
    }

    #[test]
    fn statements_and_positions() {
        let f = split("CREATE TABLE a (id int);\n\n  CREATE TABLE b (id int);CREATE TABLE c ()").expect("split");
        let got: Vec<_> = f.statements.iter().map(|s| (s.sql.as_str(), s.pos)).collect();
        assert_eq!(
            got,
            [("CREATE TABLE a (id int)", pos(1, 1)), ("CREATE TABLE b (id int)", pos(3, 3)), ("CREATE TABLE c ()", pos(3, 27)),]
        );
    }

    #[test]
    fn empty_statements_and_comments_only() {
        assert!(sqls("").is_empty());
        assert!(sqls(";;\n  ;").is_empty());
        assert!(sqls("-- nothing\n/* here */\n").is_empty());
    }

    #[test]
    fn leading_comments_are_not_part_of_the_statement() {
        let f = split("-- a table\n/* b */ SELECT 1 /* in */ + 1;").expect("split");
        assert_eq!(f.statements[0].sql, "SELECT 1 /* in */ + 1");
        assert_eq!(f.statements[0].pos, pos(2, 9));
    }

    #[test]
    fn single_quotes() {
        assert_eq!(sqls("SELECT 'a;b', 'it''s;';SELECT 2"), ["SELECT 'a;b', 'it''s;'", "SELECT 2"]);
        // With standard_conforming_strings, a backslash is an ordinary character.
        assert_eq!(sqls(r"SELECT 'a\';SELECT 2"), [r"SELECT 'a\'", "SELECT 2"]);
        assert_eq!(sqls("SELECT U&'d\\0061t;a', B'01', X'1f';SELECT 2"), ["SELECT U&'d\\0061t;a', B'01', X'1f'", "SELECT 2"]);
    }

    #[test]
    fn escape_strings() {
        assert_eq!(sqls(r"SELECT E'a\';b', e'\\';SELECT 2"), [r"SELECT E'a\';b', e'\\'", "SELECT 2"]);
        assert_eq!(sqls(r"SELECT E'it''s;';SELECT 2"), [r"SELECT E'it''s;'", "SELECT 2"]);
        // An identifier ending in e is not an escape-string prefix.
        assert_eq!(sqls(r"SELECT 'x' AS some'\';SELECT 2"), [r"SELECT 'x' AS some'\'", "SELECT 2"]);
    }

    #[test]
    fn quoted_identifiers() {
        assert_eq!(sqls(r#"CREATE TABLE "a;""b" ();SELECT 2"#), [r#"CREATE TABLE "a;""b" ()"#, "SELECT 2"]);
    }

    #[test]
    fn dollar_quotes() {
        let f = "CREATE FUNCTION f() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$;\
                 CREATE FUNCTION g() RETURNS text LANGUAGE sql AS $body$ SELECT '$$;' $body$;SELECT 3";
        let got = sqls(f);
        assert_eq!(got.len(), 3);
        assert!(got[0].ends_with("END $$"));
        assert!(got[1].ends_with("$body$"));
        // Tags are case-sensitive; `$a$` does not close `$A$`.
        assert_eq!(sqls("SELECT $A$ $a$; $A$;SELECT 2"), ["SELECT $A$ $a$; $A$", "SELECT 2"]);
    }

    #[test]
    fn dollar_signs_that_are_not_quotes() {
        assert_eq!(sqls("PREPARE p AS SELECT $1;SELECT 2"), ["PREPARE p AS SELECT $1", "SELECT 2"]);
        assert_eq!(sqls("SELECT 1 AS a$b;SELECT 2"), ["SELECT 1 AS a$b", "SELECT 2"]);
        assert_eq!(sqls("SELECT 1 AS x$$;SELECT 2"), ["SELECT 1 AS x$$", "SELECT 2"]);
    }

    #[test]
    fn line_comments() {
        assert_eq!(sqls("SELECT 1 -- ; not here\n+ 1;SELECT 2"), ["SELECT 1 -- ; not here\n+ 1", "SELECT 2"]);
        assert_eq!(sqls("SELECT 1 -- trailing"), ["SELECT 1 -- trailing"]);
        // `-` alone is an operator.
        assert_eq!(sqls("SELECT 3 - 1;"), ["SELECT 3 - 1"]);
    }

    #[test]
    fn nested_block_comments() {
        assert_eq!(sqls("SELECT /* a /* b; */ c; */ 1;SELECT 2"), ["SELECT /* a /* b; */ c; */ 1", "SELECT 2"]);
    }

    #[test]
    fn begin_atomic_bodies() {
        let f = "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT 1; SELECT CASE WHEN true THEN 2 END; END;\
                 CREATE OR REPLACE PROCEDURE p() BEGIN ATOMIC INSERT INTO t VALUES (1); END;\
                 BEGIN;CREATE TABLE t (begin int, \"end\" int);COMMIT";
        let got = sqls(f);
        assert_eq!(got.len(), 5, "{got:#?}");
        assert!(got[0].ends_with("END; END"));
        assert!(got[1].ends_with("(1); END"));
        assert_eq!(&got[2..], ["BEGIN", "CREATE TABLE t (begin int, \"end\" int)", "COMMIT"]);
    }

    #[test]
    fn multibyte_columns() {
        let f = split("SELECT 'é';  SELECT 2").expect("split");
        assert_eq!(f.statements[1].pos, pos(1, 14));
        assert_eq!(split("CREATE TABLE café ();").expect("split").statements[0].sql, "CREATE TABLE café ()");
    }

    #[test]
    fn crlf_line_endings() {
        let f = split("SELECT 1;\r\n\r\nSELECT 2;\r\n").expect("split");
        assert_eq!(f.statements[1].sql, "SELECT 2");
        assert_eq!(f.statements[1].pos, pos(3, 1));
    }

    #[test]
    fn locate_error_positions() {
        let f = split("\n  CREATE TABLE t (\n    id nosuchtype\n  );").expect("split");
        let s = &f.statements[0];
        assert_eq!(s.locate(1), pos(2, 3));
        // Postgres positions count characters from 1.
        let n = s.sql.find("nosuchtype").expect("present") as u32 + 1;
        assert_eq!(s.locate(n), pos(3, 8));
        assert_eq!(s.locate(0), pos(2, 3));
    }

    #[test]
    fn directives() {
        let f = split(
            "-- pgmig:import types.sql\n\
             -- pgmig:import  other.sql  \n\
             \n\
             -- an ordinary comment\n\
             --pgmig:sensitive\n\
             CREATE ROLE r PASSWORD 'x';\n\
             SELECT 1 -- pgmig:inside the statement\n\
             ;\n\
             -- pgmig:dangling\n",
        )
        .expect("split");
        let d = |name: &str, args: &str, line| Directive { name: name.into(), args: args.into(), pos: pos(line, 1) };
        assert_eq!(f.directives, [d("import", "types.sql", 1), d("import", "other.sql", 2), d("dangling", "", 9)]);
        assert_eq!(f.statements[0].directives, [d("sensitive", "", 5)]);
        assert_eq!(f.statements[1].directives, [Directive { name: "inside".into(), args: "the statement".into(), pos: pos(7, 10) }]);
    }

    #[test]
    fn a_blank_line_separates_directives_from_the_statement() {
        let f = split("SELECT 1;\n-- pgmig:x\n\nSELECT 2;").expect("split");
        assert_eq!(f.directives.len(), 1);
        assert!(f.statements[1].directives.is_empty());
    }

    #[test]
    fn directive_without_name() {
        assert_eq!(err("SELECT 1;\n-- pgmig:\n"), Error { pos: pos(2, 1), kind: ErrorKind::DirectiveWithoutName });
        assert!(split("-- pgmig is great\nSELECT 1").expect("split").directives.is_empty());
    }

    #[test]
    fn unterminated() {
        assert_eq!(err("SELECT 1;\nSELECT 'abc").to_string(), "2:8: unterminated quoted string");
        assert_eq!(err(r"SELECT E'\'").kind, ErrorKind::UnterminatedString);
        assert_eq!(err("SELECT \"abc").kind, ErrorKind::UnterminatedIdentifier);
        assert_eq!(err("SELECT $x$ abc $y$").kind, ErrorKind::UnterminatedDollarQuote("$x$".into()));
        assert_eq!(err("SELECT /* /* */").kind, ErrorKind::UnterminatedComment);
    }
}
