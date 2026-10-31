//! A small lexer that splits a shell script into the simple commands it runs.
//!
//! It understands what an agent's Bash commands use: quotes, escapes, `;`, `&&`, `||`, pipes,
//! subshells, command substitution in `$(...)` and backticks, arithmetic in `$((...))` and
//! `((...))`, comments and here-documents. Each simple command comes out as its words with quotes
//! removed; commands inside a substitution come out as commands of their own. A word records
//! whether the shell expands part of it at run time (`$VAR`, `${...}`, a substitution or
//! arithmetic), because then nobody can tell its value from the script.

use std::iter::Peekable;
use std::str::Chars;

/// One word of a simple command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Word {
    /// The word with quotes removed. An expansion contributes only what is written literally.
    pub(crate) text: String,
    /// Whether the shell expands part of the word at run time.
    pub(crate) dynamic: bool,
}

/// Splits `script` into the words of every simple command it contains, including commands nested
/// in command substitutions.
pub(crate) fn simple_commands(script: &str) -> Vec<Vec<Word>> {
    let mut lexer = Lexer {
        chars: script.chars().peekable(),
        commands: Vec::new(),
        heredocs: Vec::new(),
        arithmetic: 0,
    };
    lexer.commands_until(End::Input);
    lexer.commands
}

struct Lexer<'a> {
    chars: Peekable<Chars<'a>>,
    commands: Vec<Vec<Word>>,
    /// Here-documents whose bodies start after the next newline.
    heredocs: Vec<Heredoc>,
    /// How many arithmetic expressions enclose the current position; inside one, `<<` is a shift.
    arithmetic: u32,
}

struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

/// What ends the commands being lexed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Input,
    /// The `)` closing a `$(` substitution or an arithmetic expression.
    Paren,
    /// The backtick closing a backtick substitution.
    Backtick,
}

/// The words of the simple command being lexed.
#[derive(Default)]
struct Command {
    words: Vec<Word>,
    word: Option<Word>,
    /// Whether the next word is the operand of a redirection, such as a file name or a
    /// here-string, rather than an argument.
    redirection: bool,
}

impl Command {
    fn word(&mut self) -> &mut Word {
        self.word.get_or_insert_with(Word::default)
    }

    fn finish_word(&mut self) {
        if let Some(word) = self.word.take() {
            if std::mem::take(&mut self.redirection) {
                return;
            }
            self.words.push(word);
        }
    }

