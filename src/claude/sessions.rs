//! Live Claude Code session discovery and liveness (FR-1).
//!
//! This module answers a single question: *which Claude Code engine processes are
//! running right now, and for each, what is its session id, cwd, project, branch,
//! start time, version, and transcript file?* It returns a deduped
//! [`Vec<LiveSession>`] that the aggregator turns into the Discord card.
//!
//! ## How (and why this way — all verified, see `specs/research-dossier.json` B1)
//!
//! 1. **Enumerate by executable path, never `pgrep -f`.** Engine argv is enormous
//!    (a long `--plugin-dir` list) and macOS truncates the buffer `pgrep -f`
//!    matches against, so it *intermittently misses live sessions*. Instead we
//!    scan every process via `sysinfo` and keep only those whose `exe()` contains
//!    `/claude-code/` and ends with `/MacOS/claude` ([`is_engine_exe`]). That
//!    cleanly excludes the `Helpers/disclaimer` wrapper, the `Claude.app` GUI
//!    (`/MacOS/Claude`, capital C), and Discord.
//!    **sysinfo gotcha:** `exe()`/`cwd()` are `None` unless the refresh requests
//!    them — we refresh with
//!    `ProcessRefreshKind::nothing().with_exe(Always).with_cwd(Always).with_cpu()`
//!    over `ProcessesToUpdate::All` before reading anything.
//! 2. **Source of truth = the registry + `kill(pid,0)`.** For each engine PID we
//!    read `~/.claude/sessions/<PID>.json` (the authoritative session index) and
//!    confirm the process is alive with signal 0; stale/dead registry files are
//!    discarded (FR-1/AC-2).
//! 3. **cwd via sysinfo, fallback to `lsof`.** argv carries no `--cwd`. We take
//!    `sysinfo`'s cwd, falling back to `platform::macos::cwd_via_lsof` when it is
//!    empty (FR-1/AC-3); the registry cwd is a last resort.
//! 4. **Transcript via cwd→slug→newest-mtime, cross-checked.** The engine does
//!    **not** hold its `<sessionId>.jsonl` open, so we cannot find it by open fd.
//!    We map cwd → project slug → newest-mtime `*.jsonl` in
//!    `~/.claude/projects/<slug>/` and cross-check that the registry `sessionId`
//!    matches the chosen file's stem; we never trust the argv `--resume` /
//!    `--session-id` ids (verified stale on forked sessions).
//!
//! The public surface is consumed by the transcript collector and aggregator.

use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid as NixPid;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use tracing::{debug, warn};

use crate::claude::schema::{parse_session_registry, SessionRegistry};
use crate::error::Result;
use crate::platform::macos;

/// A live, deduped Claude Code session ready for the aggregator (FR-1/AC-4).
///
/// Built only from sources verified trustworthy: the `sessions/<PID>.json`
/// registry (session id, start time, version), the live process (pid, cwd), and
/// the correlated transcript file (project, branch when read later). `branch` is
/// `None` here — it is filled by the transcript collector (FR-2/AC-3); it is
/// carried on the struct so the type matches design §4.2's `SessionState`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    /// Engine process id (the `<PID>` the registry file is named for).
    pub pid: i32,
    /// The session's own id — equals the transcript filename stem. Never the
    /// argv `--resume`/`--session-id` value (verified stale).
    pub session_id: String,
    /// Resolved *live* working directory of the engine (where the user currently
    /// is — drives the card's project label). May differ from the startup dir if
    /// the session `cd`'d; use [`Self::transcript`] for the transcript file, not
    /// a slug derived from this.
    pub cwd: PathBuf,
    /// Human-friendly project name (the final path component of `cwd`).
    pub project_name: String,
    /// The `<sessionId>.jsonl` transcript path, resolved against the startup dir
    /// (not just the live cwd) so a session that `cd`'d away from where it started
    /// still gets a watcher (FR-1/AC-3). `None` until the transcript exists.
    pub transcript: Option<PathBuf>,
    /// Git branch — filled by the transcript collector, not known here.
    pub branch: Option<String>,
    /// Session start as epoch **milliseconds** (feeds the elapsed timer,
    /// FR-5/AC-4); `None` when the registry omitted it.
    pub started_at: Option<i64>,
    /// CLI version that wrote the registry file (drives the schema version gate).
    pub version: Option<String>,
}

