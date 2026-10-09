//! `jq --run-tests [file...]`: jq's own test format (jq_test.c): a program, an input, the
//! expected outputs, then a blank line; `%%FAIL` (or `%%FAIL IGNORE MSG`) before a program
//! that must not compile, followed by the expected error message.

use crate::interp::{Flow, Interp, Pv};
use crate::modules;
use crate::value::{dump, Fmt, Value};
use std::io::BufRead;

fn skipline(l: &str) -> bool {
    let t = l.trim_start_matches([' ', '\t']);
    t.is_empty() || t.starts_with('#') || t == "\n"
}

fn compact(v: &Value) -> String {
    dump(v, &Fmt::compact())
}

fn run_tests(it: &mut Interp, lib_dirs: &[String], lines: Vec<String>, skip: i64, take: i64) -> bool {
    let (mut tests, mut passed, mut invalid) = (0usize, 0usize, 0usize);
    let mut skip = skip;
    let mut take = take;
    let tests_to_skip = skip.max(0);
    let tests_to_take = take;
    let mut k = 0usize;
    let mut lineno = 0usize;
    let mut must_fail = false;
    let mut check_msg = false;
    let next = |k: &mut usize, lineno: &mut usize| -> Option<String> {
        let l = lines.get(*k)?.clone();
        *k += 1;
        *lineno += 1;
        Some(l)
    };
    let opts = modules::Options { lib_dirs: lib_dirs.to_vec(), jq_origin: String::new(), prog_origin: ".".into(), globals: vec![] };
    'outer: while let Some(prog_line) = next(&mut k, &mut lineno) {
        if skipline(&prog_line) {
            continue;
        }
        if prog_line == "%%FAIL" || prog_line == "%%FAIL IGNORE MSG" {
            must_fail = true;
            check_msg = prog_line == "%%FAIL";
            continue;
        }
        let prog = prog_line.clone();
        let mut fail = false;
        if skip > 0 {
            skip -= 1;
            // to the end of the test
            while let Some(l) = next(&mut k, &mut lineno) {
                if skipline(&l) {
                    break;
                }
            }
            must_fail = false;
            check_msg = false;
            continue;
        } else if skip == 0 {
            println!("Skipped {tests_to_skip} tests");
            skip = -1;
        }
        if take > 0 {
            take -= 1;
        } else if take == 0 {
            println!("Hit the number of tests limit ({tests_to_take}), breaking");
            break;
        }
        let mut pass = true;
        tests += 1;
        println!("Test #{}: '{}' at line number {}", tests as i64 + tests_to_skip, prog, lineno);
        let compiled = modules::compile(it, &prog, "<top-level>", &opts);
        if must_fail {
            let errs = match compiled {
                Ok(_) => {
                    println!("*** Test program compiled successfully, but should fail at line number {lineno}: {prog}");
                    fail = true;
                    vec![]
                }
                Err(e) => e,
            };
            if !fail {
                // The last error message, as jq's test callback keeps it.
                let mut err_buf: String = errs.last().cloned().unwrap_or_default();
                while let Some(l) = next(&mut k, &mut lineno) {
                    if skipline(&l) {
                        break;
                    }
                    if check_msg {
                        if !err_buf.starts_with(&l) {
                            let first = err_buf.split('\n').next().unwrap_or("").to_string();
                            println!("*** Erroneous program failed with '{first}', but expected '{l}' at line number {lineno}: {prog}");
                            fail = true;
                            break;
                        }
                        err_buf = err_buf[l.len()..].to_string();
                        if err_buf.starts_with('\n') {
                            err_buf.remove(0);
                        }
                    }
                }
                if !fail {
                    if check_msg && !err_buf.is_empty() {
                        let first = err_buf.split('\n').next().unwrap_or("").to_string();
                        println!("*** Erroneous program failed with extra message '{first}' at line {lineno}: {prog}");
                        invalid += 1;
                        pass = false;
                    }
                    must_fail = false;
                    check_msg = false;
                    if pass {
                        passed += 1;
                    }
                    continue;
                }
            }
        } else {
            match compiled {
                Err(errs) => {
                    for e in errs {
                        eprintln!("{e}");
                    }
                    println!("*** Test program failed to compile at line {lineno}: {prog}");
                    fail = true;
                }
                Ok(c) => {
                    let Some(input_line) = next(&mut k, &mut lineno) else {
                        invalid += 1;
                        break 'outer;
                    };
                    let input = match crate::json::parse_one(&input_line) {
                        Ok(v) => v,
                        Err(_) => {
                            println!("*** Input is invalid on line {lineno}: {input_line}");
                            fail = true;
                            Value::Null
                        }
                    };
                    if !fail {
                        let mut expected = vec![];
                        let mut bad = false;
                        while let Some(l) = next(&mut k, &mut lineno) {
                            if skipline(&l) {
                                break;
                            }
                            match crate::json::parse_one(&l) {
                                Ok(v) => expected.push((v, lineno)),
                                Err(_) => {
                                    println!("*** Expected result is invalid on line {lineno}: {l}");
                                    bad = true;
                                    break;
                                }
                            }
                        }
                        if bad {
                            fail = true;
                        } else {
                            // At most one more output than expected (jq pulls them one by one).
                            let want = expected.len() + 1;
                            let mut actual: Vec<Value> = vec![];
                            let r = it.eval(c.main, &c.env, Pv::val(input), false, &mut |_, pv| {
                                actual.push(pv.v);
                                if actual.len() >= want {
                                    // Not a Break: `?//` would catch that (as jq's does a break).
                                    return Err(Flow::Halt(i32::MIN, None));
                                }
                                Ok(())
                            });
                            let r = match r {
                                Err(Flow::Outer(e)) => Err(*e),
                                r => r,
                            };
                            if let Err(Flow::Err(e)) = r {
                                let m = match &e {
                                    Value::Str(s) => s.to_string(),
                                    other => compact(other),
                                };
                                eprintln!("jq: error (at <unknown>): {m}");
                            }
                            let mut ai = actual.into_iter();
                            let mut last_line = lineno;
                            for (exp, ln) in &expected {
                                last_line = *ln;
                                match ai.next() {
                                    None => {
                                        println!("*** Insufficient results for test at line number {ln}: {prog}");
                                        pass = false;
                                        break;
                                    }
                                    Some(a) => {
                                        if a != *exp {
                                            println!("*** Expected {}, but got {} for test at line number {}: {}", compact(exp), compact(&a), ln, prog);
                                            pass = false;
                                        }
                                    }
                                }
                            }
                            if pass {
                                if let Some(extra) = ai.next() {
                                    println!("*** Superfluous result: {} for test at line number {}, {}", compact(&extra), last_line, prog);
                                    invalid += 1;
                                    pass = false;
                                }
                            }
                            if pass {
                                passed += 1;
                            }
                            continue;
                        }
                    }
                }
            }
        }
        if fail {
            invalid += 1;
            while let Some(l) = next(&mut k, &mut lineno) {
                if skipline(&l) {
                    break;
                }
            }
            must_fail = false;
            check_msg = false;
        }
    }
    let total_skipped = if skip > 0 { tests_to_skip - skip } else { tests_to_skip };
    println!("{passed} of {tests} tests passed ({invalid} malformed, {total_skipped} skipped)");
    if skip > 0 {
        println!("WARN: skipped past the end of file, exiting with status 2");
        std::process::exit(2);
    }
    passed == tests
}

