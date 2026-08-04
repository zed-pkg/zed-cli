#!/usr/bin/env python3
"""Batch the best candidate across the active solver frontier.

The complete solver must inspect version-dependent manifests before choosing a
final graph, but candidate discovery must still use the established bounded
worker pool. This temporary review generator submits one highest matching
candidate per unresolved coordinate before waiting. It preserves deterministic
search and avoids eagerly downloading alternate versions.
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
    "candidate request type",
    """#[derive(Debug, Clone)]
struct Candidate {
    version: VersionMetadata,
    scheme: VersionScheme,
    dependencies: BTreeMap<String, String>,
    exact_requirement: String,
}
""",
    """#[derive(Debug, Clone)]
struct Candidate {
    version: VersionMetadata,
    scheme: VersionScheme,
    dependencies: BTreeMap<String, String>,
    exact_requirement: String,
}

#[derive(Debug, Clone)]
struct CandidateRequest {
    key: String,
    org: String,
    name: String,
    version: String,
    scheme: VersionScheme,
}
""",
)
replace_once(
    "source prefetch contract",
    """trait SolveSource {
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
""",
    """trait SolveSource {
    fn package(&mut self, org: &str, name: &str) -> Result<PackageMetadata>;
    fn candidate(
        &mut self,
        key: &str,
        org: &str,
        name: &str,
        version: &str,
        scheme: VersionScheme,
    ) -> Result<Candidate>;

    fn prefetch(&mut self, requests: &[CandidateRequest]) -> Result<()> {
        for request in requests {
            self.candidate(
                &request.key,
                &request.org,
                &request.name,
                &request.version,
                request.scheme,
            )?;
        }
        Ok(())
    }

    fn downloaded(&self) -> usize;
}
""",
)
replace_once(
    "registry batch prefetch",
    """    fn candidate(
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
        };
        self.candidates.insert(cache_key, candidate.clone());
        Ok(candidate)
    }
""",
    """    fn candidate(
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

        let request = CandidateRequest {
            key: key.to_string(),
            org: org.to_string(),
            name: name.to_string(),
            version: version.to_string(),
            scheme,
        };
        self.prefetch(std::slice::from_ref(&request))?;
        self.candidates
            .get(&cache_key)
            .cloned()
            .with_context(|| format!("candidate prefetch produced no result for {key}@{version}"))
    }

    fn prefetch(&mut self, requests: &[CandidateRequest]) -> Result<()> {
        let mut pending = Vec::new();
        for request in requests {
            let cache_key = (request.key.clone(), request.version.clone());
            if self.candidates.contains_key(&cache_key) {
                continue;
            }

            let metadata =
                self.registry
                    .get_version(&request.org, &request.name, &request.version)?;
            validate_version_identity(
                &metadata,
                &request.org,
                &request.name,
                &request.version,
            )?;
            let exact_requirement = exact_requirement(request.scheme, &request.version);
            if metadata.yanked {
                self.candidates.insert(
                    cache_key,
                    Candidate {
                        exact_requirement,
                        version: metadata,
                        scheme: request.scheme,
                        dependencies: BTreeMap::new(),
                    },
                );
                continue;
            }

            let sequence = self.next_sequence;
            self.next_sequence += 1;
            self.pool.submit(FetchTask {
                sequence,
                key: request.key.clone(),
                version: metadata.clone(),
            })?;
            pending.push((
                sequence,
                cache_key,
                metadata,
                request.scheme,
                exact_requirement,
            ));
        }

        for (sequence, cache_key, metadata, scheme, exact_requirement) in pending {
            let fetched =
                super::resolver::receive_in_order(self.pool, &mut self.buffered, sequence)?;
            self.downloaded += usize::from(fetched.downloaded);
            self.candidates.insert(
                cache_key,
                Candidate {
                    exact_requirement,
                    version: metadata,
                    scheme,
                    dependencies: fetched.dependencies,
                },
            );
        }
        Ok(())
    }
""",
)
replace_once(
    "frontier prefetch call",
    """        if let Some(failure) = self.propagate(&mut state)? {
            return Ok(SearchOutcome::Unsatisfiable(failure));
        }

        let Some(key) = state.unresolved_key() else {""",
    """        if let Some(failure) = self.propagate(&mut state)? {
            return Ok(SearchOutcome::Unsatisfiable(failure));
        }
        self.prefetch_frontier(&state)?;

        let Some(key) = state.unresolved_key() else {""",
)
replace_once(
    "frontier prefetch method",
    """    fn propagate(&self, state: &mut SolveState) -> Result<Option<SolveFailure>> {""",
    """    fn prefetch_frontier(&mut self, state: &SolveState) -> Result<()> {
        let mut requests = Vec::new();
        for (key, constraints) in &state.constraints {
            if state.registry.contains_key(key)
                || state.workspace.contains_key(key)
                || self.workspace.members.contains_key(key)
            {
                continue;
            }

            let (org, name) = split_key(key)?;
            let package = self.source.package(&org, &name)?;
            let mut versions = package.versions.clone();
            version::sort_desc(&mut versions);
            let Some(published) = versions.into_iter().find(|published| {
                constraints.iter().all(|constraint| {
                    requirement_matches(
                        package.version_scheme,
                        &constraint.requirement,
                        published,
                    )
                })
            }) else {
                continue;
            };
            requests.push(CandidateRequest {
                key: key.clone(),
                org,
                name,
                version: published,
                scheme: package.version_scheme,
            });
        }

        // Submit one best candidate for every currently active coordinate
        // before waiting. This preserves deterministic graph search and avoids
        // downloading alternate versions eagerly while still saturating the
        // bounded worker pool on wide dependency frontiers.
        self.source.prefetch(&requests)
    }

    fn propagate(&self, state: &mut SolveState) -> Result<Option<SolveFailure>> {""",
)

path.write_text(text)