/// Substring every engine executable path contains.
const ENGINE_EXE_MARKER: &str = "/claude-code/";
/// Suffix every engine executable path ends with. The `Helpers/disclaimer`
/// wrapper, the `Claude.app` GUI (`/MacOS/Claude`, capital C) and Discord all
/// fail this check.
const ENGINE_EXE_SUFFIX: &str = "/MacOS/claude";

/// True iff `exe` is a Claude Code **engine** binary (FR-1/AC-1).
///
/// The path must contain `/claude-code/` *and* end with `/MacOS/claude`
/// (lowercase). This is the verified-reliable discriminator: it matches the
/// engine spawned under
/// `…/claude-code/<ver>/claude.app/Contents/MacOS/claude` and rejects the
/// disclaimer wrapper, the capital-C GUI binary, and every Discord process.
pub fn is_engine_exe(exe: &Path) -> bool {
    let s = exe.to_string_lossy();
    s.contains(ENGINE_EXE_MARKER) && s.ends_with(ENGINE_EXE_SUFFIX)
}

/// True iff a process with `pid` is alive, via `kill(pid, 0)` (FR-1/AC-2).
///
/// Signal 0 performs the kernel's permission/existence checks without delivering
/// a signal. `Ok` ⇒ alive; `EPERM` ⇒ alive but owned by another user (still a
/// live process); `ESRCH` ⇒ no such process (dead). Any other errno is treated
/// conservatively as not-alive.
pub fn is_alive(pid: i32) -> bool {
    matches!(
        kill(NixPid::from_raw(pid), None),
        Ok(()) | Err(Errno::EPERM)
    )
}

/// Enumerate live engine processes and their resolved cwd (FR-1/AC-1, AC-3).
///
/// Returns `(pid, cwd)` pairs. cwd is taken from `sysinfo` and, if empty, from
/// the `lsof` fallback; a PID whose cwd cannot be resolved by either is dropped
/// (it cannot be mapped to a transcript). Liveness is *not* filtered here — the
/// registry pass owns that — but `sysinfo`'s enumeration is inherently of running
/// processes.
fn enumerate_engines() -> Vec<(i32, Option<PathBuf>)> {
    let mut system = System::new();
    // sysinfo gotcha: exe()/cwd() are None unless explicitly requested. cpu is
    // requested too (a later collector corroborates busy/idle with CPU%).
    let refresh = ProcessRefreshKind::nothing()
        .with_exe(UpdateKind::Always)
        .with_cwd(UpdateKind::Always)
        .with_cpu();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    let mut engines = Vec::new();
    for (pid, proc_) in system.processes() {
        let Some(exe) = proc_.exe() else { continue };
        if !is_engine_exe(exe) {
            continue;
        }
        let pid = pid.as_u32() as i32;
        let cwd = proc_
            .cwd()
            .filter(|c| !c.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| macos::cwd_via_lsof(pid));
        engines.push((pid, cwd));
    }
    engines
}

/// Read and parse `~/.claude/sessions/<PID>.json`, if present.
fn read_registry(sessions_dir: &Path, pid: i32) -> Option<SessionRegistry> {
    let path = sessions_dir.join(format!("{pid}.json"));
    let bytes = std::fs::read(&path).ok()?;
    match parse_session_registry(&bytes) {
        Ok(reg) => Some(reg),
        Err(_) => {
            // A structurally-broken registry file is skipped, not fatal (C-4).
            warn!(pid, "skipping unparseable sessions/<PID>.json");
            None
        }
    }
}

