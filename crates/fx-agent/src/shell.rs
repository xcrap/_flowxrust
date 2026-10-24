//! Whether a shell command only reads.
//!
//! Both providers ask this — Codex to rate an approval and to answer one when
//! nobody is watching, Claude to rate a `Bash` call — and they used to answer it
//! with two different prefix checks, each of which could be walked around:
//! `find . -delete`, `ls & rm -rf x`, a second command on its own line, `$(…)`,
//! a backtick, or an allowed tool's own write flag (`sort -o`,
//! `git diff --output`, `fd -x`, `rg --pre`). This is the one classifier both
//! use now.
//!
//! It fails closed. A command is read-only only when all of it is understood:
//! one simple command, no control operator, no substitution, no redirection, a
//! program on a short list, and none of that program's options that write or
//! run something else. Anything it cannot read is not read-only, because
//! over-warning costs a confirmation and under-warning costs an unapproved
//! write.

/// Programs whose every option only reads, so their arguments need no check.
///
/// `env` and `printenv` are deliberately absent. With no argument they print
/// the whole environment, which on a developer machine is where API keys live,
/// and the approval row would label that read-only.
const PLAIN_READERS: &[&str] = &[
    "basename", "cat", "df", "diff", "dirname", "du", "echo", "grep", "head", "ls", "pwd", "stat",
    "tail", "uname", "wc", "which", "whoami",
];

/// Programs that read by default but have options that write or run something
/// else. Each is checked by [`guarded_reads_only`].
const GUARDED_READERS: &[&str] = &["date", "fd", "file", "find", "git", "rg", "sort", "uniq"];

/// Where a program may be named from by absolute path.
///
/// `./cat` or `/tmp/x/cat` is a script that merely shares a reader's name, so a
/// path counts only when it is one of the system's own directories.
const SYSTEM_DIRECTORIES: &[&str] = &["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];

/// Shells whose `-c` script is classified in place of the shell itself.
///
/// Codex runs nearly every command as `bash -lc '…'` or `zsh -lc '…'`. Without
/// looking inside, every one of them is "a shell", and a deny-unless-read-only
/// policy refuses every read.
const SHELLS: &[&str] = &["bash", "sh", "zsh"];

/// `find` primaries that delete, write a file, or run a program.
const FIND_ACTIONS: &[&str] = &[
    "-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls",
];

/// Whether a command line, as a shell would read it, only reads.
pub fn is_read_only_command(command: &str) -> bool {
    split_words(command).is_some_and(|words| reads_only(&words, 0))
}

/// Whether an argv, executed directly with no shell in between, only reads.
///
/// Classified as argv rather than joined and re-split: a quote inside one
/// element would move the boundaries on the way back, so `["find", ".", "'",
/// "-delete", "'"]` would read as a harmless quoted word and run as `-delete`.
pub fn is_read_only_argv(argv: &[impl AsRef<str>]) -> bool {
    let words: Vec<Word> = argv
        .iter()
        .map(|word| Word {
            text: word.as_ref().to_string(),
            expands: false,
        })
        .collect();
    reads_only(&words, 0)
}

/// One word, after the shell has removed its quoting.
#[derive(Debug, Default)]
struct Word {
    text: String,
    /// Whether it held an unquoted glob, so the shell may turn it into names
    /// nobody wrote — including one that reads as an option.
    expands: bool,
}

/// Split a command line into words the way a POSIX shell would, or `None` for
/// anything other than one simple command.
///
/// Quoting is honoured, because `grep 'a|b' file` is one search and not a
/// pipeline. Everything that makes a shell do more than run one program is
/// refused rather than modelled: a control operator (`;`, `&`, `|`, a
/// newline), redirection, any `$` or backtick (variables, command and
/// arithmetic substitution), a subshell or group, brace expansion, and a
/// comment.
fn split_words(command: &str) -> Option<Vec<Word>> {
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    let mut characters = command.chars();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' => words.extend(current.take()),
            '\'' => {
                let word = current.get_or_insert_with(Word::default);
                loop {
                    match characters.next()? {
                        '\'' => break,
                        '\0' => return None,
                        other => word.text.push(other),
                    }
                }
            }
            '"' => {
                let word = current.get_or_insert_with(Word::default);
                loop {
                    match characters.next()? {
                        '"' => break,
                        // Expanded inside double quotes, so still substitution.
                        '$' | '`' | '\0' => return None,
                        '\\' => match characters.next()? {
                            escaped @ ('$' | '`' | '"' | '\\') => word.text.push(escaped),
                            '\n' | '\0' => return None,
                            other => {
                                word.text.push('\\');
                                word.text.push(other);
                            }
                        },
                        other => word.text.push(other),
                    }
                }
            }
            '\\' => match characters.next()? {
                // A line continuation joins two commands into one line.
                '\n' | '\r' | '\0' => return None,
                escaped => current.get_or_insert_with(Word::default).text.push(escaped),
            },
            ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' | '$' | '{' | '}' | '\n' | '\r'
            | '\0' => return None,
            '#' if current.is_none() => return None,
            '*' | '?' | '[' => {
                let word = current.get_or_insert_with(Word::default);
                word.expands = true;
                word.text.push(character);
            }
            other => current.get_or_insert_with(Word::default).text.push(other),
        }
    }
    words.extend(current);
    Some(words)
}

