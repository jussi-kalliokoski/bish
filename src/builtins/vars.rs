// `declare`, `readonly`, `unset`: the builtins that make, mark and
// unmake variables.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::arith;
use crate::parser::ArrayLiteralItem;
use crate::parser::AssignMode;
use crate::exec::{write_diagnostic, Shell};

// unset [-f|-v] NAME... Also accepts `arr[i]` to remove one element
// without touching the rest of the array. `stderr_target` mirrors real
// bash routing this error through the command's own `2>` (confirmed via
// a clean bash probe) -- unlike nounset/plain-assignment errors, which
// always go to real stderr since they happen before any redirect setup.
pub(crate) fn run_unset(sh: &mut Shell, args: &[String], stderr_target: &Option<String>) -> i32 {
    let mut only_funcs = false;
    let mut only_vars = false;
    let mut names: Vec<&String> = Vec::new();
    let mut status = 0;
    if let Some(bad) = crate::exec::first_unknown_option(args, "fvn") {
        return crate::exec::bad_option_status(sh, "unset", &bad, "unset [-f] [-v] [-n] [name ...]");
    }
    for a in args {
        match a.as_str() {
            "-f" => only_funcs = true,
            "-v" => only_vars = true,
            _ => names.push(a),
        }
    }
    for n in names {
        if only_funcs {
            sh.functions.remove(n.as_str());
            continue;
        }
        if let Some(bracket) = n.find('[') {
            if let Some(idx_expr) = n.strip_suffix(']').map(|s| &s[bracket + 1..]) {
                let arr_name = n[..bracket].to_string();
                if sh.assoc_names.contains(&arr_name) {
                    let key = sh.expand_index_as_string(idx_expr);
                    if let Some(map) = sh.assoc_arrays.get_mut(&arr_name) {
                        map.remove(&key);
                    }
                } else if let Ok(i) = arith::eval(idx_expr, sh) {
                    if let Some(idx) = sh.resolve_array_index(&arr_name, i) {
                        if let Some(map) = sh.arrays.get_mut(&arr_name) {
                            map.remove(&idx);
                        }
                    }
                }
                continue;
            }
        }
        if sh.name_is_readonly(n) {
            write_diagnostic(stderr_target, &format!("bish: unset: {}: cannot unset: readonly variable", n), sh.sink.clone());
            status = 1;
            continue;
        }
        sh.arrays.remove(n.as_str());
        sh.assoc_arrays.remove(n.as_str());
        sh.assoc_names.remove(n.as_str());
        let mut removed_local = false;
        for scope in sh.var_scopes.iter_mut().rev() {
            if scope.remove(n.as_str()).is_some() {
                removed_local = true;
                break;
            }
        }
        if !removed_local {
            unsafe {
                std::env::remove_var(n);
            }
        }
        if !only_vars {
            sh.functions.remove(n.as_str());
        }
    }
    status
}

