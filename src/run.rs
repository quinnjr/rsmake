//! Recipe execution and job scheduling.
//!
//! Expansion happens on the scheduling thread and only fully-expanded command
//! text is handed to workers. `$(shell)` and `$(eval)` mutate the engine, so
//! expanding inside a worker would need the engine to be shared and locked;
//! this way the workers touch nothing but `std::process`.

use crate::graph::Graph;
use crate::{AutoVars, Engine, Error, Flavor, Opts, Origin, Result, Var};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

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
    /// Shared rather than cloned per job: the child environment is identical
    /// for every recipe in a run and is only ever read.
    env: Arc<Vec<(String, String)>>,
    /// Under `-j N` with `N > 1`, output is captured and flushed atomically
    /// when the job finishes, so concurrent recipes cannot interleave mid-line.
    capture: bool,
}

struct Finished {
    idx: usize,
    ok: bool,
    out: Vec<u8>,
    ran: bool,
    failed: Option<(String, String)>,
}

/// Name the signal that killed a child, or fall back to its number, so the
/// `Error N` line says what actually went wrong rather than a bare `-1`.
#[cfg(unix)]
fn signal_name(signal: Option<i32>) -> String {
    match signal {
        Some(1) => "1 (Hangup)".to_string(),
        Some(2) => "2 (Interrupt)".to_string(),
        Some(3) => "3 (Quit)".to_string(),
        Some(9) => "9 (Kill)".to_string(),
        Some(13) => "13 (Broken pipe)".to_string(),
        Some(15) => "15 (Terminated)".to_string(),
        Some(n) => format!("{n} (signal)"),
        None => "1".to_string(),
    }
}

/// The single-letter flag cluster that describes `opts`, in GNU's `MAKEFLAGS`
/// form (`n`, `ks`, `Beir`). Shared by the recipe environment and by the
/// `$(MAKEFLAGS)` a makefile can read, so the two can never disagree.
///
/// `-q` is deliberately absent: it means "answer a question about this
/// invocation", and a sub-make asked the same question would build nothing
/// while its parent believed it had. GNU does not propagate it either.
/// `-j` is absent for the reason documented at [`Engine::child_env`].
pub(crate) fn makeflags_string(opts: &Opts) -> String {
    // Letter order is GNU make 4.4.1's, measured: `-e -B -i -r` gives `Beir`,
    // `-i -R -n -k` gives `iknrR`, `-r -s -i` gives `irs`. The order is visible
    // to any makefile that reads `$(MAKEFLAGS)`, so it is not arbitrary.
    let mut flags = String::new();
    if opts.always_make {
        flags.push('B');
    }
    if opts.env_overrides {
        flags.push('e');
    }
    if opts.ignore_errors {
        flags.push('i');
    }
    if opts.keep_going {
        flags.push('k');
    }
    if opts.dry_run {
        flags.push('n');
    }
    if opts.no_builtin_rules {
        flags.push('r');
    }
    if opts.no_builtin_vars {
        flags.push('R');
    }
    if opts.silent {
        flags.push('s');
    }
    flags
}

/// Test-only seam: a recipe whose text is exactly this panics inside
/// [`execute`], so the `catch_unwind` recovery around it can be exercised
/// without a real bug to trigger it. The sentinel carries NUL bytes, which no
/// command line reaching `sh -c` can contain, and the whole check is compiled
/// out of any build that is not `cfg(test)`.
#[cfg(test)]
pub(crate) const PANIC_SENTINEL: &str = "\u{0}panic-for-test\u{0}";

/// Run a job and turn a panic inside it into an ordinary failed target.
///
/// A panicked job used to take the whole build with it: the `Finished` never
/// arrived, `inflight` never came back down, and the dispatcher blocked on
/// `recv` forever. Wrapping both the serial and the worker path here means a
/// bug in rsmake fails one target, through the failure accounting that already
/// exists, rather than hanging or aborting.
fn run_job(job: Job) -> Finished {
    let idx = job.idx;
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || execute(job))).unwrap_or_else(
        |_| Finished {
            idx,
            ok: false,
            out: Vec::new(),
            ran: true,
            failed: Some(("rsmake worker panicked".to_string(), "2".to_string())),
        },
    )
}

