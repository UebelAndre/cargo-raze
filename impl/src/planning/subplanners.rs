// Copyright 2020 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
  collections::{BTreeSet, HashMap, HashSet},
  io, iter,
  path::{Path, PathBuf},
  str::FromStr,
};

use anyhow::Result;

use cargo_lock::{lockfile::Lockfile, SourceId, Version};
use cargo_platform::Platform;

use itertools::Itertools;

use crate::{
  context::{
    BuildableDependency, BuildableTarget, CrateContext, CrateDependencyContext,
    CrateTargetedDepContext, DependencyAlias, GitRepo, LicenseData, SourceDetails,
    WorkspaceContext,
  },
  error::{RazeError, PLEASE_FILE_A_BUG},
  metadata::{
    fetch_crate_checksum, CargoWorkspaceFiles, DepKindInfo, DependencyKind, Node, Package,
  },
  planning::license,
  settings::{format_registry_url, CrateSettings, GenMode, RazeSettings},
  util::{
    self, get_matching_bazel_triples, is_bazel_supported_platform, sanitize_ident, PlatformDetails,
  },
};

use super::{
  checks,
  crate_catalog::{CrateCatalog, CrateCatalogEntry},
  PlannedBuild,
};

/// Named type to reduce declaration noise for deducing the crate contexts
type CrateContextProduction = (Vec<CrateContext>, Vec<DependencyAlias>);

/** An internal working planner for generating context for an individual crate. */
struct CrateSubplanner<'planner> {
  // Workspace-Wide details
  settings: &'planner RazeSettings,
  platform_details: &'planner Option<PlatformDetails>,
  crate_catalog: &'planner CrateCatalog,
  // Crate specific content
  crate_catalog_entry: &'planner CrateCatalogEntry,
  source_id: &'planner Option<SourceId>,
  node: &'planner Node,
  crate_settings: Option<&'planner CrateSettings>,
  sha256: &'planner Option<String>,
}

/** An internal working planner for generating context for a whole workspace. */
pub struct WorkspaceSubplanner<'planner> {
  pub(super) settings: &'planner RazeSettings,
  pub(super) platform_details: &'planner Option<PlatformDetails>,
  pub(super) crate_catalog: &'planner CrateCatalog,
  pub(super) files: &'planner CargoWorkspaceFiles,
  pub(super) binary_dependencies: &'planner Vec<CrateCatalog>,
  pub(super) binary_deps_files: &'planner HashMap<String, CargoWorkspaceFiles>,
}

impl<'planner> WorkspaceSubplanner<'planner> {
  /** Produces a planned build using internal state. */
  pub fn produce_planned_build(&self) -> Result<PlannedBuild> {
    checks::check_resolve_matches_packages(&self.crate_catalog.metadata)?;

    let mut packages: Vec<&Package> = self.crate_catalog.metadata.packages.iter().collect();

    match self.settings.genmode {
      GenMode::Remote => {
        for bin_dep in self.binary_dependencies {
          checks::check_resolve_matches_packages(&bin_dep.metadata)?;
          packages.extend(bin_dep.metadata.packages.iter());
        }
      },
      GenMode::Vendored => {
        checks::check_all_vendored(self.crate_catalog.entries(), &self.settings.workspace_path)?;
      },
      _ => { /* No checks to perform */ },
    }

    checks::warn_unused_settings(&self.settings.crates, &packages);

    let (crate_contexts, workspace_aliases) = self.produce_crate_contexts()?;

    Ok(PlannedBuild {
      workspace_context: self.produce_workspace_context(),
      crate_contexts,
      workspace_aliases,
      binary_crate_files: self.binary_deps_files.clone(),
    })
  }

  /** Constructs a workspace context from settings. */
  fn produce_workspace_context(&self) -> WorkspaceContext {
    WorkspaceContext {
      workspace_path: self.settings.workspace_path.clone(),
      gen_workspace_prefix: self.settings.gen_workspace_prefix.clone(),
      output_buildfile_suffix: self.settings.output_buildfile_suffix.clone(),
    }
  }

