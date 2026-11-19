//! Dynamic single-VM workspace discovery, user `init.ns` composition, and boot synthesis.
//!
//! By Principles 1 & 5, Lava compiles system packages (`packages/*`), independent extensions
//! (`extensions/*` and `--ext-dir`), and user configuration scripts (`init.ns`) into one
//! unified Nessa compilation graph executing inside a single Nessa VM.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, UNIX_EPOCH};

use driver::{
    Diagnostic, Driver, ManifestDocument, ModuleLoader, OptLevel, PackageSources, SourceFile,
    install_artifact, load_cached_modular_artifact, load_package_sources, load_package_workspace,
};
use gc::GcConfig;
use initialization::{Engine, EngineConfig, JitConfig};
use nsbc::{CompiledArtifact, ModularArtifact};
use pkg_manager::{ModuleSources, PackageType};

use crate::host::{HostConfig, LavaHostHandler, drive_vm_loop};

/// Provenance category of a discovered Nessa package in a Lava workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageOrigin {
    /// Core infrastructure package under `<workspace_root>/packages/*`.
    CorePackage,
    /// Built-in extension package under `<workspace_root>/extensions/*`.
    BuiltinExtension,
    /// User / third-party extension package under an extra `--ext-dir` path.
    UserExtension,
}

/// Metadata summary for a discovered Lava extension or core package (`lava list-extensions`,
/// `lava check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionPackageInfo {
    pub qualified_name: String,
    pub domain: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub dependencies: Vec<String>,
    pub root_dir: PathBuf,
    pub origin: PackageOrigin,
}

/// Boot synthesis options for [`LavaWorkspace`].
#[derive(Debug, Clone)]
pub struct BootOptions {
    /// Qualified (`"lava.org/ext_app"`) or short (`"ext_app"`) entry library package name.
    pub entry_pkg: String,
    /// Initial file paths passed on the command line to open at startup.
    pub startup_files: Vec<String>,
    /// Optional headless key sequence (`--keys "..."`) injected on startup.
    pub headless_keys: String,
    /// Whether the session is running without a native UI window (`--headless`).
    pub headless: bool,
    /// Optional explicit Nessa `src/main.ns` source override for `lava.org/lava_boot`.
    pub custom_main_source: Option<String>,
}

impl Default for BootOptions {
    fn default() -> Self {
        Self {
            entry_pkg: "lava.org/ext_app".to_string(),
            startup_files: Vec::new(),
            headless_keys: String::new(),
            headless: false,
            custom_main_source: None,
        }
    }
}

impl BootOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_entry_pkg(mut self, entry_pkg: impl Into<String>) -> Self {
        self.entry_pkg = entry_pkg.into();
        self
    }

    pub fn with_startup_files(mut self, files: Vec<String>) -> Self {
        self.startup_files = files;
        self
    }

    pub fn with_headless_keys(mut self, keys: impl Into<String>) -> Self {
        self.headless_keys = keys.into();
        self
    }

    pub fn with_headless(mut self, headless: bool) -> Self {
        self.headless = headless;
        self
    }

    pub fn with_custom_main_source(mut self, source: impl Into<String>) -> Self {
        self.custom_main_source = Some(source.into());
        self
    }
}

/// Synthesized in-memory executable boot package together with its resolved source catalog.
#[derive(Debug, Clone)]
pub struct ComposedBootProgram {
    pub boot_package: PackageSources,
    pub catalog: Vec<PackageSources>,
    pub extensions: Vec<ExtensionPackageInfo>,
}

/// Modular `.nsbc` compilation output for a composed Lava workspace or entry package.
#[derive(Debug, Clone)]
pub struct CompiledBootProgram {
    /// Topologically ordered modular `.nsbc` archives (`std` first, root package last).
    pub artifacts: Vec<Arc<ModularArtifact>>,
    /// Qualified name (`domain/name`) of the root entry package.
    pub root_qualified_name: String,
    /// Non-`std` packages compiled from `.ns` sources during this invocation.
    pub compiled_packages: Vec<String>,
    /// Non-`std` packages reused from content-addressed `.nsbc` cache without recompilation.
    pub cached_packages: Vec<String>,
}

/// Executable Nessa artifact accepted by [`run_compiled_artifact`].
#[derive(Debug)]
pub enum ExecutableArtifact {
    Modular(CompiledBootProgram),
    Monolithic(CompiledArtifact),
}

impl From<CompiledBootProgram> for ExecutableArtifact {
    fn from(program: CompiledBootProgram) -> Self {
        Self::Modular(program)
    }
}

impl From<CompiledArtifact> for ExecutableArtifact {
    fn from(artifact: CompiledArtifact) -> Self {
        Self::Monolithic(artifact)
    }
}

#[derive(Debug, Clone)]
struct DiscoveredPackage {
    sources: PackageSources,
    info: ExtensionPackageInfo,
}

/// Discovers Lava packages, extensions, and user `init.ns` scripts, composing them into a
/// single Nessa compilation unit and VM runtime.
#[derive(Debug, Clone)]
pub struct LavaWorkspace {
    workspace_root: PathBuf,
    extra_ext_dirs: Vec<PathBuf>,
    user_script: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
}

impl LavaWorkspace {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            extra_ext_dirs: Vec::new(),
            user_script: None,
            cache_dir: None,
        }
    }

    pub fn with_extra_ext_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.extra_ext_dirs = dirs;
        self
    }

    pub fn with_user_script(mut self, script: Option<PathBuf>) -> Self {
        self.user_script = script;
        self
    }

    pub fn with_cache_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cache_dir = Some(dir.into());
        self
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Discover all extension packages under `<workspace_root>/extensions/*` and `extra_ext_dirs`.
    pub fn discover_extensions(&self) -> Result<Vec<ExtensionPackageInfo>, String> {
        let all = self.discover_all_packages()?;
        Ok(all
            .into_iter()
            .filter(|pkg| {
                matches!(
                    pkg.info.origin,
                    PackageOrigin::BuiltinExtension | PackageOrigin::UserExtension
                )
            })
            .map(|pkg| pkg.info)
            .collect())
    }

    /// Discover all core packages (`packages/*`) and extension packages (`extensions/*`,
    /// `extra_ext_dirs/*`).
    pub fn discover_all_package_infos(&self) -> Result<Vec<ExtensionPackageInfo>, String> {
        let all = self.discover_all_packages()?;
        Ok(all.into_iter().map(|pkg| pkg.info).collect())
    }

    fn discover_all_packages(&self) -> Result<Vec<DiscoveredPackage>, String> {
        let packages_dir = self.workspace_root.join("packages");
        if !packages_dir.is_dir() {
            return Err(format!(
                "Lava workspace packages directory not found: {}",
                packages_dir.display()
            ));
        }

        let mut by_qualified: BTreeMap<String, DiscoveredPackage> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();

        for pkg in scan_package_children(&packages_dir, PackageOrigin::CorePackage)? {
            register_discovered(&mut by_qualified, &mut order, pkg)?;
        }

        let extensions_dir = self.workspace_root.join("extensions");
        if extensions_dir.is_dir() {
            for pkg in scan_package_children(&extensions_dir, PackageOrigin::BuiltinExtension)? {
                register_discovered(&mut by_qualified, &mut order, pkg)?;
            }
        }

        for extra_dir in &self.extra_ext_dirs {
            if !extra_dir.exists() {
                return Err(format!(
                    "extra extension path does not exist: {}",
                    extra_dir.display()
                ));
            }
            if extra_dir.join("package.toml").is_file() {
                let pkg = load_single_package(extra_dir, PackageOrigin::UserExtension)?;
                register_discovered(&mut by_qualified, &mut order, pkg)?;
            } else {
                for pkg in scan_package_children(extra_dir, PackageOrigin::UserExtension)? {
                    register_discovered(&mut by_qualified, &mut order, pkg)?;
                }
            }
        }

        let mut out = Vec::with_capacity(order.len());
        for qname in order {
            if let Some(pkg) = by_qualified.remove(&qname) {
                out.push(pkg);
            }
        }
        Ok(out)
    }

    /// Compose the in-memory `lava.org/lava_boot` executable package, optional
    /// `user.local/user_init` library wrapper, and complete dependency catalog.
    pub fn compose_boot(&self, options: &BootOptions) -> Result<ComposedBootProgram, String> {
        let discovered = self.discover_all_packages()?;
        let extensions: Vec<ExtensionPackageInfo> = discovered
            .iter()
            .filter(|p| {
                matches!(
                    p.info.origin,
                    PackageOrigin::BuiltinExtension | PackageOrigin::UserExtension
                )
            })
            .map(|p| p.info.clone())
            .collect();

        let lib_packages: Vec<&DiscoveredPackage> = discovered
            .iter()
            .filter(|p| p.sources.manifest.manifest().package_type == PackageType::Lib)
            .collect();

        let entry_pkg = find_entry_package(&lib_packages, &options.entry_pkg)?;
        let mut catalog: Vec<PackageSources> =
            lib_packages.iter().map(|p| p.sources.clone()).collect();

        let user_init_pkg = if let Some(script_path) = &self.user_script {
            let pkg = synthesize_user_init_package(script_path, &lib_packages)?;
            catalog.push(pkg.clone());
            Some(pkg)
        } else {
            None
        };

        let boot_package = synthesize_boot_package(
            &self.workspace_root,
            entry_pkg,
            &lib_packages,
            user_init_pkg.as_ref(),
            options,
        )?;

        Ok(ComposedBootProgram {
            boot_package,
            catalog,
            extensions,
        })
    }

    /// Type-check and separately compile the composed single-VM boot graph into per-package
    /// modular `.nsbc` archives, reusing a sub-5ms stat-manifest pointer or interface-hash
    /// DAG `.nsbc` cache so editing one package recompiles only affected downstream packages.
    pub fn compile_boot(&self, options: &BootOptions) -> Result<CompiledBootProgram, String> {
        let stat_ptr_path = if can_use_stat_manifest_cache(options) {
            self.compute_stat_manifest_ptr_path(options)
        } else {
            None
        };
        if let Some(ptr_path) = &stat_ptr_path
            && let Some(cached) = try_load_stat_cached_modular(ptr_path)
        {
            return Ok(cached);
        }

        let composed = self.compose_boot(options)?;
        let cache_dir = resolve_nsbc_cache_dir(self.cache_dir.as_deref());
        let opt_level = resolve_lava_opt_level();
        let compile_start = Instant::now();
        let privileged = BTreeSet::new();
        let output = make_modular_driver(opt_level, cache_dir.as_deref())
            .compile_modular_graph_cached(&composed.boot_package, &composed.catalog, &privileged)
            .map_err(|diags| format_modular_diagnostics(&diags))?;
        let elapsed = compile_start.elapsed().as_secs_f64();

        if !output.compiled_packages.is_empty() {
            eprintln!(
                "[lava] compiled {} package(s), reused {} cached ({opt_level:?}) in {elapsed:.2}s",
                output.compiled_packages.len(),
                output.cached_packages.len(),
            );
        }
        if let Some(dir) = &cache_dir {
            if let Some(ptr_path) = &stat_ptr_path {
                store_stat_manifest_modular_ptr(
                    ptr_path,
                    &output.root_qualified_name,
                    &output.cache_paths,
                );
            }
            prune_old_cache_files(&dir.join("modular"), "nsbc", 512);
            prune_old_cache_files(dir, "ptr", 36);
        }

        Ok(CompiledBootProgram {
            artifacts: output.artifacts,
            root_qualified_name: output.root_qualified_name,
            compiled_packages: output.compiled_packages,
            cached_packages: output.cached_packages,
        })
    }

    /// Validate all discovered extensions (and optional `init.ns`) via modular per-package
    /// `.nsbc` compilation, warming the per-package `.nsbc` and stat-manifest caches.
    pub fn check_boot(&self, options: &BootOptions) -> Result<Vec<ExtensionPackageInfo>, String> {
        let composed = self.compose_boot(options)?;
        let cache_dir = resolve_nsbc_cache_dir(self.cache_dir.as_deref());
        let opt_level = resolve_lava_opt_level();
        let ext_count = composed.extensions.len();
        eprintln!("[lava] checking {ext_count} Nessa extension(s) ({opt_level:?})...");
        let compile_start = Instant::now();
        let privileged = BTreeSet::new();
        let output = make_modular_driver(opt_level, cache_dir.as_deref())
            .compile_modular_graph_cached(&composed.boot_package, &composed.catalog, &privileged)
            .map_err(|diags| format_modular_diagnostics(&diags))?;
        eprintln!(
            "[lava] checked in {:.2}s ({} compiled, {} cached)",
            compile_start.elapsed().as_secs_f64(),
            output.compiled_packages.len(),
            output.cached_packages.len(),
        );
        if let Some(dir) = &cache_dir {
            if can_use_stat_manifest_cache(options)
                && let Some(ptr_path) = self.compute_stat_manifest_ptr_path(options)
            {
                store_stat_manifest_modular_ptr(
                    &ptr_path,
                    &output.root_qualified_name,
                    &output.cache_paths,
                );
            }
            prune_old_cache_files(&dir.join("modular"), "nsbc", 512);
            prune_old_cache_files(dir, "ptr", 36);
        }
        Ok(composed.extensions)
    }

    /// Compile and run the composed boot program inside a single Nessa VM with [`LavaHostHandler`].
    pub fn compile_and_run_boot(
        &self,
        options: &BootOptions,
        host_config: HostConfig,
    ) -> Result<i32, String> {
        let artifact = self.compile_boot(options)?;
        run_compiled_artifact(artifact, host_config)
    }
}

