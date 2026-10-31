//! A small lexer that splits a shell script into the simple commands it runs.
//!
//! It understands what an agent's one-line Bash commands use: quotes, escapes, `;`, `&&`, `||`,
//! pipes, subshells, command substitution in `$(...)` and backticks, comments and here-documents.
//! Each simple command comes out as its words with quotes removed. Commands inside a substitution
//! come out as separate commands. Expansions such as `$VAR` stay literal because the guard cannot
//! know their values.

use std::iter::Peekable;
use std::str::Chars;

/// Splits `script` into the words of every simple command it contains, including commands nested
/// in command substitutions.
pub(crate) fn simple_commands(script: &str) -> Vec<Vec<String>> {
    let mut lexer = Lexer {
        chars: script.chars().peekable(),
        commands: Vec::new(),
        heredocs: Vec::new(),
    };
    lexer.commands_until(End::Input);
    lexer.commands
}

struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    commands: Vec<Vec<String>>,
    /// Here-documents whose bodies start after the next newline.
    heredocs: Vec<Heredoc>,
}

struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

/// What ends the commands being lexed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Input,
    /// The `)` closing a `$(` substitution.
    Paren,
    /// The backtick closing a backtick substitution.
    Backtick,
}

/// The words of the simple command being lexed.
#[derive(Default)]
struct Command {
    words: Vec<String>,
    word: Option<String>,
}

impl Command {
    fn word(&mut self) -> &mut String {
        self.word.get_or_insert_with(String::new)
    }

    fn finish_word(&mut self) {
        if let Some(word) = self.word.take() {
            self.words.push(word);
        }
    }
}

