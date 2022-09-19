use clap::ValueEnum;
use serde::Serialize;
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Display};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;
use tempfile::NamedTempFile;

use crate::somehow::{somehow as anyhow, warn, Context, Error, LowLevelResult, Result};

use super::fs_util::{
    create_tar_from_dir, file_size, summarize_dir, try_exists, try_iterdir, DirSummary, TarOptions,
};
use super::runner::{EnvironmentExists, Init, Runner, RunnerCommand};
use super::{rel_time, time_serialize_opt, Bytes, Cubicle, EnvironmentName, HostPath, RunnerKind};

mod manifest;
use manifest::{Dependency, Manifest};

/// Information about a package's source files.
pub struct PackageSpec {
    manifest: Manifest,
    dir: HostPath,
    origin: String,
    update: Option<String>,
    test: Option<String>,
}

/// Information about all available package sources.
///
/// Some package-related methods in [`Cubicle`] need this. Use
/// [`Cubicle::scan_packages`] to build one.
pub type PackageSpecs = BTreeMap<PackageName, PackageSpec>;

/// Used in [`Cubicle::update_packages`] to describe when packages should be
/// updated.
pub struct UpdatePackagesConditions {
    /// When should the named packages' transitive dependencies and
    /// build-dependencies be updated?
    pub dependencies: ShouldPackageUpdate,
    /// When should the given packages themselves be updated?
    pub named: ShouldPackageUpdate,
}

/// Describes when a package should be updated.
///
/// See [`Cubicle::update_packages`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShouldPackageUpdate {
    /// The package should be re-built.
    Always,

    /// The package should be re-built if:
    /// - It has not been successfully built for over
    ///   [`Config::auto_update`](crate::Config::auto_update) time,
    /// - Its source files have been updated since it was built, or
    /// - One of its transitive dependencies has been updated since it was
    ///   built.
    IfStale,

    /// The package should be built only if it's never successfully been built
    /// before.
    IfRequired,
}

#[derive(Clone, Copy)]
struct BuildDepends(bool);

fn transitive_depends(
    packages: &BTreeSet<FullPackageName>,
    specs: &PackageSpecs,
    build_depends: BuildDepends,
) -> Result<BTreeSet<FullPackageName>> {
    struct Visitor<'a> {
        specs: &'a PackageSpecs,
        build_depends: BuildDepends,
        visited: BTreeSet<FullPackageName>,
    }

    impl<'a> Visitor<'a> {
        fn visit(
            &mut self,
            p: &FullPackageName,
            needed_by: Option<&FullPackageName>,
        ) -> Result<()> {
            if !self.visited.contains(p) {
                self.visited.insert(p.clone());
                let spec = match &p.0 {
                    PackageNamespace::Debian => {
                        return Ok(());
                    }
                    PackageNamespace::Root => {
                        self.specs.get(&p.1).ok_or_else(|| match needed_by {
                            Some(other) => {
                                anyhow!(
                                    "could not find package definition for {p}, needed by {other}"
                                )
                            }
                            None => anyhow!("could not find package definition for {p}"),
                        })?
                    }
                    PackageNamespace::Managed(manager) => {
                        let spec = self.specs.get(manager).ok_or_else(|| match needed_by {
                        Some(other) => {
                            anyhow!("could not find package definition for package manager {}, needed by {other}", p.0)
                        }
                        None => anyhow!("could not find package definition for {p}"),
                    })?;
                        if !spec.manifest.package_manager {
                            return Err(anyhow!("package {} is not a package manager", p.0));
                        }
                        spec
                    }
                };
                for (ns, table) in &spec.manifest.depends {
                    for name in table.keys() {
                        self.visit(&FullPackageName(ns.clone(), name.clone()), Some(p))?;
                    }
                }
                if self.build_depends.0 {
                    for (ns, table) in &spec.manifest.build_depends {
                        for name in table.keys() {
                            self.visit(&FullPackageName(ns.clone(), name.clone()), Some(p))?;
                        }
                    }
                }
            }
            Ok(())
        }
    }

    let mut visitor = Visitor {
        specs,
        build_depends,
        visited: BTreeSet::new(),
    };
    for p in packages.iter() {
        visitor.visit(p, None)?;
    }
    Ok(visitor.visited)
}

