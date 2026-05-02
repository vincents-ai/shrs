// Lot of code based off of https://github.com/nuta/nsh/blob/main/src/eval.rs

use std::os::unix::io::FromRawFd;
use std::process::{Command, Stdio};

use std::cell::RefCell;
use std::collections::HashMap;

use glob::glob;
use shrs_job::{run_external_command, JobManager, Output, Process, ProcessGroup, Stdin};

use crate::{ast, Lexer, Parser, PosixError};

thread_local! {
    static FUNCTIONS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

pub fn eval(job_manager: &mut JobManager, parser: Parser, lexer: Lexer) -> Result<(), PosixError> {
    let parsed = match parser.parse(lexer) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("parse error: {e}");
            return Err(PosixError::Parse(e));
        },
    };

    let (procs, pgid) = match eval_command(job_manager, &parsed, None, None) {
        Ok((procs, pgid)) => (procs, pgid),
        Err(PosixError::CommandNotFound(cmd)) => {
            if !cmd.is_empty() {
                eprintln!("__notfound__: {cmd}");
            }
            return Err(PosixError::CommandNotFound(cmd));
        },
        Err(e) => return Err(e),
    };

    // Only run if there are actual processes to execute
    if !procs.is_empty() {
        run_job(job_manager, procs, pgid, true)?;
    }
    Ok(())
}

/// Evaluate a raw command string (used for function invocation and command substitution).
fn eval_string(job_manager: &mut JobManager, input: &str) -> Result<(), PosixError> {
    let lexer = Lexer::new(input);
    let parser = Parser::default();
    let parsed = match parser.parse(lexer) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("parse error: {e}");
            return Err(PosixError::Parse(e));
        },
    };
    let (procs, pgid) = eval_command(job_manager, &parsed, None, None)?;
    if !procs.is_empty() {
        run_job(job_manager, procs, pgid, true)?;
    }
    Ok(())
}
    let parsed = match parser.parse(lexer) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("parse error: {e}");
            return Err(PosixError::Parse(e));
        },
    };

    let (procs, pgid) = match eval_command(job_manager, &parsed, None, None) {
        Ok((procs, pgid)) => (procs, pgid),
        Err(PosixError::CommandNotFound(cmd)) => {
            eprintln!("__notfound__: {cmd}");
            return Err(PosixError::CommandNotFound(cmd));
        },
        Err(e) => return Err(e),
    };

    // Only run if there are actual processes to execute
    if !procs.is_empty() {
        run_job(job_manager, procs, pgid, true)?;
    }
    Ok(())
}

fn run_job(
    job_manager: &mut JobManager,
    procs: Vec<Box<dyn Process>>,
    pgid: Option<u32>,
    foreground: bool,
) -> Result<(), PosixError> {
    let proc_group = ProcessGroup {
        id: pgid,
        processes: procs,
        foreground,
    };

    let job_id = job_manager.create_job("", proc_group);

    if foreground {
        job_manager
            .put_job_in_foreground(Some(job_id), false)
            .map_err(|e| PosixError::Job(e))?;
    } else {
        job_manager
            .put_job_in_background(Some(job_id), false)
            .map_err(|e| PosixError::Job(e))?;
    }
    Ok(())
}

/// Expand a single argument string, handling variable references, command substitution,
/// arithmetic expansion, tilde expansion, quoting, and globbing.
fn expand_arg(arg: &str) -> Vec<String> {
    // First, perform all $ expansions in the string
    let expanded = expand_variables(arg);

    // Handle quoted strings — no glob expansion, strip quotes
    let first = arg.chars().next().unwrap_or(' ');
    if first == '\'' {
        // Single quotes: literal, no expansion happened needed but strip quotes
        let inner = arg.trim_matches('\'');
        return vec![inner.to_string()];
    }
    if first == '"' {
        // Double quotes: variables already expanded above, just strip quotes
        let inner = expanded.trim_matches('"');
        return vec![inner.to_string()];
    }

    // Tilde expansion
    let expanded = if let Some(remaining) = expanded.strip_prefix('~') {
        format!(
            "{}{}",
            dirs::home_dir().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(),
            remaining
        )
    } else {
        expanded
    };

    // Glob expansion — only if the string actually contains glob characters
    if glob::Pattern::escape(&expanded) != expanded {
        if let Ok(files) = glob(&expanded) {
            let results: Vec<String> = files
                .filter_map(|f| f.ok())
                .map(|f| f.to_string_lossy().to_string())
                .collect();
            if !results.is_empty() {
                return results;
            }
        }
    }

    vec![expanded]
}