impl Lexer<'_> {
    /// Lexes commands until `end` or the end of input, consuming the terminator.
    fn commands_until(&mut self, end: End) {
        let mut command = Command::default();
        let mut subshells: u32 = 0;
        while let Some(c) = self.chars.next() {
            match c {
                ' ' | '\t' => command.finish_word(),
                '\n' => {
                    self.finish_command(&mut command);
                    self.skip_heredoc_bodies();
                }
                ';' | '&' | '|' => self.finish_command(&mut command),
                '(' => {
                    self.finish_command(&mut command);
                    subshells = subshells.saturating_add(1);
                }
                ')' => {
                    self.finish_command(&mut command);
                    if subshells == 0 && end == End::Paren {
                        return;
                    }
                    subshells = subshells.saturating_sub(1);
                }
                '`' if end == End::Backtick => {
                    self.finish_command(&mut command);
                    return;
                }
                '`' => {
                    command.word();
                    self.commands_until(End::Backtick);
                }
                '<' | '>' => {
                    command.finish_word();
                    if c == '<' && self.chars.next_if_eq(&'<').is_some() {
                        self.redirect_from_text();
                    }
                }
                '#' if command.word.is_none() => self.skip_comment(),
                '\\' if self.chars.next_if_eq(&'\n').is_some() => {}
                other => self.word_char(other, command.word()),
            }
        }
        self.finish_command(&mut command);
    }

    /// Lexes one character that belongs to a word, with any quoted text or expansion it starts.
    fn word_char(&mut self, c: char, word: &mut String) {
        match c {
            '\\' => match self.chars.next() {
                Some('\n') | None => {}
                Some(escaped) => word.push(escaped),
            },
            '\'' => self.single_quoted(word),
            '"' => self.double_quoted(word),
            '$' => self.dollar(word),
            other => word.push(other),
        }
    }

    fn finish_command(&mut self, command: &mut Command) {
        command.finish_word();
        if !command.words.is_empty() {
            self.commands.push(std::mem::take(&mut command.words));
        }
    }

    fn single_quoted(&mut self, word: &mut String) {
        for c in self.chars.by_ref() {
            if c == '\'' {
                return;
            }
            word.push(c);
        }
    }

    fn double_quoted(&mut self, word: &mut String) {
        while let Some(c) = self.chars.next() {
            match c {
                '"' => return,
                '\\' => match self.chars.next() {
                    Some('\n') | None => {}
                    Some(escaped @ ('"' | '\\' | '$' | '`')) => word.push(escaped),
                    Some(other) => {
                        word.push('\\');
                        word.push(other);
                    }
                },
                '$' if self.chars.next_if_eq(&'(').is_some() => self.commands_until(End::Paren),
                '`' => self.commands_until(End::Backtick),
                other => word.push(other),
            }
        }
    }

    /// Lexes what follows a `$` outside quotes.
    fn dollar(&mut self, word: &mut String) {
        if self.chars.next_if_eq(&'(').is_some() {
            self.commands_until(End::Paren);
        } else if self.chars.next_if_eq(&'\'').is_some() {
            self.ansi_c_quoted(word);
        } else if self.chars.next_if_eq(&'"').is_some() {
            self.double_quoted(word);
        } else {
            word.push('$');
        }
    }

    /// Lexes `$'...'`, where backslash escapes are decoded.
    fn ansi_c_quoted(&mut self, word: &mut String) {
        while let Some(c) = self.chars.next() {
            match c {
                '\'' => return,
                '\\' => match self.chars.next() {
                    Some('n') => word.push('\n'),
                    Some('t') => word.push('\t'),
                    Some(other) => word.push(other),
                    None => {}
                },
                other => word.push(other),
            }
        }
    }

    fn skip_comment(&mut self) {
        while self.chars.next_if(|&c| c != '\n').is_some() {}
    }

    /// Handles what follows `<<`: a here-string (`<<<`) or a here-document.
    fn redirect_from_text(&mut self) {
        if self.chars.next_if_eq(&'<').is_some() {
            return;
        }
        let strip_tabs = self.chars.next_if_eq(&'-').is_some();
        while self.chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        let mut delimiter = String::new();
        while let Some(c) = self
            .chars
            .next_if(|&c| !c.is_whitespace() && !";&|()<>".contains(c))
        {
            match c {
                '\'' => self.single_quoted(&mut delimiter),
                '"' => self.double_quoted(&mut delimiter),
                '\\' => delimiter.extend(self.chars.next()),
                other => delimiter.push(other),
            }
        }
        if !delimiter.is_empty() {
            self.heredocs.push(Heredoc {
                delimiter,
                strip_tabs,
            });
        }
    }

    /// Skips the bodies of pending here-documents; they are data, not commands.
    fn skip_heredoc_bodies(&mut self) {
        for heredoc in std::mem::take(&mut self.heredocs) {
            while let Some(line) = self.line() {
                let line = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == heredoc.delimiter {
                    break;
                }
            }
        }
    }

    /// Reads the rest of the current line, without its newline. `None` at the end of input.
    fn line(&mut self) -> Option<String> {
        self.chars.peek()?;
        let mut line = String::new();
        for c in self.chars.by_ref() {
            if c == '\n' {
                break;
            }
            line.push(c);
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::simple_commands;

    fn lex(script: &str) -> Vec<Vec<String>> {
        simple_commands(script)
    }

    #[test]
    fn splits_words_on_blanks() {
        assert_eq!(
            lex("git  push\torigin main"),
            [["git", "push", "origin", "main"]],
            "blanks separate words"
        );
    }

    #[test]
    fn splits_commands_on_operators_and_newlines() {
        assert_eq!(
            lex("a 1; b 2 && c 3 || d 4 | e 5 & f 6\ng 7"),
            [
                ["a", "1"],
                ["b", "2"],
                ["c", "3"],
                ["d", "4"],
                ["e", "5"],
                ["f", "6"],
                ["g", "7"]
            ],
            "every operator ends a command"
        );
    }

    #[test]
    fn removes_quotes_and_keeps_quoted_text_together() {
        assert_eq!(
            lex(r#"git commit -m "fix --no-verify; really" -m 'it''s' a\ b"#),
            [vec![
                "git",
                "commit",
                "-m",
                "fix --no-verify; really",
                "-m",
                "its",
                "a b"
            ]],
            "quoted operators and blanks stay inside the word"
        );
    }

    #[test]
    fn decodes_escapes_inside_double_quotes() {
        assert_eq!(
            lex(r#"echo "a \"b\" \$c \x""#),
            [["echo", r#"a "b" $c \x"#]],
            "only \\\", \\\\, \\$ and \\` are escapes in double quotes"
        );
    }

    #[test]
    fn decodes_ansi_c_quotes() {
        assert_eq!(
            lex(r"git push $'--force' $'a\tb'"),
            [["git", "push", "--force", "a\tb"]],
            "$'...' is a quoted word"
        );
    }

    #[test]
    fn lexes_command_substitutions_as_commands() {
        assert_eq!(
            lex(r#"echo $(git push -f) "$(git status)" `git log`"#),
            [
                vec!["git", "push", "-f"],
                vec!["git", "status"],
                vec!["git", "log"],
                vec!["echo", "", "", ""],
            ],
            "substituted commands are commands of their own"
        );
    }

    #[test]
    fn lexes_nested_parentheses_inside_substitutions() {
        assert_eq!(
            lex("echo $( (cd x; git push -f) ) done"),
            [
                vec!["cd", "x"],
                vec!["git", "push", "-f"],
                vec!["echo", "", "done"]
            ],
            "a subshell inside a substitution does not end it"
        );
    }

    #[test]
    fn lexes_subshells() {
        assert_eq!(
            lex("(cd repo && git push -f)"),
            [vec!["cd", "repo"], vec!["git", "push", "-f"]],
            "parentheses separate commands"
        );
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            lex("git status # git push -f\nls"),
            [vec!["git", "status"], vec!["ls"]],
            "a comment runs to the end of the line"
        );
        assert_eq!(
            lex("echo a#b"),
            [["echo", "a#b"]],
            "# inside a word is not a comment"
        );
    }

    #[test]
    fn skips_heredoc_bodies() {
        let script = "git commit -m \"$(cat <<'EOF'\nfeat: block git push --force\n\nIt's done.\nEOF\n)\"\ngit status";
        assert_eq!(
            lex(script),
            [
                vec!["cat"],
                vec!["git", "commit", "-m", ""],
                vec!["git", "status"]
            ],
            "a heredoc body with quotes and commands is data"
        );
    }

    #[test]
    fn skips_tab_stripped_heredoc_bodies() {
        assert_eq!(
            lex("cat <<-END\n\tgit push -f\n\tEND\nls"),
            [vec!["cat"], vec!["ls"]],
            "<<- strips leading tabs before matching the delimiter"
        );
    }

    #[test]
    fn here_strings_are_words() {
        assert_eq!(
            lex("grep x <<< 'git push -f'"),
            [["grep", "x", "git push -f"]],
            "<<< is followed by an ordinary word"
        );
    }

    #[test]
    fn redirections_end_words() {
        assert_eq!(
            lex("git push -f>out 2>&1"),
            [vec!["git", "push", "-f", "out", "2"], vec!["1"]],
            "redirection operators separate words"
        );
    }

    #[test]
    fn line_continuations_join_lines() {
        assert_eq!(
            lex("git push \\\n  --force"),
            [["git", "push", "--force"]],
            "backslash-newline continues the command"
        );
    }

    #[test]
    fn unterminated_quotes_end_at_the_end_of_input() {
        assert_eq!(
            lex("echo 'unterminated"),
            [["echo", "unterminated"]],
            "an unterminated quote runs to the end"
        );
    }
}