  fn create_crate_context(
    &self,
    node: &Node,
    package_to_checksum: &HashMap<(String, Version), String>,
    catalog: &CrateCatalog,
    root_ctx: &mut Option<CrateContext>,
  ) -> Result<Option<CrateContext>> {
    let own_crate_catalog_entry = match catalog.entry_for_package_id(&node.id) {
      Some(x) => x,
      None => return Ok(None),
    };

    let own_package = own_crate_catalog_entry.package();
    let is_workspace = own_crate_catalog_entry.is_workspace_crate();
    let is_root = own_crate_catalog_entry.is_root();

    let checksum_opt =
      package_to_checksum.get(&(own_package.name.clone(), own_package.version.clone()));

    let is_binary_dep = self
      .settings
      .binary_deps
      .keys()
      .any(|key| key == &own_package.name);

    // Skip workspace crates, since we haven't yet decided how they should be handled.
    //
    // Except in the case of binary dependencies. They can handle these no problem
    //
    // Hey you, reader! If you have opinions about this please comment on the below bug, or file
    // another bug.
    // See Also: https://github.com/google/cargo-raze/issues/111
    if !(is_binary_dep || is_root) && is_workspace {
      return Ok(None);
    }

    // UNWRAP: Safe given unwrap during serialize step of metadata
    let own_source_id = own_package
      .source
      .as_ref()
      .map(|s| SourceId::from_url(&s.to_string()).unwrap());

    let crate_settings = self.crate_settings(&own_package)?;

    let crate_subplanner = CrateSubplanner {
      crate_catalog: &catalog,
      settings: self.settings,
      platform_details: self.platform_details,
      crate_catalog_entry: &own_crate_catalog_entry,
      source_id: &own_source_id,
      node: &node,
      crate_settings,
      sha256: &checksum_opt.map(|c| c.to_owned()),
    };

    let context: CrateContext = crate_subplanner.produce_context()?;

    if is_root && !is_binary_dep {
      if let Some(orig) = root_ctx.replace(context) {
        return Err(
          RazeError::Generic(format!(
            "Multiple root contexts found {:#?} {:#?}. {}",
            orig, root_ctx, PLEASE_FILE_A_BUG
          ))
          .into(),
        );
      } else {
        return Ok(None);
      }
    }

    Ok(Some(context))
  }

  fn crate_settings(&self, package: &Package) -> Result<Option<&CrateSettings>> {
    self
      .settings
      .crates
      .get(&package.name)
      .map_or(Ok(None), |settings| {
        let mut versions = settings
          .iter()
          .filter(|(ver_req, _)| ver_req.matches(&package.version))
          .peekable();

        match versions.next() {
          // This is possible if the crate does not have any version overrides to match against
          None => Ok(None),
          Some((_, settings)) if versions.peek().is_none() => Ok(Some(settings)),
          Some(current) => Err(RazeError::Config {
            field_path_opt: None,
            message: format!(
              "Multiple potential semver matches `[{}]` found for `{}`",
              iter::once(current).chain(versions).map(|x| x.0).join(", "),
              &package.name
            ),
          }),
        }
      })
      .map_err(|e| e.into())
  }