/// Discover extension metadata from `workspace_root/extensions/*` and `extra_ext_dirs`.
pub fn discover_extensions(
    workspace_root: &Path,
    extra_ext_dirs: &[PathBuf],
) -> Result<Vec<ExtensionPackageInfo>, String> {
    LavaWorkspace::new(workspace_root)
        .with_extra_ext_dirs(extra_ext_dirs.to_vec())
        .discover_extensions()
}

/// Type-check and validate the full workspace + extensions + optional `user_script`.
pub fn check_workspace(
    workspace_root: &Path,
    extra_ext_dirs: &[PathBuf],
    user_script: Option<&Path>,
    options: &BootOptions,
) -> Result<Vec<ExtensionPackageInfo>, String> {
    LavaWorkspace::new(workspace_root)
        .with_extra_ext_dirs(extra_ext_dirs.to_vec())
        .with_user_script(user_script.map(Path::to_path_buf))
        .check_boot(options)
}

/// Compile and execute the synthesized single-VM boot package.
pub fn compile_and_run_boot(
    workspace_root: &Path,
    extra_ext_dirs: &[PathBuf],
    user_script: Option<&Path>,
    options: &BootOptions,
    host_config: HostConfig,
) -> Result<i32, String> {
    LavaWorkspace::new(workspace_root)
        .with_extra_ext_dirs(extra_ext_dirs.to_vec())
        .with_user_script(user_script.map(Path::to_path_buf))
        .compile_and_run_boot(options, host_config)
}

/// Compile and execute an explicit entry package directory (`type = "exe"` or `type = "lib"`)
/// inside a single Nessa VM wired with [`LavaHostHandler`].
pub fn run_entry_package(
    workspace_root: &Path,
    entry_pkg_dir: &Path,
    host_config: HostConfig,
) -> Result<i32, String> {
    let raw_entry = load_package_sources(entry_pkg_dir).map_err(|err| {
        format!(
            "failed to load entry package {}: {err}",
            entry_pkg_dir.display()
        )
    })?;

    if raw_entry.manifest.manifest().package_type == PackageType::Lib {
        let qname = raw_entry.manifest.manifest().qualified_name();
        let mut extra_dirs = Vec::new();
        let ws_packages = workspace_root.join("packages");
        let ws_extensions = workspace_root.join("extensions");
        let canonical_entry =
            fs::canonicalize(entry_pkg_dir).unwrap_or_else(|_| entry_pkg_dir.to_path_buf());
        let in_builtin = canonical_entry
            .starts_with(fs::canonicalize(&ws_packages).unwrap_or(ws_packages))
            || canonical_entry
                .starts_with(fs::canonicalize(&ws_extensions).unwrap_or(ws_extensions));
        if !in_builtin {
            extra_dirs.push(entry_pkg_dir.to_path_buf());
        }
        let options = BootOptions::default().with_entry_pkg(qname);
        return LavaWorkspace::new(workspace_root)
            .with_extra_ext_dirs(extra_dirs)
            .compile_and_run_boot(&options, host_config);
    }

    let (mut root_pkg, mut catalog) = match load_package_workspace(entry_pkg_dir) {
        Ok(ws) => (ws.root, ws.dependencies),
        Err(_) => (raw_entry, Vec::new()),
    };
    attach_host_abi_anchor_module(&mut root_pkg);

    if workspace_root.join("packages").is_dir() {
        let discovered = LavaWorkspace::new(workspace_root).discover_all_packages()?;
        let mut existing_keys: BTreeSet<(String, String)> = catalog
            .iter()
            .map(|p| {
                let m = p.manifest.manifest();
                (m.qualified_name(), m.version.to_string())
            })
            .collect();
        let root_m = root_pkg.manifest.manifest();
        existing_keys.insert((root_m.qualified_name(), root_m.version.to_string()));

        for pkg in discovered {
            let m = pkg.sources.manifest.manifest();
            if m.package_type == PackageType::Lib {
                let key = (m.qualified_name(), m.version.to_string());
                if existing_keys.insert(key) {
                    catalog.push(pkg.sources);
                }
            }
        }
    }

    let cache_dir = resolve_nsbc_cache_dir(None);
    let opt_level = resolve_lava_opt_level();
    let pkg_name = root_pkg.manifest.manifest().name.clone();
    let compile_start = Instant::now();
    let privileged = BTreeSet::new();
    let output = make_modular_driver(opt_level, cache_dir.as_deref())
        .compile_modular_graph_cached(&root_pkg, &catalog, &privileged)
        .map_err(|diags| format_modular_diagnostics(&diags))?;
    let elapsed = compile_start.elapsed().as_secs_f64();
    if !output.compiled_packages.is_empty() {
        eprintln!(
            "[lava] compiled '{pkg_name}' ({} package(s), {} cached, {opt_level:?}) in {elapsed:.2}s",
            output.compiled_packages.len(),
            output.cached_packages.len(),
        );
    }
    if let Some(dir) = &cache_dir {
        prune_old_cache_files(&dir.join("modular"), "nsbc", 512);
    }
    let program = CompiledBootProgram {
        artifacts: output.artifacts,
        root_qualified_name: output.root_qualified_name,
        compiled_packages: output.compiled_packages,
        cached_packages: output.cached_packages,
    };
    run_compiled_artifact(program, host_config)
}

fn resolve_vm_heap_bytes() -> usize {
    const DEFAULT_HEAP_MB: usize = 512;
    const MIN_HEAP_MB: usize = 64;
    const MAX_HEAP_MB: usize = 4_096;
    let mb = std::env::var("LAVA_HEAP_MB")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .map(|parsed| parsed.clamp(MIN_HEAP_MB, MAX_HEAP_MB))
        .unwrap_or(DEFAULT_HEAP_MB);
    mb * 1024 * 1024
}

/// Install a modular [`CompiledBootProgram`] (via demand-driven per-`mod` [`ModuleLoader`]) or
/// monolithic [`CompiledArtifact`] into a fresh Nessa [`Engine`], attach [`LavaHostHandler`],
/// spawn the root entry task, and drive the cooperative VM event loop to completion.
pub fn run_compiled_artifact<A: Into<ExecutableArtifact>>(
    artifact: A,
    host_config: HostConfig,
) -> Result<i32, String> {
    let heap_size = resolve_vm_heap_bytes();
    let mut engine = Engine::new(EngineConfig {
        gc: GcConfig {
            heap_size,
            ..Default::default()
        },
        jit: JitConfig::default(),
        debug: false,
    });
    let (handler, controller, script_args) = LavaHostHandler::from_config(host_config);
    if !script_args.is_empty() {
        engine.vm_mut().set_script_args(script_args);
    }

    match artifact.into() {
        ExecutableArtifact::Modular(program) => {
            let mut loader = ModuleLoader::new();
            loader
                .register_artifacts(&program.artifacts)
                .map_err(|err| format!("failed to register modular Nessa artifacts: {err}"))?;
            let prepared = loader
                .prepare_module(engine.vm_mut(), &program.root_qualified_name, &[])
                .map_err(|err| {
                    format!(
                        "failed to load modular Nessa root `{}`: {err}",
                        program.root_qualified_name
                    )
                })?;
            let entry = loader
                .synthesize_entry_sequence(engine.vm_mut(), &prepared)
                .map_err(|err| {
                    format!("failed to synthesize modular Nessa entry sequence: {err}")
                })?;
            engine.vm_mut().set_host_handler(Box::new(handler));
            let Some(entry_func) = entry else {
                return Ok(0);
            };
            engine.vm_mut().spawn_root(entry_func);
            let exit_code = drive_vm_loop(engine.vm_mut(), &controller)?;
            drop(loader);
            Ok(exit_code)
        }
        ExecutableArtifact::Monolithic(mono) => {
            let entry = install_artifact(engine.vm_mut(), mono)
                .map_err(|err| format!("failed to install compiled Nessa artifact: {err}"))?;
            engine.vm_mut().set_host_handler(Box::new(handler));
            let Some(entry_func) = entry else {
                return Ok(0);
            };
            engine.vm_mut().spawn_root(entry_func);
            drive_vm_loop(engine.vm_mut(), &controller)
        }
    }
}

