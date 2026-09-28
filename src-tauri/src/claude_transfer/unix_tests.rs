use super::*;
use std::cell::RefCell;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;

struct Writer(Child);
impl Writer {
    fn start(path: &Path) -> Self {
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                r#"exec 9>>"$1"; printf 'ready\n'; read -r line; printf '%s\n' "$line" >&9"#,
                "writer",
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        Self(child)
    }

    fn append_and_exit(&mut self) {
        writeln!(self.0.stdin.take().unwrap(), "{{\"type\":\"assistant\",\"sessionId\":\"session-1\",\"message\":{{\"role\":\"assistant\",\"content\":\"NEW-B\"}}}}").unwrap();
        assert!(self.0.wait().unwrap().success());
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn recovery_root(claude: &Path) -> PathBuf {
    fs::read_dir(claude.join(".cc-sessions-moves"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

fn hook(phase: usize, action: Box<dyn FnOnce()>) {
    match phase {
        0 => AFTER_STAGE.with(|slot| *slot.borrow_mut() = Some(action)),
        1 => AFTER_DETACH.with(|slot| *slot.borrow_mut() = Some(action)),
        2 => AFTER_PUBLISH.with(|slot| *slot.borrow_mut() = Some(action)),
        _ => unreachable!(),
    }
}

#[test]
fn r04_child_writer_at_each_boundary_preserves_appends_and_can_retry() -> AppResult<()> {
    for phase in 0..3 {
        let (claude, old, new) = tests::fixture()?;
        let source = claude_sessions::project_dir_for_cwd(&claude, old.to_str().unwrap())
            .join("session-1.jsonl");
        let destination = claude_sessions::project_dir_for_cwd(&claude, new.to_str().unwrap())
            .join("session-1.jsonl");
        let before = fs::read(&source)?;
        let history_before = fs::read(claude.join("history.jsonl"))?;
        let writer = Rc::new(RefCell::new(None));
        let child = writer.clone();
        let root = claude.clone();
        let original = source.clone();
        hook(
            phase,
            Box::new(move || {
                let path = if phase == 0 {
                    original
                } else {
                    recovery_root(&root).join("source-transcript")
                };
                *child.borrow_mut() = Some(Writer::start(&path));
            }),
        );
        let result = move_session_cwd(
            &claude,
            "session-1",
            Some(source.to_str().unwrap()),
            new.to_str().unwrap(),
        );
        assert!(result.unwrap_err().to_string().contains("SESSION_BUSY"));
        assert_eq!(fs::read(&source)?, before);
        assert_eq!(fs::read(claude.join("history.jsonl"))?, history_before);
        assert!(!destination.exists());
        writer.borrow_mut().as_mut().unwrap().append_and_exit();
        assert!(fs::read_to_string(&source)?.contains("NEW-B"));
        move_session_cwd(
            &claude,
            "session-1",
            Some(source.to_str().unwrap()),
            new.to_str().unwrap(),
        )?;
        assert!(fs::read_to_string(&destination)?.contains("NEW-B"));
        drop(writer);
        fs::remove_dir_all(claude.parent().unwrap())?;
    }
    Ok(())
}

#[test]
fn r04_recreated_source_keeps_both_versions_for_recovery() -> AppResult<()> {
    let (claude, old, new) = tests::fixture()?;
    let source = claude_sessions::project_dir_for_cwd(&claude, old.to_str().unwrap())
        .join("session-1.jsonl");
    let before = fs::read(&source)?;
    let original = source.clone();
    hook(1, Box::new(move || fs::write(original, "NEW-B").unwrap()));
    let result = move_session_cwd(
        &claude,
        "session-1",
        Some(source.to_str().unwrap()),
        new.to_str().unwrap(),
    );
    assert!(result.unwrap_err().to_string().contains("MOVE_CONFLICT"));
    assert_eq!(fs::read(&source)?, b"NEW-B");
    assert_eq!(
        fs::read(recovery_root(&claude).join("source-transcript"))?,
        before
    );
    assert!(!recovery_root(&claude).join("status.json").exists());
    fs::remove_dir_all(claude.parent().unwrap())?;
    Ok(())
}

#[test]
fn r04_published_writer_is_rolled_back_without_losing_its_data() -> AppResult<()> {
    let (claude, old, new) = tests::fixture()?;
    let source = claude_sessions::project_dir_for_cwd(&claude, old.to_str().unwrap())
        .join("session-1.jsonl");
    let destination = claude_sessions::project_dir_for_cwd(&claude, new.to_str().unwrap())
        .join("session-1.jsonl");
    let before = fs::read(&source)?;
    let writer = Rc::new(RefCell::new(None));
    let child = writer.clone();
    let target = destination.clone();
    hook(
        2,
        Box::new(move || *child.borrow_mut() = Some(Writer::start(&target))),
    );
    let result = move_session_cwd(
        &claude,
        "session-1",
        Some(source.to_str().unwrap()),
        new.to_str().unwrap(),
    );
    assert!(result.unwrap_err().to_string().contains("SESSION_BUSY"));
    assert_eq!(fs::read(&source)?, before);
    assert!(!destination.exists());
    writer.borrow_mut().as_mut().unwrap().append_and_exit();
    assert!(fs::read_to_string(recovery_root(&claude).join("transcript.jsonl"))?.contains("NEW-B"));
    drop(writer);
    fs::remove_dir_all(claude.parent().unwrap())?;
    Ok(())
}

// This creates a process named claude. Run alone so unrelated concurrent fixture
// moves do not correctly refuse that process. CI invokes this test explicitly.
#[test]
#[ignore = "runs a native-name process; invoke separately with --ignored --test-threads=1"]
fn r04_idle_native_process_prevents_move_at_each_boundary() -> AppResult<()> {
    for phase in 0..3 {
        let (claude, old, new) = tests::fixture()?;
        let source = claude_sessions::project_dir_for_cwd(&claude, old.to_str().unwrap())
            .join("session-1.jsonl");
        let before = fs::read(&source)?;
        let executable = claude.parent().unwrap().join("claude");
        fs::copy("/bin/sleep", &executable)?;
        let child = Rc::new(RefCell::new(None));
        let slot = child.clone();
        let start: Box<dyn FnOnce()> = Box::new(move || {
            *slot.borrow_mut() = Some(Writer(Command::new(executable).arg("60").spawn().unwrap()));
        });
        if phase == 0 {
            start();
        } else {
            hook(phase, start);
        }
        let result = move_session_cwd(
            &claude,
            "session-1",
            Some(source.to_str().unwrap()),
            new.to_str().unwrap(),
        );
        assert!(result.unwrap_err().to_string().contains("SESSION_BUSY"));
        assert_eq!(fs::read(&source)?, before);
        drop(child.borrow_mut().take());
        move_session_cwd(
            &claude,
            "session-1",
            Some(source.to_str().unwrap()),
            new.to_str().unwrap(),
        )?;
        fs::remove_dir_all(claude.parent().unwrap())?;
    }
    Ok(())
}