/// Whether one simple command, already split into words, only reads.
///
/// `depth` bounds unwrapping: a shell's `-c` script is looked into once, and a
/// shell inside that script is just an unknown program.
fn reads_only(words: &[Word], depth: usize) -> bool {
    let Some((program, arguments)) = words.split_first() else {
        return false;
    };
    if program.expands {
        return false;
    }
    let Some(name) = program_name(&program.text) else {
        return false;
    };
    if SHELLS.contains(&name) {
        return depth == 0
            && shell_script(arguments)
                .and_then(split_words)
                .is_some_and(|inner| reads_only(&inner, depth + 1));
    }
    if PLAIN_READERS.contains(&name) {
        return true;
    }
    // A glob can expand to a file whose name reads as one of the options
    // checked below, so a tool that has such options takes no globs at all.
    GUARDED_READERS.contains(&name)
        && !arguments.iter().any(|argument| argument.expands)
        && guarded_reads_only(
            name,
            &arguments
                .iter()
                .map(|argument| argument.text.as_str())
                .collect::<Vec<_>>(),
        )
}

/// The program a command word names, when it may be trusted to be that program.
fn program_name(word: &str) -> Option<&str> {
    match word.rsplit_once('/') {
        None => Some(word),
        Some((directory, name)) => SYSTEM_DIRECTORIES.contains(&directory).then_some(name),
    }
}

/// The script of `sh -c script`, and nothing else.
///
/// Extra operands after the script become its positional parameters, which is
/// one more thing to model, so they are refused.
fn shell_script(arguments: &[Word]) -> Option<&str> {
    match arguments {
        [flag, script] if matches!(flag.text.as_str(), "-c" | "-lc") && !script.expands => {
            Some(&script.text)
        }
        _ => None,
    }
}

/// Whether a reader that has write-capable options was given none of them.
fn guarded_reads_only(program: &str, arguments: &[&str]) -> bool {
    let none = |dangerous: &dyn Fn(&str) -> bool| !arguments.iter().any(|a| dangerous(a));
    match program {
        "find" => none(&|argument| FIND_ACTIONS.contains(&argument)),
        "fd" => none(&|argument| {
            long_option(argument, &["--exec", "--exec-batch"]) || short_option(argument, "xX")
        }),
        "rg" => none(&|argument| long_option(argument, &["--pre", "--hostname-bin"])),
        "sort" => none(&|argument| {
            long_option(argument, &["--output", "--compress-program"])
                || short_option(argument, "o")
        }),
        // `uniq input output` writes its second operand.
        "uniq" => operands(arguments) <= 1,
        // `file -C` compiles a magic database to disk.
        "file" => {
            none(&|argument| long_option(argument, &["--compile"]) || short_option(argument, "C"))
        }
        // Anything but an output format may set the clock.
        "date" => arguments
            .iter()
            .all(|argument| argument.starts_with('+') || matches!(*argument, "-u" | "-R")),
        // The subcommand must come first: a global option before it (`-c`,
        // `-C`, `--exec-path`) can point git at a program to run.
        "git" => {
            matches!(
                arguments.first(),
                Some(&("status" | "log" | "diff" | "show"))
            ) && none(&|argument| long_option(argument, &["--output", "--ext-diff"]))
        }
        _ => false,
    }
}

/// Whether an argument is one of these long options, with or without `=value`.
///
/// An unambiguous prefix counts too, because getopt and git both accept one:
/// `sort --out=f` writes `f` exactly as `--output=f` does.
fn long_option(argument: &str, dangerous: &[&str]) -> bool {
    let Some(name) = argument
        .strip_prefix("--")
        .map(|rest| rest.split('=').next().unwrap_or(rest))
    else {
        return false;
    };
    !name.is_empty()
        && dangerous.iter().any(|option| {
            let option = option.trim_start_matches('-');
            option == name || option.starts_with(name)
        })
}

/// Whether an argument is a cluster of short options containing any of these.
///
/// Read as a cluster because `sort -uo file` writes `file` as surely as
/// `sort -o file` does. A letter that is really an option's value is a false
/// alarm, which is the cheap direction to be wrong in.
fn short_option(argument: &str, letters: &str) -> bool {
    argument
        .strip_prefix('-')
        .filter(|rest| !rest.is_empty() && !rest.starts_with('-'))
        .is_some_and(|cluster| cluster.chars().any(|letter| letters.contains(letter)))
}

