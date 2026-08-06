//! Recipe execution and job scheduling.
//!
//! Expansion happens on the scheduling thread and only fully-expanded command
//! text is handed to workers. `$(shell)` and `$(eval)` mutate the engine, so
//! expanding inside a worker would need the engine to be shared and locked;
//! this way the workers touch nothing but `std::process`.

use crate::graph::Graph;
use crate::{AutoVars, Engine, Error, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Pending,
    Running,
    Done,
    Failed,
}

struct Cmd {
    text: String,
    echo: bool,
    ignore: bool,
    /// False under `-n` for a line without a `+` prefix.
    execute: bool,
}

struct Job {
    idx: usize,
    cmds: Vec<Cmd>,
    shell: String,
    env: Vec<(String, String)>,
    /// Under `-j N` with `N > 1`, output is captured and flushed atomically
    /// when the job finishes, so concurrent recipes cannot interleave mid-line.
    capture: bool,
}

struct Finished {
    idx: usize,
    ok: bool,
    out: Vec<u8>,
    ran: bool,
    failed: Option<(String, i32)>,
}

fn execute(job: Job) -> Finished {
    let mut buf: Vec<u8> = Vec::new();
    let mut ran = false;
    let mut failed = None;
    let mut ok = true;

    for cmd in &job.cmds {
        if cmd.echo {
            if job.capture {
                let _ = writeln!(buf, "{}", cmd.text);
            } else {
                println!("{}", cmd.text);
                let _ = std::io::stdout().flush();
            }
        }
        if !cmd.execute {
            continue;
        }
        ran = true;
        let mut c = Command::new(&job.shell);
        c.arg("-c").arg(&cmd.text);
        c.env_clear();
        for (k, v) in &job.env {
            c.env(k, v);
        }
        let status = if job.capture {
            c.stdout(Stdio::piped()).stderr(Stdio::piped());
            match c.output() {
                Ok(o) => {
                    buf.extend_from_slice(&o.stdout);
                    buf.extend_from_slice(&o.stderr);
                    Ok(o.status)
                }
                Err(e) => Err(e),
            }
        } else {
            c.status()
        };
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                let code = s.code().unwrap_or(-1);
                if cmd.ignore {
                    let _ = writeln!(
                        buf,
                        "rsmake: [{}] Error {code} (ignored)",
                        cmd.text.lines().next().unwrap_or("")
                    );
                } else {
                    failed = Some((cmd.text.clone(), code));
                    ok = false;
                    break;
                }
            }
            Err(e) => {
                if !cmd.ignore {
                    failed = Some((format!("{}: {e}", job.shell), 127));
                    ok = false;
                    break;
                }
            }
        }
    }

    if job.capture && !buf.is_empty() {
        // Written by the worker rather than shipped back for the scheduler to
        // print, so a long-running job's output is not held until every other
        // job finishes.
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&buf);
        let _ = out.flush();
        buf.clear();
    }

    Finished {
        idx: job.idx,
        ok,
        out: buf,
        ran,
        failed,
    }
}

impl Engine {
    /// Environment for recipe subprocesses: exported variables, plus the
    /// bookkeeping a sub-make needs.
    ///
    /// `-j` is deliberately stripped from `MAKEFLAGS`. Without a jobserver,
    /// passing it down would give each sub-make its own pool of N and
    /// oversubscribe the machine geometrically; serial-but-correct beats
    /// parallel-but-forkbombing. The notice in `make()` is what keeps that
    /// visible rather than mysterious.
    fn child_env(&mut self) -> Result<Vec<(String, String)>> {
        let auto = AutoVars::default();
        let names: Vec<String> = self
            .vars
            .iter()
            .filter(|(_, v)| v.exported || self.vars.export_all)
            .map(|(k, _)| k.clone())
            .collect();
        let mut env = Vec::new();
        for n in names {
            if n == "MAKEFLAGS" || n == "MAKELEVEL" {
                continue;
            }
            let v = self.var_value(&n, &auto)?;
            env.push((n, v));
        }
        let level: u32 = self.vars.value("MAKELEVEL").parse().unwrap_or(0);
        env.push(("MAKELEVEL".to_string(), (level + 1).to_string()));

        let mut flags = String::new();
        if self.opts.keep_going {
            flags.push('k');
        }
        if self.opts.dry_run {
            flags.push('n');
        }
        if self.opts.silent {
            flags.push('s');
        }
        if self.opts.ignore_errors {
            flags.push('i');
        }
        if self.opts.no_builtin_rules {
            flags.push('r');
        }
        if self.opts.no_builtin_vars {
            flags.push('R');
        }
        env.push(("MAKEFLAGS".to_string(), flags));
        Ok(env)
    }

