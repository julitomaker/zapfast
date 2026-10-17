//! The AI command-line tools a subscription works through: Claude Code
//! (Claude), Codex CLI (ChatGPT) and Gemini CLI (Google). Whats Pro uses
//! them for Projects' analysis and for media, so they can be set up from
//! the app itself, on a Mac that has none of them and no developer tools:
//!
//! - `install` runs each tool's own installer: Claude's install script,
//!   Codex's release binary from GitHub, and Gemini through Homebrew or
//!   npm, with a private copy of Node.js when the Mac has neither.
//!   Private installs go under `<config>/tools`, never anywhere that needs
//!   an administrator.
//! - `sign_in` opens the Terminal on the tool's own sign-in, where the
//!   person signs in with their subscription's account once.
//! - `find` and `path` locate the tools for the features that run them.

use std::{
    collections::HashMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Claude,
    Codex,
    Gemini,
}

impl Tool {
    pub fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    /// The account the person signs in with.
    pub fn account(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "ChatGPT",
            Self::Gemini => "Google",
        }
    }

    /// What starts the tool's sign-in in a terminal.
    fn sign_in_args(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &[],
            Self::Codex => &["login"],
            Self::Gemini => &[],
        }
    }
}

/// Where Whats Pro keeps the tools it installs itself.
pub fn tools_dir() -> PathBuf {
    directories::ProjectDirs::from("me", "paolino", crate::paths::APP_NAME)
        .map(|dirs| dirs.config_dir().join("tools"))
        .unwrap_or_else(|| std::env::temp_dir().join("whatspro-tools"))
}

/// The folders a tool may be in, besides the PATH: an app opened from the
/// Dock gets a bare PATH, so the usual install folders are tried too.
fn folders() -> Vec<PathBuf> {
    let tools = tools_dir();
    let mut folders = vec![
        tools.join("bin"),
        tools.join("gemini/bin"),
        tools.join("node/bin"),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".bun/bin",
            ".volta/bin",
        ] {
            folders.push(home.join(dir));
        }
    }
    folders
}

/// Where `name` is installed, if it is.
pub fn find(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(folders())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// A PATH for running the tools: theirs first, then the system's. Tools
/// that are scripts find their Node.js through it.
pub fn path() -> OsString {
    let mut dirs = folders();
    dirs.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

pub fn installed(tool: Tool) -> bool {
    find(tool.binary()).is_some()
}

/// The shell script that installs `tool` into `tools` (or with Homebrew).
pub fn install_script(tool: Tool, tools: &Path) -> String {
    let tools = tools.display().to_string().replace('"', "\\\"");
    let head = format!("set -euo pipefail\nTOOLS=\"{tools}\"\nmkdir -p \"$TOOLS/bin\"\n");
    let body = match tool {
        // Claude's own installer puts `claude` in ~/.local/bin.
        Tool::Claude => "curl -fsSL https://claude.ai/install.sh | bash\n".to_owned(),
        // The release binary, as Codex publishes it on GitHub.
        Tool::Codex => r#"arch="$(uname -m)"
[ "$arch" = arm64 ] && arch=aarch64
work="$(mktemp -d)"
echo "Downloading Codex for $arch…"
curl -fsSL -o "$work/codex.tar.gz" "https://github.com/openai/codex/releases/latest/download/codex-$arch-apple-darwin.tar.gz"
tar -xzf "$work/codex.tar.gz" -C "$work"
mv "$work/codex-$arch-apple-darwin" "$TOOLS/bin/codex"
chmod +x "$TOOLS/bin/codex"
rm -rf "$work"
"$TOOLS/bin/codex" --version
"#
        .to_owned(),
        // Gemini CLI runs on Node.js: Homebrew's, the Mac's own npm, or a
        // private copy of the current Node.js LTS.
        Tool::Gemini => r#"if [ -x /opt/homebrew/bin/brew ] || [ -x /usr/local/bin/brew ]; then
  brew="$( [ -x /opt/homebrew/bin/brew ] && echo /opt/homebrew/bin/brew || echo /usr/local/bin/brew )"
  "$brew" install gemini-cli
  exit 0
fi
if ! command -v npm >/dev/null 2>&1; then
  arch="$(uname -m)"
  [ "$arch" = x86_64 ] && arch=x64
  base="https://nodejs.org/dist/latest-v22.x"
  file="$(curl -fsSL "$base/SHASUMS256.txt" | awk -v want="darwin-$arch.tar.gz" '$2 ~ want"$" {print $2}' | head -n1)"
  [ -n "$file" ] || { echo "Node.js could not be found for $arch"; exit 1; }
  echo "Downloading $file…"
  rm -rf "$TOOLS/node"
  curl -fsSL "$base/$file" | tar -xz -C "$TOOLS"
  mv "$TOOLS/${file%.tar.gz}" "$TOOLS/node"
  export PATH="$TOOLS/node/bin:$PATH"
fi
npm install -g --prefix "$TOOLS/gemini" @google/gemini-cli
"$TOOLS/gemini/bin/gemini" --version
"#
        .to_owned(),
    };
    head + &body
}

#[derive(Clone, Debug, Default)]
pub struct Job {
    pub running: bool,
    /// How the last run ended: `Ok` with a note, or the error.
    pub result: Option<Result<String, String>>,
    /// The last lines it printed.
    pub log: String,
}

fn jobs() -> &'static Mutex<HashMap<Tool, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<Tool, Job>>> = OnceLock::new();
    JOBS.get_or_init(Default::default)
}

/// The install of `tool` under way or last run.
pub fn job(tool: Tool) -> Option<Job> {
    jobs()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&tool)
        .cloned()
}

