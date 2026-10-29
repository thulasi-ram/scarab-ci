//! Which arguments of a Step's `command` name the program that runs
//! (ADR-0076): `command[0]`, and the program each wrapper in front of it
//! runs (`env`, `sudo`, `timeout`, `xargs`, ...). A value from outside the
//! Pipeline may not name one (see [`crate::render`]).
//!
//! The wrappers are read the way they read their own arguments: options
//! first, up to the first argument that is not one (each uses getopt with
//! `+`), then `NAME=value` arguments for `env` and `sudo`, then the operands
//! in front of the program (`timeout`'s duration). Where that reading is
//! not certain (a long option abbreviated to a prefix, which getopt
//! accepts), every reading is followed, so each argument that could be the
//! program is one.

use std::collections::BTreeSet;

/// How a wrapper reads the arguments in front of the program it runs.
struct Wrapper {
    /// Short options that take an argument: the rest of their group, or the
    /// next argument when the group ends with them.
    short_args: &'static str,
    /// Short options whose argument is optional and only ever written in
    /// the same group (`xargs -i{}`): the rest of the group is theirs.
    short_optional: &'static str,
    /// Long options that take an argument, after `=` or as the next one.
    long_args: &'static [&'static str],
    /// The option whose argument is itself a command line (`env -S`).
    split: Option<(char, &'static str)>,
    /// Whether `NAME=value` arguments may follow the options.
    assigns: bool,
    /// Operands before the program (`timeout`'s duration).
    operands: usize,
    /// Whether a lone `-` is an option (`env -` is `env -i`).
    dash_is_option: bool,
}

const fn wrapper(short_args: &'static str, long_args: &'static [&'static str]) -> Wrapper {
    Wrapper {
        short_args,
        short_optional: "",
        long_args,
        split: None,
        assigns: false,
        operands: 0,
        dash_is_option: false,
    }
}

/// The wrapper a program name is, if any.
fn wrapper_of(program: &str) -> Option<Wrapper> {
    let name = program.rsplit('/').next().unwrap_or(program);
    Some(match name {
        "env" => Wrapper {
            split: Some(('S', "--split-string")),
            assigns: true,
            dash_is_option: true,
            ..wrapper("uCS", &["--unset", "--chdir", "--split-string"])
        },
        "sudo" => Wrapper {
            assigns: true,
            ..wrapper(
                "ughpCDrtUTR",
                &[
                    "--user",
                    "--group",
                    "--host",
                    "--prompt",
                    "--close-from",
                    "--chdir",
                    "--role",
                    "--type",
                    "--other-user",
                    "--command-timeout",
                    "--chroot",
                ],
            )
        },
        "timeout" => Wrapper {
            operands: 1,
            ..wrapper("sk", &["--signal", "--kill-after"])
        },
        "nice" => wrapper("n", &["--adjustment"]),
        "stdbuf" => wrapper("ioe", &["--input", "--output", "--error"]),
        "exec" => wrapper("a", &[]),
        "xargs" => Wrapper {
            short_optional: "eil",
            ..wrapper(
                "adEILnPs",
                &[
                    "--arg-file",
                    "--delimiter",
                    "--max-args",
                    "--max-procs",
                    "--max-chars",
                    "--process-slot-var",
                ],
            )
        },
        "busybox" | "nohup" | "setsid" => wrapper("", &[]),
        _ => return None,
    })
}

/// An argument that names the program a Step runs, from `start` (a byte
/// offset): `0`, or where the command line of `env -S…` starts in the same
/// argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Program {
    pub index: usize,
    pub start: usize,
}

/// Every argument of `command` that may name a program (see the module
/// docs), in order.
pub(crate) fn programs(command: &[impl AsRef<str>]) -> BTreeSet<Program> {
    let mut found = BTreeSet::new();
    let mut todo = vec![0usize];
    let mut seen = BTreeSet::new();
    while let Some(at) = todo.pop() {
        if at >= command.len() || !seen.insert(at) {
            continue;
        }
        found.insert(Program {
            index: at,
            start: 0,
        });
        if let Some(w) = wrapper_of(command[at].as_ref()) {
            let (targets, splits) = runs(command, at, &w);
            found.extend(splits);
            todo.extend(targets);
        }
    }
    found
}

