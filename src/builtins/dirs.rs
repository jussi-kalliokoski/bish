// `cd`, `pushd`, `popd`, `dirs`: where the shell is, and the stack of
// where it has been.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::exec::{RESTRICTED, Shell, sh_eprintln, sh_println};

pub(crate) fn run_cd(sh: &mut Shell, args: &[String]) -> i32 {
    // `-P` resolves symlinks; `-L`, the default, keeps the route you
    // took (see Shell::change_directory). The comment that used to sit
    // here said this shell had "only ever had the logical behaviour"
    // and that `-P` was therefore safe to ignore -- it was the other
    // way round, and `-P` was the only behaviour there was.
    //
    // What is *not* ignored is a second operand: bash rejects that, and
    // accepting it silently hid a typo'd path.
    let physical = args.iter().rposition(|a| a == "-P" || a == "-L").is_some_and(|i| args[i] == "-P");
    let operands: Vec<&String> = args.iter().filter(|a| !matches!(a.as_str(), "-L" | "-P" | "-@" | "-e")).collect();
    if operands.len() > 1 {
        // 2, not 1: bash reserves 2 for a builtin's own usage error
        // and 1 for the operation failing.
        sh_eprintln!(sh, "bish: cd: too many arguments");
        return 2;
    }
    let old = sh.cwd.to_string_lossy().into_owned();
    let target = if let Some(dir) = operands.first() {
        if dir.as_str() == "-" {
            // The shell's own `OLDPWD`, not the process environment's.
            // `cd` is the thing that *writes* OLDPWD, and it writes it
            // to the shell (see Shell::chdir) -- reading it back out of
            // `environ` found whatever this process was started with,
            // so `cd -` went somewhere the shell had never been, and
            // `unset OLDPWD` did not stop it. Same for HOME below,
            // which is why `HOME=/x; cd` ignored the assignment while
            // `echo ~` honoured it.
            if !sh.var_is_set("OLDPWD") {
                sh_eprintln!(sh, "bish: cd: OLDPWD not set");
                return 1;
            }
            let p = sh.lookup_var("OLDPWD");
            sh_println!(sh, "{}", p);
            p
        } else {
            (*dir).clone()
        }
    } else {
        match sh.var_is_set("HOME").then(|| sh.lookup_var("HOME")) {
            Some(h) => h,
            None => {
                sh_eprintln!(sh, "bish: cd: HOME not set");
                return 1;
            }
        }
    };
    let _ = old;
    // `CDPATH`: a search path for `cd`, so `cd bish` from anywhere
    // finds `~/src/bish`. Only for a plain relative name -- an
    // absolute path, or one that already says `.`/`..`, means
    // exactly where it says and is never searched for.
    //
    // A hit that did not come from the current directory prints
    // where it landed, which is bash's own behaviour and not a
    // courtesy: without it, `cd bish` silently putting you somewhere
    // other than `./bish` would be the worst kind of surprise.
    let (target, from_cdpath) = sh.resolve_cdpath(target);
    // `-P` asks for the destination rather than the route, which is
    // what resolving it up front gives: the lexical normalisation
    // inside `change_directory` then has nothing left to cancel.
    let resolved = match physical {
        true => std::fs::canonicalize(&target).unwrap_or_else(|_| std::path::PathBuf::from(&target)),
        false => std::path::PathBuf::from(&target),
    };
    match sh.change_directory(&resolved) {
        Ok(()) => {
            if from_cdpath {
                sh_println!(sh, "{}", sh.cwd.display());
            }
            0
        }
        Err(e) if e == RESTRICTED => {
            sh_eprintln!(sh, "bish: cd: restricted");
            1
        }
        Err(e) => {
            sh_eprintln!(sh, "bish: cd: {}: {}", target, e);
            1
        }
    }
}

pub(crate) fn run_pushd(sh: &mut Shell, args: &[String]) -> i32 {
    // `-n`: add to the stack without changing directory. bash's own
    // flag, and the only way to describe a directory stack to a shell
    // that does not have one yet -- which is what the preamble a
    // re-exec'd construct is handed has to do (see
    // `Shell::functions_preamble`).
    if args.iter().any(|a| a == "-n") {
        let Some(dir) = args.iter().find(|a| !a.starts_with('-')) else {
            sh_eprintln!(sh, "bish: pushd: no other directory");
            return 1;
        };
        sh.dir_stack.insert(0, dir.clone());
        sh.print_dirs(false);
        return 0;
    }
    let target = match args.iter().find(|a| !a.starts_with('-')) {
        Some(d) => d.clone(),
        None => match sh.dir_stack.first() {
            Some(d) => d.clone(),
            None => {
                sh_eprintln!(sh, "bish: pushd: no other directory");
                return 1;
            }
        },
    };
    if !args.iter().any(|a| !a.starts_with('-')) {
        // Bare `pushd`: rotate -- cd into the current top-of-stack,
        // then push the old cwd back onto the front, net-swapping them.
        sh.dir_stack.remove(0);
    }
    let old_cwd = sh.cwd.to_string_lossy().into_owned();
    if run_cd(sh, &[target]) != 0 {
        return 1;
    }
    sh.dir_stack.insert(0, old_cwd);
    sh.print_dirs(false);
    0
}

pub(crate) fn run_popd(sh: &mut Shell, _args: &[String]) -> i32 {
    let target = match sh.dir_stack.first() {
        Some(d) => d.clone(),
        None => {
            sh_eprintln!(sh, "bish: popd: directory stack empty");
            return 1;
        }
    };
    if run_cd(sh, &[target]) != 0 {
        return 1;
    }
    sh.dir_stack.remove(0);
    sh.print_dirs(false);
    0
}

pub(crate) fn run_dirs(sh: &mut Shell, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "-c") {
        sh.dir_stack.clear();
        return 0;
    }
    sh.print_dirs(args.iter().any(|a| a == "-v"));
    0
}
