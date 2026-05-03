use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::{
    cli::Cli,
    config,
    core::{
        engine::{self, BuildOutcome},
        graph::{DepGraph, FeatureMergeResult, NodeState, PackageSource},
        resources::BuildResources,
    },
};

pub struct BuildScheduler {
    graph: Arc<Mutex<DepGraph>>,
    resources: Arc<BuildResources>,
    cli: Cli,
    cfg: Option<config::PackfixConfig>,
}

#[derive(Debug, Clone)]
pub struct NodeResult {
    pub package: String,
    pub outcome: BuildOutcome,
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub package: String,
    pub features: Vec<String>,
    pub source: PackageSource,
}

impl BuildScheduler {
    pub fn new(cli: Cli, cfg: Option<config::PackfixConfig>) -> Self {
        Self {
            graph: Arc::new(Mutex::new(DepGraph::new())),
            resources: Arc::new(BuildResources::new(
                PathBuf::from("/var/tmp/build-root"),
                cli.repository.clone(),
                cli.arch.clone(),
            )),
            cli,
            cfg,
        }
    }

    /// 运行调度器，并发构建所有节点。
    ///
    /// `roots` 是初始根节点列表，每个元素为 `(package_name, features)`。
    /// `project` 和 `revision` 是所有节点的默认 OBS project 和 git revision。
    pub async fn run(
        &self,
        roots: Vec<BuildRequest>,
        project: String,
        revision: String,
    ) -> Result<Vec<NodeResult>> {
        let total_roots = roots.len();

        // 初始化根节点
        {
            let mut graph = self.graph.lock().await;
            for root in roots {
                let node = graph.get_or_create(
                    &root.package,
                    root.source,
                    project.clone(),
                    revision.clone(),
                );
                for f in root.features {
                    node.features.insert(f);
                }
            }
        }

        info!(total = total_roots, "batch started");

        let mut running: JoinSet<Result<(String, BuildOutcome)>> = JoinSet::new();
        let mut results: Vec<NodeResult> = Vec::new();
        let mut completed: usize = 0;

        loop {
            // 将依赖已满足的 WaitingForDeps 节点重新置为 Pending
            {
                let mut graph = self.graph.lock().await;
                resume_waiting_nodes_with_satisfied_deps(&mut graph);
            }

            // 收集所有可执行的 Pending 节点（依赖全部 Success）
            let to_start = {
                let mut graph = self.graph.lock().await;
                let mut packages = Vec::new();
                for (pkg, node) in graph.iter() {
                    if matches!(node.state, NodeState::Pending) && graph.all_deps_success(pkg) {
                        packages.push(pkg.clone());
                    }
                }
                for pkg in &packages {
                    let next_state = graph
                        .get(pkg)
                        .map(node_running_state)
                        .unwrap_or(NodeState::SpecPreparing);
                    graph.mark_state(pkg, next_state);
                }
                packages
            };

            for pkg in to_start {
                let (features, source) = {
                    let graph = self.graph.lock().await;
                    graph
                        .get(&pkg)
                        .map(|n| {
                            (
                                n.features.iter().cloned().collect::<Vec<_>>(),
                                n.source.clone(),
                            )
                        })
                        .unwrap_or_else(|| {
                            (
                                Vec::new(),
                                PackageSource::Pypi {
                                    name: crate::upstream::python_dist_name(&pkg),
                                    version: None,
                                },
                            )
                        })
                };

                let resources = Arc::clone(&self.resources);
                let cli = self.cli.clone();
                let cfg = self.cfg.clone();
                let project = project.clone();
                let revision = revision.clone();

                running.spawn(async move {
                    let outcome = match engine::build_node(
                        features,
                        source,
                        resources,
                        &cli,
                        cfg.as_ref(),
                        &project,
                        &revision,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(err) => BuildOutcome::Failed(err.to_string()),
                    };
                    Ok((pkg, outcome))
                });
            }

            // 没有运行中的任务时，检查是否全部完成
            if running.is_empty() {
                let has_waiting = {
                    let graph = self.graph.lock().await;
                    graph.any_in_state(&NodeState::WaitingForDeps)
                };
                if !has_waiting {
                    info!("scheduler: all tasks completed");
                    break;
                }

                // 将因依赖失败而无法继续的 WaitingForDeps 节点标记为 Failed
                let mut to_fail: Vec<(String, String)> = Vec::new();
                {
                    let graph = self.graph.lock().await;
                    for (pkg, node) in graph.iter() {
                        if matches!(node.state, NodeState::WaitingForDeps) {
                            if let Some(failed_dep) = node.deps.iter().find(|dep| {
                                graph
                                    .get(dep)
                                    .map(|n| matches!(n.state, NodeState::Failed(_)))
                                    .unwrap_or(false)
                            }) {
                                to_fail.push((pkg.clone(), failed_dep.clone()));
                            }
                        }
                    }
                }
                for (pkg, failed_dep) in &to_fail {
                    warn!(
                        package = %pkg,
                        dependency = %failed_dep,
                        "dependency failed, marking node as failed"
                    );
                    let reason = format!("dependency build failed: {failed_dep}");
                    let mut graph = self.graph.lock().await;
                    graph.mark_failed(pkg, reason.clone());
                    results.push(NodeResult {
                        package: pkg.clone(),
                        outcome: BuildOutcome::Failed(reason),
                    });
                }

                // 如果还有 WaitingForDeps 但没有任何任务在运行，说明死锁
                let still_waiting = {
                    let graph = self.graph.lock().await;
                    graph.any_in_state(&NodeState::WaitingForDeps)
                };
                if still_waiting {
                    warn!(
                        "scheduler deadlock detected: nodes waiting for deps but no tasks running"
                    );
                    let mut to_fail: Vec<String> = Vec::new();
                    {
                        let graph = self.graph.lock().await;
                        for (pkg, node) in graph.iter() {
                            if matches!(node.state, NodeState::WaitingForDeps) {
                                to_fail.push(pkg.clone());
                            }
                        }
                    }
                    for pkg in &to_fail {
                        let mut graph = self.graph.lock().await;
                        graph.mark_failed(
                            pkg,
                            "scheduler deadlock: waiting for deps with no progress".into(),
                        );
                        results.push(NodeResult {
                            package: pkg.clone(),
                            outcome: BuildOutcome::Failed(
                                "scheduler deadlock: waiting for deps with no progress".into(),
                            ),
                        });
                    }
                }
                continue;
            }

            // 等待至少一个任务完成
            let (pkg, outcome) = match running.join_next().await {
                Some(Ok(Ok((pkg, outcome)))) => (pkg, outcome),
                Some(Ok(Err(e))) => {
                    warn!(error = %e, "engine task returned error");
                    continue;
                }
                Some(Err(e)) => {
                    warn!(error = %e, "tokio task panicked");
                    continue;
                }
                None => continue,
            };

            // 处理结果
            match &outcome {
                BuildOutcome::Success { .. } => {
                    info!(
                        completed = completed + 1,
                        total = total_roots,
                        package = %pkg,
                        "batch progress"
                    );
                    completed += 1;
                    let mut graph = self.graph.lock().await;
                    record_terminal_outcome(&mut graph, &mut results, pkg, outcome);
                    resume_waiting_nodes_with_satisfied_deps(&mut graph);
                }
                BuildOutcome::NeedsDependencies(deps) => {
                    info!(package = %pkg, dep_count = deps.len(), "build needs dependencies");
                    let mut graph = self.graph.lock().await;
                    graph.mark_state(&pkg, NodeState::WaitingForDeps);

                    for dep in deps {
                        let BuildRequest {
                            package: dep_package,
                            features: dep_features,
                            source: dep_source,
                        } = dependency_build_request(&dep);

                        // 循环检测
                        let would_cycle = graph.has_cycle(&pkg, &dep_package);
                        if would_cycle {
                            warn!(
                                package = %pkg,
                                dep = %dep_package,
                                "cycle detected, marking node as failed"
                            );
                            graph.mark_failed(
                                &pkg,
                                format!("dependency cycle with {}", dep_package),
                            );
                            results.push(NodeResult {
                                package: pkg.clone(),
                                outcome: BuildOutcome::Failed(format!(
                                    "dependency cycle with {}",
                                    dep_package
                                )),
                            });
                            break;
                        }

                        let merge_result = graph.add_edge(
                            &pkg,
                            &dep_package,
                            dep_features,
                            dep_source,
                            project.clone(),
                            revision.clone(),
                        );

                        if matches!(merge_result, FeatureMergeResult::NewNode) {
                            info!(package = %dep_package, "new dependency node created");
                        }

                        if matches!(merge_result, FeatureMergeResult::MergedNeedsRebuild) {
                            info!(
                                package = %dep_package,
                                "existing dependency node needs rebuild due to new features"
                            );
                        }
                    }
                }
                BuildOutcome::Failed(reason) => {
                    info!(
                        completed = completed + 1,
                        total = total_roots,
                        package = %pkg,
                        reason = %reason,
                        "batch progress"
                    );
                    completed += 1;
                    let mut graph = self.graph.lock().await;
                    record_terminal_outcome(&mut graph, &mut results, pkg, outcome);
                }
            }
        }

        let succeeded = results
            .iter()
            .filter(|r| matches!(r.outcome, BuildOutcome::Success { .. }))
            .count();
        let failed = results
            .iter()
            .filter(|r| matches!(r.outcome, BuildOutcome::Failed(_)))
            .count();
        info!(total = results.len(), succeeded, failed, "batch finished");

        Ok(results)
    }
}

