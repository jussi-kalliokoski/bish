// `compgen`, `complete` and `compopt`: programmable completion, the part
// that lives in the shell rather than the line editor.

use crate::compgen;
use crate::exec::{Shell, sh_eprintln, sh_println};

// `compgen [-V varname] [-abcdefgjksuv] [-o option] [-A action]
// [-G globpat] [-W wordlist] [-F function] [-C command] [-X filterpat]
// [-P prefix] [-S suffix] [--] [word]`, on compgen.rs's shared spec
// parser and resolver (see there for which sources `word` filters, the
// `-F`/`-C` calling convention, and the inert `-o` names).
//
// `-V` is compgen's own, stripped before `compgen::parse_spec_args`.
//
// Succeeds unless a source was requested and produced nothing: bare
// `compgen` exits 0, `compgen -W "" -- x` exits 1, as in bash, with or
// without `-V`.
pub(crate) fn run_compgen(sh: &mut Shell, args: &[String]) -> i32 {
    let mut varname: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < args.len() {
        if args[idx] == "-V" {
            let Some(v) = args.get(idx + 1) else {
                sh_eprintln!(sh, "bish: compgen: -V: option requires an argument");
                return 2;
            };
            varname = Some(v.clone());
            idx += 2;
        } else {
            rest.push(args[idx].clone());
            idx += 1;
        }
    }
    let (spec, positionals) = match compgen::parse_spec_args(&rest) {
        Ok(v) => v,
        Err(e) => return sh.report_compgen_parse_error("compgen", &e),
    };
    let word = positionals.last().cloned().unwrap_or_default();
    let had_source = spec.has_any_source();
    // A specific error for a misspelled function, as bash gives ("function
    // not found", status 1). `compgen::run_external` stays quiet on failure
    // because Tab completion shouldn't be interrupted; this builtin should
    // say what went wrong.
    if let Some(name) = &spec.function
        && !sh.functions.contains_key(name)
    {
        sh_eprintln!(sh, "bish: compgen: {name}: function not found");
        return 1;
    }
    let ctx = sh.action_context();
    let preamble = sh.functions_preamble();
    let candidates = compgen::resolve_spec(&spec, &word, &ctx, &sh.cwd, &preamble);

    let empty = candidates.is_empty();
    if let Some(var) = varname {
        sh.assoc_names.remove(&var);
        sh.array_names.insert(var.clone());
        sh.arrays.insert(var, crate::sort::btree_map(candidates.into_iter().enumerate()));
    } else {
        for c in &candidates {
            sh_println!(sh, "{c}");
        }
    }
    if empty && had_source { 1 } else { 0 }
}

// `complete [-p|-r] [options] name...` and `complete -D [options] --`:
// registers, lists or removes the specs Tab completion consults (see
// `ShellCompletionProvider`), or the `-D` default spec. Shares compgen's
// option grammar (`compgen::parse_spec_args`); `-p`, `-r`, `-D` and the
// trailing NAMEs are its own.
//
// `-p`/`-r` come first and take every other argument as a NAME; with none,
// they cover every spec. The name `-D` means the default spec, as in bash.
// Registering replaces a name's spec entirely, never merging.
pub(crate) fn run_complete(sh: &mut Shell, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None => {
            sh.print_all_completions();
            return 0;
        }
        Some("-p") => return sh.print_completions(&args[1..]),
        Some("-r") => return sh.remove_completions(&args[1..]),
        _ => {}
    }
    let is_default = args.iter().any(|a| a == "-D");
    let filtered: Vec<String> = args.iter().filter(|a| *a != "-D").cloned().collect();
    let (spec, names) = match compgen::parse_spec_args(&filtered) {
        Ok(v) => v,
        Err(e) => return sh.report_compgen_parse_error("complete", &e),
    };
    if is_default {
        sh.default_completion = Some(spec);
        return 0;
    }
    if names.is_empty() {
        sh_eprintln!(sh, "bish: complete: usage: complete [-p|-r] [name ...] | complete -D [options] | complete [options] name [name ...]");
        return 2;
    }
    for name in names {
        sh.completions.insert(name, spec.clone());
    }
    0
}

// `compopt [-o option] [+o option] [name]`: adds or removes `-o` options
// on a registered spec. Without `name`, bash adjusts the running
// completion function's options; bish never runs one that way, so this
// errors as bash does outside one ("not currently executing completion
// function").
pub(crate) fn run_compopt(sh: &mut Shell, args: &[String]) -> i32 {
    let mut adds: Vec<String> = Vec::new();
    let mut removes: Vec<String> = Vec::new();
    let mut name: Option<&str> = None;
    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-o" => {
                let Some(v) = args.get(idx + 1) else {
                    sh_eprintln!(sh, "bish: compopt: -o: option requires an argument");
                    return 2;
                };
                if !compgen::O_OPTIONS.contains(&v.as_str()) {
                    sh_eprintln!(sh, "bish: compopt: {v}: invalid option name");
                    return 2;
                }
                adds.push(v.clone());
                idx += 2;
            }
            "+o" => {
                let Some(v) = args.get(idx + 1) else {
                    sh_eprintln!(sh, "bish: compopt: +o: option requires an argument");
                    return 2;
                };
                removes.push(v.clone());
                idx += 2;
            }
            other => {
                name = Some(other);
                idx += 1;
            }
        }
    }
    let Some(name) = name else {
        sh_eprintln!(sh, "bish: compopt: not currently executing completion function");
        return 1;
    };
    let Some(spec) = sh.completions.get_mut(name) else {
        sh_eprintln!(sh, "bish: compopt: {name}: no completion specification");
        return 1;
    };
    for o in adds {
        if !spec.opts.contains(&o) {
            spec.opts.push(o);
        }
    }
    spec.opts.retain(|o| !removes.contains(o));
    0
}