// declare/typeset [-A|-a|-i|-r|-g] [NAME|NAME=value]... `-x` isn't
// tracked separately since every variable already lives in the
// process env here; other real bash flags (-u/-l/-n/...) are
// accepted but not enforced. `-p`/`-f`/`-F` are a different mode
// entirely (print instead of declare, see print_declared/
// print_functions) -- checked first, same as bash effectively
// treating them as a separate subcommand.
pub(crate) fn run_declare(sh: &mut Shell, args: &[String], array_literals: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)]) -> i32 {
    if args.iter().any(|a| a == "-f" || a == "-F") {
        let names_only = args.iter().any(|a| a == "-F");
        let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
        return sh.print_functions(&names, names_only);
    }
    if args.iter().any(|a| a == "-p") {
        let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
        return sh.print_declared(&names);
    }
    // `-g`: force the write to the true global scope even when
    // called from inside a function -- without it, a plain
    // declare/typeset inside a function auto-localizes exactly like
    // `local` does (see the scalar assignment branch below), matching
    // real bash (confirmed: `f() { declare z=5; }; f; echo "$z"`
    // prints nothing in bash, but bish used to leak z to the global
    // scope here before this fix).
    let mut global_flag = false;
    let mut array_mode: Option<bool> = None; // Some(true)=-A, Some(false)=-a
    let mut readonly_flag = false;
    let mut integer_flag = false;
    let mut nameref_flag = false;
    let mut upper_flag = false;
    let mut lower_flag = false;
    let mut export_flag = false;
    let mut status = 0;
    for (i, a) in args.iter().enumerate() {
        match a.as_str() {
            "-A" => {
                array_mode = Some(true);
                continue;
            }
            "-a" => {
                array_mode = Some(false);
                continue;
            }
            "-r" => {
                readonly_flag = true;
                continue;
            }
            "-i" => {
                integer_flag = true;
                continue;
            }
            "-n" => {
                nameref_flag = true;
                continue;
            }
            "-u" => {
                upper_flag = true;
                continue;
            }
            "-l" => {
                lower_flag = true;
                continue;
            }
            "-x" => {
                export_flag = true;
                continue;
            }
            "-g" => {
                global_flag = true;
                continue;
            }
            _ => {}
        }
        // `declare -A m=([a]=1 [b]=2)` -- this position is actually
        // an array literal, not a plain `NAME`/`NAME=value` string
        // (`a` here is just its xtrace-only display text, see
        // array_literal_display's own doc comment). `-A`/`-a` seen
        // so far decides which table it's declared into, matching
        // the plain-name case just below; no flag at all falls back
        // to whatever `name` already is (bash's own behavior:
        // without `-A`, a bracketed key is just an arithmetic index
        // into a plain indexed array).
        if let Some((_, name, mode, items)) = array_literals.iter().find(|(pos, ..)| *pos == i) {
            match array_mode {
                Some(true) => {
                    sh.assoc_names.insert(name.clone());
                    sh.assoc_arrays.entry(name.clone()).or_default();
                }
                Some(false) => {
                    sh.arrays.entry(name.clone()).or_default();
                }
                None => {}
            }
            sh.apply_array_literal(name, *mode, items);
            if readonly_flag {
                sh.readonly_names.insert(name.clone());
            }
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        let (name, val) = match a.find('=') {
            Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
            None => (a.clone(), None),
        };
        if integer_flag {
            sh.integer_names.insert(name.clone());
        }
        if upper_flag {
            sh.upper_names.insert(name.clone());
        }
        if lower_flag {
            sh.lower_names.insert(name.clone());
        }
        if export_flag {
            sh.exported_names.insert(name.clone());
        }
        if nameref_flag {
            sh.nameref_names.insert(name.clone());
            if let Some(v) = val {
                sh.raw_var_write(&name, v);
            }
            if readonly_flag {
                sh.readonly_names.insert(name);
            }
            continue;
        }
        match array_mode {
            Some(true) => {
                sh.assoc_names.insert(name.clone());
                sh.assoc_arrays.entry(name.clone()).or_default();
            }
            Some(false) => {
                sh.arrays.entry(name.clone()).or_default();
            }
            None => {
                // Auto-localize, matching `local`: a plain (non-`-g`)
                // declare/typeset inside a function creates a new
                // local shadow rather than falling through to the
                // global env. Pre-inserting an (empty, for now) entry
                // into the current scope makes assign_var's own
                // existing "write into whichever scope already
                // shadows this name" logic (raw_var_write) do the
                // right thing without needing a separate write path.
                if !global_flag && !sh.var_scopes.is_empty() {
                    sh.var_scopes.last_mut().unwrap().entry(name.clone()).or_default();
                }
                if let Some(v) = val {
                    // A readonly name refuses the write, and `declare`
                    // owes the shell that failure -- same as a bare
                    // `x=2` command does.
                    if !if global_flag { sh.assign_var_global(&name, v) } else { sh.assign_var(&name, v) } {
                        status = 1;
                    }
                } else if export_flag {
                    // Bare `declare -x NAME`/`export NAME` on an
                    // already-set variable (commonly a local: `local
                    // Z=inner; export Z`) -- re-assign its current
                    // value through assign_var so exported_names'
                    // mirror-to-env logic fires immediately, instead
                    // of only on the variable's *next* write. The
                    // empty-fallback branch below wouldn't reach this
                    // case since it only fires for a name with no
                    // value at all yet.
                    let cur = sh.lookup_var(&name);
                    if global_flag { sh.assign_var_global(&name, cur) } else { sh.assign_var(&name, cur) };
                } else if sh.lookup_var(&name).is_empty() && std::env::var(&name).is_err() {
                    if global_flag {
                        sh.assign_var_global(&name, String::new());
                    } else {
                        sh.assign_var(&name, String::new());
                    }
                }
            }
        }
        if readonly_flag {
            sh.readonly_names.insert(name);
        }
    }
    status
}