  /** Produces a crate context for each declared crate and dependency. */
  fn produce_crate_contexts(&self) -> Result<CrateContextProduction> {
    // Build a list of all catalogs we want the context for
    let mut catalogs = vec![self.crate_catalog];
    for catalog in self.binary_dependencies.iter() {
      catalogs.push(catalog);
    }

    // Build a list of all workspace files
    let mut files: Vec<&CargoWorkspaceFiles> = self
      .binary_deps_files
      .iter()
      .map(|(_key, val)| val)
      .collect();
    files.push(self.files);

    // Gather the checksums for all packages in the lockfile
    // which have them.
    //
    // Store the representation of the package as a tuple
    // of (name, version) -> checksum.
    let mut package_to_checksum = HashMap::new();
    for workspace_files in files.iter() {
      if let Some(lock_path) = workspace_files.lock_path_opt.as_ref() {
        let lockfile = Lockfile::load(lock_path.as_path())?;
        for package in lockfile.packages {
          if let Some(checksum) = package.checksum {
            // HACK: Compat hack as cargo uses semver 0.10 and cargo-lock uses 0.9
            let ver = Version::parse(&package.version.to_string())?;
            package_to_checksum.insert((package.name.to_string(), ver), checksum.to_string());
          }
        }
      }
    }

    // Additionally, the binary dependencies need to have their checksums added as well in 
    // Remote GenMode configurations. Vendored GenMode relies on the behavior of `cargo vendor`
    // and doesn't perform any special logic to fetch binary dependency crates.
    if self.settings.genmode == GenMode::Remote {
      for (pkg, info) in self.settings.binary_deps.iter() {
        let version = semver::Version::parse(info.req())?;
        package_to_checksum.insert(
          (pkg.clone(), version.clone()),
          fetch_crate_checksum(&self.settings.index_url, pkg, &version.to_string())?,
        );
      }
    }

    let mut root_ctx = None;

    let contexts = catalogs
      .iter()
      .map(|catalog| {
        (*catalog)
          .metadata
          .resolve
          .as_ref()
          .ok_or_else(|| RazeError::Generic("Missing resolve graph".into()))?
          .nodes
          .iter()
          .sorted_by_key(|n| &n.id)
          .filter_map(|node| {
            self
              .create_crate_context(node, &package_to_checksum, catalog, &mut root_ctx)
              .transpose()
          })
          .collect::<Result<Vec<_>>>()
      })
      .fold_results(Vec::new(), |mut contexts, catalog_context| {
        contexts.extend(catalog_context);
        contexts
      })?;

    let root_ctx = root_ctx.ok_or_else(|| {
      RazeError::Internal(format!("No root context found. {}", PLEASE_FILE_A_BUG))
    })?;

    let aliases =
      self.produce_workspace_aliases(root_ctx.default_deps.aliased_dependencies, &contexts);
    Ok((contexts, aliases))
  }

  fn produce_workspace_aliases(
    &self,
    root_dependency_aliases: BTreeSet<DependencyAlias>,
    all_packages: &[CrateContext],
  ) -> Vec<DependencyAlias> {
    let renames = root_dependency_aliases
      .iter()
      .map(|rename| (&rename.target, &rename.alias))
      .collect::<HashMap<_, _>>();

    all_packages
      .iter()
      .filter(|to_alias| to_alias.is_root_dependency && to_alias.lib_target_name.is_some())
      .flat_map(|to_alias| {
        let pkg_name = sanitize_ident(&to_alias.pkg_name);
        let target = format!("{}:{}", &to_alias.workspace_path_to_crate, &pkg_name);
        let alias = renames
          .get(&target)
          .map(|x| x.to_string())
          .unwrap_or(pkg_name);
        let dep_alias = DependencyAlias { alias, target };

        to_alias
          .raze_settings
          .extra_aliased_targets
          .iter()
          .map(move |extra_alias| DependencyAlias {
            alias: extra_alias.clone(),
            target: format!("{}:{}", &to_alias.workspace_path_to_crate, extra_alias),
          })
          .chain(std::iter::once(dep_alias))
      })
      .sorted()
      .collect_vec()
  }
}