fn update(ctx: &egui::Context, tool: Tool, change: impl FnOnce(&mut Job)) {
    change(
        jobs()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(tool)
            .or_default(),
    );
    ctx.request_repaint();
}

/// Installs `tool` on a thread of its own; `job` follows it.
pub fn install(ctx: &egui::Context, tool: Tool) {
    if job(tool).is_some_and(|job| job.running) {
        return;
    }
    update(ctx, tool, |job| {
        *job = Job {
            running: true,
            ..Job::default()
        }
    });
    let worker = ctx.clone();
    let spawned = std::thread::Builder::new()
        .name("tool-install".into())
        .spawn(move || {
            let result = run_install(&worker, tool);
            update(&worker, tool, |job| {
                job.running = false;
                job.result = Some(result);
            });
        });
    if spawned.is_err() {
        update(ctx, tool, |job| job.running = false);
    }
}

fn run_install(ctx: &egui::Context, tool: Tool) -> Result<String, String> {
    use std::io::{BufRead, BufReader};
    let tools = tools_dir();
    std::fs::create_dir_all(&tools).map_err(|e| e.to_string())?;
    let mut child = std::process::Command::new("/bin/bash")
        .arg("-c")
        .arg(install_script(tool, &tools))
        .env("PATH", path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Both streams feed the log, so progress shows as it happens.
    let stderr = child.stderr.take();
    let err_ctx = ctx.clone();
    let reader = stderr.map(|stderr| {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                push_log(&err_ctx, tool, &line);
            }
        })
    });
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            push_log(ctx, tool, &line);
        }
    }
    if let Some(reader) = reader {
        let _ = reader.join();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        let last = job(tool)
            .and_then(|job| job.log.lines().last().map(str::to_owned))
            .unwrap_or_default();
        return Err(format!("The install stopped ({status}). {last}"));
    }
    if installed(tool) {
        Ok(String::new())
    } else {
        Err("The install finished, but the tool was not found".to_owned())
    }
}