impl Cubicle {
    pub(super) fn resolve_debian_packages(
        &self,
        packages: &BTreeSet<FullPackageName>,
        specs: &PackageSpecs,
    ) -> Result<BTreeSet<PackageName>> {
        let strict = match self.shared.config.runner {
            RunnerKind::Bubblewrap => true,
            RunnerKind::Docker => self.shared.config.docker.strict_debian_packages,
            RunnerKind::User => true,
        };
        if strict {
            strict_debian_packages(packages, specs)
        } else {
            all_debian_packages(specs)
        }
    }

    fn add_packages(
        &self,
        packages: &mut PackageSpecs,
        dir: &HostPath,
        origin: &str,
    ) -> Result<()> {
        for name in try_iterdir(dir)? {
            let name = match name.to_str() {
                Some(name) => PackageName::from_str(name)?,
                None => {
                    return Err(anyhow!(
                        "package names must be valid UTF-8, found {name:#?} in {dir:#?}"
                    ))
                }
            };
            if packages.contains_key(&name) {
                continue;
            }
            let dir = dir.join(&name.0);

            let mut manifest = match Manifest::read(&dir, "package.toml").with_context(|| {
                format!(
                    "could not read manifest for package {name}: {:?}",
                    dir.join("package.toml").as_host_raw()
                )
            })? {
                Some(manifest) => manifest,
                None => {
                    warn(anyhow!(
                        "no manifest found for package {name}: missing {:?}",
                        dir.join("package.toml").as_host_raw()
                    ));
                    continue;
                }
            };

            manifest
                .depends
                .get_mut(&PackageNamespace::Root)
                .unwrap()
                .insert(PackageName::from_str("auto").unwrap(), Dependency {});

            let test = try_exists(&dir.join("test.sh"))
                .todo_context()?
                .then_some(String::from("./test.sh"));
            let update = try_exists(&dir.join("update.sh"))
                .todo_context()?
                .then_some(String::from("./update.sh"));
            packages.insert(
                name,
                PackageSpec {
                    manifest,
                    dir,
                    origin: origin.to_owned(),
                    test,
                    update,
                },
            );
        }
        Ok(())
    }

    /// Returns a list of available packages.
    pub fn get_package_names(&self) -> Result<BTreeSet<FullPackageName>> {
        let mut names = BTreeSet::new();
        let mut add = |dir: &HostPath| -> Result<()> {
            for name in try_iterdir(dir)? {
                if let Some(name) = name.to_str().and_then(|s| PackageName::from_str(s).ok()) {
                    names.insert(FullPackageName(PackageNamespace::Root, name));
                }
            }
            Ok(())
        };
        for dir in try_iterdir(&self.shared.user_package_dir)? {
            add(&self.shared.user_package_dir.join(dir))?;
        }
        add(&self.shared.code_package_dir)?;

        names.extend(
            try_iterdir(&self.shared.package_cache)?
                .iter()
                .filter_map(|filename| {
                    filename
                        .to_str()
                        .and_then(|filename| filename.strip_suffix(".tar"))
                        .and_then(|prefix| FullPackageName::from_str(prefix).ok())
                }),
        );

        Ok(names)
    }

    /// Returns information about available package sources.
    pub fn scan_packages(&self) -> Result<PackageSpecs> {
        let mut specs = PackageSpecs::new();

        for dir in try_iterdir(&self.shared.user_package_dir)? {
            let origin = dir.to_string_lossy();
            self.add_packages(
                &mut specs,
                &self.shared.user_package_dir.join(&dir),
                &origin,
            )?;
        }

        self.add_packages(&mut specs, &self.shared.code_package_dir, "built-in")?;

        let auto = FullPackageName::from_str("auto").unwrap();
        let auto_deps = transitive_depends(&BTreeSet::from([auto]), &specs, BuildDepends(true))?;
        for FullPackageName(ns, name) in &auto_deps {
            let spec = match ns {
                PackageNamespace::Debian => continue,
                PackageNamespace::Root => specs.get_mut(name).ok_or_else(|| {
                    anyhow!("package \"auto\" depends on {name} but package not found")
                }),
                PackageNamespace::Managed(manager) => specs.get_mut(manager).ok_or_else(|| {
                    anyhow!(
                        "package \"auto\" depends on package manager {ns} but package not found"
                    )
                }),
            }?;
            spec.manifest
                .depends
                .get_mut(&PackageNamespace::Root)
                .unwrap()
                .remove("auto");
        }

        Ok(specs)
    }