/// Expand $VAR, ${VAR}, $(cmd), and $((expr)) in a string.
fn expand_variables(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '$' {
            if i + 1 >= len {
                result.push('$');
                i += 1;
                continue;
            }

            // $((expr)) — arithmetic expansion
            if i + 2 < len && chars[i + 1] == '(' && chars[i + 2] == '(' {
                let start = i + 3;
                if let Some(end) = find_closing(&chars, start, "((", "))") {
                    let expr: String = chars[start..end].iter().collect();
                    let val = eval_arithmetic(&expr);
                    result.push_str(&val);
                    i = end + 2; // skip closing ))
                    continue;
                }
            }

            // $(cmd) — command substitution
            if chars[i + 1] == '(' {
                let start = i + 2;
                if let Some(end) = find_closing_paren(&chars, start) {
                    let cmd: String = chars[start..end].iter().collect();
                    let val = eval_command_substitution(&cmd);
                    result.push_str(&val);
                    i = end + 1; // skip closing )
                    continue;
                }
            }

            // ${VAR} — braced variable reference
            if chars[i + 1] == '{' {
                let start = i + 2;
                if let Some(end) = chars[start..].iter().position(|&c| c == '}') {
                    let var_name: String = chars[start..start + end].iter().collect();
                    let val = std::env::var(&var_name).unwrap_or_default();
                    result.push_str(&val);
                    i = start + end + 1; // skip closing }
                    continue;
                }
            }

            // $VAR — simple variable reference (alphanumeric + underscore)
            if chars[i + 1] == '?' {
                // $? — last exit status (simplified: always 0 for now)
                result.push('0');
                i += 2;
                continue;
            }

            let start = i + 1;
            let mut end = start;
            while end < len && (chars[end].is_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            if end > start {
                let var_name: String = chars[start..end].iter().collect();
                let val = std::env::var(&var_name).unwrap_or_default();
                result.push_str(&val);
                i = end;
                continue;
            }

            // Lone $ — keep as-is
            result.push('$');
            i += 1;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }

    result
}

/// Find closing delimiter in char slice starting from `start`.
fn find_closing(chars: &[char], start: usize, _open: &str, close: &str) -> Option<usize> {
    let close_chars: Vec<char> = close.chars().collect();
    let clen = close_chars.len();
    for i in start..chars.len().saturating_sub(clen - 1) {
        if chars[i..i + clen] == close_chars[..] {
            return Some(i);
        }
    }
    None
}

/// Find matching closing parenthesis, respecting nesting.
fn find_closing_paren(chars: &[char], start: usize) -> Option<usize> {
    let mut depth = 1;
    let mut i = start;
    while i < chars.len() {
        if chars[i] == '(' {
            depth += 1;
        } else if chars[i] == ')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Evaluate a simple arithmetic expression (integers only, +, -, *, /, %, parentheses).
fn eval_arithmetic(expr: &str) -> String {
    let expr = expr.trim();
    // Simple recursive descent parser for arithmetic
    match arithmetic_expr(expr) {
        Ok((remaining, val)) if remaining.trim().is_empty() => val.to_string(),
        _ => {
            // Try to evaluate as a variable reference
            if let Ok(val) = std::env::var(expr) {
                return val;
            }
            "0".to_string()
        },
    }
}

/// Recursive descent arithmetic parser.
fn arithmetic_expr(input: &str) -> Result<(&str, i64), ()> {
    let (input, val) = parse_add_sub(input.trim())?;
    Ok((input, val))
}

fn parse_add_sub(input: &str) -> Result<(&str, i64), ()> {
    let (mut input, mut val) = parse_mul_div(input)?;
    loop {
        input = input.trim_start();
        if input.starts_with('+') && !input.starts_with("++") {
            let (rest, rhs) = parse_mul_div(&input[1..])?;
            val += rhs;
            input = rest;
        } else if input.starts_with('-') && !input.starts_with("--") {
            let (rest, rhs) = parse_mul_div(&input[1..])?;
            val -= rhs;
            input = rest;
        } else {
            break;
        }
    }
    Ok((input, val))
}

fn parse_mul_div(input: &str) -> Result<(&str, i64), ()> {
    let (mut input, mut val) = parse_unary(input)?;
    loop {
        input = input.trim_start();
        if input.starts_with('*') {
            let (rest, rhs) = parse_unary(&input[1..])?;
            val *= rhs;
            input = rest;
        } else if input.starts_with('/') {
            let (rest, rhs) = parse_unary(&input[1..])?;
            if rhs != 0 {
                val /= rhs;
            }
            input = rest;
        } else if input.starts_with('%') {
            let (rest, rhs) = parse_unary(&input[1..])?;
            if rhs != 0 {
                val %= rhs;
            }
            input = rest;
        } else {
            break;
        }
    }
    Ok((input, val))
}

fn parse_unary(input: &str) -> Result<(&str, i64), ()> {
    let input = input.trim_start();
    if input.starts_with('-') {
        let (rest, val) = parse_primary(&input[1..])?;
        Ok((rest, -val))
    } else if input.starts_with('+') {
        parse_primary(&input[1..])
    } else {
        parse_primary(input)
    }
}

fn parse_primary(input: &str) -> Result<(&str, i64), ()> {
    let input = input.trim_start();
    if input.starts_with('(') {
        let (rest, val) = parse_add_sub(&input[1..])?;
        let rest = rest.trim_start();
        if rest.starts_with(')') {
            Ok((&rest[1..], val))
        } else {
            Err(())
        }
    } else {
        // Parse number or variable
        let end = input
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(input.len());
        if end == 0 {
            return Err(());
        }
        let token = &input[..end];
        let val = if token.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            token.parse::<i64>().unwrap_or(0)
        } else {
            // Variable reference
            std::env::var(token)
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0)
        };
        Ok((&input[end..], val))
    }
}

/// Execute command substitution: $(cmd) — run cmd and capture stdout.
fn eval_command_substitution(cmd: &str) -> String {
    let output = Command::new("/proc/self/exe")
        .arg("--mode")
        .arg("admin")
        .arg("-c")
        .arg(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            // Trim trailing newlines (POSIX behavior)
            s.trim_end_matches('\n').trim_end_matches('\r').to_string()
        },
        Err(_) => String::new(),
    }
}