impl<'planner> CrateSubplanner<'planner> {
  /** Builds a crate context from internal state. */
  fn produce_context(&self) -> Result<CrateContext> {
    let package = self.crate_catalog_entry.package();

    let manifest_path = PathBuf::from(&package.manifest_path);
    assert!(manifest_path.is_absolute());
    let package_root = self.find_package_root_for_manifest(&manifest_path)?;

    let mut targets = self.produce_targets(&package_root)?;
    let build_script_target_opt = self.take_build_script_target(&mut targets);

    let lib_target_name = targets
      .iter()
      .find(|target| target.kind == "lib" || target.kind == "proc-macro")
      .map(|target| target.name.clone());

    let mut deps = self.produce_deps()?;
    // Take the default deps that are not bound to platform targets
    let default_deps = deps.remove(&None).unwrap_or_default();
    // Build a list of dependencies while addression a potential whitelist of target triples
    let mut targeted_deps = deps
      .into_iter()
      .map(|(target, deps)| {
        let target = target.unwrap().to_string();
        let platform_targets = get_matching_bazel_triples(&target, &self.settings.targets)?
          .map(|x| x.to_string())
          .collect();
        Ok(CrateTargetedDepContext {
          deps,
          target,
          platform_targets,
        })
      })
      .filter(|res| match res {
        Ok(ctx) => !ctx.platform_targets.is_empty(),
        Err(_) => true,
      })
      .collect::<Result<Vec<_>>>()?;
    targeted_deps.sort();

    let context = CrateContext {
      pkg_name: package.name.clone(),
      pkg_version: package.version.clone(),
      edition: package.edition.clone(),
      license: self.produce_license(),
      features: self.node.features.clone(),
      is_root_dependency: self.crate_catalog_entry.is_root_dep(),
      default_deps,
      targeted_deps,
      workspace_path_to_crate: self.crate_catalog_entry.workspace_path(&self.settings)?,
      build_script_target: build_script_target_opt,
      raze_settings: self.crate_settings.cloned().unwrap_or_default(),
      source_details: self.produce_source_details(&package, &package_root),
      expected_build_path: self.crate_catalog_entry.local_build_path(&self.settings)?,
      sha256: self.sha256.clone(),
      registry_url: format_registry_url(
        &self.settings.registry,
        &package.name,
        &package.version.to_string(),
      ),
      lib_target_name,
      targets,
    };

    Ok(context)
  }

  /** Generates license data from internal crate details. */
  fn produce_license(&self) -> LicenseData {
    let licenses_str = self
      .crate_catalog_entry
      .package()
      .license
      .as_ref()
      .map_or("", String::as_str);

    license::get_license_from_str(licenses_str)
  }

  /** Generates the set of dependencies for the contained crate. */
  fn produce_deps(&self) -> Result<HashMap<Option<&'planner str>, CrateDependencyContext>> {
    let mut dep_production: HashMap<Option<&'planner str>, CrateDependencyContext> = HashMap::new();

    let all_skipped_deps = self
      .crate_settings
      .iter()
      .flat_map(|pkg| pkg.skipped_deps.iter())
      .collect::<HashSet<_>>();

    for dep in &self.node.deps {
      // UNWRAP(s): Safe from verification of packages_by_id
      let dep_package = self
        .crate_catalog
        .entry_for_package_id(&dep.pkg)
        .unwrap()
        .package();

      // Skip settings-indicated deps to skip
      if all_skipped_deps.contains(&format!("{}-{}", dep_package.name, dep_package.version)) {
        continue;
      }

      // TODO(GregBowyer): Reimplement what cargo does to detect bad renames
      for dep_kind in &dep.dep_kinds {
        let platform_target = dep_kind.target.as_ref().map(|x| x.repr.as_ref());
        // Skip deps that fall out of targetting
        if !self.is_dep_targetted(platform_target) {
          continue;
        }

        let mut dep_set = dep_production.entry(platform_target).or_default();
        self.process_dep(&mut dep_set, &dep.name, dep_kind, &dep_package)?;
      }
    }

    for set in dep_production.values_mut() {
      set.build_proc_macro_dependencies.sort();
      set.build_dependencies.sort();
      set.dev_dependencies.sort();
      set.proc_macro_dependencies.sort();
      set.dependencies.sort();
    }