fn resume_waiting_nodes_with_satisfied_deps(graph: &mut DepGraph) {
    let waiting = graph.packages_in_state(&NodeState::WaitingForDeps);
    for pkg in waiting {
        if graph.all_deps_success(&pkg) {
            info!(package = %pkg, "all dependencies satisfied, resuming build");
            graph.mark_pending(&pkg);
        }
    }
}

fn node_running_state(node: &crate::core::graph::Node) -> NodeState {
    match node.source {
        PackageSource::LocalWorkdir { .. } => NodeState::LocalBuilding,
        PackageSource::Pypi { .. } | PackageSource::ExistingRepo { .. } => NodeState::SpecPreparing,
    }
}

fn dependency_build_request(dep: &crate::core::graph::DependencyTarget) -> BuildRequest {
    BuildRequest {
        package: crate::upstream::python_package_name(&dep.package),
        features: dep.features.clone(),
        source: PackageSource::Pypi {
            name: dep.package.clone(),
            version: None,
        },
    }
}

fn record_terminal_outcome(
    graph: &mut DepGraph,
    results: &mut Vec<NodeResult>,
    pkg: String,
    outcome: BuildOutcome,
) {
    match &outcome {
        BuildOutcome::Success { report: _ } => {
            info!(package = %pkg, "build succeeded");
            graph.mark_success(&pkg);
        }
        BuildOutcome::Failed(reason) => {
            warn!(package = %pkg, reason = %reason, "build failed");
            graph.mark_failed(&pkg, reason.clone());
        }
        BuildOutcome::NeedsDependencies(_) => return,
    }

    results.push(NodeResult {
        package: pkg,
        outcome,
    });
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
    fn scheduler_engine_error_marks_node_failed() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "python-foo",
            pypi_source("foo"),
            "proj".into(),
            "rev".into(),
        );
        graph.mark_state("python-foo", NodeState::SpecPreparing);

        let mut results = Vec::new();
        record_terminal_outcome(
            &mut graph,
            &mut results,
            "python-foo".into(),
            BuildOutcome::Failed("engine exploded".into()),
        );

        assert!(matches!(
            graph.get("python-foo").map(|node| &node.state),
            Some(NodeState::Failed(reason)) if reason == "engine exploded"
        ));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].package, "python-foo");
        assert!(matches!(
            &results[0].outcome,
            BuildOutcome::Failed(reason) if reason == "engine exploded"
        ));
    }

    #[test]
    fn dependency_node_uses_rpm_key_but_pypi_source_name() {
        let request = dependency_build_request(&crate::core::graph::DependencyTarget {
            package: "fonttools".into(),
            features: vec!["lxml".into()],
        });

        assert_eq!(request.package, "python-fonttools");
        assert_eq!(request.features, vec!["lxml"]);
        assert_eq!(
            request.source,
            PackageSource::Pypi {
                name: "fonttools".into(),
                version: None
            }
        );
    }

    #[test]
    fn scheduler_can_resume_local_workdir_after_dependency_success() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "python-demo",
            PackageSource::LocalWorkdir {
                path: std::path::PathBuf::from("/tmp/demo"),
            },
            "proj".into(),
            "rev".into(),
        );
        graph.get_or_create(
            "python-fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        graph.get_mut("python-demo").expect("local node").deps = vec!["python-fonttools".into()];
        graph.mark_state("python-demo", NodeState::WaitingForDeps);
        graph.mark_success("python-fonttools");

        resume_waiting_nodes_with_satisfied_deps(&mut graph);

        assert!(matches!(
            graph.get("python-demo").map(|node| &node.state),
            Some(NodeState::Pending)
        ));
    }

    #[test]
    fn success_can_immediately_resume_waiting_parent() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "python-demo",
            PackageSource::LocalWorkdir {
                path: std::path::PathBuf::from("/tmp/demo"),
            },
            "proj".into(),
            "rev".into(),
        );
        graph.get_or_create(
            "python-fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        graph.get_mut("python-demo").expect("parent").deps = vec!["python-fonttools".into()];
        graph.mark_state("python-demo", NodeState::WaitingForDeps);

        let mut results = Vec::new();
        record_terminal_outcome(
            &mut graph,
            &mut results,
            "python-fonttools".into(),
            BuildOutcome::Success { report: None },
        );
        resume_waiting_nodes_with_satisfied_deps(&mut graph);

        assert!(matches!(
            graph.get("python-demo").map(|node| &node.state),
            Some(NodeState::Pending)
        ));
    }

    #[test]
    fn local_workdir_nodes_enter_local_building_state_when_started() {
        let node = crate::core::graph::Node::new(
            "python-demo".into(),
            PackageSource::LocalWorkdir {
                path: std::path::PathBuf::from("/tmp/demo"),
            },
            "proj".into(),
            "rev".into(),
        );

        assert_eq!(node_running_state(&node), NodeState::LocalBuilding);
    }

    #[test]
    fn waiting_parent_failure_reason_includes_failed_dependency_name() {
        let mut graph = DepGraph::new();
        graph.get_or_create(
            "python-demo",
            PackageSource::LocalWorkdir {
                path: std::path::PathBuf::from("/tmp/demo"),
            },
            "proj".into(),
            "rev".into(),
        );
        graph.get_or_create(
            "python-fonttools",
            pypi_source("fonttools"),
            "proj".into(),
            "rev".into(),
        );
        graph.get_mut("python-demo").expect("parent").deps = vec!["python-fonttools".into()];
        graph.mark_state("python-demo", NodeState::WaitingForDeps);
        graph.mark_failed("python-fonttools", "need human".into());

        let mut to_fail: Vec<(String, String)> = Vec::new();
        for (pkg, node) in graph.iter() {
            if matches!(node.state, NodeState::WaitingForDeps) {
                if let Some(failed_dep) = node.deps.iter().find(|dep| {
                    graph
                        .get(dep)
                        .map(|n| matches!(n.state, NodeState::Failed(_)))
                        .unwrap_or(false)
                }) {
                    to_fail.push((pkg.clone(), failed_dep.clone()));
                }
            }
        }

        assert_eq!(
            to_fail,
            vec![("python-demo".into(), "python-fonttools".into())]
        );
    }
}