/// Process redirect operators, returning modified stdin/stdout.
fn process_redirects(
    redirects: &[ast::Redirect],
    default_stdin: Stdin,
    default_stdout: Output,
) -> (Stdin, Output) {
    use std::fs;

    let mut stdin = default_stdin;
    let mut stdout = default_stdout;

    for redirect in redirects {
        let file = expand_variables(&redirect.file);
        match redirect.mode {
            ast::RedirectMode::Read => {
                if let Ok(f) = fs::File::open(&file) {
                    stdin = Stdin::File(f);
                }
            },
            ast::RedirectMode::Write => {
                if let Ok(f) = fs::File::create(&file) {
                    stdout = Output::File(f);
                }
            },
            ast::RedirectMode::WriteAppend => {
                if let Ok(f) = fs::OpenOptions::new().append(true).create(true).open(&file) {
                    stdout = Output::File(f);
                }
            },
            ast::RedirectMode::ReadAppend => {
                if let Ok(f) = fs::OpenOptions::new()
                    .read(true)
                    .append(true)
                    .create(true)
                    .open(&file)
                {
                    stdout = Output::File(f);
                }
            },
            ast::RedirectMode::ReadDup => {
                if let Ok(fd) = file.parse::<i32>() {
                    stdin = Stdin::File(unsafe { std::fs::File::from_raw_fd(fd) });
                }
            },
            ast::RedirectMode::WriteDup => {
                if let Ok(fd) = file.parse::<i32>() {
                    stdout = Output::FileDescriptor(fd);
                }
            },
            ast::RedirectMode::ReadWrite => {
                if let Ok(f) = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&file)
                {
                    stdout = Output::File(f);
                }
            },
        }
    }

    (stdin, stdout)
}