    /// Rebuilds some of the given packages and their transitive dependencies,
    /// as requested.
    pub fn update_packages(
        &self,
        packages: &BTreeSet<FullPackageName>,
        specs: &PackageSpecs,
        conditions: UpdatePackagesConditions,
    ) -> Result<()> {
        let now = SystemTime::now();
        let mut todo: Vec<FullPackageName> =
            transitive_depends(packages, specs, BuildDepends(true))?
                .into_iter()
                .filter(|FullPackageName(ns, _name)| ns != &PackageNamespace::Debian)
                .collect();
        let mut done: BTreeSet<FullPackageName> = BTreeSet::new();
        loop {
            let start_todos = todo.len();
            if start_todos == 0 {
                return Ok(());
            }
            let mut later = Vec::new();

            for full_name in todo {
                let spec = match &full_name.0 {
                    PackageNamespace::Debian => unreachable!(),
                    PackageNamespace::Root => specs.get(&full_name.1).ok_or_else(|| {
                        anyhow!("could not find definition for package {}", full_name.1)
                    })?,
                    PackageNamespace::Managed(manager) => {
                        let spec = specs.get(manager).ok_or_else(|| {
                            anyhow!("could not find definition for package manager {manager}")
                        })?;
                        if !spec.manifest.package_manager {
                            return Err(anyhow!("package {manager} is not a package manager"));
                        }
                        spec
                    }
                };

                let deps_ready = spec
                    .manifest
                    .depends
                    .iter()
                    .chain(spec.manifest.build_depends.iter())
                    .all(|(ns, deps)| {
                        ns == &PackageNamespace::Debian
                            || deps
                                .keys()
                                .all(|dep| done.contains(&FullPackageName(ns.clone(), dep.clone())))
                    });

                if deps_ready {
                    let needs_build = {
                        if spec.update.is_none() {
                            false
                        } else {
                            let when = if packages.contains(&full_name) {
                                conditions.named
                            } else {
                                conditions.dependencies
                            };
                            match when {
                                ShouldPackageUpdate::Always => true,
                                ShouldPackageUpdate::IfStale => {
                                    self.package_is_stale(&full_name, spec, now)?
                                }
                                ShouldPackageUpdate::IfRequired => {
                                    self.last_built(&full_name).is_none()
                                }
                            }
                        }
                    };
                    if needs_build {
                        self.update_package(&full_name, spec, specs)?;
                    }
                    done.insert(full_name);
                } else {
                    later.push(full_name);
                }
            }
            if later.len() == start_todos {
                later.sort();
                return Err(anyhow!(
                    "package dependencies are unsatisfiable for: {}",
                    later
                        .iter()
                        .map(|full_name| full_name.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            todo = later;
        }
    }

    fn last_built(&self, name: &FullPackageName) -> Option<SystemTime> {
        let path = self
            .shared
            .package_cache
            .join(format!("{}.tar", name.as_filename_component()));
        let metadata = std::fs::metadata(path.as_host_raw()).ok()?;
        metadata.modified().ok()
    }

    fn package_is_stale(
        &self,
        package_name: &FullPackageName,
        spec: &PackageSpec,
        now: SystemTime,
    ) -> Result<bool> {
        let built = match self.last_built(package_name) {
            Some(built) => built,
            None => return Ok(true),
        };
        if let Some(threshold) = self.shared.config.auto_update {
            match now.duration_since(built) {
                Ok(d) if d > threshold => return Ok(true),
                Err(_) => return Ok(true),
                _ => {}
            }
        }
        let DirSummary { last_modified, .. } = summarize_dir(&spec.dir)?;
        if last_modified > built {
            return Ok(true);
        }
        for (ns, table) in spec
            .manifest
            .build_depends
            .iter()
            .chain(spec.manifest.depends.iter())
        {
            for name in table.keys() {
                let full_name = FullPackageName(ns.clone(), name.clone());
                if matches!(self.last_built(&full_name), Some(b) if b > built) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn package_build_failed(&self, package_name: &FullPackageName) -> Result<bool> {
        let failed_marker = &self
            .shared
            .package_cache
            .join(format!("{}.failed", package_name.as_filename_component()));
        try_exists(failed_marker)
            .with_context(|| format!("error while checking if {failed_marker:?} exists"))
    }

    fn update_package(
        &self,
        package_name: &FullPackageName,
        spec: &PackageSpec,
        specs: &PackageSpecs,
    ) -> Result<()> {
        let package_cache = &self.shared.package_cache;
        let failed_marker =
            package_cache.join(format!("{}.failed", package_name.as_filename_component()));

        match self
            .update_package_(package_name, spec, specs)
            .with_context(|| format!("failed to update package: {package_name}"))
        {
            Ok(_) => {
                if let Err(e) = std::fs::remove_file(failed_marker.as_host_raw()) {
                    if e.kind() != io::ErrorKind::NotFound {
                        return Err(e).context(format!(
                            "failed to remove file {failed_marker:?} after \
                            successfully updating package {package_name:?}"
                        ));
                    }
                }
                Ok(())
            }
            Err(update_error) => {
                std::fs::create_dir_all(package_cache.as_host_raw())
                    .with_context(|| format!("failed to create directory {package_cache:?}"))?;
                if let Err(e2) = std::fs::File::create(failed_marker.as_host_raw())
                    .with_context(|| format!("failed to create file {failed_marker:?}"))
                {
                    warn(e2);
                }
                let cached =
                    package_cache.join(format!("{}.tar", package_name.as_filename_component()));
                let use_stale = match try_exists(&cached)
                    .with_context(|| format!("error while checking if {cached:?} exists"))
                {
                    Ok(exists) => exists,
                    Err(e2) => {
                        warn(e2);
                        false
                    }
                };
                if use_stale {
                    warn(update_error.context(format!("using stale version of {package_name}")));
                    Ok(())
                } else {
                    Err(update_error)
                }
            }
        }
    }

    fn update_package_(
        &self,
        package_name: &FullPackageName,
        spec: &PackageSpec,
        specs: &PackageSpecs,
    ) -> LowLevelResult<()> {
        println!("Updating {package_name} package");
        let env_name = EnvironmentName::for_builder_package(package_name);
        self.build_package(package_name, &env_name, spec, specs)
            .with_context(|| format!("error building package {package_name}"))?;

        let package_cache = &self.shared.package_cache;
        std::fs::create_dir_all(&package_cache.as_host_raw())
            .with_context(|| format!("failed to create directory {package_cache:?}"))?;
        let package_cache_dir = cap_std::fs::Dir::open_ambient_dir(
            &package_cache.as_host_raw(),
            cap_std::ambient_authority(),
        )
        .with_context(|| format!("failed to open directory {package_cache:?}"))?;

        let testing_tar_name = format!("{}.testing.tar", package_name.as_filename_component());
        let testing_tar_abs = package_cache.join(&testing_tar_name);
        {
            let mut file = package_cache_dir
                .open_with(
                    &testing_tar_name,
                    cap_std::fs::OpenOptions::new().create(true).write(true),
                )
                .with_context(|| {
                    format!(
                        "failed to create file for package build output: {:?}",
                        testing_tar_abs,
                    )
                })?;
            self.runner
                .copy_out_from_home(&env_name, Path::new("provides.tar"), &mut file)
                .with_context(|| format!("failed to copy build output for package {package_name} to {testing_tar_abs}"))?;
        }

        if let Some(test_script) = &spec.test {
            self.test_package(package_name, testing_tar_abs, test_script, spec, specs)
                .with_context(|| format!("error testing package {package_name}"))?;
        }

        let package_tar = format!("{}.tar", package_name.as_filename_component());
        package_cache_dir
            .rename(&testing_tar_name, &package_cache_dir, &package_tar)
            .with_context(|| {
                format!(
                    "failed to rename {testing_tar_name:?} to {package_tar:?} in {package_cache:?}"
                )
            })?;
        Ok(())
    }

    fn build_package(
        &self,
        package_name: &FullPackageName,
        env_name: &EnvironmentName,
        spec: &PackageSpec,
        specs: &PackageSpecs,
    ) -> Result<()> {
        let packages: BTreeSet<FullPackageName> = spec
            .manifest
            .build_depends
            .iter()
            .chain(spec.manifest.depends.iter())
            .flat_map(|(ns, table)| {
                table
                    .keys()
                    .map(|name| FullPackageName(ns.clone(), name.clone()))
            })
            .collect();

        let mut debian_packages = self.resolve_debian_packages(&packages, specs)?;
        if let Some(debian) = spec.manifest.depends.get(&PackageNamespace::Debian) {
            debian_packages.extend(debian.keys().cloned());
        }
        if let Some(debian) = spec.manifest.build_depends.get(&PackageNamespace::Debian) {
            debian_packages.extend(debian.keys().cloned());
        }

        let mut seeds = self.packages_to_seeds(&packages)?;

        let tar_file = NamedTempFile::new().todo_context()?;
        create_tar_from_dir(
            &spec.dir,
            tar_file.as_file(),
            &TarOptions {
                prefix: Some(PathBuf::from("w")),
                ..TarOptions::default()
            },
        )
        .with_context(|| format!("failed to tar package source for {package_name}"))?;
        seeds.push(HostPath::try_from(tar_file.path().to_owned()).unwrap());

        let init = Init {
            debian_packages: debian_packages
                .iter()
                .map(|name| name.as_str().to_owned())
                .collect(),
            env_vars: if package_name.0 == PackageNamespace::Root {
                vec![]
            } else {
                vec![("PACKAGE", package_name.1.as_str().to_owned())]
            },
            seeds,
            script: self.shared.script_path.join("dev-init.sh"),
        };

        use EnvironmentExists::*;
        match self.runner.exists(env_name)? {
            FullyExists | PartiallyExists => self.runner.reset(env_name, &init),
            NoEnvironment => self.runner.create(env_name, &init),
        }
    }

    fn test_package(
        &self,
        package_name: &FullPackageName,
        testing_tar: HostPath,
        test_script: &str,
        spec: &PackageSpec,
        specs: &PackageSpecs,
    ) -> Result<()> {
        println!("Testing {package_name} package");
        let test_name = EnvironmentName::from_string(format!(
            "test-{}",
            EnvironmentName::for_builder_package(package_name).as_str()
        ))
        .unwrap();

        self.runner.purge(&test_name)?;

        let packages: BTreeSet<FullPackageName> = spec
            .manifest
            .depends
            .iter()
            .flat_map(|(ns, table)| {
                table
                    .keys()
                    .map(|name| FullPackageName(ns.clone(), name.clone()))
            })
            .collect();

        let mut seeds = self.packages_to_seeds(&packages)?;
        seeds.push(testing_tar);

        let mut debian_packages = self.resolve_debian_packages(&packages, specs)?;
        if let Some(debian) = spec.manifest.depends.get(&PackageNamespace::Debian) {
            debian_packages.extend(debian.keys().cloned());
        }

        {
            let tar_file = NamedTempFile::new().todo_context()?;
            create_tar_from_dir(
                &spec.dir,
                tar_file.as_file(),
                &TarOptions {
                    prefix: Some(PathBuf::from("w")),
                    // `dev-init.sh` will run `update.sh` if it's present, but
                    // we don't want that
                    exclude: vec![PathBuf::from("update.sh")],
                },
            )
            .with_context(|| format!("failed to tar package source to test {package_name}"))?;
            seeds.push(HostPath::try_from(tar_file.path().to_owned()).unwrap());

            self.runner.create(
                &test_name,
                &Init {
                    debian_packages: debian_packages
                        .iter()
                        .map(|name| name.as_str().to_owned())
                        .collect(),
                    env_vars: if package_name.0 == PackageNamespace::Root {
                        vec![]
                    } else {
                        vec![("PACKAGE", package_name.1.as_str().to_owned())]
                    },
                    seeds,
                    script: self.shared.script_path.join("dev-init.sh"),
                },
            )?;
        }

        self.runner
            .run(&test_name, &RunnerCommand::Exec(&[test_script.to_owned()]))?;
        self.runner.purge(&test_name)
    }

    /// Returns details of available packages.
    pub fn get_packages(&self) -> Result<BTreeMap<FullPackageName, PackageDetails>> {
        let metadata = |name: &FullPackageName| -> (Option<SystemTime>, Option<u64>) {
            match std::fs::metadata(
                &self
                    .shared
                    .package_cache
                    .join(format!("{}.tar", name.as_filename_component()))
                    .as_host_raw(),
            ) {
                Ok(metadata) => (metadata.modified().ok(), file_size(&metadata)),
                Err(_) => (None, None),
            }
        };

        let root_packages = self.scan_packages()?.into_iter().map(
            |(name, spec)| -> Result<(FullPackageName, PackageDetails)> {
                let full_name = FullPackageName(PackageNamespace::Root, name);
                let (built, size) = metadata(&full_name);
                let edited = summarize_dir(&spec.dir).ok().map(|s| s.last_modified);
                let last_build_failed = self.package_build_failed(&full_name)?;
                Ok((
                    full_name,
                    PackageDetails {
                        build_depends: spec
                            .manifest
                            .build_depends
                            .into_iter()
                            .map(|(namespace, packages)| {
                                (
                                    namespace.as_str().to_owned(),
                                    packages
                                        .into_keys()
                                        .map(|name| name.as_str().to_owned())
                                        .collect(),
                                )
                            })
                            .collect(),
                        built,
                        depends: spec
                            .manifest
                            .depends
                            .into_iter()
                            .map(|(namespace, packages)| {
                                (
                                    namespace.as_str().to_owned(),
                                    packages
                                        .into_keys()
                                        .map(|name| name.as_str().to_owned())
                                        .collect(),
                                )
                            })
                            .collect(),
                        dir: Some(spec.dir.as_host_raw().to_owned()),
                        edited,
                        last_build_failed,
                        package_manager: spec.manifest.package_manager,
                        origin: spec.origin,
                        size,
                    },
                ))
            },
        );

        let non_root_packages = try_iterdir(&self.shared.package_cache)?
            .into_iter()
            .filter_map(|filename| {
                filename
                    .to_str()
                    .and_then(|filename| filename.strip_suffix(".tar"))
                    .and_then(|prefix| FullPackageName::from_str(prefix).ok())
            })
            .filter(|FullPackageName(ns, _name)| ns != &PackageNamespace::Root)
            .map(|name| {
                let (built, size) = metadata(&name);
                let last_build_failed = self.package_build_failed(&name)?;
                Ok((
                    name,
                    PackageDetails {
                        build_depends: BTreeMap::new(),
                        built,
                        depends: BTreeMap::new(),
                        edited: None,
                        dir: None,
                        last_build_failed,
                        package_manager: false,
                        origin: String::from("N/A"),
                        size,
                    },
                ))
            });

        root_packages.chain(non_root_packages).collect()
    }

    /// Corresponds to `cub package list`.
    pub fn list_packages(&self, format: ListPackagesFormat) -> Result<()> {
        use ListPackagesFormat::*;
        match format {
            Names => {
                for name in self.get_package_names()? {
                    println!("{}", name.unquoted());
                }
            }

            Json => {
                let packages = self.get_packages()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&packages)
                        .context("failed to serialize JSON while listing packages")?
                );
            }

            Default => {
                let packages = self.get_packages()?;
                let names: Vec<String> = packages
                    .iter()
                    .map(|(full_name, details)| {
                        if details.package_manager {
                            format!("{}.*", full_name.unquoted())
                        } else {
                            full_name.unquoted()
                        }
                    })
                    .collect();
                let nw = names.iter().map(|s| s.len()).max().unwrap_or(10);
                let ow = packages.values().map(|p| p.origin.len()).max().unwrap_or(8);
                let now = SystemTime::now();
                println!(
                    "{:<nw$}  {:<ow$}  {:>10}  {:>13}  {:>13}  {:>8}",
                    "name", "origin", "size", "built", "edited", "status"
                );
                println!(
                    "{0:-<nw$}  {0:-<ow$}  {0:-<10}  {0:-<13}  {0:-<13}  {0:-<8}",
                    ""
                );
                for (name, package) in names.iter().zip(packages.values()) {
                    println!(
                        "{:<nw$}  {:<ow$}  {:>10}  {:>13}  {:>13}  {:>8}",
                        name,
                        package.origin,
                        match package.size {
                            Some(size) => Bytes(size).to_string(),
                            None => String::from("N/A"),
                        },
                        match package.built {
                            Some(built) => rel_time(now.duration_since(built).ok()),
                            None => String::from("N/A"),
                        },
                        match package.edited {
                            Some(edited) => rel_time(now.duration_since(edited).ok()),
                            None => String::from("N/A"),
                        },
                        if package.last_build_failed {
                            "failed"
                        } else {
                            "ok"
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn read_package_list_from_env(
        &self,
        name: &EnvironmentName,
    ) -> Result<BTreeSet<FullPackageName>> {
        let mut buf = Vec::new();
        self.runner
            .copy_out_from_work(name, Path::new("packages.txt"), &mut buf)?;
        let reader = io::BufReader::new(buf.as_slice());
        let names = reader
            .lines()
            .map(|line| match line {
                Ok(line) => FullPackageName::from_str(&line),
                Err(e) => Err(e).todo_context(),
            })
            .collect::<Result<BTreeSet<FullPackageName>>>()
            .todo_context()?;
        Ok(names)
    }

    pub(super) fn packages_to_seeds(
        &self,
        packages: &BTreeSet<FullPackageName>,
    ) -> Result<Vec<HostPath>> {
        let mut seeds = Vec::with_capacity(packages.len());
        let specs = self.scan_packages()?;
        let deps = transitive_depends(packages, &specs, BuildDepends(false))?;
        for name in deps {
            let provides = self
                .shared
                .package_cache
                .join(format!("{}.tar", name.as_filename_component()));
            if try_exists(&provides).todo_context()? {
                seeds.push(provides);
            }
        }
        Ok(seeds)
    }
}

/// The name of a potential Cubicle package.
///
/// Other than '-', '_', '.' and some non-ASCII characters, values of this type
/// may not contain whitespace or special characters.
#[derive(Clone, Debug, Eq, Ord, PartialOrd, PartialEq, Serialize)]
pub struct PackageName(String);

impl PackageName {
    /// Returns a string slice representing the package name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for PackageName {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for PackageName {
    type Err = Error;
    fn from_str(mut s: &str) -> Result<Self> {
        s = s.trim();
        if s.is_empty() {
            return Err(anyhow!("package name cannot be empty"));
        }
        if s.contains(|c: char| {
            (c.is_ascii() && !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.'))
                || c.is_control()
                || c.is_whitespace()
        }) {
            return Err(anyhow!("package name cannot contain special characters"));
        }
        Ok(Self(s.to_owned()))
    }
}

impl Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

/// A namespace for packages. See [`FullPackageName`].
#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum PackageNamespace {
    /// Top-level, normal Cubicle packages live here.
    Root,
    /// OS-level packages provided by Debian.
    Debian,
    /// A special Cubicle package that acts as a package manager to install
    /// other packages.
    Managed(PackageName),
}

impl PackageNamespace {
    /// Returns the namespace as a string.
    ///
    /// Note that this may be surprising to users for the `Root` namespace.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Root => "root",
            Self::Debian => "debian",
            Self::Managed(package) => package.as_str(),
        }
    }
}

impl FromStr for PackageNamespace {
    type Err = Error;
    fn from_str(mut s: &str) -> Result<Self> {
        s = s.trim();
        if s.is_empty() {
            return Err(anyhow!("package namespace cannot be empty"));
        }
        if s.contains(|c: char| {
            (c.is_ascii() && !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_'))
                || c.is_control()
                || c.is_whitespace()
        }) {
            return Err(anyhow!(
                "package namespace cannot contain special characters"
            ));
        }
        Ok(match s {
            "cubicle" => Self::Root,
            "debian" => Self::Debian,
            _ => Self::Managed(PackageName::from_str(s)?),
        })
    }
}

impl Display for PackageNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.as_str(), f)
    }
}

/// A fully-qualified package name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FullPackageName(pub PackageNamespace, pub PackageName);

impl PartialOrd for FullPackageName {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The ordering follows that of `(package)` for the root namespace and
/// `(namespace, package)` for other namespaces.
impl Ord for FullPackageName {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // TODO: avoid allocations
        self.unquoted().cmp(&other.unquoted())
    }
}

impl Serialize for FullPackageName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.unquoted().serialize(serializer)
    }
}

impl FullPackageName {
    /// Returns a string representation of the name that a human can probably
    /// decipher when standing alone.
    pub fn unquoted(&self) -> String {
        if self.0 == PackageNamespace::Root {
            self.1.as_str().to_owned()
        } else {
            format!("{}.{}", self.0.as_str(), self.1.as_str())
        }
    }

    fn as_filename_component(&self) -> String {
        self.unquoted()
    }
}

impl Display for FullPackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == PackageNamespace::Root {
            write!(f, "{:?}", self.1.as_str())
        } else {
            write!(f, "\"{}.{}\"", self.0.as_str(), self.1.as_str())
        }
    }
}

impl FromStr for FullPackageName {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s.trim().split_once('.') {
            Some((ns, name)) => Self(
                PackageNamespace::from_str(ns)?,
                PackageName::from_str(name)?,
            ),
            None => Self(PackageNamespace::Root, PackageName::from_str(s)?),
        })
    }
}

/// Allowed formats for [`Cubicle::list_packages`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ListPackagesFormat {
    /// Human-formatted table.
    #[default]
    Default,
    /// Detailed JSON output for machine consumption.
    Json,
    /// Newline-delimited list of package names only.
    Names,
}

