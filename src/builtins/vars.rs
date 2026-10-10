// `declare`, `readonly` and `unset`: the builtins that make, mark and
// unmake variables.

use crate::arith;
use crate::exec::{Shell, sh_eprintln, write_diagnostic};
use crate::parser::ArrayLiteralItem;
use crate::parser::AssignMode;

// `unset [-f|-v] NAME...`, and `arr[i]` to remove one element. Errors go
// through the command's own `2>` (`stderr_target`), as in bash, unlike
// nounset and assignment errors, which come before redirects are set up.
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
        sh.array_names.remove(n.as_str());
        sh.assoc_names.remove(n.as_str());
        sh.remove_var(n);
        if !only_vars {
            sh.functions.remove(n.as_str());
        }
    }
    status
}

// What `declare`, `export` and `readonly` accept as a name: an identifier,
// optionally with a `[subscript]`. Anything else is "not a valid
// identifier", as in bash.
fn is_valid_declare_target(word: &str) -> bool {
    let name = word.split_once('=').map(|(n, _)| n).unwrap_or(word);
    let name = match name.split_once('[') {
        Some((base, rest)) => {
            if !rest.ends_with(']') || rest.len() < 2 {
                return false;
            }
            base
        }
        None => name,
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// The flag letters in one argument, if it's one: `declare -ir n=1`
// carries two. Every flag here is a single letter without an argument of
// its own, so the whole argument can be read at once. `--` ends the
// options; a lone `-` is an operand.
fn flag_letters(arg: &str) -> Option<impl Iterator<Item = char> + '_> {
    match arg.len() > 1 && arg.starts_with('-') && arg != "--" {
        true => Some(arg.chars().skip(1)),
        false => None,
    }
}

fn has_flag(args: &[String], want: char) -> bool {
    args.iter().take_while(|a| *a != "--").filter_map(|a| flag_letters(a)).any(|mut cs| cs.any(|c| c == want))
}

// `declare`/`typeset`, `export` and `readonly` (`who`), with the flags
// `-A -a -i -r -g -x`; bash's other letters are accepted and ignored.
// `-p`, `-f` and `-F` print rather than declare (see `print_declared`,
// `print_functions`) and are checked first.
pub(crate) fn run_declare(sh: &mut Shell, who: &str, args: &[String], array_literals: &[(usize, String, AssignMode, Vec<ArrayLiteralItem>)]) -> i32 {
    if has_flag(args, 'f') || has_flag(args, 'F') {
        let names_only = has_flag(args, 'F');
        let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
        return sh.print_functions(&names, names_only);
    }
    if has_flag(args, 'p') {
        let names: Vec<String> = args.iter().filter(|a| !a.starts_with('-')).cloned().collect();
        return sh.print_declared(&names);
    }
    // `-g` writes to the global scope even inside a function; without it,
    // `declare`/`typeset` in a function makes a local, like `local` (see the
    // scalar branch below), as in bash: `f() { declare z=5; }; f; echo "$z"`
    // prints nothing.
    //
    // `export` is always global: in bash `f() { export E=v; }; f` leaves `E`
    // set and exported in the caller.
    let mut global_flag = who == "export";
    let mut array_mode: Option<bool> = None; // Some(true)=-A, Some(false)=-a
    let mut readonly_flag = false;
    let mut integer_flag = false;
    let mut nameref_flag = false;
    let mut upper_flag = false;
    let mut lower_flag = false;
    let mut export_flag = false;
    let mut status = 0;
    // After `--` a word is an operand however it starts, so
    // `declare -x -- -bad=1` reaches "not a valid identifier".
    let mut after_dashdash = false;
    for (i, a) in args.iter().enumerate() {
        if a == "--" {
            after_dashdash = true;
            continue;
        }
        if !after_dashdash && let Some(letters) = flag_letters(a) {
            for c in letters {
                match c {
                    'A' => array_mode = Some(true),
                    'a' => array_mode = Some(false),
                    'r' => readonly_flag = true,
                    'i' => integer_flag = true,
                    'n' => nameref_flag = true,
                    'u' => upper_flag = true,
                    'l' => lower_flag = true,
                    'x' => export_flag = true,
                    'g' => global_flag = true,
                    // Every other letter is accepted and does nothing.
                    _ => {}
                }
            }
            continue;
        }
        // `declare -A m=([a]=1 [b]=2)`: an array literal at this position
        // (`a` is only its xtrace text; see `array_literal_display`). `-A`
        // or `-a` picks the table, as for a plain name; without either, the
        // name's current kind decides, as in bash, where a bracketed key
        // without `-A` is an arithmetic index.
        if let Some((_, name, mode, items)) = array_literals.iter().find(|(pos, ..)| *pos == i) {
            match array_mode {
                Some(true) => {
                    sh.assoc_names.insert(name.clone());
                }
                Some(false) => {
                    sh.array_names.insert(name.clone());
                }
                None => {}
            }
            sh.apply_array_literal(name, *mode, items);
            // The other attributes apply to arrays too: `declare -ax a=(1)`
            // is exported as well as indexed.
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
                std::rc::Rc::make_mut(&mut sh.exported_names).insert(name.clone());
            }
            if readonly_flag {
                sh.readonly_names.insert(name.clone());
            }
            continue;
        }
        if a.starts_with('-') && !after_dashdash {
            continue;
        }
        if !is_valid_declare_target(a) {
            sh_eprintln!(sh, "bish: {}: `{}': not a valid identifier", who, a);
            status = 1;
            continue;
        }
        let (name, val) = match a.find('=') {
            Some(eq) => (a[..eq].to_string(), Some(a[eq + 1..].to_string())),
            None => (a.clone(), None),
        };
        // `declare 'a[0]=5'` names an element.
        if let Some((base, rest)) = name.split_once('[') {
            let index = rest.trim_end_matches(']').to_string();
            let base = base.to_string();
            if let Some(v) = val {
                sh.array_set_index_public(&base, &index, v);
            }
            if readonly_flag {
                sh.readonly_names.insert(base);
            }
            continue;
        }
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
            std::rc::Rc::make_mut(&mut sh.exported_names).insert(name.clone());
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
            }
            Some(false) => {
                sh.array_names.insert(name.clone());
            }
            None => {
                // Make a local, as `local` does: an empty entry in the
                // current scope makes `assign_var` (via `raw_var_write`)
                // write into it.
                if !global_flag && !sh.var_scopes.is_empty() {
                    sh.var_scopes.last_mut().unwrap().entry(name.clone()).or_default();
                }
                if let Some(v) = val {
                    // A readonly name refuses the write, and `declare`
                    // fails, as `x=2` would.
                    if !if global_flag { sh.assign_var_global(&name, v) } else { sh.assign_var(&name, v) } {
                        status = 1;
                    }
                } else if export_flag {
                    // `declare -x NAME`/`export NAME` on a set variable
                    // (often a local: `local Z=inner; export Z`) reassigns
                    // its value so it's exported now, not on its next
                    // write. Only when set: `lookup_var` gives "" for an
                    // unset name, and writing that back would create it,
                    // where bash marks the attribute and exports nothing.
                    if sh.var_is_set(&name) {
                        let cur = sh.lookup_var(&name);
                        if global_flag {
                            sh.assign_var_global(&name, cur)
                        } else {
                            sh.assign_var(&name, cur)
                        };
                    }
                }
                // No `else`: `declare x` without a value records attributes
                // and leaves the name unset, as bash's
                // `declare z; echo "${z-unset}"` shows.
            }
        }
        if readonly_flag {
            sh.readonly_names.insert(name);
        }
    }
    status
}
