//! The part of Python's `argparse` (3.14) that `pdf2txt.py` and `dumppdf.py` use: the same
//! parsing (option abbreviations, `-xVALUE`, `-abc`, `--opt=value`, `--`, negative numbers,
//! mutually exclusive groups), the same error messages and exit code 2, and the same `--help`
//! text (`HelpFormatter` with `textwrap`).
//!
//! DIVERGE(argparse-terminal): Python takes the help width from `$COLUMNS`, else from the
//! terminal stdout is connected to, else 80; the port uses `$COLUMNS`, else 80. Python 3.14 also
//! colors help and errors when it writes to a terminal; the port never does. Neither affects
//! output written to a pipe or a file.

use std::collections::HashMap;
use std::collections::HashSet;

/// A parsed value.
#[derive(Clone, Debug, PartialEq)]
pub enum ArgVal {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<ArgVal>),
}

/// A conversion error: `ValueError`/`TypeError` (argparse reports "invalid TYPE value: 'x'") or
/// `ArgumentTypeError(msg)`.
pub enum ConvError {
    Invalid,
    Msg(String),
}

pub type Conv = fn(&str) -> Result<ArgVal, ConvError>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Nargs {
    Zero,
    One,
    Plus,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    StoreTrue,
    Store,
    Help,
    Version,
}

pub struct Action {
    pub option_strings: Vec<String>,
    pub dest: String,
    pub kind: Kind,
    pub nargs: Nargs,
    /// The `type=` function and its `__name__`.
    pub conv: Option<(Conv, &'static str)>,
    pub default: ArgVal,
    pub help: String,
    pub required: bool,
}

struct Group {
    title: String,
    description: Option<String>,
    actions: Vec<usize>,
}

/// How a parse ends early: print `stdout`/`stderr` and exit with `code`.
pub struct Exit {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub type Namespace = HashMap<String, ArgVal>;

type OptionTuple = (Option<usize>, String, Option<String>, Option<String>);

pub struct ArgumentParser {
    pub prog: String,
    pub description: Option<String>,
    pub version: String,
    actions: Vec<Action>,
    groups: Vec<Group>,
    mutex_groups: Vec<Vec<usize>>,
    /// `_option_string_actions`, in insertion order.
    option_strings: Vec<(String, usize)>,
}

/// `ArgumentError(action, message)` as `str()` prints it.
struct ArgError(String);

impl ArgumentParser {
    /// `ArgumentParser(prog, description, add_help=True)`.
    pub fn new(prog: &str, description: Option<&str>) -> Self {
        let mut p = ArgumentParser {
            prog: prog.to_string(),
            description: description.map(str::to_string),
            version: String::new(),
            actions: Vec::new(),
            groups: vec![
                Group { title: "positional arguments".into(), description: None, actions: vec![] },
                Group { title: "options".into(), description: None, actions: vec![] },
            ],
            mutex_groups: Vec::new(),
            option_strings: Vec::new(),
        };
        p.add_argument(
            1,
            &["-h", "--help"],
            Kind::Help,
            Nargs::Zero,
            None,
            ArgVal::None,
            "show this help message and exit",
        );
        p
    }

    /// `add_argument_group(title, description)`: returns the group's index.
    pub fn add_argument_group(&mut self, title: &str, description: Option<&str>) -> usize {
        self.groups.push(Group { title: title.into(), description: description.map(str::to_string), actions: vec![] });
        self.groups.len() - 1
    }

    /// `add_mutually_exclusive_group()`: returns the mutex index.
    pub fn add_mutually_exclusive_group(&mut self) -> usize {
        self.mutex_groups.push(Vec::new());
        self.mutex_groups.len() - 1
    }

