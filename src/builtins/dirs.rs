// `cd`, `pushd`, `popd`, `dirs`: where the shell is, and the stack of
// where it has been.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::exec::{sh_eprintln, sh_println, RESTRICTED, Shell};

pub(crate) fn run_cd(sh: &mut Shell, args: &[String]) -> i32 {
    let old = sh.cwd.to_string_lossy().into_owned();
    let target = if let Some(dir) = args.first() {
        if dir == "-" {
            match std::env::var("OLDPWD") {
                Ok(p) => {
                    sh_println!(sh, "{}", p);
                    p
                }
                Err(_) => {
                    sh_eprintln!(sh, "cd: OLDPWD not set");
                    return 1;
                }
            }
        } else {
            dir.clone()
        }
    } else {
        match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => {
                sh_eprintln!(sh, "cd: HOME not set");
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
    match sh.change_directory(std::path::Path::new(&target)) {
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
            sh_eprintln!(sh, "cd: {}: {}", target, e);
            1
        }
    }
}

pub(crate) fn run_pushd(sh: &mut Shell, args: &[String]) -> i32 {
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