/// How many operands an argument list has, counting an option's value as one.
fn operands(arguments: &[&str]) -> usize {
    let mut count = 0;
    let mut options_ended = false;
    for argument in arguments {
        if options_ended || !argument.starts_with('-') || *argument == "-" {
            count += 1;
        } else if *argument == "--" {
            options_ended = true;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_reads_are_read_only() {
        for command in [
            "ls",
            "ls -la",
            "  grep foo  ",
            "/bin/cat file",
            "cat Cargo.toml",
            "cat *.rs",
            "wc -l src/*.rs",
            "grep 'a|b' file",
            "grep \"a;b\" file",
            "echo '$(not run)'",
            "git status",
            "git log --oneline -5",
            "git diff --stat",
            "git show HEAD --output-indicator-new=+",
            "find . -name '*.rs'",
            "find . ! -name x -type f",
            "rg -n foo src",
            "rg --pre-glob '*.gz' foo",
            "fd -e rs",
            "sort -u names",
            "uniq names",
            "date +%s",
            "file Cargo.toml",
            "cat 'a file with spaces'",
            "cat ''",
        ] {
            assert!(is_read_only_command(command), "{command:?} was refused");
        }
    }

    #[test]
    fn every_known_bypass_is_refused() {
        for command in [
            // Control operators and substitution, however they are spelled.
            "ls & rm -rf x",
            "ls; rm -rf /",
            "ls | sh",
            "cat x && rm y",
            "cat x || rm y",
            "ls\nrm -rf x",
            "ls\rrm -rf x",
            "ls \\\nrm",
            "cat $(rm x)",
            "cat \"$(rm x)\"",
            "cat `rm x`",
            "cat \"`rm x`\"",
            "cat $HOME/.ssh/id_rsa",
            "cat > out.txt",
            "cat < in > out",
            "(rm x)",
            "{ rm x; }",
            "sort {-o,out}",
            "ls # rm",
            "cat 'unterminated",
            "cat \"unterminated",
            // An allowed tool's own write or exec options.
            "find . -delete",
            "find . -exec rm {} +",
            "find . -execdir rm {} \\;",
            "find . -fprint list",
            "find *",
            "sort -o f x",
            "sort -uo f x",
            "sort --output=f x",
            "sort --out=f x",
            "sort --compress-program=sh x",
            "uniq in out",
            "git diff --output=f",
            "git diff --output f",
            "git log --outp=f",
            "git diff --ext-diff",
            "git -c core.pager=sh log",
            "git -C /tmp status",
            "git commit -m x",
            "fd -x rm",
            "fd -Hx rm",
            "fd --exec rm",
            "fd -X rm",
            "rg --pre sh foo",
            "rg --pre=sh foo",
            "rg --hostname-bin=sh foo",
            "file -C -m magic",
            "date -s 2020-01-01",
            "date 0101000025",
            // Programs that are not readers, or not the reader they name.
            "rm -rf /",
            "env",
            "printenv",
            "less file",
            "FOO=1 cat x",
            "./cat x",
            "/tmp/x/cat x",
            "catalog",
            "bash -lc 'ls; rm x'",
            "bash -lc \"bash -lc 'cat x'\"",
            "bash -c 'cat x' extra",
            "",
            "  ",
            "cat x\0",
        ] {
            assert!(
                !is_read_only_command(command),
                "{command:?} was treated as read-only"
            );
        }
    }

    #[test]
    fn a_shell_wrapper_is_judged_by_its_script() {
        // The shape Codex sends almost every command in.
        assert!(is_read_only_command("/bin/zsh -lc 'rg -n foo src'"));
        assert!(is_read_only_command("bash -c \"cat Cargo.toml\""));
        assert!(!is_read_only_command("/bin/zsh -lc 'rm -rf build'"));
        assert!(!is_read_only_command("/tmp/bash -lc 'cat x'"));
        assert!(is_read_only_argv(&["/bin/zsh", "-lc", "cat Cargo.toml"]));
        assert!(!is_read_only_argv(&["bash", "-lc", "cat x | sh"]));
    }

    #[test]
    fn argv_is_classified_without_re_splitting_it() {
        assert!(is_read_only_argv(&["cat", "Cargo.toml"]));
        // A shell would never see these: no shell runs them.
        assert!(is_read_only_argv(&["cat", "a;b"]));
        assert!(is_read_only_argv(&["grep", "$HOME", "notes"]));
        // Joined, the quotes would hide `-delete` inside one harmless word.
        assert!(!is_read_only_argv(&["find", ".", "'", "-delete", "'"]));
        assert!(!is_read_only_argv(&["rm", "-rf", "/"]));
        assert!(!is_read_only_argv(&[] as &[&str]));
    }
}
