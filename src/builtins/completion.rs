// `compgen`, `complete` and `compopt`: programmable completion, the
// half that lives in the shell rather than in the line editor.
//
// Free functions taking `&mut Shell` -- see `builtins/mod.rs`.

use crate::compgen;
use crate::exec::{sh_eprintln, sh_println, Shell};

// compgen [-V varname] [-abcdefgjksuv] [-o option] [-A action]
// [-G globpat] [-W wordlist] [-F function] [-C command] [-X filterpat]
// [-P prefix] [-S suffix] [--] [word] -- bash's own completion-
// generator builtin, built on compgen.rs's shared spec parser/resolver
// (see that module's own doc comment for the design and every
// reverse-engineered semantic detail: which sources get filtered by
// `word` and which don't, the exact -F/-C calling convention, `-o`'s
// names being validated but otherwise inert).
//
// `-V varname` is compgen's own option (not part of the shared spec
// grammar, since `complete` has no equivalent) -- stripped out before
// handing the rest to compgen::parse_spec_args.
//
// Exit status is success unless a source was actually requested and
// it produced nothing
// (confirmed against real bash: bare `compgen`/a lone trailing word
// with no -A/-G/-W/-F/-C at all always exits 0 even though it prints
// nothing, but e.g. `compgen -W "" -- x` -- a real source, zero
// matches -- exits 1). Applies the same way whether or not -V
// redirected the output into an array.
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
    // A nicer, specific diagnostic for the overwhelmingly common
    // mistake (a typo'd function name) than the silent "just no
    // candidates" compgen::run_external falls back to for any
    // subprocess failure -- that tolerance exists for the interactive
    // Tab-completion path, where a hard error would be disruptive, but
    // this standalone builtin can and should still say what went
    // wrong (confirmed against real bash: `compgen -F nosuchfunc`
    // prints "function not found" and exits 1).
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
        sh.arrays.insert(var, candidates.into_iter().enumerate().collect());
    } else {
        for c in &candidates {
            sh_println!(sh, "{c}");
        }
    }
    if empty && had_source {
        1
    } else {
        0
    }
}

// complete [-p|-r] [options] name... | complete -D [options] --
// registers/lists/removes the completion specs bish's own interactive
// Tab completion consults (see ShellCompletionProvider's own doc
// comment on how) for a given command name, or the `-D` default spec
// used when no exact name matches. Shares its entire option grammar
// with `compgen` (compgen.rs's parse_spec_args) -- only `-p`/`-r`/`-D`
// themselves, and taking one-or-more trailing NAMEs instead of a
// single trailing word, are complete's own.
//
// `-p`/`-r` are detected as the very first argument (confirmed against
// real bash: always used alone, never mixed with the rest of the
// option grammar in practice) and take every remaining argument as a
// literal NAME to print/remove -- no names at all means "every
// registered spec" for both. The literal name "-D" in either list
// targets the default spec instead of a real command name (confirmed:
// `complete -p -D` prints the default spec's own line).
//
// Registration always fully replaces whatever spec a name already had
// (confirmed: re-registering `cmd1` with just `-W x` drops its
// previous -X/-P/-S/-o entirely) -- a plain HashMap::insert overwrite,
// never a merge.
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

// compopt [-o option] [+o option] [name] -- adjusts a registered
// spec's own stored `-o` list in place (add for `-o`, remove for
// `+o`), leaving every other field untouched. Real bash's own compopt
// with no `name` at all only makes sense called from inside an
// in-progress completion function (adjusting *that* completion's
// options); bish's completion generation never calls back into a
// running compopt invocation that way, so -- matching real bash's own
// behavior outside that context -- this always errors when no name is
// given (confirmed: `compopt -o nospace` outside a completion function
// prints "not currently executing completion function" and exits 1).
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