/// Locate a session's `<sessionId>.jsonl` transcript, tolerant of the session
/// having `cd`'d away from the directory it started in (FR-1/AC-3).
///
/// Claude Code writes the transcript under the project slug of the session's
/// **startup** cwd and never moves it when the user changes directory, so the
/// live process cwd can resolve to the wrong slug dir. We therefore try each
/// candidate cwd's slug in turn — typically live cwd first (the common no-`cd`
/// case), then the registry's recorded startup cwd — and match the **exact**
/// `<sessionId>.jsonl` file.
///
/// The sessionId is the authoritative transcript stem, so we never fall back to
/// newest-mtime: a single project slug dir holds **many** sessions' transcripts
/// (e.g. every `$HOME`-rooted session shares one dir), and newest-mtime could
/// return a *different* session's file — surfacing the wrong tokens/model on the
/// card. Returning `None` (retry next tick) is correct when no exact match exists
/// yet.
fn resolve_transcript(projects_dir: &Path, session_id: &str, cwds: &[&Path]) -> Option<PathBuf> {
    let mut tried: Vec<String> = Vec::new();
    for cwd in cwds {
        let slug = macos::project_slug(cwd);
        if tried.iter().any(|s| s == &slug) {
            continue;
        }
        let candidate = projects_dir.join(&slug).join(format!("{session_id}.jsonl"));
        if candidate.exists() {
            return Some(candidate);
        }
        tried.push(slug);
    }
    None
}

/// Final path component of a cwd as a display project name (e.g. `private`).
fn project_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned())
}

/// Discover all live Claude Code sessions on this machine (FR-1).
///
/// Enumerates engines by exe-path filter, joins each to its
/// `~/.claude/sessions/<PID>.json` registry, prunes stale/dead entries via
/// `kill(pid,0)`, resolves cwd, and correlates the transcript by
/// cwd→slug→newest-mtime (cross-checking `sessionId`). The result is deduped by
/// session id. Errors resolving an individual session are logged and skipped, not
/// propagated — a single unreadable process must never blank the whole card
/// (NFR-2). The only `Err` returned is failure to resolve the `~/.claude` root.
pub fn discover() -> Result<Vec<LiveSession>> {
    let sessions_dir = macos::sessions_dir()?;
    let projects_dir = macos::projects_dir()?;
    Ok(discover_in(
        &sessions_dir,
        &projects_dir,
        enumerate_engines(),
    ))
}

