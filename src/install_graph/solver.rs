use super::artifact::{split_key, validate_version_identity};
use super::*;
use zed_interfaces::registry::PackageMetadata;
use zed_interfaces::version::VersionScheme;

#[derive(Debug, Clone)]
pub(crate) struct PreparedInstall {
    packages: BTreeMap<String, Candidate>,
    pub(crate) report: PrefetchReport,
}

impl Default for PreparedInstall {
    fn default() -> Self {
        Self {
            packages: BTreeMap::new(),
            report: PrefetchReport::default(),
        }
    }
}

impl PreparedInstall {
    pub(crate) fn exact_requirements(&self) -> BTreeMap<String, String> {
        self.packages
            .iter()
            .map(|(key, candidate)| (key.clone(), candidate.exact_requirement.clone()))
            .collect()
    }

    #[cfg(test)]
    fn selected_version(&self, key: &str) -> Option<&str> {
        self.packages
            .get(key)
            .map(|candidate| candidate.version.version.as_str())
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    version: VersionMetadata,
    dependencies: BTreeMap<String, String>,
    exact_requirement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Constraint {
    requirement: String,
    path: Vec<String>,
}

#[derive(Debug, Clone)]
struct WorkspaceMember {
    version: String,
    dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
struct SolverWorkspace {
    members: BTreeMap<String, WorkspaceMember>,
}

impl SolverWorkspace {
    fn discover(project: &Path) -> Self {
        let mut current = Some(project);
        while let Some(directory) = current {
            if directory.join(MANIFEST_FILE).is_file()
                && let Ok(manifest) = read_manifest(directory)
                && let Some(workspace) = manifest.workspace.as_ref()
            {
                return Self::collect(directory, &workspace.members);
            }
            current = directory.parent();
        }
        Self::default()
    }

    fn collect(root: &Path, patterns: &[String]) -> Self {
        let mut workspace = Self::default();
        for pattern in patterns {
            let mut candidates = vec![root.to_path_buf()];
            for segment in pattern.split('/') {
                let mut next = Vec::new();
                for base in &candidates {
                    if segment.contains('*') {
                        let Ok(glob) = Glob::new(segment) else {
                            continue;
                        };
                        let matcher = glob.compile_matcher();
                        if let Ok(entries) = fs::read_dir(base) {
                            for entry in entries.flatten() {
                                let name = entry.file_name();
                                if entry.path().is_dir()
                                    && matcher.is_match(Path::new(&name))
                                    && !name.to_string_lossy().starts_with('.')
                                {
                                    next.push(entry.path());
                                }
                            }
                        }
                    } else {
                        let candidate = base.join(segment);
                        if candidate.is_dir() {
                            next.push(candidate);
                        }
                    }
                }
                candidates = next;
            }
            for member_dir in candidates {
                if let Ok(member) = read_manifest(&member_dir) {
                    workspace.members.insert(
                        member.full_name(),
                        WorkspaceMember {
                            version: member.package.version,
                            dependencies: member.dependencies,
                        },
                    );
                }
            }
        }
        workspace
    }
}

#[derive(Debug, Clone, Default)]
struct SolveState {
    constraints: BTreeMap<String, Vec<Constraint>>,
    registry: BTreeMap<String, Candidate>,
    workspace: BTreeMap<String, WorkspaceMember>,
}

impl SolveState {
    fn add_constraint(&mut self, key: String, constraint: Constraint) -> Result<bool> {
        split_key(&key)?;
        let constraints = self.constraints.entry(key).or_default();
        if constraints.contains(&constraint) {
            return Ok(false);
        }
        constraints.push(constraint);
        constraints.sort();
        Ok(true)
    }

    fn unresolved_key(&self) -> Option<String> {
        self.constraints
            .keys()
            .find(|key| !self.registry.contains_key(*key) && !self.workspace.contains_key(*key))
            .cloned()
    }
}

#[derive(Debug, Clone)]
struct SolveFailure {
    key: String,
    constraints: Vec<Constraint>,
    available: Vec<String>,
    selected: Option<String>,
    attempts: Vec<(String, SolveFailure)>,
}

impl SolveFailure {
    fn selected(key: &str, constraints: &[Constraint], version: &str) -> Self {
        Self {
            key: key.to_string(),
            constraints: constraints.to_vec(),
            available: Vec::new(),
            selected: Some(version.to_string()),
            attempts: Vec::new(),
        }
    }

    fn render(&self) -> String {
        let mut lines = Vec::new();
        self.render_into(0, &mut lines);
        lines.join("\n")
    }

    fn render_into(&self, depth: usize, lines: &mut Vec<String>) {
        let indent = "  ".repeat(depth);
        match &self.selected {
            Some(version) => lines.push(format!(
                "{indent}version conflict for {}: selected {version}, but it does not satisfy every active requirement",
                self.key
            )),
            None => lines.push(format!(
                "{indent}version conflict for {}: no single published version satisfies every active requirement",
                self.key
            )),
        }
        for constraint in self.constraints.iter().take(32) {
            lines.push(format!(
                "{indent}  - `{}` via {}",
                constraint.requirement,
                constraint.path.join(" -> ")
            ));
        }
        if self.constraints.len() > 32 {
            lines.push(format!(
                "{indent}  - ... {} additional requirement paths omitted",
                self.constraints.len() - 32
            ));
        }
        if !self.available.is_empty() {
            lines.push(format!(
                "{indent}  available versions: {}",
                self.available.join(", ")
            ));
        }
        for (version, failure) in self.attempts.iter().take(8) {
            lines.push(format!("{indent}  candidate {version} led to:"));
            failure.render_into(depth + 2, lines);
        }
        if self.attempts.len() > 8 {
            lines.push(format!(
                "{indent}  ... {} additional candidate failures omitted",
                self.attempts.len() - 8
            ));
        }
    }
}

enum SearchOutcome {
    Solved(SolveState),
    Unsatisfiable(SolveFailure),
}

trait SolveSource {
    fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata>;
    fn candidate(
        &mut self,
        key: &str,
        org: &str,
        name: &str,
        version: &str,
        scheme: VersionScheme,
    ) -> Result<Candidate>;
    fn downloaded(&self) -> usize;
}

struct RegistrySource<'a> {
    registry: &'a dyn Registry,
    pool: &'a FetchPool,
    packages: BTreeMap<String, PackageMetadata>,
    candidates: BTreeMap<(String, String), Candidate>,
    next_sequence: usize,
    buffered: BTreeMap<usize, Result<FetchResult>>,
    downloaded: usize,
}

impl<'a> RegistrySource<'a> {
    fn new(registry: &'a dyn Registry, pool: &'a FetchPool) -> Self {
        Self {
            registry,
            pool,
            packages: BTreeMap::new(),
            candidates: BTreeMap::new(),
            next_sequence: 0,
            buffered: BTreeMap::new(),
            downloaded: 0,
        }
    }
}

impl SolveSource for RegistrySource<'_> {
    fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata> {
        let key = format!("{org}/{name}");
        if let Some(package) = self.packages.get(&key) {
            return Ok(package.clone());
        }
        let package = self.registry.get_package(org, name)?;
        if package.org != org || package.name != name {
            bail!(
                "registry returned package `{}/{}` while resolving `{key}`; refusing",
                package.org,
                package.name
            );
        }
        self.packages.insert(key, package.clone());
        Ok(package)
    }

    fn candidate(
        &mut self,
        key: &str,
        org: &str,
        name: &str,
        version: &str,
        scheme: VersionScheme,
    ) -> Result<Candidate> {
        let cache_key = (key.to_string(), version.to_string());
        if let Some(candidate) = self.candidates.get(&cache_key) {
            return Ok(candidate.clone());
        }

        let metadata = self.registry.get_version(org, name, version)?;
        validate_version_identity(&metadata, org, name, version)?;
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        self.pool.submit(FetchTask {
            sequence,
            key: key.to_string(),
            version: metadata.clone(),
        })?;
        let fetched = super::resolver::receive_in_order(self.pool, &mut self.buffered, sequence)?;
        self.downloaded += usize::from(fetched.downloaded);
        let candidate = Candidate {
            exact_requirement: exact_requirement(scheme, version),
            version: metadata,
            dependencies: fetched.dependencies,
        };
        self.candidates.insert(cache_key, candidate.clone());
        Ok(candidate)
    }

    fn downloaded(&self) -> usize {
        self.downloaded
    }
}

fn exact_requirement(scheme: VersionScheme, raw: &str) -> String {
    match scheme {
        VersionScheme::Semver => format!("={raw}"),
        VersionScheme::Calver => version::normalize_calver(raw)
            .map(|normalized| format!("={normalized}"))
            .unwrap_or_else(|| raw.to_string()),
        VersionScheme::Opaque => raw.to_string(),
    }
}

struct GraphSolver<'a, S> {
    source: &'a mut S,
    workspace: &'a SolverWorkspace,
}