fn make_modular_driver(opt_level: OptLevel, cache_dir: Option<&Path>) -> Driver {
    let drv = Driver::new().with_opt_level(opt_level);
    match cache_dir {
        Some(dir) => drv.with_cache_dir(dir.to_path_buf()),
        None => drv.without_cache(),
    }
}

fn format_modular_diagnostics(diagnostics: &[Diagnostic]) -> String {
    let mut lines = Vec::with_capacity(diagnostics.len());
    for diag in diagnostics {
        let mut msg = diag.message.clone();
        for note in &diag.notes {
            msg.push_str(&format!(" (note: {note})"));
        }
        for help in &diag.helps {
            msg.push_str(&format!(" (help: {help})"));
        }
        lines.push(msg);
    }
    format!("Nessa compilation failed:\n{}", lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Content-addressed Modular `.nsbc` & Stat-Manifest Cache
// ---------------------------------------------------------------------------

static CACHE_TMP_SEQ: AtomicU64 = AtomicU64::new(1);

struct ContentHasher128 {
    lo: u64,
    hi: u64,
}

impl ContentHasher128 {
    fn new() -> Self {
        Self {
            lo: 0xcbf2_9ce4_8422_2325,
            hi: 0x8422_2325_cbf2_9ce4,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        let (chunks, rem) = bytes.as_chunks::<8>();
        for &chunk in chunks {
            let word = u64::from_le_bytes(chunk);
            self.lo = (self.lo ^ word)
                .wrapping_mul(0x0000_0100_0000_01b3)
                .rotate_left(23);
            self.hi = self
                .hi
                .rotate_left(17)
                .wrapping_add(word.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }
        for &b in rem {
            self.lo ^= u64::from(b);
            self.lo = self.lo.wrapping_mul(0x0000_0100_0000_01b3);
            self.hi = self
                .hi
                .rotate_left(7)
                .wrapping_add(u64::from(b).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        }
    }

    fn write_str(&mut self, s: &str) {
        self.write_bytes(s.as_bytes());
    }

    fn write_u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.lo ^= u64::from(b);
            self.lo = self.lo.wrapping_mul(0x0000_0100_0000_01b3);
            self.hi = self
                .hi
                .rotate_left(11)
                .wrapping_add(u64::from(b).wrapping_mul(0xbf58_476d_1ce4_e5b9));
        }
    }

    fn finish_hex(&self) -> String {
        format!("{:016x}{:016x}", self.hi, self.lo)
    }
}

fn resolve_nsbc_cache_dir_from_vars(
    override_dir: Option<&Path>,
    env_cache_dir: Option<&str>,
    xdg_cache_home: Option<&str>,
    home_dir: Option<&str>,
) -> Option<PathBuf> {
    if let Some(dir) = override_dir {
        return Some(dir.to_path_buf());
    }
    if let Some(explicit) = env_cache_dir.map(str::trim).filter(|s| !s.is_empty()) {
        if explicit == "0" || explicit.eq_ignore_ascii_case("off") {
            return None;
        }
        return Some(PathBuf::from(explicit));
    }
    if let Some(xdg) = xdg_cache_home.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("lava").join("nsbc"));
    }
    if let Some(home) = home_dir.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(home).join(".cache").join("lava").join("nsbc"));
    }
    Some(std::env::temp_dir().join("lava-nsbc-cache"))
}

fn resolve_nsbc_cache_dir(override_dir: Option<&Path>) -> Option<PathBuf> {
    let env_cache_dir = std::env::var("LAVA_NSBC_CACHE_DIR").ok();
    let xdg_cache_home = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("LOCALAPPDATA")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    let home_dir = std::env::var("HOME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|s| !s.trim().is_empty())
        });
    resolve_nsbc_cache_dir_from_vars(
        override_dir,
        env_cache_dir.as_deref(),
        xdg_cache_home.as_deref(),
        home_dir.as_deref(),
    )
}

fn resolve_lava_opt_level() -> OptLevel {
    if std::env::var_os("NESSA_OPT_LEVEL").is_some() {
        OptLevel::from_env()
    } else {
        OptLevel::O3
    }
}

fn prune_old_cache_files(dir: &Path, ext: &str, keep_latest: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some(ext) {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            files.push((mtime, p));
        }
    }
    if files.len() <= keep_latest {
        return;
    }
    files.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, old_path) in files.into_iter().skip(keep_latest) {
        let _ = fs::remove_file(old_path);
    }
}

fn can_use_stat_manifest_cache(options: &BootOptions) -> bool {
    options.custom_main_source.is_none()
        && options.startup_files.is_empty()
        && options.headless_keys.is_empty()
        && !options.headless
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct StatEntry {
    key: String,
    file_len: u64,
    mtime_secs: u64,
    mtime_nanos: u32,
}

fn record_file_stat(
    path: &Path,
    base_root: &Path,
    tag: &str,
    out: &mut Vec<StatEntry>,
) -> Option<()> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let dur = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .unwrap_or_default();
    let rel_display = path
        .strip_prefix(base_root)
        .map(|p| format!("{tag}:{}", p.to_string_lossy()))
        .unwrap_or_else(|_| format!("{tag}:{}", path.to_string_lossy()));
    out.push(StatEntry {
        key: rel_display,
        file_len: meta.len(),
        mtime_secs: dur.as_secs(),
        mtime_nanos: dur.subsec_nanos(),
    });
    Some(())
}

fn collect_src_ns_stats(
    dir: &Path,
    base_root: &Path,
    tag: &str,
    out: &mut Vec<StatEntry>,
) -> Option<()> {
    let entries = fs::read_dir(dir).ok()?;
    for entry_res in entries {
        let entry = entry_res.ok()?;
        let path = entry.path();
        let ftype = entry.file_type().ok()?;
        if ftype.is_dir() {
            collect_src_ns_stats(&path, base_root, tag, out)?;
        } else if (ftype.is_file() || ftype.is_symlink())
            && path.extension().and_then(|e| e.to_str()) == Some("ns")
        {
            record_file_stat(&path, base_root, tag, out)?;
        }
    }
    Some(())
}

fn collect_single_pkg_stats(
    pkg_dir: &Path,
    base_root: &Path,
    tag: &str,
    out: &mut Vec<StatEntry>,
) -> Option<()> {
    let manifest_path = pkg_dir.join("package.toml");
    record_file_stat(&manifest_path, base_root, tag, out)?;
    let src_dir = pkg_dir.join("src");
    if src_dir.is_dir() {
        collect_src_ns_stats(&src_dir, base_root, tag, out)?;
    }
    Some(())
}

fn collect_parent_pkg_children_stats(
    parent_dir: &Path,
    base_root: &Path,
    tag: &str,
    out: &mut Vec<StatEntry>,
) -> Option<()> {
    let entries = fs::read_dir(parent_dir).ok()?;
    for entry_res in entries {
        let entry = entry_res.ok()?;
        let path = entry.path();
        if path.is_dir() && path.join("package.toml").is_file() {
            collect_single_pkg_stats(&path, base_root, tag, out)?;
        }
    }
    Some(())
}

impl LavaWorkspace {
    fn compute_stat_manifest_ptr_path(&self, options: &BootOptions) -> Option<PathBuf> {
        let cache_dir = resolve_nsbc_cache_dir(self.cache_dir.as_deref())?;
        let packages_dir = self.workspace_root.join("packages");
        if !packages_dir.is_dir() {
            return None;
        }

        let mut stats: Vec<StatEntry> = Vec::with_capacity(96);
        collect_parent_pkg_children_stats(&packages_dir, &self.workspace_root, "ws", &mut stats)?;

        let extensions_dir = self.workspace_root.join("extensions");
        if extensions_dir.is_dir() {
            collect_parent_pkg_children_stats(
                &extensions_dir,
                &self.workspace_root,
                "ws",
                &mut stats,
            )?;
        }

        for (idx, extra_dir) in self.extra_ext_dirs.iter().enumerate() {
            if !extra_dir.exists() {
                return None;
            }
            let tag = format!("ext{idx}:{}", extra_dir.to_string_lossy());
            if extra_dir.join("package.toml").is_file() {
                collect_single_pkg_stats(extra_dir, extra_dir, &tag, &mut stats)?;
            } else {
                collect_parent_pkg_children_stats(extra_dir, extra_dir, &tag, &mut stats)?;
            }
        }

        if let Some(script_path) = &self.user_script {
            record_file_stat(script_path, Path::new(""), "init", &mut stats)?;
        }

        stats.sort();

        let mut hasher = ContentHasher128::new();
        hasher.write_str("lava-stat-mod-v1");
        hasher.write_str(std::env::consts::OS);
        hasher.write_str(env!("CARGO_PKG_VERSION"));
        hasher.write_u64(u64::from(nsbc::VERSION));
        hasher.write_u64(u64::from(runtime::BUILTIN_ABI_VERSION));
        hasher.write_u64(u64::from(resolve_lava_opt_level().as_u8()));
        hasher.write_str(&options.entry_pkg);
        hasher.write_u64(u64::from(self.user_script.is_some()));
        hasher.write_u64(stats.len() as u64);
        for st in &stats {
            hasher.write_str(&st.key);
            hasher.write_u64(st.file_len);
            hasher.write_u64(st.mtime_secs);
            hasher.write_u64(u64::from(st.mtime_nanos));
        }

        Some(cache_dir.join(format!("stat-v1-{}.ptr", hasher.finish_hex())))
    }
}

fn try_load_stat_cached_modular(ptr_path: &Path) -> Option<CompiledBootProgram> {
    let cache_dir = ptr_path.parent()?;
    let raw = fs::read_to_string(ptr_path).ok()?;
    let mut lines = raw.lines().map(str::trim).filter(|l| !l.is_empty());
    let root_qualified_name = lines.next()?.to_owned();
    let mut nsbc_paths: Vec<PathBuf> = Vec::new();
    let modular_dir = cache_dir.join("modular");
    for fname in lines {
        if !fname.ends_with(".nsbc") || fname.contains('/') || fname.contains('\\') {
            let _ = fs::remove_file(ptr_path);
            return None;
        }
        nsbc_paths.push(modular_dir.join(fname));
    }
    if nsbc_paths.len() < 2 {
        let _ = fs::remove_file(ptr_path);
        return None;
    }

    let loaded: Vec<Option<Arc<ModularArtifact>>> = if nsbc_paths.len() <= 4 {
        nsbc_paths
            .iter()
            .map(|p| load_cached_modular_artifact(p))
            .collect()
    } else {
        let mut out = Vec::with_capacity(nsbc_paths.len());
        for chunk in nsbc_paths.chunks(8) {
            let chunk_res: Vec<Option<Arc<ModularArtifact>>> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|p| s.spawn(move || load_cached_modular_artifact(p)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("modular cache reader thread panicked"))
                    .collect()
            });
            out.extend(chunk_res);
        }
        out
    };

    let mut artifacts = Vec::with_capacity(loaded.len());
    for item in loaded {
        match item {
            Some(art) => artifacts.push(art),
            None => {
                let _ = fs::remove_file(ptr_path);
                return None;
            }
        }
    }
    let cached_packages = artifacts
        .iter()
        .skip(1)
        .map(|a| a.exports.qualified_name.clone())
        .collect();
    Some(CompiledBootProgram {
        artifacts,
        root_qualified_name,
        compiled_packages: Vec::new(),
        cached_packages,
    })
}