    /// Expand a node's recipes and decide whether it has work to do.
    fn prepare(
        &mut self,
        graph: &mut Graph,
        idx: usize,
        env: &[(String, String)],
    ) -> Result<Option<Job>> {
        let shell = match self.vars.value("SHELL") {
            "" => "/bin/sh".to_string(),
            s => s.to_string(),
        };
        let capture = self.opts.jobs > 1;
        let target = graph.nodes[idx].target.clone();
        let silent_target = self.rules.silent_targets.contains(&target);
        let ignore_target = self.rules.ignore_targets.contains(&target);
        let mut cmds = Vec::new();

        for entry in 0..graph.nodes[idx].entries.len() {
            let (stale, newer) = self.staleness(graph, idx, entry);
            if !stale {
                continue;
            }
            let auto = self.auto_vars(graph, idx, entry, &newer);
            let tvars = graph.nodes[idx].tvars.clone();
            let saved = self.push_target_vars(&tvars);
            let lines = graph.nodes[idx].entries[entry].recipe.clone();
            self.loc = graph.nodes[idx].entries[entry].loc.clone();
            let mut expanded = Vec::new();
            for line in lines {
                match self.expand(&line, &auto) {
                    Ok(t) => expanded.push(t),
                    Err(e) => {
                        self.pop_target_vars(saved);
                        return Err(e);
                    }
                }
            }
            self.pop_target_vars(saved);

            for text in expanded {
                // Prefixes are read after expansion, so a prefix supplied by a
                // variable works the same as a literal one.
                let mut rest = text.trim_start();
                let (mut at, mut dash, mut plus) = (false, false, false);
                loop {
                    match rest.chars().next() {
                        Some('@') => at = true,
                        Some('-') => dash = true,
                        Some('+') => plus = true,
                        _ => break,
                    }
                    rest = &rest[1..];
                }
                let body = rest.to_string();
                if body.trim().is_empty() {
                    continue;
                }
                cmds.push(Cmd {
                    // `-n` exists to show what would run, so it overrides `@`.
                    // `-s` and `.SILENT` still win: they say "never print this",
                    // not "do not print it while building".
                    echo: !(self.opts.silent || silent_target) && (self.opts.dry_run || !at),
                    ignore: dash || self.opts.ignore_errors || ignore_target,
                    execute: !self.opts.dry_run || plus,
                    text: body,
                });
            }
        }

        if cmds.is_empty() {
            return Ok(None);
        }
        Ok(Some(Job {
            idx,
            cmds,
            shell,
            env: env.to_vec(),
            capture,
        }))
    }

