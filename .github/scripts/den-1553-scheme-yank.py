#!/usr/bin/env python3
"""Generate the reviewed DEN-1553 scheme/yank solver correction in-place.

This script is temporary review scaffolding. The workflow uploads the formatted,
fully tested solver file; the connected GitHub app publishes those exact bytes
and removes both scaffolding files after the gate succeeds.
"""

from pathlib import Path

path = Path("src/install_graph/solver.rs")
text = path.read_text()


def replace_once(label: str, old: str, new: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one match, found {count}")
    text = text.replace(old, new, 1)


replace_once(
    "candidate scheme field",
    """struct Candidate {
    version: VersionMetadata,
    dependencies: BTreeMap<String, String>,
    exact_requirement: String,
}""",
    """struct Candidate {
    version: VersionMetadata,
    scheme: VersionScheme,
    dependencies: BTreeMap<String, String>,
    exact_requirement: String,
}""",
)
replace_once(
    "workspace scheme field",
    """struct WorkspaceMember {
    version: String,
    dependencies: BTreeMap<String, String>,
}""",
    """struct WorkspaceMember {
    version: String,
    scheme: VersionScheme,
    dependencies: BTreeMap<String, String>,
}""",
)
replace_once(
    "workspace scheme collection",
    """                        WorkspaceMember {
                            version: member.package.version,
                            dependencies: member.dependencies,
                        },""",
    """                        WorkspaceMember {
                            version: member.package.version,
                            scheme: member.package.version_scheme,
                            dependencies: member.dependencies,
                        },""",
)
replace_once(
    "diagnostic compatibility and yank guidance",
    """        match &self.selected {
            Some(version) => lines.push(format!(
                "{indent}version conflict for {}: selected {version}, but it does not satisfy every active requirement",
                self.key
            )),
            None => lines.push(format!(
                "{indent}version conflict for {}: no single published version satisfies every active requirement",
                self.key
            )),
        }""",
    """        match self.selected.as_deref() {
            Some("all matching versions are yanked") => lines.push(format!(
                "{indent}version conflict for {}: all matching versions are yanked; use an existing lock with `zed install --frozen` to replay a previously selected version",
                self.key
            )),
            Some(version) => lines.push(format!(
                "{indent}version conflict for {}: selected {version}, but it does not satisfy every active requirement",
                self.key
            )),
            None => lines.push(format!(
                "{indent}version conflict for {}: no version satisfies every active requirement",
                self.key
            )),
        }""",
)
replace_once(
    "registry candidate acquisition ordering",
    """        let metadata = self.registry.get_version(org, name, version)?;
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
        };""",
    """        let metadata = self.registry.get_version(org, name, version)?;
        validate_version_identity(&metadata, org, name, version)?;
        let exact_requirement = exact_requirement(scheme, version);
        if metadata.yanked {
            let candidate = Candidate {
                exact_requirement,
                version: metadata,
                scheme,
                dependencies: BTreeMap::new(),
            };
            self.candidates.insert(cache_key, candidate.clone());
            return Ok(candidate);
        }

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
            exact_requirement,
            version: metadata,
            scheme,
            dependencies: fetched.dependencies,
        };""",
)
replace_once(
    "scheme-aware matcher helper",
    """fn exact_requirement(scheme: VersionScheme, raw: &str) -> String {
    match scheme {
        VersionScheme::Semver => format!("={raw}"),
        VersionScheme::Calver => version::normalize_calver(raw)
            .map(|normalized| format!("={normalized}"))
            .unwrap_or_else(|| raw.to_string()),
        VersionScheme::Opaque => raw.to_string(),
    }
}

struct GraphSolver<'a, S> {""",
    """fn exact_requirement(scheme: VersionScheme, raw: &str) -> String {
    match scheme {
        VersionScheme::Semver => format!("={raw}"),
        VersionScheme::Calver => version::normalize_calver(raw)
            .map(|normalized| format!("={normalized}"))
            .unwrap_or_else(|| raw.to_string()),
        VersionScheme::Opaque => raw.to_string(),
    }
}

fn requirement_matches(scheme: VersionScheme, requirement: &str, published: &str) -> bool {
    match scheme {
        VersionScheme::Opaque => requirement == published,
        VersionScheme::Semver | VersionScheme::Calver => {
            Requirement::parse(requirement).matches(published)
        }
    }
}

struct GraphSolver<'a, S> {""",
)
replace_once(
    "workspace initial selection",
    """            if constraints.iter().any(|constraint| {
                !Requirement::parse(&constraint.requirement).matches(&member.version)
            }) {""",
    """            if constraints.iter().any(|constraint| {
                !requirement_matches(
                    member.scheme,
                    &constraint.requirement,
                    &member.version,
                )
            }) {""",
)
replace_once(
    "registry candidate filtering",
    """            if constraints
                .iter()
                .any(|constraint| !Requirement::parse(&constraint.requirement).matches(published))
            {""",
    """            if constraints.iter().any(|constraint| {
                !requirement_matches(
                    package.version_scheme,
                    &constraint.requirement,
                    published,
                )
            }) {""",
)
replace_once(
    "selected registry propagation",
    """                if constraints.iter().any(|constraint| {
                    !Requirement::parse(&constraint.requirement).matches(&candidate.version.version)
                }) {""",
    """                if constraints.iter().any(|constraint| {
                    !requirement_matches(
                        candidate.scheme,
                        &constraint.requirement,
                        &candidate.version.version,
                    )
                }) {""",
)
replace_once(
    "selected workspace propagation",
    """                if constraints.iter().any(|constraint| {
                    !Requirement::parse(&constraint.requirement).matches(&member.version)
                }) {""",
    """                if constraints.iter().any(|constraint| {
                    !requirement_matches(
                        member.scheme,
                        &constraint.requirement,
                        &member.version,
                    )
                }) {""",
)
replace_once(
    "memory candidate default scheme",
    """                Candidate {
                    exact_requirement: format!("={version_text}"),
                    version: metadata,
                    dependencies: dependencies""",
    """                Candidate {
                    exact_requirement: format!("={version_text}"),
                    version: metadata,
                    scheme: VersionScheme::Semver,
                    dependencies: dependencies""",
)

publish_start = text.index(
    "        fn publish(&mut self, key: &str, version_text: &str, dependencies: &[(&str, &str)]) {"
)
impl_end_marker = "\n    }\n\n    impl SolveSource for MemorySource"
impl_end = text.index(impl_end_marker, publish_start)
helper = """

        fn publish_with_scheme(
            &mut self,
            key: &str,
            version_text: &str,
            scheme: VersionScheme,
            yanked: bool,
            dependencies: &[(&str, &str)],
        ) {
            self.publish(key, version_text, dependencies);
            let cache_key = (key.to_string(), version_text.to_string());
            let candidate = self
                .candidates
                .get_mut(&cache_key)
                .expect("published candidate must exist");
            candidate.scheme = scheme;
            candidate.exact_requirement = exact_requirement(scheme, version_text);
            candidate.version.yanked = yanked;
            self.packages
                .get_mut(key)
                .expect("published package must exist")
                .version_scheme = scheme;
        }
"""
text = text[:impl_end] + helper + text[impl_end:]

test_marker = """    #[test]
    fn dependency_cycles_terminate_with_one_selected_version_per_key() {"""
if text.count(test_marker) != 1:
    raise SystemExit("cycle test marker changed")
extra_tests = """    #[test]
    fn opaque_versions_accept_only_exact_requirements() {
        let mut source = MemorySource::default();
        source.publish_with_scheme(
            "test/opaque",
            "legacy-api",
            VersionScheme::Opaque,
            false,
            &[],
        );
        source.publish_with_scheme(
            "test/opaque",
            "release-candidate-1",
            VersionScheme::Opaque,
            false,
            &[],
        );

        let solved = solve_memory(&mut source, &[("test/opaque", "legacy-api")]).unwrap();
        assert_eq!(solved.selected_version("test/opaque"), Some("legacy-api"));

        let error = solve_memory(&mut source, &[("test/opaque", "^1")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("no version"), "{error}");
    }

    #[test]
    fn yanked_only_matches_are_rejected_with_frozen_guidance() {
        let mut source = MemorySource::default();
        source.publish_with_scheme(
            "test/yanked",
            "1.0.0",
            VersionScheme::Semver,
            true,
            &[],
        );

        let error = solve_memory(&mut source, &[("test/yanked", "^1")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("all matching versions are yanked"), "{error}");
        assert!(error.contains("--frozen"), "{error}");
    }

"""
text = text.replace(test_marker, extra_tests + test_marker, 1)
path.write_text(text)