fn store_stat_manifest_modular_ptr(
    ptr_path: &Path,
    root_qualified_name: &str,
    cache_paths: &[Option<PathBuf>],
) {
    if cache_paths.len() < 2 {
        return;
    }
    let Some(parent) = ptr_path.parent() else {
        return;
    };
    let mut payload = String::with_capacity(64 * (cache_paths.len() + 1));
    payload.push_str(root_qualified_name);
    payload.push('\n');
    for opt_path in cache_paths {
        let Some(p) = opt_path.as_deref() else {
            return;
        };
        let Some(fname) = p.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        payload.push_str(fname);
        payload.push('\n');
    }
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let seq = CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_name = format!(
        ".{}.tmp.{}.{}",
        ptr_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("stat.ptr"),
        std::process::id(),
        seq
    );
    let tmp_path = parent.join(tmp_name);
    if fs::write(&tmp_path, payload.as_bytes()).is_ok() && fs::rename(&tmp_path, ptr_path).is_err()
    {
        let _ = fs::remove_file(&tmp_path);
    }
}

fn is_platform_disabled_extension(qualified_name: &str) -> bool {
    // Windows 10 1709 predates ConPTY (`ext_toggleterm`) and disables Tree-sitter (`ext_treesitter`).
    cfg!(windows)
        && matches!(
            qualified_name,
            "lava.org/ext_toggleterm" | "lava.org/ext_treesitter"
        )
}

fn scan_package_children(
    parent_dir: &Path,
    origin: PackageOrigin,
) -> Result<Vec<DiscoveredPackage>, String> {
    let read_dir = fs::read_dir(parent_dir).map_err(|err| {
        format!(
            "cannot read package directory {}: {err}",
            parent_dir.display()
        )
    })?;

    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry_res in read_dir {
        let entry = entry_res
            .map_err(|err| format!("error reading entry in {}: {err}", parent_dir.display()))?;
        let path = entry.path();
        if path.is_dir() && path.join("package.toml").is_file() {
            subdirs.push(path);
        }
    }
    subdirs.sort();

    let mut packages = Vec::with_capacity(subdirs.len());
    for subdir in subdirs {
        let pkg = load_single_package(&subdir, origin)?;
        if !is_platform_disabled_extension(&pkg.info.qualified_name) {
            packages.push(pkg);
        }
    }
    Ok(packages)
}

fn load_single_package(pkg_dir: &Path, origin: PackageOrigin) -> Result<DiscoveredPackage, String> {
    let sources = load_package_sources(pkg_dir)
        .map_err(|err| format!("failed to load package at {}: {err}", pkg_dir.display()))?;
    let manifest = sources.manifest.manifest();
    let description = extract_package_description(&sources);
    let dependencies = manifest
        .dependencies
        .iter()
        .map(|dep| format!("{}/{}", dep.domain, dep.name))
        .collect();

    let info = ExtensionPackageInfo {
        qualified_name: manifest.qualified_name(),
        domain: manifest.domain.clone(),
        name: manifest.name.clone(),
        version: manifest.version.to_string(),
        description,
        dependencies,
        root_dir: sources.root.clone(),
        origin,
    };

    Ok(DiscoveredPackage { sources, info })
}

fn register_discovered(
    by_qualified: &mut BTreeMap<String, DiscoveredPackage>,
    order: &mut Vec<String>,
    pkg: DiscoveredPackage,
) -> Result<(), String> {
    let qname = pkg.info.qualified_name.clone();
    if let Some(existing) = by_qualified.get(&qname) {
        if pkg.info.origin == PackageOrigin::UserExtension
            && existing.info.origin != PackageOrigin::CorePackage
        {
            by_qualified.insert(qname, pkg);
            return Ok(());
        }
        if existing.info.root_dir != pkg.info.root_dir {
            return Err(format!(
                "duplicate package `{qname}` found at {} and {}",
                existing.info.root_dir.display(),
                pkg.info.root_dir.display()
            ));
        }
        return Ok(());
    }
    order.push(qname.clone());
    by_qualified.insert(qname, pkg);
    Ok(())
}

fn extract_package_description(sources: &PackageSources) -> String {
    if let Some(desc) = sources
        .manifest
        .metadata()
        .get("package")
        .and_then(|pkg| pkg.as_table())
        .and_then(|tbl| tbl.get("description"))
        .and_then(|val| val.as_str())
    {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let pkg_name = &sources.manifest.manifest().name;
    for line in sources.entry.source.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(comment) = trimmed.strip_prefix("--") {
            let clean = comment.trim();
            if clean.is_empty() {
                continue;
            }
            let prefix = format!("{pkg_name}:");
            if let Some(rest) = clean.strip_prefix(&prefix) {
                return rest.trim().to_string();
            }
            return clean.to_string();
        }
        break;
    }
    String::new()
}

fn find_entry_package<'a>(
    lib_packages: &[&'a DiscoveredPackage],
    entry_spec: &str,
) -> Result<&'a DiscoveredPackage, String> {
    if let Some(pkg) = lib_packages
        .iter()
        .copied()
        .find(|p| p.info.qualified_name == entry_spec)
    {
        return Ok(pkg);
    }
    if !entry_spec.contains('/')
        && let Some(pkg) = lib_packages
            .iter()
            .copied()
            .find(|p| p.info.name == entry_spec)
    {
        return Ok(pkg);
    }
    let available: Vec<&str> = lib_packages
        .iter()
        .map(|p| p.info.qualified_name.as_str())
        .collect();
    Err(format!(
        "entry package `{entry_spec}` not found in workspace (available: {})",
        available.join(", ")
    ))
}

fn synthesize_user_init_package(
    script_path: &Path,
    lib_packages: &[&DiscoveredPackage],
) -> Result<PackageSources, String> {
    let source = fs::read_to_string(script_path).map_err(|err| {
        format!(
            "cannot read user init script {}: {err}",
            script_path.display()
        )
    })?;

    // Deduplicate dependencies by short package name (later entries override earlier ones)
    // so `@pkg` imports in `init.ns` are always unambiguous.
    let mut deps_by_short_name: BTreeMap<String, (&str, &str)> = BTreeMap::new();
    for pkg in lib_packages {
        if pkg.info.qualified_name == "user.local/user_init" {
            continue;
        }
        deps_by_short_name.insert(
            pkg.info.name.clone(),
            (pkg.info.qualified_name.as_str(), pkg.info.version.as_str()),
        );
    }

    let mut manifest_toml = String::from(
        "[package]\nname = \"user_init\"\ndomain = \"user.local\"\nversion = \"0.1.0\"\ntype = \"lib\"\n\n[dependencies]\n",
    );
    for (qname, version) in deps_by_short_name.values() {
        manifest_toml.push_str(&format!(
            "\"{}\" = \"{}\"\n",
            escape_toml_string(qname),
            escape_toml_string(version)
        ));
    }

    let manifest = ManifestDocument::parse(&manifest_toml)
        .map_err(|err| format!("failed to synthesize user_init manifest: {err}"))?;

    let root = script_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    Ok(PackageSources {
        root,
        manifest,
        entry: SourceFile {
            file_path: script_path.to_path_buf(),
            source,
        },
        modules: Vec::new(),
        wasm_assets: Vec::new(),
    })
}

fn synthesize_boot_package(
    workspace_root: &Path,
    entry_pkg: &DiscoveredPackage,
    lib_packages: &[&DiscoveredPackage],
    user_init_pkg: Option<&PackageSources>,
    options: &BootOptions,
) -> Result<PackageSources, String> {
    let mut boot_deps: BTreeMap<String, String> = BTreeMap::new();
    boot_deps.insert(
        entry_pkg.info.qualified_name.clone(),
        entry_pkg.info.version.clone(),
    );

    let has_lava_sys = lib_packages
        .iter()
        .find(|p| p.info.qualified_name == "lava.org/lava_sys");
    if let Some(sys_pkg) = has_lava_sys {
        boot_deps.insert(
            sys_pkg.info.qualified_name.clone(),
            sys_pkg.info.version.clone(),
        );
    }

    let has_lava_core = lib_packages
        .iter()
        .find(|p| p.info.qualified_name == "lava.org/lava_core");
    if let Some(core_pkg) = has_lava_core {
        boot_deps.insert(
            core_pkg.info.qualified_name.clone(),
            core_pkg.info.version.clone(),
        );
    }

    // Discover any built-in plugin extensions (`extensions/ext_*`) or dynamic `--ext-dir`
    // user extensions not already declared as direct dependencies of `entry_pkg` that expose
    // an extension registration function (`pub fn register_*_extension(kernel: SystemKernel)`).
    let entry_declared_deps: BTreeSet<&str> = entry_pkg
        .info
        .dependencies
        .iter()
        .map(String::as_str)
        .collect();
    let mut auto_user_exts: Vec<(&DiscoveredPackage, NessaPubFn)> = Vec::new();
    for pkg in lib_packages {
        if matches!(
            pkg.info.origin,
            PackageOrigin::BuiltinExtension | PackageOrigin::UserExtension
        ) && pkg.info.qualified_name != entry_pkg.info.qualified_name
            && !entry_declared_deps.contains(pkg.info.qualified_name.as_str())
            && let Some(reg_fn) = find_extension_register_fn(&pkg.sources)
        {
            boot_deps.insert(pkg.info.qualified_name.clone(), pkg.info.version.clone());
            auto_user_exts.push((*pkg, reg_fn));
        }
    }

    if user_init_pkg.is_some() {
        boot_deps.insert("user.local/user_init".to_string(), "0.1.0".to_string());
    }

    let main_source = if let Some(custom) = &options.custom_main_source {
        // When custom main source is provided, expose all non-conflicting discovered libraries
        // so custom boot scripts can import any workspace package or extension freely.
        let mut seen_short: BTreeSet<String> = boot_deps
            .keys()
            .filter_map(|q| q.split_once('/').map(|(_, n)| n.to_string()))
            .collect();
        for pkg in lib_packages {
            if seen_short.insert(pkg.info.name.clone()) {
                boot_deps
                    .entry(pkg.info.qualified_name.clone())
                    .or_insert_with(|| pkg.info.version.clone());
            }
        }
        custom.clone()
    } else {
        generate_boot_main_source(
            entry_pkg,
            has_lava_sys.is_some(),
            has_lava_core.is_some(),
            &auto_user_exts,
            user_init_pkg,
            options,
        )?
    };

    let mut manifest_toml = String::from(
        "[package]\nname = \"lava_boot\"\ndomain = \"lava.org\"\nversion = \"0.1.0\"\ntype = \"exe\"\n\n[dependencies]\n",
    );
    for (qname, version) in &boot_deps {
        manifest_toml.push_str(&format!(
            "\"{}\" = \"{}\"\n",
            escape_toml_string(qname),
            escape_toml_string(version)
        ));
    }

    let manifest = ManifestDocument::parse(&manifest_toml)
        .map_err(|err| format!("failed to synthesize lava_boot manifest: {err}"))?;

    let mut boot_pkg = PackageSources {
        root: workspace_root.join("__lava_boot__"),
        manifest,
        entry: SourceFile {
            file_path: workspace_root.join("__lava_boot__/src/main.ns"),
            source: main_source,
        },
        modules: Vec::new(),
        wasm_assets: Vec::new(),
    };
    attach_host_abi_anchor_module(&mut boot_pkg);
    Ok(boot_pkg)
}