    /// Ends the word before a redirection operator; a file descriptor number written right
    /// against the operator, as in `2>`, belongs to the operator.
    fn finish_word_before_redirection(&mut self) {
        let descriptor = self.word.as_ref().is_some_and(|word| {
            !word.dynamic && !word.text.is_empty() && word.text.chars().all(|c| c.is_ascii_digit())
        });
        if descriptor {
            self.word = None;
        } else {
            self.finish_word();
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
                '(' if self.chars.next_if_eq(&'(').is_some() => {
                    self.finish_command(&mut command);
                    self.arithmetic();
                }
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
                    command.word().dynamic = true;
                    self.commands_until(End::Backtick);
                }
                '<' | '>' if self.arithmetic > 0 => command.finish_word(),
                '<' | '>' => {
                    command.finish_word_before_redirection();
                    if c == '<' && self.chars.next_if_eq(&'<').is_some() {
                        if self.chars.next_if_eq(&'<').is_some() {
                            command.redirection = true;
                        } else {
                            self.heredoc();
                        }
                    } else {
                        self.chars.next_if(|&next| matches!(next, '>' | '&' | '|'));
                        // `<(...)` and `>(...)` are process substitutions: commands, not files.
                        command.redirection = self.chars.peek() != Some(&'(');
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
    fn word_char(&mut self, c: char, word: &mut Word) {
        match c {
            '\\' => word.text.extend(self.chars.next()),
            '\'' => self.single_quoted(word),
            '"' => self.double_quoted(word),
            '$' if self.chars.next_if_eq(&'\'').is_some() => self.ansi_c_quoted(word),
            '$' if self.chars.next_if_eq(&'"').is_some() => self.double_quoted(word),
            '$' => self.expansion(word),
            other => word.text.push(other),
        }
    }

    fn finish_command(&mut self, command: &mut Command) {
        command.finish_word();
        command.redirection = false;
        if !command.words.is_empty() {
            self.commands.push(std::mem::take(&mut command.words));
        }
    }

    fn single_quoted(&mut self, word: &mut Word) {
        for c in self.chars.by_ref() {
            if c == '\'' {
                return;
            }
            word.text.push(c);
        }
    }

    fn double_quoted(&mut self, word: &mut Word) {
        while let Some(c) = self.chars.next() {
            match c {
                '"' => return,
                '\\' => match self.chars.next() {
                    Some('\n') | None => {}
                    Some(escaped @ ('"' | '\\' | '$' | '`')) => word.text.push(escaped),
                    Some(other) => {
                        word.text.push('\\');
                        word.text.push(other);
                    }
                },
                '$' => self.expansion(word),
                '`' => {
                    word.dynamic = true;
                    self.commands_until(End::Backtick);
                }
                other => word.text.push(other),
            }
        }
    }

    /// Lexes what follows a `$` that is not a quote: a substitution, arithmetic, a parameter, or a
    /// literal `$`.
    fn expansion(&mut self, word: &mut Word) {
        if self.chars.next_if_eq(&'(').is_some() {
            word.dynamic = true;
            if self.chars.next_if_eq(&'(').is_some() {
                self.arithmetic();
            } else {
                self.commands_until(End::Paren);
            }
        } else if self.chars.next_if_eq(&'{').is_some() {
            word.dynamic = true;
            self.parameter(word);
        } else if self
            .chars
            .peek()
            .is_some_and(|&c| c.is_ascii_alphanumeric() || "_@*#?$!-".contains(c))
        {
            word.dynamic = true;
            word.text.push('$');
        } else {
            word.text.push('$');
        }
    }

    /// Lexes an arithmetic expression after its opening `((`, through its closing `))`. Command
    /// substitutions inside it are still lexed; `<<` is a shift.
    fn arithmetic(&mut self) {
        self.arithmetic = self.arithmetic.saturating_add(1);
        self.commands_until(End::Paren);
        self.chars.next_if_eq(&')');
        self.arithmetic = self.arithmetic.saturating_sub(1);
    }

    /// Lexes `${...}` after its `${`, keeping its text.
    fn parameter(&mut self, word: &mut Word) {
        word.text.push_str("${");
        let mut depth: u32 = 0;
        for c in self.chars.by_ref() {
            word.text.push(c);
            match c {
                '{' => depth = depth.saturating_add(1),
                '}' if depth == 0 => return,
                '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }

    /// Lexes `$'...'`, where backslash escapes are decoded.
    fn ansi_c_quoted(&mut self, word: &mut Word) {
        while let Some(c) = self.chars.next() {
            match c {
                '\'' => return,
                '\\' => match self.chars.next() {
                    Some('n') => word.text.push('\n'),
                    Some('t') => word.text.push('\t'),
                    Some(other) => word.text.push(other),
                    None => {}
                },
                other => word.text.push(other),
            }
        }
    }

    fn skip_comment(&mut self) {
        while self.chars.next_if(|&c| c != '\n').is_some() {}
    }

    /// Reads a here-document's delimiter after its `<<`; the body follows the next newline.
    fn heredoc(&mut self) {
        let strip_tabs = self.chars.next_if_eq(&'-').is_some();
        while self.chars.next_if(|&c| c == ' ' || c == '\t').is_some() {}
        let mut delimiter = Word::default();
        while let Some(c) = self
            .chars
            .next_if(|&c| !c.is_whitespace() && !";&|()<>".contains(c))
        {
            match c {
                '\'' => self.single_quoted(&mut delimiter),
                '"' => self.double_quoted(&mut delimiter),
                '\\' => delimiter.text.extend(self.chars.next()),
                other => delimiter.text.push(other),
            }
        }
        if !delimiter.text.is_empty() {
            self.heredocs.push(Heredoc {
                delimiter: delimiter.text,
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

    /// The words of each command in `script`.
    fn lex(script: &str) -> Vec<Vec<String>> {
        simple_commands(script)
            .into_iter()
            .map(|words| words.into_iter().map(|word| word.text).collect())
            .collect()
    }

    /// Whether each word of each command in `script` is expanded at run time.
    fn dynamic(script: &str) -> Vec<Vec<bool>> {
        simple_commands(script)
            .into_iter()
            .map(|words| words.into_iter().map(|word| word.dynamic).collect())
            .collect()
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
        assert_eq!(
            dynamic(r#"echo "\$c""#),
            [[false, false]],
            "an escaped $ is literal"
        );
    }

    #[test]
    fn decodes_ansi_c_quotes() {
        assert_eq!(
            lex(r"git push $'--force' $'a\tb'"),
            [["git", "push", "--force", "a\tb"]],
            "$'...' is a quoted word"
        );
        assert_eq!(
            dynamic(r"git push $'--force'"),
            [[false, false, false]],
            "and holds no expansion"
        );
    }

    #[test]
    fn marks_words_the_shell_expands() {
        assert_eq!(
            dynamic(r#"git push $F "${REF}" "$(echo x)" `echo y` $((1+2)) '$LITERAL' $ plain"#),
            [
                vec![false, false],
                vec![false, false],
                vec![false],
                vec![
                    false, false, true, true, true, true, true, false, false, false
                ],
            ],
            "variables, parameters, substitutions and arithmetic are expanded; quotes and a lone \
             $ are not"
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
    fn a_shift_in_arithmetic_is_not_a_heredoc() {
        assert_eq!(
            lex("echo $((1<<2))\ngit push -f"),
            [vec!["1", "2"], vec!["echo", ""], vec!["git", "push", "-f"]],
            "the next line is still a command"
        );
        assert_eq!(
            lex("((x = 1 << 2))\ngit push -f"),
            [vec!["x", "=", "1", "2"], vec!["git", "push", "-f"]],
            "the same holds for an arithmetic command"
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
    fn here_strings_are_not_arguments() {
        assert_eq!(
            lex("bash <<< 'git push -f'"),
            [["bash"]],
            "the here-string is standard input"
        );
    }

    #[test]
    fn redirection_operands_are_not_arguments() {
        assert_eq!(
            lex("git push -f>out 2>&1 < in >> log 2> err"),
            [["git", "push", "-f"]],
            "files and descriptors belong to the redirections"
        );
        assert_eq!(
            lex("echo 2 > out"),
            [["echo", "2"]],
            "a number set apart from the operator is an argument"
        );
        assert_eq!(
            lex("diff <(git show HEAD:a) b"),
            [vec!["diff"], vec!["git", "show", "HEAD:a"], vec!["b"]],
            "a process substitution is a command"
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