fn push_log(ctx: &egui::Context, tool: Tool, line: &str) {
    // Progress bars redraw with carriage returns; keep the last state.
    let line = line.rsplit('\r').next().unwrap_or(line).trim_end();
    if line.is_empty() {
        return;
    }
    update(ctx, tool, |job| {
        job.log.push_str(line);
        job.log.push('\n');
        let lines: Vec<&str> = job.log.lines().collect();
        if lines.len() > 40 {
            job.log = lines[lines.len() - 40..].join("\n") + "\n";
        }
    });
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The script the Terminal runs to sign in to `tool`. It is opened as a
/// file rather than typed: a typed command this long would be cut off by
/// the terminal's line limit.
pub fn sign_in_script(tool: Tool, locale: crate::i18n::Locale) -> Option<String> {
    let binary = find(tool.binary())?;
    let mut run = shell_quote(&binary.to_string_lossy());
    for arg in tool.sign_in_args() {
        run.push(' ');
        run.push_str(arg);
    }
    Some(format!(
        "#!/bin/bash\n\
         export PATH={path}\n\
         clear\n\
         echo {intro}\n\
         echo\n\
         {run}\n\
         echo\n\
         echo {done}\n",
        path = shell_quote(&path().to_string_lossy()),
        intro = shell_quote(
            &crate::i18n::gettext(
                locale,
                "Whats Pro: sign in with your {account} account. Follow the steps (a browser window may open).",
            )
            .replace("{account}", tool.account())
        ),
        done = shell_quote(&crate::i18n::gettext(
            locale,
            "Done. You can close this window and go back to Whats Pro."
        )),
    ))
}

/// Opens the Terminal on `tool`'s sign-in.
pub fn sign_in(tool: Tool, locale: crate::i18n::Locale) -> Result<(), String> {
    let script = sign_in_script(tool, locale).ok_or("The tool is not installed yet")?;
    let file = tools_dir().join(format!("sign-in-{}.command", tool.binary()));
    std::fs::create_dir_all(tools_dir()).map_err(|e| e.to_string())?;
    std::fs::write(&file, script).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/open")
            .args(["-a", "Terminal"])
            .arg(&file)
            .status()
            .map_err(|e| e.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err("The Terminal could not be opened".to_owned())
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(format!("Run this in a terminal: {}", file.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_install_into_the_tools_folder() {
        let tools = Path::new("/tmp/whats pro/tools");
        let codex = install_script(Tool::Codex, tools);
        assert!(codex.starts_with("set -euo pipefail\nTOOLS=\"/tmp/whats pro/tools\""));
        assert!(codex.contains("releases/latest/download/codex-$arch-apple-darwin.tar.gz"));
        assert!(install_script(Tool::Gemini, tools).contains("--prefix \"$TOOLS/gemini\""));
        assert!(install_script(Tool::Claude, tools).contains("claude.ai/install.sh"));
    }

    /// Installs the tool named in `WHATSPRO_INSTALL` for real, as the
    /// Install button does. Run by hand: it downloads and installs.
    #[test]
    #[ignore = "downloads and installs a tool"]
    fn installs_a_tool_for_real() {
        let tool = match std::env::var("WHATSPRO_INSTALL").as_deref() {
            Ok("claude") => Tool::Claude,
            Ok("codex") => Tool::Codex,
            Ok("gemini") => Tool::Gemini,
            _ => return,
        };
        let ctx = egui::Context::default();
        let result = run_install(&ctx, tool);
        println!("{}", job(tool).map(|job| job.log).unwrap_or_default());
        assert_eq!(result, Ok(String::new()));
        assert!(installed(tool), "{tool:?} is found after its install");
    }

    #[test]
    fn the_sign_in_script_runs_the_tool_with_its_path() {
        let Some(script) = sign_in_script(Tool::Codex, crate::i18n::Locale::English) else {
            return;
        };
        assert!(script.starts_with("#!/bin/bash\nexport PATH='"));
        assert!(script.contains("codex' login\n"), "{script}");
        let file = std::env::temp_dir().join("whatspro-sign-in-test.command");
        std::fs::write(&file, &script).unwrap();
        let syntax = std::process::Command::new("/bin/bash")
            .arg("-n")
            .arg(&file)
            .status()
            .unwrap();
        let _ = std::fs::remove_file(&file);
        assert!(syntax.success(), "the script is valid bash");
    }

    #[test]
    fn paths_are_quoted_for_the_shell() {
        assert_eq!(shell_quote("a b'c"), "'a b'\\''c'");
    }
}