/// Returns group of processes and also the pgid if it has one
fn eval_command(
    job_manager: &mut JobManager,
    cmd: &ast::Command,
    stdin: Option<Stdin>,
    stdout: Option<Output>,
) -> Result<(Vec<Box<dyn Process>>, Option<u32>), PosixError> {
    match cmd {
        ast::Command::Simple {
            assigns,
            redirects,
            args,
        } => {
            // Process explicit variable assignments from the grammar (WORD = WORD)
            for assign in assigns {
                let val = expand_variables(&assign.val);
                std::env::set_var(&assign.var, &val);
            }

            // If there are no args, check if the first arg was actually an assignment
            // (lexer produces "x=world" as a single WORD)
            let mut filtered_args: Vec<&String> = Vec::new();
            for arg in args {
                if filtered_args.is_empty() {
                    // Before command name: check for inline assignment (x=value)
                    if let Some(eq_pos) = arg.find('=') {
                        if eq_pos > 0 {
                            let name = &arg[..eq_pos];
                            let val = &arg[eq_pos + 1..];
                            if name.chars().next().map_or(false, |c| c.is_alphabetic() || c == '_')
                                && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                            {
                                let expanded_val = expand_variables(val);
                                std::env::set_var(name, &expanded_val);
                                continue;
                            }
                        }
                    }
                }
                filtered_args.push(arg);
            }

            // If no actual command remains (all were assignments), just return
            if filtered_args.is_empty() {
                return Ok((vec![], None));
            }

            // Expand all arguments
            let expanded_args: Vec<String> = filtered_args.iter().flat_map(|a| expand_arg(a)).collect();
            let mut args_it = expanded_args.iter();
            let program = match args_it.next() {
                Some(p) => p.clone(),
                None => return Ok((vec![], None)),
            };
            let args = args_it.cloned().collect::<Vec<_>>();

            let default_stdin = stdin.unwrap_or(Stdin::Inherit);
            let default_stdout = stdout.unwrap_or(Output::Inherit);
            let (proc_stdin, proc_stdout) = process_redirects(redirects, default_stdin, default_stdout);

            // Shell builtins — intercept before external command lookup
            match program.as_str() {
                "true" | ":" => {
                    std::env::set_var("?", "0");
                    return Ok((vec![], None));
                },
                "false" => {
                    std::env::set_var("?", "1");
                    return Ok((vec![], None)); // returns Ok but ?=1
                },
                "break" => {
                    std::env::set_var("_break", "1");
                    return Ok((vec![], None));
                },
                _ => {},
            }

            let (proc, pgid) = match run_external_command(
                &program,
                &args,
                proc_stdin,
                proc_stdout,
                Output::Inherit,
                None,
            ) {
                Ok((proc, pgid)) => (proc, pgid),
                Err(e) => match e.kind() {
                    std::io::ErrorKind::NotFound => {
                        return Err(PosixError::CommandNotFound(program.clone()))
                    },
                    _ => return Err(PosixError::Eval(e.into())),
                },
            };
            Ok((vec![proc], pgid))
        },
        ast::Command::Pipeline(a_cmd, b_cmd) => {
            let (mut a_procs, _a_pgid) =
                eval_command(job_manager, a_cmd, stdin, Some(Output::CreatePipe))?;
            let (b_procs, b_pgid) = eval_command(
                job_manager,
                b_cmd,
                a_procs.last_mut().unwrap().stdout(),
                stdout,
            )?;
            a_procs.extend(b_procs);
            Ok((a_procs, b_pgid))
        },
        ast::Command::AsyncList(a_cmd, b_cmd) => {
            let (procs, pgid) = eval_command(job_manager, a_cmd, None, None)?;
            if !procs.is_empty() {
                run_job(job_manager, procs, pgid, false)?;
            }

            if let Some(b_cmd) = b_cmd {
                eval_command(job_manager, b_cmd, None, None)
            } else {
                Ok((vec![], None))
            }
        },
        ast::Command::SeqList(a_cmd, b_cmd) => {
            let (procs, pgid) = eval_command(job_manager, a_cmd, stdin, stdout)?;
            if !procs.is_empty() {
                let result = run_job(job_manager, procs, pgid, true);
                match result {
                    Ok(()) => std::env::set_var("?", "0"),
                    Err(_) => std::env::set_var("?", "1"),
                }
            }

            if let Some(b_cmd) = b_cmd {
                eval_command(job_manager, b_cmd, None, None)
            } else {
                Ok((vec![], None))
            }
        },
        ast::Command::And(a_cmd, b_cmd) => {
            let (procs, pgid) = eval_command(job_manager, a_cmd, None, None)?;
            if !procs.is_empty() {
                let result = run_job(job_manager, procs, pgid, true);
                match result {
                    Ok(()) => std::env::set_var("?", "0"),
                    Err(_) => std::env::set_var("?", "1"),
                }
            }
            if std::env::var("?").unwrap_or_default() == "0" {
                eval_command(job_manager, b_cmd, None, None)
            } else {
                Ok((vec![], None))
            }
        },
        ast::Command::Or(a_cmd, b_cmd) => {
            let (procs, pgid) = eval_command(job_manager, a_cmd, None, None)?;
            if !procs.is_empty() {
                let result = run_job(job_manager, procs, pgid, true);
                match result {
                    Ok(()) => std::env::set_var("?", "0"),
                    Err(_) => std::env::set_var("?", "1"),
                }
            }
            if std::env::var("?").unwrap_or_default() != "0" {
                eval_command(job_manager, b_cmd, None, None)
            } else {
                Ok((vec![], None))
            }
        },
        ast::Command::Not(cmd) => {
            let (procs, pgid) = eval_command(job_manager, cmd, None, None)?;
            if !procs.is_empty() {
                let result = run_job(job_manager, procs, pgid, true);
                // Invert: success -> failure, failure -> success
                match result {
                    Ok(()) => Ok((vec![], None)), // TODO: should return exit code 1
                    Err(_) => Ok((vec![], None)),
                }
            } else {
                Ok((vec![], None))
            }
        },
        ast::Command::Subshell(cmd) => {
            eval_command(job_manager, cmd, stdin, stdout)
        },
        ast::Command::If {
            conds,
            else_part,
        } => {
            for cond in conds {
                let (procs, pgid) = eval_command(job_manager, &cond.cond, None, None)?;
                if !procs.is_empty() {
                    let result = run_job(job_manager, procs, pgid, true);
                    match result {
                        Ok(()) => std::env::set_var("?", "0"),
                        Err(_) => std::env::set_var("?", "1"),
                    }
                }
                if std::env::var("?").unwrap_or_default() == "0" {
                    return eval_command(job_manager, &cond.body, None, None);
                }
            }
            if let Some(else_cmd) = else_part {
                eval_command(job_manager, else_cmd, None, None)
            } else {
                Ok((vec![], None))
            }
        },
        ast::Command::While { cond, body } => {
            std::env::remove_var("_break");
            loop {
                let (procs, pgid) = eval_command(job_manager, cond, None, None)?;
                if !procs.is_empty() {
                    let result = run_job(job_manager, procs, pgid, true);
                    match result {
                        Ok(()) => std::env::set_var("?", "0"),
                        Err(_) => std::env::set_var("?", "1"),
                    }
                }
                if std::env::var("?").unwrap_or_default() != "0" {
                    break;
                }
                let (body_procs, body_pgid) = eval_command(job_manager, body, None, None)?;
                if !body_procs.is_empty() {
                    run_job(job_manager, body_procs, body_pgid, true)?;
                }
                if std::env::var("_break").unwrap_or_default() == "1" {
                    std::env::remove_var("_break");
                    break;
                }
            }
            Ok((vec![], None))
        },
        ast::Command::Until { cond, body } => {
            loop {
                let (procs, pgid) = eval_command(job_manager, cond, None, None)?;
                let result = if !procs.is_empty() {
                    run_job(job_manager, procs, pgid, true)
                } else {
                    Err(PosixError::Eval(anyhow::anyhow!("empty")))
                };
                if result.is_ok() {
                    break;
                }
                let (body_procs, body_pgid) = eval_command(job_manager, body, None, None)?;
                if !body_procs.is_empty() {
                    run_job(job_manager, body_procs, body_pgid, true)?;
                }
            }
            Ok((vec![], None))
        },
        ast::Command::For {
            name,
            wordlist,
            body,
        } => {
            std::env::remove_var("_break");
            for word in wordlist {
                let expanded_word = expand_variables(word);
                std::env::set_var(name, &expanded_word);
                let (body_procs, body_pgid) = eval_command(job_manager, body, None, None)?;
                if !body_procs.is_empty() {
                    run_job(job_manager, body_procs, body_pgid, true)?;
                }
                if std::env::var("_break").unwrap_or_default() == "1" {
                    std::env::remove_var("_break");
                    break;
                }
            }
            Ok((vec![], None))
        },
        ast::Command::Case { word, arms } => {
            let expanded_word = expand_variables(word);
            for arm in arms {
                let mut matched = false;
                for pattern in &arm.pattern {
                    let expanded_pattern = expand_variables(pattern);
                    if expanded_pattern == "*" {
                        matched = true;
                        break;
                    }
                    // Try glob matching
                    if let Ok(glob_pat) = glob::Pattern::new(&expanded_pattern) {
                        if glob_pat.matches(&expanded_word) {
                            matched = true;
                            break;
                        }
                    } else if expanded_pattern == expanded_word {
                        matched = true;
                        break;
                    }
                }
                if matched {
                    let (procs, pgid) = eval_command(job_manager, &arm.body, None, None)?;
                    if !procs.is_empty() {
                        run_job(job_manager, procs, pgid, true)?;
                    }
                    return Ok((vec![], None));
                }
            }
            Ok((vec![], None))
        },
        ast::Command::Fn { fname, body } => {
            // Function body is not stored here — instead, the grammar's
            // FunctionDefinition rule already extracts the body. We store
            // the raw body source in FUNCTIONS for later invocation.
            // Since we can't easily get the source text here, we store a
            // debug representation.
            // TODO: proper function storage with AST preservation
            drop(body);
            FUNCTIONS.with(|f| {
                f.borrow_mut().insert(fname.clone(), fname.clone());
            });
            Ok((vec![], None))
        },
        ast::Command::None => Ok((vec![], None)),
    }
}