/// Core of [`discover`], with directories and the enumerated engine set injected
/// so it can be unit-tested against a temp filesystem without real processes.
fn discover_in(
    sessions_dir: &Path,
    projects_dir: &Path,
    engines: Vec<(i32, Option<PathBuf>)>,
) -> Vec<LiveSession> {
    let mut sessions: Vec<LiveSession> = Vec::new();
    for (pid, sysinfo_cwd) in engines {
        // Liveness gate (FR-1/AC-2): drop processes that died between enumeration
        // and now, so a racing exit cannot leave a phantom session on the card.
        if !is_alive(pid) {
            continue;
        }
        let Some(registry) = read_registry(sessions_dir, pid) else {
            debug!(pid, "no readable sessions/<PID>.json for live engine");
            continue;
        };
        let Some(session_id) = registry.session_id.clone() else {
            debug!(pid, "registry missing sessionId; skipping");
            continue;
        };

        let live_cwd = sysinfo_cwd.filter(|c| !c.as_os_str().is_empty());
        let registry_cwd = registry
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .filter(|c| !c.as_os_str().is_empty());

        // Display cwd: live process cwd (sysinfo/lsof) preferred — it reflects the
        // user's *current* project for the card label — with the registry's
        // startup cwd as a fallback.
        let Some(cwd) = live_cwd.clone().or_else(|| registry_cwd.clone()) else {
            debug!(pid, "could not resolve cwd for live engine; skipping");
            continue;
        };

        // Transcript: resolved against the startup dir, not just the live cwd, so
        // a session that `cd`'d away from where it started still gets a watcher
        // and keeps contributing its tokens to the combined card. Try the live
        // cwd's slug first, then the registry's startup cwd.
        let candidates: Vec<&Path> = live_cwd
            .as_deref()
            .into_iter()
            .chain(registry_cwd.as_deref())
            .collect();
        let transcript = resolve_transcript(projects_dir, &session_id, &candidates);
        if transcript.is_none() {
            // No transcript yet (brand-new session, or it has not written its
            // first line): still a live session; it gains a watcher on a later
            // discovery tick once the file appears.
            debug!(pid, "no transcript file found for session yet");
        }

        let session = LiveSession {
            pid,
            project_name: project_name(&cwd),
            cwd,
            transcript,
            session_id,
            branch: None,
            started_at: registry.started_at,
            version: registry.version.clone(),
        };

        // Dedup by session id (FR-1/AC-4): a session must appear once even if two
        // registry files somehow point at the same id.
        if sessions.iter().any(|s| s.session_id == session.session_id) {
            continue;
        }
        sessions.push(session);
    }
    sessions
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Live smoke (ignored by default; run with `--ignored` on the real Mac).
    #[test]
    #[ignore]
    fn smoke_discover_live() {
        let sessions = discover().expect("discover ok");
        eprintln!("discovered {} live session(s)", sessions.len());
        for s in &sessions {
            eprintln!(
                "  pid={} id={} project={} cwd={} ver={:?}",
                s.pid,
                s.session_id,
                s.project_name,
                s.cwd.display(),
                s.version
            );
        }
    }

    // ---- exe-path filter (FR-1/AC-1) -----------------------------------------

    #[test]
    fn engine_exe_accepts_the_real_cli_path() {
        // The verified live engine path (dossier lane B1).
        let exe = Path::new(
            "/Users/x/Library/Application Support/Claude/claude-code/2.1.181/claude.app/Contents/MacOS/claude",
        );
        assert!(is_engine_exe(exe));
    }

    #[test]
    fn engine_exe_rejects_wrapper_gui_and_discord() {
        // The disclaimer wrapper: under claude-code? no — different path, and it
        // does not end /MacOS/claude.
        assert!(!is_engine_exe(Path::new(
            "/Applications/Claude.app/Contents/Helpers/disclaimer"
        )));
        // The GUI binary: ends /MacOS/Claude (capital C), and is not under
        // /claude-code/.
        assert!(!is_engine_exe(Path::new(
            "/Applications/Claude.app/Contents/MacOS/Claude"
        )));
        // Discord and an unrelated binary.
        assert!(!is_engine_exe(Path::new(
            "/Applications/Discord.app/Contents/MacOS/Discord"
        )));
        assert!(!is_engine_exe(Path::new(
            "/opt/homebrew/bin/xcode-discord-rpc"
        )));
    }

    #[test]
    fn engine_exe_requires_both_marker_and_suffix() {
        // Has the marker but wrong suffix (a sibling helper under claude-code).
        assert!(!is_engine_exe(Path::new(
            "/x/claude-code/2.1.181/claude.app/Contents/MacOS/Claude"
        )));
        // Ends correctly but is not under /claude-code/.
        assert!(!is_engine_exe(Path::new("/usr/local/bin/foo/MacOS/claude")));
    }

    // ---- exact-stem transcript resolution (FR-1/AC-3) ------------------------

    /// Write a `<stem>.jsonl` file with trivial contents.
    fn touch_jsonl(dir: &Path, stem: &str) -> PathBuf {
        let p = dir.join(format!("{stem}.jsonl"));
        fs::write(&p, b"{}").unwrap();
        p
    }

    #[test]
    fn resolve_transcript_matches_exact_session_id_in_first_candidate() {
        let tmp = std::env::temp_dir().join(format!("cp-sess-resolve-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let live = Path::new("/Users/me/Projects/demo");
        let live_dir = tmp.join(macos::project_slug(live));
        fs::create_dir_all(&live_dir).unwrap();

        let sid = "27c2524d-6f9b-4d16-a833-57f3fdaa68f7";
        let want = touch_jsonl(&live_dir, sid);
        // A *different* session's transcript sits in the same dir and is newer —
        // exact-stem matching must NOT pick it (the shared-dir hazard).
        touch_jsonl(&live_dir, "ff489433-f9eb-4b5b-87b3-a4cb78c2b424");

        let got = resolve_transcript(&tmp, sid, &[live]);
        assert_eq!(got.as_deref(), Some(want.as_path()));

        // No matching stem → None (never a sibling session's file).
        assert!(resolve_transcript(&tmp, "no-such-session", &[live]).is_none());

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn resolve_transcript_falls_back_to_startup_cwd_after_cd() {
        // The session started in `startup` (where the transcript lives) but has
        // since `cd`'d to `live`; only the startup-cwd slug dir holds the file.
        let tmp = std::env::temp_dir().join(format!("cp-sess-cd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let startup = Path::new("/Users/me/Projects/demo");
        let live = Path::new("/Users/me/Projects/demo/crates/inner");
        let startup_dir = tmp.join(macos::project_slug(startup));
        fs::create_dir_all(&startup_dir).unwrap();

        let sid = "27c2524d-6f9b-4d16-a833-57f3fdaa68f7";
        let want = touch_jsonl(&startup_dir, sid);

        // Live cwd's slug dir has no transcript → must fall back to startup cwd.
        let got = resolve_transcript(&tmp, sid, &[live, startup]);
        assert_eq!(
            got.as_deref(),
            Some(want.as_path()),
            "a session that cd'd away from its startup dir must still resolve its transcript"
        );

        // With only the (wrong) live cwd as a candidate, it is not found — which
        // is exactly the bug the startup-cwd fallback fixes.
        assert!(resolve_transcript(&tmp, sid, &[live]).is_none());

        fs::remove_dir_all(&tmp).unwrap();
    }

    // ---- liveness (FR-1/AC-2) ------------------------------------------------

    #[test]
    fn current_process_is_alive() {
        assert!(is_alive(std::process::id() as i32));
    }

    #[test]
    fn nonexistent_pid_is_not_alive() {
        // PIDs are 32-bit; this one will not exist as a live process.
        assert!(!is_alive(0x3FFF_FFFF));
    }

    // ---- discover_in end-to-end against a temp filesystem (FR-1/AC-2..AC-4) ---

    /// Build a fake `~/.claude/{sessions,projects}` layout and run `discover_in`
    /// with a synthetic engine set, using the *current* PID so the liveness gate
    /// passes deterministically (no dependency on the machine's real sessions).
    #[test]
    fn discover_in_builds_live_session_from_registry_and_transcript() {
        let root = std::env::temp_dir().join(format!("cp-sess-disc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        let projects_dir = root.join("projects");
        fs::create_dir_all(&sessions_dir).unwrap();

        let pid = std::process::id() as i32;
        let cwd = root.join("Projects").join("demo");
        fs::create_dir_all(&cwd).unwrap();
        let slug = macos::project_slug(&cwd);
        let slug_dir = projects_dir.join(&slug);
        fs::create_dir_all(&slug_dir).unwrap();

        let session_id = "27c2524d-6f9b-4d16-a833-57f3fdaa68f7";
        // The transcript stem equals the registry sessionId.
        let transcript = touch_jsonl(&slug_dir, session_id);

        let registry = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"{}","startedAt":1781987269616,"version":"2.1.181","kind":"interactive"}}"#,
            cwd.display()
        );
        fs::write(sessions_dir.join(format!("{pid}.json")), registry).unwrap();

        let engines = vec![(pid, Some(cwd.clone()))];
        let out = discover_in(&sessions_dir, &projects_dir, engines);

        assert_eq!(out.len(), 1);
        let s = &out[0];
        assert_eq!(s.pid, pid);
        assert_eq!(s.session_id, session_id);
        assert_eq!(s.cwd, cwd);
        assert_eq!(s.project_name, "demo");
        assert_eq!(s.transcript.as_deref(), Some(transcript.as_path()));
        assert_eq!(s.started_at, Some(1781987269616));
        assert_eq!(s.version.as_deref(), Some("2.1.181"));
        assert!(s.branch.is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discover_in_resolves_transcript_under_startup_cwd_after_cd() {
        // A session whose live cwd (sysinfo) differs from its registry startup cwd
        // because it `cd`'d: the transcript lives under the *startup* slug, so the
        // resolver must fall back to the registry cwd and still find it. Without
        // it the session would be discovered but watcher-less (tokens dropped).
        let root = std::env::temp_dir().join(format!("cp-sess-disc-cd-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        let projects_dir = root.join("projects");
        fs::create_dir_all(&sessions_dir).unwrap();

        let pid = std::process::id() as i32;
        let startup_cwd = root.join("Projects").join("demo");
        let live_cwd = startup_cwd.join("crates").join("inner");
        fs::create_dir_all(&live_cwd).unwrap();
        let startup_slug_dir = projects_dir.join(macos::project_slug(&startup_cwd));
        fs::create_dir_all(&startup_slug_dir).unwrap();

        let session_id = "27c2524d-6f9b-4d16-a833-57f3fdaa68f7";
        let transcript = touch_jsonl(&startup_slug_dir, session_id);

        // Registry records the *startup* cwd; sysinfo reports the *live* (cd'd) cwd.
        let registry = format!(
            r#"{{"pid":{pid},"sessionId":"{session_id}","cwd":"{}","startedAt":1781987269616,"version":"2.1.181"}}"#,
            startup_cwd.display()
        );
        fs::write(sessions_dir.join(format!("{pid}.json")), registry).unwrap();

        let engines = vec![(pid, Some(live_cwd.clone()))];
        let out = discover_in(&sessions_dir, &projects_dir, engines);

        assert_eq!(out.len(), 1);
        let s = &out[0];
        // Display cwd stays the live (cd'd) dir; the transcript resolves under the
        // startup dir via the registry-cwd fallback.
        assert_eq!(s.cwd, live_cwd);
        assert_eq!(s.transcript.as_deref(), Some(transcript.as_path()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discover_in_prunes_dead_pid_and_missing_registry() {
        let root = std::env::temp_dir().join(format!("cp-sess-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        let projects_dir = root.join("projects");
        fs::create_dir_all(&sessions_dir).unwrap();

        // (a) A dead PID with a registry file → pruned by the liveness gate.
        let dead_pid = 0x3FFF_FFFF;
        fs::write(
            sessions_dir.join(format!("{dead_pid}.json")),
            r#"{"pid":1073741823,"sessionId":"dead","cwd":"/tmp"}"#,
        )
        .unwrap();

        // (b) A live PID (current process) but NO registry file → skipped.
        let live_no_reg = std::process::id() as i32;

        let engines = vec![(dead_pid, Some(PathBuf::from("/tmp"))), (live_no_reg, None)];
        let out = discover_in(&sessions_dir, &projects_dir, engines);
        assert!(out.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn discover_in_dedupes_by_session_id() {
        let root = std::env::temp_dir().join(format!("cp-sess-dedup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sessions_dir = root.join("sessions");
        let projects_dir = root.join("projects");
        fs::create_dir_all(&sessions_dir).unwrap();

        let pid = std::process::id() as i32;
        let cwd = root.join("p");
        fs::create_dir_all(&cwd).unwrap();
        // Two registry files (two PIDs) that resolve to the SAME sessionId.
        for p in [pid, pid] {
            fs::write(
                sessions_dir.join(format!("{p}.json")),
                format!(
                    r#"{{"pid":{p},"sessionId":"same-id","cwd":"{}"}}"#,
                    cwd.display()
                ),
            )
            .unwrap();
        }
        let engines = vec![(pid, Some(cwd.clone())), (pid, Some(cwd))];
        let out = discover_in(&sessions_dir, &projects_dir, engines);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].session_id, "same-id");

        fs::remove_dir_all(&root).unwrap();
    }
}
