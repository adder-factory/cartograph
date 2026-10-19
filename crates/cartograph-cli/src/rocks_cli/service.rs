//! Project-scoped login ownership. The service runs the existing native owner.

use clap::{Args, Subcommand};
use std::{path::PathBuf, process::ExitCode};

#[derive(Debug, Args)]
pub(crate) struct ServiceArguments {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, Subcommand)]
enum Action {
    /// Install and start autosync at login for this project.
    Start(Project),
    /// Stop this project's login service and remove its owned registration.
    Stop(Project),
    /// Report registration, launchd state, and native owner availability separately.
    Status(Project),
}

#[derive(Debug, Args)]
struct Project {
    #[arg(short = 'p', long, default_value = ".")]
    project_path: PathBuf,
    #[arg(long, value_enum, default_value_t = crate::OutputFormat::Text)]
    format: crate::OutputFormat,
}

pub(crate) fn run(
    arguments: ServiceArguments,
) -> impl std::future::Future<Output = Result<ExitCode, String>> {
    #[cfg(target_os = "macos")]
    {
        macos::run(arguments.action)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let project = match arguments.action {
            Action::Start(project) | Action::Stop(project) | Action::Status(project) => project,
        };
        let _ = (project.project_path, project.format);
        std::future::ready(Err("login service management currently requires macOS; run cartograph watch under your user service manager".to_owned()))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{Action, ExitCode};
    use serde_json::json;
    use std::{
        fs,
        io::Write as _,
        os::unix::fs::{MetadataExt as _, PermissionsExt as _},
        path::{Path, PathBuf},
        process::Stdio,
        time::Duration,
    };

    struct Registration {
        root: PathBuf,
        path: PathBuf,
        domain: String,
        label: String,
        prefix: String,
        document: String,
    }

    impl Registration {
        fn new(project: &Path) -> Result<Self, String> {
            let root = fs::canonicalize(project)
                .map_err(|_| "service project is unavailable".to_owned())?;
            if !root.is_dir() {
                return Err("service project must be a directory".to_owned());
            }
            let executable = std::env::current_exe()
                .and_then(fs::canonicalize)
                .map_err(|_| "service executable is unavailable".to_owned())?;
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or("service home is unavailable")?;
            let uid = rustix::process::geteuid().as_raw();
            let hash = blake3::hash(root.as_os_str().as_encoded_bytes())
                .to_hex()
                .to_string();
            let label = format!("com.adder-factory.cartograph.{}", &hash[..32]);
            let path = home
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist"));
            let domain = format!("gui/{uid}");
            let prefix = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!-- Cartograph project {hash} -->\n"
            );
            let executable = xml_path(&executable)?;
            let project = xml_path(&root)?;
            // CLI and editor responsiveness depends on this Unix-socket owner.
            // launchd cannot infer interactive transactions through XPC here.
            let document = format!(
                "{prefix}<plist version=\"1.0\"><dict>\n<key>Label</key><string>{label}</string>\n<key>ProgramArguments</key><array><string>{executable}</string><string>watch</string><string>--project-path</string><string>{project}</string></array>\n<key>WorkingDirectory</key><string>{project}</string>\n<key>ProcessType</key><string>Interactive</string>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n<key>ThrottleInterval</key><integer>10</integer>\n<key>ExitTimeOut</key><integer>300</integer>\n<key>StandardOutPath</key><string>/dev/null</string>\n<key>StandardErrorPath</key><string>/dev/null</string>\n</dict></plist>\n"
            );
            Ok(Self {
                root,
                path,
                domain,
                label,
                prefix,
                document,
            })
        }

        fn existing(&self) -> Result<Option<String>, String> {
            let metadata = match fs::symlink_metadata(&self.path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => return Err("service registration is unreadable".to_owned()),
            };
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.len() > 16 * 1024
                || metadata.mode() & 0o077 != 0
            {
                return Err("service registration is not a private owned file".to_owned());
            }
            let contents = fs::read_to_string(&self.path)
                .map_err(|_| "service registration is unreadable".to_owned())?;
            if !contents.starts_with(&self.prefix)
                || !contents.contains(&format!("<key>Label</key><string>{}</string>", self.label))
                || !contents.contains(&format!(
                    "<key>WorkingDirectory</key><string>{}</string>",
                    xml_path(&self.root)?
                ))
            {
                return Err("refusing a foreign service registration".to_owned());
            }
            Ok(Some(contents))
        }

        fn install(&self) -> Result<(), String> {
            self.existing()?;
            let parent = self
                .path
                .parent()
                .ok_or("service directory is unavailable")?;
            for ancestor in parent.ancestors() {
                match fs::symlink_metadata(ancestor) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err("service directory is unsafe".to_owned()),
                }
            }
            fs::create_dir_all(parent)
                .map_err(|_| "service directory creation failed".to_owned())?;
            if fs::metadata(parent)
                .map_err(|_| "service directory is unreadable".to_owned())?
                .uid()
                != rustix::process::geteuid().as_raw()
            {
                return Err("service directory has a foreign owner".to_owned());
            }
            let mut file = tempfile::NamedTempFile::new_in(parent)
                .map_err(|_| "service registration write failed".to_owned())?;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "service permissions failed".to_owned())?;
            file.write_all(self.document.as_bytes())
                .and_then(|()| file.as_file().sync_all())
                .map_err(|_| "service registration write failed".to_owned())?;
            file.persist(&self.path)
                .map_err(|_| "service registration install failed".to_owned())?;
            Ok(())
        }

        fn target(&self) -> String {
            format!("{}/{}", self.domain, self.label)
        }
    }

    fn xml_path(path: &Path) -> Result<String, String> {
        let text = path.to_str().ok_or("service paths must be UTF-8")?;
        if text.chars().any(char::is_control) {
            return Err("service path contains control characters".to_owned());
        }
        Ok(text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;"))
    }

    async fn launchctl(arguments: &[&std::ffi::OsStr]) -> Result<bool, String> {
        let mut child = tokio::process::Command::new("/bin/launchctl")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "launchctl is unavailable".to_owned())?;
        if let Ok(result) = tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
            result
                .map(|status| status.success())
                .map_err(|_| "launchctl failed".to_owned())
        } else {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err("launchctl exceeded its deadline and was reaped".to_owned())
        }
    }

    pub(super) async fn run(action: Action) -> Result<ExitCode, String> {
        let (project, start, stop) = match action {
            Action::Start(project) => (project, true, false),
            Action::Stop(project) => (project, false, true),
            Action::Status(project) => (project, false, false),
        };
        let root = project.project_path.clone();
        let (registration, existing) = tokio::task::spawn_blocking(move || {
            let registration = Registration::new(&root)?;
            let existing = registration.existing()?;
            Ok::<_, String>((registration, existing))
        })
        .await
        .map_err(|_| "service configuration worker failed".to_owned())??;
        let target = registration.target();
        let mut loaded = launchctl(&["print".as_ref(), target.as_ref()]).await?;
        if stop {
            if existing.is_none() {
                return Err("no owned login service registration exists".to_owned());
            }
            if loaded && !launchctl(&["bootout".as_ref(), target.as_ref()]).await? {
                return Err("service stop failed; registration was preserved".to_owned());
            }
            let path = registration.path.clone();
            tokio::task::spawn_blocking(move || fs::remove_file(path))
                .await
                .map_err(|_| "service removal worker failed".to_owned())?
                .map_err(|_| "service registration removal failed".to_owned())?;
            loaded = false;
        }
        if start {
            start_registration(&registration, existing.as_deref(), loaded).await?;
            loaded = true;
        }
        let available = super::super::owner::OwnerClient::connect(&registration.root)
            .await?
            .is_some();
        crate::print_serialized(
            &json!({"storage":"rocksdb", "registered": if stop { false } else { start || existing.is_some() }, "loaded":loaded, "ownerAvailable":available, "label":registration.label, "survivesMcpDisconnect":true}),
            project.format,
        )?;
        Ok(ExitCode::SUCCESS)
    }

    async fn start_registration(
        registration: &Registration,
        existing: Option<&str>,
        loaded: bool,
    ) -> Result<(), String> {
        if loaded {
            if existing != Some(registration.document.as_str()) {
                return Err("loaded service differs from this executable; stop its owned registration before starting again".to_owned());
            }
            return Ok(());
        }
        // The blocking write is awaited; it never outlives command ownership.
        let path = registration.root.clone();
        tokio::task::spawn_blocking(move || Registration::new(&path)?.install())
            .await
            .map_err(|_| "service install worker failed".to_owned())??;
        if !launchctl(&[
            "bootstrap".as_ref(),
            registration.domain.as_ref(),
            registration.path.as_os_str(),
        ])
        .await?
        {
            return Err(
                "service registration was installed but launchd did not start it".to_owned(),
            );
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn plist_paths_are_escaped_and_control_characters_rejected()
        -> Result<(), Box<dyn std::error::Error>> {
            assert_eq!(
                xml_path(Path::new("/tmp/a&<b>\"'"))?,
                "/tmp/a&amp;&lt;b&gt;&quot;&apos;"
            );
            assert!(xml_path(Path::new("/tmp/a\nb")).is_err());
            Ok(())
        }

        #[test]
        fn owned_registration_is_atomic_private_and_refuses_foreign_files_and_symlinks()
        -> Result<(), Box<dyn std::error::Error>> {
            let fixture = tempfile::tempdir()?;
            let root = fs::canonicalize(fixture.path())?;
            let mut registration = Registration::new(&root)?;
            registration.path = root.join("owned.plist");
            assert_eq!(registration.existing()?, None);
            registration.install()?;
            assert_eq!(
                registration.existing()?,
                Some(registration.document.clone())
            );
            let policy = std::process::Command::new("/usr/bin/plutil")
                .args(["-extract", "ProcessType", "raw", "-o", "-"])
                .arg(&registration.path)
                .output()?;
            assert!(policy.status.success());
            assert_eq!(String::from_utf8(policy.stdout)?.trim(), "Interactive");
            fs::set_permissions(&registration.path, fs::Permissions::from_mode(0o644))?;
            assert!(registration.existing().is_err());
            fs::set_permissions(&registration.path, fs::Permissions::from_mode(0o600))?;
            fs::write(&registration.path, "foreign registration")?;
            assert!(registration.install().is_err());
            assert_eq!(
                fs::read_to_string(&registration.path)?,
                "foreign registration"
            );
            fs::remove_file(&registration.path)?;
            let target = root.join("foreign");
            fs::write(&target, "keep")?;
            std::os::unix::fs::symlink(&target, &registration.path)?;
            assert!(registration.install().is_err());
            assert_eq!(fs::read_to_string(target)?, "keep");
            Ok(())
        }
    }
}