    /// Build one goal. Returns whether any node under it was remade.
    ///
    /// Goals are built one at a time, sharing `state` across calls, because
    /// that is what GNU make does and it is visible: a goal already remade as
    /// a prerequisite of an earlier goal reports "Nothing to be done" rather
    /// than being rebuilt or silently skipped.
    pub fn build_goal(
        &mut self,
        graph: &mut Graph,
        state: &mut [State],
        root: usize,
    ) -> Result<bool> {
        let mask = reachable(graph, root);
        let env = self.child_env()?;
        let jobs = if self.rules.not_parallel {
            1
        } else {
            self.opts.jobs.max(1)
        };
        let mut any_ran = false;
        let mut first_error: Option<Error> = None;

        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (done_tx, done_rx) = mpsc::channel::<Finished>();
        let job_rx = std::sync::Arc::new(std::sync::Mutex::new(job_rx));
        let mut workers = Vec::new();
        if jobs > 1 {
            for _ in 0..jobs {
                let rx = job_rx.clone();
                let tx = done_tx.clone();
                workers.push(std::thread::spawn(move || {
                    loop {
                        let job = {
                            let guard = rx.lock().expect("job queue poisoned");
                            guard.recv()
                        };
                        let Ok(job) = job else { break };
                        if tx.send(execute(job)).is_err() {
                            break;
                        }
                    }
                }));
            }
        }
        drop(done_tx);

        let mut inflight = 0usize;
        loop {
            let halted = first_error.is_some() && !self.opts.keep_going;

            // Dispatch every node whose prerequisites have settled.
            while inflight < jobs
                && let Some(idx) = next_ready(graph, state, &mask, halted)
            {
                if deps_of(graph, idx).any(|d| state[d] == State::Failed) {
                    state[idx] = State::Failed;
                    continue;
                }
                match self.prepare(graph, idx, &env) {
                    Err(e) => {
                        state[idx] = State::Failed;
                        first_error.get_or_insert(e);
                    }
                    Ok(None) => state[idx] = State::Done,
                    Ok(Some(job)) => {
                        state[idx] = State::Running;
                        any_ran = true;
                        if jobs > 1 {
                            job_tx.send(job).expect("workers outlive dispatch");
                            inflight += 1;
                        } else {
                            let fin = execute(job);
                            self.settle(graph, state, fin, &mut first_error);
                        }
                    }
                }
                if first_error.is_some() && !self.opts.keep_going {
                    break;
                }
            }

            if inflight == 0 {
                // Nothing running and nothing dispatchable: this goal is
                // finished, or everything left is blocked behind a failure.
                let halted = first_error.is_some() && !self.opts.keep_going;
                if next_ready(graph, state, &mask, halted).is_none() {
                    break;
                }
                continue;
            }

            let fin = done_rx.recv().expect("a job is in flight");
            inflight -= 1;
            if !fin.out.is_empty() {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&fin.out);
            }
            self.settle(graph, state, fin, &mut first_error);
        }

        drop(job_tx);
        for w in workers {
            let _ = w.join();
        }