impl<S: SolveSource> GraphSolver<'_, S> {
    fn solve(&mut self, mut state: SolveState) -> Result<SearchOutcome> {
        if let Some(failure) = self.propagate(&mut state)? {
            return Ok(SearchOutcome::Unsatisfiable(failure));
        }

        let Some(key) = state.unresolved_key() else {
            return Ok(SearchOutcome::Solved(state));
        };
        let constraints = state.constraints.get(&key).cloned().unwrap_or_default();

        if let Some(member) = self.workspace.members.get(&key) {
            if constraints
                .iter()
                .any(|constraint| !Requirement::parse(&constraint.requirement).matches(&member.version))
            {
                return Ok(SearchOutcome::Unsatisfiable(SolveFailure {
                    key,
                    constraints,
                    available: vec![member.version.clone()],
                    selected: Some(member.version.clone()),
                    attempts: Vec::new(),
                }));
            }
            state.workspace.insert(key, member.clone());
            return self.solve(state);
        }

        let (org, name) = split_key(&key)?;
        let package = self.source.package(&org, &name)?;
        let mut versions = package.versions.clone();
        version::sort_desc(&mut versions);
        let mut attempts = Vec::new();
        let mut matching = false;

        for published in &versions {
            if constraints.iter().any(|constraint| {
                !Requirement::parse(&constraint.requirement).matches(published)
            }) {
                continue;
            }
            matching = true;
            let candidate = self.source.candidate(
                &key,
                &org,
                &name,
                published,
                package.version_scheme,
            )?;
            if candidate.version.yanked {
                continue;
            }
            let mut branch = state.clone();
            branch.registry.insert(key.clone(), candidate);
            match self.solve(branch)? {
                SearchOutcome::Solved(solved) => return Ok(SearchOutcome::Solved(solved)),
                SearchOutcome::Unsatisfiable(failure) => {
                    attempts.push((published.clone(), failure));
                }
            }
        }

        let selected = if matching && attempts.is_empty() {
            Some("all matching versions are yanked".to_string())
        } else {
            None
        };
        Ok(SearchOutcome::Unsatisfiable(SolveFailure {
            key,
            constraints,
            available: versions,
            selected,
            attempts,
        }))
    }

    fn propagate(&self, state: &mut SolveState) -> Result<Option<SolveFailure>> {
        loop {
            for (key, candidate) in &state.registry {
                let constraints = state.constraints.get(key).cloned().unwrap_or_default();
                if constraints.iter().any(|constraint| {
                    !Requirement::parse(&constraint.requirement)
                        .matches(&candidate.version.version)
                }) {
                    return Ok(Some(SolveFailure::selected(
                        key,
                        &constraints,
                        &candidate.version.version,
                    )));
                }
            }
            for (key, member) in &state.workspace {
                let constraints = state.constraints.get(key).cloned().unwrap_or_default();
                if constraints.iter().any(|constraint| {
                    !Requirement::parse(&constraint.requirement).matches(&member.version)
                }) {
                    return Ok(Some(SolveFailure::selected(
                        key,
                        &constraints,
                        &member.version,
                    )));
                }
            }

            let mut additions = Vec::new();
            for (key, candidate) in &state.registry {
                let parents = state.constraints.get(key).cloned().unwrap_or_default();
                for parent in parents {
                    for (dependency, requirement) in &candidate.dependencies {
                        additions.push((
                            dependency.clone(),
                            child_constraint(
                                &parent,
                                key,
                                &candidate.version.version,
                                dependency,
                                requirement,
                            ),
                        ));
                    }
                }
            }
            for (key, member) in &state.workspace {
                let parents = state.constraints.get(key).cloned().unwrap_or_default();
                for parent in parents {
                    for (dependency, requirement) in &member.dependencies {
                        additions.push((
                            dependency.clone(),
                            child_constraint(
                                &parent,
                                key,
                                &member.version,
                                dependency,
                                requirement,
                            ),
                        ));
                    }
                }
            }

            let mut changed = false;
            for (key, constraint) in additions {
                changed |= state.add_constraint(key, constraint)?;
            }
            if !changed {
                return Ok(None);
            }
        }
    }
}