pub fn write_package_list_tar(
    packages: &BTreeSet<FullPackageName>,
) -> Result<tempfile::NamedTempFile> {
    let file = tempfile::NamedTempFile::new().todo_context()?;
    let metadata = file.as_file().metadata().todo_context()?;
    let mut builder = tar::Builder::new(file.as_file());
    let mut header = tar::Header::new_gnu();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        header.set_mtime(metadata.mtime() as u64);
        header.set_uid(metadata.uid() as u64);
        header.set_gid(metadata.gid() as u64);
        header.set_mode(metadata.mode());
    }

    let mut buf = Vec::new();
    for name in packages.iter() {
        writeln!(buf, "{}", name.unquoted()).todo_context()?;
    }
    header.set_size(buf.len() as u64);
    builder
        .append_data(
            &mut header,
            Path::new("w").join("packages.txt"),
            buf.as_slice(),
        )
        .todo_context()?;
    builder
        .into_inner()
        .and_then(|mut f| f.flush())
        .todo_context()?;
    Ok(file)
}

fn strict_debian_packages(
    packages: &BTreeSet<FullPackageName>,
    specs: &PackageSpecs,
) -> Result<BTreeSet<PackageName>> {
    Ok(transitive_depends(packages, specs, BuildDepends(false))?
        .into_iter()
        .filter_map(|FullPackageName(ns, name)| (ns == PackageNamespace::Debian).then_some(name))
        .collect())
}