        match first_error {
            Some(e) => Err(e),
            None if state[root] == State::Failed => Err(Error::new(format!(
                "target '{}' not remade",
                graph.nodes[root].target
            ))),
            None => Ok(any_ran),
        }
    }

    fn settle(
        &mut self,
        graph: &mut Graph,
        state: &mut [State],
        fin: Finished,
        first_error: &mut Option<Error>,
    ) {
        let idx = fin.idx;
        if fin.ok {
            state[idx] = State::Done;
            // The node had a recipe and rsmake committed to it. Under `-n`
            // nothing executed, but dependents must still be told the target
            // was remade or the dry run reports a different plan than the
            // real build would follow.
            graph.nodes[idx].rebuilt = true;
            if fin.ran && !self.opts.dry_run {
                // Re-stat: a recipe that did not actually touch its target
                // must not advertise a new timestamp to its dependents.
                graph.nodes[idx].mtime = std::fs::metadata(&graph.nodes[idx].path)
                    .ok()
                    .and_then(|m| m.modified().ok());
            }
            return;
        }
        state[idx] = State::Failed;
        let node = &graph.nodes[idx];
        let (cmd, code) = fin.failed.unwrap_or_else(|| (node.target.clone(), 1));
        if self.rules.delete_on_error
            && !node.phony
            && !self.rules.precious.contains(&node.target)
            && node.path.exists()
        {
            let _ = std::fs::remove_file(&node.path);
            eprintln!("rsmake: deleting file '{}'", node.target);
        }
        let msg = format!("[{}] Error {code}", cmd.lines().next().unwrap_or(&cmd));
        if first_error.is_none() {
            *first_error = Some(Error::new(msg));
        } else {
            // Under `-k` later failures are reported as they happen; only the
            // first becomes the exit status.
            eprintln!("rsmake: {msg}");
        }
    }

    /// Top level: read the makefile, resolve goals, build them.
    /// Returns the process exit code.
    pub fn make(&mut self) -> Result<i32> {
        if let Some(dir) = self.opts.directory.clone() {
            std::env::set_current_dir(&dir)
                .map_err(|e| Error::new(format!("{}: {e}", dir.display())))?;
        }

        let path = match self.opts.makefile.clone() {
            Some(p) => p,
            None => ["makefile", "Makefile"]
                .iter()
                .map(PathBuf::from)
                .find(|p| p.exists())
                .ok_or_else(|| Error::new("no makefile found, and no target given".to_string()))?,
        };
        self.parse_file(&path)?;
        self.install_builtin_rules();

        if self.opts.jobs > 1 && self.vars.value("MAKELEVEL") != "0" {
            eprintln!(
                "rsmake: parallel sub-makes need a jobserver, which is not implemented; \
                 this sub-make runs serially"
            );
            self.opts.jobs = 1;
        }

        let goals = if self.opts.goals.is_empty() {
            match self.default_goal() {
                Some(g) => vec![g],
                None => return Err(Error::new("no targets".to_string())),
            }
        } else {
            self.opts.goals.clone()
        };

        let mut graph = Graph::default();
        let mut roots = Vec::new();
        for g in &goals {
            let mut stack = Vec::new();
            roots.push(self.discover(g, &mut graph, &mut stack, &[])?);
        }

        // `-q` answers a question and builds nothing, so it must not run the
        // scheduler at all -- a `$(shell)` in a recipe would be a side effect.
        if self.opts.question {
            let out_of_date = (0..graph.nodes.len())
                .any(|i| (0..graph.nodes[i].entries.len()).any(|e| self.staleness(&graph, i, e).0));
            return Ok(if out_of_date { 1 } else { 0 });
        }

        // Wording and quoting match GNU make's exactly. The differential
        // oracle diffs this stream, so a cosmetic difference here would show
        // up as a conformance failure on every corpus entry that builds
        // nothing, and would train the reader to ignore real diffs.
        let mut state = vec![State::Pending; graph.nodes.len()];
        let mut first_error: Option<Error> = None;
        for (&r, g) in roots.iter().zip(&goals) {
            let already_done = state[r] == State::Done;
            let ran = match self.build_goal(&mut graph, &mut state, r) {
                Ok(ran) => ran,
                Err(e) => {
                    first_error.get_or_insert(e);
                    if !self.opts.keep_going {
                        break;
                    }
                    continue;
                }
            };
            if ran && !already_done {
                continue;
            }
            let has_recipe = graph.nodes[r].entries.iter().any(|e| !e.recipe.is_empty());
            if has_recipe && !already_done {
                println!("rsmake: '{g}' is up to date.");
            } else {
                println!("rsmake: Nothing to be done for '{g}'.");
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(0),
        }
    }
}

fn deps_of(graph: &Graph, idx: usize) -> impl Iterator<Item = usize> + '_ {
    graph.nodes[idx]
        .entries
        .iter()
        .flat_map(|e| e.deps.iter().chain(e.order_only.iter()).copied())
}

/// Nodes reachable from `root`, so building one goal cannot wander into
/// another goal's subtree and report its recipes out of order.
fn reachable(graph: &Graph, root: usize) -> Vec<bool> {
    let mut seen = vec![false; graph.nodes.len()];
    let mut stack = vec![root];
    while let Some(i) = stack.pop() {
        if seen[i] {
            continue;
        }
        seen[i] = true;
        for d in deps_of(graph, i) {
            if !seen[d] {
                stack.push(d);
            }
        }
    }
    seen
}

/// The lowest-numbered pending node whose prerequisites have all settled.
///
/// Lowest-numbered, because discovery appends left to right: a rule's first
/// prerequisite gets a lower index than its second. A parent is never ready
/// before its children regardless of index, so ascending order visits
/// siblings in declaration order and a serial run emits recipes in the same
/// sequence GNU make does. That ordering is what makes `-n` diffable, so it
/// is a correctness property here rather than a cosmetic one.
fn next_ready(graph: &Graph, state: &[State], mask: &[bool], halted: bool) -> Option<usize> {
    if halted {
        return None;
    }
    (0..graph.nodes.len()).find(|&i| {
        mask[i]
            && state[i] == State::Pending
            && deps_of(graph, i).all(|d| matches!(state[d], State::Done | State::Failed))
    })
}
