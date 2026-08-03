pub fn cd(args: &[String]) -> i32 {
    let target = if let Some(dir) = args.first() {
        dir.clone()
    } else {
        match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => {
                eprintln!("cd: HOME not set");
                return 1;
            }
        }
    };
    match std::env::set_current_dir(&target) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cd: {}: {}", target, e);
            1
        }
    }
}

// Variables are always process env vars in v1 (no local-vs-exported
// distinction yet), so `export NAME` with no '=' is a no-op.
pub fn export(args: &[String]) -> i32 {
    for a in args {
        if let Some(eq) = a.find('=') {
            unsafe {
                std::env::set_var(&a[..eq], &a[eq + 1..]);
            }
        }
    }
    0
}
