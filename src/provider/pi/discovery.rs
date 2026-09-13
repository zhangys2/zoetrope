//! Where pi keeps its sessions on disk, and how to tell what a file is.
//!
//! A session is `~/.pi/agent/sessions/--<cwd>--/<timestamp>_<uuid>.jsonl`
//! (pi's `docs/session-format.md`). The pi-subagents extension gives each
//! child run a session of its own beside it, under a directory named after
//! the root's stem: `<timestamp>_<uuid>/<run id>/run-<n>/session.jsonl`. So a
//! path names its session, its role and its project, and nothing is read.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::provider::{FileRole, Provider, ReadMode, Scope, SessionFile};
use crate::state::session::MAIN_ID;

/// The sessions root: `$PI_CODING_AGENT_DIR/sessions`, else
/// `~/.pi/agent/sessions`.
fn sessions_root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return Some(PathBuf::from(dir).join("sessions"));
    }
    #[allow(deprecated)]
    let home = std::env::home_dir()
        .filter(|h| !h.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))?;
    Some(home.join(".pi").join("agent").join("sessions"))
}

/// Every root under the sessions root, or under the one project directory
/// when the scope names a working directory. Only roots: a session's runs are
/// found from its root. The file name carries the session id, so an id scope
/// prunes without reading.
pub fn all_paths(scope: &Scope) -> Vec<PathBuf> {
    let Some(root) = sessions_root() else {
        return Vec::new();
    };
    let dirs: Vec<PathBuf> = match &scope.project {
        Some(cwd) => vec![root.join(project_key(cwd))],
        None => std::fs::read_dir(&root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for path in rd.flatten().map(|e| e.path()) {
            let Some(session) = path
                .extension()
                .is_some_and(|e| e == "jsonl")
                .then(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(session_of_stem)
                })
                .flatten()
            else {
                continue;
            };
            if scope
                .id_prefix
                .as_deref()
                .is_some_and(|p| !session.starts_with(p))
                || scope
                    .since
                    .is_some_and(|t| crate::provider::modified(&path) < t)
            {
                continue;
            }
            out.push(path);
        }
    }
    out.sort();
    out
}

pub fn session_file(path: &Path) -> Option<SessionFile> {
    classify_path(path, crate::provider::modified(path))
}