fn execute(job: Job) -> Finished {
    let mut buf: Vec<u8> = Vec::new();
    let mut ran = false;
    let mut failed = None;
    let mut ok = true;

    for cmd in &job.cmds {
        #[cfg(test)]
        assert!(cmd.text != PANIC_SENTINEL, "rsmake test panic seam");
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
        for (k, v) in job.env.iter() {
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
                // A child killed by a signal has no exit code. GNU make reports
                // the terminating signal by name (e.g. "Terminated"); a bare
                // `-1` would hide which signal, so embed it.
                let code = match s.code() {
                    Some(c) => c.to_string(),
                    // `signal()` is a Unix extension, like the import that
                    // brings it in; off Unix a status with no code carries no
                    // signal number to report.
                    #[cfg(unix)]
                    None => signal_name(s.signal()),
                    #[cfg(not(unix))]
                    None => "1".to_string(),
                };
                if cmd.ignore {
                    let line = format!(
                        "rsmake: [{}] Error {code} (ignored)\n",
                        cmd.text.lines().next().unwrap_or("")
                    );
                    // Under `-j` the line joins the job's captured output so it
                    // lands next to the command that produced it; GNU puts it
                    // on stderr, but with concurrent jobs a separate stream
                    // means the notice appears before the recipe it belongs to,
                    // and in-place beats stream fidelity. Serially there is
                    // nothing to interleave with, so it goes to stderr.
                    if job.capture {
                        buf.extend_from_slice(line.as_bytes());
                    } else {
                        eprint!("{line}");
                    }
                } else {
                    failed = Some((cmd.text.clone(), code));
                    ok = false;
                    break;
                }
            }
            Err(e) => {
                if !cmd.ignore {
                    failed = Some((format!("{}: {e}", job.shell), "127".to_string()));
                    ok = false;
                    break;
                }
            }
        }
    }

    // The buffer travels back with the job rather than being written here: the
    // dispatcher flushes it the instant the `Finished` arrives, so nothing is
    // held any longer, and having exactly one writer means the flush is a
    // single testable function instead of two copies that can drift.
    Finished {
        idx: job.idx,
        ok,
        out: buf,
        ran,
        failed,
    }
}

/// The flush itself, over any writer, returning the error rather than reporting
/// it — which is what makes the failure path reachable from a test.
fn flush_captured(out: &[u8], w: &mut impl Write) -> std::io::Result<()> {
    if out.is_empty() {
        return Ok(());
    }
    w.write_all(out).and_then(|()| w.flush())
}