pub fn testsuite(lib_dirs: Vec<String>, args: &[String]) -> i32 {
    let mut skip: i64 = -1;
    let mut take: i64 = -1;
    let mut nfiles = 0;
    let mut it = Interp::new();
    it.env_value = {
        let mut m = crate::value::Map::new();
        for (k, v) in std::env::vars() {
            m.insert(std::rc::Rc::from(k.as_str()), Value::string(v));
        }
        Value::obj(m)
    };
    it.search_list = lib_dirs.clone();
    let mut ok = true;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--skip" => {
                i += 1;
                let Some(n) = args.get(i) else {
                    eprintln!("--skip requires an argument");
                    std::process::exit(1);
                };
                skip = n.parse().unwrap_or(0);
            }
            "--take" => {
                i += 1;
                let Some(n) = args.get(i) else {
                    eprintln!("--take requires an argument");
                    std::process::exit(1);
                };
                take = n.parse().unwrap_or(0);
            }
            f => {
                let file = match std::fs::File::open(f) {
                    Ok(file) => file,
                    Err(e) => {
                        eprintln!("fopen: {}", crate::io_error_text(&e));
                        std::process::exit(1);
                    }
                };
                let lines: Vec<String> = std::io::BufReader::new(file).lines().map_while(Result::ok).collect();
                ok &= run_tests(&mut it, &lib_dirs, lines, skip, take);
                if !ok {
                    return 1;
                }
                nfiles += 1;
            }
        }
        i += 1;
    }
    if nfiles == 0 {
        let lines: Vec<String> = std::io::stdin().lock().lines().map_while(Result::ok).collect();
        ok &= run_tests(&mut it, &lib_dirs, lines, skip, take);
    }
    if ok {
        0
    } else {
        1
    }
}