    /// `add_argument(...)` to `group`; returns the action's index.
    #[allow(clippy::too_many_arguments)]
    pub fn add_argument(
        &mut self,
        group: usize,
        names: &[&str],
        kind: Kind,
        nargs: Nargs,
        conv: Option<(Conv, &'static str)>,
        default: ArgVal,
        help: &str,
    ) -> usize {
        let positional = !names[0].starts_with('-');
        let dest = if positional {
            names[0].to_string()
        } else {
            let long = names.iter().find(|n| n.starts_with("--")).unwrap_or(&names[0]);
            long.trim_start_matches('-').replace('-', "_")
        };
        let idx = self.actions.len();
        self.actions.push(Action {
            option_strings: if positional { vec![] } else { names.iter().map(|s| s.to_string()).collect() },
            dest,
            kind,
            nargs,
            conv,
            default,
            help: help.to_string(),
            required: positional,
        });
        let group = if positional { 0 } else { group };
        self.groups[group].actions.push(idx);
        if !positional {
            for n in names {
                self.option_strings.push((n.to_string(), idx));
            }
        }
        idx
    }

    pub fn add_to_mutex(&mut self, mutex: usize, action: usize) {
        self.mutex_groups[mutex].push(action);
    }

    fn lookup(&self, s: &str) -> Option<usize> {
        self.option_strings.iter().find(|(o, _)| o == s).map(|(_, i)| *i)
    }

    fn action_name(&self, a: usize) -> String {
        let action = &self.actions[a];
        if !action.option_strings.is_empty() { action.option_strings.join("/") } else { action.dest.clone() }
    }

    fn arg_error(&self, a: Option<usize>, msg: &str) -> ArgError {
        match a {
            Some(a) => ArgError(format!("argument {}: {}", self.action_name(a), msg)),
            None => ArgError(msg.to_string()),
        }
    }

    // --- parsing

    /// `_parse_optional(arg_string)`: `None` for a positional, else the option tuples
    /// `(action, option_string, sep, explicit_arg)`.
    fn parse_optional(&self, arg: &str) -> Option<Vec<OptionTuple>> {
        if arg.is_empty() || !arg.starts_with('-') {
            return None;
        }
        if let Some(a) = self.lookup(arg) {
            return Some(vec![(Some(a), arg.to_string(), None, None)]);
        }
        if arg.chars().count() == 1 {
            return None;
        }
        if let Some((opt, explicit)) = arg.split_once('=')
            && let Some(a) = self.lookup(opt)
        {
            return Some(vec![(Some(a), opt.to_string(), Some("=".into()), Some(explicit.to_string()))]);
        }
        let tuples = self.get_option_tuples(arg);
        if !tuples.is_empty() {
            return Some(tuples);
        }
        // negative number matcher `-\.?\d`
        let b: Vec<char> = arg.chars().collect();
        let negative =
            b.len() >= 2 && (b[1].is_ascii_digit() || (b[1] == '.' && b.len() >= 3 && b[2].is_ascii_digit()));
        if negative {
            return None;
        }
        if arg.contains(' ') {
            return None;
        }
        Some(vec![(None, arg.to_string(), None, None)])
    }

    fn get_option_tuples(&self, option_string: &str) -> Vec<OptionTuple> {
        let mut result = Vec::new();
        let chars: Vec<char> = option_string.chars().collect();
        let (prefix, sep, explicit) = match option_string.split_once('=') {
            Some((a, b)) => (a.to_string(), Some("=".to_string()), Some(b.to_string())),
            None => (option_string.to_string(), None, None),
        };
        if chars[0] == '-' && chars[1] == '-' {
            for (o, a) in &self.option_strings {
                if o.starts_with(&prefix) {
                    result.push((Some(*a), o.clone(), sep.clone(), explicit.clone()));
                }
            }
        } else {
            let short_prefix: String = chars[..2].iter().collect();
            let short_explicit: String = chars[2..].iter().collect();
            for (o, a) in &self.option_strings {
                if *o == short_prefix {
                    result.push((Some(*a), o.clone(), Some(String::new()), Some(short_explicit.clone())));
                } else if o.starts_with(&prefix) {
                    result.push((Some(*a), o.clone(), sep.clone(), explicit.clone()));
                }
            }
        }
        result
    }

    /// The number of argument strings the pattern of `nargs` matches at the start of `pattern`.
    fn match_count(nargs: Nargs, optional: bool, pattern: &[u8]) -> Option<usize> {
        match (nargs, optional) {
            (Nargs::Zero, _) => Some(0),
            (Nargs::One, true) => (pattern.first() == Some(&b'A')).then_some(1),
            (Nargs::Plus, true) => {
                let n = pattern.iter().take_while(|&&c| c == b'A').count();
                (n > 0).then_some(n)
            }
            (Nargs::One, false) => {
                // (-*A-*)
                let mut i = pattern.iter().take_while(|&&c| c == b'-').count();
                if pattern.get(i) != Some(&b'A') {
                    return None;
                }
                i += 1;
                i += pattern[i..].iter().take_while(|&&c| c == b'-').count();
                Some(i)
            }
            (Nargs::Plus, false) => {
                // (-*A[A-]*)
                let mut i = pattern.iter().take_while(|&&c| c == b'-').count();
                if pattern.get(i) != Some(&b'A') {
                    return None;
                }
                i += 1;
                i += pattern[i..].iter().take_while(|&&c| c == b'A' || c == b'-').count();
                Some(i)
            }
        }
    }

    fn get_value(&self, a: usize, s: &str) -> Result<ArgVal, ArgError> {
        match self.actions[a].conv {
            None => Ok(ArgVal::Str(s.to_string())),
            Some((f, name)) => f(s).map_err(|e| match e {
                ConvError::Invalid => self.arg_error(Some(a), &format!("invalid {} value: {}", name, py_str_repr(s))),
                ConvError::Msg(m) => self.arg_error(Some(a), &m),
            }),
        }
    }

    /// `parse_args(args)`.
    pub fn parse_args(&self, args: &[String]) -> Result<Namespace, Exit> {
        match self.parse_known(args) {
            Ok(ns) => Ok(ns),
            Err(Ok(exit)) => Err(exit),
            Err(Err(ArgError(msg))) => Err(Exit {
                code: 2,
                stdout: String::new(),
                stderr: format!("{}{}: error: {}\n", self.format_usage(), self.prog, msg),
            }),
        }
    }

    fn parse_known(&self, arg_strings: &[String]) -> Result<Namespace, Result<Exit, ArgError>> {
        let mut ns: Namespace = HashMap::new();
        for a in &self.actions {
            if a.kind != Kind::Help && a.kind != Kind::Version {
                ns.entry(a.dest.clone()).or_insert_with(|| a.default.clone());
            }
        }

        let mut conflicts: HashMap<usize, Vec<usize>> = HashMap::new();
        for g in &self.mutex_groups {
            for (i, &a) in g.iter().enumerate() {
                let c = conflicts.entry(a).or_default();
                c.extend(&g[..i]);
                c.extend(&g[i + 1..]);
            }
        }

        let mut option_indices: HashMap<usize, Vec<OptionTuple>> = HashMap::new();
        let mut pattern: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < arg_strings.len() {
            let s = &arg_strings[i];
            if s == "--" {
                pattern.push(b'-');
                pattern.extend(std::iter::repeat_n(b'A', arg_strings.len() - i - 1));
                break;
            }
            match self.parse_optional(s) {
                None => pattern.push(b'A'),
                Some(t) => {
                    option_indices.insert(i, t);
                    pattern.push(b'O');
                }
            }
            i += 1;
        }

        let mut seen: HashSet<usize> = HashSet::new();
        let mut seen_non_default: HashSet<usize> = HashSet::new();
        let mut extras: Vec<String> = Vec::new();
        let mut positionals: Vec<usize> =
            (0..self.actions.len()).filter(|&a| self.actions[a].option_strings.is_empty()).collect();

        // take_action
        let take_action = |this: &Self,
                           a: usize,
                           strings: Vec<String>,
                           ns: &mut Namespace,
                           seen: &mut HashSet<usize>,
                           seen_non_default: &mut HashSet<usize>|
         -> Result<(), Result<Exit, ArgError>> {
            seen.insert(a);
            let action = &this.actions[a];
            let value = match action.nargs {
                Nargs::Zero => ArgVal::Bool(true),
                Nargs::One => this.get_value(a, &strings[0]).map_err(Err)?,
                Nargs::Plus => {
                    let mut v = Vec::new();
                    for s in &strings {
                        v.push(this.get_value(a, s).map_err(Err)?);
                    }
                    ArgVal::List(v)
                }
            };
            if !action.option_strings.is_empty() || !strings.is_empty() {
                seen_non_default.insert(a);
                if let Some(cs) = conflicts.get(&a) {
                    for &c in cs {
                        if seen_non_default.contains(&c) {
                            let msg = format!("not allowed with argument {}", this.action_name(c));
                            return Err(Err(this.arg_error(Some(a), &msg)));
                        }
                    }
                }
            }
            match action.kind {
                Kind::Help => {
                    return Err(Ok(Exit { code: 0, stdout: this.format_help(), stderr: String::new() }));
                }
                Kind::Version => {
                    let text = format!("{}\n", this.version);
                    return Err(Ok(Exit { code: 0, stdout: text, stderr: String::new() }));
                }
                _ => {
                    ns.insert(action.dest.clone(), value);
                }
            }
            Ok(())
        };

        // consume_positionals
        let consume_positionals = |this: &Self,
                                   start_index: usize,
                                   positionals: &mut Vec<usize>,
                                   ns: &mut Namespace,
                                   seen: &mut HashSet<usize>,
                                   seen_non_default: &mut HashSet<usize>|
         -> Result<usize, Result<Exit, ArgError>> {
            let selected = &pattern[start_index..];
            // _match_arguments_partial for the remaining positionals.
            let mut counts: Vec<usize> = Vec::new();
            for n in (1..=positionals.len()).rev() {
                let mut pos = 0;
                let mut cs = Vec::new();
                let mut ok = true;
                for &p in &positionals[..n] {
                    match Self::match_count(this.actions[p].nargs, false, &selected[pos..]) {
                        Some(c) => {
                            cs.push(c);
                            pos += c;
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    if pos < selected.len() && selected[pos] == b'O' {
                        while cs.last() == Some(&0) {
                            cs.pop();
                        }
                    }
                    counts = cs;
                    break;
                }
            }
            let mut start = start_index;
            for (k, &c) in counts.iter().enumerate() {
                let a = positionals[k];
                let mut args: Vec<String> = arg_strings[start..start + c].to_vec();
                if pattern[start..start + c].contains(&b'-')
                    && let Some(p) = args.iter().position(|s| s == "--")
                {
                    args.remove(p);
                }
                start += c;
                take_action(this, a, args, ns, seen, seen_non_default)?;
            }
            positionals.drain(..counts.len());
            Ok(start)
        };

        let max_option_index: i64 = option_indices.keys().max().map_or(-1, |&m| m as i64);
        let mut start_index: usize = 0;
        while (start_index as i64) <= max_option_index {
            let mut next = start_index;
            while (next as i64) <= max_option_index {
                if option_indices.contains_key(&next) {
                    break;
                }
                next += 1;
            }
            if start_index != next {
                let end = consume_positionals(
                    self,
                    start_index,
                    &mut positionals,
                    &mut ns,
                    &mut seen,
                    &mut seen_non_default,
                )?;
                if end > start_index {
                    start_index = end;
                    continue;
                } else {
                    start_index = end;
                }
            }
            if !option_indices.contains_key(&start_index) {
                extras.extend(arg_strings[start_index..next].iter().cloned());
                start_index = next;
            }

            // consume_optional
            let tuples = &option_indices[&start_index];
            if tuples.len() > 1 {
                let options: Vec<&str> = tuples.iter().map(|t| t.1.as_str()).collect();
                let msg = format!("ambiguous option: {} could match {}", arg_strings[start_index], options.join(", "));
                return Err(Err(ArgError(msg)));
            }
            let (mut action, mut option_string, mut sep, mut explicit_arg) = tuples[0].clone();
            let mut action_tuples: Vec<(usize, Vec<String>)> = Vec::new();
            let stop;
            loop {
                let Some(a) = action else {
                    extras.push(arg_strings[start_index].clone());
                    stop = start_index + 1;
                    break;
                };
                if let Some(explicit) = explicit_arg.clone() {
                    let arg_count = Self::match_count(self.actions[a].nargs, true, b"A");
                    let second = option_string.chars().nth(1);
                    if arg_count == Some(0) && second != Some('-') && !explicit.is_empty() {
                        if sep.as_deref().is_some_and(|s| !s.is_empty()) || explicit.starts_with('-') {
                            let msg = format!("ignored explicit argument {}", py_str_repr(&explicit));
                            return Err(Err(self.arg_error(Some(a), &msg)));
                        }
                        action_tuples.push((a, vec![]));
                        let first = explicit.chars().next().unwrap();
                        option_string = format!("-{first}");
                        if let Some(na) = self.lookup(&option_string) {
                            action = Some(na);
                            let rest: String = explicit.chars().skip(1).collect();
                            if rest.is_empty() {
                                sep = None;
                                explicit_arg = None;
                            } else if let Some(r) = rest.strip_prefix('=') {
                                sep = Some("=".into());
                                explicit_arg = Some(r.to_string());
                            } else {
                                sep = Some(String::new());
                                explicit_arg = Some(rest);
                            }
                        } else {
                            extras.push(format!("-{explicit}"));
                            stop = start_index + 1;
                            break;
                        }
                    } else if arg_count == Some(1) {
                        stop = start_index + 1;
                        action_tuples.push((a, vec![explicit]));
                        break;
                    } else {
                        let msg = format!("ignored explicit argument {}", py_str_repr(&explicit));
                        return Err(Err(self.arg_error(Some(a), &msg)));
                    }
                } else {
                    let start = start_index + 1;
                    let Some(arg_count) = Self::match_count(self.actions[a].nargs, true, &pattern[start..]) else {
                        let msg = match self.actions[a].nargs {
                            Nargs::Plus => "expected at least one argument",
                            _ => "expected one argument",
                        };
                        return Err(Err(self.arg_error(Some(a), msg)));
                    };
                    stop = start + arg_count;
                    action_tuples.push((a, arg_strings[start..stop].to_vec()));
                    break;
                }
            }
            for (a, args) in action_tuples {
                take_action(self, a, args, &mut ns, &mut seen, &mut seen_non_default)?;
            }
            start_index = stop;
        }

        let stop_index =
            consume_positionals(self, start_index, &mut positionals, &mut ns, &mut seen, &mut seen_non_default)?;
        extras.extend(arg_strings[stop_index..].iter().cloned());

        let required: Vec<String> = (0..self.actions.len())
            .filter(|a| !seen.contains(a) && self.actions[*a].required)
            .map(|a| self.action_name(a))
            .collect();
        if !required.is_empty() {
            return Err(Err(ArgError(format!("the following arguments are required: {}", required.join(", ")))));
        }
        if !extras.is_empty() {
            return Err(Err(ArgError(format!("unrecognized arguments: {}", extras.join(" ")))));
        }
        Ok(ns)
    }

    // --- help

    fn width() -> usize {
        let columns = std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse::<i64>().ok()).unwrap_or(0);
        let columns = if columns <= 0 { 80 } else { columns };
        (columns - 2).max(0) as usize
    }

    fn format_args(action: &Action) -> String {
        let metavar = if action.option_strings.is_empty() { action.dest.clone() } else { action.dest.to_uppercase() };
        match action.nargs {
            Nargs::Zero => String::new(),
            Nargs::One => metavar,
            Nargs::Plus => format!("{metavar} [{metavar} ...]"),
        }
    }

    fn usage_parts(&self) -> (Vec<String>, usize) {
        // group actions by mutually exclusive groups
        let mut mutex_of: HashMap<usize, usize> = HashMap::new();
        for (g, acts) in self.mutex_groups.iter().enumerate() {
            for &a in acts {
                mutex_of.insert(a, g);
            }
        }
        let mut remaining: Vec<bool> = vec![true; self.actions.len()];
        let mut positionals: Vec<Vec<usize>> = Vec::new();
        for (a, r) in remaining.iter_mut().enumerate() {
            if self.actions[a].option_strings.is_empty() {
                *r = false;
                positionals.push(vec![a]);
            }
        }
        let mut optionals: Vec<Vec<usize>> = Vec::new();
        for a in 0..self.actions.len() {
            if !self.actions[a].option_strings.is_empty() && remaining[a] {
                remaining[a] = false;
                match mutex_of.get(&a) {
                    Some(&g) => {
                        let mut group = vec![a];
                        for &a2 in &self.mutex_groups[g] {
                            if a2 != a && remaining[a2] {
                                remaining[a2] = false;
                                group.push(a2);
                            }
                        }
                        optionals.push(group);
                    }
                    None => optionals.push(vec![a]),
                }
            }
        }
        let mut parts: Vec<String> = Vec::new();
        let mut pos_start = None;
        let n_opt = optionals.len();
        for (i, group) in optionals.into_iter().chain(positionals).enumerate() {
            let start = parts.len();
            if i == n_opt {
                pos_start = Some(start);
            }
            let in_group = group.len() > 1;
            for &a in &group {
                let action = &self.actions[a];
                let part = if action.option_strings.is_empty() {
                    let mut part = Self::format_args(action);
                    if in_group && part.starts_with('[') && part.ends_with(']') {
                        part = part[1..part.len() - 1].to_string();
                    }
                    part
                } else {
                    let option_string = &action.option_strings[0];
                    let part = if action.nargs == Nargs::Zero {
                        option_string.clone()
                    } else {
                        format!("{} {}", option_string, Self::format_args(action))
                    };
                    if !(action.required || in_group) { format!("[{part}]") } else { part }
                };
                parts.push(part);
            }
            if in_group {
                parts[start] = format!("[{}", parts[start]);
                let last = parts.len() - 1;
                for p in parts.iter_mut().take(last).skip(start) {
                    p.push_str(" |");
                }
                parts[last].push(']');
            }
        }
        let pos_start = pos_start.unwrap_or(parts.len());
        (parts, pos_start)
    }

    fn format_usage_text(&self, width: usize) -> String {
        let prefix = "usage: ";
        let prog = self.prog.clone();
        let (parts, pos_start) = self.usage_parts();
        let mut all = vec![prog.clone()];
        all.extend(parts.iter().cloned());
        let mut usage = all.iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
        let text_width = width;
        if prefix.len() + usage.chars().count() > text_width {
            let opt_parts = &parts[..pos_start];
            let pos_parts = &parts[pos_start..];
            let get_lines = |parts: &[String], indent: &str, prefix: Option<&str>| -> Vec<String> {
                let mut lines = Vec::new();
                let mut line: Vec<String> = Vec::new();
                let indent_length = indent.chars().count();
                let mut line_len: i64 =
                    if let Some(p) = prefix { p.len() as i64 - 1 } else { indent_length as i64 - 1 };
                for part in parts {
                    let part_len = part.chars().count() as i64;
                    if line_len + 1 + part_len > text_width as i64 && !line.is_empty() {
                        lines.push(format!("{}{}", indent, line.join(" ")));
                        line.clear();
                        line_len = indent_length as i64 - 1;
                    }
                    line.push(part.clone());
                    line_len += part_len + 1;
                }
                if !line.is_empty() {
                    lines.push(format!("{}{}", indent, line.join(" ")));
                }
                if prefix.is_some() && !lines.is_empty() {
                    lines[0] = lines[0].chars().skip(indent_length).collect();
                }
                lines
            };
            let prog_len = prog.chars().count();
            let lines: Vec<String> = if (prefix.len() + prog_len) as f64 <= 0.75 * text_width as f64 {
                let indent = " ".repeat(prefix.len() + prog_len + 1);
                if !opt_parts.is_empty() {
                    let mut v = vec![prog.clone()];
                    v.extend(opt_parts.iter().cloned());
                    let mut lines = get_lines(&v, &indent, Some(prefix));
                    lines.extend(get_lines(pos_parts, &indent, None));
                    lines
                } else if !pos_parts.is_empty() {
                    let mut v = vec![prog.clone()];
                    v.extend(pos_parts.iter().cloned());
                    get_lines(&v, &indent, Some(prefix))
                } else {
                    vec![prog.clone()]
                }
            } else {
                let indent = " ".repeat(prefix.len());
                let mut all = opt_parts.to_vec();
                all.extend(pos_parts.iter().cloned());
                let mut lines = get_lines(&all, &indent, None);
                if lines.len() > 1 {
                    lines = get_lines(opt_parts, &indent, None);
                    lines.extend(get_lines(pos_parts, &indent, None));
                }
                let mut v = vec![prog.clone()];
                v.extend(lines);
                v
            };
            usage = lines.join("\n");
        }
        format!("{prefix}{usage}\n\n")
    }

    /// `format_usage()`.
    pub fn format_usage(&self) -> String {
        finish_help(&self.format_usage_text(Self::width()))
    }

    fn invocation(action: &Action) -> String {
        if action.option_strings.is_empty() {
            action.dest.clone()
        } else if action.nargs == Nargs::Zero {
            action.option_strings.join(", ")
        } else {
            format!("{} {}", action.option_strings.join(", "), Self::format_args(action))
        }
    }

    /// `format_help()`.
    pub fn format_help(&self) -> String {
        let width = Self::width();
        let max_help_position = 24.min(width.saturating_sub(20).max(4));
        let mut out = String::new();
        out.push_str(&self.format_usage_text(width));
        if let Some(d) = &self.description {
            out.push_str(&format_text(d, width, 0));
        }
        // the longest invocation (+ indent 2 within sections)
        let mut action_max_length = 0;
        for g in &self.groups {
            for &a in &g.actions {
                action_max_length = action_max_length.max(Self::invocation(&self.actions[a]).chars().count() + 2);
            }
        }
        for g in &self.groups {
            let mut items = String::new();
            if let Some(d) = &g.description {
                items.push_str(&format_text(d, width, 2));
            }
            for &a in &g.actions {
                items.push_str(&self.format_action(&self.actions[a], width, action_max_length, max_help_position));
            }
            if items.is_empty() {
                continue;
            }
            out.push('\n');
            out.push_str(&format!("{}:\n", g.title));
            out.push_str(&items);
            out.push('\n');
        }
        finish_help(&out)
    }

    fn format_action(
        &self,
        action: &Action,
        width: usize,
        action_max_length: usize,
        max_help_position: usize,
    ) -> String {
        let current_indent = 2;
        let help_position = (action_max_length + 2).min(max_help_position);
        let help_width = (width as i64 - help_position as i64).max(11) as usize;
        let action_width = help_position as i64 - current_indent as i64 - 2;
        let header = Self::invocation(action);
        let mut parts = String::new();
        if action.help.is_empty() {
            parts.push_str(&format!("{}{}\n", " ".repeat(current_indent), header));
            return parts;
        }
        let indent_first = if header.chars().count() as i64 <= action_width {
            let pad = action_width as usize - header.chars().count();
            parts.push_str(&format!("{}{}{}  ", " ".repeat(current_indent), header, " ".repeat(pad)));
            0
        } else {
            parts.push_str(&format!("{}{}\n", " ".repeat(current_indent), header));
            help_position
        };
        let lines = wrap(&normalize_ws(&action.help), help_width);
        if let Some(first) = lines.first() {
            parts.push_str(&format!("{}{}\n", " ".repeat(indent_first), first));
            for line in &lines[1..] {
                parts.push_str(&format!("{}{}\n", " ".repeat(help_position), line));
            }
        }
        parts
    }
}

/// `format_help()`'s final step: `\n\n\n+` -> `\n\n`, strip newlines, one final newline.
fn finish_help(s: &str) -> String {
    let mut out = String::new();
    let mut nl = 0;
    for c in s.chars() {
        if c == '\n' {
            nl += 1;
        } else {
            if nl > 0 {
                out.push_str(&"\n".repeat(nl.min(2)));
                nl = 0;
            }
            out.push(c);
        }
    }
    let trimmed = out.trim_start_matches('\n');
    if trimmed.is_empty() { String::new() } else { format!("{trimmed}\n") }
}

/// `_format_text(text)` at `indent`.
fn format_text(text: &str, width: usize, indent: usize) -> String {
    let text_width = (width as i64 - indent as i64).max(11) as usize;
    let ind = " ".repeat(indent);
    let lines = wrap(&normalize_ws(text), text_width.saturating_sub(indent).max(1));
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&ind);
        out.push_str(l);
    }
    out.push_str("\n\n");
    out
}

/// `re.sub(r'\s+', ' ', text).strip()` (ASCII whitespace).
fn normalize_ws(text: &str) -> String {
    text.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_letter(c: char) -> bool {
    (c.is_alphanumeric() && !c.is_numeric()) || c == '_'
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_word_punct(c: char) -> bool {
    is_word(c) || "!\"'&.,?".contains(c)
}

/// `TextWrapper._split`: the chunks `wordsep_re` splits a (whitespace-normalized) text into.
fn split_chunks(text: &str) -> Vec<String> {
    let t: Vec<char> = text.chars().collect();
    let at = |i: isize| -> Option<char> { if i < 0 { None } else { t.get(i as usize).copied() } };
    let mut chunks = Vec::new();
    let mut p = 0usize;
    while p < t.len() {
        if t[p] == ' ' {
            let mut j = p;
            while j < t.len() && t[j] == ' ' {
                j += 1;
            }
            chunks.push(t[p..j].iter().collect());
            p = j;
            continue;
        }
        let pi = p as isize;
        // em-dash between words: (?<=wp) -{2,} (?=\w)
        if at(pi - 1).is_some_and(is_word_punct) && t[p] == '-' {
            let mut j = p;
            while j < t.len() && t[j] == '-' {
                j += 1;
            }
            if j - p >= 2 && at(j as isize).is_some_and(is_word) {
                chunks.push(t[p..j].iter().collect());
                p = j;
                continue;
            }
        }
        // word, possibly hyphenated: nws+? then one of the endings
        let mut j = p + 1;
        let end;
        loop {
            let ji = j as isize;
            if j > t.len() {
                end = t.len();
                break;
            }
            // hyphenated word: the hyphen at j
            if at(ji) == Some('-') {
                let lb = (at(ji - 2).is_some_and(is_letter) && at(ji - 1).is_some_and(is_letter))
                    || (at(ji - 3).is_some_and(is_letter)
                        && at(ji - 2) == Some('-')
                        && at(ji - 1).is_some_and(is_letter));
                let la = at(ji + 1).is_some_and(is_letter)
                    && (at(ji + 2).is_some_and(is_letter)
                        || (at(ji + 2) == Some('-') && at(ji + 3).is_some_and(is_letter)));
                if lb && la {
                    end = j + 1;
                    break;
                }
            }
            // end of word
            if j == t.len() || t[j] == ' ' {
                end = j;
                break;
            }
            // em-dash
            if at(ji - 1).is_some_and(is_word_punct) && at(ji) == Some('-') && at(ji + 1) == Some('-') {
                let mut k = j;
                while k < t.len() && t[k] == '-' {
                    k += 1;
                }
                if at(k as isize).is_some_and(is_word) {
                    end = j;
                    break;
                }
            }
            j += 1;
        }
        chunks.push(t[p..end].iter().collect());
        p = end;
    }
    chunks
}

/// `textwrap.wrap(text, width)` (no indents).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut chunks: Vec<String> = split_chunks(text);
    chunks.reverse();
    let mut lines: Vec<String> = Vec::new();
    while !chunks.is_empty() {
        let mut cur_line: Vec<String> = Vec::new();
        let mut cur_len = 0usize;
        if !lines.is_empty() {
            chunks.pop_if(|c| c.trim().is_empty());
        }
        while let Some(c) = chunks.pop_if(|c| cur_len + c.chars().count() <= width) {
            cur_len += c.chars().count();
            cur_line.push(c);
        }
        // _handle_long_word: splits the last chunk in place, or takes it whole.
        #[allow(clippy::manual_pop_if)]
        if let Some(c) = chunks.last()
            && c.chars().count() > width
        {
            let space_left = if width < 1 { 1 } else { width - cur_len };
            if space_left > 0 {
                let chunk: Vec<char> = c.chars().collect();
                let mut end = space_left;
                if chunk.len() > space_left {
                    let hyphen = chunk[..space_left].iter().rposition(|&ch| ch == '-');
                    if let Some(h) = hyphen
                        && h > 0
                        && chunk[..h].iter().any(|&ch| ch != '-')
                    {
                        end = h + 1;
                    }
                }
                cur_line.push(chunk[..end].iter().collect());
                let rest: String = chunk[end..].iter().collect();
                *chunks.last_mut().unwrap() = rest;
            } else if cur_line.is_empty() {
                cur_line.push(chunks.pop().unwrap());
            }
        }
        cur_line.pop_if(|s| s.trim().is_empty());
        if !cur_line.is_empty() {
            lines.push(cur_line.concat());
        }
    }
    lines
}

/// `repr(s)` of an argument string.
pub fn py_str_repr(s: &str) -> String {
    pdfminer::pyrt::fmt::str_repr(s)
}