    Ok(dep_production)
  }

  fn process_dep(
    &self,
    dep_set: &mut CrateDependencyContext,
    name: &str,
    dep: &DepKindInfo,
    pkg: &Package,
  ) -> Result<()> {
    let is_proc_macro = self.is_proc_macro(pkg);
    let is_sys_crate = pkg.name.ends_with("-sys");

    let build_dep = BuildableDependency {
      name: pkg.name.clone(),
      version: pkg.version.clone(),
      buildable_target: self.buildable_target_for_dep(pkg)?,
      is_proc_macro,
    };

    use DependencyKind::*;
    match dep.kind {
      Build if is_proc_macro => dep_set.build_proc_macro_dependencies.push(build_dep),
      Build => dep_set.build_dependencies.push(build_dep),
      Development => dep_set.dev_dependencies.push(build_dep),
      // TODO(GregBowyer): Is a proc-macro -sys crate a thing?
      Normal if is_proc_macro => dep_set.proc_macro_dependencies.push(build_dep),
      Normal => {
        // sys crates may generate DEP_* env vars that must be visible to direct dep builds
        if is_sys_crate {
          dep_set.build_dependencies.push(build_dep.clone());
        }
        dep_set.dependencies.push(build_dep)
      }
      kind => {
        return Err(
          RazeError::Planning {
            dependency_name_opt: Some(pkg.name.to_string()),
            message: format!(
              "Unhandlable dependency type {:?} on {} detected! {}",
              kind, &pkg.name, PLEASE_FILE_A_BUG
            ),
          }
          .into(),
        )
      }
    };

    if self.is_renamed(pkg) {
      let dep_alias = DependencyAlias {
        target: self.buildable_target_for_dep(pkg)?,
        alias: util::sanitize_ident(&name),
      };

      if !dep_set.aliased_dependencies.insert(dep_alias) {
        return Err(
          RazeError::Planning {
            dependency_name_opt: Some(pkg.name.to_string()),
            message: format!("Duplicated renamed package {}", name),
          }
          .into(),
        );
      }
    }

    Ok(())
  }

  fn buildable_target_for_dep(&self, dep_package: &Package) -> Result<String> {
    // UNWRAP: Guaranteed to exist by checks in WorkspaceSubplanner#produce_build_plan
    self
      .crate_catalog
      .entry_for_package_id(&dep_package.id)
      .unwrap()
      .workspace_path_and_default_target(&self.settings)
  }

  /// Test if the given dep details pertain to it being a proc-macro
  ///
  /// Implicitly dependencies are on the [lib] target from Cargo.toml (of which there is guaranteed
  /// to be at most one).
  ///
  /// We don't explicitly narrow to be considering only the [lib] Target - we rely on the fact that
  /// only one [lib] is allowed in a Package, and so treat the Package synonymously with the [lib]
  /// Target therein. Only the [lib] target is allowed to be labelled as a proc-macro, so checking
  /// if "any" target is a proc-macro is equivalent to checking if the [lib] target is a proc-macro
  /// (and accordingly, whether we need to treat this dep like a proc-macro).
  fn is_proc_macro(&self, dep_package: &Package) -> bool {
    dep_package
      .targets
      .iter()
      .flat_map(|target| target.crate_types.iter())
      .any(|crate_type| crate_type.as_str() == "proc-macro")
  }

  /// Test to see if a dep has been renamed
  ///
  /// Currently cargo-metadata provides rename detection in a few places, we take the names
  /// from the resolution of a package
  fn is_renamed(&self, dep_package: &Package) -> bool {
    self
      .crate_catalog_entry
      .package()
      .dependencies
      .iter()
      .filter(|x| x.name == dep_package.name)
      .filter(|x| x.req.matches(&dep_package.version))
      .filter(|x| x.source == dep_package.source.as_ref().map(|src| src.to_string()))
      .find(|x| x.rename.is_some())
      .map_or(false, |x| x.rename.is_some())
  }

  fn is_dep_targetted(&self, target: Option<&str>) -> bool {
    target
      .map(|platform| {
        match self
          .platform_details
          .as_ref()
          .zip(self.settings.target.as_ref())
        {
          // Legacy behavior
          Some((platform_details, settings_target)) => {
            // Skip this dep if it doesn't match our platform attributes
            // UNWRAP: It is reasonable to assume cargo is not giving us odd platform strings
            let platform = Platform::from_str(platform).unwrap();
            platform.matches(settings_target, platform_details.attrs())
          }
          None => match is_bazel_supported_platform(platform) {
            // TODO (GregBowyer): Make this an enum
            (_, true) => true,
            (true, _) => true,
            _ => false,
          },
        }
      })
      .unwrap_or(true)
  }

  /** Generates source details for internal crate. */
  fn produce_source_details(&self, package: &Package, package_root: &Path) -> SourceDetails {
    SourceDetails {
      git_data: self.source_id.as_ref().filter(|id| id.is_git()).map(|id| {
        let manifest_parent = package.manifest_path.parent().unwrap();
        let path_to_crate_root = manifest_parent.strip_prefix(package_root).unwrap();
        let path_to_crate_root = if path_to_crate_root.components().next().is_some() {
          Some(path_to_crate_root.to_string_lossy().to_string())
        } else {
          None
        };
        GitRepo {
          remote: id.url().to_string(),
          commit: id.precise().unwrap().to_owned(),
          path_to_crate_root,
        }
      }),
    }
  }

  /**
   * Extracts the (one and only) build script target from the provided set of build targets.
   *
   * This function mutates the provided list of build arguments. It removes the first (and usually,
   * only) found build script target.
   */
  fn take_build_script_target(
    &self,
    all_targets: &mut Vec<BuildableTarget>,
  ) -> Option<BuildableTarget> {
    if !self
      .crate_settings
      .and_then(|x| x.gen_buildrs)
      .unwrap_or(self.settings.default_gen_buildrs)
    {
      return None;
    }

    all_targets
      .iter()
      .position(|t| t.kind == "custom-build")
      .map(|idx| all_targets.remove(idx))
  }

  /**
   * Produces the complete set of build targets specified by this crate.
   *
   * This function may access the file system. See #find_package_root_for_manifest for more
   * details.
   */
  fn produce_targets(&self, package_root_path: &Path) -> Result<Vec<BuildableTarget>> {
    let mut targets = Vec::new();
    let package = self.crate_catalog_entry.package();
    for target in &package.targets {
      // Bazel uses / as a path delimiter, but / is not the path delimiter on all
      // operating systems (like Mac OS 9, or something people actually use like Windows).
      // Strip off the package root, decompose the path into parts and rejoin
      // them with '/'.
      let package_root_path_str = target
        .src_path
        .strip_prefix(package_root_path)
        .unwrap_or(&target.src_path)
        .components()
        .map(|c| c.as_os_str().to_str())
        .try_fold("".to_owned(), |res, v| Some(format!("{}/{}", res, v?)))
        .ok_or(io::Error::new(
          io::ErrorKind::InvalidData,
          format!(
            "{:?} contains non UTF-8 characters and is not a legal path in Bazel",
            &target.src_path
          ),
        ))?
        .trim_start_matches("/")
        .to_owned();

      for kind in &target.kind {
        targets.push(BuildableTarget {
          name: target.name.clone(),
          path: package_root_path_str.clone(),
          kind: kind.clone(),
          edition: target.edition.clone(),
        });
      }
    }

    targets.sort();
    Ok(targets)
  }

  /**
   * Finds the root of a contained git package.
   *
   * This function needs to access the file system if the dependency is a git dependency in order
   * to find the true filesystem root of the dependency. The root cause is that git dependencies
   * often aren't solely the crate of interest, but rather a repository that contains the crate of
   * interest among others.
   */
  fn find_package_root_for_manifest(&self, manifest_path: &PathBuf) -> Result<PathBuf> {
    let has_git_repo_root = {
      let is_git = self.source_id.as_ref().map_or(false, SourceId::is_git);
      is_git && self.settings.genmode == GenMode::Remote
    };

    // Return manifest path itself if not git
    if !has_git_repo_root {
      // TODO(acmcarther): How do we know parent is valid here?
      // UNWRAP: Pathbuf guaranteed to succeed from Path
      return Ok(PathBuf::from(manifest_path.parent().unwrap()));
    }

    // If package is git package it may be nested under a parent repository. We need to find the
    // package root.
    {
      let mut check_path = manifest_path.as_path();
      while let Some(c) = check_path.parent() {
        let joined = c.join(".git");
        if joined.is_dir() {
          // UNWRAP: Pathbuf guaranteed to succeed from Path
          return Ok(PathBuf::from(c));
        } else {
          check_path = c;
        }
      }

      // Reached filesystem root and did not find Git repo
      Err(
        RazeError::Generic(format!(
          "Unable to locate git repository root for manifest at {:?}. {}",
          manifest_path, PLEASE_FILE_A_BUG
        ))
        .into(),
      )
    }
  }
}