fn all_debian_packages(specs: &PackageSpecs) -> Result<BTreeSet<PackageName>> {
    let mut debian_packages = BTreeSet::new();
    for spec in specs.values() {
        if let Some(debian) = spec.manifest.depends.get(&PackageNamespace::Debian) {
            debian_packages.extend(debian.keys().cloned());
        }
        if let Some(debian) = spec.manifest.build_depends.get(&PackageNamespace::Debian) {
            debian_packages.extend(debian.keys().cloned());
        }
    }
    Ok(debian_packages)
}

/// Description of a package as returned by [`Cubicle::get_packages`].
#[derive(Debug, Serialize)]
#[non_exhaustive]
pub struct PackageDetails {
    /// Map from package namespaces to package names for packages this package
    /// needs at build-time.
    pub build_depends: BTreeMap<String, Vec<String>>,
    #[serde(serialize_with = "time_serialize_opt")]
    /// The last time the package was successfully built, if available.
    pub built: Option<SystemTime>,
    /// Map from package namespaces to package names for packages this package
    /// needs at build-time and run-time.
    pub depends: BTreeMap<String, Vec<String>>,
    #[serde(serialize_with = "time_serialize_opt")]
    /// The last time the package sources were changed (or `UNIX_EPOCH` if
    /// unavailable).
    pub edited: Option<SystemTime>,
    /// The path on the host to the package sources.
    pub dir: Option<PathBuf>,
    /// If true, the last completed build attempt failed. If false, either the
    /// last completed build succeeded or no build has yet completed to success
    /// or failure.
    pub last_build_failed: bool,
    /// If false, this is a normal package. If true, it is a meta-package that
    /// knows how to build many packages.
    pub package_manager: bool,
    /// Where the package sources came from. For package sources shipped with
    /// Cubicle, this is `"built-in"`. For local packages, it is the name of
    /// the parent directory above the package source.
    pub origin: String,
    /// The size of the last successful package build output, if available.
    pub size: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_package_name_ord() {
        let mut names =
            ["d", "c.x", "c", "b", "b.a"].map(|s| FullPackageName::from_str(s).unwrap());
        names.sort();

        assert_eq!("b b.a c c.x d", names.map(|name| name.unquoted()).join(" "));
    }
}