/// Injects an internal, non-exported module into a root package so Nessa's type resolver and
/// `CompilationIdentityContext` pre-intern and finalize every `(bool, T)` structural tuple and
/// `?T` optional type that `std.runtime.host_call` (`BuiltinCtx::invoke_host_call`) can allocate.
fn attach_host_abi_anchor_module(pkg: &mut PackageSources) {
    const MODULE_NAME: &str = "__lava_host_abi_types";
    if pkg
        .modules
        .iter()
        .any(|m| m.path.first().map(String::as_str) == Some(MODULE_NAME))
    {
        return;
    }
    let anchor_source = "\
-- Pre-registers `(bool, T)` structural tuples and optional inner types returned by
-- `std.runtime.host_call` so finalized multi-package `TypePool` identities cover every
-- `NativeValue` variant without runtime structural interning.
private struct LavaHostCallTupleAbi {
    t_unit: (bool, Unit),
    t_bool: (bool, bool),
    t_char: (bool, char),
    t_i64: (bool, i64),
    t_u64: (bool, u64),
    t_f64: (bool, f64),
    t_str: (bool, String),
    t_bytes: (bool, Bytes),
    t_any: (bool, Any),
    t_list: (bool, List),
    t_map: (bool, Map),
    t_opt_any: (bool, ?Any),
    t_opt_i64: (bool, ?i64),
    t_opt_f64: (bool, ?f64),
    t_opt_str: (bool, ?String),
}
";
    pkg.modules.push(ModuleSources {
        path: vec![MODULE_NAME.to_string()],
        entry: SourceFile {
            file_path: pkg.root.join("src/__lava_host_abi_types.ns"),
            source: anchor_source.to_string(),
        },
        children: Vec::new(),
    });
}

// ---------------------------------------------------------------------------
// Nessa Signature Inspection & `lava_boot/src/main.ns` Synthesis
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct NessaParam {
    name: String,
    is_named: bool,
    ty: String,
    has_default: bool,
}

#[derive(Debug, Clone)]
struct NessaPubFn {
    name: String,
    params: Vec<NessaParam>,
    ret_is_error_union: bool,
    ret_payload_ty: String,
}

impl NessaPubFn {
    fn required_positional(&self) -> Vec<&NessaParam> {
        self.params
            .iter()
            .filter(|p| !p.is_named && !p.has_default)
            .collect()
    }
}

fn generate_boot_main_source(
    entry_pkg: &DiscoveredPackage,
    has_lava_sys: bool,
    has_lava_core: bool,
    auto_user_exts: &[(&DiscoveredPackage, NessaPubFn)],
    user_init_pkg: Option<&PackageSources>,
    options: &BootOptions,
) -> Result<String, String> {
    let entry_alias = &entry_pkg.info.name;
    let entry_fns = scan_pub_fns(&entry_pkg.sources.entry.source);
    let kernel_holder_structs = scan_kernel_holder_structs(&entry_pkg.sources.entry.source);

    let user_configure_fn = user_init_pkg.and_then(|pkg| {
        scan_pub_fns(&pkg.entry.source)
            .into_iter()
            .find(|f| f.name == "configure")
    });

    let mut out = String::from("-- Synthesized single-VM entry module (`lava.org/lava_boot`).\n");
    if entry_alias != "runtime" {
        out.push_str("use std.runtime\n");
    }
    out.push_str(&format!("use @{entry_alias}\n"));
    if has_lava_sys && entry_alias != "lava_sys" {
        out.push_str("use @lava_sys.host_request_quit\n");
    }
    if has_lava_core && entry_alias != "lava_core" {
        out.push_str("use @lava_core.SystemKernel\n");
    }
    for (ext_pkg, _) in auto_user_exts {
        out.push_str(&format!("use @{}\n", ext_pkg.info.name));
    }
    if user_init_pkg.is_some() {
        out.push_str("use @user_init\n");
    }
    out.push_str("\nprivate fn __lava_run_boot() -> !String (bool, i64) {\n");
    out.push_str("    var startup_files = List()\n");
    for file in &options.startup_files {
        out.push_str(&format!(
            "    startup_files.push(\"{}\")\n",
            escape_nessa_string(file)
        ));
    }
    out.push_str(&format!(
        "    var headless_keys = \"{}\"\n",
        escape_nessa_string(&options.headless_keys)
    ));
    out.push_str(&format!("    var headless_mode = {}\n", options.headless));
    if entry_alias != "runtime" {
        out.push_str("    let __boot_rt = runtime.host_call(\"sys_boot_config\", [])\n");
        out.push_str("    __boot_rt match {\n");
        out.push_str("        rt_any! => {\n");
        out.push_str("            let rt_list = rt_any.as(List)\n");
        out.push_str("            if rt_list.len() >= 4 and rt_list.get(0).as(bool) {\n");
        out.push_str("                startup_files = rt_list.get(1).as(List)\n");
        out.push_str("                headless_keys = rt_list.get(2).as(String)\n");
        out.push_str("                headless_mode = rt_list.get(3).as(bool)\n");
        out.push_str("            }\n");
        out.push_str("        },\n");
        out.push_str("        error _ => (),\n");
        out.push_str("    }\n");
    }
    out.push_str("    let _ = startup_files\n");
    out.push_str("    let _ = headless_keys\n");
    out.push_str("    let _ = headless_mode\n");

    let quit_call = if has_lava_sys {
        if entry_alias == "lava_sys" {
            "lava_sys.host_request_quit"
        } else {
            "host_request_quit"
        }
    } else {
        ""
    };

    let boot_plan = select_boot_plan(&entry_fns, &kernel_holder_structs, has_lava_core)?;

    match &boot_plan.init_step {
        BootInitStep::KernelConstructor(ctor) => {
            let call_expr = format_fn_call(entry_alias, ctor, None);
            emit_propagating_binding(&mut out, "kernel", &call_expr, ctor.ret_is_error_union);
        }
        BootInitStep::AppStructConstructor { ctor, .. } => {
            let call_expr = format_fn_call(entry_alias, ctor, None);
            emit_propagating_binding(&mut out, "app", &call_expr, ctor.ret_is_error_union);
            out.push_str("    let kernel = app.kernel\n");
        }
        BootInitStep::NewKernelWithRegister(opt_reg) => {
            let kernel_ctor = if entry_alias == "lava_core" {
                "lava_core.SystemKernel.new()"
            } else {
                "SystemKernel.new()"
            };
            out.push_str(&format!("    let kernel = {kernel_ctor}\n"));
            if let Some(reg_fn) = opt_reg {
                let call_expr = format!("{entry_alias}.{}(kernel)", reg_fn.name);
                emit_propagating_void(&mut out, &call_expr, reg_fn.ret_is_error_union);
            }
        }
        BootInitStep::DirectWithoutKernel => {}
    }

    let has_kernel_var = !matches!(boot_plan.init_step, BootInitStep::DirectWithoutKernel);
    if has_kernel_var {
        for (ext_pkg, reg_fn) in auto_user_exts {
            let call_expr = format!("{}.{}(kernel)", ext_pkg.info.name, reg_fn.name);
            emit_propagating_void(&mut out, &call_expr, reg_fn.ret_is_error_union);
        }

        if let Some(conf_fn) = &user_configure_fn {
            let call_expr = "user_init.configure(kernel)";
            emit_propagating_void(&mut out, call_expr, conf_fn.ret_is_error_union);
        }

        if has_lava_core
            && (!auto_user_exts.is_empty()
                || user_configure_fn.is_some()
                || matches!(boot_plan.init_step, BootInitStep::NewKernelWithRegister(_)))
        {
            emit_propagating_void(&mut out, "kernel.activate_all()", true);
        }
    }

    if let Some(run_fn) = &boot_plan.run_step {
        let first_handle = match &boot_plan.init_step {
            BootInitStep::AppStructConstructor { struct_name, .. }
                if run_fn.params.first().is_some_and(|p| p.ty == *struct_name) =>
            {
                Some("app")
            }
            _ if has_kernel_var => Some("kernel"),
            _ => None,
        };
        let call_expr = format_fn_call(entry_alias, run_fn, first_handle);
        if run_fn.ret_payload_ty == "i64" {
            emit_propagating_binding(&mut out, "exit_code", &call_expr, run_fn.ret_is_error_union);
            out.push_str("    (true, exit_code)\n");
        } else {
            emit_propagating_void(&mut out, &call_expr, run_fn.ret_is_error_union);
            out.push_str("    (false, 0)\n");
        }
    } else {
        out.push_str("    (false, 0)\n");
    }
    out.push_str("}\n\n");

    out.push_str("fn main() {\n");
    out.push_str("    __lava_run_boot() match {\n");
    out.push_str("        error err => {\n");
    out.push_str("            eprintln(\"Lava error: {err}\")\n");
    if !quit_call.is_empty() {
        out.push_str(&format!("            {quit_call}(1)\n"));
    }
    out.push_str("        },\n");
    out.push_str("        outcome! => {\n");
    if !quit_call.is_empty() {
        out.push_str("            let (has_code, code) = outcome\n");
        out.push_str("            if has_code {\n");
        out.push_str(&format!("                {quit_call}(code)\n"));
        out.push_str("            }\n");
    } else {
        out.push_str("            let _ = outcome\n");
    }
    out.push_str("        },\n");
    out.push_str("    }\n");
    out.push_str("}\n");
    Ok(out)
}

enum BootInitStep {
    KernelConstructor(NessaPubFn),
    AppStructConstructor {
        struct_name: String,
        ctor: NessaPubFn,
    },
    NewKernelWithRegister(Option<NessaPubFn>),
    DirectWithoutKernel,
}

struct BootPlan {
    init_step: BootInitStep,
    run_step: Option<NessaPubFn>,
}