/// Where the wrapper `command[at]` may find the program it runs, and the
/// arguments of its split-string option.
fn runs(command: &[impl AsRef<str>], at: usize, w: &Wrapper) -> (Vec<usize>, Vec<Program>) {
    let arg = |i: usize| command.get(i).map(AsRef::as_ref);
    let mut targets = Vec::new();
    let mut splits = Vec::new();
    // Indexes still read as options; each is visited once.
    let mut todo = vec![at + 1];
    let mut seen = BTreeSet::new();
    while let Some(i) = todo.pop() {
        if !seen.insert(i) {
            continue;
        }
        let Some(a) = arg(i) else { continue };
        if a == "--" {
            targets.extend(target(command, i + 1, w));
        } else if a == "-" && w.dash_is_option {
            todo.push(i + 1);
        } else if let Some(long) = a.strip_prefix("--").filter(|l| !l.is_empty()) {
            let (name, attached) = match long.split_once('=') {
                Some((n, _)) => (n, true),
                None => (long, false),
            };
            let name = &a[..2 + name.len()];
            // getopt accepts any unambiguous prefix of a long option.
            let prefix_of = |full: &str| full.starts_with(name);
            let split = w.split.is_some_and(|(_, l)| prefix_of(l));
            if attached {
                if split {
                    splits.push(Program {
                        index: i,
                        start: a.find('=').map_or(a.len(), |e| e + 1),
                    });
                }
                todo.push(i + 1);
            } else if w.long_args.iter().any(|l| prefix_of(l)) {
                if split {
                    splits.push(Program {
                        index: i + 1,
                        start: 0,
                    });
                }
                todo.push(i + 2);
                // A prefix that is also an option without an argument.
                if !w.long_args.contains(&name) {
                    todo.push(i + 1);
                }
            } else {
                todo.push(i + 1);
            }
        } else if a.len() > 1 && a.starts_with('-') {
            let mut next = i + 1;
            for (o, c) in a.char_indices().skip(1) {
                let rest = o + c.len_utf8();
                if w.short_args.contains(c) {
                    let split = w.split.is_some_and(|(s, _)| s == c);
                    if rest == a.len() {
                        next = i + 2;
                        if split {
                            splits.push(Program {
                                index: i + 1,
                                start: 0,
                            });
                        }
                    } else if split {
                        splits.push(Program {
                            index: i,
                            start: rest,
                        });
                    }
                    break;
                }
                if w.short_optional.contains(c) {
                    break;
                }
            }
            todo.push(next);
        } else {
            targets.extend(target(command, i, w));
        }
    }
    (targets, splits)
}

/// The program after a wrapper's options end at `i`: past any `NAME=value`
/// arguments and the operands.
fn target(command: &[impl AsRef<str>], mut i: usize, w: &Wrapper) -> Option<usize> {
    while w.assigns && command.get(i).is_some_and(|a| a.as_ref().contains('=')) {
        i += 1;
    }
    i += w.operands;
    (i < command.len()).then_some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(command: &[&str]) -> Vec<(usize, usize)> {
        programs(command)
            .into_iter()
            .map(|p| (p.index, p.start))
            .collect()
    }

    #[test]
    fn programs_follow_each_wrapper() {
        assert_eq!(at(&["git", "fetch"]), [(0, 0)]);
        assert_eq!(at(&[]), []);
        assert_eq!(at(&["env", "A=b", "-x", "y"]), [(0, 0), (2, 0)]);
        assert_eq!(at(&["env", "-i", "-u", "X", "p"]), [(0, 0), (4, 0)]);
        assert_eq!(at(&["env", "-iu", "X", "p"]), [(0, 0), (3, 0)]);
        assert_eq!(at(&["env", "-uX", "p"]), [(0, 0), (2, 0)]);
        assert_eq!(at(&["env", "-", "p"]), [(0, 0), (2, 0)]);
        assert_eq!(at(&["env", "--", "A=b", "p"]), [(0, 0), (3, 0)]);
        assert_eq!(at(&["env", "--unset=X", "p"]), [(0, 0), (2, 0)]);
        // An abbreviated long option: both readings.
        assert_eq!(at(&["env", "--u", "X", "p"]), [(0, 0), (2, 0), (3, 0)]);
        // `-S`: its argument, and the program after it.
        assert_eq!(at(&["env", "-S", "s", "p"]), [(0, 0), (2, 0), (3, 0)]);
        assert_eq!(at(&["env", "-iSs"]), [(0, 0), (1, 3)]);
        assert_eq!(at(&["env", "--split-string=s"]), [(0, 0), (1, 15)]);
        assert_eq!(at(&["env", "--split", "s"]), [(0, 0), (2, 0)]);
        assert_eq!(at(&["timeout", "-s", "KILL", "60", "p"]), [(0, 0), (4, 0)]);
        assert_eq!(at(&["sudo", "-Eu", "root", "p"]), [(0, 0), (3, 0)]);
        assert_eq!(
            at(&["nice", "-n", "5", "nohup", "p"]),
            [(0, 0), (3, 0), (4, 0)]
        );
        assert_eq!(at(&["xargs", "-in", "p"]), [(0, 0), (2, 0)]);
        assert_eq!(at(&["xargs", "-n", "1", "p"]), [(0, 0), (3, 0)]);
        assert_eq!(at(&["/usr/bin/stdbuf", "-oL", "p"]), [(0, 0), (2, 0)]);
        assert_eq!(at(&["busybox", "sh"]), [(0, 0), (1, 0)]);
        // A wrapper with nothing after it runs nothing.
        assert_eq!(at(&["env", "-i"]), [(0, 0)]);
        assert_eq!(at(&["timeout", "60"]), [(0, 0)]);
    }
}