fn child_constraint(
    parent: &Constraint,
    parent_key: &str,
    parent_version: &str,
    dependency: &str,
    requirement: &str,
) -> Constraint {
    let mut path = parent.path.clone();
    if let Some(last) = path.last_mut() {
        *last = format!("{parent_key}@{parent_version}");
    }
    path.push(dependency.to_string());
    Constraint {
        requirement: requirement.to_string(),
        path,
    }
}

pub(super) fn solve_install(
    project: &Path,
    manifest: &Manifest,
    registry: &dyn Registry,
    pool: &FetchPool,
) -> Result<PreparedInstall> {
    if manifest.dependencies.is_empty() {
        return Ok(PreparedInstall::default());
    }

    let root = format!("{}@{}", manifest.full_name(), manifest.package.version);
    let mut state = SolveState::default();
    for (key, requirement) in &manifest.dependencies {
        split_key(key)?;
        state.add_constraint(
            key.clone(),
            Constraint {
                requirement: requirement.clone(),
                path: vec![root.clone(), key.clone()],
            },
        )?;
    }

    let workspace = SolverWorkspace::discover(project);
    let mut source = RegistrySource::new(registry, pool);
    let outcome = GraphSolver {
        source: &mut source,
        workspace: &workspace,
    }
    .solve(state)?;

    match outcome {
        SearchOutcome::Solved(solved) => Ok(PreparedInstall {
            report: PrefetchReport {
                resolved: solved.registry.len(),
                downloaded: source.downloaded(),
            },
            packages: solved.registry,
        }),
        SearchOutcome::Unsatisfiable(failure) => bail!(failure.render()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zed_interfaces::vcs::Vcs;

    #[derive(Default)]
    struct MemorySource {
        packages: BTreeMap<String, PackageMetadata>,
        candidates: BTreeMap<(String, String), Candidate>,
    }

    impl MemorySource {
        fn publish(
            &mut self,
            key: &str,
            version_text: &str,
            dependencies: &[(&str, &str)],
        ) {
            let (org, name) = key.split_once('/').unwrap();
            let metadata = VersionMetadata {
                org: org.to_string(),
                name: name.to_string(),
                version: version_text.to_string(),
                sha256: format!("{:064x}", self.candidates.len() + 1),
                size: 1,
                format: zed_interfaces::registry::ArtifactFormat::TarGz,
                vcs_tag: format!("v{version_text}"),
                vcs_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                download_url: "https://example.invalid/artifact".to_string(),
                published_at: "1970-01-01T00:00:00Z".to_string(),
                yanked: false,
            };
            self.candidates.insert(
                (key.to_string(), version_text.to_string()),
                Candidate {
                    exact_requirement: format!("={version_text}"),
                    version: metadata,
                    dependencies: dependencies
                        .iter()
                        .map(|(key, requirement)| {
                            ((*key).to_string(), (*requirement).to_string())
                        })
                        .collect(),
                },
            );
            let package = self
                .packages
                .entry(key.to_string())
                .or_insert_with(|| PackageMetadata {
                    org: org.to_string(),
                    name: name.to_string(),
                    description: None,
                    vcs: Vcs::Git,
                    repo_url: format!("https://example.invalid/{key}"),
                    version_scheme: VersionScheme::Semver,
                    latest: None,
                    tags: Vec::new(),
                    versions: Vec::new(),
                });
            package.versions.push(version_text.to_string());
            version::sort_desc(&mut package.versions);
            package.latest = package.versions.first().cloned();
        }
    }

    impl SolveSource for MemorySource {
        fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata> {
            self.packages
                .get(&format!("{org}/{name}"))
                .cloned()
                .with_context(|| format!("missing package {org}/{name}"))
        }

        fn candidate(
            &mut self,
            key: &str,
            _org: &str,
            _name: &str,
            version: &str,
            _scheme: VersionScheme,
        ) -> Result<Candidate> {
            self.candidates
                .get(&(key.to_string(), version.to_string()))
                .cloned()
                .with_context(|| format!("missing candidate {key}@{version}"))
        }

        fn downloaded(&self) -> usize {
            0
        }
    }

    fn solve_memory(
        source: &mut MemorySource,
        dependencies: &[(&str, &str)],
    ) -> Result<PreparedInstall> {
        let root = "consumer/app@0.1.0".to_string();
        let mut state = SolveState::default();
        for (key, requirement) in dependencies {
            state.add_constraint(
                (*key).to_string(),
                Constraint {
                    requirement: (*requirement).to_string(),
                    path: vec![root.clone(), (*key).to_string()],
                },
            )?;
        }
        let workspace = SolverWorkspace::default();
        let outcome = GraphSolver {
            source,
            workspace: &workspace,
        }
        .solve(state)?;
        match outcome {
            SearchOutcome::Solved(solved) => Ok(PreparedInstall {
                report: PrefetchReport {
                    resolved: solved.registry.len(),
                    downloaded: 0,
                },
                packages: solved.registry,
            }),
            SearchOutcome::Unsatisfiable(failure) => bail!(failure.render()),
        }
    }

    #[test]
    fn overlapping_ranges_select_the_highest_common_version() {
        let mut source = MemorySource::default();
        source.publish("test/shared", "1.5.0", &[]);
        source.publish("test/shared", "1.9.0", &[]);
        source.publish("test/left", "1.0.0", &[("test/shared", "^1")]);
        source.publish(
            "test/right",
            "1.0.0",
            &[("test/shared", "<=1.5.0")],
        );

        let solved = solve_memory(
            &mut source,
            &[("test/left", "=1.0.0"), ("test/right", "=1.0.0")],
        )
        .unwrap();
        assert_eq!(solved.selected_version("test/shared"), Some("1.5.0"));
    }

    #[test]
    fn rejected_candidate_constraints_do_not_leak_into_the_next_branch() {
        let mut source = MemorySource::default();
        source.publish("test/leaf", "1.0.0", &[]);
        source.publish("test/leaf", "2.0.0", &[]);
        source.publish("test/router", "1.0.0", &[("test/leaf", "^1")]);
        source.publish("test/router", "2.0.0", &[("test/leaf", "^2")]);
        source.publish("test/policy", "1.0.0", &[("test/leaf", "^1")]);

        let solved = solve_memory(
            &mut source,
            &[("test/router", ">=1"), ("test/policy", "=1.0.0")],
        )
        .unwrap();
        assert_eq!(solved.selected_version("test/router"), Some("1.0.0"));
        assert_eq!(solved.selected_version("test/leaf"), Some("1.0.0"));
    }

    #[test]
    fn backtracks_across_more_than_one_coordinate() {
        let mut source = MemorySource::default();
        source.publish("test/core", "1.0.0", &[]);
        source.publish("test/core", "2.0.0", &[]);
        source.publish("test/b", "1.0.0", &[("test/core", "^1")]);
        source.publish("test/b", "2.0.0", &[("test/core", "^2")]);
        source.publish("test/a", "1.0.0", &[("test/b", "^1")]);
        source.publish("test/a", "2.0.0", &[("test/b", "^2")]);
        source.publish("test/policy", "1.0.0", &[("test/core", "^1")]);

        let solved = solve_memory(
            &mut source,
            &[("test/a", ">=1"), ("test/policy", "=1.0.0")],
        )
        .unwrap();
        assert_eq!(solved.selected_version("test/a"), Some("1.0.0"));
        assert_eq!(solved.selected_version("test/b"), Some("1.0.0"));
        assert_eq!(solved.selected_version("test/core"), Some("1.0.0"));
    }

    #[test]
    fn unsatisfiable_diagnostic_lists_every_requirement_path_deterministically() {
        let mut source = MemorySource::default();
        source.publish("test/shared", "1.0.0", &[]);
        source.publish("test/shared", "2.0.0", &[]);
        source.publish("test/left", "1.0.0", &[("test/shared", "^1")]);
        source.publish("test/right", "1.0.0", &[("test/shared", "^2")]);

        let error = solve_memory(
            &mut source,
            &[("test/left", "=1.0.0"), ("test/right", "=1.0.0")],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("`^1` via consumer/app@0.1.0 -> test/left@1.0.0 -> test/shared"));
        assert!(error.contains("`^2` via consumer/app@0.1.0 -> test/right@1.0.0 -> test/shared"));
        let second = solve_memory(
            &mut source,
            &[("test/right", "=1.0.0"), ("test/left", "=1.0.0")],
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, second);
    }

    #[test]
    fn dependency_cycles_terminate_with_one_selected_version_per_key() {
        let mut source = MemorySource::default();
        source.publish("test/a", "1.0.0", &[("test/b", "^1")]);
        source.publish("test/b", "1.0.0", &[("test/a", "^1")]);
        let solved = solve_memory(&mut source, &[("test/a", "^1")]).unwrap();
        assert_eq!(solved.report.resolved, 2);
        assert_eq!(solved.selected_version("test/a"), Some("1.0.0"));
        assert_eq!(solved.selected_version("test/b"), Some("1.0.0"));
    }
}