fn select_boot_plan(
    entry_fns: &[NessaPubFn],
    kernel_structs: &BTreeSet<String>,
    has_lava_core: bool,
) -> Result<BootPlan, String> {
    // 1. Prefer a kernel constructor (`() -> [!] SystemKernel`) + a session runner taking `SystemKernel`.
    let kernel_ctor = entry_fns
        .iter()
        .filter(|f| f.ret_payload_ty == "SystemKernel" && can_bind_boot_args(f, None))
        .max_by_key(|f| kernel_ctor_score(f))
        .cloned();

    if let Some(ctor) = kernel_ctor {
        let runner = entry_fns
            .iter()
            .filter(|f| f.name != ctor.name && takes_first_type(f, "SystemKernel"))
            .filter(|f| can_bind_boot_args(f, Some("SystemKernel")))
            .max_by_key(|f| runner_score(f, "SystemKernel"))
            .cloned();
        return Ok(BootPlan {
            init_step: BootInitStep::KernelConstructor(ctor),
            run_step: runner,
        });
    }

    // 2. Check for an app struct holding `pub kernel: SystemKernel` + runner taking that struct or `SystemKernel`.
    for struct_name in kernel_structs {
        if let Some(ctor) = entry_fns
            .iter()
            .filter(|f| f.ret_payload_ty == *struct_name && can_bind_boot_args(f, None))
            .max_by_key(|f| kernel_ctor_score(f))
            .cloned()
        {
            let runner = entry_fns
                .iter()
                .filter(|f| {
                    f.name != ctor.name
                        && (takes_first_type(f, struct_name) || takes_first_type(f, "SystemKernel"))
                })
                .filter(|f| {
                    let first_ty = &f.params[0].ty;
                    can_bind_boot_args(f, Some(first_ty))
                })
                .max_by_key(|f| runner_score(f, struct_name))
                .cloned();
            return Ok(BootPlan {
                init_step: BootInitStep::AppStructConstructor {
                    struct_name: struct_name.clone(),
                    ctor,
                },
                run_step: runner,
            });
        }
    }

    // 3. `SystemKernel.new()` + optional `register*(kernel: SystemKernel)` + runner taking `SystemKernel`.
    if has_lava_core {
        let reg_fn = entry_fns
            .iter()
            .filter(|f| {
                let req = f.required_positional();
                req.len() == 1
                    && is_kernel_type(&req[0].ty)
                    && f.ret_payload_ty == "Unit"
                    && is_registration_name(&f.name)
            })
            .max_by_key(|f| registration_score(&f.name))
            .cloned();

        let reg_name = reg_fn.as_ref().map(|f| f.name.as_str());
        let runner = entry_fns
            .iter()
            .filter(|f| Some(f.name.as_str()) != reg_name && takes_first_type(f, "SystemKernel"))
            .filter(|f| can_bind_boot_args(f, Some("SystemKernel")))
            .max_by_key(|f| runner_score(f, "SystemKernel"))
            .cloned();

        if reg_fn.is_some() || runner.is_some() {
            return Ok(BootPlan {
                init_step: BootInitStep::NewKernelWithRegister(reg_fn),
                run_step: runner,
            });
        }
    }

    // 4. Fallback: standalone entry function taking `(startup_files, headless_keys)` or `()`.
    if let Some(standalone) = entry_fns
        .iter()
        .filter(|f| can_bind_boot_args(f, None))
        .max_by_key(|f| runner_score(f, ""))
        .cloned()
    {
        return Ok(BootPlan {
            init_step: BootInitStep::DirectWithoutKernel,
            run_step: Some(standalone),
        });
    }

    Err(
        "entry package exposes no compatible boot/run public function (`pub fn` taking `SystemKernel`, `(List, String)`, or `()`)"
            .to_string(),
    )
}

fn is_kernel_type(ty: &str) -> bool {
    ty == "SystemKernel" || ty.ends_with(".SystemKernel")
}

fn takes_first_type(f: &NessaPubFn, expected_ty: &str) -> bool {
    let Some(first) = f.params.first() else {
        return false;
    };
    if first.is_named {
        return false;
    }
    if expected_ty == "SystemKernel" {
        is_kernel_type(&first.ty)
    } else {
        first.ty == expected_ty
    }
}

fn can_bind_boot_args(f: &NessaPubFn, first_handle_ty: Option<&str>) -> bool {
    let mut idx = 0usize;
    if let Some(expected_first) = first_handle_ty {
        if !takes_first_type(f, expected_first) {
            return false;
        }
        idx = 1;
    }
    for param in &f.params[idx..] {
        if param.is_named || param.has_default {
            continue;
        }
        let is_headless_bool = param.ty == "bool" && param.name.contains("headless");
        if param.ty != "List" && param.ty != "String" && !is_headless_bool {
            return false;
        }
    }
    true
}

fn format_fn_call(pkg_alias: &str, f: &NessaPubFn, first_var: Option<&str>) -> String {
    let mut args: Vec<String> = Vec::new();
    let mut idx = 0usize;
    if let Some(handle_var) = first_var
        && !f.params.is_empty()
        && !f.params[0].is_named
    {
        args.push(handle_var.to_string());
        idx = 1;
    }

    let mut used_files = false;
    let mut used_keys = false;
    for param in &f.params[idx..] {
        if !param.is_named {
            if param.ty == "List" && !used_files {
                args.push("startup_files".to_string());
                used_files = true;
            } else if param.ty == "String" && !used_keys {
                args.push("headless_keys".to_string());
                used_keys = true;
            } else if param.ty == "bool" && param.name.contains("headless") {
                args.push("headless_mode".to_string());
            } else if !param.has_default && param.ty == "List" {
                args.push("List()".to_string());
            } else if !param.has_default && param.ty == "String" {
                args.push("\"\"".to_string());
            }
        } else if param.ty == "List"
            && (param.name.contains("file") || param.name == "paths" || param.name == "args")
        {
            args.push(format!("{} = startup_files", param.name));
        } else if param.ty == "String"
            && (param.name.contains("key") || param.name.contains("input"))
        {
            args.push(format!("{} = headless_keys", param.name));
        } else if param.ty == "bool" && param.name.contains("headless") {
            args.push(format!("{} = headless_mode", param.name));
        }
    }

    format!("{pkg_alias}.{}({})", f.name, args.join(", "))
}

fn emit_propagating_binding(out: &mut String, var_name: &str, call_expr: &str, is_fallible: bool) {
    if is_fallible {
        out.push_str(&format!("    let {var_name} = {call_expr}!\n"));
    } else {
        out.push_str(&format!("    let {var_name} = {call_expr}\n"));
    }
}

fn emit_propagating_void(out: &mut String, call_expr: &str, is_fallible: bool) {
    if is_fallible {
        out.push_str(&format!("    let _ = {call_expr}!\n"));
    } else {
        out.push_str(&format!("    let _ = {call_expr}\n"));
    }
}

fn kernel_ctor_score(f: &NessaPubFn) -> i32 {
    let mut score = 0;
    if f.name.contains("boot") {
        score += 50;
    }
    if f.name.contains("default") {
        score += 30;
    }
    if f.name.contains("create") || f.name.contains("build") || f.name.contains("new") {
        score += 20;
    }
    if f.params.is_empty() {
        score += 10;
    }
    score
}

fn is_registration_name(name: &str) -> bool {
    name.starts_with("register")
        || name.starts_with("setup")
        || name.starts_with("init")
        || name.starts_with("install")
        || name.starts_with("boot_extensions")
}

fn registration_score(name: &str) -> i32 {
    if name == "register_default_extensions" || name == "register_all_extensions" {
        100
    } else if name == "register_extension" || name.ends_with("_extensions") {
        80
    } else if name.starts_with("register_") {
        60
    } else {
        30
    }
}

fn runner_score(f: &NessaPubFn, preferred_first_ty: &str) -> i32 {
    let mut score = 0;
    if !preferred_first_ty.is_empty() && takes_first_type(f, preferred_first_ty) {
        score += 100;
    }
    if f.name == "run_session" || f.name == "run_app" || f.name == "run" {
        score += 80;
    } else if f.name.starts_with("run") || f.name.starts_with("start") {
        score += 60;
    } else if f.name.contains("session") || f.name.contains("main") {
        score += 40;
    }
    if f.params.iter().any(|p| p.ty == "List") {
        score += 25;
    }
    if f.params.iter().any(|p| p.ty == "String") {
        score += 25;
    }
    score
}

fn find_extension_register_fn(sources: &PackageSources) -> Option<NessaPubFn> {
    let pkg_name = &sources.manifest.manifest().name;
    let short_stem = pkg_name.strip_prefix("ext_").unwrap_or(pkg_name);
    let expected_a = format!("register_{short_stem}_extension");
    let expected_b = format!("register_{pkg_name}");

    scan_pub_fns(&sources.entry.source)
        .into_iter()
        .filter(|f| {
            let req = f.required_positional();
            req.len() == 1 && is_kernel_type(&req[0].ty) && f.ret_payload_ty == "Unit"
        })
        .find(|f| f.name == "register_extension" || f.name == expected_a || f.name == expected_b)
}

fn strip_nessa_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut in_str = false;

    while i < chars.len() {
        let ch = chars[i];
        if in_str {
            out.push(ch);
            if ch == '\\' && i + 1 < chars.len() {
                i += 1;
                out.push(chars[i]);
            } else if ch == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if ch == '"' {
            in_str = true;
            out.push(ch);
            i += 1;
            continue;
        }
        if ch == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        out.push(ch);
        i += 1;
    }
    out
}

fn scan_kernel_holder_structs(source: &str) -> BTreeSet<String> {
    let clean = strip_nessa_comments(source);
    let mut holders = BTreeSet::new();
    let bytes = clean.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() {
        let line_end = clean[idx..]
            .find('\n')
            .map(|off| idx + off)
            .unwrap_or(bytes.len());
        let trimmed = clean[idx..line_end].trim_start();
        let is_top_level = clean[idx..line_end].starts_with("pub struct ");
        if is_top_level && let Some(rest) = trimmed.strip_prefix("pub struct ") {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty()
                && let Some(brace_rel) = clean[idx..].find('{')
            {
                let body_start = idx + brace_rel + 1;
                if let Some(body_end) = find_matching_delim(&clean, body_start, '{', '}') {
                    let body = &clean[body_start..body_end];
                    if body.lines().any(|l| {
                        let lt = l.trim();
                        lt.starts_with("pub kernel:")
                            && lt["pub kernel:".len()..].trim().starts_with("SystemKernel")
                    }) {
                        holders.insert(name);
                    }
                    idx = body_end + 1;
                    continue;
                }
            }
        }
        idx = line_end.saturating_add(1);
    }
    holders
}

