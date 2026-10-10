// `cd`, `pushd`, `popd` and `dirs`.

use crate::exec::{RESTRICTED, Shell, sh_eprintln, sh_println};

pub(crate) fn run_cd(sh: &mut Shell, args: &[String]) -> i32 {
    // `-P` resolves symlinks; `-L`, the default, keeps the path as
    // given (see `Shell::change_directory`).
    let physical = args.iter().rposition(|a| a == "-P" || a == "-L").is_some_and(|i| args[i] == "-P");
    let operands: Vec<&String> = args.iter().filter(|a| !matches!(a.as_str(), "-L" | "-P" | "-@" | "-e")).collect();
    if operands.len() > 1 {
        // bash's status for a usage error.
        sh_eprintln!(sh, "bish: cd: too many arguments");
        return 2;
    }
    let old = sh.cwd.to_string_lossy().into_owned();
    let target = if let Some(dir) = operands.first() {
        if dir.as_str() == "-" {
            // The shell's OLDPWD and HOME, not the process environment's:
            // `cd` writes OLDPWD to the shell, and `HOME=/x; cd` has to
            // honour the assignment.
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
    // `CDPATH` is searched only for a plain relative name. A directory
    // found through it is printed, as bash does.
    let (target, from_cdpath) = sh.resolve_cdpath(target);
    // Resolved up front, so the lexical normalisation in
    // `change_directory` has nothing left to undo.
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
    // `-n` pushes without changing directory. `Shell::functions_preamble`
    // uses it to rebuild the stack in a re-exec'd shell.
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
        // Bare `pushd` swaps the cwd with the top of the stack.
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
