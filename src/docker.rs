use lazy_static::lazy_static;
use regex::{Regex, RegexBuilder};
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::fs_util::{rmtree, summarize_dir, try_exists, try_iterdir, DirSummary};
use super::newtype::EnvPath;
use super::os_util::{get_timezone, get_uids, Uids};
use super::runner::{EnvFilesSummary, EnvironmentExists, Runner, RunnerCommand};
use super::scoped_child::ScopedSpawn;
use super::{CubicleShared, EnvironmentName, ExitStatusError, HostPath};
use crate::somehow::{somehow as anyhow, Result};

pub struct Docker {
    pub(super) program: Rc<CubicleShared>,
    timezone: String,
    mounts: Mounts,
    base_image: ImageName,
    container_home: EnvPath,
    /// Flag used to build the base image when it's first needed after the
    /// program starts up, and probably not again after that.
    built_base: Cell<bool>,
}

enum Mounts {
    BindMounts {
        home_dirs: HostPath,
        work_dirs: HostPath,
    },
    Volumes,
}

use Mounts::{BindMounts, Volumes};

mod newtypes {
    use super::super::newtype;
    newtype::name!(ContainerName);
    newtype::name!(ImageName);
    newtype::name!(VolumeName);
}
use newtypes::{ContainerName, ImageName, VolumeName};

impl Docker {
    pub(super) fn new(program: Rc<CubicleShared>) -> Result<Self> {
        let timezone = get_timezone();

        let mounts = if program.config.docker.bind_mounts {
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
            BindMounts {
                home_dirs,
                work_dirs,
            }
        } else {
            Volumes
        };

        let base_image = ImageName::new(format!("{}cubicle-base", program.config.docker.prefix));

        let container_home = EnvPath::try_from(String::from("/home"))
            .unwrap()
            .join(&program.user);

        Ok(Self {
            program,
            timezone,
            mounts,
            base_image,
            container_home,
            built_base: Cell::new(false),
        })
    }

    fn container_from_environment(&self, env: &EnvironmentName) -> ContainerName {
        ContainerName::new(format!("{}{}", self.program.config.docker.prefix, env))
    }

    fn home_volume(&self, env: &EnvironmentName) -> VolumeName {
        assert!(matches!(self.mounts, Volumes));
        VolumeName::new(format!("{}{}-home", self.program.config.docker.prefix, env))
    }

    fn work_volume(&self, env: &EnvironmentName) -> VolumeName {
        assert!(matches!(self.mounts, Volumes));
        VolumeName::new(format!("{}{}-work", self.program.config.docker.prefix, env))
    }