fn scan_pub_fns(source: &str) -> Vec<NessaPubFn> {
    let clean = strip_nessa_comments(source);
    let mut fns = Vec::new();
    let mut idx = 0usize;

    while idx < clean.len() {
        let line_end = clean[idx..]
            .find('\n')
            .map(|off| idx + off)
            .unwrap_or(clean.len());
        let line = &clean[idx..line_end];
        // Top-level `pub fn` starts at column 0 (not inside `impl` blocks).
        if let Some(after_kw) = line.strip_prefix("pub fn ") {
            let fn_name: String = after_kw
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !fn_name.is_empty() {
                let search_from = idx + "pub fn ".len();
                if let Some(open_rel) = clean[search_from..].find('(') {
                    let open_pos = search_from + open_rel;
                    if let Some(close_pos) = find_matching_delim(&clean, open_pos + 1, '(', ')') {
                        let raw_params = &clean[open_pos + 1..close_pos];
                        let params = parse_nessa_params(raw_params);
                        let (ret_is_error_union, ret_payload_ty) =
                            parse_nessa_return_type(&clean[close_pos + 1..]);
                        fns.push(NessaPubFn {
                            name: fn_name,
                            params,
                            ret_is_error_union,
                            ret_payload_ty,
                        });
                        idx = close_pos + 1;
                        continue;
                    }
                }
            }
        }
        idx = line_end.saturating_add(1);
    }
    fns
}

fn find_matching_delim(s: &str, start_after_open: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 1usize;
    let mut in_str = false;
    let mut prev_backslash = false;
    for (rel, ch) in s[start_after_open..].char_indices() {
        if in_str {
            if prev_backslash {
                prev_backslash = false;
            } else if ch == '\\' {
                prev_backslash = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
        } else if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(start_after_open + rel);
            }
        }
    }
    None
}

