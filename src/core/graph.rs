use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use crate::core::BuildIssue;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageSource {
    Pypi {
        name: String,
        version: Option<String>,
    },
    ExistingRepo {
        package: String,
    },
    LocalWorkdir {
        path: PathBuf,
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    Pending,
    SpecPreparing,
    ReadyToSubmit,
    RemoteFixing,
    WaitingForDeps,
    LocalBuilding,
    Success,
    Failed(String),
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Node {
    pub package: String,
    pub source: PackageSource,
    pub features: BTreeSet<String>,
    pub deps: Vec<String>,
    pub state: NodeState,
    pub revision: String,
    pub local_workdir: std::path::PathBuf,
    pub obs_project: String,
    pub spec_path: Option<std::path::PathBuf>,
    pub last_issue: Option<BuildIssue>,
}

impl Node {
    pub fn new(
        package: String,
        source: PackageSource,
        obs_project: String,
        revision: String,
    ) -> Self {
        let local_workdir = std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("workspaces")
            .join(&package);
        Self {
            package: package.clone(),
            source,
            features: BTreeSet::new(),
            deps: Vec::new(),
            state: NodeState::Pending,
            revision,
            local_workdir,
            obs_project,
            spec_path: None,
            last_issue: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureMergeResult {
    NewNode,
    MergedNoChange,
    MergedNeedsRebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyTarget {
    pub package: String,
    pub features: Vec<String>,
}

pub struct DepGraph {
    nodes: HashMap<String, Node>,
}

impl DepGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    pub fn get(&self, package: &str) -> Option<&Node> {
        self.nodes.get(package)
    }

    #[allow(dead_code)]
    pub fn get_mut(&mut self, package: &str) -> Option<&mut Node> {
        self.nodes.get_mut(package)
    }

    pub fn get_or_create(
        &mut self,
        package: &str,
        source: PackageSource,
        obs_project: String,
        revision: String,
    ) -> &mut Node {
        if !self.nodes.contains_key(package) {
            self.nodes.insert(
                package.to_string(),
                Node::new(package.to_string(), source, obs_project, revision),
            );
        }
        self.nodes.get_mut(package).unwrap()
    }

    pub fn add_edge(
        &mut self,
        from: &str,
        to: &str,
        features: Vec<String>,
        source: PackageSource,
        obs_project: String,
        revision: String,
    ) -> FeatureMergeResult {
        // 确保 from 节点存在
        if let Some(node) = self.nodes.get_mut(from)
            && !node.deps.contains(&to.to_string())
        {
            node.deps.push(to.to_string());
        }

        if !self.nodes.contains_key(to) {
            let mut node = Node::new(to.to_string(), source, obs_project, revision);
            for feature in features {
                node.features.insert(feature);
            }
            self.nodes.insert(to.to_string(), node);
            return FeatureMergeResult::NewNode;
        }

        let needs_rebuild = if let Some(node) = self.nodes.get_mut(to) {
            let old_len = node.features.len();
            for f in features {
                node.features.insert(f);
            }
            node.features.len() > old_len && node.state == NodeState::Success
        } else {
            false
        };

        if needs_rebuild {
            if let Some(node) = self.nodes.get_mut(to) {
                node.state = NodeState::Pending;
            }
            FeatureMergeResult::MergedNeedsRebuild
        } else {
            FeatureMergeResult::MergedNoChange
        }
    }

    pub fn unresolved_deps(&self, package: &str) -> Vec<String> {
        let Some(node) = self.nodes.get(package) else {
            return Vec::new();
        };
        node.deps
            .iter()
            .filter(|dep| {
                self.nodes
                    .get(dep.as_str())
                    .map(|n| !matches!(n.state, NodeState::Success))
                    .unwrap_or(true)
            })
            .cloned()
            .collect()
    }

    pub fn mark_success(&mut self, package: &str) {
        if let Some(node) = self.nodes.get_mut(package) {
            node.state = NodeState::Success;
        }
    }

    pub fn mark_failed(&mut self, package: &str, reason: String) {
        if let Some(node) = self.nodes.get_mut(package) {
            node.state = NodeState::Failed(reason);
        }
    }

    pub fn mark_pending(&mut self, package: &str) {
        if let Some(node) = self.nodes.get_mut(package) {
            node.state = NodeState::Pending;
        }
    }

    pub fn mark_state(&mut self, package: &str, state: NodeState) {
        if let Some(node) = self.nodes.get_mut(package) {
            node.state = state;
        }
    }

    pub fn all_deps_success(&self, package: &str) -> bool {
        self.unresolved_deps(package).is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Node)> {
        self.nodes.iter()
    }

    #[allow(dead_code)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut Node)> {
        self.nodes.iter_mut()
    }

    pub fn packages_in_state(&self, state: &NodeState) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.state == *state)
            .map(|(k, _)| k.clone())
            .collect()
    }

    pub fn any_in_state(&self, state: &NodeState) -> bool {
        self.nodes.iter().any(|(_, n)| n.state == *state)
    }

    pub fn has_cycle(&self, from: &str, to: &str) -> bool {
        // 检查 to 是否已经在 from 的依赖路径上
        self._path_contains(to, from)
    }

    fn _path_contains(&self, start: &str, target: &str) -> bool {
        if start == target {
            return true;
        }
        if let Some(node) = self.nodes.get(start) {
            for dep in &node.deps {
                if self._path_contains(dep, target) {
                    return true;
                }
            }
        }
        false
    }
}

pub fn parse_dependency_targets(issue: &BuildIssue) -> Vec<DependencyTarget> {
    match issue {
        BuildIssue::DependencyUnresolvable { deps } => {
            let mut targets: Vec<DependencyTarget> = Vec::new();
            for dep in deps {
                if let Some(info) = python_dependency_info(dep) {
                    if let Some(target) = targets
                        .iter_mut()
                        .find(|target| target.package == info.package)
                    {
                        for feature in info.features {
                            if !target.features.contains(&feature) {
                                target.features.push(feature);
                            }
                        }
                    } else {
                        targets.push(DependencyTarget {
                            package: info.package,
                            features: info.features,
                        });
                    }
                }
            }
            targets
        }
        _ => Vec::new(),
    }
}

pub fn python_dependency_info(dep: &str) -> Option<DependencyTarget> {
    let dep = dep.trim();
    let rest = dep.strip_prefix("python")?;
    let dist_idx = rest.find("dist(")?;
    let module = rest
        .get(dist_idx..)?
        .strip_prefix("dist(")?
        .strip_suffix(')')?;

    let (base_module, features_str) = if let Some(bracket_pos) = module.find('[') {
        let base = &module[..bracket_pos];
        let features_part = module[bracket_pos + 1..].trim_end_matches(']');
        (base, Some(features_part))
    } else {
        (module, None)
    };

    let package = base_module.to_string();
    let features = features_str
        .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
        .unwrap_or_default();

    Some(DependencyTarget { package, features })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pypi_source(name: &str) -> PackageSource {
        PackageSource::Pypi {
            name: name.to_string(),
            version: None,
        }
    }

    #[test]
    fn python_dependency_info_parses_extras() {
        let info = python_dependency_info("python3dist(fonttools[lxml,unicode])")
            .expect("dependency info");
        assert_eq!(info.package, "fonttools");
        assert_eq!(info.features, vec!["lxml", "unicode"]);
    }

    #[test]
    fn python_dependency_info_supports_python3_13dist_format() {
        let info =
            python_dependency_info("python3.13dist(ufo2ft[compreffor])").expect("dependency info");
        assert_eq!(info.package, "ufo2ft");
        assert_eq!(info.features, vec!["compreffor"]);
    }

    #[test]
    fn parse_dependency_targets_merges_same_base_package() {
        let issue = BuildIssue::DependencyUnresolvable {
            deps: vec![
                "python3dist(fonttools)".into(),
                "python3dist(fonttools[lxml])".into(),
                "python3dist(fonttools[unicode])".into(),
            ],
        };
        let targets = parse_dependency_targets(&issue);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].package, "fonttools");
        assert_eq!(targets[0].features, vec!["lxml", "unicode"]);
    }

    #[test]
    fn feature_merge_returns_needs_rebuild() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        graph.mark_success("fonttools");

        let result = graph.add_edge(
            "fontmake",
            "fonttools",
            vec!["lxml".into()],
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        assert_eq!(result, FeatureMergeResult::MergedNeedsRebuild);
        assert_eq!(graph.get("fonttools").unwrap().state, NodeState::Pending);
    }

    #[test]
    fn feature_merge_no_change_when_already_has_feature() {
        let mut graph = DepGraph::new();
        let node = graph.get_or_create(
            "fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        node.features.insert("lxml".into());
        graph.mark_success("fonttools");

        let result = graph.add_edge(
            "fontmake",
            "fonttools",
            vec!["lxml".into()],
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        assert_eq!(result, FeatureMergeResult::MergedNoChange);
    }

    #[test]
    fn unresolved_deps_filters_success_nodes() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "fontmake",
            pypi_source("fontmake"),
            "proj".into(),
            "rev".into(),
        );
        graph.get_or_create(
            "fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        graph.get_or_create(
            "fontmath",
            pypi_source("fontmath"),
            "proj".into(),
            "rev".into(),
        );

        let fm = graph.get_mut("fontmake").unwrap();
        fm.deps = vec!["fonttools".into(), "fontmath".into()];

        graph.mark_success("fonttools");

        let unresolved = graph.unresolved_deps("fontmake");
        assert_eq!(unresolved, vec!["fontmath"]);
    }

    #[test]
    fn cycle_detection_works() {
        let mut graph = DepGraph::new();
        graph.get_or_create("a", pypi_source("a"), "proj".into(), "rev".into());
        graph.get_or_create("b", pypi_source("b"), "proj".into(), "rev".into());
        graph.get_or_create("c", pypi_source("c"), "proj".into(), "rev".into());

        graph.get_mut("a").unwrap().deps.push("b".into());
        graph.get_mut("b").unwrap().deps.push("c".into());

        assert!(graph.has_cycle("c", "a"));
        assert!(!graph.has_cycle("a", "c"));
    }

    #[test]
    fn add_edge_new_node_preserves_features() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "fontmake",
            pypi_source("fontmake"),
            "proj".into(),
            "rev".into(),
        );

        let result = graph.add_edge(
            "fontmake",
            "fonttools",
            vec!["lxml".into()],
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );

        assert_eq!(result, FeatureMergeResult::NewNode);
        assert_eq!(
            graph
                .get("fonttools")
                .unwrap()
                .features
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["lxml".to_string()]
        );
    }

    #[test]
    fn add_edge_existing_success_with_new_feature_marks_pending() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "fontmake",
            pypi_source("fontmake"),
            "proj".into(),
            "rev".into(),
        );
        graph.get_or_create(
            "fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        graph.mark_success("fonttools");

        let result = graph.add_edge(
            "fontmake",
            "fonttools",
            vec!["lxml".into()],
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );

        assert_eq!(result, FeatureMergeResult::MergedNeedsRebuild);
        assert_eq!(graph.get("fonttools").unwrap().state, NodeState::Pending);
    }
}