    fn is_container(&self, name: &ContainerName) -> Result<bool> {
        let status = Command::new("docker")
            .args(["inspect", "--type", "container", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }

    fn ps(&self) -> Result<Vec<EnvironmentName>> {
        let output = Command::new("docker")
            .args(["ps", "--all", "--format", "{{ .Names }}"])
            .output()?;
        let status = output.status;
        if !status.success() {
            return Err(anyhow!(
                "Failed to list Docker containers: \
                docker ps exited with status {:?} and output: {}",
                status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let mut envs = Vec::new();
        for line in output.stdout.lines() {
            let line = line?;
            if let Some(name) = line.strip_prefix(&self.program.config.docker.prefix) {
                if let Ok(env) = EnvironmentName::from_str(name) {
                    envs.push(env);
                }
            }
        }
        Ok(envs)
    }

    fn base_mtime(&self) -> Result<Option<SystemTime>> {
        let mut command = Command::new("docker");
        command.arg("inspect");
        command.args(["--type", "image"]);
        command.args(["--format", "{{ $.Metadata.LastTagTime.Unix }}"]);
        command.arg(&self.base_image);
        let output = command.output()?;
        let status = output.status;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if status.code() == Some(1) && stderr.starts_with("Error: No such image") {
                return Ok(None);
            }
            return Err(anyhow!(
                "Failed to get last build time for {} Docker image: \
                docker inspect exited with status {:?} and output: {}",
                self.base_image,
                status.code(),
                stderr
            ));
        }
        let timestamp: String = String::from_utf8(output.stdout)?;
        let timestamp: u64 = u64::from_str(timestamp.trim())?;
        Ok(Some(UNIX_EPOCH + Duration::from_secs(timestamp)))
    }

    fn build_base(&self) -> Result<()> {
        // These checks on the image timestamp are a little silly, since this
        // program is short-lived. They used to make more sense when the
        // Dockerfile was a normal file. They might well make more sense again
        // in the future, so it's good to keep this code active.
        let base_mtime = self.base_mtime()?.unwrap_or(UNIX_EPOCH);
        let image_fresh =
            base_mtime.elapsed().unwrap_or(Duration::ZERO) < Duration::from_secs(60 * 60 * 12);
        if image_fresh && self.built_base.get() {
            return Ok(());
        }

        let mut child = Command::new("docker")
            .args(["build", "--tag", &self.base_image, "-"])
            .stdin(Stdio::piped())
            .scoped_spawn()?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("Failed to open stdin"))?;

            let mut packages: BTreeSet<&str> = BTreeSet::from_iter(SLIM_PACKAGES.iter().cloned());
            if !self.program.config.docker.slim {
                packages.extend(NORMAL_PACKAGES);
                packages.extend(DEPENDENCIES_PACKAGES);
            }
            write_dockerfile(
                &mut stdin,
                DockerfileArgs {
                    packages: &packages,
                    timezone: &self.timezone,
                    user: &self.program.user,
                    uids: &get_uids(),
                },
            )?;
            stdin.flush()?;
        }

        let status = child.wait()?;
        if !status.success() {
            return Err(anyhow!(
                "Failed to build {} Docker image: \
                docker build exited with status {:?}",
                self.base_image,
                status.code(),
            ));
        }

        self.built_base.set(true);
        Ok(())
    }

    fn spawn(&self, env_name: &EnvironmentName) -> Result<()> {
        let container_name = self.container_from_environment(env_name);
        let seccomp_json = self.program.script_path.join("seccomp.json");
        let mut command = Command::new("docker");
        command.arg("run");
        command.arg("--detach");
        command.args(["--env", &format!("SANDBOX={}", env_name)]);
        command.arg("--hostname");
        match &self.program.hostname {
            Some(hostname) => command.arg(format!("{env_name}.{hostname}")),
            None => command.arg(env_name),
        };
        command.arg("--init");
        command.args(["--name", &container_name]);
        command.arg("--rm");
        if try_exists(&seccomp_json)? {
            command.args([
                "--security-opt",
                &format!("seccomp={}", seccomp_json.as_host_raw().display()),
            ]);
        }
        // The default `/dev/shm` is limited to only 64 MiB under
        // Docker (v20.10.5), which causes many crashes in Chromium
        // and Electron-based programs. See
        // <https://github.com/ongardie/cubicle/issues/3>.
        command.args(["--shm-size", &1_000_000_000.to_string()]);
        command.args(["--user", &self.program.user]);

        command.args(["--volume", "/tmp/.X11-unix:/tmp/.X11-unix:ro"]);

        let container_home_str = self
            .container_home
            .as_env_raw()
            .to_str()
            .ok_or_else(|| anyhow!("path not valid UTF-8: {:#?}", self.program.home))?;

        let container_work = self.container_home.join("w");
        let container_work_str = container_work
            .as_env_raw()
            .to_str()
            .ok_or_else(|| anyhow!("path not valid UTF-8: {:#?}", container_work))?;

        match &self.mounts {
            BindMounts {
                home_dirs,
                work_dirs,
            } => {
                let host_home = home_dirs.join(env_name);
                let host_work = work_dirs.join(env_name);

                command.args([
                    "--mount",
                    &format!(
                        r#""type=bind","source={}","target={}""#,
                        host_home
                            .as_host_raw()
                            .to_str()
                            .ok_or_else(|| anyhow!("path not valid UTF-8: {:#?}", host_home))?,
                        container_home_str,
                    ),
                ]);
                command.args([
                    "--mount",
                    &format!(
                        r#""type=bind","source={}","target={}""#,
                        host_work
                            .as_host_raw()
                            .to_str()
                            .ok_or_else(|| anyhow!("path not valid UTF-8: {:#?}", host_work))?,
                        container_work_str,
                    ),
                ]);
            }

            Volumes => {
                command.args([
                    "--mount",
                    &format!(
                        r#""type=volume","source={}","target={}""#,
                        self.home_volume(env_name),
                        container_home_str,
                    ),
                ]);
                command.args([
                    "--mount",
                    &format!(
                        r#""type=volume","source={}","target={}""#,
                        self.work_volume(env_name),
                        container_work_str,
                    ),
                ]);
            }
        }

        command.arg("--workdir").arg(&container_work.as_env_raw());
        command.arg(&self.base_image);
        command.args(["sleep", "90d"]);
        command.stdout(Stdio::null());
        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(ExitStatusError::new(status, "docker run").into())
        }
    }

    fn list_volumes(&self) -> Result<Vec<VolumeName>> {
        let output = Command::new("docker")
            .args(["volume", "ls", "--format", "{{ .Name }}"])
            .output()?;
        let status = output.status;
        if !status.success() {
            return Err(anyhow!(
                "Failed to list Docker volumes: \
                docker volume ls exited with status {:?} and output: {}",
                status.code(),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        output
            .stdout
            .lines()
            .map(|line| line.map(VolumeName::new).map_err(|e| e.into()))
            .collect()
    }

    fn volume_exists(&self, name: &VolumeName) -> Result<bool> {
        self.volume_mountpoint(name).map(|o| o.is_some())
    }

    fn volume_mountpoint(&self, name: &VolumeName) -> Result<Option<HostPath>> {
        let output = Command::new("docker")
            .arg("volume")
            .arg("inspect")
            .args(["--format", "{{ .Mountpoint }}"])
            .arg(&name)
            .output()?;
        let status = output.status;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if status.code() == Some(1) && stderr.starts_with("Error: No such volume") {
                return Ok(None);
            }
            return Err(anyhow!(
                "Failed to inspect Docker volume {}: \
                docker volume inspect exited with status {:?} and stderr: {}",
                name,
                status.code(),
                stderr
            ));
        }
        let stdout = String::from_utf8(output.stdout)?.trim().to_owned();
        Ok(Some(HostPath::try_from(stdout)?))
    }

    fn volume_du(&self, name: &VolumeName) -> Result<DirSummary> {
        let output = Command::new("docker")
            .arg("run")
            .arg("--mount")
            .arg(format!(r#""type=volume","source={name}","target=/v""#))
            .arg("--rm")
            .arg("debian:11")
            .arg("du")
            .arg("--block-size=1")
            .arg("--summarize")
            .arg("--time")
            .arg("--time-style=+%s")
            .arg("/v")
            .output()?;

        // ignore permissions errors
        let errors = !&output.stderr.is_empty();

        let status = output.status;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(anyhow!(
                "Failed to summarize disk usage of Docker volume {}: \
                docker run du exited with status {:?} and stderr: {}",
                name,
                status.code(),
                stderr
            ));
        }

        let stdout = String::from_utf8(output.stdout)?;

        lazy_static! {
            static ref RE: Regex =
                RegexBuilder::new(r#"^(?P<size>[0-9]+)\t(?P<mtime>[0-9]+)\t/v$"#)
                    .build()
                    .unwrap();
        }
        match RE.captures(stdout.trim_end()) {
            Some(caps) => {
                let size = caps.name("size").unwrap().as_str();
                let size = u64::from_str(size).unwrap();
                let mtime = caps.name("mtime").unwrap().as_str();
                let mtime = u64::from_str(mtime).unwrap();
                let mtime = UNIX_EPOCH + Duration::from_secs(mtime);
                Ok(DirSummary {
                    errors,
                    total_size: size,
                    last_modified: mtime,
                })
            }
            None => Err(anyhow!("Unexpected output from du: {:#?}", stdout)),
        }
    }

    fn ensure_volume_exists(&self, name: &VolumeName) -> Result<()> {
        let status = Command::new("docker")
            .arg("volume")
            .arg("create")
            .arg(&name)
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(anyhow!(
                "Failed to create Docker volume {}: \
                docker volume create exited with status {:?}",
                name,
                status.code(),
            ));
        }
        Ok(())
    }

    fn ensure_no_volume(&self, name: &VolumeName) -> Result<()> {
        let status = Command::new("docker")
            .arg("volume")
            .arg("rm")
            .arg("--force")
            .arg(&name)
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(anyhow!(
                "Failed to remove Docker volume {}: \
                docker volume rm exited with status {:?}",
                name,
                status.code(),
            ));
        }
        Ok(())
    }

    fn docker_cp_out_from_root(
        &self,
        env_name: &EnvironmentName,
        abs_path: &EnvPath,
        w: &mut dyn io::Write,
    ) -> Result<()> {
        let container_name = self.container_from_environment(env_name);
        if !self.is_container(&container_name)? {
            self.build_base()?;
            self.spawn(env_name)?;
        }

        let abs_path_str = abs_path
            .as_env_raw()
            .to_str()
            .ok_or_else(|| anyhow!("path not valid UTF-8: {abs_path:?}"))?;
        let mut child = Command::new("docker")
            .arg("cp")
            .arg(format!("{container_name}:{abs_path_str}",))
            .arg("-")
            .stdout(Stdio::piped())
            .scoped_spawn()?;

        let stdout = child.stdout.take().unwrap();

        // Unfortunately, we get a tar file here, since we specified "-".
        let mut archive = tar::Archive::new(stdout);
        let mut entries = archive.entries()?;
        let mut entry = entries
            .next()
            .transpose()?
            .ok_or_else(|| anyhow!("tar file had no entries, expected 1"))?;
        io::copy(&mut entry, w)?;
        if entries.next().is_some() {
            return Err(anyhow!("tar file had multiple entries, expected 1"));
        }

        let status = child.wait()?;
        if !status.success() {
            return Err(anyhow!(
                "Failed to copy file {:?} from Docker container {}. \
                docker cp exited with status {:?}",
                abs_path,
                container_name,
                status.code(),
            ));
        }
        Ok(())
    }
}

impl Runner for Docker {
    fn copy_out_from_home(
        &self,
        env_name: &EnvironmentName,
        path: &Path,
        w: &mut dyn io::Write,
    ) -> Result<()> {
        match &self.mounts {
            BindMounts { home_dirs, .. } => {
                let home_dir = cap_std::fs::Dir::open_ambient_dir(
                    &home_dirs.join(env_name).as_host_raw(),
                    cap_std::ambient_authority(),
                )?;
                let mut file = home_dir.open(path)?;
                io::copy(&mut file, w)?;
                Ok(())
            }
            Volumes => {
                let abs_path = self.container_home.join(path);
                self.docker_cp_out_from_root(env_name, &abs_path, w)
            }
        }
    }

    fn copy_out_from_work(
        &self,
        env_name: &EnvironmentName,
        path: &Path,
        w: &mut dyn io::Write,
    ) -> Result<()> {
        match &self.mounts {
            BindMounts { work_dirs, .. } => {
                let work_dir = cap_std::fs::Dir::open_ambient_dir(
                    &work_dirs.join(env_name).as_host_raw(),
                    cap_std::ambient_authority(),
                )?;
                let mut file = work_dir.open(path)?;
                io::copy(&mut file, w)?;
                Ok(())
            }
            Volumes => {
                let abs_path = self.container_home.join("w").join(path);
                self.docker_cp_out_from_root(env_name, &abs_path, w)
            }
        }
    }

    fn create(&self, env_name: &EnvironmentName) -> Result<()> {
        let container_name = self.container_from_environment(env_name);
        if self.is_container(&container_name)? {
            return Err(anyhow!("Docker container {container_name} already exists"));
        }
        match &self.mounts {
            BindMounts {
                home_dirs,
                work_dirs,
            } => {
                std::fs::create_dir_all(&home_dirs.as_host_raw())?;
                std::fs::create_dir_all(&work_dirs.as_host_raw())?;
                let host_home = home_dirs.join(env_name);
                let host_work = work_dirs.join(env_name);
                std::fs::create_dir(&host_home.as_host_raw())?;
                std::fs::create_dir(&host_work.as_host_raw())?;
                Ok(())
            }
            Volumes => {
                self.ensure_volume_exists(&self.home_volume(env_name))?;
                self.ensure_volume_exists(&self.work_volume(env_name))
            }
        }
    }

    fn exists(&self, env_name: &EnvironmentName) -> Result<EnvironmentExists> {
        let container_name = self.container_from_environment(env_name);
        let is_container = self.is_container(&container_name)?;

        let has_home_dir;
        let has_work_dir;
        match &self.mounts {
            BindMounts {
                home_dirs,
                work_dirs,
            } => {
                has_home_dir = try_exists(&home_dirs.join(env_name))?;
                has_work_dir = try_exists(&work_dirs.join(env_name))?;
            }
            Volumes => {
                has_home_dir = self.volume_exists(&self.home_volume(env_name))?;
                has_work_dir = self.volume_exists(&self.work_volume(env_name))?;
            }
        }

        use EnvironmentExists::*;
        Ok(if has_home_dir && has_work_dir {
            FullyExists
        } else if is_container || has_home_dir || has_work_dir {
            PartiallyExists
        } else {
            NoEnvironment
        })
    }

    fn stop(&self, env_name: &EnvironmentName) -> Result<()> {
        let container_name = self.container_from_environment(env_name);
        if self.is_container(&container_name)? {
            let status = Command::new("docker")
                .args(["rm", "--force", &container_name])
                .stdout(Stdio::null())
                .status()?;
            if !status.success() {
                return Err(anyhow!(
                    "Failed to stop Docker container {}: \
                    docker rm exited with status {:?}",
                    container_name,
                    status.code(),
                ));
            }
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<EnvironmentName>> {
        let mut envs = BTreeSet::from_iter(self.ps()?);

        match &self.mounts {
            BindMounts {
                home_dirs,
                work_dirs,
            } => {
                for name in try_iterdir(home_dirs)? {
                    let env = name
                        .to_str()
                        .ok_or_else(|| anyhow!("Path not UTF-8: {:?}", home_dirs.join(&name)))
                        .and_then(EnvironmentName::from_str)?;
                    envs.insert(env);
                }

                for name in try_iterdir(work_dirs)? {
                    let env = name
                        .to_str()
                        .ok_or_else(|| anyhow!("Path not UTF-8: {:?}", work_dirs.join(&name)))
                        .and_then(EnvironmentName::from_str)?;
                    envs.insert(env);
                }
            }
            Volumes => {
                for name in self.list_volumes()? {
                    if let Some(name) = name.strip_prefix(&self.program.config.docker.prefix) {
                        if let Some(env) = name.strip_suffix("-home") {
                            envs.insert(EnvironmentName::from_str(env)?);
                        }
                        if let Some(env) = name.strip_suffix("-work") {
                            envs.insert(EnvironmentName::from_str(env)?);
                        }
                    }
                }
            }
        }

        Ok(Vec::from_iter(envs))
    }

    fn files_summary(&self, name: &EnvironmentName) -> Result<EnvFilesSummary> {
        match &self.mounts {
            BindMounts {
                home_dirs,
                work_dirs,
            } => {
                let home_dir = home_dirs.join(name);
                let home_dir_exists = try_exists(&home_dir)?;
                let home_dir_summary = if home_dir_exists {
                    summarize_dir(&home_dir)?
                } else {
                    DirSummary::new_with_errors()
                };

                let work_dir = work_dirs.join(name);
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

            Volumes => {
                let home_volume = self.home_volume(name);
                let work_volume = self.work_volume(name);
                Ok(EnvFilesSummary {
                    home_dir_path: self.volume_mountpoint(&home_volume)?,
                    home_dir: self.volume_du(&home_volume)?,
                    work_dir_path: self.volume_mountpoint(&work_volume)?,
                    work_dir: self.volume_du(&work_volume)?,
                })
            }
        }
    }

    fn reset(&self, name: &EnvironmentName) -> Result<()> {
        self.stop(name)?;
        match &self.mounts {
            BindMounts { home_dirs, .. } => {
                let host_home = home_dirs.join(name);
                rmtree(&host_home)?;
                std::fs::create_dir(&host_home.as_host_raw())?;
            }
            Volumes => {
                let home_volume = self.home_volume(name);
                self.ensure_no_volume(&home_volume)?;
                self.ensure_volume_exists(&home_volume)?;
            }
        }
        Ok(())
    }

    fn purge(&self, name: &EnvironmentName) -> Result<()> {
        self.stop(name)?;
        match &self.mounts {
            BindMounts {
                home_dirs,
                work_dirs,
            } => {
                rmtree(&home_dirs.join(name))?;
                rmtree(&work_dirs.join(name))
            }
            Volumes => {
                self.ensure_no_volume(&self.home_volume(name))?;
                self.ensure_no_volume(&self.work_volume(name))
            }
        }
    }

    fn run(&self, env_name: &EnvironmentName, run_command: &RunnerCommand) -> Result<()> {
        let container_name = self.container_from_environment(env_name);
        if !self.is_container(&container_name)? {
            self.build_base()?;
            self.spawn(env_name)?;
        }

        let script_path = EnvPath::try_from(String::from("/.cubicle-init")).unwrap();

        if let RunnerCommand::Init { script, seeds } = run_command {
            let status = Command::new("docker")
                .arg("cp")
                .arg(script.as_host_raw())
                .arg(format!(
                    "{}:{}",
                    container_name,
                    script_path.as_env_raw().to_str().unwrap()
                ))
                .status()?;
            if !status.success() {
                return Err(anyhow!(
                    "Failed to copy init script into Docker container: \
                    docker cp exited with status {:?}",
                    status.code(),
                ));
            }

            if !seeds.is_empty() {
                println!("Copying/extracting seed tarball");
                // Use pv from inside the container since it may not be
                // installed on the host. Since it's reading from a stream, it
                // needs to know the total size to display a good progress bar.
                #[cfg(not(unix))]
                let size: Option<u64> = None;
                #[cfg(unix)]
                let size: Option<u64> = Some({
                    let mut size: u64 = 0;
                    for path in seeds {
                        use std::os::unix::fs::MetadataExt;
                        let metadata = std::fs::metadata(path.as_host_raw())?;
                        size += metadata.size();
                    }
                    size
                });

                let mut child = Command::new("docker")
                    .arg("exec")
                    .arg("--interactive")
                    .arg(&container_name)
                    .args([
                        "sh",
                        "-c",
                        &format!(
                            "pv --interval 0.1 --force {} | \
                            tar --ignore-zero --directory ~ --extract",
                            match size {
                                Some(size) => format!("--size {size}"),
                                None => String::from(""),
                            },
                        ),
                    ])
                    .stdin(Stdio::piped())
                    .scoped_spawn()?;
                {
                    let mut stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| anyhow!("failed to open stdin"))?;
                    for path in seeds {
                        let mut file = std::fs::File::open(path.as_host_raw())?;
                        io::copy(&mut file, &mut stdin)?;
                    }
                }
                let status = child.wait()?;
                if !status.success() {
                    return Err(anyhow!(
                        "Failed to copy package seeds into Docker container {}: \
                        docker exec (pv | tar) exited with status {:?}",
                        container_name,
                        status.code(),
                    ));
                }
            }
        }

        let mut command = Command::new("docker");
        command.arg("exec");
        command.args(["--env", "DISPLAY"]);
        command
            .arg("--env")
            .arg(fallback_path(&self.container_home));
        command.args(["--env", "SHELL"]);
        command.args(["--env", "USER"]);
        command.args(["--env", "TERM"]);
        command.arg("--interactive");

        // If we really don't have a TTY, Docker will exit with status 1 when
        // we request one.
        if atty::is(atty::Stream::Stdin)
            || atty::is(atty::Stream::Stdout)
            || atty::is(atty::Stream::Stderr)
        {
            command.arg("--tty");
        }

        command.arg(&container_name);
        command.args([&self.program.shell, "-l"]);
        match run_command {
            RunnerCommand::Interactive => {}
            RunnerCommand::Init { .. } => {
                command.arg("-c").arg(script_path.as_env_raw());
            }
            RunnerCommand::Exec(exec) => {
                command.arg("-c");
                command.arg(shlex::join(exec.iter().map(|a| a.as_str())));
            }
        }

        let status = command.status()?;
        if status.success() {
            Ok(())
        } else {
            Err(ExitStatusError::new(status, "docker exec").into())
        }
    }
}

fn fallback_path(container_home: &EnvPath) -> OsString {
    let home_bin = container_home.join("bin");
    let paths = [
        home_bin.as_env_raw(),
        // The debian:11 image hasn't gone through usrmerge, so
        // /usr/bin and /bin are distinct there.
        Path::new("/bin"),
        Path::new("/sbin"),
        Path::new("/usr/bin"),
        Path::new("/usr/sbin"),
    ];
    let joined = match std::env::join_paths(&paths) {
        Ok(joined) => joined,
        Err(e) => {
            println!(
                "Warning: unable to add container home dir ({container_home:?}) to $PATH: {e}"
            );
            std::env::join_paths(&paths[1..]).unwrap()
        }
    };
    [OsStr::new("PATH="), &joined].into_iter().collect()
}

/// Debian packages that many packages might depend on for basic functionality.
/// They are installed in the CI system.
const SLIM_PACKAGES: &[&str] = &[
    "curl", "git", "jq", "lz4", "procps", "pv", "sudo", "vim", "wget", "zip", "zstd", "zsh",
];

/// Debian packages that many users may like. To save time, they are not
/// normally installed in the CI system.
const NORMAL_PACKAGES: &[&str] = &[
    "apt-file",
    "bash-completion",
    "bind9-dnsutils",
    "build-essential",
    "dialog",
    "eatmydata",
    "file",
    "iproute2",
    "iputils-ping",
    "less",
    "man-db",
    "manpages",
    "manpages-posix-dev",
    "manpages-dev",
    "net-tools",
    "ripgrep",
    "rsync",
    "sqlite3",
    "strace",
    "time",
    "tree",
    "xdg-utils",
    "zsh-autosuggestions",
    "zsh-syntax-highlighting",
];

/// Debian packages that some of the Cubicle packages depend on. Because
/// there's no way for them to express that yet, they go here for now.
const DEPENDENCIES_PACKAGES: &[&str] = &[
    // for Python
    "build-essential",
    "gdb",
    "lcov",
    "libbz2-dev",
    "libffi-dev",
    "libgdbm-compat-dev",
    "libgdbm-dev",
    "liblzma-dev",
    "libncurses5-dev",
    "libreadline6-dev",
    "libsqlite3-dev",
    "libssl-dev",
    "lzma",
    "lzma-dev",
    "pkg-config",
    "tk-dev",
    "uuid-dev",
    "zlib1g-dev",
    // for firefox and vscodium
    "libasound2",
    "libatk-bridge2.0-0",
    "libatk1.0-0",
    "libcups2",
    "libdbus-glib-1-2",
    "libdrm2",
    "libegl1",
    "libgbm1",
    "libglib2.0-0",
    "libgtk-3-0",
    "libnss3",
    "libpci3",
    "x11-utils",
    // for mold and rust
    "bsdmainutils",
    "cmake",
    "clang",
];

struct DockerfileArgs<'a> {
    packages: &'a BTreeSet<&'a str>,
    timezone: &'a str,
    user: &'a str,
    uids: &'a Uids,
}

fn write_dockerfile<W: io::Write>(w: &mut W, args: DockerfileArgs) -> Result<()> {
    // Quote all the Strings that go into the file.
    let packages: Vec<String> = args
        .packages
        .iter()
        .map(|p| shlex::quote(p).into_owned())
        .collect();
    let timezone = shlex::quote(args.timezone);
    let user = shlex::quote(args.user);
    let has_apt_file = args.packages.contains("apt-file");
    let has_sudo = args.packages.contains("sudo");
    let uid = args.uids.real_user;
    let gid = args.uids.group;

    // Don't let the code below here access unquoted 'args'.
    #[allow(clippy::drop_non_drop)]
    std::mem::drop(args);

    // Note: If we wanted to trim this down even more for CI, we might be able
    // to use the '11-slim' base image here.
    writeln!(w, "FROM debian:11")?;

    // Set time zone.
    writeln!(w, "RUN echo {timezone} > /etc/timezone && \\")?;
    writeln!(
        w,
        "    ln -fs '/usr/share/zoneinfo/'{timezone} /etc/localtime"
    )?;

    // Set up a user account. Use the same UID as the host because that makes
    // the file permissions usable for bind mounts. The Debian convention is to
    // have a group with the same name as the user and put the user in it. Some
    // hosts use a GID with a small number for many users (GitHub Actions Mac
    // OS appears to have GID 20). If the group ID is taken on the Debian image
    // already, this falls back to any available GID, even if the group
    // permissions end up wonky for bind mounts.
    writeln!(
        w,
        "RUN addgroup --gid {gid} {user} || addgroup {user} && \\"
    )?;
    //
    // Prevent using gid below.
    #[allow(unused)]
    let gid: ();
    //
    writeln!(
        w,
        "    adduser --disabled-password --gecos '' --uid {uid} --ingroup {user} {user} && \\",
    )?;
    writeln!(w, "    adduser {user} sudo && \\")?;
    // For a Docker volume to be owned/writable by a regular user, a directory
    // needs to exist there before the volume is mounted. See
    // <https://github.com/moby/moby/issues/2259>.
    writeln!(w, "    mkdir /home/{user}/w && \\")?;
    writeln!(w, "    chown {user}:{user} /home/{user}/w")?;

    // Configure and Update apt.
    writeln!(
        w,
        r#"RUN sed -i 's/ main$/ main contrib non-free/' /etc/apt/sources.list"#
    )?;
    writeln!(w, "RUN apt-get update && apt-get upgrade -y")?;

    // Install requested packages.
    if let Some((last, init)) = packages.split_last() {
        writeln!(w, "RUN apt-get install -y \\")?;
        for package in init {
            writeln!(w, "    {package} \\")?;
        }
        writeln!(w, "    {last}")?;
    }

    // Update lists of package contents (after 'apt-file' is installed).
    if has_apt_file {
        writeln!(w, "RUN apt-file update")?;
    }

    // Configure sudo (after 'sudo' is installed, which creates the directory
    // with the right permissions).
    if has_sudo {
        writeln!(
            w,
            r#"RUN sh -c 'echo "Defaults umask = 0027" > /etc/sudoers.d/umask' && \"#
        )?;
        writeln!(
            w,
            r#"    sh -c 'echo "%sudo ALL=(ALL) CWD=* NOPASSWD: ALL" > /etc/sudoers.d/nopasswd'"#
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use std::path::PathBuf;

    #[test]
    fn fallback_path() {
        assert_snapshot!(
            super::fallback_path(&EnvPath::try_from(PathBuf::from("/home/foo")).unwrap()).to_string_lossy(),
            @"PATH=/home/foo/bin:/bin:/sbin:/usr/bin:/usr/sbin"
        );
        assert_snapshot!(
            super::fallback_path(&EnvPath::try_from(PathBuf::from("/home/fo:oo")).unwrap()).to_string_lossy(),
            @"PATH=/bin:/sbin:/usr/bin:/usr/sbin"
        );
    }

    #[test]
    fn write_dockerfile() {
        let mut buf: Vec<u8> = Vec::new();
        super::write_dockerfile(
            &mut buf,
            DockerfileArgs {
                packages: &BTreeSet::from(["apt-file", "pack#age1", "package2", "sudo"]),
                timezone: "Etc/Timez'one",
                user: "h#x*r",
                uids: &Uids {
                    real_user: 1337,
                    group: 7331,
                },
            },
        )
        .unwrap();
        let dockerfile = String::from_utf8(buf).unwrap();
        assert_snapshot!("Dockerfile", dockerfile);
    }
}
