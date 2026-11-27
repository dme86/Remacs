//! Real local Git repositories exercise installation, startup loading and update
//! notifications together without network access or the user's plugin state.
use super::*;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct Project(pub(super) PathBuf);

impl Project {
    pub(super) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = env::temp_dir().join(format!(
            "remacs-managed-plugin-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let project = Self(root);
        project.git(&["init", "--initial-branch=main", "work"]);
        project.git(&["init", "--bare", "--initial-branch=main", "remote.git"]);
        project.git(&["-C", "work", "config", "user.name", "Remacs Tests"]);
        project.git(&[
            "-C",
            "work",
            "config",
            "user.email",
            "remacs-tests@example.invalid",
        ]);
        std::fs::create_dir_all(project.0.join("config")).unwrap();
        std::fs::write(
            project.0.join("config/init.lua"),
            "remacs.state.config_saw_plugin = remacs.state.managed_version\n",
        )
        .unwrap();
        project
    }

    pub(super) fn git(&self, args: &[&str]) {
        let result = Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .current_dir(&self.0)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    pub(super) fn publish(&self, version: u32) {
        std::fs::write(self.0.join("work/plugin.lua"), format!(
            "remacs.state.managed_version = {version}\nremacs.commands.add('managed_hello', function() remacs.message('version {version}') end)\n"
        )).unwrap();
        self.git(&["-C", "work", "add", "plugin.lua"]);
        self.git(&[
            "-C",
            "work",
            "commit",
            "-m",
            &format!("plugin version {version}"),
        ]);
        self.git(&[
            "-C",
            "work",
            "push",
            self.0.join("remote.git").to_str().unwrap(),
            "main",
        ]);
    }

    fn editor(&self) -> Editor {
        let runtime = LuaRuntime::with_config_dir(&self.0, self.0.join("config")).unwrap();
        let mut editor = Editor::with_runtime(Document::from_text("unchanged buffer"), runtime);
        editor.managed_plugins = plugin_manager::Manager::new(self.0.join("managed"));
        editor.screen.size = Some((120, 24));
        editor
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn finish(editor: &mut Editor) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        editor.poll_plugins();
        if !editor.managed_plugins.is_busy() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "plugin work did not finish: {}",
            editor.message
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    editor.poll_plugins();
}

#[test]
fn git_plugin_lifecycle_keeps_running_version_until_restart_and_reports_updates() {
    let project = Project::new();
    project.publish(1);
    let mut editor = project.editor();
    editor.command(&format!(
        "plugins add demo {}",
        project.0.join("remote.git").display()
    ));
    finish(&mut editor);
    assert!(editor.plugin_restart_required, "{}", editor.message);
    assert!(
        editor.managed_plugins.installed().is_empty(),
        "a new install must not execute in an existing VM"
    );
    drop(editor);

    let mut editor = project.editor();
    editor.load_lua_config();
    assert!(
        editor.plugin_load_errors.is_empty(),
        "{:?}",
        editor.plugin_load_errors
    );
    editor.command("managed_hello");
    assert_eq!(editor.message, "version 1");
    editor.command("lua assert(remacs.state.config_saw_plugin == 1); assert(remacs.plugins.list()[1].name == 'demo')");
    assert!(
        !editor.message.starts_with("Lua error"),
        "{}",
        editor.message
    );
    let running_sha = editor.managed_plugins.installed()[0].sha.clone();

    project.publish(2);
    editor.command("plugins check");
    finish(&mut editor);
    assert_eq!(
        editor.managed_plugins.update_count(),
        1,
        "{}",
        editor.message
    );
    assert!(editor.plugin_status_hint().contains("↑1"));
    assert!(editor.message.contains("update"), "{}", editor.message);
    editor.command("managed_hello");
    assert_eq!(editor.message, "version 1");
    editor.command("plugins update demo");
    finish(&mut editor);
    assert!(editor.plugin_restart_required);
    assert_eq!(editor.managed_plugins.installed()[0].sha, running_sha);
    editor.command("managed_hello");
    assert_eq!(editor.message, "version 1");
    drop(editor);

    let mut editor = project.editor();
    editor.load_lua_config();
    assert!(
        editor.plugin_load_errors.is_empty(),
        "{:?}",
        editor.plugin_load_errors
    );
    editor.command("managed_hello");
    assert_eq!(editor.message, "version 2");
    assert_ne!(editor.managed_plugins.installed()[0].sha, running_sha);
    editor.command("plugins remove demo");
    editor.command("managed_hello");
    assert_eq!(
        editor.message, "version 2",
        "removal must not unload live callbacks"
    );
    drop(editor);

    let mut editor = project.editor();
    editor.load_lua_config();
    assert!(editor.managed_plugins.installed().is_empty());
    editor.command("managed_hello");
    assert!(editor.message.contains("Unknown command"));
    assert_eq!(editor.doc.lsp_text(), "unchanged buffer");
}