/// Flush a finished job's captured output. Called on the dispatcher thread, for
/// both the serial and the worker paths, as soon as the job settles: a single
/// owner means whole-job blocks appear in the order the jobs finished and
/// nothing can interleave mid-line.
fn write_captured(out: &[u8]) {
    // Checked before the lock as well as inside `flush_captured`: without
    // capture every settled node carries an empty buffer, and taking the
    // stdout lock for each of them buys nothing.
    if out.is_empty() {
        return;
    }
    let mut w = std::io::stdout().lock();
    // Reported rather than swallowed: losing a recipe's whole output to a
    // closed pipe with no explanation looks like a recipe that printed
    // nothing, which is a different bug entirely.
    if let Err(e) = flush_captured(out, &mut w) {
        eprintln!("rsmake: warning: could not write captured recipe output: {e}");
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
    ///
    /// GNU passes `SHELL` into the recipe environment regardless of export;
    /// rsmake only forwards exported variables, so recipes see no `SHELL`
    /// unless the makefile exports it — a known divergence.
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
        env.push((
            "MAKELEVEL".to_string(),
            // Wrapping, because GNU make wraps: measured on 4.4.1, a hand-set
            // `MAKELEVEL=4294967295` gives the child `MAKELEVEL=0`. Saturating
            // would be tidier and would diverge, and the contract here is
            // conformance, not tidiness.
            self.make_level.wrapping_add(1).to_string(),
        ));

        // Everything else the parent was told, the child is told.
        env.push(("MAKEFLAGS".to_string(), makeflags_string(&self.opts)));
        Ok(env)
    }

    /// Expand a node's recipes and decide whether it has work to do.
    fn prepare(
        &mut self,
        graph: &mut Graph,
        idx: usize,
        env: &Arc<Vec<(String, String)>>,
    ) -> Result<Option<Job>> {
        // `SHELL` is expanded like any other variable: `SHELL = $(BASH)` names
        // a program, not a literal `$(BASH)` to hand to exec.
        let raw = self.vars.value("SHELL").to_string();
        let shell = match self.expand(&raw, &AutoVars::default())?.trim() {
            "" => "/bin/sh".to_string(),
            s => s.to_string(),
        };
        let capture = self.opts.jobs > 1;
        let target = graph.nodes[idx].target.clone();
        let silent_target =
            self.rules.silent_all || self.rules.silent_targets.contains(&target);
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
                // GNU make §5.7.1: a recipe line that mentions the `MAKE`
                // variable runs even under `-n`, as if it were `+`-prefixed,
                // so a dry run recurses and shows the whole build's plan
                // rather than stopping at the top level. The test is on the
                // line as written, before expansion, exactly as GNU's is.
                let refs_make = line.contains("$(MAKE)") || line.contains("${MAKE}");
                match self.expand(&line, &auto) {
                    Ok(t) => expanded.push((t, refs_make)),
                    Err(e) => {
                        self.pop_target_vars(saved);
                        return Err(e);
                    }
                }
            }
            self.pop_target_vars(saved);

            for (text, refs_make) in expanded {
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
                    // `-n` exists to show what would run, so it overrides both
                    // `@` and silencing: GNU prints the recipe under `-s -n`
                    // too, because the whole point of the run is to be told.
                    echo: self.opts.dry_run || (!(self.opts.silent || silent_target) && !at),
                    ignore: dash || self.opts.ignore_errors || ignore_target,
                    execute: !self.opts.dry_run || plus || refs_make,
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
            env: Arc::clone(env),
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
        env: &Arc<Vec<(String, String)>>,
    ) -> Result<bool> {
        let mask = reachable(graph, root);
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
                        }; // guard is dropped here, before execute() can panic, so a panic cannot poison the queue mutex
                        let Ok(job) = job else { break };
                        if tx.send(run_job(job)).is_err() {
                            break;
                        }
                    }
                }));
            }
        }
        drop(done_tx);

        // A settled node decrements the outstanding-prerequisite count of each
        // of its dependents; when one reaches zero it is ready. A min-heap by
        // index yields the lowest-numbered ready node in O(log N), replacing
        // the O(N) rescan-from-zero the dispatcher used to do on every settle.
        //
        // Only prerequisites that have *not* already settled are counted.
        // `state` is shared across goals, so a node finished while building an
        // earlier goal will never be settled again; counting it here would
        // leave its dependents permanently short of zero, and the second goal
        // would be skipped in silence and reported up to date.
        let settled = |s: State| matches!(s, State::Done | State::Failed);
        let mut unsettled: Vec<usize> = (0..graph.nodes.len())
            .map(|i| deps_of(graph, i).filter(|&d| !settled(state[d])).count())
            .collect();
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
        for i in 0..graph.nodes.len() {
            for d in deps_of(graph, i) {
                dependents[d].push(i);
            }
        }
        let mut ready: BTreeSet<usize> = (0..graph.nodes.len())
            .filter(|&i| mask[i] && state[i] == State::Pending && unsettled[i] == 0)
            .collect();

        // Mark `idx` settled and release whichever of its dependents are now
        // ready, unless they sit behind a failed prerequisite.
        fn release_ready_dependents(
            dependents: &[Vec<usize>],
            ready: &mut BTreeSet<usize>,
            mask: &[bool],
            state: &[State],
            unsettled: &mut [usize],
            idx: usize,
        ) {
            for &d in &dependents[idx] {
                if mask[d] && state[d] == State::Pending {
                    // The counter *is* the invariant: it exists precisely so
                    // that releasing a node costs O(dependents) instead of a
                    // rescan of its whole prerequisite list.
                    unsettled[d] -= 1;
                    if unsettled[d] == 0 {
                        ready.insert(d);
                    }
                }
            }
        }

        // The nodes handed to workers and not yet settled, rather than a bare
        // count: if the worker pool dies with jobs in flight, those nodes are
        // still `Running`, and `state` outlives this call. A later goal sharing
        // it would treat "not Pending" as "already settled", walk straight past
        // the never-finished chain and report success. Keeping the indices lets
        // the disconnect path settle them as failures.
        let mut inflight: Vec<usize> = Vec::new();
        loop {
            let halted = first_error.is_some() && !self.opts.keep_going;

            // Dispatch every node whose prerequisites have settled.
            while inflight.len() < jobs && !halted {
                let Some(&idx) = ready.iter().next() else { break };
                ready.remove(&idx);
                if state[idx] != State::Pending {
                    continue;
                }
                if deps_of(graph, idx).any(|d| state[d] == State::Failed) {
                    state[idx] = State::Failed;
                    release_ready_dependents(&dependents, &mut ready, &mask, state, &mut unsettled, idx);
                    continue;
                }
                match self.prepare(graph, idx, env) {
                    Err(e) => {
                        state[idx] = State::Failed;
                        release_ready_dependents(&dependents, &mut ready, &mask, state, &mut unsettled, idx);
                        first_error.get_or_insert(e);
                    }
                    Ok(None) => {
                        state[idx] = State::Done;
                        release_ready_dependents(&dependents, &mut ready, &mask, state, &mut unsettled, idx);
                    }
                    Ok(Some(job)) => {
                        state[idx] = State::Running;
                        any_ran = true;
                        if jobs > 1 {
                            job_tx.send(job).expect("workers outlive dispatch");
                            inflight.push(idx);
                        } else {
                            let fin = run_job(job);
                            let fin_idx = fin.idx;
                            // Same as the `recv` path below: a serial job that
                            // captured its output (`.NOTPARALLEL` under `-j`)
                            // would otherwise have it dropped on the floor.
                            write_captured(&fin.out);
                            self.settle(graph, state, fin, &mut first_error);
                            release_ready_dependents(&dependents, &mut ready, &mask, state, &mut unsettled, fin_idx);
                        }
                    }
                }
                if first_error.is_some() && !self.opts.keep_going {
                    break;
                }
            }

            if inflight.is_empty() {
                // Nothing running and nothing dispatchable: this goal is
                // finished, or everything left is blocked behind a failure.
                // `halted` must break too, not spin: the dispatch loop above
                // refuses to touch `ready` once a recipe has failed without
                // `-k`, so with siblings still queued this arm would `continue`
                // forever at 100% CPU instead of exiting 2.
                if ready.is_empty() || halted {
                    break;
                }
                continue;
            }

            // A `recv` error means every worker is gone while a job is still
            // in flight -- the channel cannot close otherwise. Blocking on
            // `expect` here would abort mid-build with a panic message about
            // rsmake's internals; failing the goal says what happened and lets
            // the normal exit path run.
            let Ok(fin) = done_rx.recv() else {
                first_error.get_or_insert_with(|| {
                    Error::new("worker thread died with a job still in flight")
                });
                // Settle what will never settle itself, so a later goal sharing
                // `state` sees a failure rather than a node stuck `Running`.
                for &i in &inflight {
                    state[i] = State::Failed;
                }
                break;
            };
            inflight.retain(|&i| i != fin.idx);
            write_captured(&fin.out);
            let fin_idx = fin.idx;
            self.settle(graph, state, fin, &mut first_error);
            release_ready_dependents(&dependents, &mut ready, &mask, state, &mut unsettled, fin_idx);
        }

        drop(job_tx);
        for w in workers {
            // A worker that panicked outside `execute` has no job to blame, so
            // it is reported here rather than lost: a silent `let _ = join()`
            // turned every such bug into "the build did nothing".
            if let Err(payload) = w.join() {
                let what = payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                eprintln!("rsmake: warning: worker thread panicked: {what}");
            }
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
        let (cmd, code) = fin.failed.unwrap_or_else(|| (node.target.clone(), "1".to_string()));
        if self.rules.delete_on_error
            && !node.phony
            && !self.rules.precious.contains(&node.target)
            && node.path.exists()
        {
            // Announced only if it actually happened: claiming to have deleted
            // a half-written target that is still on disk sends the reader
            // looking for a different bug entirely.
            match std::fs::remove_file(&node.path) {
                Ok(()) => eprintln!("rsmake: deleting file '{}'", node.target),
                Err(e) => eprintln!(
                    "rsmake: warning: could not delete '{}': {e}",
                    node.target
                ),
            }
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
        // Each `-C` is entered from where the previous one left off, so
        // `-C x -C y` lands in `x/y` exactly as GNU make does; last-wins would
        // have quietly built the wrong tree.
        for dir in self.opts.directory.clone() {
            std::env::set_current_dir(&dir)
                .map_err(|e| Error::new(format!("{}: {e}", dir.display())))?;
        }

        // Every `-f` is read, in order: GNU make accumulates them, and the
        // default goal comes from the first one.
        let paths: Vec<PathBuf> = match self.opts.makefile.clone() {
            v if !v.is_empty() => v,
            _ => vec![
                ["makefile", "Makefile"]
                    .iter()
                    .map(PathBuf::from)
                    .find(|p| p.exists())
                    .ok_or_else(|| {
                        Error::new("no makefile found, and no target given".to_string())
                    })?,
            ],
        };
        let makeflags_at_parse = self.vars.value("MAKEFLAGS").to_string();
        for path in &paths {
            self.parse_file(path)?;
        }
        self.install_builtin_rules();

        // Recompute now that parsing is done: `.IGNORE:` sets `ignore_errors`
        // while the makefile is being read, and GNU shows the resulting `i` in
        // both `$(MAKEFLAGS)` as seen after the parse and in every recipe's
        // environment. Freezing the value at construction left the flag out of
        // both. Skipped if the makefile assigned `MAKEFLAGS` itself — that
        // assignment is the makefile's answer and must not be overwritten.
        if self.vars.value("MAKEFLAGS") == makeflags_at_parse {
            self.vars.set(
                "MAKEFLAGS",
                Var {
                    value: makeflags_string(&self.opts),
                    flavor: Flavor::Recursive,
                    origin: Origin::File,
                    exported: false,
                },
            );
        }

        if self.opts.jobs > 1 && self.make_level != 0 {
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
        // GNU still runs `+`-prefixed lines under `-q`; rsmake does not run any
        // recipe here -- a known divergence.
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
        // Computed once for the whole run rather than once per goal: building
        // it re-expands every exported variable, so a `$(shell)` on the right
        // of an exported `=` used to run again for every goal on the command
        // line. Nothing between goals can change it -- MAKELEVEL and MAKEFLAGS
        // are fixed at engine construction, and a recipe cannot assign to the
        // parent's variables.
        let env = Arc::new(self.child_env()?);
        // GNU's end-of-run summary under `-k`: the build did not stop at the
        // failure, so without these lines the reader has to reconstruct which
        // goals were abandoned from a scroll of interleaved errors. Collected
        // here rather than printed, because GNU emits them at the very end of
        // the run -- after the `*** ... Error N` line, which this function
        // returns rather than prints. [`Engine::not_remade`] carries them out
        // to `main`, which prints them last.
        let mut not_remade: Vec<String> = Vec::new();
        for (&r, g) in roots.iter().zip(&goals) {
            let already_done = state[r] == State::Done;
            let ran = match self.build_goal(&mut graph, &mut state, r, &env) {
                Ok(ran) => ran,
                Err(e) => {
                    first_error.get_or_insert(e);
                    if self.opts.keep_going {
                        // Only when the goal was *blocked*, never when its own
                        // recipe failed: GNU prints the failure line alone for
                        // `make -k a` where `a`'s recipe exits non-zero, and
                        // adds this line only when a prerequisite of `a` is what
                        // failed. The test is therefore on the prerequisites,
                        // not on `a`'s own state.
                        let blocked = state[r] == State::Failed
                            && deps_of(&graph, r).any(|d| state[d] == State::Failed);
                        if blocked {
                            not_remade.push(graph.nodes[r].target.clone());
                        }
                        continue;
                    }
                    break;
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
        // Handed to the caller rather than printed here: GNU prints the summary
        // *after* the `*** ... Error N` line, and `make()` returns that error
        // for `main` to print, so printing here would put the summary above it.
        // It is only ever non-empty when `first_error` is set, so there is no
        // path where the caller returns `Ok` with lines left unprinted.
        self.not_remade = not_remade;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Opts;

    /// The three orderings the [`makeflags_string`] doc comment claims were
    /// measured against GNU make 4.4.1. Asserted here so a letter reordered for
    /// tidiness cannot silently change what every makefile reading
    /// `$(MAKEFLAGS)` sees.
    #[test]
    fn makeflags_letter_order_matches_gnu() {
        let opts = |f: &dyn Fn(&mut Opts)| {
            let mut o = Opts::default();
            f(&mut o);
            makeflags_string(&o)
        };
        assert_eq!(
            opts(&|o| {
                o.env_overrides = true;
                o.always_make = true;
                o.ignore_errors = true;
                o.no_builtin_rules = true;
            }),
            "Beir"
        );
        // `-R` implies `-r`, which is why an `r` appears without being asked
        // for; GNU prints `iknrR` for `-i -R -n -k`.
        assert_eq!(
            opts(&|o| {
                o.ignore_errors = true;
                o.no_builtin_vars = true;
                o.no_builtin_rules = true;
                o.dry_run = true;
                o.keep_going = true;
            }),
            "iknrR"
        );
        assert_eq!(
            opts(&|o| {
                o.no_builtin_rules = true;
                o.silent = true;
                o.ignore_errors = true;
            }),
            "irs"
        );
    }

    /// `.SILENT:` silences recipes without putting `s` in `MAKEFLAGS`; `-s`
    /// does both. Measured against GNU 4.4.1.
    #[test]
    fn silent_from_makefile_does_not_reach_makeflags() {
        let mut e = Engine::new(Opts::default());
        e.parse_text(".SILENT:\nall:\n\t@true\n", "t").unwrap();
        assert!(e.rules.silent_all);
        assert!(!e.opts.silent);
        assert_eq!(makeflags_string(&e.opts), "");
    }

    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no room"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The failure branch of the capture flush is the whole reason
    /// [`flush_captured`] returns its error instead of reporting it: with the
    /// `eprintln!` inlined there was no way to reach it from a test at all.
    #[test]
    fn flush_captured_surfaces_write_errors() {
        assert!(flush_captured(b"anything", &mut FailingWriter).is_err());
        // An empty buffer must not touch the writer, or every job without
        // output would report a spurious warning.
        assert!(flush_captured(b"", &mut FailingWriter).is_ok());
        let mut sink: Vec<u8> = Vec::new();
        assert!(flush_captured(b"hi", &mut sink).is_ok());
        assert_eq!(sink, b"hi");
    }

    /// Build `all: boom ok` under `-k`, where `boom`'s recipe panics inside the
    /// worker. The build must not hang, `boom` must fail, and `ok` must still
    /// be built.
    fn panicking_build(jobs: usize) {
        let text = format!(
            ".PHONY: all boom ok\nall: boom ok\nboom:\n\t@{PANIC_SENTINEL}\nok:\n\t@true\n"
        );
        let mut e = Engine::new(Opts {
            jobs,
            keep_going: true,
            ..Opts::default()
        });
        e.parse_text(&text, "t").unwrap();
        let mut graph = Graph::default();
        let mut stack = Vec::new();
        let root = e.discover("all", &mut graph, &mut stack, &[]).unwrap();
        let idx = |t: &str| graph.nodes.iter().position(|n| n.target == t).unwrap();
        let (boom, ok) = (idx("boom"), idx("ok"));
        let env = Arc::new(e.child_env().unwrap());
        let mut state = vec![State::Pending; graph.nodes.len()];

        let err = e
            .build_goal(&mut graph, &mut state, root, &env)
            .expect_err("a panicking recipe must fail its target");
        assert!(
            err.to_string().contains("rsmake worker panicked"),
            "panic should be synthesised into the target's error, got {err}"
        );
        assert_eq!(state[boom], State::Failed);
        // `-k`: the sibling has nothing to do with the panic and must still run.
        assert_eq!(state[ok], State::Done);
        assert_eq!(state[root], State::Failed);
    }

    #[test]
    fn panicking_recipe_is_contained_in_parallel() {
        panicking_build(2);
    }

    #[test]
    fn panicking_recipe_is_contained_serially() {
        panicking_build(1);
    }
}
