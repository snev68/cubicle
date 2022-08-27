use anyhow::{anyhow, Context, Result};
use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::str::FromStr;

use super::fs_util::{rmtree, summarize_dir, try_exists, try_iterdir, DirSummary};
use super::newtype::EnvPath;
use super::runner::{EnvFilesSummary, EnvironmentExists, Runner, RunnerCommand};
use super::scoped_child::{ScopedChild, ScopedSpawn};
use super::{CubicleShared, EnvironmentName, ExitStatusError, HostPath};

pub struct Bubblewrap {
    pub(super) program: Rc<CubicleShared>,
    home_dirs: HostPath,
    work_dirs: HostPath,
}

impl Bubblewrap {
    pub(super) fn new(program: Rc<CubicleShared>) -> Result<Self> {
        let xdg_cache_home = match std::env::var("XDG_CACHE_HOME") {
            Ok(path) => HostPath::try_from(path)?,
            Err(_) => program.home.join(".cache"),
        };
        let home_dirs = xdg_cache_home.join("cubicle").join("home");

        let xdg_data_home = match std::env::var("XDG_DATA_HOME") {
            Ok(path) => HostPath::try_from(path)?,
            Err(_) => program.home.join(".local").join("share"),
        };
        let work_dirs = xdg_data_home.join("cubicle").join("work");

        Ok(Self {
            program,
            home_dirs,
            work_dirs,
        })
    }

    fn config(&self) -> &super::config::Bubblewrap {
        self.program
            .config
            .bubblewrap
            .as_ref()
            .expect("Bubblewrap config needed")
    }
}

fn get_fd_for_child<F>(file: &F) -> Result<String>
where
    F: rustix::fd::AsFd + std::os::unix::io::AsRawFd,
{
    // This is pretty ugly, but it's how bwrap likes it.
    let mut flags = rustix::fs::fcntl_getfd(file)?;
    flags.remove(rustix::fs::FdFlags::CLOEXEC);
    rustix::fs::fcntl_setfd(&file, flags)?;
    Ok(file.as_raw_fd().to_string())
}

fn ro_bind_try(path: &str) -> [&str; 3] {
    ["--ro-bind-try", path, path]
}

impl Runner for Bubblewrap {
    fn copy_out_from_home(
        &self,
        name: &EnvironmentName,
        path: &Path,
        w: &mut dyn io::Write,
    ) -> Result<()> {
        let home_dir = cap_std::fs::Dir::open_ambient_dir(
            &self.home_dirs.join(name).as_host_raw(),
            cap_std::ambient_authority(),
        )?;
        let mut file = home_dir.open(path)?;
        io::copy(&mut file, w)?;
        Ok(())
    }

    fn copy_out_from_work(
        &self,
        name: &EnvironmentName,
        path: &Path,
        w: &mut dyn io::Write,
    ) -> Result<()> {
        let work_dir = cap_std::fs::Dir::open_ambient_dir(
            &self.work_dirs.join(name).as_host_raw(),
            cap_std::ambient_authority(),
        )?;
        let mut file = work_dir.open(path)?;
        io::copy(&mut file, w)?;
        Ok(())
    }

    fn create(&self, name: &EnvironmentName) -> Result<()> {
        std::fs::create_dir_all(&self.home_dirs.as_host_raw())?;
        std::fs::create_dir_all(&self.work_dirs.as_host_raw())?;
        let host_home = self.home_dirs.join(name);
        let host_work = self.work_dirs.join(name);
        std::fs::create_dir(&host_home.as_host_raw())?;
        std::fs::create_dir(&host_work.as_host_raw())?;
        Ok(())
    }

    fn exists(&self, name: &EnvironmentName) -> Result<EnvironmentExists> {
        let has_home_dir = try_exists(&self.home_dirs.join(name))?;
        let has_work_dir = try_exists(&self.work_dirs.join(name))?;

        use EnvironmentExists::*;
        Ok(if has_home_dir && has_work_dir {
            FullyExists
        } else if has_home_dir || has_work_dir {
            PartiallyExists
        } else {
            NoEnvironment
        })
    }

    fn stop(&self, _name: &EnvironmentName) -> Result<()> {
        // don't know how to enumerate such processes, so don't bother
        Ok(())
    }

    fn list(&self) -> Result<Vec<EnvironmentName>> {
        let mut envs = BTreeSet::new();

        for name in try_iterdir(&self.home_dirs)? {
            let env = name
                .to_str()
                .ok_or_else(|| anyhow!("Path not UTF-8: {:?}", self.home_dirs.join(&name)))
                .and_then(EnvironmentName::from_str)?;
            envs.insert(env);
        }

        for name in try_iterdir(&self.work_dirs)? {
            let env = name
                .to_str()
                .ok_or_else(|| anyhow!("Path not UTF-8: {:?}", self.work_dirs.join(&name)))
                .and_then(EnvironmentName::from_str)?;
            envs.insert(env);
        }

        Ok(Vec::from_iter(envs))
    }

    fn files_summary(&self, name: &EnvironmentName) -> Result<EnvFilesSummary> {
        let home_dir = self.home_dirs.join(name);
        let home_dir_exists = try_exists(&home_dir)?;
        let home_dir_summary = if home_dir_exists {
            summarize_dir(&home_dir)?
        } else {
            DirSummary::new_with_errors()
        };

        let work_dir = self.work_dirs.join(name);
        let work_dir_exists = try_exists(&work_dir)?;
        let work_dir_summary = if work_dir_exists {
            summarize_dir(&work_dir)?
        } else {
            DirSummary::new_with_errors()
        };

        Ok(EnvFilesSummary {
            home_dir_path: home_dir_exists.then_some(home_dir),
            home_dir: home_dir_summary,
            work_dir_path: work_dir_exists.then_some(work_dir),
            work_dir: work_dir_summary,
        })
    }