fn parse_nessa_params(raw: &str) -> Vec<NessaParam> {
    let mut parts = Vec::new();
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut in_str = false;
    let mut start = 0usize;

    for (idx, ch) in raw.char_indices() {
        if in_str {
            if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '(' => depth_paren += 1,
            ')' => depth_paren = depth_paren.saturating_sub(1),
            '[' => depth_bracket += 1,
            ']' => depth_bracket = depth_bracket.saturating_sub(1),
            ',' if depth_paren == 0 && depth_bracket == 0 => {
                parts.push(&raw[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&raw[start..]);

    let mut params = Vec::new();
    for part in parts {
        let trimmed = part.trim();
        if trimmed.is_empty() || trimmed == "self" {
            continue;
        }
        let (is_named, rest) = if let Some(stripped) = trimmed.strip_prefix('.') {
            (true, stripped.trim_start())
        } else {
            (false, trimmed)
        };
        let Some((name_raw, after_colon)) = rest.split_once(':') else {
            continue;
        };
        let name = name_raw.trim().to_string();
        let (ty_part, has_default) = match after_colon.split_once('=') {
            Some((ty_s, _default_s)) => (ty_s.trim(), true),
            None => (after_colon.trim(), false),
        };
        params.push(NessaParam {
            name,
            is_named,
            ty: ty_part.to_string(),
            has_default,
        });
    }
    params
}

fn parse_nessa_return_type(after_params: &str) -> (bool, String) {
    // Scan up to `{` or `=` at top level (not `=>` inside `fn(...) -> ...`).
    let mut end_idx = after_params.len();
    let bytes = after_params.as_bytes();
    let mut depth_paren = 0usize;
    let mut depth_bracket = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth_paren += 1,
            b')' => depth_paren = depth_paren.saturating_sub(1),
            b'[' => depth_bracket += 1,
            b']' => depth_bracket = depth_bracket.saturating_sub(1),
            b'{' if depth_paren == 0 && depth_bracket == 0 => {
                end_idx = i;
                break;
            }
            b'=' if depth_paren == 0
                && depth_bracket == 0
                && bytes.get(i + 1).copied() != Some(b'>') =>
            {
                end_idx = i;
                break;
            }
            b'\n'
                if depth_paren == 0 && depth_bracket == 0 && !after_params[..i].contains("->") =>
            {
                end_idx = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }

    let header = after_params[..end_idx].trim();
    let Some(arrow_pos) = header.find("->") else {
        return (false, "Unit".to_string());
    };
    let mut rem = header[arrow_pos + 2..].trim();

    // Strip optional effect annotation `#Effect` or `#[e1, e2]`.
    if let Some(after_hash) = rem.strip_prefix('#') {
        let after_hash = after_hash.trim_start();
        if after_hash.starts_with('[') {
            if let Some(close_b) = find_matching_delim(after_hash, 1, '[', ']') {
                rem = after_hash[close_b + 1..].trim_start();
            }
        } else {
            let tok_len = after_hash
                .find(char::is_whitespace)
                .unwrap_or(after_hash.len());
            rem = after_hash[tok_len..].trim_start();
        }
    }

    // Check optional error-union `!ErrorType PayloadType`.
    if let Some(after_bang) = rem.strip_prefix('!') {
        let after_bang = after_bang.trim_start();
        let payload = if after_bang.starts_with('(') {
            if let Some(close_p) = find_matching_delim(after_bang, 1, '(', ')') {
                after_bang[close_p + 1..].trim()
            } else {
                "Any"
            }
        } else {
            let err_len = after_bang
                .find(char::is_whitespace)
                .unwrap_or(after_bang.len());
            after_bang[err_len..].trim()
        };
        let normalized = if payload.is_empty() { "Unit" } else { payload };
        return (true, normalized.to_string());
    }

    let normalized = if rem.is_empty() { "Unit" } else { rem };
    (false, normalized.to_string())
}

fn escape_nessa_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            other => out.push(other),
        }
    }
    out
}

fn escape_toml_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use driver::CompileResult;
    use interpreter::VmResult;
    use lava_proto::{EventQueue, FrameSlot};
    use nsbc_io::{read_artifact, write_artifact};

    use super::*;

    fn test_host_config() -> HostConfig {
        HostConfig::new(Arc::new(FrameSlot::new()), Arc::new(EventQueue::new()))
    }

    fn compile_result_to_artifact(compiled: CompileResult) -> Result<CompiledArtifact, String> {
        if compiled.has_errors {
            return Err(format_modular_diagnostics(&compiled.diagnostics));
        }
        compiled
            .into_artifact()
            .map_err(|err| format!("failed to finalize Nessa artifact: {err}"))
    }

    #[test]
    fn lava_accel_doc_store_host_and_fallback_e2e() {
        let lava_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("lava workspace root")
            .to_path_buf();
        let accel_pkg_dir = lava_root.join("packages/lava_accel");
        let accel_sources = load_package_sources(&accel_pkg_dir).expect("load packages/lava_accel");

        let tmp_dir = std::env::temp_dir().join(format!(
            "lava_vm_accel_test_{}_{}",
            std::process::id(),
            CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&tmp_dir).expect("create temp dir");
        let sample_file = tmp_dir.join("sample.ns");
        let saved_file = tmp_dir.join("saved.ns");
        fs::write(&sample_file, "fn alpha() {\n    let x = 1\n}\n").expect("write sample.ns");

        let manifest_toml = "\
[package]
name = \"accel_smoke\"
domain = \"lava.org\"
version = \"0.1.0\"
type = \"exe\"

[dependencies]
\"lava.org/lava_accel\" = \"0.1.0\"
";
        let manifest = ManifestDocument::parse(manifest_toml).expect("parse manifest");

        let main_source = format!(
            r#"use @lava_accel.{{
    NativeConflictHit,
    NativeDiagHit,
    SymbolOutlineSpan,
    TodoItem,
    accel_doc_analyze_diagnostics,
    accel_doc_clone_from_lines,
    accel_doc_collect_words,
    accel_doc_delete_lines,
    accel_doc_delete_span,
    accel_doc_diff_head,
    accel_doc_extract_span,
    accel_doc_find_backward,
    accel_doc_find_forward,
    accel_doc_find_matching_bracket,
    accel_doc_find_word_occurrences,
    accel_doc_free,
    accel_doc_insert_line,
    accel_doc_insert_lines,
    accel_doc_insert_text,
    accel_doc_line_at,
    accel_doc_line_char_len,
    accel_doc_line_count,
    accel_doc_new,
    accel_doc_open_file,
    accel_doc_remove_line,
    accel_doc_save_file,
    accel_doc_scan_conflicts,
    accel_doc_scan_symbol_spans,
    accel_doc_scan_todos,
    accel_doc_set_line,
    accel_doc_slice_lines,
    accel_doc_snapshot_matches,
    accel_doc_snapshot_restore,
    accel_doc_snapshot_take,
    accel_doc_to_text_capped,
    accel_doc_version,
    accel_lsp_did_change_doc,
    accel_lsp_did_open_doc,
    accel_lsp_did_save_doc,
}}

fn assert_eq_i64(a: i64, b: i64, label: String) {{
    if a != b {{
        panic("mismatch " ++ label ++ ": " ++ a.to_string() ++ " != " ++ b.to_string())
    }}
}}

fn assert_eq_str(a: String, b: String, label: String) {{
    if a != b {{
        panic("mismatch " ++ label ++ ": '" ++ a ++ "' != '" ++ b ++ "'")
    }}
}}

fn main() {{
    let (opened_id, opened_lines, opened_crlf)! = accel_doc_open_file("{sample}") else {{
        panic("failed to open sample file")
    }}
    assert_eq_i64(opened_lines, 3, "opened_lines")
    if opened_crlf {{
        panic("expected LF")
    }}
    assert_eq_str(accel_doc_line_at(opened_id, 1), "    let x = 1", "opened line 1")
    let written! = accel_doc_save_file(opened_id, "{saved}", false) else {{
        panic("failed to save file")
    }}
    if written <= 0 {{
        panic("expected positive bytes written")
    }}
    accel_doc_free(opened_id)

    let doc = accel_doc_new([
        "pub fn greet(name: String) {{{{",
        "    let TODOS = 1 -- TODO: implement café",
        "    return (1 + [2])",
        "}}}}",
    ])
    assert_eq_i64(accel_doc_line_count(doc), 4, "init count")
    assert_eq_i64(accel_doc_version(doc), 1, "init version")

    let snap1 = accel_doc_snapshot_take(doc)
    if not accel_doc_snapshot_matches(doc, snap1) {{
        panic("snap1 must match immediately")
    }}

    let (nr, nc) = accel_doc_insert_text(doc, 2, 4, "let y = 10\n    ")
    assert_eq_i64(nr, 3, "insert_text row")
    assert_eq_i64(nc, 4, "insert_text col")
    assert_eq_i64(accel_doc_line_count(doc), 5, "after insert_text count")
    if accel_doc_snapshot_matches(doc, snap1) {{
        panic("snap1 must not match mutated doc")
    }}

    let extracted = accel_doc_extract_span(doc, 2, 4, 3, 4)
    assert_eq_str(extracted, "let y = 10\n    ", "extract_span")
    let deleted = accel_doc_delete_span(doc, 2, 4, 3, 4)
    assert_eq_str(deleted, "let y = 10\n    ", "delete_span")
    if not accel_doc_snapshot_matches(doc, snap1) {{
        panic("snap1 must match after inverse delete_span")
    }}

    accel_doc_set_line(doc, 2, "    return (1 + [2}}}}")
    let v_res = accel_doc_snapshot_restore(doc, snap1)
    if v_res <= 1 {{
        panic("restore should increment version")
    }}

    let fwd = accel_doc_find_forward(doc, "TODO", 0, 0, 0, 3, false, false)
    if fwd == null {{
        panic("expected forward TODO match")
    }}
    let (fr, fc) = fwd.as((i64, i64))
    assert_eq_i64(fr, 1, "fwd row")
    assert_eq_i64(fc, 8, "fwd col")

    let bwd = accel_doc_find_backward(doc, "pub", 3, 0, 0, 3, false, false)
    if bwd == null {{
        panic("expected backward pub match")
    }}
    let (br, bc) = bwd.as((i64, i64))
    assert_eq_i64(br, 0, "bwd row")
    assert_eq_i64(bc, 0, "bwd col")

    let mpos = accel_doc_find_matching_bracket(doc, 0, 27, 0, 3, 50)
    if mpos == null {{
        panic("expected matching brace")
    }}
    let (mr, mc) = mpos.as((i64, i64))
    assert_eq_i64(mr, 3, "brace match row")
    assert_eq_i64(mc, 0, "brace match col")

    let todos = accel_doc_scan_todos(doc, "greet.ns", 50)
    assert_eq_i64(todos.len(), 1, "todos len")
    let td = todos.get(0).as(TodoItem)
    assert_eq_str(td.tag, "TODO", "todo tag")
    assert_eq_i64(td.col, 21, "todo col after TODOS")

    let spans = accel_doc_scan_symbol_spans(doc, "nessa", 50)
    assert_eq_i64(spans.len(), 1, "symbol spans len")
    let sp = spans.get(0).as(SymbolOutlineSpan)
    assert_eq_str(sp.name, "greet", "symbol name")
    assert_eq_i64(sp.row, 0, "symbol start")
    assert_eq_i64(sp.end_row, 3, "symbol end")

    let occs = accel_doc_find_word_occurrences(doc, "TODOS", 0, 3, 50)
    assert_eq_i64(occs.len(), 1, "word occs")
    let words = accel_doc_collect_words(doc, 1, 10, 1, 8, 50)
    if words.contains("TODOS") {{
        panic("expected TODOS at (1, 8) to be skipped")
    }}
    if not words.contains("greet") {{
        panic("expected greet in collected words")
    }}

    accel_doc_insert_lines(doc, 4, [
        "<<<<<<< HEAD",
        "let a = 1",
        "=======",
        "let a = 2",
        ">>>>>>> feat",
        " \tlet bad = \"unterminated",
    ])
    let conflicts = accel_doc_scan_conflicts(doc, 50)
    assert_eq_i64(conflicts.len(), 1, "conflicts len")
    let cf = conflicts.get(0).as(NativeConflictHit)
    assert_eq_str(cf.ours_label, "HEAD", "ours_label")
    assert_eq_str(cf.theirs_label, "feat", "theirs_label")

    let (diags, errs, warns) = accel_doc_analyze_diagnostics(doc, "nessa", 50)
    if errs < 4 or warns < 1 or diags.is_empty() {{
        panic("expected conflict + syntax errors + mixed-indent warn")
    }}
    let d0 = diags.get(0).as(NativeDiagHit)
    if d0.severity == "" {{
        panic("expected non-empty diag severity")
    }}

    let _ = accel_doc_diff_head(doc, "{sample}", false)
    let _ = accel_lsp_did_open_doc(0, doc, "{sample}", "nessa", 1, false)
    let _ = accel_lsp_did_change_doc(0, doc, "{sample}", 2, false)
    let _ = accel_lsp_did_save_doc(0, doc, "{sample}", false)

    accel_doc_insert_line(doc, 0, "// top")
    let removed = accel_doc_remove_line(doc, 0)
    assert_eq_str(removed, "// top", "remove_line")
    assert_eq_i64(accel_doc_line_char_len(doc, 1), 41, "char_len with café")
    let sl = accel_doc_slice_lines(doc, 0, 2)
    assert_eq_i64(sl.len(), 2, "slice_lines len")
    let _ = accel_doc_delete_lines(doc, 4, 9)
    let txt = accel_doc_to_text_capped(doc, false)
    if not txt.contains("café") {{
        panic("to_text_capped missing café")
    }}
    accel_doc_clone_from_lines(doc, ["one", "two"])
    assert_eq_i64(accel_doc_line_count(doc), 2, "clone_from_lines count")
    accel_doc_free(doc)
}}
"#,
            sample = escape_nessa_string(&sample_file.to_string_lossy()),
            saved = escape_nessa_string(&saved_file.to_string_lossy()),
        );

        let mut root_pkg = PackageSources {
            root: tmp_dir.join("accel_smoke"),
            manifest,
            entry: SourceFile {
                file_path: tmp_dir.join("accel_smoke/src/main.ns"),
                source: main_source,
            },
            modules: Vec::new(),
            wasm_assets: Vec::new(),
        };
        attach_host_abi_anchor_module(&mut root_pkg);

        let compiled = Driver::new().compile_package_sources_with_privileged(
            &root_pkg,
            &[accel_sources],
            &BTreeSet::new(),
        );
        let artifact = compile_result_to_artifact(compiled)
            .expect("packages/lava_accel + smoke test should compile cleanly");

        let artifact_bytes = write_artifact(&artifact).expect("serialize artifact");
        let artifact_for_fb = read_artifact(&artifact_bytes).expect("deserialize artifact");

        // 1. Execute WITH LavaHostHandler (verifying Rust NativeDocStore + host.rs bridge).
        let exit_host = run_compiled_artifact(artifact, test_host_config()).expect("host run");
        assert_eq!(exit_host, 0);
        let saved_bytes = fs::read_to_string(&saved_file).expect("read saved_file");
        assert_eq!(saved_bytes, "fn alpha() {\n    let x = 1\n}\n");

        // 2. Execute WITHOUT host handler (verifying standalone pure-Nessa fallbacks in lib.ns).
        fs::remove_file(&saved_file).expect("remove saved_file before fallback run");
        let mut standalone_engine = Engine::with_defaults();
        let entry = install_artifact(standalone_engine.vm_mut(), artifact_for_fb)
            .expect("install standalone artifact")
            .expect("entry fn");
        standalone_engine.vm_mut().spawn_root(entry);
        match standalone_engine.vm_mut().run() {
            VmResult::Finished => {}
            VmResult::Error(err) => panic!("standalone fallback execution failed: {err}"),
        }
        let saved_fb = fs::read_to_string(&saved_file).expect("read fallback saved_file");
        assert_eq!(saved_fb, "fn alpha() {\n    let x = 1\n}\n");

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn warm_boot_stat_manifest_cache_hit_and_invalidation() {
        let tmp_ws = std::env::temp_dir().join(format!(
            "lava_stat_cache_ws_{}_{}",
            std::process::id(),
            CACHE_TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let pkg_dir = tmp_ws.join("packages/mini_app/src");
        let cache_dir = tmp_ws.join("target/lava-nsbc-cache");
        fs::create_dir_all(&pkg_dir).expect("create mini_app/src");
        fs::create_dir_all(&cache_dir).expect("create target/lava-nsbc-cache");

        fs::write(
            tmp_ws.join("packages/mini_app/package.toml"),
            "[package]\nname = \"mini_app\"\ndomain = \"lava.org\"\nversion = \"0.1.0\"\ntype = \"lib\"\n",
        )
        .expect("write package.toml");
        let lib_ns = pkg_dir.join("lib.ns");
        fs::write(&lib_ns, "pub fn launch() -> i64 {\n    return 0\n}\n").expect("write lib.ns");

        let ws = LavaWorkspace::new(&tmp_ws).with_cache_dir(&cache_dir);
        let opts = BootOptions::default().with_entry_pkg("lava.org/mini_app");

        let ptr1 = ws
            .compute_stat_manifest_ptr_path(&opts)
            .expect("stat ptr path");
        assert!(!ptr1.exists());

        let art1 = ws.compile_boot(&opts).expect("cold compile_boot");
        assert!(ptr1.is_file(), "cold compile_boot must write stat-v1-*.ptr");
        assert!(!art1.compiled_packages.is_empty());

        let art2 = ws.compile_boot(&opts).expect("warm compile_boot");
        assert_eq!(art1.artifacts.len(), art2.artifacts.len());
        assert!(art2.compiled_packages.is_empty());

        let rc = run_compiled_artifact(art2, test_host_config()).expect("run mini_app");
        assert_eq!(rc, 0);

        // Mutate lib.ns with a different byte length -> stat key must change and recompile cleanly.
        fs::write(
            &lib_ns,
            "pub fn helper() -> i64 { 7 }\npub fn launch() -> i64 {\n    return helper() - 7\n}\n",
        )
        .expect("modify lib.ns");
        let ptr2 = ws
            .compute_stat_manifest_ptr_path(&opts)
            .expect("updated stat ptr path");
        assert_ne!(
            ptr1, ptr2,
            "stat manifest key must change after source edit"
        );

        let art3 = ws.compile_boot(&opts).expect("recompile after edit");
        assert!(ptr2.is_file());
        let rc3 = run_compiled_artifact(art3, test_host_config()).expect("run updated");
        assert_eq!(rc3, 0);

        let _ = fs::remove_dir_all(&tmp_ws);
    }

    #[test]
    fn xdg_cache_dir_resolution_respects_precedence_and_disable() {
        let explicit = PathBuf::from("/tmp/explicit-ws-cache");
        assert_eq!(
            resolve_nsbc_cache_dir_from_vars(
                Some(&explicit),
                Some("/tmp/env"),
                Some("/tmp/xdg"),
                Some("/tmp/home")
            ),
            Some(explicit)
        );
        assert_eq!(
            resolve_nsbc_cache_dir_from_vars(
                None,
                Some("off"),
                Some("/tmp/xdg"),
                Some("/tmp/home")
            ),
            None
        );
        assert_eq!(
            resolve_nsbc_cache_dir_from_vars(None, Some("0"), Some("/tmp/xdg"), Some("/tmp/home")),
            None
        );
        assert_eq!(
            resolve_nsbc_cache_dir_from_vars(
                None,
                Some("/tmp/custom-lava"),
                Some("/tmp/xdg"),
                Some("/tmp/home")
            ),
            Some(PathBuf::from("/tmp/custom-lava"))
        );
        assert_eq!(
            resolve_nsbc_cache_dir_from_vars(None, None, Some("/tmp/xdg"), Some("/tmp/home")),
            Some(PathBuf::from("/tmp/xdg/lava/nsbc"))
        );
        assert_eq!(
            resolve_nsbc_cache_dir_from_vars(None, None, None, Some("/tmp/home")),
            Some(PathBuf::from("/tmp/home/.cache/lava/nsbc"))
        );
    }
}