/// The session id a root stem carries: `<timestamp>_<uuid>` → the uuid.
fn session_of_stem(stem: &str) -> Option<&str> {
    let (_, id) = stem.rsplit_once('_')?;
    (id.len() == 36 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(id)
}

fn name(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

/// [`session_file`] without the filesystem: pure path logic, so the browser
/// can classify a file by the path it came with.
pub fn classify_path(path: &Path, modified: SystemTime) -> Option<SessionFile> {
    let file = |session: &str, role, project: &Path| {
        Some(SessionFile {
            provider: Provider::Pi,
            path: path.to_path_buf(),
            session: session.to_string(),
            role,
            read: ReadMode::Tail,
            project_key: name(project)?.to_string(),
            modified,
        })
    };
    if path.extension().is_some_and(|e| e == "jsonl")
        && let Some(session) = session_of_stem(path.file_stem()?.to_str()?)
    {
        return file(session, FileRole::Root, path.parent()?);
    }
    if name(path)? != "session.jsonl" {
        return None;
    }
    let attempt = path.parent()?;
    name(attempt)?.strip_prefix("run-")?;
    let session_dir = attempt.parent()?.parent()?;
    let session = session_of_stem(name(session_dir)?)?;
    file(
        session,
        FileRole::Agent {
            parent: session.to_string(),
        },
        session_dir.parent()?,
    )
}

/// The root transcript a run file belongs to: `<stem>/<run>/run-<n>/session.jsonl`
/// → `<stem>.jsonl` beside the `<stem>` directory.
fn root_of_run(run: &Path) -> Option<PathBuf> {
    let session_dir = run.parent()?.parent()?.parent()?;
    Some(session_dir.with_file_name(format!("{}.jsonl", name(session_dir)?)))
}

/// The run id a run file sits under.
fn run_of(run: &Path) -> Option<&str> {
    name(run.parent()?.parent()?)
}

/// Every run file under a root's `<stem>` directory.
fn runs_of(root: &Path) -> Vec<PathBuf> {
    let Some(stem) = root.file_stem() else {
        return Vec::new();
    };
    let session_dir = root.with_file_name(stem);
    let mut out = Vec::new();
    let Ok(runs) = std::fs::read_dir(&session_dir) else {
        return out;
    };
    for run in runs.flatten() {
        let Ok(attempts) = std::fs::read_dir(run.path()) else {
            continue;
        };
        for attempt in attempts.flatten() {
            let file = attempt.path().join("session.jsonl");
            if name(&attempt.path()).is_some_and(|n| n.starts_with("run-")) && file.is_file() {
                out.push(file);
            }
        }
    }
    out.sort();
    out
}

/// Where the rest of a file's session is: the root, and every run beside it.
pub fn related_paths(file: &SessionFile) -> Vec<PathBuf> {
    let root = match file.role {
        FileRole::Root => file.path.clone(),
        _ => match root_of_run(&file.path) {
            Some(r) => r,
            None => return Vec::new(),
        },
    };
    let mut out = runs_of(&root);
    if root.is_file() {
        out.insert(0, root);
    }
    out.retain(|p| *p != file.path);
    out
}

/// pi names a project directory after its working directory: the leading
/// separator dropped, every separator and a drive's colon made a dash, and
/// the whole wrapped in `--`.
pub fn project_key(cwd: &Path) -> String {
    let cwd = cwd.to_string_lossy();
    let body: String = cwd
        .trim_start_matches(['/', '\\'])
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{body}--")
}

/// The stream for one of the session's files: the root speaks as `main`, a
/// run as its run id.
pub fn stream_for(file: &SessionFile) -> super::Stream {
    let owner = match file.role {
        FileRole::Root => MAIN_ID,
        _ => run_of(&file.path).unwrap_or(&file.session),
    };
    super::Stream::new(owner.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FileRole, Provider, ReadMode};

    const ROOT: &str = "/h/.pi/agent/sessions/--C--Users-me-app--/2026-09-12T21-48-02-988Z_01a09797-7e2c-7002-a725-c0a3455ef1c3.jsonl";
    const RUN: &str = "/h/.pi/agent/sessions/--C--Users-me-app--/2026-09-12T21-48-02-988Z_01a09797-7e2c-7002-a725-c0a3455ef1c3/3602b259/run-0/session.jsonl";

    /// pi names a project directory `--<cwd>--`, with the leading separator
    /// dropped and every separator and drive colon a dash. The Windows
    /// literals are directories pi wrote.
    #[test]
    fn project_key_is_pis_directory_name() {
        assert_eq!(
            project_key(Path::new(r"C:\Users\zhang\source\repos\essvi-bench")),
            "--C--Users-zhang-source-repos-essvi-bench--"
        );
        assert_eq!(
            project_key(Path::new(r"C:\Users\zhang\Documents\git\.pi")),
            "--C--Users-zhang-Documents-git-.pi--"
        );
        assert_eq!(
            project_key(Path::new("/home/demo/app")),
            "--home-demo-app--"
        );
    }

    /// The root is `<project>/<timestamp>_<uuid>.jsonl`, and the session is
    /// the uuid. A pi-subagents run is `<root stem>/<run id>/run-<n>/session.jsonl`:
    /// the path names its session and its run, so nothing is read.
    #[test]
    fn root_and_run_files_classify_by_path() {
        let root = classify_path(Path::new(ROOT), SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(root.provider, Provider::Pi);
        assert_eq!(root.session, "01a09797-7e2c-7002-a725-c0a3455ef1c3");
        assert_eq!(root.role, FileRole::Root);
        assert_eq!(root.read, ReadMode::Tail);
        assert_eq!(root.project_key, "--C--Users-me-app--");

        let run = classify_path(Path::new(RUN), SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(run.session, root.session);
        assert_eq!(
            run.role,
            FileRole::Agent {
                parent: root.session.clone()
            }
        );
        assert_eq!(run.project_key, root.project_key);

        assert!(
            classify_path(
                Path::new("/h/.pi/agent/settings.json"),
                SystemTime::UNIX_EPOCH
            )
            .is_none()
        );
        assert!(
            classify_path(
                Path::new("/h/.pi/agent/sessions/--p--/2026-09-12T21-48-02-988Z_01a09797-7e2c-7002-a725-c0a3455ef1c3/3602b259/events.jsonl"),
                SystemTime::UNIX_EPOCH
            )
            .is_none()
        );
    }
}