    fn reset(&self, name: &EnvironmentName) -> Result<()> {
        let host_home = self.home_dirs.join(name);
        let host_work = self.work_dirs.join(name);
        rmtree(&host_home)?;
        std::fs::create_dir_all(host_home.as_host_raw())?;
        std::fs::create_dir_all(host_work.as_host_raw())?;
        Ok(())
    }

    fn purge(&self, name: &EnvironmentName) -> Result<()> {
        rmtree(&self.home_dirs.join(name))?;
        rmtree(&self.work_dirs.join(name))
    }

    fn run(&self, name: &EnvironmentName, run_command: &RunnerCommand) -> Result<()> {
        let host_home = self.home_dirs.join(name);
        let host_work = self.work_dirs.join(name);

        struct Seed {
            _child: ScopedChild, // this is here so its destructor will reap it later
            stdout: ChildStdout,
        }
        let seed = match run_command {
            RunnerCommand::Init { seeds, .. } if !seeds.is_empty() => {
                println!("Packing seed tarball");
                let mut child = Command::new("pv")
                    .args(["-i", "0.1"])
                    .args(seeds.iter().map(|s| s.as_host_raw()))
                    .stdout(Stdio::piped())
                    .scoped_spawn()?;
                let stdout = child.stdout.take().unwrap();
                Some(Seed {
                    _child: child,
                    stdout,
                })
            }
            _ => None,
        };

        let seccomp: Option<std::fs::File> = {
            use super::config::PathOrDisabled::*;
            match &self.config().seccomp {
                Path(path) => Some(
                    std::fs::File::open(path)
                        .with_context(|| format!("Failed to open seccomp filter: {path:?}"))?,
                ),
                DangerouslyDisabled => None,
            }
        };

        let mut command = Command::new("bwrap");

        let env_home = EnvPath::try_from(self.program.home.as_host_raw().to_owned())?;
        let init_script = EnvPath::try_from(String::from("/cubicle-init.sh"))?;

        command.env_clear();
        command.env(
            "PATH",
            match self.program.home.as_host_raw().to_str() {
                Some(home) => format!("{home}/bin:/bin:/usr/bin:/sbin:/usr/sbin"),
                None => String::from("/bin:/usr/bin:/sbin:/usr/sbin"),
            },
        );
        command.env("HOME", env_home.as_env_raw());
        command.env("SANDBOX", name);
        command.env("TMPDIR", env_home.join("tmp").as_env_raw());
        for key in ["DISPLAY", "SHELL", "TERM", "USER"] {
            if let Ok(value) = std::env::var(key) {
                command.env(key, value);
            }
        }

        command.arg("--die-with-parent");
        command.arg("--unshare-cgroup");
        command.arg("--unshare-ipc");
        command.arg("--unshare-pid");
        command.arg("--unshare-uts");

        command.arg("--hostname");
        match &self.program.hostname {
            Some(hostname) => command.arg(format!("{name}.{hostname}")),
            None => command.arg(name),
        };

        command.args(["--symlink", "/usr/bin", "/bin"]);
        command.args(["--dev", "/dev"]);

        if let RunnerCommand::Init { script, .. } = run_command {
            command
                .arg("--ro-bind-try")
                .arg(script.as_host_raw())
                .arg(init_script.as_env_raw());
        }

        if let Some(Seed { stdout, .. }) = &seed {
            command
                .arg("--file")
                .arg(get_fd_for_child(stdout)?)
                .arg("/dev/shm/seed.tar");
        }
        command.args(ro_bind_try("/etc"));
        command
            .arg("--bind")
            .arg(host_home.as_host_raw())
            .arg(env_home.as_env_raw());
        command
            .arg("--bind")
            .arg(host_work.as_host_raw())
            .arg(env_home.join("w").as_env_raw());
        command.args(["--symlink", "/usr/lib", "/lib"]);
        command.args(["--symlink", "/usr/lib64", "/lib64"]);
        command.args(ro_bind_try("/opt"));
        command.args(["--proc", "/proc"]);
        command.args(["--symlink", "/usr/sbin", "/sbin"]);
        command.args(["--tmpfs", "/tmp"]);
        command.args(ro_bind_try("/usr"));
        command.args(ro_bind_try("/var/lib/apt/lists"));
        command.args(ro_bind_try("/var/lib/dpkg"));
        if let Some(seccomp) = &seccomp {
            command.arg("--seccomp").arg(get_fd_for_child(seccomp)?);
        }
        command.arg("--chdir").arg(env_home.join("w").as_env_raw());
        command.arg("--");
        command.arg(&self.program.shell);
        command.arg("-l");

        match run_command {
            RunnerCommand::Interactive => {}
            RunnerCommand::Init { .. } => {
                command.arg("-c").arg(init_script.as_env_raw());
            }
            RunnerCommand::Exec(exec) => {
                command.arg("-c");
                command.arg(shlex::join(exec.iter().map(|a| a.as_str())));
            }
        }

        let status = command
            .status()
            .context("Failed to execute bwrap process")?;
        if status.success() {
            Ok(())
        } else {
            Err(ExitStatusError::new(status, "bwrap").into())
        }
    }
}
